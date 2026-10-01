//! Concurrent-safe aggregation of scan results.
//!
//! Workers never touch shared state per file. Each accumulates a local batch
//! (see [`crate::scan`]) and hands it over in one go, so the aggregator's lock
//! is taken once per batch rather than once per entry.
//!
//! Memory is deliberately bounded:
//!
//! * one small record per **directory** (never per file),
//! * extension totals (a small, fixed key space),
//! * a bounded top-N heap of largest files,
//! * a bounded sample of filesystem errors.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::path::{Path, PathBuf};

/// Recursive totals for one directory.
///
/// A directory's totals always include every descendant, because the worker
/// that finishes a directory adds its figures to that directory and to each
/// ancestor up to the scan root.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DirTotals {
    /// Bytes of files found at or below this directory.
    pub bytes: u64,
    /// Number of files at or below this directory.
    pub files: u64,
    /// Number of directories strictly below this directory.
    pub subdirs: u64,
}

/// One directory's immediate findings, as reported by a worker.
#[derive(Debug, Clone)]
pub struct DirReport {
    /// Directory the worker read.
    pub path: PathBuf,
    /// Bytes of files sitting directly in this directory.
    pub bytes: u64,
    /// Number of files sitting directly in this directory.
    pub files: u64,
    /// Number of child directories discovered.
    pub subdirs: u64,
}

/// Totals for one file extension.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExtTotals {
    /// Bytes across all files with this extension.
    pub bytes: u64,
    /// Number of files with this extension.
    pub files: u64,
}

/// A directory in the final report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DirEntry {
    /// Full path.
    pub path: String,
    /// Recursive size in bytes.
    pub size_bytes: u64,
    /// Recursive file count.
    pub files: u64,
    /// Recursive child directory count.
    pub directories: u64,
    /// Recursive size as a display string.
    pub size: String,
}

/// An extension in the final report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExtEntry {
    /// Lowercase extension without the dot; empty for extensionless files.
    pub extension: String,
    /// Bytes across all matching files.
    pub size_bytes: u64,
    /// Number of matching files.
    pub files: u64,
    /// Size as a display string.
    pub size: String,
}

/// A file in the final largest-files report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FileEntry {
    /// Full path.
    pub path: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Size as a display string.
    pub size: String,
}

/// A filesystem entry DS could not inspect.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScanIssue {
    /// Path that failed.
    pub path: String,
    /// Operating-system message.
    pub message: String,
}

/// Heap entry ordered so the smallest retained item sits on top and is the
/// first to be evicted.
#[derive(Debug)]
struct Candidate {
    bytes: u64,
    path: PathBuf,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes && self.path == other.path
    }
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .bytes
            .cmp(&self.bytes)
            .then_with(|| other.path.cmp(&self.path))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A bounded top-N set of `(bytes, path)` pairs.
///
/// Only `keep` entries are ever retained, so this stays flat regardless of how
/// many files are scanned.
#[derive(Debug)]
struct TopN {
    heap: BinaryHeap<Candidate>,
    keep: usize,
}

impl TopN {
    fn new(keep: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(keep.min(1024)),
            keep,
        }
    }

    /// Offer one candidate, cloning the path only when it is actually kept.
    fn offer(&mut self, bytes: u64, path: &Path) {
        if self.keep == 0 {
            return;
        }

        if self.heap.len() >= self.keep
            && self.heap.peek().is_some_and(|worst| bytes <= worst.bytes)
        {
            return;
        }

        if self.heap.len() >= self.keep {
            self.heap.pop();
        }

        self.heap.push(Candidate {
            bytes,
            path: path.to_path_buf(),
        });
    }

    /// Drain the retained entries, biggest first.
    fn drain(&self) -> Vec<(PathBuf, u64)> {
        let mut items: Vec<(PathBuf, u64)> = self
            .heap
            .iter()
            .map(|c| (c.path.clone(), c.bytes))
            .collect();
        items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        items
    }
}

