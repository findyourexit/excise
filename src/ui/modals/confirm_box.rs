use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
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
            "EXIT",
            self.theme,
            self.theme.focus,
            self.ascii,
        );
        let lines = if self.save_preferences {
            vec![
                Line::from(""),
                Line::from("Safe UI preferences changed this session."),
                Line::from("[s] save preferences and quit"),
                Line::from("[d] discard changes and quit"),
                Line::from("[Esc/q/n] back"),
            ]
        } else {
            vec![
                Line::from(""),
                Line::from("Quit Excise?"),
                Line::from("[y] quit    [Esc/q/n] back"),
            ]
        };
        Paragraph::new(lines)
            .style(Style::default().fg(readable_text_on(self.theme, self.theme.surface_raised)))
            .alignment(Alignment::Center)
            .render(inner, buffer);
    }
}
