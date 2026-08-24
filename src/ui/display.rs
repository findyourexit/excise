use std::path::Path;
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthStr as _;

use crate::UiMode;
use crate::animation::AnimationScheduler;
use crate::config::KeyPreset;
use crate::error::AppError;
use crate::model::{ByteBounds, NodeKind, NodeState, SyntheticKind, UnscannedReason};
use crate::native_path::SafeDisplayPath;
use crate::os::is_user_admin;
use crate::state::UiEffects;
use crate::state::files::FileTree;
use crate::state::tiles::{Board, FileType};
use crate::theme::Theme;
use crate::ui::TermTooSmall;
use crate::ui::format::{
    DECEPTIVE_DISPLAY_MARKER, DisplaySize, display_os_str_middle, display_path_info,
    display_path_middle, display_text, display_text_info, truncate_marked, truncate_middle,
};
use crate::ui::grid::RectangleGrid;
use crate::ui::modals::{ConfirmBox, ErrorBox, HelpBox, MessageBox, NoticeBox, WarningBox};

pub struct Display<B>
where
    B: Backend,
{
    terminal: Terminal<B>,
}

impl<B> Display<B>
where
    B: Backend,
{
    /// # Errors
    /// Returns a terminal error if initialization, clearing, or cursor setup fails.
    pub fn new(terminal_backend: B) -> Result<Self, AppError> {
        let mut terminal = Terminal::new(terminal_backend)
            .map_err(|error| AppError::terminal("initialization", error))?;
        terminal
            .backend_mut()
            .clear()
            .map_err(|error| AppError::terminal("clear", error))?;
        terminal
            .hide_cursor()
            .map_err(|error| AppError::terminal("cursor hide", error))?;
        Ok(Self { terminal })
    }
    /// # Errors
    /// Returns a terminal error if the terminal size cannot be read.
    pub fn size(&self) -> Result<Rect, AppError> {
        let size = self
            .terminal
            .size()
            .map_err(|error| AppError::terminal("size query", error))?;
        Ok(Rect::new(0, 0, size.width, size.height))
    }
    /// Renders the application UI based on the current mode
    ///
    /// # Errors
    /// Returns a terminal error if drawing fails.
    #[allow(
        clippy::fn_params_excessive_bools,
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "rendering needs the complete runtime presentation state in one atomic frame"
    )]
    pub fn render(
        &mut self,
        file_tree: &FileTree,
        board: &mut Board,
        ui_mode: &UiMode,
        ui_effects: &UiEffects,
        animation: &mut AnimationScheduler,
        now: Duration,
        theme_name: &str,
        theme: Theme,
        ascii: bool,
        monochrome: bool,
        keymap: KeyPreset,
        mouse_enabled: bool,
        reduced_guardrails: bool,
        reduced_motion: bool,
    ) -> Result<(), AppError> {
        self.terminal
            .draw(|frame| {
                let full_screen = frame.area();
                let elevated = is_user_admin();
                if matches!(ui_mode, UiMode::ScreenTooSmall) {
                    frame.render_widget(TermTooSmall::new(), full_screen);
                    render_safety_banner(
                        frame.buffer_mut(),
                        full_screen,
                        theme,
                        reduced_guardrails,
                        elevated,
                    );
                } else {
                    let shell = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            Constraint::Length(3),
                            Constraint::Min(5),
                            Constraint::Length(2),
                        ])
                        .split(full_screen);
                    render_instrument_header(
                        frame.buffer_mut(),
                        shell[0],
                        file_tree,
                        ui_mode,
                        theme,
                        ascii,
                    );

                    let (workspace_area, inspector_area) = body_areas(
                        shell[1],
                        matches!(ui_mode, UiMode::Normal) && board.currently_selected().is_some(),
                    );
                    let workspace_block = Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Plain)
                        .border_style(Style::default().fg(theme.border))
                        .title(Span::styled(
                            if full_screen.width < 72 {
                                " LIST "
                            } else {
                                " STORAGE MAP "
                            },
                            Style::default()
                                .fg(theme.text_secondary)
                                .add_modifier(Modifier::BOLD),
                        ));
                    let workspace = workspace_block.inner(workspace_area);
                    frame.render_widget(workspace_block, workspace_area);
                    board.change_area(workspace);
                    board.advance_geometry(now, reduced_motion);
                    if board.is_list_layout() {
                        render_list(frame.buffer_mut(), workspace, board, theme, ascii);
                    } else {
                        frame.render_widget(
                            RectangleGrid::new(
                                board.rendered_tiles(),
                                board.unrenderable_tile_coordinates,
                                board.selected_index,
                                theme,
                            ),
                            workspace,
                        );
                    }
                    if let Some(inspector_area) = inspector_area {
                        render_inspector(
                            frame.buffer_mut(),
                            inspector_area,
                            file_tree,
                            board,
                            theme,
                            ascii,
                        );
                    }
                    render_status(
                        frame.buffer_mut(),
                        shell[2],
                        file_tree,
                        board,
                        ui_mode,
                        ui_effects,
                        theme,
                        theme_name,
                        keymap,
                        mouse_enabled,
                        reduced_guardrails,
                        elevated,
                        reduced_motion,
                        ascii,
                    );

                    match ui_mode {
                        UiMode::PlanningDeletion(target) => {
                            frame.render_widget(MessageBox::planning(target), full_screen);
                        }
                        UiMode::DeleteConfirm {
                            plan: Some(plan),
                            input,
                        } => {
                            frame.render_widget(
                                MessageBox::confirm(plan, input, elevated, reduced_guardrails),
                                full_screen,
                            );
                        }
                        UiMode::Deleting {
                            planned_entries,
                            stopping,
                        } => {
                            frame.render_widget(
                                MessageBox::deleting(*planned_entries, *stopping),
                                full_screen,
                            );
                        }
                        UiMode::DeletionCancel { planned_entries } => {
                            frame.render_widget(MessageBox::cancel(*planned_entries), full_screen);
                        }
                        UiMode::DeletionResult { report } => {
                            frame.render_widget(MessageBox::result(report), full_screen);
                        }
                        UiMode::ErrorMessage(message) => {
                            frame.render_widget(ErrorBox::new(message), full_screen);
                        }
                        UiMode::Notice(message) => {
                            frame.render_widget(NoticeBox::new(message), full_screen);
                        }
                        UiMode::Exiting { save_preferences } => {
                            frame.render_widget(ConfirmBox::new(*save_preferences), full_screen);
                        }
                        UiMode::WarningMessage => {
                            frame.render_widget(WarningBox::new(), full_screen);
                        }
                        UiMode::Help => {
                            frame.render_widget(HelpBox::new(), full_screen);
                        }
                        UiMode::Loading
                        | UiMode::Normal
                        | UiMode::Rescanning { .. }
                        | UiMode::FilterInput { .. }
                        | UiMode::ScreenTooSmall
                        | UiMode::DeleteConfirm { plan: None, .. } => {}
                    }
                }
                Self::apply_theme(frame.buffer_mut(), theme);
                animation.process(now, frame.buffer_mut(), full_screen);
                if ascii {
                    Self::apply_ascii(frame.buffer_mut());
                }
                if monochrome {
                    apply_monochrome(frame.buffer_mut(), theme.surface_base);
                }
            })
            .map_err(|error| AppError::terminal("draw", error))?;
        Ok(())
    }

    /// # Errors
    /// Returns a terminal error if clearing or cursor restoration fails.
    pub fn clear(&mut self) -> Result<(), AppError> {
        self.terminal
            .backend_mut()
            .clear()
            .map_err(|error| AppError::terminal("clear", error))?;
        self.terminal
            .show_cursor()
            .map_err(|error| AppError::terminal("cursor show", error))
    }
    fn apply_theme(buffer: &mut Buffer, theme: Theme) {
        for cell in &mut buffer.content {
            cell.fg = match cell.fg {
                Color::Reset | Color::White => theme.text_primary,
                Color::Black => theme.text_inverse,
                Color::Gray | Color::DarkGray => theme.text_muted,
                Color::Blue | Color::Cyan => theme.focus,
                Color::Green => theme.state_complete,
                Color::Yellow => theme.state_aggregated,
                Color::Magenta => theme.state_shared,
                Color::Red | Color::LightRed => theme.text_danger,
                color => color,
            };
            cell.bg = match cell.bg {
                Color::Reset => theme.surface_base,
                Color::Black | Color::Yellow | Color::Magenta => theme.surface_raised,
                Color::White => theme.surface_panel,
                Color::Gray | Color::DarkGray | Color::Blue | Color::Cyan => {
                    theme.surface_selection
                }
                Color::Red | Color::LightRed => theme.surface_danger,
                color => color,
            };
        }
    }

    fn apply_ascii(buffer: &mut Buffer) {
        for cell in &mut buffer.content {
            let replacement = match cell.symbol() {
                "─" | "═" => Some("-"),
                "│" | "║" => Some("|"),
                "┌" | "┐" | "└" | "┘" | "├" | "┤" | "┬" | "┴" | "┼" | "╔" | "╗" | "╚" | "╝"
                | "╠" | "╣" | "╦" | "╩" | "╬" => Some("+"),
                "█" => Some("#"),
                "◆" => Some("C"),
                "◇" => Some("A"),
                "◫" => Some("S"),
                "·" => Some("."),
                "◌" => Some("~"),
                "≥" => Some(">"),
                _ => None,
            };
            if let Some(replacement) = replacement {
                cell.set_symbol(replacement);
            }
        }
    }
}
const COMPACT_INSPECTOR_HEIGHT: u16 = 9;
const MINIMUM_WORKSPACE_HEIGHT: u16 = 5;

