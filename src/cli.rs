//! Command-line interface definition.

use std::time::Duration;

use clap::{Parser, ValueEnum};

/// Field to order drives by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
#[value(rename_all = "lower")]
pub enum SortKey {
    /// Drive label, ascending.
    #[default]
    Drive,
    /// Free space, ascending.
    Free,
    /// Used space, ascending.
    Used,
    /// Total space, ascending.
    Total,
    /// Fraction of the volume in use, ascending.
    Usage,
    /// Recursive size of a scanned directory, ascending.
    Size,
    /// Number of files under a scanned directory, ascending.
    Files,
}

impl SortKey {
    /// Whether this key orders volumes or scanned directories.
    pub fn is_scan_key(self) -> bool {
        matches!(self, SortKey::Size | SortKey::Files)
    }
}

/// Display style for the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
#[value(rename_all = "lower")]
pub enum Style {
    /// Rounded Unicode borders.
    #[default]
    Rounded,
    /// Square Unicode borders.
    Sharp,
    /// Plain ASCII borders.
    Ascii,
}

/// Lightweight disk space utility.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "ds",
    version,
    about = "Show disk space usage and analyse what is using it",
    long_about = None,
    // Repeating a single-value flag takes the last occurrence rather than
    // erroring, which is the least surprising behaviour in a shell.
    args_override_self = true
)]
pub struct Cli {
    /// Drive or directory to inspect, e.g. `C:` or `C:\Users`. Defaults to
    /// every available drive.
    #[arg(value_name = "DRIVE")]
    pub drive: Option<String>,

    /// Use ASCII-only table borders.
    #[arg(long)]
    pub ascii: bool,

    /// Output machine-readable JSON instead of tables.
    #[arg(long)]
    pub json: bool,

    /// Sort by a field: drive|free|used|total|usage for volumes,
    /// size|files for a scanned directory.
    #[arg(long, value_enum, default_value_t = SortKey::Drive)]
    pub sort: SortKey,

    /// Refresh continuously until interrupted.
    #[arg(long)]
    pub watch: bool,

    /// Seconds between watch refreshes.
    #[arg(long, value_name = "SECONDS", default_value_t = 2)]
    pub interval: u64,

    /// Use human-readable units (KB, MB, GB, TB). On by default; pass
    /// `--bytes` for raw counts instead.
    #[arg(long)]
    pub human: bool,

    /// Show raw byte counts instead of human-readable units.
    #[arg(long)]
    pub bytes: bool,

    /// Disable colour on the usage column.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub plain: bool,

    /// Table border style.
    #[arg(long, value_enum)]
    pub style: Option<Style>,

    /// Decimal places for the usage column.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub precision: usize,

    /// Reverse the sort order.
    #[arg(short, long)]
    pub reverse: bool,

    /// Highlight drives above this usage percentage.
    #[arg(long, value_name = "PERCENT")]
    pub warn_above: Option<f64>,

    /// Concurrent scanner workers (1-256).
    #[arg(
        long,
        value_name = "N",
        default_value_t = crate::scan::DEFAULT_THREADS,
        value_parser = parse_threads
    )]
    pub threads: usize,

    /// How many largest files to report.
    #[arg(long, value_name = "N", default_value_t = 10)]
    pub largest: usize,

    /// How many largest directories to report.
    #[arg(long, value_name = "N", default_value_t = 15)]
    pub dirs: usize,

    /// Maximum recursion depth below the target; 0 means unlimited.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub depth: usize,

    /// Show a per-extension size breakdown.
    #[arg(long)]
    pub ext: bool,

    /// Skip the live progress display.
    #[arg(long)]
    pub quiet: bool,

    /// Only report volume capacity; do not walk the filesystem.
    #[arg(long)]
    pub no_scan: bool,
}

/// Parse and validate `--threads`, mapping the bound to a readable error.
fn parse_threads(raw: &str) -> Result<usize, String> {
    let value: usize = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a whole number"))?;

    crate::scan::ScanOptions::validate_threads(value)
}

