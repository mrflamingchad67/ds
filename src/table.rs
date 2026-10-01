//! Table presentation built on `tabled`.

use tabled::settings::object::Cell;
use tabled::settings::style::HorizontalLine;
use tabled::settings::{Alignment, Color, Format, Padding, Style};
use tabled::{Table, Tabled};

use crate::cli::Style as BorderStyle;

/// Column headers for the drive table, in display order.
pub const HEADERS: [&str; 5] = ["Drive", "Used", "Free", "Total", "Usage"];

/// One volume row of the drive table.
#[derive(Debug, Clone, PartialEq, Eq, Tabled)]
pub struct Row {
    /// Drive label, e.g. `C:\`.
    #[tabled(rename = "Drive")]
    pub drive: String,
    /// Human-readable used space.
    #[tabled(rename = "Used")]
    pub used: String,
    /// Human-readable free space.
    #[tabled(rename = "Free")]
    pub free: String,
    /// Human-readable total space.
    #[tabled(rename = "Total")]
    pub total: String,
    /// Percentage of the volume in use.
    #[tabled(rename = "Usage")]
    pub usage: String,
}

/// One row of the scan's directory table.
#[derive(Debug, Clone, PartialEq, Eq, Tabled)]
pub struct DirRow {
    /// Full path of the directory.
    #[tabled(rename = "Path")]
    pub path: String,
    /// Recursive size.
    #[tabled(rename = "Size")]
    pub size: String,
    /// Recursive file count.
    #[tabled(rename = "Files")]
    pub files: String,
}

/// One row of the extension table.
#[derive(Debug, Clone, PartialEq, Eq, Tabled)]
pub struct ExtRow {
    /// Lowercase extension without the dot.
    #[tabled(rename = "Extension")]
    pub extension: String,
    /// Total size for this extension.
    #[tabled(rename = "Size")]
    pub size: String,
    /// File count for this extension.
    #[tabled(rename = "Files")]
    pub files: String,
}

/// One row of the largest-files table.
#[derive(Debug, Clone, PartialEq, Eq, Tabled)]
pub struct FileRow {
    /// Full path of the file.
    #[tabled(rename = "Path")]
    pub path: String,
    /// File size.
    #[tabled(rename = "Size")]
    pub size: String,
}

/// Build the drive table from preformatted rows.
pub fn build(rows: Vec<Row>, style: BorderStyle) -> Table {
    let mut table = Table::new(rows);
    apply_style(&mut table, style);
    table
}

/// Build the directory table.
pub fn build_dirs(rows: Vec<DirRow>, style: BorderStyle) -> Table {
    let mut table = Table::new(rows);
    apply_style(&mut table, style);
    table
}

/// Build the extension table.
pub fn build_exts(rows: Vec<ExtRow>, style: BorderStyle) -> Table {
    let mut table = Table::new(rows);
    apply_style(&mut table, style);
    table
}

/// Build the largest-files table.
pub fn build_files(rows: Vec<FileRow>, style: BorderStyle) -> Table {
    let mut table = Table::new(rows);
    apply_style(&mut table, style);
    table
}

/// Colour one cell per data row.
///
/// `color` receives the 1-based `tabled` row index, so callers can map a row
/// back to its own data. Note `Cell` takes `(column, row)`.
pub fn colorize(table: &mut Table, column: usize, mut color: impl FnMut(usize) -> Color) {
    let rows = table.count_rows().saturating_sub(1);
    for row in 1..=rows {
        let paint = color(row);
        table.modify(
            Cell::new(row, column),
            Format::content(move |text| paint.colorize(text)),
        );
    }
}

/// Index of the usage column in the drive table.
pub const USAGE_COLUMN: usize = 4;

/// Index of the size column in the scan tables.
pub const SIZE_COLUMN: usize = 1;

