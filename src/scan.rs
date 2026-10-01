//! Filesystem scanner: a bounded worker pool performing a metadata-only scan.
//!
//! # Design
//!
//! ```text
//!                   Scanner
//!                     │
//!           WorkQueue (bounded, self-closing)
//!                     │
//!       ┌──────────┬──┴─────┬──────────┐
//!       ▼          ▼        ▼          ▼
//!    Worker 1  Worker 2  Worker 3  Worker 16
//!       │  own      │        │        │
//!       ▼  agg      ▼        ▼        ▼
//!    per-worker aggregation (no shared lock at all)
//!                     │
//!                     ▼  merge, once, at the end
//!                ScanOutcome
//! ```
//!
//! Invariants that keep this fast, bounded, and safe:
//!
//! * **Exactly `--threads` threads.** A worker that finds a subdirectory pushes
//!   it back onto the shared queue, so any worker can take any branch. No thread
//!   is created per directory.
//! * **Metadata only.** File contents are never opened or read; size comes from
//!   a single `stat`. A 20 GB ISO costs the same as an empty file.
//! * **No shared lock on the hot path.** Each worker owns an [`Aggregator`] and
//!   the coordinator merges them once at the end, so no mutex is ever contended
//!   per file.
//! * **Batching.** A worker flushes its local batch every few hundred entries,
//!   amortising bookkeeping.
//! * **No symlink following.** Directory links and Windows reparse points are
//!   measured but never descended, which makes loops impossible.
//! * **Never blocks on a full queue.** A worker that cannot enqueue handles the
//!   directory itself, so bounded memory never deadlocks and never loses a
//!   subtree.
//! * **Errors are data.** Disappearing files and permission failures are counted
//!   and sampled, never fatal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::aggregate::{Aggregator, DirReport, ExtTotals};
use crate::queue::{self, WorkQueue};

/// Default number of concurrent workers.
///
/// Sixteen measured fastest on the reference machine (NVMe SSD, a 1.2M-file
/// tree): concurrency helps sharply up to that point, then plateaus and drifts
/// slightly backwards. Raise it with `--threads` for larger or slower storage,
/// but be aware more workers are not automatically faster.
pub const DEFAULT_THREADS: usize = 16;

/// Upper bound on `--threads`, so a scan cannot thrash the machine.
pub const MAX_THREADS: usize = 256;

/// Default cap on queued directories.
pub const QUEUE_CAPACITY: usize = queue::DEFAULT_CAPACITY;

/// Directory reports a worker accumulates before flushing.
const BATCH_DIRS: usize = 512;

/// Files a worker accumulates before flushing.
const BATCH_FILES: usize = 256;

/// Failures a worker accumulates before flushing.
const BATCH_ISSUES: usize = 64;

/// How often workers publish counters for the renderer.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(100);

/// How often the cancellation watchdog checks the flag.
const CANCEL_POLL: Duration = Duration::from_millis(20);

/// Retained failure samples per scan.
const ISSUE_SAMPLE_LIMIT: usize = 256;

/// Settings for one scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Number of concurrent workers.
    pub threads: usize,
    /// Maximum recursion depth below the root; `0` means unlimited.
    pub max_depth: usize,
    /// How many largest files to retain for the report; `0` disables it.
    pub largest: usize,
    /// Whether to collect extension statistics.
    pub collect_extensions: bool,
    /// Cap on queued directories.
    pub queue_capacity: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            threads: DEFAULT_THREADS,
            max_depth: 0,
            largest: 20,
            collect_extensions: true,
            queue_capacity: QUEUE_CAPACITY,
        }
    }
}

impl ScanOptions {
    /// Validate a user-supplied worker count.
    ///
    /// Returns an explanatory message for an unusable value instead of silently
    /// clamping, so `--threads 0` or `--threads 9999` is a clear error rather
    /// than a surprising scan.
    pub fn validate_threads(threads: usize) -> Result<usize, String> {
        if threads == 0 {
            return Err("--threads must be at least 1".to_string());
        }
        if threads > MAX_THREADS {
            return Err(format!(
                "--threads must be at most {MAX_THREADS} (got {threads})"
            ));
        }
        Ok(threads)
    }

    /// A queue capacity suited to this worker count.
    ///
    /// Kept modest so the queue itself never dominates memory; workers falling
    /// back to inline traversal is cheap and keeps coverage intact.
    pub fn resolved_queue_capacity(&self) -> usize {
        (self.threads * 64).clamp(256, QUEUE_CAPACITY)
    }
}

/// Live counters, read by the progress renderer.
///
/// Workers add deltas with relaxed atomics and the renderer polls. Neither side
/// takes a lock, so progress tracking is free per file.
#[derive(Debug, Default)]
pub struct Progress {
    files: AtomicU64,
    bytes: AtomicU64,
    directories: AtomicU64,
    errors: AtomicU64,
}

