use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::theme::Theme;
use crate::ui::format::display_text;
use crate::ui::pane::{readable_text_on, render_modal};

pub struct ErrorBox<'a> {
    error_message: &'a str,
    theme: Theme,
    ascii: bool,
}

impl<'a> ErrorBox<'a> {
    pub const fn new(error_message: &'a str, theme: Theme, ascii: bool) -> Self {
        Self {
            error_message,
            theme,
            ascii,
        }
    }
}

impl Widget for ErrorBox<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let width = area.width.saturating_sub(2).clamp(30, 72).min(area.width);
        let height = area.height.saturating_sub(2).clamp(7, 9).min(area.height);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width.min(area.width),
            height.min(area.height),
        );
        let inner = render_modal(
            buffer,
            rect,
            "! ERROR",
            self.theme,
            self.theme.text_danger,
            self.ascii,
        );
        Paragraph::new(format!(
            "{}\n\n[Esc] dismiss",
            display_text(self.error_message)
        ))
        .style(Style::default().fg(readable_text_on(self.theme, self.theme.surface_raised)))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .render(inner, buffer);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::widgets::Widget;

    use crate::theme::ThemeId;

    use super::*;

    #[test]
    fn hostile_error_text_is_escaped_and_marked() {
        let area = Rect::new(0, 0, 40, 9);
        let mut buffer = Buffer::empty(area);
        ErrorBox::new(
            "permission denied: bad\n\u{202e}name\u{1b}[31m",
            Theme::for_id(ThemeId::ExciseDark),
            false,
        )
        .render(area, &mut buffer);
        let text = buffer.content.iter().fold(String::new(), |mut text, cell| {
            text.push_str(cell.symbol());
            text
        });
        assert!(text.contains("[deceptive]"));
        assert!(text.contains("\\n"));
        assert!(text.contains("\\u{202e}"));
        assert!(text.contains("\\x1b"));
        assert!(!text.chars().any(char::is_control));
        assert!(!text.contains('\u{202e}'));
    }
}
