use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;

pub struct TermTooSmall;

impl TermTooSmall {
    pub const fn new() -> Self {
        Self
    }
}

impl Widget for TermTooSmall {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let lines = [
            "EXCISE - terminal too small",
            "Resize to at least 32 x 8.",
            "Press q to quit.",
        ];
        let first_y = area.y + area.height.saturating_sub(lines.len() as u16) / 2;
        for (index, line) in lines.iter().enumerate() {
            if area.width < line.chars().count() as u16 {
                continue;
            }
            buffer.set_string(
                area.x + area.width.saturating_sub(line.chars().count() as u16) / 2,
                first_y.saturating_add(index as u16),
                line,
                Style::default().add_modifier(Modifier::BOLD),
            );
        }
    }
}
