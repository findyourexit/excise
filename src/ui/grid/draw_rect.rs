use ::unicode_width::UnicodeWidthStr;
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};

use crate::native_path::SafeDisplayPath;
use crate::state::tiles::{FileType, Tile};
use crate::theme::Theme;
use crate::ui::format::{
    DisplaySize, DisplaySizeRounded, display_os_str_info, truncate_marked, truncate_middle,
};
use crate::ui::grid::{boundaries, draw_next_symbol};
fn tile_first_line(tile: &Tile) -> String {
    let max_text_length = tile.width.saturating_sub(2);
    let name = display_os_str_info(&tile.name);
    let deceptive = name.deceptive;
    let descendant_count = &tile.descendants;
    let filename_text = match tile.file_type {
        FileType::File => name.text,
        FileType::Folder => format!("{}/", name.text),
        FileType::Synthetic => format!("[{}]", name.text),
    };
    let text = match tile.file_type {
        FileType::File | FileType::Synthetic => filename_text,
        FileType::Folder => {
            let descendant_count = descendant_count.expect("folder should have descendants");
            let short_descendants_indication = format!("(+{descendant_count})");
            let long_descendants_indication = format!("(+{descendant_count} descendants)");
            if filename_text.width() + long_descendants_indication.width()
                <= usize::from(max_text_length)
            {
                format!("{filename_text} {long_descendants_indication}")
            } else if filename_text.width() + short_descendants_indication.width()
                <= usize::from(max_text_length)
            {
                format!("{filename_text} {short_descendants_indication}")
            } else {
                filename_text
            }
        }
    };
    truncate_marked(
        &SafeDisplayPath { text, deceptive },
        max_text_length,
        truncate_middle,
    )
}

fn tile_second_line(tile: &Tile) -> String {
    let max_text_length = tile.width.saturating_sub(2);
    let percentage = &tile.percentage;
    let display_size = DisplaySize(tile.size as f64);
    let display_size_rounded = DisplaySizeRounded(tile.size as f64);
    let prefix = if tile.uncertain { "≥" } else { "" };
    let display_size = if tile.uncertain && tile.size == 0 {
        "?".to_string()
    } else {
        format!("{prefix}{display_size}")
    };
    let display_size_rounded = if tile.uncertain && tile.size == 0 {
        "?".to_string()
    } else {
        format!("{prefix}{display_size_rounded}")
    };
    let display_size_len = display_size.chars().count() as u16;
    let display_size_rounded_len = display_size_rounded.chars().count() as u16;
    if max_text_length >= display_size_len + 7 {
        // 7 == "(100%)" + 1 space
        format!("{} ({:.0}%)", display_size, percentage * 100.0)
    } else if max_text_length > display_size_len {
        display_size
    } else if max_text_length > display_size_rounded_len {
        display_size_rounded
    } else if max_text_length > 6 {
        // 6 == "(100%)"
        format!("({:.0}%)", (percentage * 100.0).round())
    } else if max_text_length >= 4 {
        // 4 == "100%"
        format!("{:.0}%", (percentage * 100.0).round())
    } else {
        unreachable!("trying to render a rect of less than minimum size")
    }
}

