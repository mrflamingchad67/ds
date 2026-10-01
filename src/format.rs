//! Human-readable byte formatting.

/// Binary size suffixes, ordered from smallest to largest.
pub const UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];

/// Number of bytes in one binary unit.
const UNIT_BYTES: f64 = 1024.0;

/// Format a byte count with a binary suffix and one decimal place.
///
/// `format_bytes(153_600_000)` yields `"146.4 MB"`. Non-human modes return a
/// comma-grouped integer, which is easier to parse in scripts.
pub fn format_bytes(bytes: u64, human: bool) -> String {
    if !human {
        return group_digits(bytes);
    }
    human_bytes(bytes)
}

/// Format a byte count using binary units (`KB`, `MB`, `GB`, ...).
///
/// Values below one kilobyte are reported in whole bytes. Rounding is applied
/// to the scaled value, so `1024` reads as `1.0 KB` rather than `1023 B`.
pub fn human_bytes(bytes: u64) -> String {
    let bytes = bytes as f64;
    if bytes < UNIT_BYTES {
        return format!("{} B", bytes as u64);
    }

    let mut value = bytes / UNIT_BYTES;
    let mut unit = 0;

    while value >= UNIT_BYTES && unit < UNITS.len() - 2 {
        value /= UNIT_BYTES;
        unit += 1;
    }

    format!("{value:.1} {}", UNITS[unit + 1])
}

/// Format a ratio as a fixed-precision percentage with a trailing sign.
pub fn format_percent(value: f64, decimals: usize) -> String {
    format!("{value:.decimals$}%")
}

/// Group a count in threes, e.g. `184231` becomes `184,231`.
pub fn group_count(value: u64) -> String {
    group_digits(value)
}

/// Format a duration compactly, e.g. `1.5s`, `2m 04s`, or `380ms`.
///
/// Sub-second scans are common, so milliseconds are kept rather than rounding
/// to a bare `0s`.
pub fn format_duration(duration: std::time::Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1000 {
        return format!("{millis}ms");
    }

    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{}.{:01}s", total_secs, (millis % 1000) / 100)
    }
}

/// Group an integer's digits in threes with thin separators.
fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);

    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_plain_bytes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
    }

    #[test]
    fn rounds_up_into_the_next_unit_at_exactly_one_kib() {
        assert_eq!(human_bytes(1024), "1.0 KB");
    }

    #[test]
    fn formats_gigabytes_with_one_decimal() {
        let gb = 1024 * 1024 * 1024;
        assert_eq!(human_bytes(157_600_000_000), "146.8 GB");
        assert_eq!(human_bytes(gb * 5), "5.0 GB");
        assert_eq!(human_bytes(gb * 2 + gb / 2), "2.5 GB");
    }

    #[test]
    fn scales_up_to_terabytes_and_beyond() {
        let tb = 1024u64.pow(4);
        assert_eq!(human_bytes(tb), "1.0 TB");
        let pb = 1024u64.pow(5);
        assert_eq!(human_bytes(pb), "1.0 PB");
        let eb = 1024u64.pow(6);
        assert_eq!(human_bytes(eb), "1.0 EB");
    }

    #[test]
    fn caps_at_the_largest_known_unit() {
        let huge = u64::MAX;
        assert!(human_bytes(huge).ends_with(" EB"));
    }

    #[test]
    fn non_human_mode_groups_digits() {
        assert_eq!(format_bytes(0, false), "0");
        assert_eq!(format_bytes(999, false), "999");
        assert_eq!(format_bytes(1000, false), "1,000");
        assert_eq!(format_bytes(1234567, false), "1,234,567");
        assert_eq!(format_bytes(157_600_000_000, false), "157,600,000,000");
    }

    #[test]
    fn human_mode_uses_units_instead_of_grouping() {
        assert_eq!(format_bytes(1024, true), "1.0 KB");
    }

    #[test]
    fn formats_percentages_with_requested_precision() {
        assert_eq!(format_percent(97.944, 1), "97.9%");
        assert_eq!(format_percent(67.0, 1), "67.0%");
        assert_eq!(format_percent(50.0, 0), "50%");
        assert_eq!(format_percent(0.0, 1), "0.0%");
        assert_eq!(format_percent(100.0, 1), "100.0%");
    }

    #[test]
    fn groups_counts() {
        assert_eq!(group_count(0), "0");
        assert_eq!(group_count(999), "999");
        assert_eq!(group_count(1000), "1,000");
        assert_eq!(group_count(184_231), "184,231");
        assert_eq!(group_count(1_000_000), "1,000,000");
    }

    #[test]
    fn formats_durations_compactly() {
        use std::time::Duration;

        assert_eq!(format_duration(Duration::ZERO), "0ms");
        assert_eq!(format_duration(Duration::from_millis(1)), "1ms");
        assert_eq!(format_duration(Duration::from_millis(380)), "380ms");
        assert_eq!(format_duration(Duration::from_millis(999)), "999ms");
        assert_eq!(format_duration(Duration::from_millis(1000)), "1.0s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(format_duration(Duration::from_millis(4200)), "4.2s");
        assert_eq!(format_duration(Duration::from_millis(59_900)), "59.9s");
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 00s");
        assert_eq!(format_duration(Duration::from_secs(64)), "1m 04s");
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h 00m 00s");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h 02m 05s");
    }
}
