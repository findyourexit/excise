use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};

use crate::ui::format::display_text;

pub struct NoticeBox<'a> {
    message: &'a str,
}

impl<'a> NoticeBox<'a> {
    pub const fn new(message: &'a str) -> Self {
        Self { message }
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
        Clear.render(rect, buffer);
        let style = Style::default()
            .fg(Color::Green)
            .bg(Color::Black)
            .add_modifier(Modifier::BOLD);
        Paragraph::new(display_text(self.message))
            .style(style)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Double)
                    .border_style(style)
                    .title(" COMPLETE "),
            )
            .render(rect, buffer);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::widgets::Widget;

    use super::*;

    #[test]
    fn hostile_notice_text_is_escaped_and_marked() {
        let area = Rect::new(0, 0, 40, 9);
        let mut buffer = Buffer::empty(area);
        NoticeBox::new("complete: bad\n\u{202e}name\u{1b}[31m").render(area, &mut buffer);
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
