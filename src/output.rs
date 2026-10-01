//! Rendering drives into tables or JSON.

use std::io::IsTerminal;
use std::io::Write;

use crate::aggregate;
use crate::cli::{Cli, SortKey};
use crate::disk::{self, DiskUsage, Drive};
use crate::format;
use crate::scan;
use crate::table::{self, Row};

/// A drive prepared for display or serialisation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Entry {
    /// Display label for the drive.
    pub drive: String,
    /// Total bytes on the volume.
    pub total_bytes: u64,
    /// Free bytes available to the current user.
    pub free_bytes: u64,
    /// Used bytes.
    pub used_bytes: u64,
    /// Percentage of the volume in use.
    pub usage_percent: f64,
    /// Usage percentage as a display string.
    pub usage: String,
    /// Used space as a display string.
    pub used: String,
    /// Free space as a display string.
    pub free: String,
    /// Total space as a display string.
    pub total: String,
}

impl Entry {
    /// Build a display entry from raw usage figures.
    fn new(drive: &Drive, cli: &Cli) -> Self {
        let usage = drive.usage;
        let human = cli.use_human_units();
        Self {
            drive: drive.label().to_string(),
            total_bytes: usage.total,
            free_bytes: usage.available,
            used_bytes: usage.used(),
            usage_percent: round(usage.usage_percent(), cli.precision),
            usage: format::format_percent(usage.usage_percent(), cli.precision),
            used: format::format_bytes(usage.used(), human),
            free: format::format_bytes(usage.available, human),
            total: format::format_bytes(usage.total, human),
        }
    }
}

/// Build a single volume row for display.
///
/// Used by the scan path, which prints the target's capacity above the analysis.
pub fn volume_entry(drive: &Drive, cli: &Cli) -> Entry {
    Entry::new(drive, cli)
}

/// Round to `decimals` places, keeping JSON output tidy.
fn round(value: f64, decimals: usize) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

/// Order entries by the requested key, returning a new vector.
///
/// Ties break on drive label so repeated runs produce identical output.
/// `descending` flips the result, which is what `-r` selects.
pub fn sort(mut entries: Vec<Entry>, key: SortKey, descending: bool) -> Vec<Entry> {
    entries.sort_by(|a, b| {
        let ordering = match key {
            SortKey::Drive => a.drive.cmp(&b.drive),
            SortKey::Free => a.free_bytes.cmp(&b.free_bytes),
            SortKey::Used => a.used_bytes.cmp(&b.used_bytes),
            SortKey::Total => a.total_bytes.cmp(&b.total_bytes),
            SortKey::Usage => a
                .usage_percent
                .partial_cmp(&b.usage_percent)
                .unwrap_or(std::cmp::Ordering::Equal),
            // Scan-only keys. Volume listings ignore them rather than failing,
            // so `ds --sort size` still lists drives.
            SortKey::Size | SortKey::Files => a.drive.cmp(&b.drive),
        };
        ordering.then_with(|| a.drive.cmp(&b.drive))
    });

    if descending {
        entries.reverse();
    }

    entries
}

/// Collect usage for the drive named on the command line, or for all drives.
///
/// Returns the entries in display order, or every failure encountered.
pub fn collect(cli: &Cli) -> Result<Vec<Entry>, Vec<disk::DriveError>> {
    let (entries, errors) = match &cli.drive {
        Some(spec) => {
            // Volume listing reports the volume's own figures, so a subdirectory
            // argument collapses to its drive root.
            let path = disk::volume_root(std::path::Path::new(spec));
            match disk::query(&path) {
                Ok(usage) => {
                    let drive = Drive {
                        name: path.to_string_lossy().into_owned(),
                        path,
                        usage,
                    };
                    (vec![Entry::new(&drive, cli)], Vec::new())
                }
                Err(message) => (
                    Vec::new(),
                    vec![disk::DriveError {
                        target: path.to_string_lossy().into_owned(),
                        message,
                    }],
                ),
            }
        }
        None => {
            let drives = disk::list();
            let entries = drives.iter().map(|drive| Entry::new(drive, cli)).collect();
            (entries, Vec::new())
        }
    };

    if !errors.is_empty() {
        return Err(errors);
    }

    let (key, descending) = cli.sort_order();
    Ok(sort(entries, key, descending))
}