/// Running totals, sorted views, and issue log for a whole scan.
#[derive(Debug)]
pub struct Aggregator {
    /// One entry per directory discovered. This is the main memory cost, and
    /// it scales with directory count, never with file count.
    dirs: HashMap<PathBuf, DirTotals>,
    /// Totals keyed by lowercase extension.
    exts: HashMap<String, ExtTotals>,
    /// Bounded largest-file set, updated as files are seen.
    largest: TopN,
    /// Retained issue samples; the full count is tracked separately.
    issues: Vec<ScanIssue>,
    issue_count: u64,
    issue_keep: usize,
    root: PathBuf,
    files: u64,
    bytes: u64,
    dirs_seen: u64,
}

impl Aggregator {
    /// Create an aggregator bounded by the requested report sizes.
    ///
    /// `largest_keep` caps retained files and `issue_keep` caps retained
    /// failures; neither affects what is scanned.
    pub fn new(root: PathBuf, largest_keep: usize, issue_keep: usize) -> Self {
        Self {
            dirs: HashMap::new(),
            exts: HashMap::new(),
            largest: TopN::new(largest_keep),
            issues: Vec::new(),
            issue_count: 0,
            issue_keep,
            root,
            files: 0,
            bytes: 0,
            dirs_seen: 0,
        }
    }

    /// Record one directory's immediate findings.
    ///
    /// `directories` counts directories actually read, not children merely
    /// discovered: a child contributes when its own report arrives, which keeps
    /// the count free of double counting.
    pub fn add_report(&mut self, report: DirReport) {
        self.files = self.files.saturating_add(report.files);
        self.bytes = self.bytes.saturating_add(report.bytes);
        self.dirs_seen = self.dirs_seen.saturating_add(1);

        self.add_up_tree(&report.path, report.bytes, report.files);
    }

    /// Fold a batch of directory reports.
    ///
    /// Callers flush a batch per worker rather than reporting one directory at
    /// a time, keeping per-entry overhead off the hot path.
    pub fn add_reports(&mut self, reports: &[DirReport]) {
        for report in reports {
            let report = report.clone();
            self.add_report(report);
        }
    }

    /// Add `bytes`/`files` to `path` and each ancestor up to the root.
    ///
    /// Each ancestor also gains the byte and file totals, but only the *direct*
    /// parent gains a child-directory count. That is what makes `C:\Games`
    /// include everything under `C:\Games\Minecraft` without a second pass over
    /// the filesystem, while keeping directory counts exact.
    fn add_up_tree(&mut self, path: &Path, bytes: u64, files: u64) {
        let mut current = Some(path);
        let mut is_direct_child = true;

        while let Some(dir) = current {
            let totals = self.dirs.entry(dir.to_path_buf()).or_default();
            totals.bytes = totals.bytes.saturating_add(bytes);
            totals.files = totals.files.saturating_add(files);

            if dir == self.root {
                break;
            }

            let Some(parent) = dir.parent() else { break };

            if is_direct_child {
                self.dirs.entry(parent.to_path_buf()).or_default().subdirs += 1;
                is_direct_child = false;
            }

            current = Some(parent);
        }
    }

    /// Add one file's contribution to an extension total.
    pub fn add_extension(&mut self, extension: &str, bytes: u64, files: u64) {
        let entry = self.exts.entry(extension.to_string()).or_default();
        entry.bytes = entry.bytes.saturating_add(bytes);
        entry.files = entry.files.saturating_add(files);
    }

    /// Fold a batch of per-extension file figures.
    pub fn add_extensions(&mut self, exts: &[(String, ExtTotals)]) {
        for (extension, totals) in exts {
            self.add_extension(extension, totals.bytes, totals.files);
        }
    }

    /// Offer one file to the bounded largest-file set.
    pub fn add_file(&mut self, bytes: u64, path: &Path) {
        self.largest.offer(bytes, path);
    }

    /// Fold a batch of files into the bounded largest-file set.
    pub fn add_files(&mut self, files: &[(u64, PathBuf)]) {
        for (bytes, path) in files {
            self.largest.offer(*bytes, path);
        }
    }

