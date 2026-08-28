use std::time::Duration;

use ratatui::buffer::{Buffer, CellWidth as _};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border::Set;
use ratatui::widgets::{Block, Borders, Clear, Widget as _};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::theme::Theme;
use crate::ui::palette::{
    ColorCycle, CycleInk, MIN_FOCUS_CONTRAST, Oklch, cycle_step, derived_for,
    derived_for_with_monochrome,
};

/// One cell of breathing room between independent panes, matching exabind's
/// single-column padding between adjacent widgets.
pub(crate) const PANE_GAP: u16 = 1;

/// How far the interface behind a modal sinks toward the base surface. Enough
/// to guarantee the dialog reads as a separate layer, short of erasing what it
/// interrupts: the map stays legible as context for the decision.
pub(crate) const SCRIM_SINK: f32 = 0.62;

/// exabind's low-ink frame: eighth-block edges that read as a hairline rule,
/// anchored by quadrant corners instead of a heavy box outline.
pub(crate) const PANE_BORDER_SET: Set = Set {
    top_left: "▟",
    top_right: "▜",
    bottom_left: "▔",
    bottom_right: "▔",
    vertical_left: "▏",
    vertical_right: "▕",
    horizontal_top: "▔",
    horizontal_bottom: "▔",
};

/// ASCII counterpart of [`PANE_BORDER_SET`], selected before user text is drawn.
pub(crate) const ASCII_PANE_BORDER_SET: Set = Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "-",
    bottom_right: "-",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

/// Draws a pane and returns its content area.
///
/// Frame, then travelling cycle, then title chip, strictly in that order. The
/// chip is painted last so the animation underneath can never overwrite it,
/// which is the failure mode of animating a `Block` title in place.
#[allow(
    clippy::fn_params_excessive_bools,
    clippy::too_many_arguments,
    reason = "pane rendering keeps geometry, theme, accessibility, timing, and output mode explicit"
)]
pub(crate) fn render_pane(
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    theme: Theme,
    active: bool,
    animate: bool,
    monochrome: bool,
    ascii: bool,
    now: Duration,
) -> Rect {
    let perimeter = border_len(area);
    let active_cycle = active.then(|| derived_for_with_monochrome(theme, monochrome).0);
    let accent = active_cycle.map_or(theme.border, |cycle| {
        if animate || monochrome {
            cycle.at_perimeter(0, 0, perimeter)
        } else {
            cycle.at(0)
        }
    });
    let cycle = active_cycle.filter(|_| animate || monochrome).map(|cycle| {
        let step = if animate { cycle_step(now) } else { 0 };
        (cycle, step, perimeter)
    });
    let border_set = if ascii {
        ASCII_PANE_BORDER_SET
    } else {
        PANE_BORDER_SET
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border_set)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(theme.surface_panel));
    let inner = block.inner(area);
    block.render(area, buffer);

    if let Some((cycle, step, perimeter)) = cycle.as_ref() {
        walk_border(area, |x, y, index| {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.fg = cycle.at_perimeter(*step, index, *perimeter);
            }
        });
    }

    draw_title_chip(
        buffer,
        area,
        title,
        theme,
        theme.surface_panel,
        accent,
        cycle
            .as_ref()
            .map(|(cycle, step, perimeter)| (cycle, *step, *perimeter)),
        monochrome,
        ascii,
    );
    inner
}

/// Visits every border cell clockwise from the top-left, exactly once.
///
/// The order matters: consecutive indices must land on physically adjacent
/// cells, otherwise a colour cycle keyed on the index reads as noise instead of
/// as one gradient moving around the frame.
fn walk_border(area: Rect, mut visit: impl FnMut(u16, u16, usize)) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut index = 0usize;
    if area.height == 1 {
        for x in area.x..area.right() {
            visit(x, area.y, index);
            index += 1;
        }
        return;
    }
    if area.width == 1 {
        for y in area.y..area.bottom() {
            visit(area.x, y, index);
            index += 1;
        }
        return;
    }
    let last_x = area.right() - 1;
    let last_y = area.bottom() - 1;
    for x in area.x..area.right() {
        visit(x, area.y, index);
        index += 1;
    }
    for y in area.y + 1..area.bottom() {
        visit(last_x, y, index);
        index += 1;
    }
    for x in (area.x..last_x).rev() {
        visit(x, last_y, index);
        index += 1;
    }
    for y in (area.y + 1..last_y).rev() {
        visit(area.x, y, index);
        index += 1;
    }
}