/// Render entries as a table, colouring the usage column when enabled.
///
/// Colour is suppressed when stdout is not a terminal, so redirected output
/// stays free of escape codes.
pub fn render_table(entries: &[Entry], cli: &Cli) -> String {
    render_table_with(entries, cli, std::io::stdout().is_terminal())
}

/// Render entries as a table, with the terminal check supplied by the caller.
pub fn render_table_with(entries: &[Entry], cli: &Cli, is_terminal: bool) -> String {
    let rows: Vec<Row> = entries
        .iter()
        .map(|entry| Row {
            drive: entry.drive.clone(),
            used: entry.used.clone(),
            free: entry.free.clone(),
            total: entry.total.clone(),
            usage: entry.usage.clone(),
        })
        .collect();

    let mut table = table::build(rows, cli.table_style());

    if is_terminal && cli.use_color() {
        let warn_above = cli.warn_above;
        let human = cli.use_human_units();
        table::colorize(&mut table, table::USAGE_COLUMN, |row| {
            let index = row.saturating_sub(1);
            let percent = entries.get(index).map_or(0.0, |e| e.usage_percent);
            table::color_usage(percent, warn_above, human)
        });
    }

    table.to_string()
}

/// The scan result, serialised for `--json`.
///
/// Directory, extension, and file sections all come from the same aggregated
/// scan, so JSON never triggers a second pass over the filesystem.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanReport {
    /// Volume capacity for the target, when it could be measured. Absent for a
    /// subdirectory that is not a volume root.
    pub volume: Option<Entry>,
    /// Root that was scanned.
    pub root: String,
    /// Total bytes found by the scan.
    pub total_bytes: u64,
    /// Total files found.
    pub files: u64,
    /// Total directories read.
    pub directories: u64,
    /// Filesystem failures seen.
    pub errors: u64,
    /// Wall-clock scan duration in milliseconds.
    pub elapsed_ms: u128,
    /// Workers used.
    pub threads: usize,
    /// Whether the scan was cancelled early.
    pub cancelled: bool,
    /// Largest directories, biggest first.
    pub top_directories: Vec<aggregate::DirEntry>,
    /// Largest files, biggest first.
    pub largest_files: Vec<aggregate::FileEntry>,
    /// Extension totals, biggest first. Empty unless `--ext` was passed.
    pub extensions: Vec<aggregate::ExtEntry>,
    /// Retained samples of paths that could not be inspected.
    pub issues: Vec<aggregate::ScanIssue>,
}

/// Build the JSON report from a finished scan.
///
/// `volume` carries the capacity row when the target is a measurable volume, so
/// `--json` stays a single valid document instead of a table followed by JSON.
pub fn scan_report(outcome: &scan::ScanOutcome, cli: &Cli, volume: Option<Entry>) -> ScanReport {
    let human = cli.use_human_units();
    let aggregator = &outcome.aggregator;

    ScanReport {
        volume,
        root: outcome.root.to_string_lossy().into_owned(),
        total_bytes: outcome.bytes,
        files: outcome.files,
        directories: outcome.directories,
        errors: outcome.errors,
        elapsed_ms: outcome.elapsed.as_millis(),
        threads: outcome.threads,
        cancelled: outcome.cancelled,
        top_directories: aggregator.dir_entries(cli.dirs, human),
        largest_files: aggregator.file_entries(human),
        extensions: if cli.ext {
            aggregator.ext_entries(human)
        } else {
            Vec::new()
        },
        issues: aggregator.issues().to_vec(),
    }
}

/// Render a scan report as JSON.
pub fn render_scan_json(outcome: &scan::ScanOutcome, cli: &Cli, volume: Option<Entry>) -> String {
    serde_json::to_string_pretty(&scan_report(outcome, cli, volume))
        .unwrap_or_else(|_| "{}".to_string())
}

