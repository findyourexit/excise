use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::theme::Theme;
use crate::ui::pane::{readable_text_on, render_modal};

pub struct WarningBox {
    theme: Theme,
    ascii: bool,
}

impl WarningBox {
    pub const fn new(theme: Theme, ascii: bool) -> Self {
        Self { theme, ascii }
    }
}

impl Widget for WarningBox {
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
            "WARNING",
            self.theme,
            self.theme.state_aggregated,
            self.ascii,
        );
        Paragraph::new(
            "Deletion is locked until scanning or focused rescanning completes.\n\n[Any key] dismiss",
        )
        .style(Style::default().fg(readable_text_on(
            self.theme,
            self.theme.surface_raised,
        )))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .render(inner, buffer);
    }
}
