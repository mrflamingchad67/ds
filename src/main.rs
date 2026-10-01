//! DS — a lightweight native disk space utility.
//!
//! Modules are kept small and single-purpose:
//!
//! * [`cli`] parses arguments,
//! * [`disk`] enumerates volumes and reports their capacity,
//! * [`queue`] provides the bounded work queue,
//! * [`scan`] walks the filesystem with a fixed worker pool,
//! * [`aggregate`] folds per-directory, per-extension, and largest-file totals,
//! * [`progress`] renders live counters without touching the scanner,
//! * [`format`] renders byte counts,
//! * [`table`] builds the tabled views,
//! * [`output`] turns drives or scan results into rows or JSON, and
//! * [`watch`] drives the live refresh loop.
//!
//! Volume capacity comes from the platform API; everything about *what is using
//! the space* comes from DS's own metadata-only filesystem scan.

pub mod aggregate;
pub mod cli;
pub mod disk;
pub mod format;
pub mod output;
pub mod progress;
pub mod queue;
pub mod scan;
pub mod table;
pub mod watch;

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

use crate::cli::Cli;
use crate::scan::{ScanOptions, ScanOutcome, Scanner};

/// Entry point. Parses arguments, then lists volumes or scans a target.
fn main() -> ExitCode {
    let cli = match cli::parse_from(std::env::args_os()) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return match err.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    ExitCode::SUCCESS
                }
                _ => ExitCode::from(2),
            };
        }
    };

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if is_broken_pipe(err.as_ref()) {
                // The reader closed the pipe, e.g. `ds | head`. Not an error.
                return ExitCode::SUCCESS;
            }
            let _ = writeln!(io::stderr(), "ds: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Collect and render once, or loop when `--watch` is set.
fn run(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.watch {
        // The alternate screen owns rendering; JSON is a single snapshot.
        if cli.json {
            return Err("--json cannot be combined with --watch".into());
        }
        watch::run(cli)?;
        return Ok(());
    }

    if cli.should_scan() {
        return run_scan(cli);
    }

    run_volumes(cli)
}

/// Print the volume table, as DS has always done.
fn run_volumes(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = io::stdout();

    match output::collect(cli) {
        Ok(entries) if cli.json => output::write_json(&mut out, &entries)?,
        Ok(entries) if cli.warn_above.is_some() => {
            writeln!(out, "{}", output::render_table(&entries, cli))?;
            let hot: Vec<&str> = entries
                .iter()
                .filter(|e| Some(e.usage_percent) >= cli.warn_above)
                .map(|e| e.drive.as_str())
                .collect();
            writeln!(out, "Above threshold: {}", join_or_none(&hot))?;
        }
        Ok(entries) => output::write_table(&mut out, &entries, cli)?,
        Err(errors) => {
            for error in &errors {
                writeln!(io::stderr(), "ds: {error}")?;
            }
            return Err("no readable drives".into());
        }
    }

    Ok(())
}

/// Walk the target filesystem and print the analysis.
///
/// The volume row for the same target is printed first, so `ds C:` still opens
/// with the capacity figures users expect.
fn run_scan(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = io::stdout();

    let target = disk::scan_target(std::path::Path::new(
        cli.drive.as_deref().unwrap_or_default(),
    ));

    // Volume capacity for the target, when it can be measured. In JSON mode this
    // is folded into the document so the output stays parseable.
    let volume = disk::query(&target).ok().map(|usage| {
        output::volume_entry(
            &disk::Drive {
                name: disk::volume_root(&target).to_string_lossy().into_owned(),
                path: target.clone(),
                usage,
            },
            cli,
        )
    });

    // Only print the volume table when something else will follow it.
    if let (false, Some(entry)) = (cli.json, volume.as_ref()) {
        writeln!(
            out,
            "{}",
            output::render_table(std::slice::from_ref(entry), cli)
        )?;
        out.write_all(b"\n")?;
    }

    let options = cli.scan_options();
    let outcome = scan_with_progress(cli, &options, &target)?;

    if cli.json {
        writeln!(out, "{}", output::render_scan_json(&outcome, cli, volume))?;
        return Ok(());
    }

    write!(
        out,
        "{}",
        output::render_scan(&outcome, cli, io::stdout().is_terminal())
    )?;
    Ok(())
}

/// Run a scan, showing live progress unless it is turned off.
fn scan_with_progress(
    cli: &Cli,
    options: &ScanOptions,
    target: &std::path::Path,
) -> Result<ScanOutcome, Box<dyn std::error::Error>> {
    let mut scanner = Scanner::new(options.clone());

    let mut reporter = if cli.quiet {
        progress::Reporter::disabled()
    } else {
        progress::Reporter::start(
            scanner.progress(),
            &format!("Scanning {}", target.display()),
            cli.progress_interval(),
            options.threads,
        )
    };

    let outcome = scanner.scan(target)?;
    reporter.stop();
    Ok(outcome)
}

/// Join warning labels, or say so when there are none.
fn join_or_none(items: &[&str]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}

/// Whether an error is a closed pipe, which is a normal exit for a CLI.
///
/// Boxed errors are downcast back to the originating `io::Error`.
fn is_broken_pipe(err: &(dyn std::error::Error + 'static)) -> bool {
    err.downcast_ref::<io::Error>()
        .is_some_and(|io_err| io_err.kind() == io::ErrorKind::BrokenPipe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::parse_from;

    fn cli(args: &[&str]) -> Cli {
        parse_from(args).expect("args should parse")
    }

    /// A non-I/O error, used to confirm the downcast stays narrow.
    #[derive(Debug)]
    struct CustomError;

    impl std::fmt::Display for CustomError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "custom")
        }
    }

    impl std::error::Error for CustomError {}

    #[test]
    fn a_single_run_succeeds_and_prints_a_table() {
        assert!(run(&cli(&["ds"])).is_ok());
    }

    #[test]
    fn json_output_succeeds() {
        assert!(run(&cli(&["ds", "--json"])).is_ok());
    }

    #[test]
    fn watch_with_json_is_rejected() {
        let err = run(&cli(&["ds", "--watch", "--json"])).expect_err("should be rejected");
        assert!(err.to_string().contains("--json"));
    }

    #[test]
    fn warn_threshold_path_succeeds() {
        assert!(run(&cli(&["ds", "--warn-above", "1"])).is_ok());
    }

    #[test]
    fn a_missing_drive_reports_failure() {
        let missing = if cfg!(windows) {
            "Z:\\"
        } else {
            "/nope-not-here"
        };
        assert!(run(&cli(&["ds", missing])).is_err());
    }

    #[test]
    fn sorts_without_panicking() {
        assert!(run(&cli(&["ds", "--sort", "free"])).is_ok());
        assert!(run(&cli(&["ds", "--sort", "used", "-r"])).is_ok());
    }

    #[test]
    fn join_or_none_handles_both_cases() {
        assert_eq!(join_or_none(&[]), "none");
        assert_eq!(join_or_none(&["C:\\"]), "C:\\");
        assert_eq!(join_or_none(&["C:\\", "D:\\"]), "C:\\, D:\\");
    }

    #[test]
    fn broken_pipe_is_detected() {
        let broken = io::Error::new(io::ErrorKind::BrokenPipe, "closed");
        assert!(is_broken_pipe(&broken));

        let other = io::Error::other("other");
        assert!(!is_broken_pipe(&other));

        // A non-`io::Error` must not be mistaken for a broken pipe.
        let custom = CustomError;
        assert!(!is_broken_pipe(&custom));
    }
}
