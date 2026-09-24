use std::time::Duration;

use ratatui::buffer::{Buffer, CellWidth as _};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border::Set;
use ratatui::text::Span;
use ratatui::widgets::{Block, Clear, Widget as _};
use unicode_segmentation::UnicodeSegmentation as _;

use tachyonfx::{Duration as FxDuration, fx};

use crate::theme::Theme;
use crate::ui::palette::{ColorCycle, MIN_FOCUS_CONTRAST, Oklch, cycle_step, derived_for};

/// One cell of breathing room between independent panes.
pub(crate) const PANE_GAP: u16 = 1;

/// How far the interface behind a modal sinks toward the base surface. Enough
/// to guarantee the dialog reads as a separate layer, short of erasing what it
/// interrupts: the map stays legible as context for the decision.
pub(crate) const SCRIM_SINK: f32 = 0.62;

/// Frame-only timing and accessibility state for a modal. Content widgets do
/// not receive it, so modal text never animates with its attention border.
#[derive(Clone, Copy)]
pub(crate) struct ModalChrome {
    now: Duration,
    animate: bool,
    monochrome: bool,
}

impl ModalChrome {
    #[must_use]
    pub(crate) const fn new(now: Duration, animate: bool, monochrome: bool) -> Self {
        Self {
            now,
            animate,
            monochrome,
        }
    }
}
/// A low-ink frame: a quadrant leading corner, hairline rules, and a reversed
/// padded title tab instead of a heavy box outline.
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