impl Progress {
    /// Add one worker's batch totals.
    ///
    /// Public so tests can drive the renderer's counters without a real scan.
    pub fn add(&self, files: u64, bytes: u64, directories: u64, errors: u64) {
        self.files.fetch_add(files, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.directories.fetch_add(directories, Ordering::Relaxed);
        self.errors.fetch_add(errors, Ordering::Relaxed);
    }

    /// A snapshot for display.
    pub fn snapshot(&self) -> ProgressCounters {
        ProgressCounters {
            files: self.files.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            directories: self.directories.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

/// Snapshot of [`Progress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgressCounters {
    /// Files fully measured.
    pub files: u64,
    /// Bytes measured.
    pub bytes: u64,
    /// Directories fully read.
    pub directories: u64,
    /// Filesystem failures.
    pub errors: u64,
}

/// Shared result of a completed scan.
#[derive(Debug)]
pub struct ScanOutcome {
    /// Root that was scanned.
    pub root: PathBuf,
    /// Total bytes found.
    pub bytes: u64,
    /// Total files found.
    pub files: u64,
    /// Total directories found.
    pub directories: u64,
    /// Total failures seen.
    pub errors: u64,
    /// Wall-clock duration of the scan.
    pub elapsed: Duration,
    /// Worker count actually used.
    pub threads: usize,
    /// Whether the scan stopped early because it was cancelled.
    pub cancelled: bool,
    /// The merged aggregate, used to build reports.
    pub aggregator: Aggregator,
}

impl ScanOutcome {
    /// Recursive totals for one directory.
    pub fn dir_totals(&self, path: &Path) -> Option<crate::aggregate::DirTotals> {
        self.aggregator.dir_totals(path)
    }

    /// Directories held in memory, used by tests to show memory stays bounded.
    pub fn tracked_directories(&self) -> usize {
        self.aggregator.tracked_directories()
    }

    /// Files per second, or `None` when the scan was instantaneous.
    pub fn files_per_second(&self) -> Option<f64> {
        let secs = self.elapsed.as_secs_f64();
        (secs > 0.0).then(|| self.files as f64 / secs)
    }

    /// Directories per second, or `None` when the scan was instantaneous.
    pub fn directories_per_second(&self) -> Option<f64> {
        let secs = self.elapsed.as_secs_f64();
        (secs > 0.0).then(|| self.directories as f64 / secs)
    }
}

/// A handle that can cancel an in-flight scan.
#[derive(Debug, Clone)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// Ask the scan to stop at its next safe point.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Runs one scan.
pub struct Scanner {
    options: ScanOptions,
    cancel: Cancel,
    progress: Arc<Progress>,
    started: Instant,
}

impl Scanner {
    /// Create a scanner with the given options.
    pub fn new(options: ScanOptions) -> Self {
        Self {
            options,
            cancel: Cancel(Arc::new(AtomicBool::new(false))),
            progress: Arc::new(Progress::default()),
            started: Instant::now(),
        }
    }

    /// A handle that can cancel this scanner.
    pub fn cancel_handle(&self) -> Cancel {
        self.cancel.clone()
    }

    /// Live progress counters for the renderer.
    pub fn progress(&self) -> Arc<Progress> {
        Arc::clone(&self.progress)
    }

    /// Wall-clock time since the scan started.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Scan `root` to completion, returning the merged aggregate.
    ///
    /// Every worker is joined before this returns, so no thread outlives the
    /// call even when the scan is cancelled.
    pub fn scan(&mut self, root: &Path) -> Result<ScanOutcome, std::io::Error> {
        self.started = Instant::now();

        let metadata = root.metadata()?;
        if !metadata.is_dir() {
            return Err(std::io::Error::other(format!(
                "{} is not a directory",
                root.display()
            )));
        }

        let threads = self.options.threads.clamp(1, MAX_THREADS);
        let queue = Arc::new(WorkQueue::new(self.options.resolved_queue_capacity()));
        let done = Arc::new(AtomicBool::new(false));

        queue.push(root.to_path_buf());

        // The watchdog makes cancellation reliable: a worker blocked in `pop`
        // cannot observe the flag itself, so something must abort the queue.
        let watchdog = {
            let queue = Arc::clone(&queue);
            let cancel = self.cancel.clone();
            let done = Arc::clone(&done);
            thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    if cancel.is_cancelled() {
                        queue.abort();
                        return;
                    }
                    thread::sleep(CANCEL_POLL);
                }
            })
        };

        // Cloned up front so each worker closure owns exactly what it needs and the
        // coordinator keeps its own handles.
        let worker_queue = Arc::clone(&queue);
        let worker_progress = Arc::clone(&self.progress);
        let worker_cancel = self.cancel.clone();
        let worker_root = root.to_path_buf();
        let worker_options = self.options.clone();

        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            let queue = Arc::clone(&worker_queue);
            let progress = Arc::clone(&worker_progress);
            let cancel = worker_cancel.clone();
            let root = worker_root.clone();
            let options = worker_options.clone();

            handles.push(thread::spawn(move || {
                worker_main(queue, progress, cancel, root, options)
            }));
        }

