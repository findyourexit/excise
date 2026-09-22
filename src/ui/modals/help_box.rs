use ratatui::buffer::{Buffer, CellWidth as _};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::config::{CustomKeyBindings, KeyPreset};
use crate::theme::Theme;
use crate::ui::pane::{ModalChrome, readable_text_on, render_modal};

pub struct HelpBox<'a> {
    keymap: KeyPreset,
    custom_keys: Option<&'a CustomKeyBindings>,
    theme: Theme,
    ascii: bool,
    chrome: ModalChrome,
}

enum MovementKeyLabel {
    Space,
    Character(char),
}

impl std::fmt::Display for MovementKeyLabel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Space => formatter.write_str("Space"),
            Self::Character(key) => std::fmt::Display::fmt(key, formatter),
        }
    }
}

/// Names Space because it is the only supported custom movement key without a visible glyph.
const fn movement_key_label(key: char) -> MovementKeyLabel {
    match key {
        ' ' => MovementKeyLabel::Space,
        _ => MovementKeyLabel::Character(key),
    }
}

fn custom_movement_line(bindings: &CustomKeyBindings) -> String {
    format!(
        "  L:{} D:{} U:{} R:{}",
        movement_key_label(bindings.left),
        movement_key_label(bindings.down),
        movement_key_label(bindings.up),
        movement_key_label(bindings.right)
    )
}
/// The complete Help layout needs sixteen inner rows and fifty-five inner columns.
/// Its widest export row is fifty-five columns, so it must never be truncated.
const FULL_HELP_CONTENT_ROWS: u16 = 16;
const FULL_HELP_CONTENT_COLUMNS: u16 = 55;

impl<'a> HelpBox<'a> {
    pub(crate) const fn with_chrome(
        keymap: KeyPreset,
        custom_keys: Option<&'a CustomKeyBindings>,
        theme: Theme,
        ascii: bool,
        chrome: ModalChrome,
    ) -> Self {
        Self {
            keymap,
            custom_keys,
            theme,
            ascii,
            chrome,
        }
    }
}

