use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::theme::Theme;
use crate::ui::pane::{readable_text_on, render_modal};

pub struct ConfirmBox {
    save_preferences: bool,
    theme: Theme,
    ascii: bool,
}

impl ConfirmBox {
    pub const fn new(save_preferences: bool, theme: Theme, ascii: bool) -> Self {
        Self {
            save_preferences,
            theme,
            ascii,
        }
    }
}

impl Widget for ConfirmBox {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let width = area.width.saturating_sub(4).clamp(30, 64).min(area.width);
        let height = if self.save_preferences { 10 } else { 8 }.min(area.height);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let inner = render_modal(
            buffer,
            rect,
            "QUIT",
            self.theme,
            self.theme.focus,
            self.ascii,
        );
        let lines = if self.save_preferences {
            vec![
                Line::from("Save interface preferences before quitting?"),
                Line::from(""),
                Line::styled(
                    "[s] Save and quit",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    "[d] Quit without saving",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Line::from("[Esc/q/n] Keep working"),
            ]
        } else {
            vec![
                Line::from("Quit Excise?"),
                Line::from(""),
                Line::styled("[y] Quit", Style::default().add_modifier(Modifier::BOLD)),
                Line::from("[Esc/q/n] Keep working"),
            ]
        };
        Paragraph::new(lines)
            .style(Style::default().fg(readable_text_on(self.theme, self.theme.surface_raised)))
            .alignment(Alignment::Center)
            .render(inner, buffer);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::widgets::Widget;

    use crate::theme::ThemeId;

    use super::*;

    fn rendered_quit_dialog(save_preferences: bool) -> String {
        let area = Rect::new(0, 0, 64, 10);
        let mut buffer = Buffer::empty(area);
        ConfirmBox::new(save_preferences, Theme::for_id(ThemeId::ExciseDark), false)
            .render(area, &mut buffer);
        buffer.content.iter().fold(String::new(), |mut text, cell| {
            text.push_str(cell.symbol());
            text
        })
    }

    #[test]
    fn quit_dialogs_show_only_the_actions_available_for_the_current_choice() {
        let save = rendered_quit_dialog(true);
        assert!(save.contains("QUIT"));
        assert!(save.contains("Save interface preferences before quitting?"));
        assert!(save.contains("[s] Save and quit"));
        assert!(save.contains("[d] Quit without saving"));
        assert!(save.contains("[Esc/q/n] Keep working"));

        let plain = rendered_quit_dialog(false);
        assert!(plain.contains("Quit Excise?"));
        assert!(plain.contains("[y] Quit"));
        assert!(!plain.contains("Save and quit"));
    }
}