pub fn tile_style(tile: &Tile, selected: bool, theme: Theme) -> (Option<Style>, Style, Style) {
    let selected_modifier = if theme.surface_selection == theme.text_inverse {
        Modifier::BOLD | Modifier::REVERSED
    } else {
        Modifier::BOLD
    };
    let (background_style, mut first_line_style, mut second_line_style) = if selected {
        (
            Some(
                Style::default()
                    .fg(theme.surface_selection)
                    .bg(theme.surface_selection),
            ),
            Style::default()
                .fg(theme.text_inverse)
                .bg(theme.surface_selection)
                .add_modifier(selected_modifier),
            Style::default()
                .fg(theme.text_inverse)
                .bg(theme.surface_selection)
                .add_modifier(selected_modifier),
        )
    } else {
        match tile.file_type {
            FileType::File => (
                None,
                Style::default().fg(theme.text_primary),
                Style::default().fg(theme.text_primary),
            ),
            FileType::Folder => (
                None,
                Style::default()
                    .fg(theme.focus)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(theme.text_primary),
            ),
            FileType::Synthetic => (
                None,
                Style::default()
                    .fg(theme.state_aggregated)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(theme.text_primary),
            ),
        }
    };
    if tile.uncertain {
        first_line_style = first_line_style.add_modifier(Modifier::DIM);
        second_line_style = second_line_style.add_modifier(Modifier::DIM);
    }
    (background_style, first_line_style, second_line_style)
}

pub fn draw_rect_on_grid(buf: &mut Buffer, coords: (u16, u16), dimensions: (u16, u16)) {
    let (coords_x, coords_y) = coords;
    let (width, height) = dimensions;
    if width < 1 || height < 1 {
        return;
    }

    let buf_width = buf.area().width;
    let buf_height = buf.area().height;

    // top, bottom and corners
    for x in coords_x..=(coords_x + width) {
        if x >= buf_width {
            break;
        }
        if x == coords_x {
            if coords_y < buf_height {
                draw_next_symbol(buf, x, coords_y, boundaries::TOP_LEFT);
            }
            if coords_y + height < buf_height {
                draw_next_symbol(buf, x, coords_y + height, boundaries::BOTTOM_LEFT);
            }
        } else if x == coords_x + width {
            if coords_y < buf_height {
                draw_next_symbol(buf, x, coords_y, boundaries::TOP_RIGHT);
            }
            if coords_y + height < buf_height {
                draw_next_symbol(buf, x, coords_y + height, boundaries::BOTTOM_RIGHT);
            }
        } else {
            if coords_y < buf_height {
                draw_next_symbol(buf, x, coords_y, boundaries::HORIZONTAL);
            }
            if coords_y + height < buf_height {
                draw_next_symbol(buf, x, coords_y + height, boundaries::HORIZONTAL);
            }
        }
    }

    // left and right
    for y in (coords_y + 1)..(coords_y + height) {
        if y >= buf_height {
            break;
        }
        if coords_x < buf_width {
            draw_next_symbol(buf, coords_x, y, boundaries::VERTICAL);
        }
        if coords_x + width < buf_width {
            draw_next_symbol(buf, coords_x + width, y, boundaries::VERTICAL);
        }
    }
}

