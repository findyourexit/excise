use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;

pub struct TermTooSmall {}

impl TermTooSmall {
    pub const fn new() -> Self {
        Self {}
    }
}

impl Widget for TermTooSmall {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let text = [
            "Terminal window is too small ¯\\_(ツ)_/¯",
            "Window too small ¯\\_(ツ)_/¯",
            "too small ¯\\_(ツ)_/¯",
            "¯\\_(ツ)_/¯",
            "!!!",
        ];
        for line in &text {
            if area.width >= line.chars().count() as u16 {
                buf.set_string(
                    u16::midpoint(area.x, area.width) - ((line.chars().count() / 2) as u16),
                    area.y + area.height / 2,
                    line,
                    Style::default().add_modifier(Modifier::BOLD),
                );
                break;
            }
        }
    }
}
