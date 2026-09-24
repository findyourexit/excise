use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;

use crate::theme::{Theme, ThemeId};
use crate::ui::pane::{ModalChrome, readable_text_on, render_modal};

/// A keyboard-only list of built-in themes. Selection previews immediately.
/// Committing saves the selected theme immediately. Restoring leaves the saved
/// preference untouched.
pub struct ThemePicker {
    selected: ThemeId,
    theme: Theme,
    ascii: bool,
    chrome: ModalChrome,
}

impl ThemePicker {
    pub(crate) const fn new(
        selected: ThemeId,
        theme: Theme,
        ascii: bool,
        chrome: ModalChrome,
    ) -> Self {
        Self {
            selected,
            theme,
            ascii,
            chrome,
        }
    }
}

impl Widget for ThemePicker {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let width = area.width.saturating_sub(4).clamp(30, 52).min(area.width);
        let height = area.height.saturating_sub(2).clamp(8, 22).min(area.height);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let inner = render_modal(
            buffer,
            rect,
            "THEME PREVIEW",
            self.theme,
            self.theme.focus,
            self.ascii,
            self.chrome,
        );
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let text = readable_text_on(self.theme, self.theme.surface_raised);
        buffer.set_stringn(
            inner.x,
            inner.y,
            "Preview applies immediately; Enter saves",
            usize::from(inner.width),
            Style::default().fg(text),
        );
        let instruction_y = inner.bottom().saturating_sub(1);
        let instruction = if inner.width >= 50 {
            "[Up/Down/j/k] preview  [Enter] save  [Esc] restore"
        } else if inner.width >= 46 {
            "[Up/Down] preview  [Enter] save  [Esc] restore"
        } else if self.ascii {
            "[Up/Dn] Enter save Esc undo"
        } else {
            "[↑↓] Enter save Esc undo"
        };
        buffer.set_stringn(
            inner.x,
            instruction_y,
            instruction,
            usize::from(inner.width),
            Style::default().fg(self.theme.text_secondary),
        );

        let rows = inner.height.saturating_sub(2);
        if rows == 0 {
            return;
        }
        let selected = ThemeId::ALL
            .iter()
            .position(|candidate| *candidate == self.selected)
            .unwrap_or_default();
        let visible = usize::from(rows);
        let first = selected
            .saturating_add(1)
            .saturating_sub(visible)
            .min(ThemeId::ALL.len().saturating_sub(visible));
        for (row, id) in ThemeId::ALL.iter().enumerate().skip(first).take(visible) {
            let y = inner
                .y
                .saturating_add(1)
                .saturating_add(u16::try_from(row.saturating_sub(first)).unwrap_or(u16::MAX));
            let selected_row = *id == self.selected;
            let style = if selected_row {
                let style = Style::default()
                    .fg(readable_text_on(self.theme, self.theme.surface_selection))
                    .bg(self.theme.surface_selection)
                    .add_modifier(Modifier::BOLD);
                if self.theme.surface_selection == self.theme.surface_base {
                    style.add_modifier(Modifier::REVERSED)
                } else {
                    style
                }
            } else {
                Style::default().fg(text)
            };
            buffer.set_stringn(inner.x, y, if selected_row { "> " } else { "  " }, 2, style);
            buffer.set_stringn(
                inner.x.saturating_add(2),
                y,
                id.label(),
                usize::from(inner.width.saturating_sub(2)),
                style,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Cell;

    use super::*;

    #[test]
    fn picker_keeps_the_selected_theme_and_restore_hint_visible() {
        let area = Rect::new(0, 0, 56, 24);
        let mut buffer = Buffer::empty(area);
        ThemePicker::new(
            ThemeId::TokyoNight,
            Theme::for_id(ThemeId::TokyoNight),
            false,
            ModalChrome::new(std::time::Duration::ZERO, false, false),
        )
        .render(area, &mut buffer);

        let text = buffer.content.iter().map(Cell::symbol).collect::<String>();
        assert!(text.contains("Tokyo Night"));
        assert!(text.contains("save"));
        assert!(text.contains("restore"));
        assert!(text.contains("THEME PREVIEW"));
    }

    #[test]
    fn narrow_ascii_picker_keeps_save_and_restore_actions_visible() {
        let area = Rect::new(0, 0, 32, 8);
        let mut buffer = Buffer::empty(area);
        ThemePicker::new(
            ThemeId::ExciseDark,
            Theme::for_id(ThemeId::ExciseDark),
            true,
            ModalChrome::new(std::time::Duration::ZERO, false, false),
        )
        .render(area, &mut buffer);

        let text = buffer.content.iter().map(Cell::symbol).collect::<String>();
        assert!(text.contains("[Up/Dn] Enter save Esc undo"));
    }
}