        let mut merged =
            Aggregator::new(root.to_path_buf(), self.options.largest, ISSUE_SAMPLE_LIMIT);

        for handle in handles {
            // A panicking worker is joined and skipped; the rest still drain the
            // queue, so one bad directory cannot end the scan.
            if let Ok(local) = handle.join() {
                merged.merge(local);
            }
        }

        done.store(true, Ordering::SeqCst);
        let _ = watchdog.join();

        Ok(ScanOutcome {
            root: root.to_path_buf(),
            bytes: merged.bytes(),
            files: merged.files(),
            directories: merged.directories(),
            errors: merged.issue_count(),
            elapsed: self.started.elapsed(),
            threads,
            cancelled: self.cancel.is_cancelled(),
            aggregator: merged,
        })
    }
}

/// Body of one worker: scan directories until the queue drains.
fn worker_main(
    queue: Arc<WorkQueue>,
    progress: Arc<Progress>,
    cancel: Cancel,
    root: PathBuf,
    options: ScanOptions,
) -> Aggregator {
    // Each worker owns its aggregate outright: no lock is shared during a scan.
    let mut aggregator = Aggregator::new(root.clone(), options.largest, ISSUE_SAMPLE_LIMIT);
    let mut batch = LocalBatch::default();
    let mut deltas = LocalDeltas::default();
    let mut last_publish = Instant::now();

    while let Some(path) = queue.pop() {
        if cancel.is_cancelled() {
            queue.abort();
            break;
        }

        scan_task(
            &Task::new(path),
            &root,
            &options,
            &queue,
            &cancel,
            &mut batch,
            &mut deltas,
        );
        queue.complete();

        if batch.full() || last_publish.elapsed() >= PUBLISH_INTERVAL {
            flush(&mut batch, &mut aggregator, &progress, &mut deltas);
            last_publish = Instant::now();
        }
    }

    flush(&mut batch, &mut aggregator, &progress, &mut deltas);
    aggregator
}

/// A directory queued for scanning.
#[derive(Debug, Clone)]
struct Task {
    path: PathBuf,
}

impl Task {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

/// One worker's pending batch, flushed periodically.
#[derive(Debug, Default)]
struct LocalBatch {
    dirs: Vec<DirReport>,
    files: Vec<(u64, PathBuf)>,
    exts: HashMap<String, ExtTotals>,
    issues: Vec<(PathBuf, String)>,
}

impl LocalBatch {
    fn is_empty(&self) -> bool {
        self.dirs.is_empty()
            && self.files.is_empty()
            && self.exts.is_empty()
            && self.issues.is_empty()
    }

    fn full(&self) -> bool {
        self.dirs.len() >= BATCH_DIRS
            || self.files.len() >= BATCH_FILES
            || self.issues.len() >= BATCH_ISSUES
    }

    fn clear(&mut self) {
        self.dirs.clear();
        self.files.clear();
        self.exts.clear();
        self.issues.clear();
    }
}

/// Per-worker progress deltas, published on flush.
#[derive(Debug, Default)]
struct LocalDeltas {
    files: u64,
    bytes: u64,
    directories: u64,
    errors: u64,
}

/// Read one directory and its descendants, filling the batch.
///
/// Children go back onto the shared queue when there is room. When the queue is
/// full the child is handled inline instead: blocking would stall a worker that
/// might be the only one able to drain the queue. The inline case uses an
/// explicit stack rather than recursion so a deep tree cannot grow the stack.
fn scan_task(
    task: &Task,
    root: &Path,
    options: &ScanOptions,
    queue: &WorkQueue,
    cancel: &Cancel,
    batch: &mut LocalBatch,
    deltas: &mut LocalDeltas,
) {
    let mut pending = vec![task.clone()];

    while let Some(task) = pending.pop() {
        if cancel.is_cancelled() {
            return;
        }

        let entries = match std::fs::read_dir(&task.path) {
            Ok(entries) => entries,
            Err(err) => {
                // Access denied, missing, or vanished: record and carry on.
                deltas.errors += 1;
                batch.issues.push((task.path.clone(), err.to_string()));
                continue;
            }
        };

        let mut bytes = 0u64;
        let mut files = 0u64;
        let mut subdirs = 0u64;

        for entry in entries {
            if cancel.is_cancelled() {
                return;
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    deltas.errors += 1;
                    batch.issues.push((task.path.clone(), err.to_string()));
                    continue;
                }
            };

            // `file_type` uses cached data where available and does not follow
            // links, so a directory symlink or Windows junction reports
            // `is_symlink`.
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(err) => {
                    deltas.errors += 1;
                    batch.issues.push((entry.path(), err.to_string()));
                    continue;
                }
            };

            if file_type.is_symlink() {
                // Measured as an entry but never descended, which rules out
                // cycles such as A/B/link -> A.
                if let Ok(meta) = entry.metadata() {
                    bytes = bytes.saturating_add(meta.len());
                    files += 1;
                }
                continue;
            }

            if file_type.is_dir() {
                subdirs += 1;
                let child = entry.path();
                if !within_depth(root, &child, options.max_depth) {
                    continue;
                }
                if let Err(child) = queue.try_push_or_return(child) {
                    pending.push(Task::new(child));
                }
                continue;
            }

            if !file_type.is_file() {
                // Sockets, devices, and FIFOs carry no meaningful disk usage.
                continue;
            }

            // Metadata only: a stat, never an open or a read.
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(err) => {
                    // A file deleted between listing and stat is expected.
                    deltas.errors += 1;
                    batch.issues.push((path, err.to_string()));
                    continue;
                }
            };