/// Render the scan as text: a summary line, then the requested tables.
pub fn render_scan(outcome: &scan::ScanOutcome, cli: &Cli, is_terminal: bool) -> String {
    let human = cli.use_human_units();
    let aggregator = &outcome.aggregator;
    let style = cli.table_style();
    let color = is_terminal && cli.use_color();

    let mut out = String::new();
    out.push_str(&summary_line(outcome, human));
    out.push('\n');

    // Largest directories. The aggregator returns biggest-first, which is what
    // `--sort size` asks for; other keys reorder explicitly.
    let dirs = order_dirs(aggregator.dir_entries(cli.dirs, human), cli);
    let dir_rows: Vec<table::DirRow> = dirs
        .iter()
        .map(|entry| table::DirRow {
            files: format::group_count(entry.files),
            size: entry.size.clone(),
            path: entry.path.clone(),
        })
        .collect();

    if !dir_rows.is_empty() {
        let mut built = table::build_dirs(dir_rows, style);
        if color {
            color_by_share(
                &mut built,
                dirs.iter().map(|entry| entry.size_bytes),
                cli.warn_above,
            );
        }
        out.push_str(&built.to_string());
        out.push('\n');
    } else {
        out.push_str("No subdirectories found.\n");
    }

    if cli.ext {
        let exts = aggregator.ext_entries(human);
        let ext_rows: Vec<table::ExtRow> = exts
            .iter()
            .take(cli.dirs.max(5))
            .map(|entry| table::ExtRow {
                files: format::group_count(entry.files),
                size: entry.size.clone(),
                extension: entry.extension.clone(),
            })
            .collect();

        if !ext_rows.is_empty() {
            let mut built = table::build_exts(ext_rows, style);
            if color {
                color_by_share(
                    &mut built,
                    exts.iter()
                        .take(cli.dirs.max(5))
                        .map(|entry| entry.size_bytes),
                    cli.warn_above,
                );
            }
            out.push_str(&built.to_string());
            out.push('\n');
        }
    }

    if cli.largest > 0 {
        let files = aggregator.file_entries(human);
        let kept: Vec<&aggregate::FileEntry> = files.iter().take(cli.largest).collect();
        let file_rows: Vec<table::FileRow> = kept
            .iter()
            .map(|entry| table::FileRow {
                size: entry.size.clone(),
                path: entry.path.clone(),
            })
            .collect();

        if !file_rows.is_empty() {
            let mut built = table::build_files(file_rows, style);
            if color {
                color_by_share(
                    &mut built,
                    kept.iter().map(|entry| entry.size_bytes),
                    cli.warn_above,
                );
            }
            out.push_str(&built.to_string());
            out.push('\n');
        }
    }

    if outcome.errors > 0 {
        out.push_str(&format!(
            "[!] {} path(s) could not be read; the scan is incomplete.\n",
            format::group_count(outcome.errors)
        ));
    }

    out
}

/// A one-line description of what the scan found.
fn summary_line(outcome: &scan::ScanOutcome, human: bool) -> String {
    let rate = outcome.files_per_second().map_or_else(
        || "-".to_string(),
        |value| format::group_count(value.round() as u64),
    );

    format!(
        "Scanned {}: {} in {}, {} files, {} directories, {} workers ({}/s)",
        outcome.root.display(),
        format::format_bytes(outcome.bytes, human),
        format::format_duration(outcome.elapsed),
        format::group_count(outcome.files),
        format::group_count(outcome.directories),
        outcome.threads,
        rate,
    )
}

/// Colour a size column by each row's share of the biggest row.
///
/// Rows are already ordered biggest-first, so the first value is the reference.
fn color_by_share(
    table: &mut tabled::Table,
    sizes: impl Iterator<Item = u64>,
    warn_above: Option<f64>,
) {
    let sizes: Vec<u64> = sizes.collect();
    let Some(largest) = sizes.first().copied().filter(|value| *value > 0) else {
        return;
    };

    let shares: Vec<f64> = sizes
        .iter()
        .map(|bytes| (*bytes as f64 / largest as f64) * 100.0)
        .collect();

    table::colorize(table, table::SIZE_COLUMN, |row| {
        let index = row.saturating_sub(1);
        table::color_usage(shares.get(index).copied().unwrap_or(0.0), warn_above, true)
    });
}

