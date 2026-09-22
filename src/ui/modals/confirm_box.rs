use std::sync::atomic::Ordering;

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::app::ExitWork;
use crate::theme::Theme;
use crate::ui::pane::{ModalChrome, readable_text_on, render_modal};

pub struct ConfirmBox<'a> {
    save_preferences: bool,
    work: &'a ExitWork,
    theme: Theme,
    ascii: bool,
    chrome: ModalChrome,
}

impl<'a> ConfirmBox<'a> {
    pub(crate) const fn with_chrome(
        save_preferences: bool,
        work: &'a ExitWork,
        theme: Theme,
        ascii: bool,
        chrome: ModalChrome,
    ) -> Self {
        Self {
            save_preferences,
            work,
            theme,
            ascii,
            chrome,
        }
    }
}

impl Widget for ConfirmBox<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let width = area.width.saturating_sub(4).clamp(30, 68).min(area.width);
        let height = match self.work {
            ExitWork::Active { .. } | ExitWork::Stopping { .. } => 11,
            ExitWork::Pending { .. } | ExitWork::Cancelling { .. } => 9,
            ExitWork::None if self.save_preferences => 10,
            ExitWork::None => 8,
        }
        .min(area.height);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let inner = render_modal(
            buffer,
            rect,
            "QUIT",
            self.theme,
            self.theme.focus,
            self.ascii,
            self.chrome,
        );
        Paragraph::new(exit_lines(self.save_preferences, self.work))
            .style(Style::default().fg(readable_text_on(self.theme, self.theme.surface_raised)))
            .alignment(Alignment::Center)
            .render(inner, buffer);
    }
}

fn exit_lines(save_preferences: bool, work: &ExitWork) -> Vec<Line<'static>> {
    match work {
        ExitWork::None if save_preferences => vec![
            Line::from("Safe UI preferences changed this session."),
            Line::from("Save interface preferences before quitting?"),
            Line::from(""),
            Line::styled(
                "[s] Save and quit",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                "[d] Quit without saving",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::from("[Esc/q/n] Keep working"),
        ],
        ExitWork::None => vec![
            Line::from("Quit Excise?"),
            Line::from(""),
            Line::styled("[y] Quit", Style::default().add_modifier(Modifier::BOLD)),
            Line::from("[Esc/q/n] Keep working"),
        ],
        ExitWork::Pending { count } => vec![
            Line::from(format!("{count} deletion check(s) are waiting.")),
            Line::from("No filesystem mutation has started."),
            Line::from(""),
            Line::styled(
                "[c] Cancel checks and quit",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::from("[w/Esc/q/n] Keep working"),
        ],
        ExitWork::Cancelling { count } => vec![
            Line::from(format!("Cancelling {count} deletion check(s).")),
            Line::from("No filesystem mutation will start."),
            Line::from("Waiting for cancellation acknowledgement."),
        ],
        ExitWork::Active {
            planned_entries,
            completed,
            pending,
        } => vec![
            Line::from(format!(
                "{} of {planned_entries} items processed.",
                completed.load(Ordering::Relaxed)
            )),
            Line::from(format!(
                "{pending} additional deletion check(s) are waiting."
            )),
            Line::from("The active removal cannot be detached."),
            Line::styled(
                "[s] Stop after current item and quit",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::from("[w/Esc/q/n] Keep working"),
        ],
        ExitWork::Stopping {
            planned_entries,
            completed,
        } => vec![
            Line::from(format!(
                "{} of {planned_entries} items processed.",
                completed.load(Ordering::Relaxed)
            )),
            Line::from("Stopping after the current item."),
            Line::from("Waiting for the serial deletion worker to finish."),
        ],
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use super::*;

    fn text(lines: &[Line<'_>]) -> String {
        lines.iter().fold(String::new(), |mut text, line| {
            for span in &line.spans {
                text.push_str(span.content.as_ref());
            }
            text.push('\n');
            text
        })
    }

    #[test]
    fn active_exit_requires_an_explicit_safe_stop_or_wait() {
        let lines = exit_lines(
            false,
            &ExitWork::Active {
                planned_entries: 12,
                completed: Arc::new(AtomicU64::new(7)),
                pending: 2,
            },
        );
        let rendered = text(&lines);
        assert!(rendered.contains("7 of 12 items processed."));
        assert!(rendered.contains("The active removal cannot be detached."));
        assert!(rendered.contains("[s] Stop after current item and quit"));
        assert!(rendered.contains("[w/Esc/q/n] Keep working"));
    }

    #[test]
    fn pending_exit_can_cancel_without_claiming_mutation_started() {
        let rendered = text(&exit_lines(false, &ExitWork::Pending { count: 3 }));
        assert!(rendered.contains("3 deletion check(s) are waiting."));
        assert!(rendered.contains("No filesystem mutation has started."));
        assert!(rendered.contains("[c] Cancel checks and quit"));
    }
}