impl Cli {
    /// Build the scanner settings implied by the flags.
    pub fn scan_options(&self) -> crate::scan::ScanOptions {
        crate::scan::ScanOptions {
            threads: self.threads,
            max_depth: self.depth,
            largest: self.largest,
            collect_extensions: true,
            ..crate::scan::ScanOptions::default()
        }
    }

    /// Whether a filesystem walk should run for this invocation.
    ///
    /// A drive argument is scanned unless `--no-scan` is given; no argument
    /// means the cheap volume listing only.
    pub fn should_scan(&self) -> bool {
        !self.no_scan && self.drive.is_some()
    }

    /// Repaint interval for the live progress display.
    pub fn progress_interval(&self) -> Duration {
        Duration::from_millis(crate::progress::MIN_INTERVAL.as_millis() as u64)
    }

    /// Whether sizes render as `KB`/`MB`/`GB` or as raw byte counts.
    ///
    /// Human units are the default, so `--human` is only a restatement and
    /// `--bytes` is what actually changes the rendering.
    pub fn use_human_units(&self) -> bool {
        !self.bytes
    }

    /// Whether the usage column should be colourised.
    ///
    /// Colour is skipped for JSON, for plain mode, and for ASCII borders, so
    /// the frame stays byte-clean where escape codes would be noise.
    pub fn use_color(&self) -> bool {
        !self.plain && !self.json && self.table_style() != Style::Ascii
    }

    /// Border style, letting `--ascii` override `--style`.
    pub fn table_style(&self) -> Style {
        if self.ascii {
            return Style::Ascii;
        }
        self.style.unwrap_or_default()
    }

    /// Effective sort order: the chosen field plus the direction flag.
    pub fn sort_order(&self) -> (SortKey, bool) {
        (self.sort, self.reverse)
    }

    /// Refresh interval, clamped to a sane minimum so `--watch` cannot busy
    /// loop.
    pub fn refresh_interval(&self) -> Duration {
        Duration::from_secs(self.interval.max(1))
    }
}

