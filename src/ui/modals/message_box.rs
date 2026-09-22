use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::deletion::{ConfirmationChallenge, DeletionPlan, DeletionReport, PlannedKind};
use crate::state::FileToDelete;
use crate::theme::Theme;
use crate::ui::format::{DisplaySize, display_path_end, display_text};
use crate::ui::pane::{readable_text_on, render_modal};

#[derive(Clone, Copy)]
pub enum DeletionView<'a> {
    Planning {
        target: &'a FileToDelete,
        enter_armed: bool,
        /// Whether Enter pre-arming is applicable for this target (i.e. the
        /// challenge will be single-key). False for directories that require a
        /// typed name and for entries with deceptive filenames (`TypePhrase`).
        armable: bool,
    },
    Confirm {
        plan: &'a DeletionPlan,
        input: &'a str,
        elevated: bool,
        reduced_guardrails: bool,
    },
    Deleting {
        planned_entries: u64,
        completed: u64,
        stopping: bool,
    },
    Cancel {
        planned_entries: u64,
    },
    Result(&'a DeletionReport),
}

pub struct MessageBox<'a> {
    view: DeletionView<'a>,
    theme: Theme,
    ascii: bool,
}

impl<'a> MessageBox<'a> {
    pub const fn planning(
        target: &'a FileToDelete,
        enter_armed: bool,
        armable: bool,
        theme: Theme,
        ascii: bool,
    ) -> Self {
        Self {
            view: DeletionView::Planning {
                target,
                enter_armed,
                armable,
            },
            theme,
            ascii,
        }
    }

    pub const fn confirm(
        plan: &'a DeletionPlan,
        input: &'a str,
        elevated: bool,
        reduced_guardrails: bool,
        theme: Theme,
        ascii: bool,
    ) -> Self {
        Self {
            view: DeletionView::Confirm {
                plan,
                input,
                elevated,
                reduced_guardrails,
            },
            theme,
            ascii,
        }
    }

    pub const fn deleting(
        planned_entries: u64,
        completed: u64,
        stopping: bool,
        theme: Theme,
        ascii: bool,
    ) -> Self {
        Self {
            view: DeletionView::Deleting {
                planned_entries,
                completed,
                stopping,
            },
            theme,
            ascii,
        }
    }

    pub const fn cancel(planned_entries: u64, theme: Theme, ascii: bool) -> Self {
        Self {
            view: DeletionView::Cancel { planned_entries },
            theme,
            ascii,
        }
    }

    pub const fn result(report: &'a DeletionReport, theme: Theme, ascii: bool) -> Self {
        Self {
            view: DeletionView::Result(report),
            theme,
            ascii,
        }
    }
}

