//! The screen model: what a user would see.
//!
//! [`Screen`] wraps a `vt100` terminal emulator. Everything the harness asserts about the display
//! comes from here, never from raw output bytes: raw matching confirmed the wrong deletion target
//! in the spike that led to this harness.
//!
//! Besides text, the model finds *boxes*: the bordered panes and dialogs `excise` draws, whose top
//! border is a corner glyph, a padded title, a run of border glyphs, and a corner glyph. See
//! [`Screen::boxes`].

use std::fmt;

use crate::scenario::Region;

/// The rows of the header band: the title, the totals, and the status line.
pub const HEADER_ROWS: u16 = 3;

/// The tallest box the model looks for, in rows. It bounds the search for a bottom border.
const MAX_BOX_ROWS: u16 = 30;

/// Answers a cursor position report request (`ESC [ 6 n`) with the cursor position of the
/// emulator at the moment the request was parsed.
#[derive(Debug, Default)]
struct Responder {
    replies: Vec<u8>,
    /// How many cursor position report requests have been answered, in all.
    answered: u32,
}

impl vt100::Callbacks for Responder {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        intermediate_1: Option<u8>,
        intermediate_2: Option<u8>,
        params: &[&[u16]],
        final_byte: char,
    ) {
        if final_byte == 'n'
            && intermediate_1.is_none()
            && intermediate_2.is_none()
            && params.len() == 1
            && params[0] == [6]
        {
            let (row, column) = screen.cursor_position();
            let (_, columns) = screen.size();
            // The emulator keeps a pending wrap as a column one past the edge; a terminal
            // reports the last column.
            let column = column.min(columns.saturating_sub(1));
            self.replies
                .extend_from_slice(format!("\u{1b}[{};{}R", row + 1, column + 1).as_bytes());
            self.answered += 1;
        }
    }
}

/// The terminal modes that the emulator tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenModes {
    /// The alternate screen is active.
    pub alternate_screen: bool,
    /// The cursor is visible.
    pub cursor_visible: bool,
}

/// The inclusive cell rectangle of a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxRect {
    /// The column of the left border.
    pub left: u16,
    /// The row of the top border.
    pub top: u16,
    /// The column of the right border.
    pub right: u16,
    /// The row of the bottom border.
    pub bottom: u16,
}

/// A bordered box on the screen, such as a pane or a dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxView {
    /// The title in the top border, without its padding.
    pub title: String,
    /// Where the box is.
    pub rect: BoxRect,
    /// The rows between the borders, without the borders. Text keeps the position it has on the
    /// screen; use `trim` for centred text.
    pub interior: Vec<String>,
    /// The whole box as drawn, borders included, one line per row with trailing spaces removed.
    pub text: String,
}

/// The glyphs of one border style.
struct Border {
    top_left: &'static str,
    top_right: &'static str,
    fill: &'static str,
    left: &'static str,
    right: &'static str,
}

const UNICODE_BORDER: Border = Border {
    top_left: "▟",
    top_right: "▜",
    fill: "▔",
    left: "▏",
    right: "▕",
};

const ASCII_BORDER: Border = Border {
    top_left: "+",
    top_right: "+",
    fill: "-",
    left: "|",
    right: "|",
};

/// A terminal emulator and the queries the harness makes of it.
pub struct Screen {
    parser: vt100::Parser<Responder>,
}

impl fmt::Debug for Screen {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Screen")
            .field("size", &self.size())
            .field("cursor", &self.cursor())
            .finish_non_exhaustive()
    }
}

