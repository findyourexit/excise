use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::deletion::ConfirmationChallenge;
use crate::state::FileToDelete;
use crate::state::tiles::FileType;
use crate::theme::Theme;
use crate::ui::format::{display_path_end, display_text};
use crate::ui::pane::{ModalChrome, readable_text_on, render_modal};

#[derive(Clone, Copy)]
pub(crate) struct DeletionSafety {
    pub(crate) elevated: bool,
    pub(crate) reduced_guardrails: bool,
}

/// The immediate foreground deletion decision. The planner, revalidation, and
/// filesystem mutation remain bounded background work after consent.
pub struct MessageBox<'a> {
    target: &'a FileToDelete,
    challenge: &'a ConfirmationChallenge,
    input: &'a str,
    safety: DeletionSafety,
    theme: Theme,
    ascii: bool,
    chrome: ModalChrome,
}

impl<'a> MessageBox<'a> {
    pub(crate) const fn with_chrome(
        target: &'a FileToDelete,
        challenge: &'a ConfirmationChallenge,
        input: &'a str,
        safety: DeletionSafety,
        theme: Theme,
        ascii: bool,
        chrome: ModalChrome,
    ) -> Self {
        Self {
            target,
            challenge,
            input,
            safety,
            theme,
            ascii,
            chrome,
        }
    }
}

impl Widget for MessageBox<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let width = area.width.saturating_sub(4).clamp(32, 78).min(area.width);
        let height = area.height.saturating_sub(2).clamp(8, 12).min(area.height);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let title = match self.target.file_type {
            FileType::Folder => "! DELETE FOLDER",
            FileType::File => "! DELETE FILE",
            FileType::Synthetic => "! DELETE ITEM",
        };
        let inner = render_modal(
            buffer,
            rect,
            title,
            self.theme,
            self.theme.text_danger,
            self.ascii,
            self.chrome,
        );
        Paragraph::new(confirmation_lines(
            self.target,
            self.challenge,
            self.input,
            self.safety.elevated,
            self.safety.reduced_guardrails,
            inner.width,
            self.ascii,
        ))
        .style(Style::default().fg(readable_text_on(self.theme, self.theme.surface_raised)))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .render(inner, buffer);
    }
}

fn confirmation_lines(
    target: &FileToDelete,
    challenge: &ConfirmationChallenge,
    input: &str,
    elevated: bool,
    reduced_guardrails: bool,
    width: u16,
    ascii: bool,
) -> Vec<Line<'static>> {
    let separator = if ascii { "." } else { "·" };
    let detail = match target.file_type {
        FileType::Folder => "Contents are checked before each removal.",
        FileType::File | FileType::Synthetic => "The live file is checked before removal.",
    };
    let mut lines = vec![
        Line::from(display_path_end(&target.full_path(), width)),
        Line::from("Deletion continues in the background."),
        Line::from(detail),
    ];
    append_safety_labels(&mut lines, reduced_guardrails, elevated, width, ascii);
    match challenge {
        ConfirmationChallenge::ConfirmFile | ConfirmationChallenge::ReducedGuard => {
            lines.push(Line::styled(
                format!("[Enter/y] start {separator} [Esc/n] cancel"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        ConfirmationChallenge::TypeName(expected) | ConfirmationChallenge::TypePhrase(expected) => {
            lines.push(Line::from(format!(
                "Type this exactly: {}",
                display_text(expected)
            )));
            lines.push(Line::from(format!("> {}_", display_text(input))));
            lines.push(Line::styled(
                format!("[Enter] start when exact {separator} [Esc/n] cancel"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
    }
    lines
}

fn append_safety_labels(
    lines: &mut Vec<Line<'static>>,
    reduced_guardrails: bool,
    elevated: bool,
    width: u16,
    ascii: bool,
) {
    let separator = if ascii { "." } else { "·" };
    if reduced_guardrails && elevated && width < 56 {
        lines.push(Line::styled(
            format!("ELEVATED {separator} REDUCED SAFEGUARDS ACTIVE"),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        return;
    }
    if reduced_guardrails {
        lines.push(Line::styled(
            "REDUCED DELETE SAFEGUARDS ACTIVE",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    if elevated {
        lines.push(Line::styled(
            "ELEVATED PRIVILEGES ACTIVE",
            Style::default().add_modifier(Modifier::BOLD),
        ));
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
    fn deletion_safety_alerts_stay_separate_and_ascii_safe() {
        let mut lines = Vec::new();
        append_safety_labels(&mut lines, true, true, 78, false);
        let expanded = text(&lines);
        assert!(expanded.contains("REDUCED DELETE SAFEGUARDS ACTIVE"));
        assert!(expanded.contains("ELEVATED PRIVILEGES ACTIVE"));

        let mut compact_lines = Vec::new();
        append_safety_labels(&mut compact_lines, true, true, 48, true);
        let compact = text(&compact_lines);
        assert!(compact.contains("ELEVATED . REDUCED SAFEGUARDS ACTIVE"));
        assert!(!compact.contains('·'));
    }
}