impl Widget for MessageBox<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let width = area.width.saturating_sub(4).clamp(32, 78).min(area.width);
        let height = area.height.saturating_sub(2).clamp(8, 15).min(area.height);
        let message_rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let title = match &self.view {
            DeletionView::Planning { .. } => "PREPARING DELETE",
            DeletionView::Confirm { plan, .. } => match plan.root_snapshot().kind {
                PlannedKind::Directory => "DELETE FOLDER",
                PlannedKind::Link => "DELETE LINK",
                PlannedKind::File => "DELETE FILE",
            },
            DeletionView::Deleting { stopping: true, .. } => "STOPPING DELETE",
            DeletionView::Deleting { .. } => "DELETING",
            DeletionView::Cancel { .. } => "STOP DELETE?",
            DeletionView::Result(_) => "DELETION RESULTS",
        };
        let inner = render_modal(
            buf,
            message_rect,
            title,
            self.theme,
            self.theme.text_danger,
            self.ascii,
        );
        let text = readable_text_on(self.theme, self.theme.surface_raised);
        Paragraph::new(lines(self.view, inner.width, self.ascii))
            .style(Style::default().fg(text))
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: true })
            .render(inner, buf);
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "each deletion phase is kept in one exhaustive presentation match"
)]
fn lines(view: DeletionView<'_>, width: u16, ascii: bool) -> Vec<Line<'static>> {
    let separator = if ascii { "." } else { "·" };
    match view {
        DeletionView::Planning {
            target,
            enter_armed,
            armable,
        } => {
            let status_line = if enter_armed {
                "Deletion starts when checks finish."
            } else {
                "Checking the selected item before deletion."
            };
            let estimate_line = target.num_descendants.map(|count| {
                let item = if count == 1 { "item" } else { "items" };
                format!("About {count} {item} to check")
            });
            let action = if enter_armed {
                String::from("[Esc] stop and cancel")
            } else if armable {
                format!("[Enter] delete when ready {separator} [Esc] cancel")
            } else {
                String::from("[Esc] cancel")
            };
            let mut content = vec![
                Line::from(display_path_end(&target.full_path(), width)),
                Line::from(status_line),
            ];
            if let Some(estimate) = estimate_line {
                content.push(Line::from(estimate));
            }
            content.push(Line::styled(
                action,
                Style::default().add_modifier(Modifier::BOLD),
            ));
            content
        }
        DeletionView::Confirm {
            plan,
            input,
            elevated,
            reduced_guardrails,
        } => {
            let reduced_guardrails = reduced_guardrails
                || matches!(&plan.challenge, ConfirmationChallenge::ReducedGuard);
            let count = plan.planned_entries();
            let item = if count == 1 { "item" } else { "items" };
            let mut content = vec![
                Line::from(display_path_end(&plan.target.full_path(), width)),
                Line::from(format!(
                    "{count} {item} {separator} {} content",
                    DisplaySize(plan.apparent_bytes as f64)
                )),
                Line::from("Permanent deletion. New or changed items are skipped."),
            ];
            append_safety_labels(&mut content, reduced_guardrails, elevated, width, ascii);
            match &plan.challenge {
                ConfirmationChallenge::ConfirmFile | ConfirmationChallenge::ReducedGuard => {
                    content.push(Line::styled(
                        format!("[Enter/y] delete permanently {separator} [Esc/q] cancel"),
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                }
                ConfirmationChallenge::TypeName(expected) => {
                    content.push(Line::from(format!(
                        "Type this name exactly: {}",
                        display_text(expected)
                    )));
                    content.push(Line::from(format!("> {}_", display_text(input))));
                    content.push(Line::styled(
                        format!("[Enter] delete when exact {separator} [Esc/q] cancel"),
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                }
                ConfirmationChallenge::TypePhrase(expected) => {
                    content.push(Line::from(format!(
                        "Type this exactly: {}",
                        display_text(expected)
                    )));
                    content.push(Line::from(format!("> {}_", display_text(input))));
                    content.push(Line::styled(
                        format!("[Enter] delete when exact {separator} [Esc/q] cancel"),
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                }
            }
            content
        }
        DeletionView::Deleting {
            planned_entries,
            completed,
            stopping: false,
        } => vec![
            Line::from(format!("{completed} of {planned_entries} items processed.")),
            Line::from("Every item is checked again before removal."),
            Line::from("New or changed items are skipped."),
            Line::from(""),
            Line::styled(
                "[Esc/q] stop options",
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ],
        DeletionView::Deleting {
            planned_entries,
            completed,
            stopping: true,
        } => vec![
            Line::from(format!(
                "{completed} of {planned_entries} items processed; stopping after the current item."
            )),
            Line::from("No new items will start."),
            Line::from("Waiting for the current removal to finish."),
            Line::styled(
                "[h/Ctrl-C] stop now; final state may be unknown",
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ],
        DeletionView::Cancel { planned_entries } => vec![
            Line::from(format!("{planned_entries} items in this deletion.")),
            Line::styled(
                "[s] stop after current item; results stay precise",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                "[h/Ctrl-C] stop now; final state may be unknown",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                "[Esc/b] continue deletion",
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ],
        DeletionView::Result(report) => vec![
            Line::from(format!("Removed      {}", report.deleted_entries())),
            Line::from(format!("Changed      {}", report.changed_entries())),
            Line::from(format!("Missing      {}", report.missing_entries())),
            Line::from(format!("Failed       {}", report.failed_entries())),
            Line::from(format!("Not started  {}", report.unattempted_entries())),
            Line::styled(
                if !report.reporting_complete() {
                    "Results incomplete; rescan needed. [Enter/Esc/q] close"
                } else if report.precise {
                    "Finished. [Enter/Esc/q] close"
                } else {
                    "Final state unknown; rescan needed. [Enter/Esc/q] close"
                },
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ],
    }
}

fn append_safety_labels(
    content: &mut Vec<Line<'static>>,
    reduced_guardrails: bool,
    elevated: bool,
    width: u16,
    ascii: bool,
) {
    let separator = if ascii { "." } else { "·" };
    if reduced_guardrails && elevated && width < 56 {
        content.push(Line::styled(
            format!("ELEVATED {separator} REDUCED SAFEGUARDS ACTIVE"),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    } else {
        if reduced_guardrails {
            content.push(Line::styled(
                "REDUCED DELETE SAFEGUARDS ACTIVE",
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        if elevated {
            content.push(Line::styled(
                "ELEVATED PRIVILEGES ACTIVE",
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
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
    fn soft_stop_is_explicit_and_noninteractive() {
        let lines = lines(
            DeletionView::Deleting {
                planned_entries: 12,
                completed: 7,
                stopping: true,
            },
            78,
            false,
        );
        let text = text(&lines);
        assert!(text.contains("7 of 12 items processed; stopping after the current item."));
        assert!(text.contains("No new items will start."));
        assert!(text.contains("[h/Ctrl-C] stop now; final state may be unknown"));
        assert!(!text.contains("Esc"));
        assert!(text.contains("Ctrl-C"));
    }

    #[test]
    fn deletion_safety_alerts_stay_separate_and_ascii_safe() {
        let mut lines = Vec::new();
        append_safety_labels(&mut lines, true, true, 78, false);
        let expanded_text = text(&lines);
        assert!(expanded_text.contains("REDUCED DELETE SAFEGUARDS ACTIVE"));
        assert!(expanded_text.contains("ELEVATED PRIVILEGES ACTIVE"));

        let mut compact = Vec::new();
        append_safety_labels(&mut compact, true, true, 48, false);
        let compact = text(&compact);
        assert!(compact.contains("ELEVATED"));
        assert!(compact.contains("REDUCED SAFEGUARDS ACTIVE"));

        let mut ascii = Vec::new();
        append_safety_labels(&mut ascii, true, true, 48, true);
        let ascii = text(&ascii);
        assert!(ascii.contains("ELEVATED . REDUCED SAFEGUARDS ACTIVE"));
        assert!(!ascii.contains('·'));
    }

    #[test]
    fn deletion_stop_dialog_distinguishes_precise_and_immediate_choices() {
        let text = text(&lines(
            DeletionView::Cancel {
                planned_entries: 12,
            },
            78,
            false,
        ));
        assert!(text.contains("12 items in this deletion."));
        assert!(text.contains("[s] stop after current item; results stay precise"));
        assert!(text.contains("[h/Ctrl-C] stop now; final state may be unknown"));
        assert!(text.contains("[Esc/b] continue deletion"));
    }
}
