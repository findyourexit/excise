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
    work: &'a ExitWork,
    mutation_started: Option<bool>,
    theme: Theme,
    ascii: bool,
    chrome: ModalChrome,
}

impl<'a> ConfirmBox<'a> {
    pub(crate) const fn with_chrome(
        work: &'a ExitWork,
        mutation_started: Option<bool>,
        theme: Theme,
        ascii: bool,
        chrome: ModalChrome,
    ) -> Self {
        Self {
            work,
            mutation_started,
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
        Paragraph::new(exit_lines(self.work, self.mutation_started))
            .style(Style::default().fg(readable_text_on(self.theme, self.theme.surface_raised)))
            .alignment(Alignment::Center)
            .render(inner, buffer);
    }
}

fn exit_lines(work: &ExitWork, mutation_started: Option<bool>) -> Vec<Line<'static>> {
    match work {
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
            progress, pending, ..
        } if !mutation_started.unwrap_or_else(|| progress.has_started_mutation()) => vec![
            Line::from("Final safety check."),
            Line::from(format!(
                "{pending} additional deletion check(s) are waiting."
            )),
            Line::from("No target removal has started."),
            Line::styled(
                "[s] Request safe stop and quit",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::from("[w/Esc/q/n] Keep working"),
        ],
        ExitWork::Active {
            planned_entries,
            progress,
            pending,
        } => vec![
            Line::from(format!(
                "{} of {planned_entries} items processed.",
                progress.completed().load(Ordering::Relaxed)
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
        ExitWork::Stopping { progress, .. }
            if !mutation_started.unwrap_or_else(|| progress.has_started_mutation()) =>
        {
            vec![
                Line::from("Stopping at the next safe boundary."),
                Line::from("Waiting for final safety check cancellation."),
            ]
        }
        ExitWork::Stopping {
            planned_entries,
            progress,
        } => vec![
            Line::from(format!(
                "{} of {planned_entries} items processed.",
                progress.completed().load(Ordering::Relaxed)
            )),
            Line::from("Stopping after the current item."),
            Line::from("Waiting for the serial deletion worker to finish."),
        ],
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use unicode_width::UnicodeWidthStr as _;

    use super::*;
    use crate::state::deletion_work::DeletionExecutionProgress;

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
        let progress = Arc::new(DeletionExecutionProgress::new());
        progress.begin_mutation();
        progress.completed().store(7, Ordering::Relaxed);
        let lines = exit_lines(
            &ExitWork::Active {
                planned_entries: 12,
                progress,
                pending: 2,
            },
            None,
        );
        let rendered = text(&lines);
        assert!(rendered.contains("7 of 12 items processed."));
        assert!(rendered.contains("The active removal cannot be detached."));
        assert!(rendered.contains("[s] Stop after current item and quit"));
        assert!(rendered.contains("[w/Esc/q/n] Keep working"));
    }

    #[test]
    fn verification_exit_uses_the_frame_phase_snapshot() {
        let progress = Arc::new(DeletionExecutionProgress::new());
        progress.begin_mutation();
        let active = text(&exit_lines(
            &ExitWork::Active {
                planned_entries: 12,
                progress: Arc::clone(&progress),
                pending: 2,
            },
            Some(false),
        ));
        let stopping = text(&exit_lines(
            &ExitWork::Stopping {
                planned_entries: 12,
                progress,
            },
            Some(false),
        ));

        assert!(active.contains("Final safety check."));
        assert!(active.contains("No target removal has started."));
        assert!(active.contains("Request safe stop"));
        assert!(!active.contains("items processed"));
        assert!(stopping.contains("Stopping at the next safe boundary."));
        assert!(!stopping.contains("items processed"));
    }

    #[test]
    fn verification_copy_fits_the_minimum_modal_inner_width() {
        let progress = Arc::new(DeletionExecutionProgress::new());
        let rendered = text(&exit_lines(
            &ExitWork::Active {
                planned_entries: 12,
                progress,
                pending: 2,
            },
            Some(false),
        ));

        assert!(
            rendered.lines().all(|line| line.width() <= 44),
            "verification copy must fit the 50-column terminal modal"
        );
    }

    #[test]
    fn pending_exit_can_cancel_without_claiming_mutation_started() {
        let rendered = text(&exit_lines(&ExitWork::Pending { count: 3 }, None));
        assert!(rendered.contains("3 deletion check(s) are waiting."));
        assert!(rendered.contains("No filesystem mutation has started."));
        assert!(rendered.contains("[c] Cancel checks and quit"));
    }
}
