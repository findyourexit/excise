use std::path::Path;
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthStr as _;

use crate::UiMode;
use crate::animation::AnimationScheduler;
use crate::config::{CustomKeyBindings, KeyPreset};
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
use crate::ui::grid::{DenseRectangleGrid, MapLayout};
use crate::ui::modals::{ConfirmBox, ErrorBox, HelpBox, MessageBox, NoticeBox, WarningBox};
use crate::ui::pane::{
    PANE_GAP, accent_at, contrast_ratio, fill_pane, readable_text_on, render_pane,
};

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
        custom_keys: Option<&CustomKeyBindings>,
        mouse_enabled: bool,
        reduced_guardrails: bool,
        reduced_motion: bool,
    ) -> Result<(), AppError> {
        self.terminal
            .draw(|frame| {
                let full_screen = frame.area();
                let elevated = is_user_admin();
                if matches!(ui_mode, UiMode::ScreenTooSmall) {
                    board.settle_geometry();
                    frame.render_widget(TermTooSmall::new(), full_screen);
                    render_safety_banner(
                        frame.buffer_mut(),
                        full_screen,
                        theme,
                        reduced_guardrails,
                        elevated,
                        ascii,
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
                        monochrome,
                    );

                    let has_selection =
                        matches!(ui_mode, UiMode::Normal) && board.currently_selected().is_some();
                    let (workspace_area, inspector_area) = body_areas(shell[1]);
                    let workspace = workspace_content_area(workspace_area);
                    board.change_area(workspace);
                    board.advance_geometry(now, reduced_motion);
                    let rendered_workspace = render_pane(
                        frame.buffer_mut(),
                        workspace_area,
                        workspace_title(board),
                        theme,
                        has_selection,
                        !reduced_motion,
                        monochrome,
                        ascii,
                        now,
                    );
                    debug_assert_eq!(workspace, rendered_workspace);
                    let show_empty_label = file_tree.current_node().state == NodeState::Complete
                        && file_tree.filter().is_none();
                    if board.is_list_layout() {
                        render_list(
                            frame.buffer_mut(),
                            rendered_workspace,
                            board,
                            theme,
                            ascii,
                            now,
                            reduced_motion,
                            matches!(ui_mode, UiMode::Loading | UiMode::Rescanning { .. }),
                            show_empty_label,
                        );
                    } else {
                        frame.render_widget(
                            DenseRectangleGrid::new(
                                map_layout(board, show_empty_label),
                                theme,
                                ascii,
                                monochrome,
                            ),
                            rendered_workspace,
                        );
                    }
                    if let Some(inspector_area) = inspector_area {
                        render_inspector(
                            frame.buffer_mut(),
                            inspector_area,
                            file_tree,
                            board,
                            ui_mode,
                            theme,
                            ascii,
                            monochrome,
                            now,
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
                        custom_keys,
                        mouse_enabled,
                        reduced_guardrails,
                        elevated,
                        reduced_motion,
                        ascii,
                    );
                }
                Self::apply_theme(frame.buffer_mut(), theme);
                // Effects acknowledge an event in the header band and nowhere else.
                // They must land before an overlay so a dialog stays a still,
                // readable decision surface.
                animation.process(
                    now,
                    frame.buffer_mut(),
                    effect_area(full_screen),
                    full_screen,
                );
                if shows_modal(ui_mode) {
                    crate::ui::pane::draw_scrim(frame.buffer_mut(), full_screen, theme, monochrome);
                }

                match ui_mode {
                    UiMode::PlanningDeletion(target) => {
                        frame
                            .render_widget(MessageBox::planning(target, theme, ascii), full_screen);
                    }
                    UiMode::DeleteConfirm {
                        plan: Some(plan),
                        input,
                    } => {
                        frame.render_widget(
                            MessageBox::confirm(
                                plan,
                                input,
                                elevated,
                                reduced_guardrails,
                                theme,
                                ascii,
                            ),
                            full_screen,
                        );
                    }
                    UiMode::Deleting {
                        planned_entries,
                        stopping,
                    } => {
                        frame.render_widget(
                            MessageBox::deleting(*planned_entries, *stopping, theme, ascii),
                            full_screen,
                        );
                    }
                    UiMode::DeletionCancel { planned_entries } => {
                        frame.render_widget(
                            MessageBox::cancel(*planned_entries, theme, ascii),
                            full_screen,
                        );
                    }
                    UiMode::DeletionResult { report } => {
                        frame.render_widget(MessageBox::result(report, theme, ascii), full_screen);
                    }
                    UiMode::ErrorMessage(message) => {
                        frame.render_widget(ErrorBox::new(message, theme, ascii), full_screen);
                    }
                    UiMode::Notice(message) => {
                        frame.render_widget(NoticeBox::new(message, theme, ascii), full_screen);
                    }
                    UiMode::Exiting { save_preferences } => {
                        frame.render_widget(
                            ConfirmBox::new(*save_preferences, theme, ascii),
                            full_screen,
                        );
                    }
                    UiMode::WarningMessage => {
                        frame.render_widget(WarningBox::new(theme, ascii), full_screen);
                    }
                    UiMode::Help => {
                        frame.render_widget(
                            HelpBox::new(keymap, custom_keys, theme, ascii),
                            full_screen,
                        );
                    }
                    UiMode::Loading
                    | UiMode::Normal
                    | UiMode::Rescanning { .. }
                    | UiMode::FilterInput { .. }
                    | UiMode::ScreenTooSmall
                    | UiMode::DeleteConfirm { plan: None, .. } => {}
                }
                if monochrome {
                    apply_monochrome(frame.buffer_mut(), theme);
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
            let foreground = cell.fg;
            cell.fg = if is_semantic_theme_color(foreground, theme) {
                foreground
            } else {
                match foreground {
                    Color::Reset | Color::White => theme.text_primary,
                    Color::Black => theme.text_inverse,
                    Color::Gray | Color::DarkGray => theme.text_muted,
                    Color::Blue | Color::Cyan => theme.focus,
                    Color::Green => theme.state_complete,
                    Color::Yellow => theme.state_aggregated,
                    Color::Magenta => theme.state_shared,
                    Color::Red | Color::LightRed => theme.text_danger,
                    color => color,
                }
            };
            cell.bg = match cell.bg {
                Color::Reset => theme.surface_base,
                color @ (Color::White
                | Color::Black
                | Color::Gray
                | Color::DarkGray
                | Color::Blue
                | Color::Cyan
                | Color::Green
                | Color::Yellow
                | Color::Magenta
                | Color::Red
                | Color::LightRed)
                    if is_semantic_theme_color(color, theme) =>
                {
                    color
                }
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
}

/// Whether a color is already one of the selected theme's semantic roles.
///
/// Most palettes use RGB values, but High Contrast deliberately uses ANSI
/// colors. Those values must bypass the legacy ANSI sentinel mapping above.
fn is_semantic_theme_color(color: Color, theme: Theme) -> bool {
    color == theme.surface_base
        || color == theme.surface_panel
        || color == theme.surface_raised
        || color == theme.surface_selection
        || color == theme.surface_danger
        || color == theme.text_primary
        || color == theme.text_secondary
        || color == theme.text_muted
        || color == theme.text_inverse
        || color == theme.text_danger
        || color == theme.state_scanning
        || color == theme.state_complete
        || color == theme.state_aggregated
        || color == theme.state_rescanning
        || color == theme.state_uncertain
        || color == theme.state_shared
        || color == theme.state_excluded
        || color == theme.border
        || color == theme.focus
}

/// Whether a dialog is layered over the interface this frame.
///
/// The exhaustive match in [`Display::render`] keeps this honest: a new mode
/// has to be classified there, and an unclassified one scrims — a dialog that
/// is too separated costs nothing, one that dissolves into the map costs a
/// misread deletion.
const fn shows_modal(ui_mode: &UiMode) -> bool {
    !matches!(
        ui_mode,
        UiMode::Loading
            | UiMode::Normal
            | UiMode::Rescanning { .. }
            | UiMode::FilterInput { .. }
            | UiMode::ScreenTooSmall
            | UiMode::DeleteConfirm { plan: None, .. }
    )
}

const COMPACT_INSPECTOR_HEIGHT: u16 = 9;
const MINIMUM_WORKSPACE_HEIGHT: u16 = 5;
/// Inner columns the treemap needs before the board falls back to a list.
const MINIMUM_MAP_WIDTH: u16 = 72;
const MINIMUM_INSPECTOR_WIDTH: u16 = 34;
const MAXIMUM_INSPECTOR_WIDTH: u16 = 44;
const MIN_CURSOR_CONTRAST: f32 = 3.0;

/// Splits the body into map and inspector.
///
/// The split never depends on what is selected. An inspector that appears and
/// disappears with the cursor resizes the map underneath it, and every resize
/// re-lays out the treemap: the pane arrangement has to be a property of the
/// terminal, not of the selection.
fn body_areas(area: Rect) -> (Rect, Option<Rect>) {
    // Pane borders cost two columns on each side, and the map only stays a map
    // while its inner width holds `MINIMUM_MAP_WIDTH`.
    let side_by_side = area
        .width
        .saturating_sub(MINIMUM_MAP_WIDTH + 2 + PANE_GAP)
        .min(MAXIMUM_INSPECTOR_WIDTH);
    if side_by_side >= MINIMUM_INSPECTOR_WIDTH {
        let inspector_width = (area.width / 4).clamp(MINIMUM_INSPECTOR_WIDTH, side_by_side);
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(MINIMUM_MAP_WIDTH + 2),
                Constraint::Length(PANE_GAP),
                Constraint::Length(inspector_width),
            ])
            .split(area);
        return (body[0], Some(body[2]));
    }
    if area.width >= 32
        && area.height >= MINIMUM_WORKSPACE_HEIGHT + COMPACT_INSPECTOR_HEIGHT + PANE_GAP
    {
        let workspace_height = area
            .height
            .saturating_sub(COMPACT_INSPECTOR_HEIGHT + PANE_GAP);
        let inspector = Rect::new(
            area.x,
            area.y + workspace_height + PANE_GAP,
            area.width,
            COMPACT_INSPECTOR_HEIGHT,
        );
        let workspace = Rect::new(area.x, area.y, area.width, workspace_height);
        return (workspace, Some(inspector));
    }
    (area, None)
}

/// Finds the same content rectangle that `render_pane` gives the board.
fn workspace_content_area(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
}

/// Selects the pane label from the board layout that occupies its inner area.
fn workspace_title(board: &Board) -> &'static str {
    if board.is_list_layout() {
        "LIST"
    } else {
        "STORAGE MAP"
    }
}

/// Collects the board state that belongs to the geometry currently on screen.
fn map_layout(board: &Board, show_empty_label: bool) -> MapLayout<'_> {
    MapLayout {
        rectangles: board.rendered_tiles(),
        departing: board.departing_tiles(),
        overflow: board.rendered_overflow(),
        selected_rect_index: board.selected_index,
        transitioning: board.is_transitioning(),
        show_empty_label,
    }
}

/// The band effects are allowed to touch: the header, and nothing else.
fn effect_area(full_screen: Rect) -> Rect {
    Rect::new(full_screen.x, full_screen.y, full_screen.width, 3).intersection(full_screen)
}

fn render_safety_banner(
    buffer: &mut Buffer,
    area: Rect,
    theme: Theme,
    reduced_guardrails: bool,
    elevated: bool,
    ascii: bool,
) {
    let Some(label) = safety_label(reduced_guardrails, elevated, area.width, ascii) else {
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
    monochrome: bool,
) {
    fill_pane(buffer, area, theme);
    let current = file_tree.current_node();
    let total = file_tree.total_node();
    let (marker, state, state_color) = view_state(ui_mode, current.state, ascii, theme);
    let state_background = if monochrome {
        theme.surface_panel
    } else {
        state_color
    };
    let path = display_path_middle(&file_tree.get_current_path(), area.width.saturating_sub(30));
    let title = Line::from(vec![
        Span::styled(
            " EXCISE ",
            Style::default()
                .fg(theme.surface_base)
                .bg(theme.focus)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {path} "), Style::default().fg(theme.text_primary)),
        Span::styled(
            format!(" {marker} {state} "),
            Style::default()
                .fg(theme.surface_base)
                .bg(state_background)
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
    Paragraph::new(vec![title, detail]).render(area, buffer);
    let rule_y = area.y.saturating_add(area.height.saturating_sub(1));
    for x in area.x..area.right() {
        if let Some(cell) = buffer.cell_mut((x, rule_y)) {
            cell.set_symbol(if ascii { "-" } else { "▔" })
                .set_style(Style::default().fg(theme.border));
        }
    }
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

/// Builds the list selection style without relying on colour contrast alone.
///
/// Monochrome gives the base and selection surfaces the same reset value. Keep
/// the selection explicit there so the global monochrome pass can remove ink
/// without erasing the cursor row.
fn selected_list_style(theme: Theme, foreground: Color) -> Style {
    let style = Style::default().fg(foreground).bg(theme.surface_selection);
    if theme.surface_selection == theme.surface_base {
        style.add_modifier(Modifier::REVERSED)
    } else {
        style
    }
}

/// Keeps the positional cursor readable when the focus accent is also the
/// selected row's fill, as it is in reduced-motion and ANSI fallback modes.
fn selected_cursor_style(theme: Theme, foreground: Color) -> Style {
    let foreground = match contrast_ratio(foreground, theme.surface_selection) {
        Some(ratio) if ratio >= MIN_CURSOR_CONTRAST => foreground,
        Some(_) => readable_text_on(theme, theme.surface_selection),
        None if foreground == theme.surface_selection => theme.text_inverse,
        None => foreground,
    };
    selected_list_style(theme, foreground)
}

#[allow(
    clippy::fn_params_excessive_bools,
    clippy::too_many_arguments,
    reason = "list rendering keeps presentation, scan state, and empty-state confirmation explicit"
)]
fn render_list(
    buffer: &mut Buffer,
    area: Rect,
    board: &Board,
    theme: Theme,
    ascii: bool,
    now: Duration,
    reduced_motion: bool,
    scanning: bool,
    show_empty_label: bool,
) {
    if board.tiles.is_empty() {
        // An empty model is also the initial loading state. Do not call a
        // directory empty until the scan (or focused rescan) has settled.
        if scanning || !show_empty_label {
            return;
        }
        let label = "Folder is empty";
        let width = u16::try_from(label.len()).unwrap_or(u16::MAX);
        if area.width >= width && area.height > 0 {
            buffer.set_string(
                area.x.saturating_add(area.width.saturating_sub(width) / 2),
                area.y.saturating_add(area.height / 2),
                label,
                Style::default()
                    .fg(theme.text_muted)
                    .add_modifier(Modifier::BOLD),
            );
        }
        return;
    }
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
            selected_list_style(theme, theme.text_inverse).add_modifier(Modifier::BOLD)
        } else if tile.uncertain {
            Style::default().fg(theme.state_uncertain)
        } else {
            Style::default().fg(theme.text_primary)
        };
        let row_y = area.y.saturating_add(index as u16);
        if selected {
            for x in area.x..area.right() {
                if let Some(cell) = buffer.cell_mut((x, row_y)) {
                    cell.set_style(style);
                }
            }
        }

        buffer.set_stringn(area.x, row_y, line, usize::from(area.width), style);
        if selected {
            // One live cell carries the cursor. The row itself holds still, so a
            // long list never becomes a moving target to read.
            if let Some(cell) = buffer.cell_mut((area.x, row_y)) {
                cell.set_symbol(if ascii { ">" } else { "▌" })
                    .set_style(selected_cursor_style(
                        theme,
                        accent_at(theme, now, !reduced_motion, 0),
                    ));
            }
        }
    }
}

fn inspector_action(
    ui_mode: &UiMode,
    synthetic: bool,
    complete: bool,
    ascii: bool,
) -> &'static str {
    match ui_mode {
        UiMode::Normal => {
            if synthetic {
                if ascii {
                    "Enter focused rescan . deletion unavailable"
                } else {
                    "Enter focused rescan · deletion unavailable"
                }
            } else if complete {
                if ascii {
                    "Enter open . Backspace permanent delete"
                } else {
                    "Enter open · Backspace permanent delete"
                }
            } else if ascii {
                "Incomplete scope . deletion unavailable"
            } else {
                "Incomplete scope · deletion unavailable"
            }
        }
        UiMode::FilterInput { .. } => {
            if ascii {
                "Filter input . Enter apply . Esc cancel"
            } else {
                "Filter input · Enter apply · Esc cancel"
            }
        }
        UiMode::Loading | UiMode::Rescanning { .. } => {
            if ascii {
                "Scanning . deletion unavailable"
            } else {
                "Scanning · deletion unavailable"
            }
        }
        _ => "Actions unavailable",
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the inspector keeps its responsive variants and animation context together"
)]
fn render_inspector(
    buffer: &mut Buffer,
    area: Rect,
    file_tree: &FileTree,
    board: &Board,
    ui_mode: &UiMode,
    theme: Theme,
    ascii: bool,
    monochrome: bool,
    now: Duration,
) {
    Clear.render(area, buffer);
    // Only the pane that owns the cursor animates. The workspace holds the
    // selection, so the inspector stays a quiet reference surface beside it.
    let inner = render_pane(
        buffer, area, "INSPECT", theme, false, false, monochrome, ascii, now,
    );
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
    let separator = if ascii { "." } else { "·" };
    let folded_detail = match node.kind {
        NodeKind::Synthetic(SyntheticKind::Other) => Some(format!(
            "folded    {} entries {separator} grouped into this aggregate",
            node.metrics.descendants
        )),
        _ => None,
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
    // An Other node can represent either filter-omitted or capacity-folded entries,
    // so its scope cannot claim a cause the model does not retain.
    let reason = if matches!(node.kind, NodeKind::Synthetic(SyntheticKind::Other)) {
        display_text_info("scope     aggregate")
    } else {
        node.unscanned_reason.as_ref().map_or_else(
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
        )
    };
    let reason_detail = SafeDisplayPath {
        text: format!("{link_detail} {separator} {}", reason.text),
        deceptive: reason.deceptive,
    };
    let action = inspector_action(
        ui_mode,
        node.kind.is_synthetic(),
        node.state == NodeState::Complete,
        ascii,
    );
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
            format!(" {separator} {kind}"),
            Style::default().fg(theme.text_secondary),
        ),
    ]);
    let narrow_state_line = Line::styled(
        truncate_middle(&format!("{marker} {state} {separator} {kind}"), inner.width),
        Style::default().fg(state_color),
    );
    let details = if inner.width < 54 {
        let compact_state_line = if folded_detail.is_some() {
            Line::styled(
                truncate_middle(
                    &format!(
                        "{marker} {state} {separator} {} folded",
                        node.metrics.descendants
                    ),
                    inner.width,
                ),
                Style::default().fg(state_color),
            )
        } else {
            narrow_state_line
        };
        let compact_identity_line = if folded_detail.is_some() {
            Line::styled(
                truncate_middle(action, inner.width),
                Style::default().fg(theme.text_muted),
            )
        } else {
            Line::from(truncate_middle(&identity, inner.width))
        };
        vec![
            name_line,
            compact_state_line,
            Line::from(truncate_middle(
                &format!("allocated {}", format_bounds(node.metrics.allocated_bytes)),
                inner.width,
            )),
            Line::from(truncate_middle(
                &format!(
                    "reclaim   {}",
                    format_bounds(node.metrics.reclaimable_bytes)
                ),
                inner.width,
            )),
            Line::from(truncate_middle(
                &format!(
                    "apparent {} {separator} entries {}",
                    DisplaySize(node.metrics.apparent_bytes as f64),
                    node.metrics.descendants
                ),
                inner.width,
            )),
            compact_identity_line,
            Line::from(truncate_marked(
                &reason_detail,
                inner.width,
                truncate_middle,
            )),
        ]
    } else if inner.height < 12 || (folded_detail.is_some() && inner.height < 13) {
        let identity_or_folded = folded_detail.as_ref().map_or_else(
            || truncate_middle(&identity, inner.width),
            |detail| truncate_middle(detail, inner.width),
        );
        vec![
            name_line,
            state_line,
            Line::from(format!(
                "allocated {} {separator} reclaim {}",
                format_bounds(node.metrics.allocated_bytes),
                format_bounds(node.metrics.reclaimable_bytes)
            )),
            Line::from(format!(
                "apparent {} {separator} entries {}",
                DisplaySize(node.metrics.apparent_bytes as f64),
                node.metrics.descendants
            )),
            Line::from(identity_or_folded),
            Line::from(truncate_marked(
                &reason_detail,
                inner.width,
                truncate_middle,
            )),
            Line::styled(
                truncate_middle(action, inner.width),
                Style::default().fg(theme.text_muted),
            ),
        ]
    } else {
        let mut details = vec![
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
        ];
        if let Some(folded_detail) = &folded_detail {
            details.insert(7, Line::from(truncate_middle(folded_detail, inner.width)));
        }
        details
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
    custom_keys: Option<&CustomKeyBindings>,
    mouse_enabled: bool,
    reduced_guardrails: bool,
    elevated: bool,
    reduced_motion: bool,
    ascii: bool,
) {
    let separator = if ascii { "." } else { "·" };
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
    let movement = movement_hint(keymap, custom_keys);
    let command = command_hint(&movement, used, limit, area.width);
    let transient_status = match ui_mode {
        UiMode::FilterInput { input, error } => Some(error.as_ref().map_or_else(
            || format!("/ {}_  [Enter] apply  [Esc] cancel", display_text(input)),
            |error| format!("/ {}_  ERROR: {}", display_text(input), display_text(error)),
        )),
        UiMode::Rescanning { target } => Some(status_with_path(
            "~ RESCANNING ",
            target,
            if ascii {
                " . deletion locked . [Esc] cancel"
            } else {
                " · deletion locked · [Esc] cancel"
            },
            area.width,
        )),
        UiMode::Loading => Some(ui_effects.last_read_path.as_ref().map_or_else(
            || format!("~ SCANNING {separator} deletion locked"),
            |path| status_with_path("~ SCANNING ", path, "", area.width),
        )),
        _ if file_tree.failed_to_read > 0 => {
            Some(format!("? {} unreadable entries", file_tree.failed_to_read))
        }
        _ if board.overflow().is_some() => Some(format!(
            "Small entries are a viewport summary {separator} use / filter or zoom"
        )),
        _ if board.is_list_layout() && board.hidden_list_entries() > 0 => Some(format!(
            "{} more entries below {separator} use arrows to scroll",
            board.hidden_list_entries()
        )),
        _ => None,
    };
    let status = transient_status.map_or_else(
        || baseline_status(&flags, reduced_guardrails, elevated, area.width, ascii),
        |status| status_with_safety(status, reduced_guardrails, elevated, area.width, ascii),
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
fn command_hint(movement: &str, used: usize, limit: usize, width: u16) -> String {
    const MOVE: &str = " move";
    const OPEN_RESCAN: &str = "  Enter open/rescan";
    const FILTER: &str = "  / filter";
    const EXPORT: &str = "  e export";
    const THEME: &str = "  t theme";
    const DELETE: &str = "  Backspace delete";
    const HELP: &str = "  ? help";

    let available = usize::from(width);
    let movement_width = " ".width().saturating_add(movement.width());
    let basic_width = movement_width
        .saturating_add(MOVE.width())
        .saturating_add(OPEN_RESCAN.width())
        .saturating_add(HELP.width());
    let filter_width = basic_width.saturating_add(FILTER.width());
    let export_width = filter_width.saturating_add(EXPORT.width());
    let theme_width = export_width.saturating_add(THEME.width());
    let delete_width = theme_width.saturating_add(DELETE.width());

    if delete_width <= available {
        let memory = format!(
            "  mem {}/{}",
            DisplaySize(used as f64),
            DisplaySize(limit as f64)
        );
        if delete_width.saturating_add(memory.width()) <= available {
            return format!(
                " {movement}{MOVE}{OPEN_RESCAN}{FILTER}{EXPORT}{THEME}{DELETE}{HELP}{memory}"
            );
        }
        return format!(" {movement}{MOVE}{OPEN_RESCAN}{FILTER}{EXPORT}{THEME}{DELETE}{HELP}");
    }

    if theme_width <= available {
        return format!(" {movement}{MOVE}{OPEN_RESCAN}{FILTER}{EXPORT}{THEME}{HELP}");
    }
    if export_width <= available {
        return format!(" {movement}{MOVE}{OPEN_RESCAN}{FILTER}{EXPORT}{HELP}");
    }
    if filter_width <= available {
        return format!(" {movement}{MOVE}{OPEN_RESCAN}{FILTER}{HELP}");
    }
    if basic_width <= available {
        return format!(" {movement}{MOVE}{OPEN_RESCAN}{HELP}");
    }

    let movement_and_action_width = movement_width
        .saturating_add(MOVE.width())
        .saturating_add(HELP.width());
    if movement_and_action_width <= available {
        return format!(" {movement}{MOVE}{HELP}");
    }
    let movement_and_help_width = movement_width.saturating_add(HELP.width());
    if movement_and_help_width <= available {
        return format!(" {movement}{HELP}");
    }
    if "? help".width() <= available {
        return "? help".to_string();
    }
    if available > 0 {
        return "?".to_string();
    }
    String::new()
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

fn movement_hint(keymap: KeyPreset, custom_keys: Option<&CustomKeyBindings>) -> String {
    match (keymap, custom_keys) {
        (KeyPreset::Vim, _) => "arrows/hjkl".to_string(),
        (KeyPreset::Emacs, _) => "arrows/Ctrl-b/n/p/f".to_string(),
        (KeyPreset::Custom, Some(bindings)) => format!(
            "arrows/{}/{}/{}/{}",
            movement_key_label(bindings.left),
            movement_key_label(bindings.down),
            movement_key_label(bindings.up),
            movement_key_label(bindings.right)
        ),
        (KeyPreset::Custom, None) => "arrows".to_string(),
    }
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

fn safety_label(
    reduced_guardrails: bool,
    elevated: bool,
    width: u16,
    ascii: bool,
) -> Option<&'static str> {
    match (reduced_guardrails, elevated) {
        (true, true) if width < 36 => Some(if ascii {
            "! ELEVATED . ! REDUCED GUARD"
        } else {
            "! ELEVATED · ! REDUCED GUARD"
        }),
        (true, true) => Some(if ascii {
            "! ELEVATED . ! REDUCED DELETE GUARD"
        } else {
            "! ELEVATED · ! REDUCED DELETE GUARD"
        }),
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
    ascii: bool,
) -> String {
    let separator = if ascii { "." } else { "·" };
    let Some(label) = safety_label(reduced_guardrails, elevated, width, ascii) else {
        return status;
    };
    if let Some(status) = status
        .strip_prefix(DECEPTIVE_DISPLAY_MARKER)
        .and_then(|status| status.strip_prefix(' '))
    {
        return format!("{DECEPTIVE_DISPLAY_MARKER} {label} {separator} {status}");
    }
    if let Some(status) = status.strip_prefix("! ~ RESCANNING ") {
        return format!("! {label} {separator} ~ RESCANNING {status}");
    }
    if let Some(status) = status.strip_prefix("! ~ SCANNING ") {
        return format!("! {label} {separator} ~ SCANNING {status}");
    }
    format!("{label} {separator} {status}")
}

fn baseline_status(
    flags: &[&str],
    reduced_guardrails: bool,
    elevated: bool,
    width: u16,
    ascii: bool,
) -> String {
    let separator = if ascii { " . " } else { " · " };
    let status = flags.join(separator);
    if status.chars().count() <= usize::from(width)
        || safety_label(reduced_guardrails, elevated, width, ascii).is_none()
    {
        return status;
    }
    let mut context = String::new();
    for flag in flags.iter().copied().filter(|flag| !flag.starts_with("! ")) {
        if !context.is_empty() {
            context.push_str(separator);
        }
        context.push_str(flag);
    }
    status_with_safety(context, reduced_guardrails, elevated, width, ascii)
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
fn apply_monochrome(buffer: &mut Buffer, theme: Theme) {
    for cell in &mut buffer.content {
        if is_monochrome_emphasis_surface(cell.bg, theme) {
            cell.modifier.insert(Modifier::REVERSED);
        }
        cell.fg = Color::Reset;
        cell.bg = Color::Reset;
    }
}

/// Identifies the two surface roles that may gain contrast after colour is removed.
///
/// Base and panel fills are structural, not emphasis. Existing `REVERSED` flags
/// are intentionally left alone: sources use them for selection and for surfaces
/// whose monochrome roles all resolve to `Reset`.
fn is_monochrome_emphasis_surface(color: Color, theme: Theme) -> bool {
    color != theme.surface_base
        && color != theme.surface_panel
        && (color == theme.surface_selection || color == theme.surface_raised)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;

    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;
    use ratatui::style::Style;

    use crate::filter::FilterPattern;
    use crate::model::{MIN_PROCESS_MIB, NodeId};
    use crate::native_path::identity_for;
    use crate::theme::ThemeId;

    use super::*;

    fn map_file(id: u32, size: u128, percentage: f64) -> crate::state::tiles::FileMetadata {
        crate::state::tiles::FileMetadata {
            node_id: NodeId(id),
            name: OsString::from(format!("file-{id}")),
            size,
            apparent_size: size,
            descendants: None,
            percentage,
            file_type: crate::state::tiles::FileType::File,
            synthetic_kind: None,
            uncertain: false,
        }
    }

    #[test]
    fn display_uses_the_full_surface_for_animation_cadence() {
        let root = tempfile::tempdir().expect("animation root should exist");
        let file_tree = FileTree::new(root.path().to_path_buf(), true, MIN_PROCESS_MIB)
            .expect("file tree should be created");
        let mut display =
            Display::new(TestBackend::new(200, 100)).expect("display should be created");
        let mut board = Board::new();
        let effects = UiEffects::new();
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        animation.set_activity(true);

        display
            .render(
                &file_tree,
                &mut board,
                &UiMode::Normal,
                &effects,
                &mut animation,
                Duration::ZERO,
                "test",
                Theme::for_id(ThemeId::ExciseDark),
                false,
                false,
                KeyPreset::Vim,
                None,
                false,
                false,
                false,
            )
            .expect("display should render");

        assert_eq!(
            animation.next_frame_at(),
            Some(Duration::from_millis(66)),
            "the header paint target must not select the surface cadence"
        );
    }

    #[test]
    fn rendered_map_layout_keeps_stable_overflow_visible() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![
            map_file(1, 11, 0.4),
            map_file(2, 22, 0.3),
            map_file(3, 33, 0.2),
            map_file(4, 44, 0.1),
        ]);
        board.advance_geometry(Duration::ZERO, true);

        board.change_area(Rect::new(0, 0, 72, 1));
        assert!(!board.is_transitioning());
        assert_eq!(map_layout(&board, true).overflow, board.overflow());
    }

    #[test]
    fn command_hints_keep_complete_movement_at_each_tier_boundary() {
        for movement in ["arrows/hjkl", "arrows/Ctrl-b/n/p/f"] {
            for width in [49, 50, 71, 72, 111, 112] {
                let command = command_hint(movement, 0, 0, width);
                assert!(
                    command.width() <= usize::from(width),
                    "command exceeded width {width}: {command:?}"
                );
                assert!(
                    !command.contains("[.."),
                    "command was truncated at width {width}: {command:?}"
                );
                assert!(
                    command.contains(movement),
                    "movement hint was cut at width {width}: {command:?}"
                );
            }
        }
    }

    #[test]
    fn thirty_two_column_command_hints_keep_complete_bindings() {
        let custom = CustomKeyBindings {
            left: ' ',
            down: 's',
            up: 'w',
            right: 'd',
        };
        let custom_movement = movement_hint(KeyPreset::Custom, Some(&custom));
        for (movement, expected) in [
            ("arrows/hjkl", " arrows/hjkl move  ? help"),
            ("arrows/Ctrl-b/n/p/f", " arrows/Ctrl-b/n/p/f  ? help"),
            (custom_movement.as_str(), " arrows/Space/s/w/d move  ? help"),
        ] {
            let command = command_hint(movement, 0, 0, 32);
            assert_eq!(command, expected);
            assert!(command.width() <= 32);
        }
    }

    #[test]
    fn emacs_movement_hint_matches_the_help_spelling() {
        assert_eq!(movement_hint(KeyPreset::Emacs, None), "arrows/Ctrl-b/n/p/f");
    }

    #[test]
    fn command_hints_never_advertise_an_unnamed_delete_key() {
        let mut saw_named_delete = false;
        for width in 72_u16..112 {
            let command = command_hint("arrows/hjkl", 0, 0, width);
            if command.contains("delete") {
                assert!(
                    command.contains("Backspace delete"),
                    "unnamed delete hint at width {width}: {command:?}"
                );
                saw_named_delete = true;
            }
        }
        assert!(
            saw_named_delete,
            "the wide tier should retain the delete binding"
        );
    }

    #[test]
    fn command_hints_keep_rescan_when_memory_does_not_fit() {
        let movement = "arrows/hjkl";
        let expected = " arrows/hjkl move  Enter open/rescan  / filter  e export  t theme  Backspace delete  ? help";
        let width = u16::try_from(expected.width()).expect("rescan hint should fit u16");

        let command = command_hint(movement, 1_024, 2_048, width);

        assert_eq!(command, expected);
        assert!(!command.contains(" mem "));
    }

    #[test]
    fn medium_width_command_hints_keep_the_synthetic_rescan_action() {
        let movement = "arrows/hjkl";
        let expected = " arrows/hjkl move  Enter open/rescan  / filter  e export  t theme  ? help";

        assert_eq!(command_hint(movement, 0, 0, 80), expected);
        for width in [44, 54, 64, 73, 80, 90] {
            let command = command_hint(movement, 0, 0, width);
            assert!(
                command.contains("Enter open/rescan"),
                "wrong Enter action at width {width}: {command:?}"
            );
            assert!(command.width() <= usize::from(width));
        }
    }

    #[test]
    fn forced_monochrome_keeps_selected_and_modal_surfaces_distinct() {
        let theme = Theme::for_id(ThemeId::ExciseDark);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        buffer[(0, 0)].set_style(
            Style::default()
                .fg(theme.text_primary)
                .bg(theme.surface_panel),
        );
        buffer[(1, 0)].set_style(
            Style::default()
                .fg(theme.text_inverse)
                .bg(theme.surface_selection)
                .add_modifier(Modifier::REVERSED),
        );
        buffer[(2, 0)].set_style(
            Style::default()
                .fg(theme.text_primary)
                .bg(theme.surface_raised)
                .add_modifier(Modifier::REVERSED),
        );
        buffer[(3, 0)].set_style(
            Style::default()
                .fg(theme.surface_base)
                .bg(theme.state_aggregated)
                .add_modifier(Modifier::BOLD),
        );
        buffer[(4, 0)].set_style(
            Style::default()
                .fg(theme.text_primary)
                .bg(theme.surface_raised),
        );

        apply_monochrome(&mut buffer, theme);

        for x in [0, 1, 2, 3, 4] {
            assert_eq!(buffer[(x, 0)].fg, Color::Reset);
            assert_eq!(buffer[(x, 0)].bg, Color::Reset);
        }
        assert!(
            !buffer[(0, 0)].modifier.contains(Modifier::REVERSED),
            "ordinary panel cells must stay neutral"
        );
        assert!(
            buffer[(1, 0)].modifier.contains(Modifier::REVERSED),
            "an explicit selection cue must survive"
        );
        assert!(
            buffer[(2, 0)].modifier.contains(Modifier::REVERSED),
            "an explicit modal cue must survive"
        );
        assert!(
            !buffer[(3, 0)].modifier.contains(Modifier::REVERSED),
            "semantic state chips must not become generic emphasis"
        );
        assert!(
            buffer[(4, 0)].modifier.contains(Modifier::REVERSED),
            "a raised modal surface must remain distinct when colour is forced off"
        );

        let monochrome = Theme::for_id(ThemeId::Monochrome);
        let mut explicit_modal = Buffer::empty(Rect::new(0, 0, 1, 1));
        explicit_modal[(0, 0)].set_style(Style::default().add_modifier(Modifier::REVERSED));
        apply_monochrome(&mut explicit_modal, monochrome);
        assert!(explicit_modal[(0, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn loading_list_does_not_claim_an_empty_folder() {
        let area = Rect::new(0, 0, 48, 2);
        let mut board = Board::new();
        board.change_area(area);
        let theme = Theme::for_id(ThemeId::ExciseDark);

        let mut loading = Buffer::empty(area);
        render_list(
            &mut loading,
            area,
            &board,
            theme,
            false,
            Duration::ZERO,
            false,
            true,
            false,
        );
        let loading_text = loading.content.iter().map(Cell::symbol).collect::<String>();
        assert!(!loading_text.contains("Folder is empty"));

        let mut settled = Buffer::empty(area);
        render_list(
            &mut settled,
            area,
            &board,
            theme,
            false,
            Duration::ZERO,
            false,
            false,
            true,
        );
        let settled_text = settled.content.iter().map(Cell::symbol).collect::<String>();
        assert!(settled_text.contains("Folder is empty"));
    }

    #[test]
    fn unconfirmed_empty_list_does_not_claim_an_empty_folder() {
        let area = Rect::new(0, 0, 48, 2);
        let board = Board::new();
        let theme = Theme::for_id(ThemeId::ExciseDark);
        let mut buffer = Buffer::empty(area);

        render_list(
            &mut buffer,
            area,
            &board,
            theme,
            false,
            Duration::ZERO,
            false,
            false,
            false,
        );

        let text = buffer.content.iter().map(Cell::symbol).collect::<String>();
        assert!(!text.contains("Folder is empty"));
    }

    #[test]
    fn ascii_list_preserves_literal_user_glyphs() {
        let area = Rect::new(0, 0, 48, 2);
        let mut board = Board::new();
        board.change_area(area);
        board.change_files(vec![map_file(1, 40, 1.0)]);
        board.tiles[0].name = OsString::from("quarter▌block");
        let mut buffer = Buffer::empty(area);

        render_list(
            &mut buffer,
            area,
            &board,
            Theme::for_id(ThemeId::ExciseDark),
            true,
            Duration::ZERO,
            false,
            false,
            true,
        );

        let text = buffer.content.iter().map(Cell::symbol).collect::<String>();
        assert!(
            text.contains("quarter▌block"),
            "user glyph was rewritten: {text:?}"
        );
    }

    #[test]
    fn monochrome_list_selection_remains_a_full_row_cue() {
        let area = Rect::new(0, 0, 48, 2);
        let mut board = Board::new();
        board.change_area(area);
        board.change_files(vec![map_file(1, 40, 0.5), map_file(2, 40, 0.5)]);
        board.set_selected_index(0);
        assert!(
            board.is_list_layout(),
            "the fixture must use the list renderer"
        );

        let monochrome = Theme::for_id(ThemeId::Monochrome);
        let mut buffer = Buffer::empty(area);
        render_list(
            &mut buffer,
            area,
            &board,
            monochrome,
            false,
            Duration::ZERO,
            true,
            false,
            false,
        );
        Display::<TestBackend>::apply_theme(&mut buffer, monochrome);
        apply_monochrome(&mut buffer, monochrome);

        for x in area.x..area.right() {
            let selected = &buffer[(x, area.y)];
            assert_eq!(selected.fg, Color::Reset);
            assert_eq!(selected.bg, Color::Reset);
            assert!(
                selected.modifier.contains(Modifier::REVERSED),
                "selected list cell {x} must remain reverse-video"
            );

            let ordinary = &buffer[(x, area.y + 1)];
            assert_eq!(ordinary.fg, Color::Reset);
            assert_eq!(ordinary.bg, Color::Reset);
            assert!(
                !ordinary.modifier.contains(Modifier::REVERSED),
                "ordinary list cell {x} must stay neutral"
            );
        }

        let high_contrast = Theme::for_id(ThemeId::HighContrast);
        let mut contrast_buffer = Buffer::empty(area);
        render_list(
            &mut contrast_buffer,
            area,
            &board,
            high_contrast,
            false,
            Duration::ZERO,
            true,
            false,
            false,
        );
        Display::<TestBackend>::apply_theme(&mut contrast_buffer, high_contrast);
        let cursor = &contrast_buffer[(area.x, area.y)];
        assert_eq!(cursor.fg, high_contrast.text_inverse);
        assert_eq!(cursor.bg, high_contrast.surface_selection);
        assert!(!cursor.modifier.contains(Modifier::REVERSED));
        let selected_text = &contrast_buffer[(area.x + 1, area.y)];
        assert_eq!(selected_text.fg, high_contrast.text_inverse);
        assert_eq!(selected_text.bg, high_contrast.surface_selection);
        assert!(!selected_text.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn animated_list_cursor_meets_selection_contrast_floor() {
        let area = Rect::new(0, 0, 48, 2);
        let mut board = Board::new();
        board.change_area(area);
        board.change_files(vec![map_file(1, 40, 0.5), map_file(2, 40, 0.5)]);
        board.set_selected_index(0);
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let mut buffer = Buffer::empty(area);

        render_list(
            &mut buffer,
            area,
            &board,
            theme,
            false,
            Duration::ZERO,
            false,
            false,
            true,
        );

        let cursor = &buffer[(area.x, area.y)];
        assert!(
            contrast_ratio(cursor.fg, cursor.bg).is_some_and(|ratio| ratio >= MIN_CURSOR_CONTRAST),
            "animated cursor must remain readable against the selected row"
        );
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
    fn high_contrast_state_chip_backgrounds_survive_theme_postprocessing() {
        let theme = Theme::for_id(ThemeId::HighContrast);
        for node_state in [NodeState::Scanning, NodeState::Aggregated] {
            let (_, state, state_color) = view_state(&UiMode::Normal, node_state, false, theme);
            let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
            buffer.set_string(
                0,
                0,
                state,
                Style::default()
                    .fg(theme.surface_base)
                    .bg(state_color)
                    .add_modifier(Modifier::BOLD),
            );

            Display::<TestBackend>::apply_theme(&mut buffer, theme);

            assert_eq!(buffer[(0, 0)].bg, state_color, "{state} chip background");
        }
    }

    #[test]
    fn high_contrast_scrim_stays_separate_from_the_modal_surface() {
        let root = tempfile::tempdir().expect("modal root should exist");
        let file_tree = FileTree::new(root.path().to_path_buf(), true, MIN_PROCESS_MIB)
            .expect("file tree should be created");
        let mut display =
            Display::new(TestBackend::new(80, 24)).expect("display should be created");
        let mut board = Board::new();
        let effects = UiEffects::new();
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        let theme = Theme::for_id(ThemeId::HighContrast);

        display
            .render(
                &file_tree,
                &mut board,
                &UiMode::Help,
                &effects,
                &mut animation,
                Duration::ZERO,
                "High Contrast",
                theme,
                false,
                false,
                KeyPreset::Vim,
                None,
                false,
                false,
                false,
            )
            .expect("display should render");

        let buffer = display.terminal.backend().buffer();
        assert_ne!(
            buffer[(79, 12)].bg,
            theme.surface_raised,
            "the unpainted gap behind the modal must remain a scrim, not a raised surface"
        );
        assert_eq!(
            buffer[(4, 4)].bg,
            theme.surface_raised,
            "the modal itself must retain its raised surface"
        );
    }

    #[test]
    fn scheduled_header_effect_cannot_repaint_compact_error_modal() {
        let area = Rect::new(0, 0, 30, 7);
        let root = tempfile::tempdir().expect("modal root should exist");
        let file_tree = FileTree::new(root.path().to_path_buf(), true, MIN_PROCESS_MIB)
            .expect("file tree should be created");
        let mut display = Display::new(TestBackend::new(area.width, area.height))
            .expect("display should be created");
        let mut board = Board::new();
        let effects = UiEffects::new();
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        let theme = Theme::for_id(ThemeId::ExciseDark);
        let message = "X";
        animation.schedule_error();
        assert!(
            animation.is_running(),
            "the header effect should be scheduled"
        );

        display
            .render(
                &file_tree,
                &mut board,
                &UiMode::ErrorMessage(message.to_string()),
                &effects,
                &mut animation,
                Duration::ZERO,
                "test",
                theme,
                false,
                false,
                KeyPreset::Vim,
                None,
                false,
                false,
                false,
            )
            .expect("display should render");

        let mut expected = Buffer::empty(area);
        ratatui::widgets::Widget::render(ErrorBox::new(message, theme, false), area, &mut expected);
        assert_eq!(
            &display.terminal.backend().buffer().content,
            &expected.content,
            "a compact error modal covers the header and must paint after its effect"
        );
    }

    #[test]
    fn the_inspector_stacks_below_the_map_until_the_terminal_can_seat_it_beside() {
        for width in [32, 60, 80, 100] {
            let (workspace, inspector) = body_areas(Rect::new(0, 0, width, 19));
            let inspector = inspector.expect("a narrow terminal stacks the inspector");
            assert_eq!(workspace.width, width);
            assert_eq!(workspace.height, 9);
            assert_eq!(inspector.y, workspace.bottom() + PANE_GAP);
            assert_eq!(inspector.width, width);
            assert_eq!(inspector.height, COMPACT_INSPECTOR_HEIGHT);
        }

        let (workspace, inspector) = body_areas(Rect::new(0, 0, 140, 19));
        let inspector = inspector.expect("a wide terminal seats the inspector beside the map");
        assert_eq!(
            workspace.height, 19,
            "a side inspector costs the map no rows"
        );
        assert_eq!(inspector.x, workspace.right() + PANE_GAP);
        assert!(
            workspace.width >= MINIMUM_MAP_WIDTH + 2,
            "the map keeps the columns it needs to stay a map: {}",
            workspace.width
        );
        assert!((MINIMUM_INSPECTOR_WIDTH..=MAXIMUM_INSPECTOR_WIDTH).contains(&inspector.width));

        assert!(
            body_areas(Rect::new(0, 0, 100, 8)).1.is_none(),
            "a terminal too short for both panes keeps the map whole"
        );
    }

    #[test]
    fn workspace_title_follows_the_inner_list_layout_boundary() {
        for width in [72, 73] {
            let mut board = Board::new();
            let workspace = workspace_content_area(Rect::new(0, 0, width, 12));
            board.change_area(workspace);

            assert!(
                board.is_list_layout(),
                "inner width at {width} must use the list"
            );
            assert_eq!(workspace_title(&board), "LIST");
        }

        let mut board = Board::new();
        let workspace = workspace_content_area(Rect::new(0, 0, 74, 12));
        board.change_area(workspace);
        assert!(!board.is_list_layout());
        assert_eq!(workspace_title(&board), "STORAGE MAP");
    }

    #[test]
    fn inspector_actions_follow_the_active_ui_mode() {
        assert_eq!(
            inspector_action(&UiMode::Normal, false, true, false),
            "Enter open · Backspace permanent delete"
        );
        assert_eq!(
            inspector_action(
                &UiMode::FilterInput {
                    input: String::new(),
                    error: None,
                },
                false,
                true,
                false,
            ),
            "Filter input · Enter apply · Esc cancel"
        );
        assert_eq!(
            inspector_action(
                &UiMode::Rescanning {
                    target: std::path::PathBuf::new()
                },
                false,
                true,
                false,
            ),
            "Scanning · deletion unavailable"
        );
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
            &UiMode::Normal,
            Theme::for_id(ThemeId::ExciseDark),
            false,
            false,
            Duration::ZERO,
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
    fn filtered_other_inspector_keeps_action_and_scope_within_seven_compact_rows() {
        let root = tempfile::tempdir().expect("inspector root should exist");
        let matched = root.path().join("matched.log");
        let omitted = root.path().join("omitted.tmp");
        fs::write(&matched, b"abc").expect("matched fixture should be written");
        fs::write(&omitted, b"defgh").expect("omitted fixture should be written");
        let mut tree = FileTree::new(root.path().to_path_buf(), true, MIN_PROCESS_MIB)
            .expect("file tree should be created");
        tree.begin_rescan(
            root.path().to_path_buf(),
            Some(FilterPattern::new("*.log").expect("filter should compile")),
        )
        .expect("focused rescan should begin");
        for path in [&matched, &omitted] {
            let metadata = fs::symlink_metadata(path).expect("fixture metadata should exist");
            let identity = identity_for(path, &metadata)
                .expect("fixture identity should be readable")
                .expect("fixture should not be a link");
            tree.add_entry(&metadata, path, identity)
                .expect("fixture should be recorded");
        }
        tree.finish_rescan()
            .expect("focused rescan should finalize");

        let files = tree.files_in_current_folder(0);
        let other = files
            .iter()
            .find(|file| file.synthetic_kind == Some(SyntheticKind::Other))
            .map(|file| file.node_id)
            .expect("filtered entry should be represented by Other");
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 120, 24));
        board.change_files(files);
        assert!(board.select_node(other));

        let area = Rect::new(0, 0, 120, 24);
        let mut buffer = Buffer::empty(area);
        render_inspector(
            &mut buffer,
            area,
            &tree,
            &board,
            &UiMode::Normal,
            Theme::for_id(ThemeId::ExciseDark),
            false,
            false,
            Duration::ZERO,
        );
        let text = buffer.content.iter().fold(String::new(), |mut text, cell| {
            text.push_str(cell.symbol());
            text
        });
        assert!(text.contains("folded    1 entries"));
        assert!(text.contains("grouped into this aggregate"));
        assert!(!text.contains("retained-entry cap"));
        assert!(!text.contains("memory budget"));
        assert!(!text.contains("MemoryAggregation"));

        let compact_area = Rect::new(0, 0, 52, COMPACT_INSPECTOR_HEIGHT);
        let mut compact_buffer = Buffer::empty(compact_area);
        render_inspector(
            &mut compact_buffer,
            compact_area,
            &tree,
            &board,
            &UiMode::Normal,
            Theme::for_id(ThemeId::ExciseDark),
            false,
            false,
            Duration::ZERO,
        );
        let compact_text = compact_buffer
            .content
            .iter()
            .fold(String::new(), |mut text, cell| {
                text.push_str(cell.symbol());
                text
            });
        for expected in [
            "1 folded",
            "Enter focused rescan · deletion unavailable",
            "scope     aggregate",
        ] {
            assert!(
                compact_text.contains(expected),
                "missing compact aggregate detail: {expected}"
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
                &UiMode::Normal,
                Theme::for_id(ThemeId::ExciseDark),
                false,
                false,
                Duration::ZERO,
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
                &UiMode::Normal,
                Theme::for_id(ThemeId::ExciseDark),
                false,
                false,
                Duration::ZERO,
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
            let rendered = status_with_safety(status.to_string(), true, true, 80, false);
            assert!(rendered.contains("ELEVATED"));
            assert!(rendered.contains("REDUCED DELETE GUARD"));
            assert!(rendered.contains(status));
        }
        let compact = status_with_safety("status".to_string(), true, true, 32, false);
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
            false,
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
        let marked_with_safety = status_with_safety(marked, true, true, 80, false);
        assert!(marked_with_safety.starts_with(DECEPTIVE_DISPLAY_MARKER));
        let compact = status_with_path("~ RESCANNING ", path, "", 5);
        let compact_with_safety = status_with_safety(compact, true, true, 5, false);
        assert!(compact_with_safety.starts_with('!'));
    }
}