    /// Record one filesystem failure, retaining only a bounded sample.
    pub fn add_issue(&mut self, path: &Path, message: &str) {
        self.issue_count += 1;
        if self.issues.len() < self.issue_keep {
            self.issues.push(ScanIssue {
                path: path.to_string_lossy().into_owned(),
                message: message.to_string(),
            });
        }
    }

    /// Fold a batch of filesystem failures.
    pub fn add_issues(&mut self, issues: &[(PathBuf, String)]) {
        for (path, message) in issues {
            self.add_issue(path, message);
        }
    }

    /// Absorb another aggregator's findings.
    ///
    /// Used to combine per-worker aggregators. Each worker propagates its own
    /// figures all the way up to the shared root, so shared ancestors simply
    /// accumulate: summing is sufficient and no directory is ever counted twice.
    pub fn merge(&mut self, other: Aggregator) {
        self.files = self.files.saturating_add(other.files);
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.dirs_seen = self.dirs_seen.saturating_add(other.dirs_seen);
        self.issue_count = self.issue_count.saturating_add(other.issue_count);

        for (path, totals) in other.dirs {
            let entry = self.dirs.entry(path).or_default();
            entry.bytes = entry.bytes.saturating_add(totals.bytes);
            entry.files = entry.files.saturating_add(totals.files);
            entry.subdirs = entry.subdirs.saturating_add(totals.subdirs);
        }

        for (extension, totals) in other.exts {
            let entry = self.exts.entry(extension).or_default();
            entry.bytes = entry.bytes.saturating_add(totals.bytes);
            entry.files = entry.files.saturating_add(totals.files);
        }

        for (path, bytes) in other.largest.drain() {
            self.largest.offer(bytes, &path);
        }

        for issue in other.issues {
            if self.issues.len() < self.issue_keep {
                self.issues.push(issue);
            }
        }
    }

    /// Total bytes of all files found.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Total files found.
    pub fn files(&self) -> u64 {
        self.files
    }

    /// Total directories found, including the root.
    pub fn directories(&self) -> u64 {
        self.dirs_seen
    }

    /// Total failures seen, including those beyond the retained sample.
    pub fn issue_count(&self) -> u64 {
        self.issue_count
    }

    /// Retained failure samples.
    pub fn issues(&self) -> &[ScanIssue] {
        &self.issues
    }

    /// Recursive totals for a single directory.
    pub fn dir_totals(&self, path: &Path) -> Option<DirTotals> {
        self.dirs.get(path).copied()
    }

    /// Number of directories currently held in the totals map.
    pub fn tracked_directories(&self) -> usize {
        self.dirs.len()
    }

    /// Largest directories, biggest first, capped at `keep`.
    ///
    /// The scan root is excluded: it is the target being analysed, and its
    /// total is simply the sum of everything else, so listing it would waste a
    /// row. Computed from the totals map at report time, so no heap maintenance
    /// is needed during the scan and the result stays correct as directories grow.
    pub fn top_dirs(&self, keep: usize) -> Vec<(PathBuf, DirTotals)> {
        if keep == 0 {
            return Vec::new();
        }

        let mut best = TopN::new(keep);
        for (path, totals) in &self.dirs {
            if path == &self.root {
                continue;
            }
            if best.heap.len() >= keep
                && best
                    .heap
                    .peek()
                    .is_some_and(|worst| totals.bytes <= worst.bytes)
            {
                continue;
            }
            best.offer(totals.bytes, path);
        }

        best.drain()
            .into_iter()
            .map(|(path, bytes)| {
                let totals = self.dirs.get(&path).copied().unwrap_or_default();
                (path, DirTotals { bytes, ..totals })
            })
            .collect()
    }

    /// Largest files, biggest first.
    pub fn top_files(&self) -> Vec<(PathBuf, u64)> {
        self.largest.drain()
    }