/// Returns the count and order-space of [`walk_border`] for `area`.
fn border_len(area: Rect) -> usize {
    match (area.width, area.height) {
        (0, _) | (_, 0) => 0,
        (width, 1) => usize::from(width),
        (1, height) => usize::from(height),
        (width, height) => 2 * (usize::from(width) + usize::from(height)) - 4,
    }
}

/// Paints the pane label as a chip seated in the top rule.
///
/// exabind styles its title as reversed bold ink laid straight onto the frame,
/// and lets the perimeter cycle run through the title row, so the label reads as
/// part of the border rather than as a plaque bolted onto it. Ours does the
/// same: `cycle` carries the border's colour sequence continuing across the
/// chip's own cells, and quadrant caps close each end so the block is not left
/// squared off against the hairline rule.
#[allow(
    clippy::too_many_arguments,
    reason = "title chip rendering keeps geometry, palette, cycle, and accessibility inputs explicit"
)]
fn draw_title_chip(
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    theme: Theme,
    surface: Color,
    accent: Color,
    cycle: Option<(&ColorCycle, usize, usize)>,
    monochrome: bool,
    ascii: bool,
) {
    if area.width < 7 || area.height == 0 || title.is_empty() {
        return;
    }
    let room = usize::from(area.width.saturating_sub(4));
    let title = title_prefix_to_width(title, room.saturating_sub(2));
    let title_width = usize::from(title.cell_width());
    if title_width == 0 {
        return;
    }
    let static_style = if cycle.is_none() {
        static_chip_style(accent, theme, monochrome)
    } else {
        Style::default()
    };

    // The top row is the start of the border walk, so a cell's place in the
    // cycle is just its distance from the left edge. Colouring the chip by the
    // same rule makes the sequence run through the label instead of restarting
    // at it, which is what keeps the title reading as part of the frame.
    let start = area.x.saturating_add(2);
    let colour_at = |x: u16| {
        cycle.map_or(accent, |(cycle, step, perimeter)| {
            cycle.at_perimeter(step, usize::from(x.saturating_sub(area.x)), perimeter)
        })
    };
    let style_at = |x: u16| match cycle {
        Some((cycle, step, perimeter)) => {
            let (fill, ink) =
                cycle.chip_at_perimeter(step, usize::from(x.saturating_sub(area.x)), perimeter);
            match ink {
                CycleInk::Foreground(foreground) => Style::default()
                    .fg(foreground)
                    .bg(fill)
                    .add_modifier(Modifier::BOLD),
                CycleInk::Reversed => Style::default()
                    .fg(fill)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            }
        }
        None => static_style,
    };
    let end = {
        let mut x = start;
        let mut paint_segment = |text: &str, max_width: usize| {
            let segment_start = x;
            x = buffer
                .set_stringn(x, area.y, text, max_width, Style::default())
                .0;
            for cell_x in segment_start..x {
                if let Some(cell) = buffer.cell_mut((cell_x, area.y)) {
                    cell.set_style(style_at(cell_x));
                }
            }
        };
        paint_segment(" ", 1);
        paint_segment(title, title_width);
        paint_segment(" ", 1);
        x
    };

    // Half-block caps: the chip's colour bleeds into the rule on the left and
    // back out of it on the right, so the label is seated in the frame rather
    // than squared off against it.
    let left = start.saturating_sub(1);
    let right = end;
    if let Some(cell) = buffer.cell_mut((left, area.y)) {
        cell.set_symbol(if ascii { "|" } else { "▐" })
            .set_style(Style::default().fg(colour_at(left)).bg(surface));
    }
    if let Some(cell) = buffer.cell_mut((right, area.y)) {
        cell.set_symbol(if ascii { "|" } else { "▌" })
            .set_style(Style::default().fg(colour_at(right)).bg(surface));
    }
}