/// Apply the directory sort key to an already size-ordered list.
///
/// The aggregator hands back the biggest first, which is what `--sort size`
/// wants, so only `--sort files` needs an explicit reordering.
fn order_dirs(entries: Vec<aggregate::DirEntry>, cli: &Cli) -> Vec<aggregate::DirEntry> {
    let (_, descending) = cli.sort_order();

    if cli.sort != SortKey::Files {
        return entries;
    }

    let mut sorted = entries;
    sorted.sort_by(|a, b| {
        b.files
            .cmp(&a.files)
            .then_with(|| b.size_bytes.cmp(&a.size_bytes))
    });
    if descending {
        sorted.reverse();
    }
    sorted
}

/// Render entries as a JSON document.
pub fn render_json(entries: &[Entry]) -> String {
    serde_json::to_string_pretty(entries).unwrap_or_else(|_| "[]".to_string())
}

/// Write a rendered table, optionally annotated with a usage bar.
pub fn write_table(out: &mut impl Write, entries: &[Entry], cli: &Cli) -> std::io::Result<()> {
    writeln!(out, "{}", render_table(entries, cli))
}

/// Write a rendered JSON document with a trailing newline.
pub fn write_json(out: &mut impl Write, entries: &[Entry]) -> std::io::Result<()> {
    writeln!(out, "{}", render_json(entries))
}

/// Percentage of the volume in use, used by the watch footer.
pub fn usage_of(entry: &Entry) -> f64 {
    entry.usage_percent
}