    /// Extension totals ordered by size, biggest first.
    pub fn extension_totals(&self) -> Vec<(String, ExtTotals)> {
        let mut items: Vec<(String, ExtTotals)> = self
            .exts
            .iter()
            .map(|(name, totals)| (name.clone(), *totals))
            .collect();
        items.sort_by(|a, b| {
            b.1.bytes
                .cmp(&a.1.bytes)
                .then_with(|| b.1.files.cmp(&a.1.files))
                .then_with(|| a.0.cmp(&b.0))
        });
        items
    }

    /// Build the directory report.
    pub fn dir_entries(&self, keep: usize, human: bool) -> Vec<DirEntry> {
        self.top_dirs(keep)
            .into_iter()
            .map(|(path, totals)| DirEntry {
                size: crate::format::format_bytes(totals.bytes, human),
                path: path.to_string_lossy().into_owned(),
                size_bytes: totals.bytes,
                files: totals.files,
                directories: totals.subdirs,
            })
            .collect()
    }

    /// Build the extension report.
    pub fn ext_entries(&self, human: bool) -> Vec<ExtEntry> {
        self.extension_totals()
            .into_iter()
            .map(|(extension, totals)| ExtEntry {
                size: crate::format::format_bytes(totals.bytes, human),
                extension,
                size_bytes: totals.bytes,
                files: totals.files,
            })
            .collect()
    }