/// Parse a raw CLI argument list.
///
/// Exposed so parsing can be tested without spawning a process.
pub fn parse_from<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    Cli::try_parse_from(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{DEFAULT_THREADS, ScanOptions};

    fn parse(args: &[&str]) -> Cli {
        parse_from(args).expect("expected the arguments to parse")
    }

    #[test]
    fn defaults_to_all_drives_with_rounded_unicode_borders() {
        let cli = parse(&["ds"]);
        assert_eq!(cli.drive, None);
        assert!(!cli.ascii);
        assert!(!cli.json);
        assert!(!cli.human);
        assert!(!cli.watch);
        assert_eq!(cli.sort, SortKey::Drive);
        assert_eq!(cli.table_style(), Style::Rounded);
        assert_eq!(cli.precision, 1);
        assert_eq!(cli.interval, 2);
    }

    #[test]
    fn parses_a_specific_drive() {
        assert_eq!(parse(&["ds", "C:"]).drive.as_deref(), Some("C:"));
        assert_eq!(parse(&["ds", "d:"]).drive.as_deref(), Some("d:"));
    }

    #[test]
    fn ascii_flag_selects_the_ascii_style() {
        assert_eq!(parse(&["ds", "--ascii"]).table_style(), Style::Ascii);
    }

    #[test]
    fn ascii_flag_wins_over_an_explicit_style() {
        let cli = parse(&["ds", "--style", "sharp", "--ascii"]);
        assert_eq!(cli.table_style(), Style::Ascii);
    }

    #[test]
    fn parses_every_sort_key() {
        assert_eq!(parse(&["ds", "--sort", "free"]).sort, SortKey::Free);
        assert_eq!(parse(&["ds", "--sort", "used"]).sort, SortKey::Used);
        assert_eq!(parse(&["ds", "--sort", "total"]).sort, SortKey::Total);
        assert_eq!(parse(&["ds", "--sort", "usage"]).sort, SortKey::Usage);
        assert_eq!(parse(&["ds", "--sort", "drive"]).sort, SortKey::Drive);
    }

    #[test]
    fn rejects_an_unknown_sort_key() {
        assert!(parse_from(["ds", "--sort", "nonsense"]).is_err());
        assert!(parse_from(["ds", "--sort", "filesize"]).is_err());
    }

    #[test]
    fn parses_every_scan_sort_key() {
        assert_eq!(parse(&["ds", "--sort", "size"]).sort, SortKey::Size);
        assert_eq!(parse(&["ds", "--sort", "files"]).sort, SortKey::Files);
        assert!(SortKey::Size.is_scan_key());
        assert!(SortKey::Files.is_scan_key());
        assert!(!SortKey::Drive.is_scan_key());
    }

    #[test]
    fn scanner_settings_follow_the_flags() {
        let cli = parse(&[
            "ds",
            "C:",
            "--threads",
            "8",
            "--depth",
            "3",
            "--largest",
            "50",
        ]);
        let options = cli.scan_options();
        assert_eq!(options.threads, 8);
        assert_eq!(options.max_depth, 3);
        assert_eq!(options.largest, 50);
    }

    #[test]
    fn threads_default_to_sixteen() {
        assert_eq!(parse(&["ds", "C:"]).threads, 16);
        assert_eq!(ScanOptions::default().threads, DEFAULT_THREADS);
        assert_eq!(DEFAULT_THREADS, 16);
    }

    #[test]
    fn unreasonable_thread_counts_are_rejected_at_parse_time() {
        assert!(parse_from(["ds", "C:", "--threads", "0"]).is_err());
        assert!(parse_from(["ds", "C:", "--threads", "9999"]).is_err());
        assert!(parse_from(["ds", "C:", "--threads", "many"]).is_err());
        assert!(parse_from(["ds", "C:", "--threads", "-4"]).is_err());
    }

    #[test]
    fn the_thread_bound_is_enforced_but_reasonable_values_pass() {
        assert!(parse_from(["ds", "C:", "--threads", "1"]).is_ok());
        assert!(parse_from(["ds", "C:", "--threads", "64"]).is_ok());
        assert!(parse_from(["ds", "C:", "--threads", "256"]).is_ok());
        assert!(parse_from(["ds", "C:", "--threads", "257"]).is_err());
    }

    #[test]
    fn scanning_happens_only_for_an_explicit_target() {
        assert!(
            !parse(&["ds"]).should_scan(),
            "no target means volumes only"
        );
        assert!(parse(&["ds", "C:"]).should_scan());
        assert!(
            !parse(&["ds", "C:", "--no-scan"]).should_scan(),
            "--no-scan opts out"
        );
    }

    #[test]
    fn report_sizes_and_depth_are_configurable() {
        let cli = parse(&[
            "ds",
            "C:",
            "--largest",
            "20",
            "--dirs",
            "7",
            "--depth",
            "0",
            "--ext",
        ]);
        assert_eq!(cli.largest, 20);
        assert_eq!(cli.dirs, 7);
        assert_eq!(cli.depth, 0);
        assert!(cli.ext);
        assert!(!cli.quiet, "progress is on by default");
    }

    #[test]
    fn quiet_and_ext_are_boolean_flags() {
        assert!(parse(&["ds", "C:", "--quiet"]).quiet);
        assert!(!parse(&["ds", "C:"]).quiet);
        assert!(parse(&["ds", "C:", "--ext"]).ext);
        assert!(!parse(&["ds", "C:"]).ext);
    }

    #[test]
    fn reverse_flag_flips_direction_without_changing_the_field() {
        assert_eq!(
            parse(&["ds", "--reverse"]).sort_order(),
            (SortKey::Drive, true)
        );
        assert_eq!(
            parse(&["ds", "--sort", "free", "-r"]).sort_order(),
            (SortKey::Free, true)
        );
        assert_eq!(
            parse(&["ds", "--sort", "used"]).sort_order(),
            (SortKey::Used, false)
        );
        assert_eq!(parse(&["ds"]).sort_order(), (SortKey::Drive, false));
    }

    #[test]
    fn watch_defaults_to_two_seconds() {
        let cli = parse(&["ds", "--watch"]);
        assert!(cli.watch);
        assert_eq!(cli.refresh_interval(), Duration::from_secs(2));
    }

    #[test]
    fn watch_interval_is_honoured() {
        let cli = parse(&["ds", "--watch", "--interval", "10"]);
        assert_eq!(cli.refresh_interval(), Duration::from_secs(10));
    }

    #[test]
    fn watch_interval_is_clamped_to_one_second() {
        let cli = parse(&["ds", "--watch", "--interval", "0"]);
        assert_eq!(cli.refresh_interval(), Duration::from_secs(1));
    }

    #[test]
    fn parses_json_and_human_flags_together() {
        let cli = parse(&["ds", "--json", "--human"]);
        assert!(cli.json);
        assert!(cli.human);
    }

    #[test]
    fn parses_style_and_precision() {
        let cli = parse(&["ds", "--style", "ascii", "--precision", "3"]);
        assert_eq!(cli.table_style(), Style::Ascii);
        assert_eq!(cli.precision, 3);
    }

    #[test]
    fn parses_warn_threshold() {
        let cli = parse(&["ds", "--warn-above", "90"]);
        assert_eq!(cli.warn_above, Some(90.0));
        assert_eq!(parse(&["ds"]).warn_above, None);
    }

    #[test]
    fn supports_a_long_flag_and_its_short_alias() {
        assert!(parse(&["ds", "--reverse"]).reverse);
        assert!(parse(&["ds", "-r"]).reverse);
        assert!(!parse(&["ds"]).reverse);
    }

    #[test]
    fn rejects_a_non_numeric_interval() {
        assert!(parse_from(["ds", "--interval", "soon"]).is_err());
    }

    #[test]
    fn rejects_unknown_flags() {
        assert!(parse_from(["ds", "--nope"]).is_err());
    }

    #[test]
    fn rejects_extra_positional_arguments() {
        assert!(parse_from(["ds", "C:", "D:"]).is_err());
    }

    #[test]
    fn version_and_help_short_circuit() {
        assert!(parse_from(["ds", "--version"]).is_err());
        assert!(parse_from(["ds", "--help"]).is_err());
    }

    #[test]
    fn drive_argument_with_a_path_is_accepted() {
        let cli = parse(&["ds", "C:\\Users"]);
        assert_eq!(cli.drive.as_deref(), Some("C:\\Users"));
    }

    #[test]
    fn repeated_single_value_flags_take_the_last_occurrence() {
        let cli = parse(&["ds", "--sort", "free", "--sort", "usage"]);
        assert_eq!(cli.sort, SortKey::Usage);

        let cli = parse(&["ds", "--warn-above", "10", "--warn-above", "20"]);
        assert_eq!(cli.warn_above, Some(20.0));
    }

    #[test]
    fn rejects_negative_numeric_options() {
        assert!(parse_from(["ds", "--interval", "-1"]).is_err());
        assert!(parse_from(["ds", "--precision", "-1"]).is_err());
        assert!(parse_from(["ds", "--warn-above", "-5"]).is_err());
    }

    #[test]
    fn zero_precision_is_allowed() {
        assert_eq!(parse(&["ds", "--precision", "0"]).precision, 0);
    }

    #[test]
    fn boolean_flags_reject_an_attached_value() {
        assert!(parse_from(["ds", "--json=true"]).is_err());
        assert!(parse_from(["ds", "--ascii=1"]).is_err());
    }
}