/// Byte figures for a drive, retained for callers that need the raw values.
pub fn usage_for(drive: &Drive) -> DiskUsage {
    drive.usage
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::parse_from;
    use crate::scan::{ScanOptions, Scanner};
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    fn cli(args: &[&str]) -> Cli {
        parse_from(args).expect("args should parse")
    }

    /// A clean scratch directory, removed first so repeat runs are deterministic.
    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("ds-out-{name}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// A small tree, scanned once and reused by the rendering tests.
    ///
    /// ```text
    /// root/videos/clip.mp4    3000
    /// root/videos/trailer.mov 2000
    /// root/notes/readme.txt    500
    /// root/top.bin            4000
    /// ```
    fn scanned(name: &str) -> (scan::ScanOutcome, PathBuf) {
        let root = scratch(name);
        fs::create_dir_all(root.join("videos")).unwrap();
        fs::create_dir_all(root.join("notes")).unwrap();
        fs::write(root.join("videos").join("clip.mp4"), vec![0u8; 3000]).unwrap();
        fs::write(root.join("videos").join("trailer.mov"), vec![0u8; 2000]).unwrap();
        fs::write(root.join("notes").join("readme.txt"), vec![0u8; 500]).unwrap();
        fs::write(root.join("top.bin"), vec![0u8; 4000]).unwrap();

        let mut scanner = Scanner::new(ScanOptions {
            threads: 4,
            ..ScanOptions::default()
        });
        let outcome = scanner.scan(&root).expect("scan should succeed");
        (outcome, root)
    }

    fn entry(drive: &str, total: u64, free: u64) -> Entry {
        let usage = DiskUsage {
            available: free,
            total,
        };
        let c = cli(&["ds", "--human"]);
        Entry::new(
            &Drive {
                name: drive.to_string(),
                path: std::path::PathBuf::from(drive),
                usage,
            },
            &c,
        )
    }

    #[test]
    fn computes_used_free_and_total_strings() {
        let e = entry("C:\\", 160_000_000_000, 3_200_000_000);
        assert_eq!(e.used_bytes, 156_800_000_000);
        assert!(e.used.ends_with(" GB"));
        assert!(e.total.ends_with(" GB"));
        assert_eq!(e.usage, "98.0%");
    }

    #[test]
    fn rounds_usage_to_the_requested_precision() {
        let c = cli(&["ds", "--human", "--precision", "2"]);
        let e = Entry::new(
            &Drive {
                name: "C:\\".to_string(),
                path: std::path::PathBuf::from("C:\\"),
                usage: DiskUsage {
                    available: 3_100_000_000,
                    total: 149_400_000_000,
                },
            },
            &c,
        );
        assert_eq!(e.usage, "97.93%");
        assert!((e.usage_percent - 97.93).abs() < 0.001);
    }

    #[test]
    fn zero_precision_rounds_to_whole_percent() {
        assert_eq!(round(97.94, 0), 98.0);
        assert_eq!(round(97.44, 0), 97.0);
    }

    #[test]
    fn sorting_by_free_is_ascending() {
        let entries = vec![
            entry("C:\\", 100, 10),
            entry("D:\\", 100, 50),
            entry("E:\\", 100, 30),
        ];
        let sorted = sort(entries, SortKey::Free, false);
        assert_eq!(label(&sorted), ["C:\\", "E:\\", "D:\\"]);
    }

    #[test]
    fn sorting_by_used_is_ascending() {
        let entries = vec![
            entry("C:\\", 100, 10),
            entry("D:\\", 100, 90),
            entry("E:\\", 100, 50),
        ];
        // Used: C=90, D=10, E=50, so ascending order is D, E, C.
        assert_eq!(
            label(&sort(entries, SortKey::Used, false)),
            ["D:\\", "E:\\", "C:\\"]
        );
    }

    #[test]
    fn sorting_by_total_is_ascending() {
        let entries = vec![
            entry("C:\\", 300, 10),
            entry("D:\\", 100, 10),
            entry("E:\\", 200, 10),
        ];
        assert_eq!(
            label(&sort(entries, SortKey::Total, false)),
            ["D:\\", "E:\\", "C:\\"]
        );
    }

    #[test]
    fn sorting_by_usage_is_ascending() {
        let entries = vec![
            entry("C:\\", 100, 90),
            entry("D:\\", 100, 10),
            entry("E:\\", 100, 50),
        ];
        // Usage: C=10% used, D=90% used, E=50% used, so ascending is C, E, D.
        assert_eq!(
            label(&sort(entries, SortKey::Usage, false)),
            ["C:\\", "E:\\", "D:\\"]
        );
    }

    #[test]
    fn sorting_by_drive_is_alphabetical() {
        let entries = vec![
            entry("D:\\", 100, 10),
            entry("C:\\", 100, 10),
            entry("E:\\", 100, 10),
        ];
        assert_eq!(
            label(&sort(entries, SortKey::Drive, false)),
            ["C:\\", "D:\\", "E:\\"]
        );
    }

    #[test]
    fn descending_flips_the_chosen_field() {
        let entries = vec![
            entry("C:\\", 100, 10),
            entry("D:\\", 100, 90),
            entry("E:\\", 100, 50),
        ];

        assert_eq!(
            label(&sort(entries.clone(), SortKey::Free, true)),
            ["D:\\", "E:\\", "C:\\"]
        );
        assert_eq!(
            label(&sort(entries, SortKey::Used, true)),
            ["C:\\", "E:\\", "D:\\"]
        );
    }

    #[test]
    fn descending_preserves_the_tie_break() {
        let entries = vec![entry("D:\\", 100, 50), entry("C:\\", 100, 50)];
        assert_eq!(
            label(&sort(entries, SortKey::Free, true)),
            ["D:\\", "C:\\"],
            "descending order reverses the whole result, ties included"
        );
    }

    #[test]
    fn ties_break_on_drive_label() {
        let entries = vec![entry("D:\\", 100, 50), entry("C:\\", 100, 50)];
        assert_eq!(
            label(&sort(entries, SortKey::Free, false)),
            ["C:\\", "D:\\"]
        );
    }

    #[test]
    fn sorting_an_empty_list_is_a_no_op() {
        assert!(sort(Vec::new(), SortKey::Free, false).is_empty());
    }

    #[test]
    fn sorting_a_single_entry_keeps_it() {
        assert_eq!(
            label(&sort(vec![entry("C:\\", 10, 5)], SortKey::Usage, false)),
            ["C:\\"]
        );
    }

    #[test]
    fn table_output_contains_headers_and_rows() {
        let entries = vec![entry("C:\\", 149_400_000_000, 3_100_000_000)];
        let text = render_table(&entries, &cli(&["ds", "--human"]));
        for header in crate::table::HEADERS {
            assert!(text.contains(header), "missing header {header}");
        }
        assert!(text.contains("C:\\"));
    }

    #[test]
    fn json_output_parses_and_carries_raw_bytes() {
        let entries = vec![entry("C:\\", 149_400_000_000, 3_100_000_000)];
        let json = render_json(&entries);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let first = &parsed[0];
        assert_eq!(first["drive"], "C:\\");
        assert_eq!(first["total_bytes"], 149_400_000_000u64);
        assert_eq!(first["free_bytes"], 3_100_000_000u64);
        assert_eq!(first["used_bytes"], 146_300_000_000u64);
    }

    #[test]
    fn json_of_an_empty_list_is_an_array() {
        assert_eq!(render_json(&[]), "[]");
    }

    #[test]
    fn write_helpers_terminate_with_a_newline() {
        let entries = vec![entry("C:\\", 100, 10)];

        let mut table_out: Vec<u8> = Vec::new();
        write_table(&mut table_out, &entries, &cli(&["ds", "--human"])).unwrap();
        assert!(table_out.ends_with(b"\n"));

        let mut json_out: Vec<u8> = Vec::new();
        write_json(&mut json_out, &entries).unwrap();
        assert!(json_out.ends_with(b"\n"));
    }

    #[test]
    fn collect_reports_an_error_for_a_missing_drive() {
        let missing = if cfg!(windows) {
            "Z:\\"
        } else {
            "/nope-not-here"
        };
        let c = cli(&["ds", missing]);
        let errors = collect(&c).expect_err("expected an error");
        assert_eq!(errors.len(), 1);
        assert!(!errors[0].message.is_empty(), "expected a cause message");
    }

    #[test]
    fn collect_returns_all_drives_when_none_is_named() {
        let entries = collect(&cli(&["ds"])).expect("collection should succeed");
        assert!(!entries.is_empty());
        assert!(entries.iter().all(|e| e.total_bytes > 0));
    }

    #[test]
    fn collect_sorts_as_requested() {
        let entries = collect(&cli(&["ds", "--sort", "usage"])).expect("collection");
        let percents: Vec<f64> = entries.iter().map(usage_of).collect();
        assert!(percents.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn collect_resolves_a_single_drive() {
        let spec = if cfg!(windows) { "C:" } else { "/" };
        let entries = collect(&cli(&["ds", spec])).expect("collection");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn non_human_mode_emits_grouped_integers() {
        let c = cli(&["ds", "--bytes"]);
        let e = Entry::new(
            &Drive {
                name: "C:\\".to_string(),
                path: std::path::PathBuf::from("C:\\"),
                usage: DiskUsage {
                    available: 1234567,
                    total: 12345678,
                },
            },
            &c,
        );
        assert_eq!(e.free, "1,234,567");
        assert_eq!(e.total, "12,345,678");
    }

    #[test]
    fn usage_for_exposes_the_raw_figures() {
        let d = Drive {
            name: "C:\\".to_string(),
            path: std::path::PathBuf::from("C:\\"),
            usage: DiskUsage {
                available: 10,
                total: 100,
            },
        };
        assert_eq!(usage_for(&d).total, 100);
    }

    #[test]
    fn colour_is_emitted_only_for_a_terminal() {
        let entries = vec![entry("C:\\", 100, 98)];

        let piped = render_table_with(&entries, &cli(&["ds"]), false);
        assert!(!piped.contains('\u{1b}'), "redirected output must be plain");

        let tty = render_table_with(&entries, &cli(&["ds"]), true);
        assert!(tty.contains('\u{1b}'), "terminal output should be coloured");
    }

    #[test]
    fn plain_mode_suppresses_colour_on_a_terminal() {
        let entries = vec![entry("C:\\", 100, 98)];
        let text = render_table_with(&entries, &cli(&["ds", "--plain"]), true);
        assert!(!text.contains('\u{1b}'), "plain mode must stay plain");
    }

    #[test]
    fn json_rendering_is_stable_across_runs() {
        let entries = vec![entry("C:\\", 100, 10), entry("D:\\", 200, 20)];
        assert_eq!(render_json(&entries), render_json(&entries));
    }

    #[test]
    fn the_scan_summary_reports_totals_and_workers() {
        let (outcome, _root) = scanned("summary");
        let text = render_scan(&outcome, &cli(&["ds", "C:"]), false);

        assert!(text.contains("Scanned"), "missing summary: {text}");
        assert!(text.contains("9.3 KB"), "missing total size: {text}");
        assert!(text.contains("4 files"), "missing file count: {text}");
        assert!(text.contains("3 directories"), "missing dir count: {text}");
        assert!(text.contains("4 workers"), "missing worker count: {text}");
    }

    #[test]
    fn the_directory_table_rolls_children_into_parents() {
        let (outcome, root) = scanned("dirs");
        let text = render_scan(&outcome, &cli(&["ds", "C:"]), false);

        assert!(text.contains("Path"), "missing header: {text}");
        assert!(text.contains("videos"), "missing child dir: {text}");
        assert!(
            text.contains(&root.join("videos").to_string_lossy().to_string()),
            "missing full path: {text}"
        );
        // videos holds 3000 + 2000 bytes; notes holds 500.
        assert!(text.contains("4.9 KB"), "missing rolled-up size: {text}");
    }

    #[test]
    fn the_largest_files_table_lists_files_biggest_first() {
        let (outcome, _root) = scanned("largest");
        let text = render_scan(&outcome, &cli(&["ds", "C:", "--largest", "3"]), false);

        let bin = text.find("top.bin").expect("largest file should appear");
        let clip = text.find("clip.mp4").expect("second largest should appear");
        assert!(bin < clip, "largest files must be ordered by size: {text}");
    }

    #[test]
    fn largest_zero_omits_the_file_table() {
        let (outcome, _root) = scanned("nofiles");
        let text = render_scan(&outcome, &cli(&["ds", "C:", "--largest", "0"]), false);
        assert!(!text.contains("top.bin"), "should not list files: {text}");
    }

    #[test]
    fn the_extension_table_appears_only_with_ext() {
        let (outcome, _root) = scanned("ext");
        let flags = &["ds", "C:", "--ext"];

        let with = render_scan(&outcome, &cli(flags), false);
        assert!(with.contains("Extension"), "missing header: {with}");
        assert!(with.contains("mp4"), "missing extension: {with}");

        let without = render_scan(&outcome, &cli(&["ds", "C:"]), false);
        assert!(
            !without.contains("Extension"),
            "extension table must be opt-in: {without}"
        );
    }

    #[test]
    fn a_leaf_directory_says_so_instead_of_showing_an_empty_table() {
        let (_discard, root) = scanned("leaf");
        fs::remove_dir_all(root.join("videos")).unwrap();
        fs::remove_dir_all(root.join("notes")).unwrap();

        let mut scanner = Scanner::new(ScanOptions {
            threads: 2,
            ..ScanOptions::default()
        });
        let leaf = scanner.scan(&root).expect("scan should succeed");
        let text = render_scan(&leaf, &cli(&["ds", "C:"]), false);

        assert!(
            text.contains("No subdirectories found."),
            "expected a note for a leaf directory: {text}"
        );
    }

    #[test]
    fn scan_json_is_valid_and_carries_every_section() {
        let (outcome, _root) = scanned("json");
        let flags = &["ds", "C:", "--ext"];
        let json = render_scan_json(&outcome, &cli(flags), None);

        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["files"], 4);
        assert_eq!(parsed["total_bytes"], 9500);
        assert!(parsed["volume"].is_null(), "no volume expected");
        assert!(
            !parsed["top_directories"]
                .as_array()
                .expect("array")
                .is_empty()
        );
        assert!(
            !parsed["largest_files"]
                .as_array()
                .expect("array")
                .is_empty()
        );
        assert!(!parsed["extensions"].as_array().expect("array").is_empty());
    }

    #[test]
    fn scan_json_omits_extensions_unless_requested() {
        let (outcome, _root) = scanned("json-noext");
        let json = render_scan_json(&outcome, &cli(&["ds", "C:"]), None);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert!(
            parsed["extensions"].as_array().expect("array").is_empty(),
            "extensions must be opt-in"
        );
    }

    #[test]
    fn scan_json_includes_the_volume_row_when_supplied() {
        let (outcome, _root) = scanned("json-vol");
        let volume = entry("D:\\", 1000, 400);
        let json = render_scan_json(&outcome, &cli(&["ds", "C:"]), Some(volume));

        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["volume"]["drive"], "D:\\");
        assert_eq!(parsed["volume"]["total_bytes"], 1000);
    }

    #[test]
    fn scan_json_starts_with_an_object() {
        // Guards the regression where a volume table preceded the JSON and made
        // the output unparseable.
        let (outcome, _root) = scanned("json-pure");
        let json = render_scan_json(&outcome, &cli(&["ds", "C:"]), Some(entry("D:\\", 10, 4)));
        assert!(
            json.starts_with('{'),
            "JSON must start with an object: {json}"
        );
    }

    #[test]
    fn ascii_scan_output_stays_ascii() {
        let (outcome, _root) = scanned("ascii");
        let text = render_scan(&outcome, &cli(&["ds", "C:", "--ascii"]), false);
        assert!(text.is_ascii(), "ASCII mode must stay ASCII-only: {text}");
    }

    #[test]
    fn scan_colour_only_appears_for_a_terminal() {
        let (outcome, _root) = scanned("color");

        let piped = render_scan(&outcome, &cli(&["ds", "C:"]), false);
        assert!(!piped.contains('\u{1b}'), "piped output must be plain");

        let tty = render_scan(&outcome, &cli(&["ds", "C:"]), true);
        assert!(tty.contains('\u{1b}'), "terminal output should be coloured");
    }

    #[test]
    fn plain_mode_suppresses_scan_colour() {
        let (outcome, _root) = scanned("plain");
        let text = render_scan(&outcome, &cli(&["ds", "C:", "--plain"]), true);
        assert!(!text.contains('\u{1b}'), "plain mode must stay plain");
    }

    #[test]
    fn bytes_mode_renders_raw_counts_in_the_scan_report() {
        let (outcome, _root) = scanned("bytes");
        let text = render_scan(&outcome, &cli(&["ds", "C:", "--bytes"]), false);
        assert!(text.contains("9,500"), "expected raw bytes: {text}");
    }

    #[test]
    fn sort_files_reorders_the_directory_table() {
        let (outcome, _root) = scanned("sortfiles");
        let text = render_scan(&outcome, &cli(&["ds", "C:", "--sort", "files"]), false);

        // videos holds 2 files, notes holds 1, so videos must come first.
        let videos = text.find("videos").expect("videos should appear");
        let notes = text.find("notes").expect("notes should appear");
        assert!(videos < notes, "--sort files must order by count: {text}");
    }

    #[test]
    fn sort_size_keeps_the_biggest_first() {
        let (outcome, _root) = scanned("sortsize");
        let text = render_scan(&outcome, &cli(&["ds", "C:", "--sort", "size"]), false);

        let videos = text.find("videos").expect("videos should appear");
        let notes = text.find("notes").expect("notes should appear");
        assert!(videos < notes, "--sort size keeps biggest first: {text}");
    }

    #[test]
    fn an_incomplete_scan_says_so() {
        let (mut outcome, _root) = scanned("errors");
        outcome.errors = 7;

        let text = render_scan(&outcome, &cli(&["ds", "C:"]), false);
        assert!(
            text.contains("could not be read"),
            "expected a warning: {text}"
        );
        assert!(text.contains('7'), "expected the failure count: {text}");
    }

    #[test]
    fn a_cancelled_scan_is_reported_in_json() {
        let (mut outcome, _root) = scanned("cancelled");
        outcome.cancelled = true;

        let parsed: serde_json::Value =
            serde_json::from_str(&render_scan_json(&outcome, &cli(&["ds", "C:"]), None)).unwrap();
        assert_eq!(parsed["cancelled"], true);
    }

    #[test]
    fn the_summary_handles_an_instantaneous_scan() {
        let (_discard, root) = scanned("instant");
        let mut scanner = Scanner::new(ScanOptions {
            threads: 1,
            ..ScanOptions::default()
        });
        let scanned_once = scanner.scan(&root).expect("scan should succeed");

        // Force a zero duration so the rate branch is exercised.
        let outcome = scan::ScanOutcome {
            elapsed: Duration::ZERO,
            ..scanned_once
        };

        let text = render_scan(&outcome, &cli(&["ds", "C:"]), false);
        assert!(text.contains("(-/s)"), "expected a dash rate: {text}");
    }

    fn label(entries: &[Entry]) -> Vec<String> {
        entries.iter().map(|e| e.drive.clone()).collect()
    }
}