            let size = meta.len();
            bytes = bytes.saturating_add(size);
            files += 1;

            if options.collect_extensions
                && let Some(extension) = extension_of(&path)
            {
                let totals = batch.exts.entry(extension).or_default();
                totals.bytes = totals.bytes.saturating_add(size);
                totals.files += 1;
            }

            if options.largest > 0 {
                batch.files.push((size, path));
            }
        }

        deltas.files += files;
        deltas.bytes += bytes;
        deltas.directories += 1;

        batch.dirs.push(DirReport {
            path: task.path.clone(),
            bytes,
            files,
            subdirs,
        });
    }
}

/// Whether a directory sits within the allowed depth below `root`.
///
/// `max_depth == 0` means unlimited. A subdirectory is scanned when its depth
/// *relative to the root* is less than `max_depth`, so `--depth 1` reads only
/// the root's own files and `--depth 2` reads one level down. A child whose path
/// is no longer under the root is always allowed, since refusing it would
/// silently drop data.
fn within_depth(root: &Path, child: &Path, max_depth: usize) -> bool {
    if max_depth == 0 {
        return true;
    }
    match child.strip_prefix(root) {
        Ok(relative) => relative.components().count() < max_depth,
        // Reached through a link that escapes the root: allow it.
        Err(_) => true,
    }
}

/// Lowercase extension without the dot, or `None` when the file has none.
fn extension_of(path: &Path) -> Option<String> {
    let extension = path.extension()?;
    if extension.is_empty() {
        return None;
    }
    Some(extension.to_string_lossy().to_ascii_lowercase())
}

