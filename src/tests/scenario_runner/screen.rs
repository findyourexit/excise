//! The screen model: what a person would read off the terminal.
//!
//! A screen is the visible text of every row, cell by cell. A cell that a double-width glyph
//! covers is empty. A row's text is its cells left to right with trailing spaces removed, and the
//! screen's text is its rows joined with `\n`. Assertions match this text, never styles.
//!
//! The parsers here read the shapes the product draws: the header band, modal dialogs, the
//! selected-item panel, and the filter prompt.

use excise_harness::scenario::{Region, ScanState};
use ratatui::buffer::Buffer;
use unicode_width::UnicodeWidthStr as _;

/// The rows of the header band: the title row, the storage summary, and the status line.
const HEADER_ROWS: usize = 3;
/// The row of the header band that carries the scan state badge.
const STATE_ROW: usize = 0;
/// The row of the header band that carries the status line, and with it the filter prompt.
const STATUS_ROW: usize = 2;

/// The titles of the product's modal dialogs. Panes share the same chrome, so a box is a dialog
/// only when its title is one of these.
const DIALOG_TITLES: [&str; 9] = [
    "! DELETE FOLDER",
    "! DELETE FILE",
    "! DELETE ITEM",
    "QUIT",
    "ERROR",
    "SCAN IN PROGRESS",
    "HELP",
    "DONE",
    "THEME PREVIEW",
];
/// The title of the panel that describes the selected map entry.
const SELECTED_ITEM_TITLE: &str = "SELECTED ITEM";
/// What the selected-item panel says when nothing is selected.
const NO_SELECTION: &str = "Choose an item";
/// The first line of the deletion challenge that asks for typed text.
const CHALLENGE: &str = "Type this exactly:";

/// The characters a pane or dialog border is drawn with.
struct Chrome {
    top_left: char,
    top_right: char,
    horizontal: char,
    left: char,
    right: char,
}

const UNICODE_CHROME: Chrome = Chrome {
    top_left: '▟',
    top_right: '▜',
    horizontal: '▔',
    left: '▏',
    right: '▕',
};

const ASCII_CHROME: Chrome = Chrome {
    top_left: '+',
    top_right: '+',
    horizontal: '-',
    left: '|',
    right: '|',
};

/// The visible text of the terminal, by row and cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    cells: Vec<Vec<String>>,
}

/// A bordered box on the screen: a pane or a dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Panel {
    title: String,
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
    /// The content rows between the borders, each trimmed, without the blank ones.
    lines: Vec<String>,
}

/// The entry the selected-item panel describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedItem {
    /// The name line. The panel shortens long names in the middle.
    pub name: String,
}

impl Screen {
    /// Reads the visible text off a terminal buffer.
    pub fn from_buffer(buffer: &Buffer) -> Self {
        let mut cells = Vec::with_capacity(usize::from(buffer.area.height));
        for y in 0..buffer.area.height {
            let mut row = Vec::with_capacity(usize::from(buffer.area.width));
            let mut covered = 0;
            for x in 0..buffer.area.width {
                if covered > 0 {
                    row.push(String::new());
                    covered -= 1;
                    continue;
                }
                let symbol = buffer[(x, y)].symbol();
                covered = symbol.width().saturating_sub(1);
                row.push(symbol.to_owned());
            }
            cells.push(row);
        }
        Self { cells }
    }

    /// A screen that shows `lines`, padded with spaces to the widest one. For tests of the
    /// parsers.
    pub fn from_lines(lines: &[&str]) -> Self {
        Self::from_buffer(&Buffer::with_lines(lines.iter().copied()))
    }

    pub fn height(&self) -> usize {
        self.cells.len()
    }

    /// The text of one row, or an empty string below the screen.
    pub fn row(&self, y: usize) -> String {
        self.cells
            .get(y)
            .map_or_else(String::new, |row| row.concat().trim_end().to_owned())
    }

    /// The text of the whole screen.
    pub fn text(&self) -> String {
        self.rows_text(0, self.height().saturating_sub(1))
    }

