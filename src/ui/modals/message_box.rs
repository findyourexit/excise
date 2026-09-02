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
            DeletionView::Planning { .. } => "! BUILDING IDENTITY PLAN",
            DeletionView::Confirm { plan, .. } => {
                match plan.root_snapshot().map(|item| item.kind) {
                    Some(PlannedKind::Directory) => "! PERMANENT DIRECTORY DELETION",
                    Some(PlannedKind::Link) => "! PERMANENT LINK DELETION",
                    _ => "! PERMANENT FILE DELETION",
                }
            }
            DeletionView::Deleting { stopping: true, .. } => "! STOPPING PERMANENT DELETION",
            DeletionView::Deleting { .. } => "! PERMANENT DELETION ACTIVE",
            DeletionView::Cancel { .. } => "! INTERRUPT DELETION",
            DeletionView::Result(report) if report.precise => {
                if self.ascii {
                    "! DELETION RESULT . PRECISE"
                } else {
                    "! DELETION RESULT · PRECISE"
                }
            }
            DeletionView::Result(_) => {
                if self.ascii {
                    "! DELETION RESULT . UNKNOWN"
                } else {
                    "! DELETION RESULT · UNKNOWN"
                }
            }
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
            .style(Style::default().fg(text).add_modifier(Modifier::BOLD))
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
                "Armed: will execute when plan is ready."
            } else {
                "Building identity plan."
            };
            // Show the in-memory entry count as a planning estimate when available.
            // For directories this is populated from the scanned subtree; for files
            // it is always 1. The tilde signals it is an estimate, not the final plan.
            let estimate_line = target.num_descendants.map(|n| {
                let label = if n == 1 { "entry" } else { "entries" };
                format!("~{n} {label} expected")
            });
            let key_line = if enter_armed {
                String::from("[Esc] disarm and cancel")
            } else if armable {
                format!("[Enter] arm for immediate execution {separator} [Esc] cancel")
            } else {
                String::from("[Esc] cancel")
            };
            let mut lines = vec![
                Line::from(""),
                Line::from(display_path_end(&target.full_path(), width)),
                Line::from(""),
                Line::from(status_line),
            ];
            if let Some(estimate) = estimate_line {
                lines.push(Line::from(estimate));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(key_line));
            lines
        }
        DeletionView::Confirm {
            plan,
            input,
            elevated,
            reduced_guardrails,
        } => {
            let reduced_guardrails = reduced_guardrails
                || matches!(&plan.challenge, ConfirmationChallenge::ReducedGuard);
            let snapshot = plan.root_snapshot();
            let identity = snapshot.map_or_else(
                || "identity unavailable".to_string(),
                |snapshot| format!("identity {:?}", snapshot.identity.file_id),
            );
            let mut content = vec![
                Line::from(""),
                Line::from(display_path_end(&plan.target.full_path(), width)),
                Line::from(truncate(&identity, width)),
                Line::from(format!(
                    "{} planned entries {separator} {} logical",
                    plan.planned_entries(),
                    DisplaySize(plan.apparent_bytes as f64)
                )),
                Line::from(""),
                Line::from("This cannot be undone. New or changed entries are skipped."),
            ];
            append_safety_labels(&mut content, reduced_guardrails, elevated, width, ascii);
            match &plan.challenge {
                ConfirmationChallenge::ConfirmFile => {
                    content.push(Line::from("[Enter] or y to permanently delete this entry."));
                }
                ConfirmationChallenge::ReducedGuard => {
                    content.push(Line::from("[Enter] or y to arm permanent deletion."));
                }
                ConfirmationChallenge::TypeName(expected) => {
                    content.push(Line::from(format!("Type exact name: {expected}")));
                    content.push(Line::from(format!("> {}_", display_text(input))));
                    content.push(Line::from("[Enter] arm when exact"));
                }
                ConfirmationChallenge::TypePhrase(expected) => {
                    content.push(Line::from(format!("Type: {expected}")));
                    content.push(Line::from(format!("> {}_", display_text(input))));
                    content.push(Line::from("[Enter] arm when exact"));
                }
            }
            content.push(Line::from("[Esc] cancel"));
            content
        }
        DeletionView::Deleting {
            planned_entries,
            completed,
            stopping: false,
        } => vec![
            Line::from(""),
            Line::from(format!(
                "{completed} of {planned_entries} {separator} revalidated before each mutation"
            )),
            Line::from("New and changed entries are never swept."),
            Line::from(""),
            Line::from("[Esc/q] interruption options"),
        ],
        DeletionView::Deleting {
            planned_entries,
            completed,
            stopping: true,
        } => vec![
            Line::from(""),
            Line::from(format!(
                "Stopping after current entry… {completed} of {planned_entries}."
            )),
            Line::from("No further entry will start."),
            Line::from("Waiting for the active identity mutation to finish."),
            Line::from("[h/Ctrl-C] hard cancel and return control immediately"),
        ],
        DeletionView::Cancel { planned_entries } => vec![
            Line::from(""),
            Line::from(format!("Deletion plan: {planned_entries} identities")),
            Line::from("[s] soft cancel between entries; result remains precise"),
            Line::from("[h/Ctrl-C] hard cancel; final filesystem state is unknown"),
            Line::from("[Esc/b] back; continue deletion"),
        ],
        DeletionView::Result(report) => vec![
            Line::from(""),
            Line::from(format!("deleted       {}", report.deleted_entries())),
            Line::from(format!("changed       {}", report.changed_entries())),
            Line::from(format!("missing       {}", report.missing_entries())),
            Line::from(format!("failed        {}", report.failed_entries())),
            Line::from(format!("unattempted   {}", report.unattempted_entries())),
            Line::from(""),
            Line::from(if report.precise {
                "Result is precise. [Enter/Esc] close"
            } else {
                "Result is unknown; rescan required. [Enter/Esc] close"
            }),
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
        content.push(Line::from(format!(
            "ELEVATED {separator} REDUCED GUARDRAILS ACTIVE"
        )));
    } else {
        if reduced_guardrails {
            content.push(Line::from("SESSION-ONLY REDUCED GUARDRAILS ACTIVE"));
        }
        if elevated {
            content.push(Line::from("ELEVATED PRIVILEGES ACTIVE"));
        }
    }
}

fn truncate(value: &str, width: u16) -> String {
    let maximum = usize::from(width.saturating_sub(1));
    if value.chars().count() <= maximum {
        value.to_string()
    } else if maximum > 1 {
        let mut text = value.chars().take(maximum - 1).collect::<String>();
        text.push('…');
        text
    } else {
        String::new()
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
        assert!(text.contains("Stopping after current entry…"));
        assert!(text.contains("No further entry will start."));
        assert!(text.contains("hard cancel"));
        assert!(!text.contains("Esc"));
        assert!(text.contains("Ctrl-C"));
    }

    #[test]
    fn confirmation_safety_labels_are_independent() {
        let mut lines = Vec::new();
        append_safety_labels(&mut lines, true, true, 78, false);
        let expanded_text = text(&lines);
        assert!(expanded_text.contains("REDUCED GUARDRAILS ACTIVE"));
        assert!(expanded_text.contains("ELEVATED PRIVILEGES ACTIVE"));
        let mut compact = Vec::new();
        append_safety_labels(&mut compact, true, true, 48, false);
        let compact = text(&compact);
        assert!(compact.contains("ELEVATED"));
        assert!(compact.contains("REDUCED GUARDRAILS ACTIVE"));
    }
}