/// Draws a static workspace pane and returns its content area.
///
/// Only decision dialogs animate. A selected map entry carries the moving focus
/// cue, leaving persistent workspace chrome still and legible.
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
    monochrome: bool,
    ascii: bool,
) -> Rect {
    let accent = if active {
        contrast_safe_accent(theme, theme.surface_panel, theme.focus)
    } else {
        theme.border
    };
    let border_set = if ascii {
        ASCII_PANE_BORDER_SET
    } else {
        PANE_BORDER_SET
    };
    let title_tab = padded_title_tab(title, area, ascii);

    fill_pane(buffer, area, theme);
    let mut block = Block::bordered()
        .border_set(border_set)
        .style(Style::default().fg(accent));
    if let Some(title_tab) = title_tab {
        block = block.title(Span::styled(
            title_tab,
            padded_title_style(accent, theme.surface_panel, active, monochrome),
        ));
    }
    let inner = block.inner(area);
    block.render(area, buffer);
    restore_frame_backdrop(buffer, area, theme.surface_base);
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

/// A padded title tab sits in the top rule. The leading quadrant remains a
/// small, portable bevel. The top rule completes the title without a synthetic
/// trailing cap.
fn padded_title_tab(title: &str, area: Rect, ascii: bool) -> Option<String> {
    if title.is_empty() {
        return None;
    }
    let title = if ascii {
        title_prefix_to_width(title, usize::from(area.width.saturating_sub(4)))
    } else {
        title
    };
    if title.is_empty() {
        return None;
    }

    let mut title = title.to_owned();
    title.insert(0, ' ');
    title.push(' ');
    Some(title)
}

fn padded_title_style(accent: Color, surface: Color, active: bool, monochrome: bool) -> Style {
    let style = Style::default()
        .fg(accent)
        .bg(surface)
        .add_modifier(Modifier::BOLD);
    if !monochrome || active {
        style.add_modifier(Modifier::REVERSED)
    } else {
        style
    }
}

/// Restores the outer backdrop around the triangular leading cap and hairline
/// southern border.
fn restore_frame_backdrop(buffer: &mut Buffer, area: Rect, outer_surface: Color) {
    let border_south = area.rows().next_back().unwrap_or_default();

    for position in border_south.positions() {
        if let Some(cell) = buffer.cell_mut(position) {
            let style = cell.style();
            cell.set_style(style.bg(outer_surface));
        }
    }

    let top_left = area.as_position();
    if let Some(cell) = buffer.cell_mut(top_left) {
        let style = cell.style();
        cell.set_style(style.bg(outer_surface));
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

/// Text resolved against a styled surface must retain ordinary body-text contrast.
const TITLE_CHIP_CONTRAST_FLOOR: f32 = 4.5;

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
        // A reversed chip keeps its visible fill in the raw foreground. Resolve
        // that presentation before sinking it and removing reverse video.
        let (foreground, background) = if cell.modifier.contains(Modifier::REVERSED) {
            (style.bg, style.fg)
        } else {
            (style.fg, style.bg)
        };
        let (fg, bg) = if monochrome {
            (foreground.unwrap_or(theme.text_muted), theme.surface_base)
        } else {
            (
                sink(foreground, base, theme.text_muted, &mut sources),
                sink(background, base, theme.surface_base, &mut sources),
            )
        };
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
/// Dialogs alone own the travelling focus cycle. The effect runs after the
/// static frame but before the title tab, so `TachyonFX` advances the perimeter
/// foreground while decision text and its reversed tab stay still.
/// ASCII, monochrome, reduced-motion, and ANSI-only themes retain static chrome.
pub(crate) fn render_modal(
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    theme: Theme,
    accent: Color,
    ascii: bool,
    chrome: ModalChrome,
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
    let cycle =
        (chrome.animate && !ascii && !chrome.monochrome && ColorCycle::can_animate(theme.focus))
            .then(|| {
                let (cycle, _) = derived_for(theme);
                (cycle, cycle_step(chrome.now))
            });
    let border_accent = cycle
        .as_ref()
        .map_or(accent, |(cycle, step)| cycle.at(*step));
    let border_set = if ascii {
        ASCII_PANE_BORDER_SET
    } else {
        PANE_BORDER_SET
    };

    let title_tab = padded_title_tab(title, area, ascii);
    fill_surface(buffer, area, modal_style);
    let mut block = Block::bordered()
        .border_set(border_set)
        .style(Style::default().fg(border_accent));
    if let Some(title_tab) = title_tab {
        block = block.title(Span::styled(
            title_tab,
            padded_title_style(accent, theme.surface_raised, true, chrome.monochrome),
        ));
    }
    let inner = block.inner(area);
    block.render(area, buffer);
    restore_frame_backdrop(buffer, area, theme.surface_base);
    if let Some((cycle, step)) = cycle {
        animate_modal_border(buffer, area, cycle, step);
    }
    inner
}

/// An unbounded `TachyonFX` buffer callback advances a linear colour cycle at one
/// sample per border cell.
fn animate_modal_border(buffer: &mut Buffer, area: Rect, cycle: ColorCycle, step: usize) {
    let mut effect = fx::effect_fn_buf((), u32::MAX, move |(), _, buffer| {
        walk_border(area, |x, y, index| {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.fg = cycle.at(step.saturating_add(index));
            }
        });
    })
    .with_area(area);
    let _ = effect.process(FxDuration::ZERO, buffer, area);
}

fn fill_surface(buffer: &mut Buffer, area: Rect, style: Style) {
    for position in area.positions() {
        if let Some(cell) = buffer.cell_mut(position) {
            cell.set_symbol(" ").set_style(style);
        }
    }
}

pub(crate) fn fill_pane(buffer: &mut Buffer, area: Rect, theme: Theme) {
    fill_surface(buffer, area, Style::default().bg(theme.surface_panel));
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

    #[derive(Clone, Copy)]
    enum PanePresentation {
        Color,
        Monochrome,
        Ascii,
    }

    #[derive(Clone, Copy)]
    struct PaneRenderSettings {
        active: bool,
        presentation: PanePresentation,
        theme: ThemeId,
    }

    fn render(active: bool, animate: bool, now: Duration, theme: ThemeId) -> Buffer {
        render_with_monochrome(active, animate, false, now, theme)
    }

    fn render_with_monochrome(
        active: bool,
        _animate: bool,
        monochrome: bool,
        _now: Duration,
        theme: ThemeId,
    ) -> Buffer {
        render_with_capabilities(
            Rect::new(0, 0, 20, 5),
            "STORAGE MAP",
            PaneRenderSettings {
                active,
                presentation: if monochrome {
                    PanePresentation::Monochrome
                } else {
                    PanePresentation::Color
                },
                theme,
            },
        )
    }

    fn render_with_capabilities(area: Rect, title: &str, settings: PaneRenderSettings) -> Buffer {
        let (monochrome, ascii) = match settings.presentation {
            PanePresentation::Color => (false, false),
            PanePresentation::Monochrome => (true, false),
            PanePresentation::Ascii => (false, true),
        };
        let mut buffer = Buffer::empty(area);
        render_pane(
            &mut buffer,
            area,
            title,
            Theme::for_id(settings.theme),
            settings.active,
            monochrome,
            ascii,
        );
        buffer
    }

    #[test]
    fn title_chip_remains_readable_on_static_pane_chrome() {
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
    fn title_chip_ends_at_the_top_rule() {
        let area = Rect::new(0, 0, 20, 5);
        let buffer = render_with_capabilities(
            area,
            "PANE",
            PaneRenderSettings {
                active: false,
                presentation: PanePresentation::Color,
                theme: ThemeId::CatppuccinMocha,
            },
        );

        assert_eq!(buffer[(0, 0)].symbol(), "▟");
        assert_eq!(buffer[(6, 0)].symbol(), " ");
        assert!(buffer[(6, 0)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buffer[(7, 0)].symbol(), "▔");
    }

    #[test]
    fn a_wide_title_stays_inside_the_pane_border() {
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
        );

        assert_eq!(buffer[(0, 0)].symbol(), "▟");
        assert_eq!(buffer[(8, 0)].symbol(), "▜");
        assert_eq!(buffer[(2, 0)].symbol(), "地");
        assert_eq!(buffer[(4, 0)].symbol(), "図");
    }

    #[test]
    fn a_zwj_title_stays_whole_and_keeps_the_pane_corners() {
        let title = "👩‍💻 map";

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
        );

        assert_eq!(buffer[(0, 0)].symbol(), "▟");
        assert_eq!(buffer[(7, 0)].symbol(), "▜");
        assert!(row_text(&buffer, 0).contains("👩‍💻"));
    }

    #[test]
    fn halfwidth_voiced_katakana_stays_whole_inside_the_pane() {
        let title = "ｶﾞ map";

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
        );

        assert_eq!(buffer[(0, 0)].symbol(), "▟");
        assert_eq!(buffer[(7, 0)].symbol(), "▜");
        assert!(row_text(&buffer, 0).contains("ｶﾞ"));
    }

    #[test]
    fn active_pane_chip_and_border_stay_static() {
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let early = render(true, true, Duration::ZERO, ThemeId::CatppuccinMocha);
        let late = render(
            true,
            true,
            Duration::from_millis(933),
            ThemeId::CatppuccinMocha,
        );

        assert!(
            early
                .content
                .iter()
                .zip(late.content.iter())
                .all(|(left, right)| left.style() == right.style()),
            "ordinary pane chrome must not move when a selection is active"
        );
        assert_eq!(
            early[(0, 4)].fg,
            contrast_safe_accent(theme, theme.surface_panel, theme.focus)
        );
    }

    #[test]
    fn inactive_pane_is_completely_still() {
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
            "all ordinary panes must remain still"
        );
        assert_eq!(
            early[(0, 0)].fg,
            Theme::for_id(ThemeId::CatppuccinMocha).border
        );
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
    fn high_contrast_active_panes_stay_static_with_a_legible_title_chip() {
        let early = render(true, true, Duration::ZERO, ThemeId::HighContrast);
        let late = render(
            true,
            true,
            Duration::from_millis(933),
            ThemeId::HighContrast,
        );

        assert!(
            early
                .content
                .iter()
                .zip(late.content.iter())
                .all(|(left, right)| {
                    left.fg == right.fg && left.bg == right.bg && left.modifier == right.modifier
                }),
            "palette-only high-contrast chrome must not animate"
        );
        assert!(
            early[(3, 0)].modifier.contains(Modifier::REVERSED),
            "the static high-contrast chip must retain an explicit contrast treatment"
        );
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
            true,
        );
        assert_eq!(buffer[(0, 0)].symbol(), "+");
        assert!(
            buffer.content.iter().all(|cell| cell.symbol().is_ascii()),
            "ASCII pane emitted non-ASCII chrome"
        );
    }

    #[test]
    fn active_ascii_panes_hold_a_static_truncated_chip_inside_their_corners() {
        let area = Rect::new(0, 0, 7, 5);
        let early = render_with_capabilities(
            area,
            "WIDE",
            PaneRenderSettings {
                active: true,
                presentation: PanePresentation::Ascii,

                theme: ThemeId::CatppuccinMocha,
            },
        );
        let late = render_with_capabilities(
            area,
            "WIDE",
            PaneRenderSettings {
                active: true,
                presentation: PanePresentation::Ascii,

                theme: ThemeId::CatppuccinMocha,
            },
        );

        assert!(
            early
                .content
                .iter()
                .zip(late.content.iter())
                .all(|(left, right)| {
                    left.fg == right.fg && left.bg == right.bg && left.modifier == right.modifier
                }),
            "ASCII chrome must remain static"
        );
        assert_eq!(row_text(&early, 0), "+ WID +");
        assert!(
            early.content.iter().all(|cell| cell.symbol().is_ascii()),
            "ASCII chrome emitted a non-ASCII cell"
        );
    }

    #[test]
    fn main_title_truncation_preserves_content_before_padding() {
        let area = Rect::new(4, 2, 7, 5);
        let early = render_with_capabilities(
            area,
            "WIDE",
            PaneRenderSettings {
                active: true,
                presentation: PanePresentation::Color,
                theme: ThemeId::CatppuccinMocha,
            },
        );
        let later = render_with_capabilities(
            area,
            "WIDE",
            PaneRenderSettings {
                active: true,
                presentation: PanePresentation::Color,
                theme: ThemeId::CatppuccinMocha,
            },
        );

        assert_eq!(row_text(&early, area.y), "▟ WIDE▜");
        assert!(
            early
                .content
                .iter()
                .zip(later.content.iter())
                .all(|(left, right)| left.style() == right.style()),
            "a truncated ordinary pane chip must stay static"
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
    fn a_non_multiple_pane_perimeter_stays_static() {
        let area = Rect::new(0, 0, 80, 15);
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let early = render_with_capabilities(
            area,
            "",
            PaneRenderSettings {
                active: true,
                presentation: PanePresentation::Color,
                theme: ThemeId::CatppuccinMocha,
            },
        );
        let later = render_with_capabilities(
            area,
            "",
            PaneRenderSettings {
                active: true,
                presentation: PanePresentation::Color,
                theme: ThemeId::CatppuccinMocha,
            },
        );

        assert!(
            early
                .content
                .iter()
                .zip(later.content.iter())
                .all(|(left, right)| left.style() == right.style()),
            "an ordinary pane perimeter must not advance with time"
        );
        assert_eq!(
            early[(0, 0)].fg,
            contrast_safe_accent(theme, theme.surface_panel, theme.focus)
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
    fn forced_monochrome_focus_chrome_stays_static_and_reversed() {
        let early =
            render_with_monochrome(true, true, true, Duration::ZERO, ThemeId::CatppuccinMocha);
        let late = render_with_monochrome(
            true,
            true,
            true,
            Duration::from_millis(933),
            ThemeId::CatppuccinMocha,
        );

        assert!(
            early
                .content
                .iter()
                .zip(late.content.iter())
                .all(|(left, right)| {
                    left.fg == right.fg && left.bg == right.bg && left.modifier == right.modifier
                }),
            "forced monochrome chrome must not advance its truecolour phase"
        );
        assert!(
            early[(2, 0)].modifier.contains(Modifier::REVERSED),
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
        render_modal(
            &mut buffer,
            modal_area,
            "DIALOG",
            theme,
            theme.focus,
            false,
            ModalChrome::new(Duration::ZERO, false, false),
        );

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
                theme.state_attention,
            ] {
                let area = Rect::new(0, 0, 30, 8);
                let mut buffer = Buffer::empty(area);
                render_modal(
                    &mut buffer,
                    area,
                    "DIALOG",
                    theme,
                    accent,
                    false,
                    ModalChrome::new(Duration::ZERO, false, false),
                );
                assert!(
                    contrast_ratio(buffer[(0, 0)].fg, theme.surface_raised)
                        .is_some_and(|ratio| ratio >= MIN_FOCUS_CONTRAST),
                    "{id:?} modal border lost contrast"
                );
            }
        }
    }

    #[test]
    fn modal_attention_walks_the_border_and_title_chip() {
        let area = Rect::new(0, 0, 24, 7);
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let body_style = Style::default()
            .fg(theme.text_primary)
            .bg(theme.surface_raised);
        let mut first = Buffer::empty(area);
        render_modal(
            &mut first,
            area,
            "DIALOG",
            theme,
            theme.focus,
            false,
            ModalChrome::new(Duration::ZERO, true, false),
        );
        first.set_string(4, 3, "DECIDE", body_style);
        let mut later = Buffer::empty(area);
        render_modal(
            &mut later,
            area,
            "DIALOG",
            theme,
            theme.focus,
            false,
            ModalChrome::new(Duration::from_millis(533), true, false),
        );
        later.set_string(4, 3, "DECIDE", body_style);

        assert_ne!(first[(0, 0)].fg, later[(0, 0)].fg);
        assert!(
            first
                .content
                .iter()
                .zip(later.content.iter())
                .all(|(first, later)| first.bg == later.bg),
            "modal attention must not issue changing terminal background colours"
        );
        assert_ne!(
            first[(2, 0)].fg,
            later[(2, 0)].fg,
            "the walking frame must carry the title chip with it"
        );
        assert_eq!(
            first[(2, 0)].bg,
            later[(2, 0)].bg,
            "the title chip's raw surface stays fixed while its visible reversed fill walks"
        );
        for x in 4..10 {
            assert_eq!(first[(x, 3)].symbol(), later[(x, 3)].symbol());
            assert_eq!(first[(x, 3)].style(), later[(x, 3)].style());
        }

        for (ascii, monochrome) in [(true, false), (false, true)] {
            let mut static_first = Buffer::empty(area);
            let mut static_later = Buffer::empty(area);
            render_modal(
                &mut static_first,
                area,
                "DIALOG",
                theme,
                theme.focus,
                ascii,
                ModalChrome::new(Duration::ZERO, true, monochrome),
            );
            render_modal(
                &mut static_later,
                area,
                "DIALOG",
                theme,
                theme.focus,
                ascii,
                ModalChrome::new(Duration::from_millis(533), true, monochrome),
            );
            assert_eq!(static_first[(0, 0)].style(), static_later[(0, 0)].style());
        }

        let high_contrast = Theme::for_id(ThemeId::HighContrast);
        let mut contrast_first = Buffer::empty(area);
        let mut contrast_later = Buffer::empty(area);
        render_modal(
            &mut contrast_first,
            area,
            "DIALOG",
            high_contrast,
            high_contrast.focus,
            false,
            ModalChrome::new(Duration::ZERO, true, false),
        );
        render_modal(
            &mut contrast_later,
            area,
            "DIALOG",
            high_contrast,
            high_contrast.focus,
            false,
            ModalChrome::new(Duration::from_millis(533), true, false),
        );
        assert_eq!(
            contrast_first[(0, 0)].style(),
            contrast_later[(0, 0)].style(),
            "ANSI high-contrast output must not animate"
        );
    }

    #[test]
    fn title_uses_a_reversed_border_accent_in_light_themes() {
        let theme = Theme::for_id(ThemeId::ExciseLight);
        let buffer = render(false, false, Duration::ZERO, ThemeId::ExciseLight);
        let chip = &buffer[(2, 0)];

        assert_eq!(chip.fg, theme.border);
        assert_eq!(chip.bg, theme.surface_panel);
        assert!(chip.modifier.contains(Modifier::BOLD | Modifier::REVERSED));
    }

    #[test]
    fn title_keeps_its_border_accent_without_neutral_substitution() {
        let theme = Theme::for_id(ThemeId::CatppuccinLatte);
        let buffer = render(false, false, Duration::ZERO, ThemeId::CatppuccinLatte);
        let chip = &buffer[(2, 0)];

        assert_eq!(chip.fg, theme.border);
        assert_eq!(chip.bg, theme.surface_panel);
        assert!(chip.modifier.contains(Modifier::BOLD | Modifier::REVERSED));
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
    fn frame_keeps_the_outer_background_outside_each_pane() {
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let area = Rect::new(0, 0, 20, 5);
        let mut pane = Buffer::empty(area);
        render_pane(&mut pane, area, "PANE", theme, false, false, false);
        assert_eq!(pane[(0, 0)].bg, theme.surface_base);
        assert!(
            (area.x..area.right()).all(|x| pane[(x, area.bottom() - 1)].bg == theme.surface_base),
            "the hairline bottom border must sit on the outer surface"
        );
        assert_eq!(pane[(1, 1)].bg, theme.surface_panel);

        let mut modal = Buffer::empty(area);
        render_modal(
            &mut modal,
            area,
            "MODAL",
            theme,
            theme.focus,
            false,
            ModalChrome::new(Duration::ZERO, false, false),
        );
        assert_eq!(modal[(0, 0)].bg, theme.surface_base);
        assert!(
            (area.x..area.right()).all(|x| modal[(x, area.bottom() - 1)].bg == theme.surface_base),
            "the modal bottom border must sit on the outer surface"
        );
        assert_eq!(modal[(1, 1)].bg, theme.surface_raised);
    }

    #[test]
    fn scrim_keeps_a_reversed_title_tab_as_a_muted_background() {
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let area = Rect::new(0, 0, 20, 5);
        let mut pane = Buffer::empty(area);
        render_pane(&mut pane, area, "PANE", theme, false, false, false);
        let title = pane[(2, 0)].clone();
        assert!(title.modifier.contains(Modifier::REVERSED));

        draw_scrim(&mut pane, area, theme, false);

        let base = Oklch::from_color(theme.surface_base);
        let mut sources = ScrimSourceCache::default();
        let expected_foreground = sink(Some(title.bg), base, theme.text_muted, &mut sources);
        let expected_background = sink(Some(title.fg), base, theme.surface_base, &mut sources);
        let scrimmed = &pane[(2, 0)];
        assert_eq!(scrimmed.fg, expected_foreground);
        assert_eq!(scrimmed.bg, expected_background);
        assert!(!scrimmed.modifier.contains(Modifier::REVERSED));
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
