//! Periodic progress rendering.
//!
//! The scanner never performs terminal I/O. It publishes counters to
//! [`Progress`](crate::scan::Progress) and this module's renderer thread polls
//! them on a timer, so a scan is never throttled by the terminal.
//!
//! Rendering is suppressed entirely when stdout is not a terminal, so
//! redirected output stays clean and costs nothing.

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::format;
use crate::scan::{Progress, ScanOutcome};

/// Floor on how often the screen is repainted.
pub const MIN_INTERVAL: Duration = Duration::from_millis(200);

/// A running progress display.
pub struct Reporter {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    interval: Duration,
    active: bool,
}

impl Reporter {
    /// Start a reporter for the given counters.
    ///
    /// Returns an inactive reporter when progress should not be shown, in which
    /// case nothing is printed and no thread is spawned.
    pub fn start(progress: Arc<Progress>, label: &str, interval: Duration, threads: usize) -> Self {
        let active = std::io::stdout().is_terminal();
        let interval = interval.max(MIN_INTERVAL);

        if !active {
            return Self {
                stop: Arc::new(AtomicBool::new(false)),
                handle: None,
                interval,
                active: false,
            };
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let label = label.to_string();

        let handle = thread::spawn(move || {
            let mut out = std::io::stdout();
            let started = Instant::now();

            while !thread_stop.load(Ordering::Relaxed) {
                draw(&mut out, &progress, &label, threads, started.elapsed());

                // Sleep in slices so quitting stays responsive.
                let mut waited = Duration::ZERO;
                while waited < interval {
                    if thread_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let slice = MIN_INTERVAL.min(interval - waited);
                    thread::sleep(slice);
                    waited += slice;
                }
            }
        });

        Self {
            stop,
            handle: Some(handle),
            interval,
            active: true,
        }
    }

    /// A reporter that draws nothing and spawns no thread.
    pub fn disabled() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
            interval: MIN_INTERVAL,
            active: false,
        }
    }

    /// Whether this reporter will actually draw.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Configured repaint interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Stop the reporter and clear the progress line.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if self.active {
            clear_line();
        }
        self.active = false;
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Draw one frame in place.
fn draw(out: &mut impl Write, progress: &Progress, label: &str, threads: usize, elapsed: Duration) {
    let counters = progress.snapshot();
    let _ = write!(
        out,
        "{} Files: {}  Dirs: {}  Size: {}  Workers: {}  Elapsed: {}",
        label,
        format::group_count(counters.files),
        format::group_count(counters.directories),
        format::format_bytes(counters.bytes, true),
        threads,
        format::format_duration(elapsed),
    );

    if counters.errors > 0 {
        let _ = write!(out, "  Errors: {}", format::group_count(counters.errors));
    }

    let _ = writeln!(out);
    let _ = out.flush();
}

/// Erase the progress line so it does not scroll away with the report.
pub fn clear_line() {
    let mut out = std::io::stdout();
    let _ = write!(out, "\r\x1b[2K");
    let _ = out.flush();
}

/// A one-line summary printed after a scan finishes.
pub fn summary(outcome: &ScanOutcome) -> String {
    let mut line = format!(
        "Scanned {} in {} with {} worker{}",
        outcome.root.display(),
        format::format_duration(outcome.elapsed),
        outcome.threads,
        if outcome.threads == 1 { "" } else { "s" }
    );

    if outcome.cancelled {
        line.push_str(" (cancelled)");
    }

    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inactive_reporter_spawns_nothing() {
        // Under `cargo test` stdout is captured, so the reporter is inactive.
        let progress = Arc::new(Progress::default());
        let mut reporter = Reporter::start(progress, "C:\\", Duration::from_millis(200), 32);

        assert!(!reporter.is_active());
        assert_eq!(reporter.interval(), Duration::from_millis(200));
        reporter.stop();
    }

    #[test]
    fn stopping_is_idempotent() {
        let progress = Arc::new(Progress::default());
        let mut reporter = Reporter::start(progress, "C:\\", Duration::from_millis(200), 4);
        reporter.stop();
        reporter.stop();
    }

    #[test]
    fn the_interval_is_never_below_the_floor() {
        let progress = Arc::new(Progress::default());
        let reporter = Reporter::start(progress, "C:\\", Duration::from_millis(1), 4);
        assert!(reporter.interval() >= MIN_INTERVAL);
    }

    #[test]
    fn a_draw_writes_counters_and_does_not_panic() {
        let progress = Progress::default();
        progress.add(184_231, 77_000_000_000, 21_482, 2);

        let mut buffer: Vec<u8> = Vec::new();
        draw(
            &mut buffer,
            &progress,
            "Scanning C:\\",
            32,
            Duration::from_millis(4200),
        );

        let text = String::from_utf8(buffer).expect("draw should emit UTF-8");
        assert!(text.contains("Scanning C:\\"), "missing label: {text}");
        assert!(text.contains("184,231"), "missing file count: {text}");
        assert!(text.contains("21,482"), "missing dir count: {text}");
        assert!(text.contains("Workers: 32"), "missing worker count: {text}");
        assert!(text.contains("Errors: 2"), "missing error count: {text}");
        assert!(text.contains("4.2s"), "missing elapsed: {text}");
        assert!(text.ends_with('\n'), "frame should terminate the line");
    }

    #[test]
    fn a_draw_omits_errors_when_there_are_none() {
        let progress = Progress::default();
        progress.add(1, 1, 1, 0);

        let mut buffer: Vec<u8> = Vec::new();
        draw(&mut buffer, &progress, "x", 1, Duration::ZERO);

        let text = String::from_utf8(buffer).unwrap();
        assert!(!text.contains("Errors"), "no errors to report: {text}");
    }

    #[test]
    fn summary_mentions_the_root_workers_and_time() {
        let root = std::path::PathBuf::from("C:\\");
        let outcome = ScanOutcome {
            root: root.clone(),
            bytes: 1024,
            files: 2,
            directories: 1,
            errors: 0,
            elapsed: Duration::from_millis(1500),
            threads: 32,
            cancelled: false,
            aggregator: crate::aggregate::Aggregator::new(root, 0, 0),
        };

        let text = summary(&outcome);
        assert!(text.contains("C:\\"), "missing root: {text}");
        assert!(text.contains("32 workers"), "missing workers: {text}");
        assert!(text.contains("1.5s"), "missing duration: {text}");
        assert!(!text.contains("cancelled"), "should not mention cancel");
    }

    #[test]
    fn summary_notes_cancellation() {
        let root = std::path::PathBuf::from("C:\\");
        let outcome = ScanOutcome {
            root: root.clone(),
            bytes: 0,
            files: 0,
            directories: 0,
            errors: 0,
            elapsed: Duration::from_millis(10),
            threads: 1,
            cancelled: true,
            aggregator: crate::aggregate::Aggregator::new(root, 0, 0),
        };

        let text = summary(&outcome);
        assert!(
            text.contains("cancelled"),
            "should note cancellation: {text}"
        );
        assert!(text.contains("1 worker"), "singular worker: {text}");
    }
}