    /// The text of the rows `first..=last`, clamped to the screen.
    fn rows_text(&self, first: usize, last: usize) -> String {
        (first..=last.min(self.height().saturating_sub(1)))
            .map(|y| self.row(y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The text of a region, or `None` when the region is a dialog and none is open.
    pub fn region_text(&self, region: Option<Region>) -> Option<String> {
        match region {
            None => Some(self.text()),
            Some(Region::Header) => Some(self.rows_text(0, HEADER_ROWS - 1)),
            Some(Region::Rows([first, last])) => {
                Some(self.rows_text(usize::from(first), usize::from(last)))
            }
            Some(Region::Dialog) => self.dialog().map(|dialog| dialog.rect_text(self)),
        }
    }

    /// The scan state in the header's state badge. The badge ends the title row; the same words
    /// appear elsewhere on screen and mean nothing there.
    pub fn header_state(&self) -> Option<ScanState> {
        let title = self.row(STATE_ROW);
        if title.ends_with(" SCANNING") {
            Some(ScanState::Scanning)
        } else if title.ends_with(" COMPLETE") {
            Some(ScanState::Complete)
        } else {
            None
        }
    }

    /// The modal dialog, when one is open.
    pub fn dialog(&self) -> Option<Panel> {
        self.panels()
            .into_iter()
            .find(|panel| DIALOG_TITLES.contains(&panel.title.as_str()))
    }

    /// The entry the selected-item panel describes, or `None` when the panel is absent (small
    /// terminals omit it) or nothing is selected.
    pub fn selected_item(&self) -> Option<SelectedItem> {
        let panel = self
            .panels()
            .into_iter()
            .find(|panel| panel.title == SELECTED_ITEM_TITLE)?;
        let name = panel.lines.first()?;
        (!name.starts_with(NO_SELECTION)).then(|| SelectedItem { name: name.clone() })
    }

    /// The text in the open filter prompt, or `None` when the filter is not open.
    ///
    /// The status line reads `/ <text>_  [Enter] apply  [Esc] cancel`, or ends in the filter's
    /// error, after a safety label when the program runs elevated and cut short in the middle on
    /// a narrow terminal. Only the prompt has `_  [` or `_  ERROR` after its text, and no safety
    /// label holds a `/ `.
    pub fn filter_prompt(&self) -> Option<String> {
        let status = self.row(STATUS_ROW);
        let end = status.find("_  [").or_else(|| status.find("_  ERROR"))?;
        let start = status[..end].find("/ ")?;
        Some(status[start + 2..end].to_owned())
    }

    /// Whether typed characters go into a text prompt: the filter, or a deletion challenge that
    /// asks for typed text.
    pub fn text_entry_open(&self) -> bool {
        self.filter_prompt().is_some()
            || self.dialog().is_some_and(|dialog| {
                dialog
                    .lines()
                    .iter()
                    .any(|line| line.starts_with(CHALLENGE))
            })
    }

    /// Every bordered box with a title on the screen.
    fn panels(&self) -> Vec<Panel> {
        let mut found = Vec::new();
        for (y, row) in self.cells.iter().enumerate() {
            for (x, cell) in row.iter().enumerate() {
                for chrome in [&UNICODE_CHROME, &ASCII_CHROME] {
                    if is_glyph(cell, chrome.top_left)
                        && let Some(panel) = self.panel_at(x, y, chrome)
                    {
                        found.push(panel);
                    }
                }
            }
        }
        found
    }

    /// The box whose top-left corner is at `(left, top)`, if the cells there draw one: a top
    /// border with a title tab, then side borders, then a bottom border.
    fn panel_at(&self, left: usize, top: usize, chrome: &Chrome) -> Option<Panel> {
        let top_row = self.cells.get(top)?;
        let right = (left + 1..top_row.len()).find(|&x| is_glyph(&top_row[x], chrome.top_right))?;
        let border = top_row[left + 1..right].concat();
        let title = border
            .trim_end_matches([chrome.horizontal, ' '])
            .trim()
            .to_owned();
        if title.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        for y in top + 1..self.height() {
            let row = &self.cells[y];
            let (Some(first), Some(last)) = (row.get(left), row.get(right)) else {
                return None;
            };
            if is_glyph(first, chrome.left) && is_glyph(last, chrome.right) {
                let line = row[left + 1..right].concat().trim().to_owned();
                if !line.is_empty() {
                    lines.push(line);
                }
            } else if is_glyph(first, chrome.horizontal) && is_glyph(last, chrome.horizontal) {
                return Some(Panel {
                    title,
                    left,
                    top,
                    right,
                    bottom: y,
                    lines,
                });
            } else {
                return None;
            }
        }
        None
    }
}

impl Panel {
    /// The title from the top border.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The content lines between the borders, trimmed, without the blank ones.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The box as a region of the screen: its rows and columns, borders included.
    pub fn rect_text(&self, screen: &Screen) -> String {
        (self.top..=self.bottom)
            .map(|y| {
                screen.cells[y][self.left..=self.right]
                    .concat()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Whether a cell holds exactly one glyph.
fn is_glyph(cell: &str, glyph: char) -> bool {
    let mut characters = cell.chars();
    characters.next() == Some(glyph) && characters.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows of a box `width` cells wide: a top border with its title tab, the `inner` lines,
    /// and a bottom border.
    fn boxed(title: &str, inner: &[&str], width: usize) -> Vec<String> {
        let tab = format!(" {title} ");
        let fill = "▔".repeat(width - 2 - tab.chars().count());
        let mut rows = vec![format!("▟{tab}{fill}▜")];
        rows.extend(
            inner
                .iter()
                .map(|line| format!("▏{line:<w$}▕", w = width - 2)),
        );
        rows.push("▔".repeat(width));
        rows
    }

    fn lines_of(rows: &[String]) -> Screen {
        Screen::from_lines(&rows.iter().map(String::as_str).collect::<Vec<_>>())
    }

    #[test]
    fn the_state_badge_is_the_end_of_the_title_row_and_nothing_else() {
        // A folder named after the other state, and the selected-item panel's own state line,
        // are not the header's state.
        let scanning = Screen::from_lines(&[
            " EXCISE  /work/my COMPLETE notes  ◌ SCANNING ",
            " Space used 1K",
            "Excise · store 1K/2G",
            "▏◆ COMPLETE · file ▕",
        ]);
        assert_eq!(scanning.header_state(), Some(ScanState::Scanning));
        let complete = Screen::from_lines(&[
            " EXCISE  /work/my SCANNING notes  C COMPLETE ",
            "~ SCANNING",
        ]);
        assert_eq!(complete.header_state(), Some(ScanState::Complete));
        // Any other state of the badge is neither.
        let rebuilding = Screen::from_lines(&[" EXCISE  /work  ◌ REBUILDING ", "◆ COMPLETE"]);
        assert_eq!(rebuilding.header_state(), None);
    }

    #[test]
    fn the_filter_prompt_survives_a_safety_label_and_a_narrow_terminal() {
        let status = |text: &str| Screen::from_lines(&["title", "detail", text]).filter_prompt();
        assert_eq!(
            status("/ abc_  [Enter] apply  [Esc] cancel · store 1K/2G"),
            Some("abc".to_owned())
        );
        // Elevated: a label comes first.
        assert_eq!(
            status("! ELEVATED · / abc_  [Enter] apply  [Esc] cancel"),
            Some("abc".to_owned())
        );
        // Narrow: the middle is cut out, the text before it stays.
        assert_eq!(status("/ abc_  [E[...]] cancel"), Some("abc".to_owned()));
        assert_eq!(status("/ _  ERROR: invalid glob"), Some(String::new()));
        // Other statuses are not the prompt, even when they hold a path.
        assert_eq!(status("Excise · store 1K/2G"), None);
        assert_eq!(
            status("Waiting for confirmation: /tmp/a b · 0 waiting"),
            None
        );
    }

    #[test]
    fn a_box_is_found_by_its_title_at_its_cell_position_past_wide_glyphs() {
        // Two double-width glyphs take four cells, so the box starts at cell 4, not character 2.
        // The cells the glyphs cover hold no text of their own.
        let dialog = boxed("QUIT", &["Quit?"], 11);
        let rows = [
            format!("日本{}", dialog[0]),
            format!("    {}", dialog[1]),
            format!("    {}", dialog[2]),
        ];
        let screen = lines_of(&rows);
        assert_eq!(screen.row(0), format!("日本{}", dialog[0]));
        let found = screen.dialog().expect("the dialog should be found");
        assert_eq!(found.title(), "QUIT");
        assert_eq!(found.lines(), ["Quit?"]);
        assert_eq!(
            screen.region_text(Some(Region::Dialog)).as_deref(),
            Some("▟ QUIT ▔▔▔▜\n▏Quit?    ▕\n▔▔▔▔▔▔▔▔▔▔▔")
        );
    }

    #[test]
    fn panes_are_not_dialogs_and_the_selected_item_is_read_from_its_panel() {
        let workspace = boxed("STORAGE MAP", &["tiles"], 20);
        let pane_only = lines_of(&workspace);
        assert_eq!(pane_only.dialog(), None);
        assert_eq!(pane_only.region_text(Some(Region::Dialog)), None);

        let selected = boxed("SELECTED ITEM", &["victim.bin", "◆ COMPLETE · file"], 24);
        assert_eq!(
            lines_of(&selected).selected_item(),
            Some(SelectedItem {
                name: "victim.bin".to_owned()
            })
        );
        let nothing = boxed(
            "SELECTED ITEM",
            &["Choose an item to see its", "space."],
            30,
        );
        assert_eq!(lines_of(&nothing).selected_item(), None);
    }
}
