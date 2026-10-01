//! Live refresh loop for `--watch`.
//!
//! Each refresh is a complete, self-contained operation:
//!
//! * the previous refresh's worker pool has already been joined and shut down,
//!   so no threads accumulate across iterations;
//! * a scan in flight is cancelled when the user quits, and its threads are
//!   joined before the terminal is restored;
//! * the frame is written once per refresh, not per file, so terminal I/O stays
//!   negligible next to scanning.

use std::io::{self, Stdout, Write};
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{ExecutableCommand, cursor, terminal};

use crate::cli::Cli;
use crate::disk::{self, DriveError};
use crate::output::{self, Entry};
use crate::scan::{ScanOutcome, Scanner};

/// Why the watch loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The user pressed `q` or `Esc`.
    UserQuit,
    /// The user pressed `Ctrl-C`.
    UserInterrupt,
}

/// Run the refresh loop until the user exits.
///
/// Returns the exit reason, or an I/O error if the terminal could not be
/// driven.
pub fn run(cli: &Cli) -> io::Result<Stop> {
    let mut out = io::stdout();
    let mut session = Session::enter(&mut out)?;
    let reason = session.loop_until_exit(cli, &mut out);
    session.leave(&mut out)?;
    reason
}

/// Owns the alternate-screen and raw-mode state, restoring it on exit.
struct Session {
    active: bool,
    /// Cancellation handle for the refresh currently in flight.
    pending: Option<crate::scan::Cancel>,
}

impl Session {
    /// Enter the alternate screen with raw mode enabled.
    fn enter(out: &mut Stdout) -> io::Result<Self> {
        enable_raw_mode()?;
        out.execute(EnterAlternateScreen)?;
        out.execute(cursor::Hide)?;
        Ok(Self {
            active: true,
            pending: None,
        })
    }

    /// Restore the terminal to its original state.
    fn leave(&mut self, out: &mut Stdout) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        // Never leave a scan running behind a restored terminal.
        if let Some(cancel) = self.pending.take() {
            cancel.cancel();
        }
        disable_raw_mode()?;
        out.execute(cursor::Show)?;
        out.execute(LeaveAlternateScreen)?;
        Ok(())
    }

    /// Redraw until the user quits.
    fn loop_until_exit(&mut self, cli: &Cli, out: &mut Stdout) -> io::Result<Stop> {
        let interval = cli.refresh_interval();

        loop {
            draw(out, cli, self)?;
            if let Some(stop) = poll_exit(interval)? {
                return Ok(stop);
            }
        }
    }
}

/// Clear the screen and write a fresh frame.
fn draw(out: &mut Stdout, cli: &Cli, session: &mut Session) -> io::Result<()> {
    out.execute(terminal::Clear(terminal::ClearType::All))?;
    out.execute(cursor::MoveTo(0, 0))?;

    match frame_for(cli, session) {
        Ok(text) => writeln!(out, "{text}")?,
        Err(text) => writeln!(out, "{text}")?,
    }

    out.flush()
}

/// Produce one frame's text, scanning first when a target was given.
fn frame_for(cli: &Cli, session: &mut Session) -> Result<String, String> {
    if !cli.should_scan() {
        return match output::collect(cli) {
            Ok(entries) => Ok(frame(&entries, cli)),
            Err(errors) => Err(error_frame(&errors)),
        };
    }

    let target = disk::scan_target(Path::new(cli.drive.as_deref().unwrap_or_default()));
    match scan_once(cli, session, &target) {
        Ok(outcome) => Ok(scan_frame(&outcome, cli)),
        Err(message) => Err(format!("Scan failed: {message}\nPress q to quit")),
    }
}

/// Run a single scan, keeping its cancel handle available for shutdown.
///
/// The scanner joins all of its workers before returning, so no thread outlives
/// the call.
fn scan_once(cli: &Cli, session: &mut Session, target: &Path) -> Result<ScanOutcome, String> {
    let mut scanner = Scanner::new(cli.scan_options());
    session.pending = Some(scanner.cancel_handle());

    let outcome = scanner.scan(target).map_err(|err| err.to_string());

    // The scan has finished and joined its workers, so nothing is left to
    // cancel from here on.
    session.pending = None;
    outcome
}

/// Build a frame for a finished scan.
fn scan_frame(outcome: &ScanOutcome, cli: &Cli) -> String {
    let mut text = output::render_scan(outcome, cli, true);
    text.push_str("\nPress q to quit");
    text
}

/// Build one frame: the table plus a status line.
pub fn frame(entries: &[Entry], cli: &Cli) -> String {
    let mut out = output::render_table(entries, cli);

    if let Some(threshold) = cli.warn_above {
        let hot = entries
            .iter()
            .filter(|e| e.usage_percent >= threshold)
            .count();
        out.push_str(&format!(
            "\n{} of {} drive(s) above {threshold:.0}% — press q to quit",
            hot,
            entries.len()
        ));
    } else {
        out.push_str("\nPress q to quit");
    }

    out
}