/// Apply borders, alignment, and padding.
///
/// Each style keeps its column separators, so every column reads as a distinct
/// field. The styles are applied inline because `tabled`'s `Style` carries
/// per-edge type parameters.
fn apply_style(table: &mut Table, style: BorderStyle) {
    match style {
        BorderStyle::Rounded => table.with(Style::rounded()),
        BorderStyle::Sharp => table.with(Style::sharp()),
        // `Style::ascii` draws a rule between every row; the spec keeps only the
        // header rule. Clearing the inner horizontals first, then restoring the
        // header split alone, gives the expected four-line frame.
        BorderStyle::Ascii => table.with(
            Style::ascii()
                .remove_horizontal()
                .horizontals([(1, HorizontalLine::full('-', '+', '+', '+'))]),
        ),
    };

    table.with(Alignment::left()).with(padding());
}

/// Colour a usage percentage according to how full the volume is.
///
/// Non-human mode stays plain, since bare figures read better uncoloured.
pub fn color_usage(usage: f64, warn_above: Option<f64>, human: bool) -> Color {
    if let Some(threshold) = warn_above
        && usage >= threshold
    {
        return Color::FG_RED;
    }
    if !human {
        return Color::FG_WHITE;
    }
    if usage >= 90.0 {
        return Color::FG_RED;
    }
    if usage >= 75.0 {
        Color::FG_YELLOW
    } else {
        Color::FG_GREEN
    }
}

/// Whether a value is one of the table's headers.
pub fn is_header(name: &str) -> bool {
    HEADERS.contains(&name)
}

/// Padding shared by every cell: one space on each side, flush vertically.
pub fn padding() -> Padding {
    Padding::new(1, 1, 0, 0)
}

/// Alignment shared by every cell.
pub fn alignment() -> Alignment {
    Alignment::left()
}