fn body_areas(area: Rect, has_selection: bool) -> (Rect, Option<Rect>) {
    if area.width >= 120 {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(48), Constraint::Length(36)])
            .split(area);
        return (body[0], Some(body[1]));
    }
    if area.width >= 32
        && has_selection
        && area.height >= MINIMUM_WORKSPACE_HEIGHT + COMPACT_INSPECTOR_HEIGHT
    {
        let inspector = Rect::new(
            area.x,
            area.y + area.height - COMPACT_INSPECTOR_HEIGHT,
            area.width,
            COMPACT_INSPECTOR_HEIGHT,
        );
        return (area, Some(inspector));
    }
    (area, None)
}

fn render_safety_banner(
    buffer: &mut Buffer,
    area: Rect,
    theme: Theme,
    reduced_guardrails: bool,
    elevated: bool,
) {
    let Some(label) = safety_label(reduced_guardrails, elevated, area.width) else {
        return;
    };
    let line = Rect::new(
        area.x,
        area.y.saturating_add(area.height.saturating_sub(1)),
        area.width,
        area.height.min(1),
    );
    Paragraph::new(label)
        .style(
            Style::default()
                .fg(theme.text_danger)
                .add_modifier(Modifier::BOLD),
        )
        .render(line, buffer);
}

fn render_instrument_header(
    buffer: &mut Buffer,
    area: Rect,
    file_tree: &FileTree,
    ui_mode: &UiMode,
    theme: Theme,
    ascii: bool,
) {
    let current = file_tree.current_node();
    let total = file_tree.total_node();
    let (marker, state, state_color) = view_state(ui_mode, current.state, ascii, theme);
    let path = display_path_middle(&file_tree.get_current_path(), area.width.saturating_sub(30));
    let title = Line::from(vec![
        Span::styled(
            " EXCISE ",
            Style::default()
                .fg(theme.text_inverse)
                .bg(theme.focus)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {path} "), Style::default().fg(theme.text_primary)),
        Span::styled(
            format!(" {marker} {state} "),
            Style::default()
                .fg(state_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let metrics = total.metrics;
    let detail = Line::from(vec![
        Span::styled(" allocated ", Style::default().fg(theme.text_muted)),
        Span::styled(
            format_bounds(metrics.allocated_bytes),
            Style::default().fg(theme.text_primary),
        ),
        Span::styled("   reclaim ", Style::default().fg(theme.text_muted)),
        Span::styled(
            format_bounds(metrics.reclaimable_bytes),
            Style::default().fg(theme.text_primary),
        ),
        Span::styled("   apparent ", Style::default().fg(theme.text_muted)),
        Span::styled(
            format!("{}", DisplaySize(metrics.apparent_bytes as f64)),
            Style::default().fg(theme.text_secondary),
        ),
        Span::styled(
            format!("   {} entries ", metrics.descendants),
            Style::default().fg(theme.text_muted),
        ),
    ]);
    Paragraph::new(vec![title, detail])
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(theme.border)),
        )
        .render(area, buffer);
}

fn view_state(
    ui_mode: &UiMode,
    node_state: NodeState,
    ascii: bool,
    theme: Theme,
) -> (&'static str, &'static str, Color) {
    if matches!(ui_mode, UiMode::Rescanning { .. }) {
        return (
            if ascii { "~" } else { "◌" },
            "RESCANNING",
            theme.state_rescanning,
        );
    }
    if matches!(ui_mode, UiMode::Loading) {
        return (
            if ascii { "~" } else { "◌" },
            "SCANNING",
            theme.state_scanning,
        );
    }
    match node_state {
        NodeState::Scanning => (
            if ascii { "~" } else { "◌" },
            "SCANNING",
            theme.state_scanning,
        ),
        NodeState::Complete => (
            if ascii { "C" } else { "◆" },
            "COMPLETE",
            theme.state_complete,
        ),
        NodeState::Aggregated => (
            if ascii { "A" } else { "◇" },
            "AGGREGATED",
            theme.state_aggregated,
        ),
        NodeState::Uncertain => ("?", "UNCERTAIN", theme.state_uncertain),
    }
}

fn render_list(buffer: &mut Buffer, area: Rect, board: &Board, theme: Theme, ascii: bool) {
    for (index, tile) in board.tiles.iter().enumerate() {
        if index >= usize::from(area.height) {
            break;
        }
        let marker = match (tile.file_type, tile.synthetic_kind) {
            (FileType::Folder, _) => {
                if ascii {
                    ">"
                } else {
                    "▸"
                }
            }
            (FileType::Synthetic, Some(SyntheticKind::Shared)) => {
                if ascii {
                    "S"
                } else {
                    "◫"
                }
            }
            (FileType::Synthetic, _) => {
                if ascii {
                    "A"
                } else {
                    "◇"
                }
            }
            (FileType::File, _) if tile.uncertain => "?",
            _ => " ",
        };
        let name_width = area.width.saturating_sub(28);
        let name = display_os_str_middle(&tile.name, name_width);
        let size = if tile.uncertain && tile.size == 0 {
            "unknown".to_string()
        } else if tile.uncertain {
            format!(">={}", DisplaySize(tile.size as f64))
        } else {
            format!("{}", DisplaySize(tile.size as f64))
        };
        let line = format!(
            " {marker} {name:<name_width$} {size:>10} {:>6.1}%",
            tile.percentage * 100.0,
            name_width = usize::from(name_width)
        );
        let selected = board.selected_index == Some(index);
        let style = if selected {
            Style::default()
                .fg(theme.text_inverse)
                .bg(theme.surface_selection)
                .add_modifier(if theme.surface_selection == theme.text_inverse {
                    Modifier::BOLD | Modifier::REVERSED
                } else {
                    Modifier::BOLD
                })
        } else if tile.uncertain {
            Style::default().fg(theme.state_uncertain)
        } else {
            Style::default().fg(theme.text_primary)
        };
        buffer.set_stringn(
            area.x,
            area.y.saturating_add(index as u16),
            line,
            usize::from(area.width),
            style,
        );
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the inspector keeps wide and narrow information-equivalent layouts together"
)]
fn render_inspector(
    buffer: &mut Buffer,
    area: Rect,
    file_tree: &FileTree,
    board: &Board,
    theme: Theme,
    ascii: bool,
) {
    Clear.render(area, buffer);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " INSPECT ",
            Style::default()
                .fg(theme.text_secondary)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    block.render(area, buffer);
    let Some(tile) = board.currently_selected() else {
        Paragraph::new("Select an entry for identity, bounds, state, and actions.")
            .style(Style::default().fg(theme.text_muted))
            .wrap(Wrap { trim: true })
            .render(inner, buffer);
        return;
    };
    let Some(node) = file_tree.node(tile.node_id) else {
        return;
    };
    let (marker, state, state_color) = view_state(&UiMode::Normal, node.state, ascii, theme);
    let kind = match node.kind {
        NodeKind::Root => "root",
        NodeKind::Directory => "directory",
        NodeKind::File => "file",
        NodeKind::Link => "link",
        NodeKind::Synthetic(SyntheticKind::Other) => "Other aggregate",
        NodeKind::Synthetic(SyntheticKind::Shared) => "Shared allocation",
        NodeKind::Synthetic(SyntheticKind::Aggregate) => "cold aggregate",
    };
    let identity = node.snapshot.identity.as_ref().map_or_else(
        || "identity  —".to_string(),
        |identity| format!("identity  {:?}", identity.file_id),
    );
    let link_detail = node.snapshot.identity.as_ref().map_or_else(
        || "links     —".to_string(),
        |identity| {
            identity.link_count.map_or_else(
                || "links     unknown".to_string(),
                |count| format!("links     {count}"),
            )
        },
    );
    let reason = node.unscanned_reason.as_ref().map_or_else(
        || display_text_info("scope     materialized"),
        |reason| {
            let mut displayed = display_text_info(&format!("scope     {reason:?}"));
            let deceptive = match reason {
                UnscannedReason::Excluded(value)
                | UnscannedReason::Metadata(value)
                | UnscannedReason::Replacement(value) => {
                    let displayed = display_text_info(value);
                    displayed.deceptive || value.contains(DECEPTIVE_DISPLAY_MARKER)
                }
                UnscannedReason::SymbolicLink
                | UnscannedReason::FilesystemBoundary
                | UnscannedReason::MemoryAggregation => false,
            };
            displayed.deceptive |= deceptive;
            displayed
        },
    );
    let reason_detail = SafeDisplayPath {
        text: format!("{link_detail} · {}", reason.text),
        deceptive: reason.deceptive,
    };
    let action = if node.kind.is_synthetic() {
        "Enter focused rescan · deletion unavailable"
    } else if node.state == NodeState::Complete {
        "Enter open · Backspace permanent delete"
    } else {
        "Incomplete scope · deletion unavailable"
    };
    let name_line = Line::styled(
        display_os_str_middle(&node.name, inner.width),
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );
    let state_line = Line::from(vec![
        Span::styled(
            format!("{marker} {state}"),
            Style::default().fg(state_color),
        ),
        Span::styled(
            format!(" · {kind}"),
            Style::default().fg(theme.text_secondary),
        ),
    ]);
    let narrow_state_line = Line::styled(
        truncate_middle(&format!("{marker} {state} · {kind}"), inner.width),
        Style::default().fg(state_color),
    );
    let details = if inner.width < 54 {
        vec![
            name_line,
            narrow_state_line,
            Line::from(format!(
                "allocated {}",
                format_bounds(node.metrics.allocated_bytes)
            )),
            Line::from(format!(
                "reclaim   {}",
                format_bounds(node.metrics.reclaimable_bytes)
            )),
            Line::from(format!(
                "apparent {} · entries {}",
                DisplaySize(node.metrics.apparent_bytes as f64),
                node.metrics.descendants
            )),
            Line::from(truncate_middle(&identity, inner.width)),
            Line::from(truncate_marked(
                &reason_detail,
                inner.width,
                truncate_middle,
            )),
        ]
    } else if inner.height < 12 {
        vec![
            name_line,
            state_line,
            Line::from(format!(
                "allocated {} · reclaim {}",
                format_bounds(node.metrics.allocated_bytes),
                format_bounds(node.metrics.reclaimable_bytes)
            )),
            Line::from(format!(
                "apparent {} · entries {}",
                DisplaySize(node.metrics.apparent_bytes as f64),
                node.metrics.descendants
            )),
            Line::from(truncate_middle(&identity, inner.width)),
            Line::from(truncate_marked(
                &reason_detail,
                inner.width,
                truncate_middle,
            )),
            Line::styled(action, Style::default().fg(theme.text_muted)),
        ]
    } else {
        vec![
            name_line,
            state_line,
            Line::from(""),
            Line::from(format!(
                "allocated {}",
                format_bounds(node.metrics.allocated_bytes)
            )),
            Line::from(format!(
                "reclaim   {}",
                format_bounds(node.metrics.reclaimable_bytes)
            )),
            Line::from(format!(
                "apparent  {}",
                DisplaySize(node.metrics.apparent_bytes as f64)
            )),
            Line::from(format!("entries   {}", node.metrics.descendants)),
            Line::from(truncate_middle(&identity, inner.width)),
            Line::from(link_detail),
            Line::from(truncate_marked(&reason, inner.width, truncate_middle)),
            Line::from(""),
            Line::styled(action, Style::default().fg(theme.text_muted)),
        ]
    };
    Paragraph::new(details)
        .style(Style::default().fg(theme.text_primary))
        .wrap(Wrap { trim: true })
        .render(inner, buffer);
}

#[allow(
    clippy::fn_params_excessive_bools,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "status composition must evaluate all runtime and safety flags without precedence loss"
)]
fn render_status(
    buffer: &mut Buffer,
    area: Rect,
    file_tree: &FileTree,
    board: &Board,
    ui_mode: &UiMode,
    ui_effects: &UiEffects,
    theme: Theme,
    theme_name: &str,
    keymap: KeyPreset,
    mouse_enabled: bool,
    reduced_guardrails: bool,
    elevated: bool,
    reduced_motion: bool,
    ascii: bool,
) {
    let (used, limit, spilled) = file_tree.model_stats();
    let mut flags = vec![theme_name];
    if reduced_guardrails {
        flags.push("! REDUCED DELETE GUARD");
    }
    if elevated {
        flags.push("! ELEVATED");
    }
    if reduced_motion {
        flags.push("REDUCED MOTION");
    }
    if mouse_enabled {
        flags.push("MOUSE");
    }
    if ascii {
        flags.push("ASCII");
    }
    if spilled {
        flags.push("IDENTITY SPILL");
    }
    let command = if area.width < 50 {
        " arrows move  Enter open  / filter  ? help".to_string()
    } else if area.width < 72 {
        " arrows move  Enter open  Backspace delete  ? help".to_string()
    } else {
        format!(
            " / filter  arrows/{keymap:?} move  Enter open/rescan  Backspace delete  ? help  mem {}/{}",
            DisplaySize(used as f64),
            DisplaySize(limit as f64)
        )
    };
    let transient_status = match ui_mode {
        UiMode::FilterInput { input, error } => Some(error.as_ref().map_or_else(
            || format!("/ {}_  [Enter] apply  [Esc] cancel", display_text(input)),
            |error| format!("/ {}_  ERROR: {}", display_text(input), display_text(error)),
        )),
        UiMode::Rescanning { target } => Some(status_with_path(
            "~ RESCANNING ",
            target,
            " · deletion locked · [Esc] cancel",
            area.width,
        )),
        UiMode::Loading => Some(ui_effects.last_read_path.as_ref().map_or_else(
            || "~ SCANNING · deletion locked".to_string(),
            |path| status_with_path("~ SCANNING ", path, "", area.width),
        )),
        _ if file_tree.failed_to_read > 0 => {
            Some(format!("? {} unreadable entries", file_tree.failed_to_read))
        }
        _ if board.unrenderable_tile_coordinates.is_some() => {
            Some("Small entries are a viewport summary · use / filter or zoom".to_string())
        }
        _ if board.is_list_layout() && board.hidden_list_entries() > 0 => Some(format!(
            "{} more entries below · use arrows to scroll",
            board.hidden_list_entries()
        )),
        _ => None,
    };
    let status = transient_status.map_or_else(
        || baseline_status(&flags, reduced_guardrails, elevated, area.width),
        |status| status_with_safety(status, reduced_guardrails, elevated, area.width),
    );
    Paragraph::new(vec![
        Line::styled(command, Style::default().fg(theme.text_secondary)),
        Line::styled(
            status,
            Style::default().fg(if reduced_guardrails || elevated {
                theme.text_danger
            } else {
                theme.text_muted
            }),
        ),
    ])
    .alignment(Alignment::Left)
    .render(area, buffer);
}
fn status_with_path(prefix: &str, path: &Path, suffix: &str, width: u16) -> String {
    let displayed = display_path_info(path);
    let marker = if displayed.deceptive {
        if width == 0 {
            ""
        } else if usize::from(width) <= DECEPTIVE_DISPLAY_MARKER.width() {
            "!"
        } else {
            DECEPTIVE_DISPLAY_MARKER
        }
    } else {
        ""
    };
    let separator = if marker.is_empty() { "" } else { " " };
    let reserved = marker
        .width()
        .saturating_add(separator.width())
        .saturating_add(prefix.width())
        .saturating_add(suffix.width());
    let path_width = usize::from(width).saturating_sub(reserved);
    let path = truncate_middle(
        &displayed.text,
        u16::try_from(path_width).unwrap_or(u16::MAX),
    );
    format!("{marker}{separator}{prefix}{path}{suffix}")
}

fn safety_label(reduced_guardrails: bool, elevated: bool, width: u16) -> Option<&'static str> {
    match (reduced_guardrails, elevated) {
        (true, true) if width < 36 => Some("! ELEVATED · ! REDUCED GUARD"),
        (true, true) => Some("! ELEVATED · ! REDUCED DELETE GUARD"),
        (true, false) => Some("! REDUCED DELETE GUARD"),
        (false, true) => Some("! ELEVATED"),
        (false, false) => None,
    }
}

