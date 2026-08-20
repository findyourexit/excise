use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget};

pub struct HelpBox;

impl HelpBox {
    pub const fn new() -> Self {
        Self
    }
}

impl Widget for HelpBox {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let width = area.width.saturating_sub(4).clamp(32, 76).min(area.width);
        let height = area.height.saturating_sub(2).clamp(10, 20).min(area.height);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        Clear.render(rect, buffer);
        let style = Style::default().fg(Color::Cyan).bg(Color::Black);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(style)
            .title(" EXCISE COMMANDS ");
        let inner = block.inner(rect);
        block.render(rect, buffer);
        Paragraph::new(vec![
            Line::styled("Explore", style.add_modifier(Modifier::BOLD)),
            Line::from("  arrows                 move focus"),
            Line::from("  h/j/k/l                Vim preset movement"),
            Line::from("  Ctrl-b/n/p/f           Emacs preset movement"),
            Line::from("  Enter                  open or focused rescan"),
            Line::from("  Esc                    parent / cancel"),
            Line::from("  +  -  0                zoom in / out / reset"),
            Line::from("  /                      exact or glob filter"),
            Line::from(""),
            Line::styled("Safety", style.add_modifier(Modifier::BOLD)),
            Line::from("  Backspace              plan permanent deletion"),
            Line::from("  q / Ctrl-c             quit or interruption options"),
            Line::from("  synthetic aggregates   never directly deletable"),
            Line::from("  new/changed entries    skipped by identity plan"),
            Line::from(""),
            Line::from("[Esc/?/q] close help"),
        ])
        .style(style)
        .render(inner, buffer);
    }
}