/// Whether a border style is expected to render ASCII only.
pub fn is_ascii(style: BorderStyle) -> bool {
    style == BorderStyle::Ascii
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> Row {
        Row {
            drive: "C:\\".to_string(),
            used: "146.4 GB".to_string(),
            free: "3.1 GB".to_string(),
            total: "149.4 GB".to_string(),
            usage: "97.9%".to_string(),
        }
    }

    fn render(rows: Vec<Row>, style: BorderStyle) -> String {
        build(rows, style).to_string()
    }

    #[test]
    fn renders_all_five_columns() {
        let text = render(vec![row()], BorderStyle::Rounded);
        for header in HEADERS {
            assert!(text.contains(header), "missing header {header}");
        }
        assert!(text.contains("C:\\"));
        assert!(text.contains("146.4 GB"));
        assert!(text.contains("97.9%"));
    }

    #[test]
    fn rounded_borders_use_box_drawing_characters() {
        let text = render(vec![row()], BorderStyle::Rounded);
        assert!(text.contains('\u{256d}'), "missing rounded corner: {text}");
        assert!(text.contains('\u{2570}'), "missing rounded corner: {text}");
        assert!(
            text.contains('\u{2502}'),
            "missing column separator: {text}"
        );
    }

    #[test]
    fn sharp_borders_use_square_corners() {
        let text = render(vec![row()], BorderStyle::Sharp);
        assert!(text.contains('\u{250c}'), "missing square corner: {text}");
        assert!(!text.contains('\u{256d}'), "sharp style should not round");
    }

    #[test]
    fn ascii_borders_avoid_all_non_ascii_characters() {
        let text = render(vec![row()], BorderStyle::Ascii);
        assert!(text.is_ascii(), "ASCII mode must stay ASCII-only: {text}");
        assert!(text.contains('+'));
        assert!(text.contains('-'));
    }

    #[test]
    fn ascii_borders_render_a_complete_frame() {
        let text = render(vec![row(), row()], BorderStyle::Ascii);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() >= 4, "expected frame lines, got {text}");

        // Top and bottom rules are made of `+` and `-` only.
        let last = lines.len() - 1;
        for line in [lines[0], lines[last]] {
            assert!(line.starts_with('+'), "line missing corner: {line}");
            assert!(line.ends_with('+'), "line missing corner: {line}");
            assert!(
                line.chars().all(|c| matches!(c, '+' | '-')),
                "unexpected rule character: {line}"
            );
        }

        // Data rows are framed by `|` at both ends.
        let data: Vec<&str> = text.lines().filter(|l| l.contains("GB")).collect();
        assert_eq!(data.len(), 2);
        for line in data {
            assert!(line.starts_with('|'), "row missing left frame: {line}");
            assert!(line.ends_with('|'), "row missing right frame: {line}");
        }
    }

    #[test]
    fn data_rows_have_one_separator_between_columns() {
        let text = render(vec![row()], BorderStyle::Rounded);
        // A row has one leading and one trailing rule, plus four between five
        // columns, so six verticals in total.
        let body: Vec<&str> = text.lines().filter(|l| l.contains("GB")).collect();
        assert_eq!(body.len(), 1);
        assert_eq!(body[0].matches('\u{2502}').count(), 6, "bad row: {body:?}");
    }

    #[test]
    fn every_row_is_rendered() {
        let rows = vec![
            row(),
            Row {
                drive: "D:\\".into(),
                ..row()
            },
            Row {
                drive: "E:\\".into(),
                ..row()
            },
        ];
        let text = render(rows, BorderStyle::Rounded);
        for drive in ["C:\\", "D:\\", "E:\\"] {
            assert!(text.contains(drive), "missing row {drive}");
        }
    }

    #[test]
    fn empty_input_still_renders_headers() {
        let text = render(Vec::new(), BorderStyle::Rounded);
        for header in HEADERS {
            assert!(text.contains(header), "missing header {header}");
        }
    }

    #[test]
    fn rows_are_derivable_into_tables() {
        let table = Table::new(vec![row()]);
        assert!(table.to_string().contains("Drive"));
    }

    #[test]
    fn colorize_adds_escape_codes_to_the_usage_column() {
        let mut table = build(
            vec![
                row(),
                Row {
                    drive: "D:\\".into(),
                    ..row()
                },
            ],
            BorderStyle::Rounded,
        );
        colorize(&mut table, USAGE_COLUMN, |_| Color::FG_GREEN);
        let text = table.to_string();
        assert!(text.contains('\u{1b}'), "expected escapes in {text:?}");
        // Colour must not disturb the frame.
        assert!(text.contains('\u{256d}'), "frame lost its corner: {text:?}");
    }

    #[test]
    fn usage_colour_tracks_thresholds() {
        assert_eq!(color_usage(50.0, None, true), Color::FG_GREEN);
        assert_eq!(color_usage(80.0, None, true), Color::FG_YELLOW);
        assert_eq!(color_usage(97.9, None, true), Color::FG_RED);
    }

    #[test]
    fn usage_colour_honours_an_explicit_threshold() {
        assert_eq!(color_usage(50.0, Some(40.0), true), Color::FG_RED);
        assert_eq!(color_usage(30.0, Some(40.0), true), Color::FG_GREEN);
    }

    #[test]
    fn usage_colour_stays_plain_in_non_human_mode() {
        assert_eq!(color_usage(97.9, None, false), Color::FG_WHITE);
    }

    #[test]
    fn header_names_are_recognised() {
        assert!(is_header("Drive"));
        assert!(is_header("Usage"));
        assert!(!is_header("drive"));
        assert!(!is_header("Nope"));
    }

    #[test]
    fn shared_padding_is_one_space_each_side() {
        assert_eq!(padding(), Padding::new(1, 1, 0, 0));
    }

    #[test]
    fn shared_alignment_is_left() {
        assert_eq!(alignment(), Alignment::left());
    }

    #[test]
    fn only_the_ascii_style_is_ascii() {
        assert!(is_ascii(BorderStyle::Ascii));
        assert!(!is_ascii(BorderStyle::Rounded));
        assert!(!is_ascii(BorderStyle::Sharp));
    }

    #[test]
    fn header_line_leads_with_a_border_and_padded_column() {
        let text = render(vec![row()], BorderStyle::Rounded);
        let header = text
            .lines()
            .find(|l| l.contains("Drive"))
            .expect("header line");
        assert!(header.contains('\u{2502}'));
        assert!(header.contains(" Drive "));
    }
}
