use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};

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
        Paragraph::new(format!("{}\n\n[Esc] dismiss", self.error_message))
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
