use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};

use crate::ui::format::display_text;

pub struct ErrorBox<'a> {
    error_message: &'a str,
}

impl<'a> ErrorBox<'a> {
    pub const fn new(error_message: &'a str) -> Self {
        Self { error_message }
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
        Clear.render(rect, buffer);
        let style = Style::default()
            .fg(Color::Red)
            .bg(Color::Black)
            .add_modifier(Modifier::BOLD);
        Paragraph::new(format!(
            "{}\n\n[Esc] dismiss",
            display_text(self.error_message)
        ))
        .style(style)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Double)
                .border_style(style)
                .title(" ! ERROR "),
        )
        .render(rect, buffer);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::widgets::Widget;

    use super::*;

    #[test]
    fn hostile_error_text_is_escaped_and_marked() {
        let area = Rect::new(0, 0, 40, 9);
        let mut buffer = Buffer::empty(area);
        ErrorBox::new("permission denied: bad\n\u{202e}name\u{1b}[31m").render(area, &mut buffer);
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