/// Retains only whole grapheme clusters that fit the available terminal columns.
/// Widths follow Ratatui's buffer writer exactly, including terminal-visible
/// halfwidth voiced and semi-voiced Katakana marks.
fn title_prefix_to_width(title: &str, max_width: usize) -> &str {
    if usize::from(title.cell_width()) <= max_width {
        return title;
    }

    let mut width: usize = 0;
    let mut end = 0;
    for (index, grapheme) in title.grapheme_indices(true) {
        let grapheme_width = usize::from(grapheme.cell_width());
        if width.saturating_add(grapheme_width) > max_width {
            break;
        }
        width += grapheme_width;
        end = index + grapheme.len();
    }
    &title[..end]
}

/// Text placed on a title chip must retain ordinary body-text contrast.
const TITLE_CHIP_CONTRAST_FLOOR: f32 = 4.5;

/// Applies the strongest semantic contrast once for a chip that does not travel
/// with the focus cycle, falling back to the animated chip's neutral polarity.
fn static_chip_style(lead: Color, theme: Theme, monochrome: bool) -> Style {
    if lead == Color::Reset && theme.surface_base == Color::Reset {
        // The monochrome theme has no colour channel. Leave the inactive chip
        // plain so the active chip's reverse-video treatment remains distinct.
        return Style::default()
            .fg(lead)
            .bg(theme.surface_panel)
            .add_modifier(Modifier::BOLD);
    }
    if monochrome && !matches!(lead, Color::Rgb(..)) {
        // ANSI colours cannot be measured, and the active chip already carries
        // reverse video in forced monochrome. Keep this inactive chip plain.
        return Style::default()
            .fg(lead)
            .bg(theme.surface_panel)
            .add_modifier(Modifier::BOLD);
    }
    match strongest_static_ink(lead, [theme.text_primary, theme.surface_base]) {
        Some(ink) => Style::default()
            .fg(ink)
            .bg(lead)
            .add_modifier(Modifier::BOLD),
        // Palette terminals cannot be measured for contrast. Let the terminal
        // invert the chip for us, as exabind does.
        None => Style::default()
            .fg(lead)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED),
    }
}

fn strongest_static_ink(lead: Color, candidates: [Color; 2]) -> Option<Color> {
    candidates
        .into_iter()
        .filter_map(|candidate| contrast_ratio(lead, candidate).map(|ratio| (candidate, ratio)))
        .filter(|(_, ratio)| *ratio >= TITLE_CHIP_CONTRAST_FLOOR)
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(candidate, _)| candidate)
        .or_else(|| strongest_neutral_static_ink(lead))
}

/// Chooses readable semantic or neutral text for a truecolour surface.
pub(crate) fn readable_text_on(theme: Theme, surface: Color) -> Color {
    strongest_static_ink(surface, [theme.text_primary, theme.surface_base])
        .unwrap_or(theme.text_primary)
}

/// Matches animated chips: when semantic ink is too close to the fill, use the
/// measurable neutral with the greater contrast.
fn strongest_neutral_static_ink(lead: Color) -> Option<Color> {
    let Color::Rgb(_, _, _) = lead else {
        return None;
    };
    let black = Color::Rgb(0, 0, 0);
    let white = Color::Rgb(u8::MAX, u8::MAX, u8::MAX);
    let black_contrast = contrast_ratio(lead, black)?;
    let white_contrast = contrast_ratio(lead, white)?;
    Some(if black_contrast >= white_contrast {
        black
    } else {
        white
    })
}

pub(crate) fn contrast_ratio(first: Color, second: Color) -> Option<f32> {
    let first = relative_luminance(first)?;
    let second = relative_luminance(second)?;
    let (lighter, darker) = if first >= second {
        (first, second)
    } else {
        (second, first)
    };
    Some((lighter + 0.05) / (darker + 0.05))
}

fn relative_luminance(color: Color) -> Option<f32> {
    let Color::Rgb(red, green, blue) = color else {
        return None;
    };
    Some(0.2126f32.mul_add(
        chip_srgb_to_linear(red),
        0.7152f32.mul_add(
            chip_srgb_to_linear(green),
            0.0722 * chip_srgb_to_linear(blue),
        ),
    ))
}

