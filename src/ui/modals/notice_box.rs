use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::theme::Theme;
use crate::ui::format::display_text;
use crate::ui::pane::{readable_text_on, render_modal};

pub struct NoticeBox<'a> {
    message: &'a str,
    theme: Theme,
    ascii: bool,
}

impl<'a> NoticeBox<'a> {
    pub const fn new(message: &'a str, theme: Theme, ascii: bool) -> Self {
        Self {
            message,
            theme,
            ascii,
        }
    }
}

impl Widget for NoticeBox<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let width = area.width.saturating_sub(4).clamp(30, 72).min(area.width);
        let height = area.height.saturating_sub(2).clamp(7, 9).min(area.height);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let inner = render_modal(
            buffer,
            rect,
            "COMPLETE",
            self.theme,
            self.theme.state_complete,
            self.ascii,
        );
        Paragraph::new(display_text(self.message))
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
    fn hostile_notice_text_is_escaped_and_marked() {
        let area = Rect::new(0, 0, 40, 9);
        let mut buffer = Buffer::empty(area);
        NoticeBox::new(
            "complete: bad\n\u{202e}name\u{1b}[31m",
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