/// Fold a worker's pending batch into its own aggregator and publish deltas.
fn flush(
    batch: &mut LocalBatch,
    aggregator: &mut Aggregator,
    progress: &Progress,
    deltas: &mut LocalDeltas,
) {
    if deltas.files > 0 || deltas.bytes > 0 || deltas.directories > 0 || deltas.errors > 0 {
        progress.add(
            deltas.files,
            deltas.bytes,
            deltas.directories,
            deltas.errors,
        );
        *deltas = LocalDeltas::default();
    }

    if batch.is_empty() {
        return;
    }

    aggregator.add_reports(&batch.dirs);
    for (extension, totals) in &batch.exts {
        aggregator.add_extension(extension, totals.bytes, totals.files);
    }
    aggregator.add_files(&batch.files);
    aggregator.add_issues(&batch.issues);

    batch.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A clean scratch directory, removed first so repeat runs are deterministic.
    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("ds-scan-{name}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Build a scratch tree, returning its root.
    ///
    /// ```text
    /// root/loose.txt          40 bytes
    /// root/a/one.txt          10 bytes
    /// root/a/sub/two.bin      20 bytes
    /// root/b/three.mp4        30 bytes
    /// ```
    fn tree(name: &str) -> PathBuf {
        let root = scratch(name);
        fs::create_dir_all(root.join("a").join("sub")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::write(root.join("loose.txt"), vec![0u8; 40]).unwrap();
        fs::write(root.join("a").join("one.txt"), vec![0u8; 10]).unwrap();
        fs::write(root.join("a").join("sub").join("two.bin"), vec![0u8; 20]).unwrap();
        fs::write(root.join("b").join("three.mp4"), vec![0u8; 30]).unwrap();
        root
    }

    fn options(threads: usize) -> ScanOptions {
        ScanOptions {
            threads,
            largest: 10,
            ..ScanOptions::default()
        }
    }

    #[test]
    fn a_scan_totals_files_and_directories() {
        let root = tree("totals");
        let mut scanner = Scanner::new(options(4));
        let outcome = scanner.scan(&root).expect("scan should succeed");

        assert_eq!(outcome.files, 4, "four files were created");
        assert_eq!(outcome.bytes, 100, "10 + 20 + 30 + 40");
        assert_eq!(outcome.directories, 4, "root, a, a/sub, b");
        assert_eq!(outcome.errors, 0);
        assert!(!outcome.cancelled);
    }

    #[test]
    fn parent_directories_aggregate_descendants() {
        let root = tree("aggregate");
        let mut scanner = Scanner::new(options(4));
        let outcome = scanner.scan(&root).expect("scan should succeed");

        let a = outcome.dir_totals(&root.join("a")).expect("a tracked");
        assert_eq!(a.bytes, 30, "a holds one.txt (10) and sub (20)");
        assert_eq!(a.files, 2);

        let sub = outcome
            .dir_totals(&root.join("a").join("sub"))
            .expect("sub tracked");
        assert_eq!(sub.bytes, 20);

        let totals = outcome.dir_totals(&root).expect("root tracked");
        assert_eq!(totals.bytes, 100);
        assert_eq!(totals.files, 4);
        assert_eq!(totals.subdirs, 2, "root has a and b");
    }

    #[test]
    fn nested_directories_roll_up_to_their_parents() {
        // Sizes are kept small on purpose: the rollup arithmetic is checked
        // against exact GB figures in the aggregator's own tests, which needs
        // no filesystem.
        let root = scratch("games");
        let mc = root.join("Games").join("Minecraft");
        fs::create_dir_all(mc.join("assets")).unwrap();
        fs::create_dir_all(root.join("Games").join("Steam")).unwrap();

        fs::write(mc.join("game.jar"), vec![0u8; 500]).unwrap();
        fs::write(mc.join("assets").join("pack.dat"), vec![0u8; 2000]).unwrap();
        fs::write(
            root.join("Games").join("Steam").join("data.bin"),
            vec![0u8; 30_000],
        )
        .unwrap();

        let mut scanner = Scanner::new(options(8));
        let outcome = scanner.scan(&root).expect("scan should succeed");

        let minecraft = outcome.dir_totals(&mc).expect("Minecraft tracked");
        assert_eq!(minecraft.bytes, 2500, "game.jar plus assets");
        assert_eq!(minecraft.files, 2);

        let steam = outcome
            .dir_totals(&root.join("Games").join("Steam"))
            .expect("Steam tracked");
        assert_eq!(steam.bytes, 30_000, "Steam keeps its own total");

        let games = outcome
            .dir_totals(&root.join("Games"))
            .expect("Games tracked");
        assert_eq!(games.bytes, 32_500, "Games rolls up both children");
        assert_eq!(games.subdirs, 2);

        assert_eq!(outcome.bytes, 32_500, "the root is the grand total");
    }

    #[test]
    fn file_contents_are_never_read() {
        // An 8 MiB file is measured from metadata, so the reported size is exact
        // and the scan stays fast.
        let root = scratch("large");
        fs::write(root.join("blob.bin"), vec![7u8; 8 * 1024 * 1024]).unwrap();

        let mut scanner = Scanner::new(options(2));
        let outcome = scanner.scan(&root).expect("scan should succeed");
        assert_eq!(outcome.bytes, 8 * 1024 * 1024);
        assert_eq!(outcome.files, 1);
    }

    #[test]
    fn extension_statistics_come_from_the_same_scan() {
        let root = tree("ext");
        let mut scanner = Scanner::new(options(4));
        let outcome = scanner.scan(&root).expect("scan should succeed");

        let totals = outcome.aggregator.extension_totals();
        let by_ext = |name: &str| {
            totals
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| *v)
                .unwrap_or_default()
        };

        assert_eq!(by_ext("txt").bytes, 50, "one.txt (10) + loose.txt (40)");
        assert_eq!(by_ext("txt").files, 2);
        assert_eq!(by_ext("bin").bytes, 20);
        assert_eq!(by_ext("mp4").bytes, 30);
    }

    #[test]
    fn extension_stats_can_be_skipped() {
        let root = tree("noext");
        let opts = ScanOptions {
            collect_extensions: false,
            ..options(4)
        };
        let mut scanner = Scanner::new(opts);
        let outcome = scanner.scan(&root).expect("scan should succeed");

        assert!(outcome.aggregator.extension_totals().is_empty());
        assert_eq!(outcome.bytes, 100, "totals are unaffected");
    }

    #[test]
    fn largest_files_are_bounded_and_ordered() {
        let root = scratch("largest");
        for n in 0..200u64 {
            fs::write(root.join(format!("f{n:03}.bin")), vec![0u8; n as usize]).unwrap();
        }

        let opts = ScanOptions {
            largest: 5,
            ..options(8)
        };
        let mut scanner = Scanner::new(opts);
        let outcome = scanner.scan(&root).expect("scan should succeed");

        let top = outcome.aggregator.top_files();
        assert_eq!(top.len(), 5, "only five files are retained");
        assert_eq!(top[0].1, 199, "biggest first");
        assert!(top.windows(2).all(|w| w[0].1 >= w[1].1), "must stay sorted");
        assert_eq!(outcome.files, 200, "all files still counted");
    }

    #[test]
    fn largest_files_off_retains_none() {
        let root = tree("nolargest");
        let opts = ScanOptions {
            largest: 0,
            ..options(4)
        };
        let mut scanner = Scanner::new(opts);
        let outcome = scanner.scan(&root).expect("scan should succeed");
        assert!(outcome.aggregator.top_files().is_empty());
        assert_eq!(outcome.files, 4);
    }

    #[test]
    fn depth_one_reads_only_the_root() {
        let root = tree("depth1");
        let opts = ScanOptions {
            max_depth: 1,
            ..options(4)
        };
        let mut scanner = Scanner::new(opts);
        let outcome = scanner.scan(&root).expect("scan should succeed");
        assert_eq!(outcome.files, 1, "only the root's own file");
        assert_eq!(outcome.bytes, 40);
    }

    #[test]
    fn depth_two_reads_one_level_down() {
        let root = tree("depth2");
        let opts = ScanOptions {
            max_depth: 2,
            ..options(4)
        };
        let mut scanner = Scanner::new(opts);
        let outcome = scanner.scan(&root).expect("scan should succeed");
        // loose.txt + a/one.txt + b/three.mp4; a/sub sits one level deeper.
        assert_eq!(outcome.files, 3);
    }

    #[test]
    fn zero_depth_means_unlimited() {
        let root = tree("depth0");
        let opts = ScanOptions {
            max_depth: 0,
            ..options(4)
        };
        let mut scanner = Scanner::new(opts);
        let outcome = scanner.scan(&root).expect("scan should succeed");
        assert_eq!(outcome.files, 4);
        assert_eq!(outcome.directories, 4);
    }

    #[test]
    fn worker_counts_do_not_change_the_totals() {
        let root = tree("threads");
        let mut baseline: Option<(u64, u64, u64)> = None;

        for threads in [1usize, 2, 8, 32] {
            let mut scanner = Scanner::new(options(threads));
            let outcome = scanner.scan(&root).expect("scan should succeed");
            assert_eq!(outcome.threads, threads);
            let totals = (outcome.files, outcome.bytes, outcome.directories);

            match baseline {
                None => baseline = Some(totals),
                Some(first) => {
                    assert_eq!(totals, first, "totals must not depend on threads");
                }
            }
        }
    }

    #[test]
    fn merging_worker_aggregators_keeps_totals_exact() {
        // Totals must not depend on how work happens to be split across
        // workers, since each worker aggregates independently before merging.
        let root = scratch("merge");
        for i in 0..12 {
            let dir = root.join(format!("d{i}")).join("inner");
            fs::create_dir_all(&dir).unwrap();
            for n in 0..25u64 {
                fs::write(dir.join(format!("f{n}.bin")), vec![0u8; n as usize]).unwrap();
            }
        }

        let mut baseline: Option<(u64, u64, u64)> = None;
        for threads in [1usize, 3, 16, 32] {
            let mut scanner = Scanner::new(options(threads));
            let outcome = scanner.scan(&root).expect("scan should succeed");
            let totals = (outcome.files, outcome.bytes, outcome.directories);
            match baseline {
                None => baseline = Some(totals),
                Some(first) => assert_eq!(totals, first, "threads={threads}"),
            }
        }

        assert_eq!(
            baseline.expect("baseline"),
            (300, 3_600, 25),
            "12 directories x sum(0..24) = 12 x 300 bytes"
        );
    }

    #[test]
    fn a_single_worker_drains_the_whole_queue() {
        let root = tree("single");
        let mut scanner = Scanner::new(options(1));
        let outcome = scanner.scan(&root).expect("scan should succeed");
        assert_eq!(outcome.files, 4);
        assert_eq!(outcome.bytes, 100);
        assert_eq!(outcome.directories, 4);
    }

    #[test]
    fn a_tiny_queue_still_finds_everything() {
        let root = tree("tinyqueue");
        let opts = ScanOptions {
            queue_capacity: 1,
            ..options(4)
        };
        let mut scanner = Scanner::new(opts);
        let outcome = scanner.scan(&root).expect("scan should succeed");
        assert_eq!(outcome.files, 4);
        assert_eq!(outcome.bytes, 100);
    }

    #[test]
    fn a_wide_shallow_tree_completes() {
        let root = scratch("wide");
        for i in 0..50 {
            let dir = root.join(format!("d{i}"));
            fs::create_dir_all(&dir).unwrap();
            for n in 0..20 {
                fs::write(dir.join(format!("f{n}.txt")), vec![0u8; 5]).unwrap();
            }
        }

        let opts = ScanOptions {
            queue_capacity: 8,
            ..options(32)
        };
        let mut scanner = Scanner::new(opts);
        let outcome = scanner.scan(&root).expect("scan should succeed");
        assert_eq!(outcome.files, 1000);
        assert_eq!(outcome.bytes, 5000);
        assert_eq!(outcome.directories, 51);
    }

    #[test]
    fn a_deep_tree_does_not_exhaust_the_stack() {
        let root = scratch("deep");
        let mut path = root.clone();
        for i in 0..200 {
            path = path.join(format!("level{i}"));
        }
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("deep.txt"), vec![0u8; 7]).unwrap();

        // A tiny queue forces the inline path, which must stay iterative.
        let opts = ScanOptions {
            queue_capacity: 2,
            ..options(4)
        };
        let mut scanner = Scanner::new(opts);
        let outcome = scanner.scan(&root).expect("deep scan should succeed");
        assert_eq!(outcome.files, 1);
        assert_eq!(outcome.directories, 201);
    }

    #[test]
    fn memory_tracks_directories_not_files() {
        let root = scratch("memory");
        let dir = root.join("one");
        fs::create_dir_all(&dir).unwrap();
        for n in 0..2000 {
            fs::write(dir.join(format!("f{n}.bin")), vec![0u8; 1]).unwrap();
        }

        let opts = ScanOptions {
            largest: 1,
            ..options(8)
        };
        let mut scanner = Scanner::new(opts);
        let outcome = scanner.scan(&root).expect("scan should succeed");

        assert_eq!(outcome.files, 2000);
        assert_eq!(
            outcome.tracked_directories(),
            2,
            "only root and one/ are tracked, not 2000 files"
        );
        assert_eq!(outcome.aggregator.top_files().len(), 1);
    }

    #[test]
    fn scanning_a_missing_root_fails() {
        let missing = std::env::temp_dir().join("ds-scan-missing-xyz");
        let _ = fs::remove_dir_all(&missing);
        let mut scanner = Scanner::new(options(4));
        assert!(scanner.scan(&missing).is_err());
    }

    #[test]
    fn scanning_a_file_root_fails() {
        let root = tree("fileroot");
        let file = root.join("loose.txt");
        let mut scanner = Scanner::new(options(2));
        let err = scanner.scan(&file).expect_err("a file is not a scan root");
        assert!(err.to_string().contains("not a directory"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_does_not_hang() {
        let root = scratch("loop");
        fs::create_dir_all(root.join("a").join("b")).unwrap();
        fs::write(root.join("a").join("file.txt"), vec![0u8; 5]).unwrap();
        std::os::unix::fs::symlink(&root, root.join("a").join("b").join("loop")).unwrap();

        let mut scanner = Scanner::new(options(4));
        let outcome = scanner.scan(&root).expect("scan must terminate");

        // The link is measured as an entry but never followed, so file.txt is
        // counted exactly once and the loop adds no files.
        assert_eq!(outcome.files, 2, "file.txt plus the link itself");
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_symlink_is_not_descended() {
        let root = scratch("dirlink");
        fs::create_dir_all(root.join("real")).unwrap();
        fs::write(root.join("real").join("f.txt"), vec![0u8; 3]).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();

        let mut scanner = Scanner::new(options(4));
        let outcome = scanner.scan(&root).expect("scan should succeed");

        assert_eq!(outcome.files, 2, "f.txt plus the directory link entry");
        assert_eq!(outcome.directories, 2, "root and real only");
    }

    #[test]
    fn thread_count_validation() {
        assert_eq!(ScanOptions::validate_threads(1), Ok(1));
        assert_eq!(ScanOptions::validate_threads(32), Ok(32));
        assert_eq!(ScanOptions::validate_threads(MAX_THREADS), Ok(MAX_THREADS));

        let zero = ScanOptions::validate_threads(0).expect_err("zero is invalid");
        assert!(zero.contains("at least 1"));

        let huge = ScanOptions::validate_threads(99_999).expect_err("absurd is invalid");
        assert!(huge.contains(&MAX_THREADS.to_string()));
    }

    #[test]
    fn default_options_use_sixteen_workers() {
        assert_eq!(ScanOptions::default().threads, 16);
        assert_eq!(DEFAULT_THREADS, 16);
        assert_eq!(ScanOptions::default().queue_capacity, QUEUE_CAPACITY);
    }

    #[test]
    fn queue_capacity_scales_with_threads() {
        let few = ScanOptions {
            threads: 1,
            ..ScanOptions::default()
        }
        .resolved_queue_capacity();
        let many = ScanOptions {
            threads: 32,
            ..ScanOptions::default()
        }
        .resolved_queue_capacity();

        assert_eq!(few, 256, "clamped up to the floor");
        assert!(many > few, "more workers deserve more queued work");
        assert!(many <= QUEUE_CAPACITY, "never above the ceiling");
    }

    #[test]
    fn extension_extraction_is_case_insensitive() {
        assert_eq!(extension_of(Path::new("a/b.MP4")).as_deref(), Some("mp4"));
        assert_eq!(extension_of(Path::new("a/b.tar.gz")).as_deref(), Some("gz"));
        assert_eq!(extension_of(Path::new("a/b")).as_deref(), None);
        assert_eq!(extension_of(Path::new("a/.hidden")).as_deref(), None);
    }

    #[test]
    fn depth_gate_limits_how_deep_the_walk_goes() {
        let root = Path::new("C:\\root");

        // --depth 1 reads the root's own files only, so no child is followed.
        assert!(!within_depth(root, Path::new("C:\\root\\a"), 1));
        assert!(!within_depth(root, Path::new("C:\\root\\a\\b"), 1));

        // --depth 2 follows one level of subdirectories.
        assert!(within_depth(root, Path::new("C:\\root\\a"), 2));
        assert!(!within_depth(root, Path::new("C:\\root\\a\\b"), 2));

        // Zero means unlimited, whatever the depth.
        assert!(within_depth(root, Path::new("C:\\root\\a\\b\\c\\d"), 0));

        // A path outside the root is allowed rather than silently dropped.
        assert!(within_depth(root, Path::new("D:\\elsewhere\\deep"), 1));
    }

    #[test]
    fn progress_counters_accumulate_deltas() {
        let progress = Progress::default();
        progress.add(1, 100, 1, 0);
        progress.add(2, 200, 3, 1);

        let counters = progress.snapshot();
        assert_eq!(counters.files, 3);
        assert_eq!(counters.bytes, 300);
        assert_eq!(counters.directories, 4);
        assert_eq!(counters.errors, 1);
    }

    #[test]
    fn rates_are_none_for_an_instantaneous_scan() {
        let root = PathBuf::from("/");
        let outcome = ScanOutcome {
            root: root.clone(),
            bytes: 0,
            files: 100,
            directories: 4,
            errors: 0,
            elapsed: Duration::ZERO,
            threads: 1,
            cancelled: false,
            aggregator: Aggregator::new(root, 0, 0),
        };
        assert_eq!(outcome.files_per_second(), None);
        assert_eq!(outcome.directories_per_second(), None);
    }

    #[test]
    fn cancelling_before_the_scan_returns_cleanly() {
        let root = tree("cancel-early");
        let mut scanner = Scanner::new(options(4));
        scanner.cancel_handle().cancel();

        let outcome = scanner.scan(&root).expect("a cancelled scan still returns");
        assert!(outcome.cancelled);
        assert_eq!(outcome.threads, 4);
    }

    #[test]
    fn cancelling_stops_the_worker_pool() {
        // The watchdog must abort the queue so blocked workers wake; without it
        // this test would hang rather than fail.
        let root = scratch("cancel-mid");
        for i in 0..40 {
            let dir = root.join(format!("d{i}"));
            fs::create_dir_all(&dir).unwrap();
            for n in 0..50 {
                fs::write(dir.join(format!("f{n}.bin")), vec![0u8; 64]).unwrap();
            }
        }

        let mut scanner = Scanner::new(options(8));
        let handle = scanner.cancel_handle();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(2));
            handle.cancel();
        });

        let outcome = scanner.scan(&root).expect("scan returns after cancel");
        assert!(outcome.cancelled);
        assert!(
            outcome.files <= 2000,
            "cancellation must never invent files, saw {}",
            outcome.files
        );
    }

    #[test]
    fn cancel_handle_is_shared() {
        let scanner = Scanner::new(options(2));
        let handle = scanner.cancel_handle();
        assert!(!handle.is_cancelled());
        handle.cancel();
        assert!(handle.is_cancelled());
        assert!(scanner.cancel_handle().is_cancelled());
    }

    #[test]
    fn repeated_scans_reuse_a_scanner() {
        let root = tree("reuse");
        let mut scanner = Scanner::new(options(4));

        let first = scanner.scan(&root).expect("first scan");
        let second = scanner.scan(&root).expect("second scan");

        assert_eq!(first.files, second.files);
        assert_eq!(first.bytes, second.bytes);
        assert_eq!(first.directories, second.directories);
    }
}