    /// Build the largest-file report.
    pub fn file_entries(&self, human: bool) -> Vec<FileEntry> {
        self.top_files()
            .into_iter()
            .map(|(path, bytes)| FileEntry {
                size: crate::format::format_bytes(bytes, human),
                path: path.to_string_lossy().into_owned(),
                size_bytes: bytes,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Portable stand-in for a volume root.
    ///
    /// A relative name is used deliberately: `C:\` is a single path component
    /// with no parent on Unix, which would silently disable the ancestor
    /// walk-up these tests exist to verify.
    fn root() -> PathBuf {
        PathBuf::from("root")
    }

    /// A path below [`root`], given with `/` separators.
    fn dir(rel: &str) -> PathBuf {
        let mut path = root();
        for part in rel.split('/').filter(|part| !part.is_empty()) {
            path.push(part);
        }
        path
    }

    fn report(rel: &str, bytes: u64, files: u64, subdirs: u64) -> DirReport {
        DirReport {
            path: dir(rel),
            bytes,
            files,
            subdirs,
        }
    }

    #[test]
    fn a_single_directory_totals_its_own_files() {
        let mut agg = Aggregator::new(root(), 4, 4);
        agg.add_reports(&[report("", 100, 2, 0)]);

        assert_eq!(agg.bytes(), 100);
        assert_eq!(agg.files(), 2);
        assert_eq!(agg.directories(), 1);
    }

    #[test]
    fn the_directory_count_counts_reads_not_discoveries() {
        // Each directory is counted when its own report arrives, so a parent
        // listing three children must not inflate the total.
        let mut agg = Aggregator::new(root(), 4, 4);
        agg.add_reports(&[report("", 0, 0, 3)]);
        agg.add_reports(&[report("a", 0, 0, 0)]);
        agg.add_reports(&[report("b", 0, 0, 0)]);
        agg.add_reports(&[report("c", 0, 0, 0)]);

        assert_eq!(agg.directories(), 4, "root plus three children");
        assert_eq!(agg.dir_totals(&root()).unwrap().subdirs, 3);
    }

    #[test]
    fn parent_directories_include_descendants() {
        let mut agg = Aggregator::new(root(), 4, 4);
        agg.add_reports(&[
            report("Games", 0, 0, 2),
            report("Games\\Steam", 30_000, 3, 0),
            report("Games\\Minecraft", 2_500, 2, 0),
        ]);

        let games = agg.dir_totals(&dir("Games")).unwrap();
        assert_eq!(games.bytes, 32_500, "Games must include its children");
        assert_eq!(games.files, 5);
        assert_eq!(games.subdirs, 2);

        let steam = agg.dir_totals(&dir("Games\\Steam")).unwrap();
        assert_eq!(steam.bytes, 30_000);

        let root = agg.dir_totals(&root()).unwrap();
        assert_eq!(root.bytes, 32_500);
        assert_eq!(root.subdirs, 1, "root has one child: Games");
    }

    #[test]
    fn the_documented_games_example_holds() {
        // C:\Games\Minecraft -> 500 MB + 2 GB = 2.5 GB
        // C:\Games\Steam     -> 30 GB
        let mb = 1_000_000u64;
        let gb = 1_000 * mb;
        let mut agg = Aggregator::new(root(), 4, 4);
        agg.add_reports(&[
            report("Games", 0, 0, 2),
            report("Games\\Minecraft", 500 * mb, 1, 1),
            report("Games\\Minecraft\\assets", 2 * gb, 4, 0),
            report("Games\\Steam", 30 * gb, 20, 0),
        ]);

        let mc = agg.dir_totals(&dir("Games\\Minecraft")).unwrap();
        assert_eq!(mc.bytes, 500 * mb + 2 * gb);

        let steam = agg.dir_totals(&dir("Games\\Steam")).unwrap();
        assert_eq!(steam.bytes, 30 * gb);

        let games = agg.dir_totals(&dir("Games")).unwrap();
        assert_eq!(games.bytes, 30 * gb + 500 * mb + 2 * gb);
    }

    #[test]
    fn batches_accumulate_like_single_reports() {
        let mut batched = Aggregator::new(root(), 4, 4);
        batched.add_reports(&[report("a", 10, 1, 1)]);
        batched.add_reports(&[report("a\\b", 20, 2, 0)]);

        let mut single = Aggregator::new(root(), 4, 4);
        single.add_reports(&[report("a", 10, 1, 1), report("a\\b", 20, 2, 0)]);

        assert_eq!(batched.bytes(), single.bytes());
        assert_eq!(batched.files(), single.files());
        assert_eq!(batched.directories(), single.directories());
    }

    #[test]
    fn top_dirs_returns_the_biggest_first() {
        let mut agg = Aggregator::new(root(), 4, 4);
        agg.add_reports(&[
            report("small", 10, 1, 0),
            report("big", 900, 1, 0),
            report("mid", 400, 1, 0),
        ]);

        let top = agg.top_dirs(3);
        let names: Vec<String> = top
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["big", "mid", "small"]);
    }

    #[test]
    fn top_dirs_respects_its_cap_and_excludes_the_root() {
        let mut agg = Aggregator::new(root(), 4, 4);
        agg.add_reports(&[
            report("a", 10, 1, 0),
            report("b", 20, 1, 0),
            report("c", 30, 1, 0),
        ]);

        let top = agg.top_dirs(2);
        assert_eq!(top.len(), 2, "cap wins over the root's larger total");
        let names: Vec<String> = top
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["c", "b"], "biggest first");
    }

    #[test]
    fn top_dirs_of_zero_returns_nothing() {
        let mut agg = Aggregator::new(root(), 4, 4);
        agg.add_reports(&[report("a", 10, 1, 0)]);
        assert!(agg.top_dirs(0).is_empty());
    }

    #[test]
    fn largest_files_are_bounded_and_ordered() {
        let mut agg = Aggregator::new(root(), 3, 4);
        for n in 1..=100u64 {
            agg.add_files(&[(n, PathBuf::from(format!("file-{n}")))]);
        }

        let top = agg.top_files();
        assert_eq!(top.len(), 3, "top-N must stay bounded");
        assert_eq!(top[0].1, 100);
        assert_eq!(top[1].1, 99);
        assert_eq!(top[2].1, 98);
    }

    #[test]
    fn largest_files_of_zero_retains_nothing() {
        let mut agg = Aggregator::new(root(), 0, 4);
        agg.add_files(&[(10, PathBuf::from("a"))]);
        assert!(agg.top_files().is_empty());
    }

    #[test]
    fn largest_files_break_ties_by_path() {
        let mut agg = Aggregator::new(root(), 2, 4);
        agg.add_files(&[(5, PathBuf::from("b")), (5, PathBuf::from("a"))]);
        let top = agg.top_files();
        assert_eq!(top[0].0, PathBuf::from("a"));
    }

    #[test]
    fn extension_totals_merge_and_sort_by_size() {
        let mut agg = Aggregator::new(root(), 4, 4);
        agg.add_extensions(&[
            (
                "mp4".to_string(),
                ExtTotals {
                    bytes: 10,
                    files: 1,
                },
            ),
            (
                "zip".to_string(),
                ExtTotals {
                    bytes: 50,
                    files: 2,
                },
            ),
        ]);
        agg.add_extensions(&[("mp4".to_string(), ExtTotals { bytes: 5, files: 1 })]);

        let totals = agg.extension_totals();
        assert_eq!(totals[0].0, "zip");
        assert_eq!(totals[1].0, "mp4");
        assert_eq!(totals[1].1.bytes, 15, "mp4 batches must merge");
        assert_eq!(totals[1].1.files, 2);
    }

    #[test]
    fn issues_are_counted_but_only_a_sample_is_retained() {
        let mut agg = Aggregator::new(root(), 4, 2);
        for n in 0..10 {
            agg.add_issues(&[(PathBuf::from(format!("p{n}")), "denied".to_string())]);
        }

        assert_eq!(agg.issue_count(), 10, "every failure is counted");
        assert_eq!(agg.issues().len(), 2, "only the sample is retained");
    }

    #[test]
    fn tracked_directories_grows_with_directories_not_files() {
        let mut agg = Aggregator::new(root(), 4, 4);
        agg.add_reports(&[report("a", 0, 0, 1)]);
        agg.add_reports(&[report("a\\b", 0, 0, 1)]);
        // 10,000 files in one directory must not add 10,000 map entries.
        agg.add_files(&[(1, PathBuf::from("f"))]);

        assert_eq!(agg.tracked_directories(), 3, "root, a, and a\\b");
        assert_eq!(agg.files(), 0, "add_files does not touch counters");
    }

    #[test]
    fn entries_render_human_units() {
        let mut agg = Aggregator::new(root(), 4, 4);
        agg.add_reports(&[report("a", 1024 * 1024 * 3, 2, 0)]);
        agg.add_extensions(&[(
            "mp4".to_string(),
            ExtTotals {
                bytes: 2048,
                files: 1,
            },
        )]);
        agg.add_files(&[(4096, dir("a/big.bin"))]);

        let dirs = agg.dir_entries(5, true);
        assert_eq!(dirs[0].size, "3.0 MB");

        let exts = agg.ext_entries(true);
        assert_eq!(exts[0].size, "2.0 KB");

        let files = agg.file_entries(true);
        assert_eq!(files[0].size, "4.0 KB");
        assert_eq!(files[0].path, dir("a/big.bin").to_string_lossy());
    }

    #[test]
    fn entries_can_render_raw_bytes() {
        let mut agg = Aggregator::new(root(), 4, 4);
        agg.add_reports(&[report("a", 1234567, 2, 0)]);
        assert_eq!(agg.dir_entries(5, false)[0].size, "1,234,567");
    }

    #[test]
    fn an_empty_aggregator_reports_nothing() {
        let agg = Aggregator::new(root(), 4, 4);
        assert_eq!(agg.bytes(), 0);
        assert_eq!(agg.files(), 0);
        assert_eq!(agg.directories(), 0);
        assert!(agg.top_files().is_empty());
        assert!(agg.extension_totals().is_empty());
    }

    #[test]
    fn totals_saturate_instead_of_overflowing() {
        let mut agg = Aggregator::new(root(), 4, 4);
        agg.add_reports(&[report("a", u64::MAX, 1, 0)]);
        agg.add_reports(&[report("a", u64::MAX, 1, 0)]);
        assert_eq!(agg.dir_totals(&dir("a")).unwrap().bytes, u64::MAX);
    }
}