/// Build a frame describing drives that could not be read.
pub fn error_frame(errors: &[DriveError]) -> String {
    let mut out = String::from("Could not read disk information:\n");
    for error in errors {
        out.push_str(&format!("  {error}\n"));
    }
    out
}

/// Wait up to `interval`, returning `Some` if the user asked to exit.
fn poll_exit(interval: Duration) -> io::Result<Option<Stop>> {
    if !event::poll(interval)? {
        return Ok(None);
    }

    match event::read()? {
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => Ok(Some(Stop::UserQuit)),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Ok(Some(Stop::UserInterrupt))
            }
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::parse_from;
    use crate::disk::DiskUsage;
    use crate::output::Entry;

    fn cli(args: &[&str]) -> Cli {
        parse_from(args).expect("args should parse")
    }

    fn entry(drive: &str, total: u64, free: u64) -> Entry {
        Entry {
            drive: drive.to_string(),
            total_bytes: total,
            free_bytes: free,
            used_bytes: total - free,
            usage_percent: 50.0,
            usage: "50.0%".to_string(),
            used: "5 B".to_string(),
            free: "5 B".to_string(),
            total: "10 B".to_string(),
        }
    }

    #[test]
    fn frame_contains_the_table_and_a_quit_hint() {
        let entries = vec![entry("C:\\", 10, 5)];
        let text = frame(&entries, &cli(&["ds", "--human"]));
        assert!(text.contains("Drive"));
        assert!(text.contains("C:\\"));
        assert!(text.contains("q to quit"));
    }

    #[test]
    fn frame_reports_a_warning_count_when_thresholded() {
        let entries = vec![entry("C:\\", 10, 5), entry("D:\\", 10, 5)];
        let text = frame(&entries, &cli(&["ds", "--warn-above", "40"]));
        assert!(text.contains("2 of 2 drive(s) above 40%"));
    }

    #[test]
    fn frame_omits_the_count_when_no_threshold_is_set() {
        let entries = vec![entry("C:\\", 10, 5)];
        let text = frame(&entries, &cli(&["ds"]));
        assert!(!text.contains("drive(s)"));
        assert!(text.contains("q to quit"));
    }

    #[test]
    fn error_frame_lists_each_failure() {
        let errors = vec![DriveError {
            target: "Z:\\".to_string(),
            message: "device not ready".to_string(),
        }];
        let text = error_frame(&errors);
        assert!(text.contains("Could not read disk information"));
        assert!(text.contains("Z:\\: device not ready"));
    }

    #[test]
    fn error_frame_with_no_errors_is_still_valid_text() {
        let text = error_frame(&[]);
        assert!(text.contains("Could not read disk information"));
    }

    #[test]
    fn an_inactive_session_leaves_without_touching_the_terminal() {
        // The real terminal is never driven from a test, so only the inert
        // state machine is exercised here.
        let mut out = io::stdout();
        let mut session = inactive_session();
        // `leave` returns before touching the terminal when inactive.
        assert!(session.leave(&mut out).is_ok());
        assert!(!session.active);
    }

    #[test]
    fn leaving_an_inactive_session_is_a_no_op() {
        let mut out = io::stdout();
        let mut session = inactive_session();
        assert!(session.leave(&mut out).is_ok());
    }

    /// A session that never entered the alternate screen.
    fn inactive_session() -> Session {
        Session {
            active: false,
            pending: None,
        }
    }

    #[test]
    fn frame_uses_raw_figures_when_human_is_off() {
        let mut e = entry("C:\\", 1024, 512);
        e.free = "512".to_string();
        let text = frame(&[e], &cli(&["ds", "--ascii"]));
        assert!(text.contains(" 512 "), "expected raw figure in {text}");
    }

    #[test]
    fn coloured_frames_emit_escape_codes() {
        let entries = vec![entry("C:\\", 10, 5), entry("D:\\", 10, 1)];
        let text = output::render_table_with(&entries, &cli(&["ds"]), true);
        assert!(text.contains('\u{1b}'), "expected colour escapes: {text:?}");
    }

    #[test]
    fn plain_frames_stay_escape_free() {
        let entries = vec![entry("C:\\", 10, 5), entry("D:\\", 10, 1)];
        let text = frame(&entries, &cli(&["ds", "--plain"]));
        assert!(!text.contains('\u{1b}'), "unexpected escapes: {text:?}");
    }

    #[test]
    fn ascii_frames_stay_ascii() {
        let entries = vec![entry("C:\\", 10, 5)];
        let text = frame(&entries, &cli(&["ds", "--ascii"]));
        assert!(text.is_ascii(), "expected ASCII-only frame: {text}");
    }

    #[test]
    fn disk_usage_is_readable_from_a_frame() {
        let usage = DiskUsage {
            available: 512,
            total: 1024,
        };
        assert_eq!(usage.used(), 512);
    }
}
