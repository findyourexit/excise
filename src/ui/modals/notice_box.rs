use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};

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
        Paragraph::new(self.message)
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