fn status_with_safety(
    status: String,
    reduced_guardrails: bool,
    elevated: bool,
    width: u16,
) -> String {
    let Some(label) = safety_label(reduced_guardrails, elevated, width) else {
        return status;
    };
    if let Some(status) = status
        .strip_prefix(DECEPTIVE_DISPLAY_MARKER)
        .and_then(|status| status.strip_prefix(' '))
    {
        return format!("{DECEPTIVE_DISPLAY_MARKER} {label} · {status}");
    }
    if let Some(status) = status.strip_prefix("! ~ RESCANNING ") {
        return format!("! {label} · ~ RESCANNING {status}");
    }
    if let Some(status) = status.strip_prefix("! ~ SCANNING ") {
        return format!("! {label} · ~ SCANNING {status}");
    }
    format!("{label} · {status}")
}

fn baseline_status(flags: &[&str], reduced_guardrails: bool, elevated: bool, width: u16) -> String {
    let status = flags.join(" · ");
    if status.chars().count() <= usize::from(width)
        || safety_label(reduced_guardrails, elevated, width).is_none()
    {
        return status;
    }
    let mut context = String::new();
    for flag in flags.iter().copied().filter(|flag| !flag.starts_with("! ")) {
        if !context.is_empty() {
            context.push_str(" · ");
        }
        context.push_str(flag);
    }
    status_with_safety(context, reduced_guardrails, elevated, width)
}
fn format_bounds(bounds: ByteBounds) -> String {
    match bounds.upper {
        Some(upper) if upper == bounds.lower => format!("{}", DisplaySize(upper as f64)),
        Some(upper) => format!(
            "{}..{}",
            DisplaySize(bounds.lower as f64),
            DisplaySize(upper as f64)
        ),
        None if bounds.lower == 0 => "unknown".to_string(),
        None => format!(">={}", DisplaySize(bounds.lower as f64)),
    }
}
fn apply_monochrome(buffer: &mut Buffer, base: Color) {
    for cell in &mut buffer.content {
        if cell.bg != Color::Reset && cell.bg != base {
            cell.modifier.insert(Modifier::REVERSED);
        }
        cell.fg = Color::Reset;
        cell.bg = Color::Reset;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ratatui::backend::TestBackend;
    use ratatui::style::Style;

    use crate::model::MIN_PROCESS_MIB;
    use crate::native_path::identity_for;
    use crate::theme::ThemeId;

    use super::*;

    #[test]
    fn monochrome_removes_colors_and_preserves_contrast() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        buffer[(0, 0)].set_style(
            Style::default()
                .fg(Color::Red)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        );
        buffer[(1, 0)].set_style(Style::default().fg(Color::Green));

        apply_monochrome(&mut buffer, Color::Reset);

        assert_eq!(buffer[(0, 0)].fg, Color::Reset);
        assert_eq!(buffer[(0, 0)].bg, Color::Reset);
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert!(buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buffer[(1, 0)].fg, Color::Reset);
        assert!(!buffer[(1, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn every_theme_maps_focus_and_danger_roles() {
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
            buffer[(0, 0)].set_style(Style::default().fg(Color::Blue).bg(Color::Gray));
            buffer[(1, 0)].set_style(Style::default().fg(Color::Red));
            Display::<TestBackend>::apply_theme(&mut buffer, theme);
            assert_eq!(buffer[(0, 0)].fg, theme.focus);
            assert_eq!(buffer[(0, 0)].bg, theme.surface_selection);
            assert_eq!(buffer[(1, 0)].fg, theme.text_danger);
        }
    }

    #[test]
    fn ascii_mode_removes_semantic_unicode_symbols() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        buffer[(0, 0)].set_symbol("◆");
        buffer[(1, 0)].set_symbol("◇");
        buffer[(2, 0)].set_symbol("◫");
        buffer[(3, 0)].set_symbol("◌");
        Display::<TestBackend>::apply_ascii(&mut buffer);
        assert_eq!(buffer[(0, 0)].symbol(), "C");
        assert_eq!(buffer[(1, 0)].symbol(), "A");
        assert_eq!(buffer[(2, 0)].symbol(), "S");
        assert_eq!(buffer[(3, 0)].symbol(), "~");
    }

    #[test]
    fn narrow_selected_entries_receive_a_compact_inspector() {
        for width in [32, 60, 80, 100] {
            let (workspace, inspector) = body_areas(Rect::new(0, 0, width, 19), true);
            let inspector = inspector.expect("selected narrow entry should remain inspectable");
            assert_eq!(workspace.width, width);
            assert_eq!(workspace.height, 19);
            assert_eq!(inspector.width, width);
            assert_eq!(inspector.height, COMPACT_INSPECTOR_HEIGHT);
        }
        assert!(body_areas(Rect::new(0, 0, 100, 19), false).1.is_none());
    }

    #[test]
    fn compact_inspector_exposes_identity_bounds_links_and_aggregation() {
        let root = tempfile::tempdir().expect("inspector root should exist");
        let path = root.path().join("selected-entry");
        fs::write(&path, b"selected contents").expect("fixture should be written");
        let metadata = fs::symlink_metadata(&path).expect("fixture metadata should exist");
        let identity = identity_for(&path, &metadata)
            .expect("fixture identity should be readable")
            .expect("fixture should not be a link");
        let mut tree = FileTree::new(root.path().to_path_buf(), true, MIN_PROCESS_MIB)
            .expect("file tree should be created");
        tree.add_entry(&metadata, &path, identity)
            .expect("fixture should be added")
            .expect("fixture should remain materialized");
        tree.complete_directory(root.path(), None)
            .expect("fixture root should complete");
        tree.finalize().expect("fixture tree should finalize");

        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 78, 10));
        board.change_files(tree.files_in_current_folder(0));
        board.set_selected_index(0);
        let area = Rect::new(0, 0, 80, COMPACT_INSPECTOR_HEIGHT);
        let mut buffer = Buffer::empty(area);
        render_inspector(
            &mut buffer,
            area,
            &tree,
            &board,
            Theme::for_id(ThemeId::ExciseDark),
            false,
        );
        let text = buffer.content.iter().fold(String::new(), |mut text, cell| {
            text.push_str(cell.symbol());
            text
        });
        for expected in [
            "selected-entry",
            "COMPLETE",
            "allocated",
            "reclaim",
            "entries",
            "identity",
            "links",
            "scope",
        ] {
            assert!(
                text.contains(expected),
                "missing compact detail: {expected}"
            );
        }
    }

    #[test]
    fn deceptive_inspector_reason_marker_stays_visible_when_narrow() {
        let root = tempfile::tempdir().expect("inspector root should exist");
        let path = root.path().join("selected-entry");
        fs::write(&path, b"selected contents").expect("fixture should be written");
        let metadata = fs::symlink_metadata(&path).expect("fixture metadata should exist");
        let identity = identity_for(&path, &metadata)
            .expect("fixture identity should be readable")
            .expect("fixture should not be a link");
        let mut tree = FileTree::new(root.path().to_path_buf(), true, MIN_PROCESS_MIB)
            .expect("file tree should be created");
        tree.add_entry(&metadata, &path, identity)
            .expect("fixture should be added")
            .expect("fixture should remain materialized");
        tree.record_unscanned(
            &path,
            UnscannedReason::Metadata("metadata failed\t\u{202e}name\u{1b}[31m".to_string()),
        )
        .expect("hostile reason should be recorded");
        tree.complete_directory(root.path(), None)
            .expect("fixture root should complete");
        tree.finalize().expect("fixture tree should finalize");

        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 78, 10));
        board.change_files(tree.files_in_current_folder(0));
        board.set_selected_index(0);

        for (width, height) in [(4, 64), (12, 40), (24, 24), (52, 10), (80, 10), (80, 14)] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            render_inspector(
                &mut buffer,
                area,
                &tree,
                &board,
                Theme::for_id(ThemeId::ExciseDark),
                false,
            );
            let rendered = buffer.content.iter().fold(String::new(), |mut text, cell| {
                text.push_str(cell.symbol());
                text
            });
            assert!(!rendered.chars().any(char::is_control));
            assert!(!rendered.contains('\u{202e}'));
            assert!(
                rendered.contains(DECEPTIVE_DISPLAY_MARKER) || rendered.contains('!'),
                "deception marker lost at inspector size {width}x{height}: {rendered:?}"
            );
        }
    }

    #[test]
    fn premarked_inspector_reason_marker_stays_visible_when_narrow() {
        let root = tempfile::tempdir().expect("inspector root should exist");
        let path = root.path().join("selected-entry");
        fs::write(&path, b"selected contents").expect("fixture should be written");
        let metadata = fs::symlink_metadata(&path).expect("fixture metadata should exist");
        let identity = identity_for(&path, &metadata)
            .expect("fixture identity should be readable")
            .expect("fixture should not be a link");
        let mut tree = FileTree::new(root.path().to_path_buf(), true, MIN_PROCESS_MIB)
            .expect("file tree should be created");
        tree.add_entry(&metadata, &path, identity)
            .expect("fixture should be added")
            .expect("fixture should remain materialized");
        tree.record_unscanned(
            &path,
            UnscannedReason::Metadata(format!("{DECEPTIVE_DISPLAY_MARKER} metadata failed")),
        )
        .expect("premarked reason should be recorded");
        tree.complete_directory(root.path(), None)
            .expect("fixture root should complete");
        tree.finalize().expect("fixture tree should finalize");

        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 78, 10));
        board.change_files(tree.files_in_current_folder(0));
        board.set_selected_index(0);

        for (width, height) in [(4, 64), (12, 40), (24, 24), (52, 10), (80, 10), (80, 14)] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            render_inspector(
                &mut buffer,
                area,
                &tree,
                &board,
                Theme::for_id(ThemeId::ExciseDark),
                false,
            );
            let rendered = buffer.content.iter().fold(String::new(), |mut text, cell| {
                text.push_str(cell.symbol());
                text
            });
            assert!(!rendered.chars().any(char::is_control));
            assert!(
                rendered.contains(DECEPTIVE_DISPLAY_MARKER) || rendered.contains('!'),
                "premarked deception marker lost at inspector size {width}x{height}: {rendered:?}"
            );
        }
    }

    #[test]
    fn safety_labels_survive_every_transient_status() {
        for status in [
            "~ SCANNING · deletion locked",
            "~ RESCANNING /target · deletion locked",
            "/ filter_  [Enter] apply",
            "? 2 unreadable entries",
            "Small entries are a viewport summary",
            "3 more entries below",
        ] {
            let rendered = status_with_safety(status.to_string(), true, true, 80);
            assert!(rendered.contains("ELEVATED"));
            assert!(rendered.contains("REDUCED DELETE GUARD"));
            assert!(rendered.contains(status));
        }
        let compact = status_with_safety("status".to_string(), true, true, 32);
        assert!(compact.contains("ELEVATED"));
        assert!(compact.contains("REDUCED GUARD"));
        let baseline = baseline_status(
            &[
                "Catppuccin Mocha",
                "! REDUCED DELETE GUARD",
                "! ELEVATED",
                "REDUCED MOTION",
            ],
            true,
            true,
            32,
        );
        assert!(baseline.starts_with("! ELEVATED · ! REDUCED GUARD"));
    }

    #[test]
    fn deceptive_status_marker_stays_visible_when_narrow() {
        let path = Path::new("status-\u{202e}hostile");
        for width in [1, 5, 10, 11, 12, 24] {
            let rendered = status_with_path("~ RESCANNING ", path, "", width);
            assert!(!rendered.chars().any(char::is_control));
            assert!(!rendered.contains('\u{202e}'));
            assert!(
                rendered.starts_with('!') || rendered.starts_with(DECEPTIVE_DISPLAY_MARKER),
                "deception marker lost at width {width}: {rendered:?}"
            );
        }
        let marked = status_with_path("~ RESCANNING ", path, "", 80);
        let marked_with_safety = status_with_safety(marked, true, true, 80);
        assert!(marked_with_safety.starts_with(DECEPTIVE_DISPLAY_MARKER));
        let compact = status_with_path("~ RESCANNING ", path, "", 5);
        let compact_with_safety = status_with_safety(compact, true, true, 5);
        assert!(compact_with_safety.starts_with('!'));
    }
}
