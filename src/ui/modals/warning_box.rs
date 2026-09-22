use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::theme::Theme;
use crate::ui::pane::{ModalChrome, readable_text_on, render_modal};

pub struct WarningBox {
    theme: Theme,
    ascii: bool,
    chrome: ModalChrome,
}

impl WarningBox {
    pub(crate) const fn with_chrome(theme: Theme, ascii: bool, chrome: ModalChrome) -> Self {
        Self {
            theme,
            ascii,
            chrome,
        }
    }
}

impl Widget for WarningBox {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let width = area.width.saturating_sub(2).clamp(30, 72).min(area.width);
        let height = 9_u16.min(area.height);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width.min(area.width),
            height.min(area.height),
        );
        let inner = render_modal(
            buffer,
            rect,
            "SCAN IN PROGRESS",
            self.theme,
            self.theme.state_rebuilding,
            self.ascii,
            self.chrome,
        );
        Paragraph::new(vec![
            Line::from("Scanning is still in progress."),
            Line::from("Deletion is unavailable until it finishes."),
            Line::from(""),
            Line::styled(
                "[Any key] close",
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ])
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
    fn generation_rebuild_warning_explains_that_deletion_is_unavailable() {
        let area = Rect::new(0, 0, 48, 9);
        let mut buffer = Buffer::empty(area);
        WarningBox::with_chrome(
            Theme::for_id(ThemeId::ExciseDark),
            false,
            ModalChrome::new(std::time::Duration::ZERO, false, false),
        )
        .render(area, &mut buffer);
        let text = buffer.content.iter().fold(String::new(), |mut text, cell| {
            text.push_str(cell.symbol());
            text
        });

        assert!(text.contains("SCAN IN PROGRESS"));
        assert!(text.contains("Deletion is unavailable until it finishes."));
        assert!(text.contains("[Any key] close"));
    }
}