fn chip_srgb_to_linear(channel: u8) -> f32 {
    let value = f32::from(channel) / 255.0;
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// A live sample of the focus cycle for small accents, such as the cursor
/// marker on a list row. Holds still when motion is disabled.
pub(crate) fn accent_at(theme: Theme, now: Duration, animate: bool, offset: usize) -> Color {
    if !animate {
        return theme.focus;
    }
    let (cycle, _) = derived_for(theme);
    cycle.at(cycle_step(now) + offset)
}

/// Sinks everything already drawn one step behind the interface, so a modal
/// reads as a layer above it rather than a panel pasted into it.
///
/// Without this the dialog can land on a tile that shares its surface colour
/// and dissolve into the map. `monochrome` terminals have no colour to sink,
/// so the layer behind is flattened onto the base surface instead: the map
/// stops carrying inverted ink and the dialog becomes the only raised block
/// on screen.
pub(crate) fn draw_scrim(buffer: &mut Buffer, area: Rect, theme: Theme, monochrome: bool) {
    let base = if monochrome {
        None
    } else {
        Oklch::from_color(theme.surface_base)
    };
    let mut sources = ScrimSourceCache::default();
    for position in area.positions() {
        let Some(cell) = buffer.cell_mut(position) else {
            continue;
        };
        let style = cell.style();
        let (fg, bg) = if monochrome {
            (style.fg.unwrap_or(theme.text_muted), theme.surface_base)
        } else {
            (
                sink(style.fg, base, theme.text_muted, &mut sources),
                sink(style.bg, base, theme.surface_base, &mut sources),
            )
        };
        // A scrimmed layer carries no highlights: reversed ink behind a dialog
        // reads as brighter than the dialog itself.
        cell.modifier.remove(Modifier::REVERSED);
        cell.set_style(Style::default().fg(fg).bg(bg));
    }
}

/// Remembers the tones adjacent cells share so a scrim only decomposes each
/// repeated truecolour source once while it crosses the terminal.
#[derive(Default)]
struct ScrimSourceCache {
    newest: Option<(Color, Oklch)>,
    older: Option<(Color, Oklch)>,
}

impl ScrimSourceCache {
    fn decompose(&mut self, color: Color) -> Option<Oklch> {
        if let Some((cached, source)) = self.newest
            && cached == color
        {
            return Some(source);
        }
        if let Some((cached, source)) = self.older
            && cached == color
        {
            return Some(source);
        }
        let source = Oklch::from_color(color)?;
        self.older = self.newest;
        self.newest = Some((color, source));
        Some(source)
    }
}

/// Mixes a colour most of the way to the base surface, falling back to a flat
/// `fallback` when the theme is not truecolour and cannot be interpolated.
fn sink(
    color: Option<Color>,
    base: Option<Oklch>,
    fallback: Color,
    sources: &mut ScrimSourceCache,
) -> Color {
    let (Some(color), Some(base)) = (color, base) else {
        return fallback;
    };
    let Some(source) = sources.decompose(color) else {
        return fallback;
    };
    source
        .towards(base, SCRIM_SINK)
        .shifted(0.0, 0.45)
        .to_color()
}

fn contrast_safe_accent(theme: Theme, surface: Color, accent: Color) -> Color {
    if contrast_ratio(accent, surface).is_some_and(|ratio| ratio >= MIN_FOCUS_CONTRAST) {
        return accent;
    }
    strongest_static_ink(surface, [theme.text_primary, theme.surface_base])
        .unwrap_or(theme.text_primary)
}

/// Draws a modal panel and returns its content area.
///
/// Same frame and chip as a pane, raised one surface step off the interface it
/// interrupts, and deliberately still: a dialog that pulses competes with the
/// decision it is asking for.
pub(crate) fn render_modal(
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    theme: Theme,
    accent: Color,
    ascii: bool,
) -> Rect {
    Clear.render(area, buffer);
    // Reset-valued surface roles cannot express elevation, so retain it as an
    // explicit modifier instead of relying on background-colour inference.
    let modal_style = Style::default().bg(theme.surface_raised);
    let modal_style = if theme.surface_raised == theme.surface_base {
        modal_style.add_modifier(Modifier::REVERSED)
    } else {
        modal_style
    };
    let accent = contrast_safe_accent(theme, theme.surface_raised, accent);
    let border_set = if ascii {
        ASCII_PANE_BORDER_SET
    } else {
        PANE_BORDER_SET
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border_set)
        .border_style(Style::default().fg(accent))
        .style(modal_style);
    let inner = block.inner(area);
    block.render(area, buffer);
    draw_title_chip(
        buffer,
        area,
        title,
        theme,
        theme.surface_raised,
        accent,
        None,
        false,
        ascii,
    );
    inner
}

pub(crate) fn fill_pane(buffer: &mut Buffer, area: Rect, theme: Theme) {
    for position in area.positions() {
        if let Some(cell) = buffer.cell_mut(position) {
            cell.set_symbol(" ")
                .set_style(Style::default().bg(theme.surface_panel));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeId;

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (buffer.area.x..buffer.area.right()).fold(String::new(), |mut text, x| {
            text.push_str(buffer[(x, y)].symbol());
            text
        })
    }

    fn render(active: bool, animate: bool, now: Duration, theme: ThemeId) -> Buffer {
        render_with_monochrome(active, animate, false, now, theme)
    }

    fn render_with_monochrome(
        active: bool,
        animate: bool,
        monochrome: bool,
        now: Duration,
        theme: ThemeId,
    ) -> Buffer {
        let area = Rect::new(0, 0, 20, 5);
        let mut buffer = Buffer::empty(area);
        render_pane(
            &mut buffer,
            area,
            "STORAGE MAP",
            Theme::for_id(theme),
            active,
            animate,
            monochrome,
            false,
            now,
        );
        buffer
    }

    #[test]
    fn the_title_chip_survives_the_border_animation() {
        let buffer = render(
            true,
            true,
            Duration::from_millis(533),
            ThemeId::CatppuccinMocha,
        );
        assert!(
            row_text(&buffer, 0).contains("STORAGE MAP"),
            "the chip must never be overpainted: {:?}",
            row_text(&buffer, 0)
        );
        assert!(
            buffer[(2, 0)].bg != buffer[(2, 0)].fg,
            "the label has to stay readable against its own chip"
        );
    }

    #[test]
    fn a_wide_title_uses_terminal_columns_without_overwriting_its_caps() {
        let area = Rect::new(0, 0, 9, 5);
        let mut buffer = Buffer::empty(area);
        render_pane(
            &mut buffer,
            area,
            "地図",
            Theme::for_id(ThemeId::CatppuccinMocha),
            false,
            false,
            false,
            false,
            Duration::ZERO,
        );

        assert_eq!(buffer[(1, 0)].symbol(), "▐");
        assert_eq!(buffer[(3, 0)].symbol(), "地");
        assert_eq!(buffer[(4, 0)].symbol(), " ");
        assert_eq!(buffer[(5, 0)].symbol(), " ");
        assert_eq!(buffer[(6, 0)].symbol(), "▌");
        assert_eq!(buffer[(8, 0)].symbol(), "▜");
    }

    #[test]
    fn a_zwj_title_stays_whole_and_leaves_room_for_caps() {
        let title = "👩‍💻 map";
        assert_eq!(title_prefix_to_width(title, 2), "👩‍💻");
        assert_eq!(title_prefix_to_width(title, 1), "");

        let area = Rect::new(0, 0, 8, 5);
        let mut buffer = Buffer::empty(area);
        render_pane(
            &mut buffer,
            area,
            title,
            Theme::for_id(ThemeId::CatppuccinMocha),
            false,
            false,
            false,
            false,
            Duration::ZERO,
        );

        assert_eq!(buffer[(1, 0)].symbol(), "▐");
        assert_eq!(buffer[(3, 0)].symbol(), "👩‍💻");
        assert_eq!(buffer[(6, 0)].symbol(), "▌");
        assert_eq!(buffer[(7, 0)].symbol(), "▜");
    }

    #[test]
    fn halfwidth_voiced_katakana_uses_ratatui_width_without_splitting_its_grapheme() {
        let title = "ｶﾞ map";
        assert_eq!(title_prefix_to_width(title, 1), "");
        assert_eq!(title_prefix_to_width(title, 2), "ｶﾞ");

        let area = Rect::new(0, 0, 8, 5);
        let mut buffer = Buffer::empty(area);
        render_pane(
            &mut buffer,
            area,
            title,
            Theme::for_id(ThemeId::CatppuccinMocha),
            false,
            false,
            false,
            false,
            Duration::ZERO,
        );

        assert_eq!(buffer[(1, 0)].symbol(), "▐");
        assert_eq!(buffer[(3, 0)].symbol(), "ｶﾞ");
        assert_eq!(buffer[(4, 0)].symbol(), " ");
        assert_eq!(buffer[(5, 0)].symbol(), " ");
        assert_eq!(buffer[(6, 0)].symbol(), "▌");
        assert_eq!(buffer[(7, 0)].symbol(), "▜");
    }

    #[test]
    fn the_chip_runs_the_same_colour_cycle_as_the_frame_it_sits_in() {
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let cycle = derived_for(theme).0;
        let now = Duration::from_millis(533);
        let buffer = render(true, true, now, ThemeId::CatppuccinMocha);
        let step = cycle_step(now);
        let perimeter = border_len(buffer.area);

        for x in 2..8u16 {
            assert_eq!(
                buffer[(x, 0)].bg,
                cycle.at_perimeter(step, usize::from(x), perimeter),
                "chip cell {x} must continue the border's sequence, not restart it"
            );
        }

        let later = render(
            true,
            true,
            Duration::from_millis(933),
            ThemeId::CatppuccinMocha,
        );
        assert!(
            buffer[(2, 0)].bg != later[(2, 0)].bg,
            "the chip travels with the frame instead of anchoring it"
        );
    }

    #[test]
    fn an_inactive_pane_is_completely_still() {
        let early = render(false, true, Duration::ZERO, ThemeId::CatppuccinMocha);
        let late = render(
            false,
            true,
            Duration::from_millis(700),
            ThemeId::CatppuccinMocha,
        );
        assert!(
            early
                .content
                .iter()
                .zip(late.content.iter())
                .all(|(left, right)| left.fg == right.fg && left.bg == right.bg),
            "only the focused pane may move"
        );
        assert_eq!(
            early[(0, 0)].fg,
            Theme::for_id(ThemeId::CatppuccinMocha).border
        );
    }

    #[test]
    fn reduced_motion_pins_the_focused_frame_to_the_focus_accent() {
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let buffer = render(
            true,
            false,
            Duration::from_millis(900),
            ThemeId::CatppuccinMocha,
        );
        assert_eq!(buffer[(0, 4)].fg, derived_for(theme).0.at(0));
    }

    #[test]
    fn reduced_motion_focus_accent_clears_the_non_text_floor() {
        for id in [ThemeId::SolarizedLight, ThemeId::CatppuccinLatte] {
            let theme = Theme::for_id(id);
            let buffer = render(true, false, Duration::ZERO, id);
            assert!(
                contrast_ratio(buffer[(0, 4)].fg, theme.surface_panel)
                    .is_some_and(|ratio| ratio >= MIN_FOCUS_CONTRAST),
                "{id:?} reduced-motion focus border lost contrast"
            );
        }
    }

    #[test]
    fn ascii_panes_use_ascii_chrome_before_user_text_is_drawn() {
        let area = Rect::new(0, 0, 20, 5);
        let mut buffer = Buffer::empty(area);
        render_pane(
            &mut buffer,
            area,
            "PANE",
            Theme::for_id(ThemeId::CatppuccinMocha),
            false,
            false,
            false,
            true,
            Duration::ZERO,
        );
        assert_eq!(buffer[(0, 0)].symbol(), "+");
        assert!(
            buffer.content.iter().all(|cell| cell.symbol().is_ascii()),
            "ASCII pane emitted non-ASCII chrome"
        );
    }

    #[test]
    fn the_frame_walk_covers_each_border_cell_exactly_once() {
        let area = Rect::new(0, 0, 6, 4);
        let mut visits: Vec<(u16, u16)> = Vec::new();
        walk_border(area, |x, y, _| visits.push((x, y)));
        let expected = 2 * usize::from(area.width) + 2 * usize::from(area.height) - 4;
        assert_eq!(visits.len(), expected);
        let mut unique = visits.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), expected, "a cell was visited twice");
        assert_eq!(visits[0], (0, 0), "the walk starts at the top-left");
        assert_eq!(visits[1], (1, 0), "consecutive indices must be adjacent");
    }

    #[test]
    fn a_non_multiple_perimeter_closes_the_frame_cycle() {
        let area = Rect::new(0, 0, 80, 15);
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let cycle = derived_for(theme).0;
        let now = Duration::from_millis(533);
        let step = cycle_step(now);
        let perimeter = border_len(area);
        let mut buffer = Buffer::empty(area);
        render_pane(
            &mut buffer,
            area,
            "PANE",
            theme,
            true,
            true,
            false,
            false,
            now,
        );

        let mut last = (area.x, area.y);
        walk_border(area, |x, y, _| last = (x, y));
        assert_eq!(
            perimeter, 186,
            "the fixture must not align with the 44-sample loop"
        );
        assert_eq!(buffer[(0, 0)].fg, cycle.at_perimeter(step, 0, perimeter));
        assert_eq!(
            buffer[last].fg,
            cycle.at_perimeter(step, perimeter - 1, perimeter),
            "the trailing border cell must use the closed perimeter phase"
        );
        assert_eq!(
            cycle.at_perimeter(step, perimeter, perimeter),
            buffer[(0, 0)].fg,
            "the virtual cell after the perimeter must join the first"
        );
    }

    #[test]
    fn a_palette_terminal_inverts_the_chip_instead_of_measuring_contrast() {
        let buffer = render(true, true, Duration::ZERO, ThemeId::Monochrome);
        assert!(row_text(&buffer, 0).contains("STORAGE MAP"));
        assert!(buffer[(2, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn monochrome_active_pane_has_a_distinct_title_chip() {
        let active = render_with_monochrome(true, false, true, Duration::ZERO, ThemeId::Monochrome);
        let inactive =
            render_with_monochrome(false, false, true, Duration::ZERO, ThemeId::Monochrome);
        assert!(active[(3, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!inactive[(3, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn forced_monochrome_reverses_an_rgb_focused_chip() {
        let buffer =
            render_with_monochrome(true, false, true, Duration::ZERO, ThemeId::CatppuccinMocha);
        assert!(
            buffer[(2, 0)].modifier.contains(Modifier::REVERSED),
            "a forced monochrome RGB chip must retain explicit focus contrast"
        );
    }

    #[test]
    fn forced_monochrome_high_contrast_keeps_active_pane_distinct() {
        let active =
            render_with_monochrome(true, false, true, Duration::ZERO, ThemeId::HighContrast);
        let inactive =
            render_with_monochrome(false, false, true, Duration::ZERO, ThemeId::HighContrast);

        assert!(active[(3, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!inactive[(3, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn monochrome_modal_explicitly_inverts_its_collapsed_surface() {
        let theme = Theme::for_id(ThemeId::Monochrome);
        assert_eq!(theme.surface_raised, theme.surface_base);
        let full_area = Rect::new(0, 0, 20, 7);
        let modal_area = Rect::new(3, 2, 14, 3);
        let mut buffer = Buffer::empty(full_area);
        fill_pane(&mut buffer, full_area, theme);
        render_modal(&mut buffer, modal_area, "DIALOG", theme, theme.focus, false);

        let background = &buffer[(0, 0)];
        let modal_surface = &buffer[(10, 3)];
        assert_eq!(background.bg, modal_surface.bg);
        assert!(!background.modifier.contains(Modifier::REVERSED));
        assert!(
            modal_surface.modifier.contains(Modifier::REVERSED),
            "the modal must retain an explicit raised surface after monochrome normalization"
        );
    }

    #[test]
    fn modal_accents_clear_the_non_text_contrast_floor() {
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            if contrast_ratio(theme.surface_raised, theme.text_primary).is_none() {
                continue;
            }
            for accent in [
                theme.focus,
                theme.text_danger,
                theme.state_complete,
                theme.state_aggregated,
            ] {
                let area = Rect::new(0, 0, 30, 8);
                let mut buffer = Buffer::empty(area);
                render_modal(&mut buffer, area, "DIALOG", theme, accent, false);
                assert!(
                    contrast_ratio(buffer[(0, 0)].fg, theme.surface_raised)
                        .is_some_and(|ratio| ratio >= MIN_FOCUS_CONTRAST),
                    "{id:?} modal border lost contrast"
                );
            }
        }
    }

    #[test]
    fn static_light_theme_chips_use_the_strongest_available_ink() {
        let theme = Theme::for_id(ThemeId::ExciseLight);
        let buffer = render(false, false, Duration::ZERO, ThemeId::ExciseLight);
        let base_contrast = contrast_ratio(theme.border, theme.surface_base).expect("truecolour");
        let primary_contrast =
            contrast_ratio(theme.border, theme.text_primary).expect("truecolour");

        assert!(base_contrast > primary_contrast);
        assert_eq!(buffer[(2, 0)].fg, theme.surface_base);
    }

    #[test]
    fn sub_floor_latte_static_chips_use_a_neutral_title_ink() {
        let theme = Theme::for_id(ThemeId::CatppuccinLatte);
        let semantic_inks = [theme.text_primary, theme.surface_base];
        assert!(semantic_inks.into_iter().all(|ink| {
            contrast_ratio(theme.border, ink).expect("truecolour") < TITLE_CHIP_CONTRAST_FLOOR
        }));

        let buffer = render(false, false, Duration::ZERO, ThemeId::CatppuccinLatte);
        let chip = &buffer[(2, 0)];
        assert_eq!(chip.fg, Color::Rgb(u8::MAX, u8::MAX, u8::MAX));
        assert!(contrast_ratio(chip.bg, chip.fg).expect("truecolour") >= TITLE_CHIP_CONTRAST_FLOOR);
    }

    #[test]
    fn modal_body_text_uses_a_contrast_safe_ink() {
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            let Color::Rgb(surface_red, surface_green, surface_blue) = theme.surface_raised else {
                continue;
            };
            let Color::Rgb(ink_red, ink_green, ink_blue) =
                readable_text_on(theme, theme.surface_raised)
            else {
                continue;
            };
            assert!(
                contrast_ratio(
                    Color::Rgb(surface_red, surface_green, surface_blue),
                    Color::Rgb(ink_red, ink_green, ink_blue),
                )
                .is_some_and(|contrast| contrast >= TITLE_CHIP_CONTRAST_FLOOR),
                "{id:?} modal body text must clear the contrast floor"
            );
        }
    }
    #[test]
    fn title_caps_keep_each_container_surface() {
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let area = Rect::new(0, 0, 20, 5);
        let mut pane = Buffer::empty(area);
        render_pane(
            &mut pane,
            area,
            "PANE",
            theme,
            false,
            false,
            false,
            false,
            Duration::ZERO,
        );
        assert_eq!(pane[(1, 0)].bg, theme.surface_panel);
        assert_eq!(pane[(8, 0)].bg, theme.surface_panel);

        let mut modal = Buffer::empty(area);
        render_modal(&mut modal, area, "MODAL", theme, theme.focus, false);
        assert_eq!(modal[(1, 0)].bg, theme.surface_raised);
        assert_eq!(modal[(9, 0)].bg, theme.surface_raised);
    }

    fn tiled(theme: Theme, area: Rect) -> Buffer {
        // A map tile: a flat field of surface colour, exactly what a dialog
        // lands on when it opens over the treemap.
        let mut buffer = Buffer::empty(area);
        let tile = Style::default()
            .fg(theme.text_primary)
            .bg(theme.surface_raised)
            .add_modifier(Modifier::REVERSED);
        for position in area.positions() {
            buffer[position].set_symbol(" ").set_style(tile);
        }
        buffer
    }

    #[test]
    fn the_scrim_separates_a_modal_from_whatever_it_covers() {
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let area = Rect::new(0, 0, 20, 5);
        let mut buffer = tiled(theme, area);
        let covered = buffer[(10, 2)].clone();
        draw_scrim(&mut buffer, area, theme, false);
        let scrimmed = buffer[(10, 2)].clone();
        assert_ne!(
            covered.bg, scrimmed.bg,
            "the interface behind a dialog has to sink, or the dialog dissolves into it"
        );
        assert_ne!(
            scrimmed.bg, theme.surface_raised,
            "a scrimmed cell must not land back on the modal surface"
        );
        assert!(
            !scrimmed.modifier.contains(Modifier::REVERSED),
            "reversed ink behind a dialog reads brighter than the dialog"
        );
    }

    #[test]
    fn a_two_colour_terminal_flattens_the_layer_behind_a_modal() {
        let theme = Theme::for_id(ThemeId::Monochrome);
        let area = Rect::new(0, 0, 20, 5);
        let mut buffer = tiled(theme, area);
        draw_scrim(&mut buffer, area, theme, true);
        let scrimmed = buffer[(10, 2)].clone();
        assert_eq!(
            scrimmed.bg, theme.surface_base,
            "without colour the layer behind has to sit on the base surface, \
             so the monochrome pass leaves it uninverted"
        );
        assert!(
            !scrimmed.modifier.contains(Modifier::REVERSED),
            "the dialog must be the only inverted block on a two-colour screen"
        );
    }
}
