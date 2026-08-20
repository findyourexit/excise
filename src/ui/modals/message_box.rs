use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};

use crate::deletion::{ConfirmationChallenge, DeletionPlan, DeletionReport, PlannedKind};
use crate::state::FileToDelete;
use crate::ui::format::{DisplaySize, display_path};

#[derive(Clone, Copy)]
pub enum DeletionView<'a> {
    Planning(&'a FileToDelete),
    Confirm {
        plan: &'a DeletionPlan,
        input: &'a str,
        elevated: bool,
        reduced_guardrails: bool,
    },
    Deleting {
        planned_entries: u64,
        stopping: bool,
    },
    Cancel {
        planned_entries: u64,
    },
    Result(&'a DeletionReport),
}

pub struct MessageBox<'a> {
    view: DeletionView<'a>,
}

impl<'a> MessageBox<'a> {
    pub const fn planning(target: &'a FileToDelete) -> Self {
        Self {
            view: DeletionView::Planning(target),
        }
    }

    pub const fn confirm(
        plan: &'a DeletionPlan,
        input: &'a str,
        elevated: bool,
        reduced_guardrails: bool,
    ) -> Self {
        Self {
            view: DeletionView::Confirm {
                plan,
                input,
                elevated,
                reduced_guardrails,
            },
        }
    }

    pub const fn deleting(planned_entries: u64, stopping: bool) -> Self {
        Self {
            view: DeletionView::Deleting {
                planned_entries,
                stopping,
            },
        }
    }

    pub const fn cancel(planned_entries: u64) -> Self {
        Self {
            view: DeletionView::Cancel { planned_entries },
        }
    }

    pub const fn result(report: &'a DeletionReport) -> Self {
        Self {
            view: DeletionView::Result(report),
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
        let danger = Style::default()
            .fg(Color::Red)
            .bg(Color::Black)
            .add_modifier(Modifier::BOLD);
        let title = match &self.view {
            DeletionView::Planning(_) => " ! BUILDING IDENTITY PLAN ",
            DeletionView::Confirm { plan, .. } => {
                match plan.root_snapshot().map(|item| item.kind) {
                    Some(PlannedKind::Directory) => " ! PERMANENT DIRECTORY DELETION ",
                    Some(PlannedKind::Link) => " ! PERMANENT LINK DELETION ",
                    _ => " ! PERMANENT FILE DELETION ",
                }
            }
            DeletionView::Deleting { stopping: true, .. } => " ! STOPPING PERMANENT DELETION ",
            DeletionView::Deleting { .. } => " ! PERMANENT DELETION ACTIVE ",
            DeletionView::Cancel { .. } => " ! INTERRUPT DELETION ",
            DeletionView::Result(report) if report.precise => " ! DELETION RESULT · PRECISE ",
            DeletionView::Result(_) => " ! DELETION RESULT · UNKNOWN ",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Double)
            .border_style(danger)
            .title(Span::styled(title, danger));
        Clear.render(message_rect, buf);
        let inner = block.inner(message_rect);
        block.render(message_rect, buf);
        Paragraph::new(lines(self.view, inner.width))
            .style(danger)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: true })
            .render(inner, buf);
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "each deletion phase is kept in one exhaustive presentation match"
)]
fn lines(view: DeletionView<'_>, width: u16) -> Vec<Line<'static>> {
    match view {
        DeletionView::Planning(target) => vec![
            Line::from(""),
            Line::from(truncate(&display_path(&target.full_path()), width)),
            Line::from(""),
            Line::from("Enumerating a no-follow identity snapshot."),
            Line::from("Deletion is disabled until the plan is complete."),
            Line::from(""),
            Line::from("[Esc] cancel"),
        ],
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
                Line::from(truncate(&display_path(&plan.target.full_path()), width)),
                Line::from(truncate(&identity, width)),
                Line::from(format!(
                    "{} planned entries · {} logical",
                    plan.planned_entries(),
                    DisplaySize(plan.apparent_bytes as f64)
                )),
                Line::from(""),
                Line::from("This cannot be undone. New or changed entries are skipped."),
            ];
            append_safety_labels(&mut content, reduced_guardrails, elevated, width);
            match &plan.challenge {
                ConfirmationChallenge::ConfirmFile => {
                    content.push(Line::from("Press y to permanently delete this entry."));
                }
                ConfirmationChallenge::ReducedGuard => {
                    content.push(Line::from("Press y to arm permanent deletion."));
                }
                ConfirmationChallenge::TypeName(expected) => {
                    content.push(Line::from(format!("Type exact name: {expected}")));
                    content.push(Line::from(format!("> {input}_")));
                    content.push(Line::from("[Enter] arm when exact"));
                }
                ConfirmationChallenge::TypePhrase(expected) => {
                    content.push(Line::from(format!("Type: {expected}")));
                    content.push(Line::from(format!("> {input}_")));
                    content.push(Line::from("[Enter] arm when exact"));
                }
            }
            content.push(Line::from("[Esc] cancel"));
            content
        }
        DeletionView::Deleting {
            planned_entries,
            stopping: false,
        } => vec![
            Line::from(""),
            Line::from(format!("Executing {planned_entries} planned identities.")),
            Line::from("Every entry is revalidated immediately before mutation."),
            Line::from("New and changed entries are never swept."),
            Line::from(""),
            Line::from("[Esc/q] interruption options"),
        ],
        DeletionView::Deleting {
            planned_entries,
            stopping: true,
        } => vec![
            Line::from(""),
            Line::from(format!(
                "Stopping after current entry… {planned_entries} planned."
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
) {
    if reduced_guardrails && elevated && width < 56 {
        content.push(Line::from("ELEVATED · REDUCED GUARDRAILS ACTIVE"));
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
                stopping: true,
            },
            78,
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
        append_safety_labels(&mut lines, true, true, 78);
        let expanded_text = text(&lines);
        assert!(expanded_text.contains("REDUCED GUARDRAILS ACTIVE"));
        assert!(expanded_text.contains("ELEVATED PRIVILEGES ACTIVE"));
        let mut compact = Vec::new();
        append_safety_labels(&mut compact, true, true, 48);
        let compact = text(&compact);
        assert!(compact.contains("ELEVATED"));
        assert!(compact.contains("REDUCED GUARDRAILS ACTIVE"));
    }
}