pub fn draw_tile_text_on_grid(buf: &mut Buffer, tile: &Tile, selected: bool, theme: Theme) {
    let buf_width = buf.area().width;
    let buf_height = buf.area().height;

    let first_line = tile_first_line(tile);
    let first_line_length = first_line.width() as u16;
    let first_line_start_position =
        (f64::from(tile.width - first_line_length) / 2.0).ceil() as u16 + tile.x;
    let second_line = tile_second_line(tile);
    let second_line_length = second_line.width();
    let second_line_start_position =
        (f64::from(tile.width - second_line_length as u16) / 2.0).ceil() as u16 + tile.x;
    let (background_style, first_line_style, second_line_style) = tile_style(tile, selected, theme);

    if let Some(background_style) = background_style {
        for x in tile.x + 1..tile.x + tile.width {
            if x >= buf_width {
                break;
            }
            for y in tile.y + 1..tile.y + tile.height {
                if y >= buf_height {
                    break;
                }
                buf[(x, y)].set_symbol("█").set_style(background_style);
                // we set both the filling symbol and the style
                // because some terminals do not show this symbol on the one side
                // and our tests need it in order to pass on the other side
                // some terminals also don't have colors and would need this
                // as an indication so... best of all worlds!
            }
        }
    }

    if tile.height > 5 {
        let line_gap = if tile.height % 2 == 0 { 1 } else { 2 };
        buf.set_string(
            first_line_start_position,
            (tile.height / 2) + tile.y - 1,
            first_line,
            first_line_style,
        );
        buf.set_string(
            second_line_start_position,
            (tile.height / 2) + tile.y + line_gap,
            second_line,
            second_line_style,
        );
    } else if tile.height == 5 {
        buf.set_string(
            first_line_start_position,
            (tile.height / 2) + tile.y,
            first_line,
            first_line_style,
        );
        buf.set_string(
            second_line_start_position,
            (tile.height / 2) + tile.y + 1,
            second_line,
            second_line_style,
        );
    } else if tile.height == 4 {
        buf.set_string(
            first_line_start_position,
            tile.y + 1,
            first_line,
            first_line_style,
        );
        buf.set_string(
            second_line_start_position,
            tile.y + 3,
            second_line,
            second_line_style,
        );
    } else if tile.height > 2 {
        buf.set_string(
            first_line_start_position,
            tile.y + 1,
            first_line,
            first_line_style,
        );
        buf.set_string(
            second_line_start_position,
            tile.y + 2,
            second_line,
            second_line_style,
        );
    } else {
        buf.set_string(
            first_line_start_position,
            tile.y + 1,
            first_line,
            first_line_style,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use crate::model::NodeId;
    use crate::theme::ThemeId;

    use super::*;

    fn tile(file_type: FileType) -> Tile {
        Tile {
            x: 0,
            y: 0,
            width: 12,
            height: 5,
            node_id: NodeId(1),
            name: OsString::from("entry"),
            size: 1,
            apparent_size: 1,
            descendants: None,
            percentage: 1.0,
            file_type,
            synthetic_kind: None,
            uncertain: false,
        }
    }

    #[test]
    fn selected_labels_use_semantic_inverse_roles_in_every_theme() {
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            for file_type in [FileType::File, FileType::Folder, FileType::Synthetic] {
                let (background, first, second) = tile_style(&tile(file_type), true, theme);
                let background = background.expect("selected tile should paint its background");
                assert_eq!(background.bg, Some(theme.surface_selection), "{id:?}");
                assert_eq!(first.bg, Some(theme.surface_selection), "{id:?}");
                assert_eq!(second.bg, Some(theme.surface_selection), "{id:?}");
                assert_eq!(first.fg, Some(theme.text_inverse), "{id:?}");
                assert_eq!(second.fg, Some(theme.text_inverse), "{id:?}");
                assert!(first.add_modifier.contains(Modifier::BOLD), "{id:?}");
                if id == ThemeId::Monochrome {
                    assert!(first.add_modifier.contains(Modifier::REVERSED), "{id:?}");
                } else {
                    assert_ne!(theme.text_inverse, theme.surface_selection, "{id:?}");
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn hostile_tile_names_keep_escaped_text_and_marker_when_narrow() {
        use std::os::unix::ffi::OsStringExt as _;

        let mut tile = tile(FileType::File);
        tile.width = 40;
        tile.name = OsString::from_vec(b"bad\xffname".to_vec());
        let rendered = tile_first_line(&tile);
        assert!(rendered.starts_with(crate::native_path::DECEPTIVE_DISPLAY_MARKER));
        assert!(rendered.contains("bad\\xffname"));
        assert!(!rendered.chars().any(char::is_control));

        tile.name = OsString::from("prefix-\u{202e}hostile\u{1b}[31m");
        for width in 1..=24 {
            tile.width = width + 2;
            let rendered = tile_first_line(&tile);
            assert!(!rendered.chars().any(char::is_control));
            assert!(!rendered.contains('\u{202e}'));
            assert!(
                rendered.starts_with('!')
                    || rendered.starts_with(crate::native_path::DECEPTIVE_DISPLAY_MARKER),
                "deception marker lost at width {width}: {rendered:?}"
            );
        }
    }
}