impl Widget for HelpBox<'_> {
    #[allow(
        clippy::too_many_lines,
        reason = "the full and compact help layouts are kept together for row-by-row review"
    )]
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let width = area.width.saturating_sub(4).clamp(36, 76).min(area.width);
        let height = area.height.saturating_sub(2).clamp(10, 20).min(area.height);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let inner = render_modal(
            buffer,
            rect,
            "HELP",
            self.theme,
            self.theme.focus,
            self.ascii,
            self.chrome,
        );
        let full =
            inner.height >= FULL_HELP_CONTENT_ROWS && inner.width >= FULL_HELP_CONTENT_COLUMNS;
        let heading = Style::default()
            .fg(readable_text_on(self.theme, self.theme.surface_raised))
            .add_modifier(Modifier::BOLD);
        let content = if full {
            let movement = match (self.keymap, self.custom_keys) {
                (KeyPreset::Vim, _) => Line::from("  h/j/k/l                Vim movement"),
                (KeyPreset::Emacs, _) => Line::from("  Ctrl-b/n/p/f           Emacs movement"),
                (KeyPreset::Custom, Some(bindings)) => Line::from(custom_movement_line(bindings)),
                (KeyPreset::Custom, None) => Line::from("  arrows                 move selection"),
            };
            vec![
                Line::styled("Navigate", heading),
                Line::from("  arrows                 move selection"),
                movement,
                Line::from("  Enter                  open"),
                Line::from("  Esc                    back / cancel"),
                Line::from("  +  -  0 / PgUp PgDn   zoom / scan pages"),
                Line::from("  /                      filter items"),
                Line::from("  e                      export report"),
                Line::from("  t                      preview and save themes"),
                Line::styled("Delete safely", heading),
                Line::from("  Backspace              queue permanent deletion"),
                Line::from("  q / Ctrl-c             quit / interruption options"),
                Line::from("  virtual summaries      cannot be deleted"),
                Line::from("  changed/new entries    skipped before deletion"),
                Line::styled(
                    "[Esc/?/q] close help",
                    Style::default().fg(readable_text_on(self.theme, self.theme.surface_raised)),
                ),
            ]
        } else {
            // The minimum supported viewport has six inner rows. Put required
            // movement and safety guidance first, then append less urgent rows
            // until the available height is exhausted.
            let mut content = vec![Line::from("  arrows: move selection")];
            match (self.keymap, self.custom_keys) {
                (KeyPreset::Vim, _) => content.push(Line::from("  h/j/k/l: move selection")),
                (KeyPreset::Emacs, _) => {
                    content.push(Line::from("  Ctrl-b/n/p/f: move selection"));
                }
                (KeyPreset::Custom, Some(bindings)) => {
                    let movement = custom_movement_line(bindings);
                    if movement.cell_width() > inner.width {
                        content.push(Line::from(format!(
                            "  L:{} D:{}",
                            movement_key_label(bindings.left),
                            movement_key_label(bindings.down)
                        )));
                        content.push(Line::from(format!(
                            "  U:{} R:{}",
                            movement_key_label(bindings.up),
                            movement_key_label(bindings.right)
                        )));
                    } else {
                        content.push(Line::from(movement));
                    }
                }
                (KeyPreset::Custom, None) => {}
            }
            content.extend([
                Line::from("  Backspace: permanent delete"),
                Line::from("  q / Ctrl-c: quit / interrupt"),
                Line::styled(
                    "[Esc/?/q] close help",
                    Style::default().fg(readable_text_on(self.theme, self.theme.surface_raised)),
                ),
                Line::from("  virtual: cannot delete"),
                Line::from("  summary folder: open / delete"),
                Line::from("  changed/new: skipped"),
                Line::from("  Enter: open"),
                Line::from("  Esc: back / cancel"),
                Line::from("  +/-/0 zoom; PgUp/PgDn pages"),
                Line::from("  / filter; e scan; E history; t saves theme"),
            ]);
            content.truncate(usize::from(inner.height));
            content
        };
        Paragraph::new(content)
            .style(Style::default().fg(readable_text_on(self.theme, self.theme.surface_raised)))
            .render(inner, buffer);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;

    use crate::config::{CustomKeyBindings, KeyPreset};
    use crate::theme::ThemeId;

    use super::*;

    fn rendered_help_in(
        area: Rect,
        keymap: KeyPreset,
        custom_keys: Option<&CustomKeyBindings>,
    ) -> String {
        let mut buffer = Buffer::empty(area);
        HelpBox::with_chrome(
            keymap,
            custom_keys,
            Theme::for_id(ThemeId::ExciseDark),
            false,
            ModalChrome::new(std::time::Duration::ZERO, false, false),
        )
        .render(area, &mut buffer);
        buffer.content.iter().fold(String::new(), |mut text, cell| {
            text.push_str(cell.symbol());
            text
        })
    }

    fn rendered_help(keymap: KeyPreset, custom_keys: Option<&CustomKeyBindings>) -> String {
        rendered_help_in(Rect::new(0, 0, 80, 20), keymap, custom_keys)
    }

    #[test]
    fn custom_help_lists_bindings_and_existing_commands() {
        let bindings = CustomKeyBindings {
            left: 'a',
            down: 's',
            up: 'w',
            right: 'd',
        };
        let rendered = rendered_help(KeyPreset::Custom, Some(&bindings));

        assert!(rendered.contains("L:a D:s U:w R:d"));
        assert!(rendered.contains("HELP"));
        assert!(rendered.contains("Navigate"));
        assert!(rendered.contains("export report"));
        assert!(rendered.contains("preview and save themes"));
        assert!(rendered.contains("Delete safely"));
        assert!(rendered.contains("queue permanent deletion"));
        assert!(rendered.contains("virtual summaries"));
        assert!(rendered.contains("skipped before deletion"));
        assert!(rendered.contains("[Esc/?/q] close help"));
    }
    #[test]
    fn narrow_help_keeps_compact_safety_text_unclipped() {
        let rendered = rendered_help_in(Rect::new(0, 0, 32, 20), KeyPreset::Vim, None);

        assert!(rendered.contains("Backspace: permanent delete"));
        assert!(rendered.contains("q / Ctrl-c: quit / interrupt"));
        assert!(rendered.contains("virtual: cannot delete"));
        assert!(rendered.contains("changed/new: skipped"));
        assert!(rendered.contains("[Esc/?/q] close help"));
    }

    #[test]
    fn help_stays_compact_at_56_through_60_columns() {
        for width in 56..=60 {
            let rendered = rendered_help_in(Rect::new(0, 0, width, 20), KeyPreset::Vim, None);

            assert!(
                rendered.contains("Backspace: permanent delete"),
                "{width}-column help used the full safety layout too early: {rendered:?}"
            );
            assert!(
                rendered.contains("q / Ctrl-c: quit / interrupt"),
                "{width}-column help clipped the compact exit guidance: {rendered:?}"
            );
        }

        let rendered = rendered_help_in(Rect::new(0, 0, 61, 20), KeyPreset::Vim, None);
        assert!(rendered.contains("q / Ctrl-c             quit / interruption options"));
    }

    #[test]
    fn minimum_height_help_keeps_movement_safety_and_dismissal_guidance() {
        let rendered = rendered_help_in(Rect::new(0, 0, 32, 8), KeyPreset::Vim, None);

        assert!(rendered.contains("arrows: move selection"));
        assert!(rendered.contains("h/j/k/l: move selection"));
        assert!(rendered.contains("Backspace: permanent delete"));
        assert!(rendered.contains("q / Ctrl-c: quit / interrupt"));
        assert!(rendered.contains("[Esc/?/q] close help"));
    }

    #[test]
    fn short_custom_help_keeps_safety_guidance_on_one_row() {
        let bindings = CustomKeyBindings {
            left: 'a',
            down: 's',
            up: 'w',
            right: 'd',
        };
        let rendered = rendered_help_in(Rect::new(0, 0, 32, 8), KeyPreset::Custom, Some(&bindings));

        assert!(rendered.contains("L:a D:s U:w R:d"));
        assert!(rendered.contains("virtual: cannot delete"));
    }
    #[test]
    fn preset_help_lists_preset_movement_bindings() {
        assert!(rendered_help(KeyPreset::Vim, None).contains("h/j/k/l"));
        assert!(rendered_help(KeyPreset::Emacs, None).contains("Ctrl-b/n/p/f"));
    }

    #[test]
    fn custom_help_names_space_binding() {
        let bindings = CustomKeyBindings {
            left: ' ',
            down: 's',
            up: 'w',
            right: 'd',
        };

        assert!(rendered_help(KeyPreset::Custom, Some(&bindings)).contains("L:Space D:s U:w R:d"));
    }

    #[test]
    fn narrow_custom_help_keeps_the_complete_direction_mapping() {
        let bindings = CustomKeyBindings {
            left: ' ',
            down: ' ',
            up: ' ',
            right: ' ',
        };
        let rendered =
            rendered_help_in(Rect::new(0, 0, 36, 20), KeyPreset::Custom, Some(&bindings));

        assert!(
            rendered.contains("L:Space D:Space U:Space R:Space"),
            "narrow custom help dropped the direction mapping: {rendered:?}"
        );
    }
}
