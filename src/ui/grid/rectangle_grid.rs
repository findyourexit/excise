use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;

use crate::state::tiles::Tile;
use crate::theme::Theme;
use crate::ui::grid::{draw_rect_on_grid, draw_tile_text_on_grid};

fn draw_small_files_rect_on_grid(buf: &mut Buffer, rect: Rect, theme: Theme) {
    let style = Style::default()
        .bg(theme.surface_panel)
        .fg(theme.text_muted);
    for x in rect.x + 1..(rect.x + rect.width) {
        for y in rect.y + 1..(rect.y + rect.height) {
            let buf = &mut buf[(x, y)];
            buf.set_symbol("·");
            buf.set_style(style);
        }
    }
    draw_rect_on_grid(buf, (rect.x, rect.y), (rect.width, rect.height));
    if rect.width > 16 && rect.height > 2 {
        buf.set_string(rect.x + 2, rect.y + 1, "Small entries", style);
    }
}

fn draw_empty_folder(buf: &mut Buffer, area: Rect, theme: Theme) {
    let fill_style = Style::default()
        .bg(theme.surface_panel)
        .fg(theme.surface_panel);
    for x in area.x + 1..area.x + area.width {
        for y in area.y + 1..area.y + area.height {
            let buf = &mut buf[(x, y)];
            buf.set_symbol("█");
            buf.set_style(fill_style);
        }
    }
    let empty_folder_line = "Folder is empty";
    let text_length = empty_folder_line.len();
    let text_style = Style::default()
        .fg(theme.text_secondary)
        .bg(theme.surface_panel);
    let text_start_position =
        (f64::from((area.width - 1) - text_length as u16) / 2.0).ceil() as u16 + area.x;
    buf.set_string(
        text_start_position,
        (area.height / 2) + area.y - 1,
        empty_folder_line,
        text_style,
    );
    draw_rect_on_grid(buf, (area.x, area.y), (area.width - 1, area.height - 1));
}

#[derive(Clone)]
pub struct RectangleGrid<'a> {
    rectangles: &'a [Tile],
    small_files_coordinates: Option<(u16, u16)>,
    selected_rect_index: Option<usize>,
    theme: Theme,
}

impl<'a> RectangleGrid<'a> {
    pub const fn new(
        rectangles: &'a [Tile],
        small_files_coordinates: Option<(u16, u16)>,
        selected_rect_index: Option<usize>,
        theme: Theme,
    ) -> Self {
        RectangleGrid {
            rectangles,
            small_files_coordinates,
            selected_rect_index,
            theme,
        }
    }
}

impl Widget for RectangleGrid<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if self.rectangles.is_empty() {
            draw_empty_folder(buf, area, self.theme);
        } else {
            for (index, tile) in self.rectangles.iter().enumerate() {
                let selected = if let Some(selected_rect_index) = self.selected_rect_index {
                    index == selected_rect_index
                } else {
                    false
                };
                draw_tile_text_on_grid(buf, tile, selected, self.theme);
                draw_rect_on_grid(buf, (tile.x, tile.y), (tile.width, tile.height));
            }
        }
        if let Some(coords) = self.small_files_coordinates {
            let (x, y) = coords;
            let width = (area.x + area.width) - x - 1;
            let height = (area.y + area.height) - y - 1;
            let small_files_rect = Rect {
                x,
                y,
                width,
                height,
            };
            draw_small_files_rect_on_grid(buf, small_files_rect, self.theme);
        }
    }
}