impl Screen {
    /// An empty screen of `rows` by `cols` cells.
    #[must_use]
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, 0, Responder::default()),
        }
    }

    /// Feeds terminal output to the emulator and returns the bytes it must answer with.
    ///
    /// The answers are the cursor position reports that the output asked for, each computed from
    /// the cursor position at the point in the stream where the request appeared.
    pub fn process(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.process(bytes);
        std::mem::take(&mut self.parser.callbacks_mut().replies)
    }

    /// How many cursor position report requests (`ESC [ 6 n`) this screen has answered, in all.
    /// A step that times out on an empty screen reports this: zero means the program's output
    /// never reached the screen model at all, not even the handshake `ConPTY` waits for on
    /// Windows (see the module documentation of [`crate::pty::session`]).
    #[must_use]
    pub fn cursor_reports_answered(&self) -> u32 {
        self.parser.callbacks().answered
    }

    /// Resizes the emulator.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// The size as `(rows, cols)`.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    /// The cursor position as `(row, column)`, both zero-based.
    #[must_use]
    pub fn cursor(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    /// The alternate-screen and cursor-visibility modes.
    #[must_use]
    pub fn modes(&self) -> ScreenModes {
        let screen = self.parser.screen();
        ScreenModes {
            alternate_screen: screen.alternate_screen(),
            cursor_visible: !screen.hide_cursor(),
        }
    }

    /// The text of the cell at `row` and `col`: a space for an empty cell and the empty string for
    /// the second half of a wide character.
    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> &str {
        match self.parser.screen().cell(row, col) {
            Some(cell) if cell.has_contents() => cell.contents(),
            Some(cell) if cell.is_wide_continuation() => "",
            _ => " ",
        }
    }

    /// The visible text of `row`, without trailing spaces. An out-of-range row is empty.
    #[must_use]
    pub fn row_text(&self, row: u16) -> String {
        let (_, cols) = self.size();
        self.parser
            .screen()
            .rows(0, cols)
            .nth(usize::from(row))
            .map(|text| text.trim_end().to_owned())
            .unwrap_or_default()
    }

    /// The visible text of the rows `first` to `last`, inclusive, one line per row without
    /// trailing spaces. Rows outside the screen are left out.
    #[must_use]
    pub fn rows_text(&self, first: u16, last: u16) -> String {
        let (_, cols) = self.size();
        self.parser
            .screen()
            .rows(0, cols)
            .enumerate()
            .filter(|(row, _)| (usize::from(first)..=usize::from(last)).contains(row))
            .map(|(_, text)| text.trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole screen as text: each row's visible cells left to right with trailing spaces
    /// removed, rows joined with a newline.
    #[must_use]
    pub fn text(&self) -> String {
        let (rows, _) = self.size();
        self.rows_text(0, rows.saturating_sub(1))
    }

    /// The text of `region`, or of the whole screen for `None`.
    ///
    /// The `dialog` region is empty when no dialog is open.
    #[must_use]
    pub fn region_text(&self, region: Option<Region>) -> String {
        match region {
            None => self.text(),
            Some(Region::Header) => self.rows_text(0, HEADER_ROWS - 1),
            Some(Region::Rows([first, last])) => self.rows_text(first, last),
            Some(Region::Dialog) => self.dialog().map(|dialog| dialog.text).unwrap_or_default(),
        }
    }

    /// Every titled, closed box on the screen, from the top down.
    ///
    /// A box's top border is a corner glyph, a space, the title, a space, a run of border glyphs,
    /// and a corner glyph. Below it, every row has a vertical border glyph at both edges, and the
    /// box ends at a row made only of border glyphs. Both the Unicode and the ASCII border styles
    /// are recognised. A box that is still being drawn, and so lacks a bottom edge, is not
    /// reported.
    #[must_use]
    pub fn boxes(&self) -> Vec<BoxView> {
        let (rows, cols) = self.size();
        let mut found = Vec::new();
        for row in 0..rows {
            for border in [&UNICODE_BORDER, &ASCII_BORDER] {
                let mut col = 0;
                while col + 2 < cols {
                    if self.cell(row, col) == border.top_left
                        && self.cell(row, col + 1) == " "
                        && let Some(view) = self.read_box(row, col, border)
                    {
                        col = view.rect.right + 1;
                        found.push(view);
                    } else {
                        col += 1;
                    }
                }
            }
        }
        found.sort_by_key(|view| (view.rect.top, view.rect.left));
        found
    }

    /// The modal dialog: the box that is inset from both screen edges and nearest the centre.
    ///
    /// `excise` draws a dialog centred, with a margin on each side, and draws its panes across the
    /// whole width, so the first rule tells them apart. With no such box there is no dialog.
    #[must_use]
    pub fn dialog(&self) -> Option<BoxView> {
        let (rows, cols) = self.size();
        let offset = |twice_a: u16, twice_b: u16, extent: u16| {
            (i32::from(twice_a) + i32::from(twice_b) - i32::from(extent.saturating_sub(1))).abs()
        };
        self.boxes()
            .into_iter()
            .filter(|view| view.rect.left >= 1 && view.rect.right + 2 <= cols)
            .min_by_key(|view| {
                offset(view.rect.left, view.rect.right, cols)
                    + offset(view.rect.top, view.rect.bottom, rows)
            })
    }

    /// The box titled `title`, if one is drawn.
    #[must_use]
    pub fn box_titled(&self, title: &str) -> Option<BoxView> {
        self.boxes().into_iter().find(|view| view.title == title)
    }

    fn read_box(&self, top: u16, left: u16, border: &Border) -> Option<BoxView> {
        let (rows, cols) = self.size();
        let right = (left + 2..cols).find(|col| self.cell(top, *col) == border.top_right)?;
        let mut fill_start = right;
        while fill_start > left + 2 && self.cell(top, fill_start - 1) == border.fill {
            fill_start -= 1;
        }
        // The title tab is a space, the title, and a space, and the border run starts after it.
        if fill_start < left + 4 || self.cell(top, fill_start - 1) != " " {
            return None;
        }
        let title = self.cells_text(top, left + 1, fill_start).trim().to_owned();
        if title.is_empty() {
            return None;
        }

        let mut interior = Vec::new();
        let last_row = rows.min(top.saturating_add(MAX_BOX_ROWS));
        for row in top + 1..last_row {
            if self.cell(row, left) == border.left && self.cell(row, right) == border.right {
                interior.push(self.cells_text(row, left + 1, right));
            } else if (left..=right).all(|col| self.cell(row, col) == border.fill) {
                let rect = BoxRect {
                    left,
                    top,
                    right,
                    bottom: row,
                };
                let text = (top..=row)
                    .map(|row| self.cells_text(row, left, right + 1).trim_end().to_owned())
                    .collect::<Vec<_>>()
                    .join("\n");
                return Some(BoxView {
                    title,
                    rect,
                    interior,
                    text,
                });
            } else {
                return None;
            }
        }
        None
    }

    /// The text of `row` from `from` up to, but not including, `to`.
    fn cells_text(&self, row: u16, from: u16, to: u16) -> String {
        (from..to).map(|col| self.cell(row, col)).collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fmt::Write as _;

    use super::*;

    /// A screen with `lines` drawn at the given `(row, column)` cells.
    pub(crate) fn drawn(rows: u16, cols: u16, lines: &[(u16, u16, &str)]) -> Screen {
        let mut screen = Screen::new(rows, cols);
        let mut bytes = String::new();
        for (row, col, text) in lines {
            let _ = write!(bytes, "\u{1b}[{};{}H{text}", row + 1, col + 1);
        }
        screen.process(bytes.as_bytes());
        screen
    }

    /// The rows of a Unicode box `width` cells wide with the given title and body lines. The
    /// lines are centred, like the dialogs `excise` draws.
    pub(crate) fn unicode_box(title: &str, width: usize, body: &[&str]) -> Vec<String> {
        boxed(&UNICODE_BORDER, title, width, body)
    }

    pub(crate) fn ascii_box(title: &str, width: usize, body: &[&str]) -> Vec<String> {
        boxed(&ASCII_BORDER, title, width, body)
    }

    fn boxed(border: &Border, title: &str, width: usize, body: &[&str]) -> Vec<String> {
        let tab = format!(" {title} ");
        let fill = width - 2 - tab.chars().count();
        let mut rows = vec![format!(
            "{}{}{}{}",
            border.top_left,
            tab,
            border.fill.repeat(fill),
            border.top_right
        )];
        for line in body {
            let padding = width - 2 - line.chars().count();
            rows.push(format!(
                "{}{}{}{}{}",
                border.left,
                " ".repeat(padding / 2),
                line,
                " ".repeat(padding - padding / 2),
                border.right
            ));
        }
        rows.push(border.fill.repeat(width));
        rows
    }

    /// Draws the rows of a box with its top-left corner at `(row, col)`.
    pub(crate) fn place(screen: &mut Screen, row: u16, col: u16, lines: &[String]) {
        let mut bytes = String::new();
        for (offset, line) in lines.iter().enumerate() {
            let offset = u16::try_from(offset).expect("a small box");
            let _ = write!(bytes, "\u{1b}[{};{}H{line}", row + offset + 1, col + 1);
        }
        screen.process(bytes.as_bytes());
    }

    #[test]
    fn a_cursor_position_request_is_answered_with_the_cursor_position() {
        let mut screen = Screen::new(24, 80);

        let reply = screen.process(b"\x1b[3;5Habc\x1b[6n");

        assert_eq!(reply, b"\x1b[3;8R", "the cursor moved past `abc`");
    }

    #[test]
    fn every_request_in_one_read_gets_its_own_answer_at_its_own_position() {
        let mut screen = Screen::new(24, 80);

        let reply = screen.process(b"\x1b[6nx\x1b[10;20H\x1b[6n");

        assert_eq!(reply, b"\x1b[1;1R\x1b[10;20R");
    }

    #[test]
    fn cursor_reports_answered_counts_every_request_seen_including_split_ones() {
        let mut screen = Screen::new(24, 80);
        assert_eq!(screen.cursor_reports_answered(), 0);

        screen.process(b"\x1b[6nx\x1b[10;20H\x1b[6n");
        assert_eq!(screen.cursor_reports_answered(), 2);

        screen.process(b"\x1b[");
        screen.process(b"6");
        screen.process(b"n");
        assert_eq!(screen.cursor_reports_answered(), 3);
    }

    #[test]
    fn a_request_split_across_reads_is_answered_once() {
        let mut screen = Screen::new(24, 80);

        assert!(screen.process(b"\x1b[2;2H\x1b[").is_empty());
        assert!(screen.process(b"6").is_empty());
        assert_eq!(screen.process(b"n"), b"\x1b[2;2R");
    }

    #[test]
    fn the_answer_reports_the_last_column_not_a_pending_wrap() {
        let mut screen = Screen::new(5, 10);

        let reply = screen.process(b"0123456789\x1b[6n");

        assert_eq!(reply, b"\x1b[1;10R");
    }

    #[test]
    fn other_queries_are_not_answered() {
        let mut screen = Screen::new(24, 80);

        assert!(screen.process(b"\x1b[c\x1b[5n\x1b[?6n\x1b[>c").is_empty());
    }

    #[test]
    fn text_is_the_visible_cells_without_trailing_spaces() {
        let screen = drawn(4, 20, &[(0, 2, "title  "), (2, 0, "row two")]);

        assert_eq!(screen.row_text(0), "  title");
        assert_eq!(screen.text(), "  title\n\nrow two\n");
        assert_eq!(screen.rows_text(2, 2), "row two");
        assert_eq!(screen.rows_text(2, 99), "row two\n");
        assert_eq!(screen.row_text(99), "");
    }

    #[test]
    fn the_header_region_is_the_top_three_rows() {
        let screen = drawn(6, 20, &[(0, 0, "one"), (2, 0, "three"), (3, 0, "four")]);

        assert_eq!(screen.region_text(Some(Region::Header)), "one\n\nthree");
        assert_eq!(
            screen.region_text(Some(Region::Rows([2, 3]))),
            "three\nfour"
        );
    }

    #[test]
    fn alternate_screen_and_cursor_modes_follow_the_output() {
        let mut screen = Screen::new(5, 20);
        assert_eq!(
            screen.modes(),
            ScreenModes {
                alternate_screen: false,
                cursor_visible: true
            }
        );

        screen.process(b"\x1b[?1049h\x1b[?25l");
        assert_eq!(
            screen.modes(),
            ScreenModes {
                alternate_screen: true,
                cursor_visible: false
            }
        );

        screen.process(b"\x1b[?25h\x1b[?1049l");
        assert!(!screen.modes().alternate_screen && screen.modes().cursor_visible);
    }

    #[test]
    fn resizing_changes_the_reported_size() {
        let mut screen = Screen::new(10, 40);

        screen.resize(5, 30);

        assert_eq!(screen.size(), (5, 30));
    }

    #[test]
    fn wide_characters_take_two_cells_and_read_back_once() {
        let screen = drawn(2, 10, &[(0, 0, "漢字x")]);

        assert_eq!(screen.row_text(0), "漢字x");
        assert_eq!(screen.cell(0, 0), "漢");
        assert_eq!(screen.cell(0, 1), "", "the second half of a wide character");
        assert_eq!(screen.cell(0, 4), "x");
    }

    #[test]
    fn a_dialog_is_the_inset_box_and_a_full_width_pane_is_not() {
        let mut screen = Screen::new(20, 60);
        // The pane spans the whole screen, so the dialog sits inside it and leaves its edges whole.
        place(
            &mut screen,
            0,
            0,
            &unicode_box("STORAGE MAP", 60, &[""; 18]),
        );
        place(
            &mut screen,
            5,
            10,
            &unicode_box(
                "! DELETE FOLDER",
                40,
                &["/tmp/x/victim", "Deletion continues.", "[Enter/y] start"],
            ),
        );

        let boxes = screen.boxes();
        let dialog = screen.dialog().expect("a dialog");

        assert_eq!(
            boxes
                .iter()
                .map(|view| view.title.as_str())
                .collect::<Vec<_>>(),
            ["STORAGE MAP", "! DELETE FOLDER"]
        );
        assert_eq!(dialog.title, "! DELETE FOLDER");
        assert_eq!(
            dialog.rect,
            BoxRect {
                left: 10,
                top: 5,
                right: 49,
                bottom: 9
            }
        );
        assert_eq!(
            dialog
                .interior
                .iter()
                .map(|line| line.trim())
                .collect::<Vec<_>>(),
            ["/tmp/x/victim", "Deletion continues.", "[Enter/y] start"]
        );
        assert!(dialog.text.starts_with("▟ ! DELETE FOLDER ▔"));
        assert!(dialog.text.ends_with("▔▔▔"));
        assert_eq!(dialog.text.lines().count(), 5);
    }

    #[test]
    fn ascii_boxes_are_recognised_too() {
        let mut screen = Screen::new(12, 50);
        place(
            &mut screen,
            2,
            5,
            &ascii_box("QUIT", 30, &["Quit Excise?", "[y] Quit"]),
        );

        let dialog = screen.dialog().expect("an ASCII dialog");

        assert_eq!(dialog.title, "QUIT");
        assert_eq!(
            dialog
                .interior
                .iter()
                .map(|line| line.trim())
                .collect::<Vec<_>>(),
            ["Quit Excise?", "[y] Quit"]
        );
    }

    #[test]
    fn a_box_without_its_bottom_edge_is_not_a_dialog_yet() {
        let mut screen = Screen::new(12, 50);
        let mut rows = unicode_box("QUIT", 30, &["Quit Excise?", "[y] Quit"]);
        rows.pop();
        place(&mut screen, 2, 5, &rows);

        assert_eq!(screen.dialog(), None);
    }

    #[test]
    fn map_glyphs_beside_a_dialog_do_not_change_what_it_contains() {
        let mut screen = Screen::new(12, 60);
        place(
            &mut screen,
            0,
            0,
            &unicode_box(
                "STORAGE MAP",
                60,
                &[
                    "▒░ · ▓▒░░ · ▓▒░ ·",
                    "· ▒░░ · ▓▒░ ·",
                    "▓▒░",
                    "▒░",
                    "",
                    "",
                    "",
                    "",
                    "",
                ],
            ),
        );
        place(
            &mut screen,
            3,
            12,
            &unicode_box("QUIT", 36, &["Quit Excise?", "[y] Quit"]),
        );

        let dialog = screen.dialog().expect("a dialog");

        assert_eq!(
            dialog
                .interior
                .iter()
                .map(|line| line.trim())
                .collect::<Vec<_>>(),
            ["Quit Excise?", "[y] Quit"]
        );
        assert!(!dialog.text.contains('▒'));
    }

    #[test]
    fn the_dialog_region_is_the_box_and_is_empty_without_one() {
        let mut screen = Screen::new(12, 50);
        assert_eq!(screen.region_text(Some(Region::Dialog)), "");

        place(
            &mut screen,
            2,
            5,
            &unicode_box("QUIT", 30, &["Quit Excise?", "[y] Quit"]),
        );

        let text = screen.region_text(Some(Region::Dialog));
        assert!(
            text.contains("Quit Excise?") && text.contains("[y] Quit"),
            "{text}"
        );
    }

    #[test]
    fn a_box_can_be_found_by_its_title() {
        let mut screen = Screen::new(20, 60);
        place(
            &mut screen,
            10,
            0,
            &unicode_box("SELECTED ITEM", 60, &["victim", "◆ COMPLETE · folder"]),
        );

        let pane = screen.box_titled("SELECTED ITEM").expect("the pane");

        assert_eq!(pane.rect.left, 0);
        assert_eq!(pane.interior[0].trim(), "victim");
        assert_eq!(screen.box_titled("QUIT"), None);
    }
}
