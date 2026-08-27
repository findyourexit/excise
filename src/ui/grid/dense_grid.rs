use std::collections::BTreeMap;

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::model::SyntheticKind;
use crate::native_path::SafeDisplayPath;
use crate::state::tiles::{FileType, HALF_ROWS_PER_CELL, MapOverflow, Tile};
use crate::theme::Theme;
use crate::ui::format::{DisplaySize, display_os_str_info, truncate_marked, truncate_middle};
use crate::ui::palette::{
    Emphasis, MapPalette, TILE_BASE_DROP, TILE_CROWN_LIFT, TILE_EDGE_DROP, TILE_SELECTED_BASE_DROP,
    TILE_SELECTED_EDGE_DROP, TileTone, derived_for,
};

/// Composite cell: the upper half takes the foreground colour, the lower half
/// the background. Cells whose halves agree collapse to a space so that flat
/// interiors never show the hairline seam some fonts leave between half-blocks.
const HALF_CELL: &str = "▀";
/// Shading ramp for terminals or themes that cannot supply truecolour.
const SHADES: [&str; 4] = ["░", "▒", "▓", "█"];
const ASCII_SHADES: [&str; 4] = ["-", "=", "+", "#"];
/// Non-colour boundary cues retain map structure when every entry shares a shade.
const SHADED_TOP: &str = "▔";
const SHADED_EDGE: &str = "▕";
const SHADED_TOP_EDGE: &str = "┐";
const ASCII_SHADED_TOP: &str = "^";
const ASCII_SHADED_EDGE: &str = "|";
const ASCII_SHADED_TOP_EDGE: &str = "+";

/// A reverse-video space exposes selection when terminal colours are reset.
const MONOCHROME_SELECTED_SHADE: &str = " ";
/// Perceptual offsets that emboss each entry, separating neighbours without
/// spending a single cell on a gap.
const CROWN_LIFT: f32 = TILE_CROWN_LIFT;
const BASE_DROP: f32 = TILE_BASE_DROP;
const EDGE_DROP: f32 = TILE_EDGE_DROP;
/// Columns an entry needs before it is worth labelling.
const MINIMUM_LABEL_WIDTH: u16 = 6;

/// The geometry one frame of the map is drawn from.
///
/// Every field arrives from the same layout pass, so they travel together
/// rather than as loose arguments that could disagree with each other.
#[derive(Clone, Copy)]
pub struct MapLayout<'a> {
    pub rectangles: &'a [Tile],
    /// Entries the layout is leaving behind, still receding on screen.
    pub departing: &'a [Tile],
    pub overflow: Option<MapOverflow>,
    pub selected_rect_index: Option<usize>,
    /// Whether entries are still moving toward their targets. While they are,
    /// they may overlap, which decides whether a covered entry may be labelled.
    pub transitioning: bool,
    /// Whether an empty layout has been confirmed by the model as truly empty.
    pub show_empty_label: bool,
}

/// A densely tessellated treemap.
///
/// Geometry arrives in half-rows (see [`Tile`]), so one terminal cell can carry
/// two different entries. Nothing is inset and nothing is padded: the map is a
/// continuous surface, and structure comes from shading rather than from gaps.
#[allow(
    clippy::struct_excessive_bools,
    reason = "map presentation flags are independent rendering capabilities"
)]
pub struct DenseRectangleGrid<'a> {
    rectangles: &'a [Tile],
    departing: &'a [Tile],
    overflow: Option<MapOverflow>,
    selected_rect_index: Option<usize>,
    theme: Theme,
    ascii: bool,
    monochrome: bool,
    transitioning: bool,
    show_empty_label: bool,
}

impl<'a> DenseRectangleGrid<'a> {
    #[must_use]
    pub const fn new(layout: MapLayout<'a>, theme: Theme, ascii: bool, monochrome: bool) -> Self {
        Self {
            rectangles: layout.rectangles,
            departing: layout.departing,
            overflow: layout.overflow,
            selected_rect_index: layout.selected_rect_index,
            theme,
            ascii,
            monochrome,
            transitioning: layout.transitioning,
            show_empty_label: layout.show_empty_label,
        }
    }

    /// Where one entry sits relative to the cursor. With nothing selected the
    /// whole map rests: dimming only means something once there is a cursor.
    fn emphasis(&self, index: usize) -> Emphasis {
        match self.selected_rect_index {
            None => Emphasis::Resting,
            Some(selected) if selected == index => Emphasis::Selected,
            Some(_) => Emphasis::Unselected,
        }
    }
    /// The selected entry stays on top wherever terminal-cell rasterization makes
    /// half-row siblings share a cell. That remains true after a tween settles,
    /// so an odd boundary cannot hand its cell back to an unselected sibling.
    fn selected_paint_index(&self) -> Option<usize> {
        self.selected_rect_index
            .filter(|&index| index < self.rectangles.len())
    }

    fn ink(
        &self,
        tile: &Tile,
        palette: MapPalette,
        emphasis: Emphasis,
        scale: HeatScale,
    ) -> TileInk {
        TileInk::resolve(tile, self.theme, palette, emphasis, scale)
    }

    fn render_composited(
        &self,
        area: Rect,
        buffer: &mut Buffer,
        palette: MapPalette,
        label_occlusions: Option<&[bool]>,
        prepared_labels: Option<&[Option<TileLabel>]>,
        selected_last: Option<usize>,
    ) {
        let backdrop = palette.backdrop();
        for position in area.positions() {
            buffer[position]
                .set_symbol(HALF_CELL)
                .set_style(Style::default().fg(backdrop).bg(backdrop));
        }

        // The folder being left sits beneath the incoming layout. While geometry
        // moves, walk the same stack from front to back and rasterize only each
        // tile's exposed half-rows; that has the normal paint result without
        // repainting a pivot once for every child that starts inside it.
        let departing_scale = HeatScale::for_ramp_tiles(self.departing);
        let scale = HeatScale::for_ramp_tiles(self.rectangles);
        if self.transitioning {
            let mut covered = HalfRowCoverage::new();
            for (index, tile) in tile_paint_order(self.rectangles, selected_last).rev() {
                let ink = self.ink(tile, palette, self.emphasis(index), scale);
                paint_visible_tile(buffer, area, tile, &ink, &mut covered);
            }
            for tile in self.departing.iter().rev() {
                let ink = self.ink(tile, palette, Emphasis::Unselected, departing_scale);
                paint_visible_tile(buffer, area, tile, &ink, &mut covered);
            }
        } else {
            for tile in self.departing {
                let ink = self.ink(tile, palette, Emphasis::Unselected, departing_scale);
                paint_tile(buffer, area, tile, &ink);
            }
            for (index, tile) in tile_paint_order(self.rectangles, selected_last) {
                let ink = self.ink(tile, palette, self.emphasis(index), scale);
                paint_tile(buffer, area, tile, &ink);
            }
        }

        let overflow_area = self
            .overflow
            .and_then(|overflow| overflow_region(area, overflow));
        collapse_flat_cells(buffer, area, overflow_area, palette.grain(), backdrop);

        for (index, tile) in tile_paint_order(self.rectangles, selected_last) {
            if label_occlusions.is_some_and(|occlusions| occlusions[index]) {
                continue;
            }
            if let Some(labels) = prepared_labels {
                let Some(label) = labels[index].as_ref() else {
                    continue;
                };
                let ink = self.ink(tile, palette, self.emphasis(index), scale);
                draw_prepared_tile_label(
                    buffer, area, tile, label, ink.fill, ink.text, ink.detail, false,
                );
            } else {
                let ink = self.ink(tile, palette, self.emphasis(index), scale);
                draw_tile_label(
                    buffer, area, tile, ink.fill, ink.text, ink.detail, self.ascii, false,
                );
            }
        }

        if let Some(overflow) = self.overflow
            && let Some(overflow_area) = overflow_area
        {
            draw_overflow_label(
                buffer,
                overflow_area,
                overflow,
                self.theme,
                backdrop,
                self.ascii,
            );
        }
    }

    /// Fallback for monochrome, high-contrast, and ASCII presentation, where a
    /// perceptual colour band is unavailable. Entries are separated by shading
    /// density instead of hue, which survives a two-colour terminal intact.
    #[allow(
        clippy::too_many_lines,
        reason = "shaded rendering keeps both settled and transition paths together"
    )]
    fn render_shaded(
        &self,
        area: Rect,
        buffer: &mut Buffer,
        label_occlusions: Option<&[bool]>,
        prepared_labels: Option<&[Option<TileLabel>]>,
        selected_last: Option<usize>,
    ) {
        let shades = if self.ascii { ASCII_SHADES } else { SHADES };
        let surface = self.theme.map_surface();
        for position in area.positions() {
            buffer[position]
                .set_symbol(" ")
                .set_style(Style::default().fg(self.theme.text_muted).bg(surface));
        }
        let ink = ShadedInk {
            shades: &shades,
            surface,
            text: self.theme.text_primary,
        };
        // Density stands in for hue here, and it carries the same meaning: the
        // larger the entry, the more solid its shading. Unlike the chromatic
        // ramp, density has no semantic-state role, so every tile stays in its fit.
        // Departing entries are fitted separately so the incoming layout does not jump.
        let departing_scale = HeatScale::for_tiles(self.departing);
        let scale = HeatScale::for_tiles(self.rectangles);
        if self.transitioning {
            let mut covered = TerminalRowCoverage::new();
            for (index, tile) in tile_paint_order(self.rectangles, selected_last).rev() {
                let selected = self.selected_rect_index == Some(index);
                paint_visible_shaded_tile(buffer, area, tile, ink, selected, scale, &mut covered);
            }
            for tile in self.departing.iter().rev() {
                paint_visible_shaded_tile(
                    buffer,
                    area,
                    tile,
                    ink,
                    false,
                    departing_scale,
                    &mut covered,
                );
            }
        } else {
            for tile in self.departing {
                paint_shaded_tile(buffer, area, tile, ink, false, departing_scale);
            }
            for (index, tile) in tile_paint_order(self.rectangles, selected_last) {
                let selected = self.selected_rect_index == Some(index);
                paint_shaded_tile(buffer, area, tile, ink, selected, scale);
            }
        }
        let overflow_area = self
            .overflow
            .and_then(|overflow| overflow_region(area, overflow));
        if let Some(region) = overflow_area {
            // The shading ramp is spent on the entries themselves, so the
            // remainder keeps the speck field: without colour it is the one mark
            // that cannot be mistaken for an entry of its own.
            let grain = if self.ascii { "." } else { "·" };
            for position in area.positions() {
                if region.contains(position.x, position.y)
                    && let Some(cell) = buffer.cell_mut(position)
                    && cell.symbol() == " "
                {
                    cell.set_symbol(grain);
                }
            }
        }
        let text = self.theme.text_primary;
        let detail = self.theme.text_secondary;
        for (index, tile) in tile_paint_order(self.rectangles, selected_last) {
            if label_occlusions.is_some_and(|occlusions| occlusions[index]) {
                continue;
            }
            let selected = self.selected_rect_index == Some(index);
            let background = if selected {
                self.theme.surface_selection
            } else {
                surface
            };
            let (label_text, label_detail) = if selected {
                (self.theme.text_inverse, self.theme.text_inverse)
            } else {
                (text, detail)
            };
            let preserve_reverse =
                selected && self.theme.surface_selection == self.theme.surface_base;
            if let Some(labels) = prepared_labels {
                let Some(label) = labels[index].as_ref() else {
                    continue;
                };
                draw_prepared_tile_label(
                    buffer,
                    area,
                    tile,
                    label,
                    background,
                    label_text,
                    label_detail,
                    preserve_reverse,
                );
            } else {
                draw_tile_label(
                    buffer,
                    area,
                    tile,
                    background,
                    label_text,
                    label_detail,
                    self.ascii,
                    preserve_reverse,
                );
            }
        }

        if let Some(overflow) = self.overflow
            && let Some(overflow_area) = overflow_area
        {
            draw_overflow_label(
                buffer,
                overflow_area,
                overflow,
                self.theme,
                surface,
                self.ascii,
            );
        }
    }
}

impl Widget for DenseRectangleGrid<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        // A terminal with no colour cannot carry a perceptual band: shading
        // density is the only channel left, and it keeps the map textured
        // instead of collapsing every entry into one inverted block.
        let palette = if self.ascii || self.monochrome {
            None
        } else {
            derived_for(self.theme).1
        };
        // Entries that never earned a cell still exist: only a folder with
        // nothing in it at all may say so, or the map calls thousands of small
        // files an empty directory.
        if self.rectangles.is_empty() && self.departing.is_empty() && self.overflow.is_none() {
            draw_empty_surface(
                buffer,
                area,
                self.theme,
                palette,
                self.ascii,
                self.show_empty_label,
            );
            return;
        }
        let selected_last = self.selected_paint_index();
        let transition_labels = self
            .transitioning
            .then(|| label_occlusions(area, self.rectangles, selected_last, self.ascii));
        let occluded_labels = transition_labels
            .as_ref()
            .map(|labels| labels.occlusions.as_slice());
        let prepared_labels = transition_labels
            .as_ref()
            .map(|labels| labels.labels.as_slice());
        match palette {
            Some(palette) => self.render_composited(
                area,
                buffer,
                palette,
                occluded_labels,
                prepared_labels,
                selected_last,
            ),
            None => self.render_shaded(
                area,
                buffer,
                occluded_labels,
                prepared_labels,
                selected_last,
            ),
        }
    }
}

/// Semantic entries describe a storage state rather than a point on the size
/// ramp, so they never consume an endpoint intended for ordinary entries.
fn is_ramp_eligible(tile: &Tile) -> bool {
    !tile.uncertain && tile.synthetic_kind.is_none()
}

/// Every colour one entry needs, resolved once per frame rather than per cell.
struct TileInk {
    fill: Color,
    crown: Color,
    base: Color,
    edge: Color,
    text: Color,
    detail: Color,
}

impl TileInk {
    fn resolve(
        tile: &Tile,
        theme: Theme,
        palette: MapPalette,
        emphasis: Emphasis,
        scale: HeatScale,
    ) -> Self {
        let tone = match tile.file_type {
            FileType::Folder => TileTone::Folder,
            FileType::File | FileType::Synthetic => TileTone::File,
        };
        let resting = if is_ramp_eligible(tile) {
            palette.tile(scale.of(tile.size), tone)
        } else if tile.uncertain {
            palette.semantic(theme.state_uncertain)
        } else if tile.synthetic_kind == Some(SyntheticKind::Shared) {
            palette.semantic(theme.state_shared)
        } else {
            palette.semantic(theme.state_aggregated)
        };
        let resting = palette.emphasised(resting, emphasis);
        let (text, detail) = resting.inks();
        let base_drop = if emphasis == Emphasis::Selected {
            TILE_SELECTED_BASE_DROP
        } else {
            BASE_DROP
        };
        let edge_drop = if emphasis == Emphasis::Selected {
            TILE_SELECTED_EDGE_DROP
        } else {
            EDGE_DROP
        };
        Self {
            fill: resting.to_color(),
            crown: resting.shifted(CROWN_LIFT, 1.0).to_color(),
            base: resting.shifted(-base_drop, 1.0).to_color(),
            edge: resting.shifted(-edge_drop, 1.0).to_color(),
            text,
            detail,
        }
    }
}

/// The ramp fitted to one folder: where each entry sits between the smallest
/// and the largest thing drawn beside it.
///
/// Sizes are heavy-tailed — one entry routinely holds more than everything
/// around it put together — so positions are taken in log space. Measuring
/// against both ends rather than the largest alone means the whole ramp is
/// spent on the folder in front of the reader instead of collapsing into the
/// cold end whenever one entry dominates.
#[derive(Clone, Copy)]
struct HeatScale {
    coldest: f32,
    span: f32,
}

impl HeatScale {
    /// Nothing to compare — a lone entry, or a folder of one size — rests in
    /// the middle of the ramp rather than claiming either extreme.
    const NEUTRAL: f32 = 0.5;

    fn neutral() -> Self {
        Self {
            coldest: 0.0,
            span: 0.0,
        }
    }

    fn for_tiles(tiles: &[Tile]) -> Self {
        Self::for_sizes(tiles.iter().map(|tile| tile.size))
    }

    fn for_ramp_tiles(tiles: &[Tile]) -> Self {
        Self::for_sizes(
            tiles
                .iter()
                .filter(|tile| is_ramp_eligible(tile))
                .map(|tile| tile.size),
        )
    }

    fn for_sizes(sizes: impl Iterator<Item = u128>) -> Self {
        let mut positions = sizes.map(log_size);
        let Some(first) = positions.next() else {
            return Self::neutral();
        };
        let mut coldest = first;
        let mut hottest = first;
        for position in positions {
            coldest = coldest.min(position);
            hottest = hottest.max(position);
        }
        Self {
            coldest,
            span: hottest - coldest,
        }
    }

    fn of(self, size: u128) -> f32 {
        if self.span <= f32::EPSILON {
            // One comparable entry, or several equal ones, genuinely has no
            // ordering to express and deliberately rests at the ramp midpoint.
            return Self::NEUTRAL;
        }
        ((log_size(size) - self.coldest) / self.span).clamp(0.0, 1.0)
    }
}

/// Makes the zero-byte endpoint finite while keeping it distinct from one byte.
/// The neutral branch above is therefore reserved for equal comparable sizes,
/// not for a logarithm that happened to merge two different entries.
fn log_size(size: u128) -> f32 {
    (size as f64).ln_1p() as f32
}

/// The presentation a shaded frame shares across every entry, resolved once so
/// the per-tile call carries only what varies from one entry. Mirrors
/// [`TileInk`], which does the same for the composited path.
#[derive(Clone, Copy)]
struct ShadedInk<'a> {
    shades: &'a [&'a str; 4],
    surface: Color,
    text: Color,
}

/// Each row stores merged horizontal spans that an entry above it already owns.
/// The key means half-rows for composited painting and terminal rows for shaded
/// painting and label occlusion.
#[derive(Default)]
struct RasterCoverage {
    rows: BTreeMap<u32, BTreeMap<u16, u16>>,
    /// Rectangles whose clipped footprints are entirely owned after a tile has
    /// been processed. Initial drill frames often stack every incoming tile on
    /// one pivot; an exact region hit avoids probing every raster row again.
    fully_covered_regions: BTreeMap<(u16, u16), BTreeMap<u32, u32>>,
}

type HalfRowCoverage = RasterCoverage;
type TerminalRowCoverage = RasterCoverage;

impl RasterCoverage {
    fn new() -> Self {
        Self::default()
    }

    fn has_remembered_fully_covered_region(
        &self,
        top: u32,
        bottom: u32,
        left: u16,
        right: u16,
    ) -> bool {
        self.fully_covered_regions
            .get(&(left, right))
            .and_then(|regions| {
                regions
                    .range(..=top)
                    .next_back()
                    .map(|(_, &covered_bottom)| covered_bottom)
            })
            .is_some_and(|covered_bottom| covered_bottom >= bottom)
    }

    fn region_is_fully_covered(&self, top: u32, bottom: u32, left: u16, right: u16) -> bool {
        self.has_remembered_fully_covered_region(top, bottom, left, right)
            || raster_region_is_covered(&self.rows, top, bottom, left, right)
    }

    /// Adds an opaque clipped footprint unless an earlier tile already owns the
    /// exact region. Labels use this cache-only fast path rather than walking
    /// every row of each pivot just to rediscover the same coverage.
    fn cover_region_if_needed(&mut self, top: u32, bottom: u32, left: u16, right: u16) -> bool {
        if top >= bottom
            || left >= right
            || self.has_remembered_fully_covered_region(top, bottom, left, right)
        {
            return false;
        }
        for row in top..bottom {
            insert_covered_interval(self.rows.entry(row).or_default(), left, right);
        }
        self.remember_fully_covered_region(top, bottom, left, right);
        true
    }

    fn remember_fully_covered_region(&mut self, top: u32, bottom: u32, left: u16, right: u16) {
        if top < bottom && left < right {
            insert_covered_interval(
                self.fully_covered_regions.entry((left, right)).or_default(),
                top,
                bottom,
            );
        }
    }
}

/// Logical paint order: source order, except that the selected entry rises above
/// its siblings. Reversing this iterator walks visible z-order front first.
fn tile_paint_order(
    tiles: &[Tile],
    selected_last: Option<usize>,
) -> impl DoubleEndedIterator<Item = (usize, &Tile)> {
    let selected_last = selected_last.filter(|&index| index < tiles.len());
    tiles
        .iter()
        .enumerate()
        .filter(move |(index, _)| Some(*index) != selected_last)
        .chain(
            selected_last
                .into_iter()
                .map(move |index| (index, &tiles[index])),
        )
}

fn shaded_tile_style(ink: ShadedInk<'_>, selected: bool) -> Style {
    let style = Style::default().fg(ink.text).bg(ink.surface);
    if selected {
        // A full block must keep its own foreground in reverse-video fallbacks;
        // otherwise it becomes the map surface and disappears. Reset-valued
        // monochrome themes still need reverse video for a non-colour cue.
        if ink.surface == Color::Reset {
            style.add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            style.add_modifier(Modifier::BOLD)
        }
    } else {
        // `Cell::set_style` patches modifiers. Clear selection state explicitly
        // when an unselected tile replaces a selected one during an overlap.
        style.remove_modifier(Modifier::BOLD | Modifier::REVERSED)
    }
}

/// `Rect::bottom` saturates at `u16::MAX`; map-space geometry must instead
/// retain the full origin-plus-extent before it reaches the terminal buffer.
fn area_bottom_row(area: Rect) -> u32 {
    u32::from(area.y) + u32::from(area.height)
}

fn area_bottom_half_row(area: Rect) -> u32 {
    area_bottom_row(area) * u32::from(HALF_ROWS_PER_CELL)
}

fn clipped_terminal_rows(area: Rect, tile: &Tile) -> Option<(u32, u32)> {
    let top = tile.top_row().max(u32::from(area.y));
    let bottom = tile.bottom_row().min(area_bottom_row(area));
    (top < bottom).then_some((top, bottom))
}

/// Paints one entry with density when a terminal cannot carry the heat ramp.
fn paint_shaded_tile(
    buffer: &mut Buffer,
    area: Rect,
    tile: &Tile,
    ink: ShadedInk<'_>,
    selected: bool,
    scale: HeatScale,
) {
    let fill = if selected && ink.surface == Color::Reset {
        MONOCHROME_SELECTED_SHADE
    } else if selected {
        ink.shades[3]
    } else {
        ink.shades[shade_index(scale, tile.size)]
    };
    let left = tile.x.max(area.x);
    let right = tile.x.saturating_add(tile.width).min(area.right());
    let Some((top, bottom)) = clipped_terminal_rows(area, tile) else {
        return;
    };
    if left >= right {
        return;
    }
    for row in top..bottom {
        let Ok(y) = u16::try_from(row) else {
            continue;
        };
        for x in left..right {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_symbol(shaded_fill_symbol(ink, selected, tile, x, row, fill))
                    .set_style(shaded_tile_style(ink, selected));
            }
        }
    }
}

/// Front-to-back variant used only while rectangles overlap. It leaves covered
/// spans untouched, exactly as a later opaque paint would have done.
fn paint_visible_shaded_tile(
    buffer: &mut Buffer,
    area: Rect,
    tile: &Tile,
    ink: ShadedInk<'_>,
    selected: bool,
    scale: HeatScale,
    covered: &mut TerminalRowCoverage,
) -> bool {
    let fill = if selected && ink.surface == Color::Reset {
        MONOCHROME_SELECTED_SHADE
    } else if selected {
        ink.shades[3]
    } else {
        ink.shades[shade_index(scale, tile.size)]
    };
    let style = shaded_tile_style(ink, selected);
    let left = tile.x.max(area.x);
    let right = tile.x.saturating_add(tile.width).min(area.right());
    let Some((top, bottom)) = clipped_terminal_rows(area, tile) else {
        return false;
    };
    if left >= right {
        return false;
    }
    if covered.region_is_fully_covered(top, bottom, left, right) {
        covered.remember_fully_covered_region(top, bottom, left, right);
        return false;
    }
    let mut painted = false;
    for row in top..bottom {
        let Ok(y) = u16::try_from(row) else {
            continue;
        };
        let ranges = covered.rows.entry(row).or_default();
        painted |= paint_uncovered_intervals(ranges, left, right, |start, end| {
            for x in start..end {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_symbol(shaded_fill_symbol(ink, selected, tile, x, row, fill))
                        .set_style(style);
                }
            }
        });
        insert_covered_interval(ranges, left, right);
    }
    covered.remember_fully_covered_region(top, bottom, left, right);
    painted
}

fn shaded_fill_symbol<'a>(
    ink: ShadedInk<'_>,
    selected: bool,
    tile: &Tile,
    x: u16,
    row: u32,
    fill: &'a str,
) -> &'a str {
    if selected {
        return fill;
    }
    let ascii = ink.shades[0] == ASCII_SHADES[0];
    let tile_right = tile.x.saturating_add(tile.width);
    let at_top = row == tile.top_row();
    let at_right = x.saturating_add(1) == tile_right;
    match (ascii, at_top, at_right) {
        (true, true, true) => ASCII_SHADED_TOP_EDGE,
        (false, true, true) => SHADED_TOP_EDGE,
        (true, true, false) => ASCII_SHADED_TOP,
        (false, true, false) => SHADED_TOP,
        (true, false, true) => ASCII_SHADED_EDGE,
        (false, false, true) => SHADED_EDGE,
        _ => fill,
    }
}

fn shade_index(scale: HeatScale, size: u128) -> usize {
    match scale.of(size) {
        heat if heat < 0.25 => 0,
        heat if heat < 0.5 => 1,
        heat if heat < 0.75 => 2,
        _ => 3,
    }
}

/// Paints one half-row, re-expanding a collapsed cell so the other half keeps
/// whatever colour it already held.
fn paint_half(buffer: &mut Buffer, x: u16, y: u16, upper: bool, colour: Color) {
    let Some(cell) = buffer.cell_mut((x, y)) else {
        return;
    };
    if cell.symbol() != HALF_CELL {
        cell.set_symbol(HALF_CELL);
        cell.fg = cell.bg;
    }
    if upper {
        cell.fg = colour;
    } else {
        cell.bg = colour;
    }
}

fn paint_tile(buffer: &mut Buffer, area: Rect, tile: &Tile, ink: &TileInk) {
    let left = tile.x.max(area.x);
    let right = tile.x.saturating_add(tile.width).min(area.right());
    let half_rows = u32::from(HALF_ROWS_PER_CELL);
    let first = tile.y.max(u32::from(area.y) * half_rows);
    let last = tile
        .y
        .saturating_add(tile.height)
        .min(area_bottom_half_row(area));
    if left >= right || first >= last {
        return;
    }
    let crown = tile.y;
    let base = crown.saturating_add(tile.height).saturating_sub(1);
    let edge = tile.x.saturating_add(tile.width).saturating_sub(1);
    for half in first..last {
        let Ok(y) = u16::try_from(half / half_rows) else {
            continue;
        };
        let upper = half % half_rows == 0;
        for x in left..right {
            let colour = if x == edge {
                ink.edge
            } else if half == crown {
                ink.crown
            } else if half == base {
                ink.base
            } else {
                ink.fill
            };
            paint_half(buffer, x, y, upper, colour);
        }
    }
}

fn paint_visible_tile(
    buffer: &mut Buffer,
    area: Rect,
    tile: &Tile,
    ink: &TileInk,
    covered: &mut HalfRowCoverage,
) -> bool {
    let left = tile.x.max(area.x);
    let right = tile.x.saturating_add(tile.width).min(area.right());
    let half_rows = u32::from(HALF_ROWS_PER_CELL);
    let first = tile.y.max(u32::from(area.y) * half_rows);
    let last = tile
        .y
        .saturating_add(tile.height)
        .min(area_bottom_half_row(area));
    if left >= right || first >= last {
        return false;
    }
    if covered.region_is_fully_covered(first, last, left, right) {
        covered.remember_fully_covered_region(first, last, left, right);
        return false;
    }
    let crown = tile.y;
    let base = crown.saturating_add(tile.height).saturating_sub(1);
    let edge = tile.x.saturating_add(tile.width).saturating_sub(1);
    let mut painted = false;
    for half in first..last {
        let Ok(y) = u16::try_from(half / half_rows) else {
            continue;
        };
        let upper = half % half_rows == 0;
        let ranges = covered.rows.entry(half).or_default();
        painted |= paint_uncovered_intervals(ranges, left, right, |start, end| {
            for x in start..end {
                let colour = if x == edge {
                    ink.edge
                } else if half == crown {
                    ink.crown
                } else if half == base {
                    ink.base
                } else {
                    ink.fill
                };
                paint_half(buffer, x, y, upper, colour);
            }
        });
        insert_covered_interval(ranges, left, right);
    }
    covered.remember_fully_covered_region(first, last, left, right);
    painted
}

fn raster_region_is_covered(
    covered: &BTreeMap<u32, BTreeMap<u16, u16>>,
    top: u32,
    bottom: u32,
    left: u16,
    right: u16,
) -> bool {
    (top..bottom).all(|row| {
        covered
            .get(&row)
            .is_some_and(|ranges| row_is_fully_covered(ranges, left, right))
    })
}

/// Paints the portions of a span that no front entry has claimed. Coverage rows
/// are merged, so only the direct predecessor can reach `left`; every earlier
/// span is necessarily wholly left and need not be traversed again.
fn paint_uncovered_intervals(
    ranges: &BTreeMap<u16, u16>,
    left: u16,
    right: u16,
    mut paint: impl FnMut(u16, u16),
) -> bool {
    if left >= right {
        return false;
    }
    let mut cursor = left;
    let mut painted = false;
    let predecessor = ranges
        .range(..=left)
        .next_back()
        .map(|(&start, &end)| (start, end))
        .filter(|&(_, end)| end > left);
    for (start, end) in predecessor.into_iter().chain(
        ranges
            .range((
                std::ops::Bound::Excluded(left),
                std::ops::Bound::Excluded(right),
            ))
            .map(|(&start, &end)| (start, end)),
    ) {
        if end <= cursor {
            continue;
        }
        if start > cursor {
            paint(cursor, start);
            painted = true;
        }
        cursor = cursor.max(end);
        if cursor >= right {
            return painted;
        }
    }
    if cursor < right {
        paint(cursor, right);
        painted = true;
    }
    painted
}

/// The overflow field lives in map coordinates until a cell is actually
/// addressed. A terminal [`Rect`] cannot represent every valid widened row.
#[derive(Clone, Copy)]
struct OverflowRegion {
    x: u16,
    y: u32,
    right: u16,
    bottom: u32,
}

impl OverflowRegion {
    fn width(self) -> u16 {
        self.right.saturating_sub(self.x)
    }

    fn height(self) -> u32 {
        self.bottom.saturating_sub(self.y)
    }

    fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.right && u32::from(y) >= self.y && u32::from(y) < self.bottom
    }
}

/// The rectangle the entries too small to draw were pushed into.
///
/// Layout stops where entries stop earning a cell, so everything from that
/// corner to the bottom right of the map stands for the remainder. The corner
/// advances to the next whole terminal row: the half-row it starts on still
/// carries the last drawn entry, and the summary belongs on the field rather
/// than on top of a neighbour.
fn overflow_region(area: Rect, overflow: MapOverflow) -> Option<OverflowRegion> {
    let x = overflow.x.max(area.x);
    let right = area.right();
    let half_rows = u32::from(HALF_ROWS_PER_CELL);
    let bottom = area_bottom_row(area);
    let y = overflow.y.div_ceil(half_rows).max(u32::from(area.y));
    (x < right && y < bottom).then_some(OverflowRegion {
        x,
        y,
        right,
        bottom,
    })
}

fn collapse_flat_cells(
    buffer: &mut Buffer,
    area: Rect,
    overflow: Option<OverflowRegion>,
    grain: Color,
    backdrop: Color,
) {
    for position in area.positions() {
        let Some(cell) = buffer.cell_mut(position) else {
            continue;
        };
        if cell.symbol() != HALF_CELL || cell.fg != cell.bg {
            continue;
        }
        let colour = cell.bg;
        let in_overflow = overflow.is_some_and(|region| region.contains(position.x, position.y));
        if in_overflow {
            // Grain rather than punctuation: the same half-block the map is
            // built from, alternating halves, so the remainder reads as a field
            // of entries too small to draw at the resolution the map has.
            let lit = (position.x ^ position.y) & 1 == 0;
            let (fg, bg) = if lit {
                (grain, backdrop)
            } else {
                (backdrop, grain)
            };
            cell.set_symbol(HALF_CELL)
                .set_style(Style::default().fg(fg).bg(bg));
        } else {
            cell.set_symbol(" ")
                .set_style(Style::default().fg(colour).bg(colour));
        }
    }
}

/// Finds labels that a top-painted entry would overwrite while geometry moves.
/// Entries tessellate once a layout settles, but a transition briefly lets
/// labels share terminal cells. The reverse paint order records opaque tile
/// bodies while each lower entry probes only the cells its label actually draws.
struct TileLabel {
    top: u32,
    first: String,
    second: Option<String>,
}

struct TransitionLabels {
    labels: Vec<Option<TileLabel>>,
    occlusions: Vec<bool>,
}

fn label_occlusions(
    area: Rect,
    rectangles: &[Tile],
    selected_last: Option<usize>,
    ascii: bool,
) -> TransitionLabels {
    let mut covered = TerminalRowCoverage::new();
    let mut labels: Vec<Option<TileLabel>> = Vec::with_capacity(rectangles.len());
    labels.resize_with(rectangles.len(), || None);
    let mut occlusions = vec![false; rectangles.len()];
    for (index, tile) in tile_paint_order(rectangles, selected_last).rev() {
        let left = tile.x.max(area.x);
        let right = tile.x.saturating_add(tile.width).min(area.right());
        let Some((top, bottom)) = clipped_terminal_rows(area, tile) else {
            continue;
        };
        if left >= right {
            continue;
        }

        if covered.has_remembered_fully_covered_region(top, bottom, left, right) {
            // Every drawable label cell lies within its tile body. Do not format
            // text or revisit rows when a pivot is already fully hidden.
            occlusions[index] = can_draw_tile_label(tile);
            continue;
        }

        let label = tile_label(tile, ascii);
        if let Some(label) = label.as_ref() {
            occlusions[index] = tile_label_is_occluded(&covered, area, tile, label);
        }
        labels[index] = label;
        // A tile without room for text still hides a lower label behind its body.
        covered.cover_region_if_needed(top, bottom, left, right);
    }
    TransitionLabels { labels, occlusions }
}

fn tile_label_is_occluded(
    covered: &TerminalRowCoverage,
    area: Rect,
    tile: &Tile,
    label: &TileLabel,
) -> bool {
    label_span_is_covered(covered, area, tile, label.top, &label.first)
        || label.second.as_deref().is_some_and(|second| {
            label_span_is_covered(covered, area, tile, label.top.saturating_add(1), second)
        })
}

fn label_span_is_covered(
    covered: &TerminalRowCoverage,
    area: Rect,
    tile: &Tile,
    row: u32,
    text: &str,
) -> bool {
    let Some(span) = rendered_label_span(area, tile, row, text) else {
        return false;
    };
    covered
        .rows
        .get(&span.row)
        .is_some_and(|ranges| row_has_covered_interval(ranges, span.left, span.right))
}

fn row_has_covered_interval(ranges: &BTreeMap<u16, u16>, left: u16, right: u16) -> bool {
    ranges
        .range(..right)
        .next_back()
        .is_some_and(|(_, end)| *end > left)
}

fn row_is_fully_covered(ranges: &BTreeMap<u16, u16>, left: u16, right: u16) -> bool {
    ranges
        .range(..=left)
        .next_back()
        .is_some_and(|(_, end)| *end >= right)
}

fn insert_covered_interval<T: Ord + Copy>(ranges: &mut BTreeMap<T, T>, left: T, right: T) {
    let mut merged_left = left;
    let mut merged_right = right;
    if let Some((start, end)) = ranges
        .range(..=left)
        .next_back()
        .map(|(&start, &end)| (start, end))
        && end >= left
    {
        merged_left = start;
        merged_right = merged_right.max(end);
        ranges.remove(&start);
    }
    while let Some((start, end)) = ranges
        .range(merged_left..)
        .next()
        .map(|(&start, &end)| (start, end))
    {
        if start > merged_right {
            break;
        }
        merged_right = merged_right.max(end);
        ranges.remove(&start);
    }
    ranges.insert(merged_left, merged_right);
}

fn label_max_width(tile: &Tile) -> u16 {
    tile.width.saturating_sub(2)
}

/// The terminal rows a tile owns completely. Labels overwrite whole cells, so a
/// touched half-row belongs to the sibling sharing the other half as well.
fn fully_owned_label_rows(tile: &Tile) -> (u32, u32) {
    let half_rows = u32::from(HALF_ROWS_PER_CELL);
    let top = tile.y.div_ceil(half_rows);
    let bottom = tile.y.saturating_add(tile.height) / half_rows;
    (top, bottom)
}

/// Keeps the renderer on whole rows that the entry owns, rather than on every
/// terminal row the entry merely touches.
fn can_draw_tile_label(tile: &Tile) -> bool {
    let (top, bottom) = fully_owned_label_rows(tile);
    top < bottom && tile.width >= MINIMUM_LABEL_WIDTH && label_max_width(tile) >= 4
}

fn tile_label(tile: &Tile, ascii: bool) -> Option<TileLabel> {
    if !can_draw_tile_label(tile) {
        return None;
    }
    let (first_owned_row, last_owned_row) = fully_owned_label_rows(tile);
    let rows = last_owned_row.saturating_sub(first_owned_row);
    let max_width = label_max_width(tile);
    let second = if rows >= 2 {
        tile_detail_line(tile, max_width, ascii)
    } else {
        None
    };
    let name = display_os_str_info(&tile.name);
    let filename = match tile.file_type {
        FileType::File => name.text.clone(),
        FileType::Folder => {
            let folder = format!("{}/", name.text);
            let detail = tile.descendants.map(|count| format!("{folder} (+{count})"));
            detail
                .filter(|value| value.width() <= usize::from(max_width))
                .unwrap_or(folder)
        }
        FileType::Synthetic => format!("[{}]", name.text),
    };
    let uncertainty =
        (tile.uncertain && second.is_none()).then(|| uncertainty_marker(tile.size, ascii));
    let first = if let Some(marker) = uncertainty {
        let filename_width = max_width.saturating_sub(marker.cell_width());
        let filename = truncate_marked(
            &SafeDisplayPath {
                text: filename,
                deceptive: name.deceptive,
            },
            filename_width,
            truncate_middle,
        );
        format!("{marker}{filename}")
    } else {
        truncate_marked(
            &SafeDisplayPath {
                text: filename,
                deceptive: name.deceptive,
            },
            max_width,
            truncate_middle,
        )
    };
    let lines = if second.is_some() { 2 } else { 1 };
    // Leave the first row to the focus ring whenever the entry is tall enough
    // to spare it, then centre the text in what is left.
    let inset = u32::from(rows > lines);
    let top = first_owned_row
        .saturating_add(inset.saturating_add(rows.saturating_sub(lines).saturating_sub(inset) / 2));
    Some(TileLabel { top, first, second })
}

fn uncertainty_marker(size: u128, ascii: bool) -> &'static str {
    if size == 0 {
        "?"
    } else if ascii {
        ">="
    } else {
        "≥"
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "label rendering carries explicit geometry and style inputs"
)]
fn draw_tile_label(
    buffer: &mut Buffer,
    area: Rect,
    tile: &Tile,
    background: Color,
    text: Color,
    detail: Color,
    ascii: bool,
    preserve_reverse: bool,
) {
    let Some(label) = tile_label(tile, ascii) else {
        return;
    };
    draw_prepared_tile_label(
        buffer,
        area,
        tile,
        &label,
        background,
        text,
        detail,
        preserve_reverse,
    );
}

#[allow(
    clippy::too_many_arguments,
    reason = "prepared labels carry explicit geometry and style inputs"
)]
fn draw_prepared_tile_label(
    buffer: &mut Buffer,
    area: Rect,
    tile: &Tile,
    label: &TileLabel,
    background: Color,
    text: Color,
    detail: Color,
    preserve_reverse: bool,
) {
    let mut first_style = Style::default()
        .fg(text)
        .bg(background)
        .add_modifier(Modifier::BOLD);
    let mut detail_style = Style::default().fg(detail).bg(background);
    if !preserve_reverse {
        first_style = first_style.remove_modifier(Modifier::REVERSED);
        detail_style = detail_style.remove_modifier(Modifier::REVERSED);
    }
    draw_line(buffer, area, tile, label.top, &label.first, first_style);
    if let Some(second) = label.second.as_deref() {
        draw_line(
            buffer,
            area,
            tile,
            label.top.saturating_add(1),
            second,
            detail_style,
        );
    }
}

fn tile_detail_line(tile: &Tile, max_width: u16, ascii: bool) -> Option<String> {
    let (size, rounded_size) = tile_size_labels(tile, ascii);
    let percentage = format!("{:.0}%", tile.percentage * 100.0);
    let separator = if ascii { "." } else { "·" };
    let detail = [
        format!("{size} {separator} {percentage}"),
        size,
        rounded_size,
    ]
    .into_iter()
    .find(|candidate| candidate.width() <= usize::from(max_width));
    if tile.uncertain {
        detail
    } else {
        detail.or_else(|| (percentage.width() <= usize::from(max_width)).then_some(percentage))
    }
}

fn tile_size_labels(tile: &Tile, ascii: bool) -> (String, String) {
    size_labels(tile.size, tile.uncertain, ascii)
}

fn size_labels(size: u128, uncertain: bool, ascii: bool) -> (String, String) {
    if uncertain && size == 0 {
        return ("?".to_string(), "?".to_string());
    }
    let prefix = if uncertain {
        uncertainty_marker(size, ascii)
    } else {
        ""
    };

    let (size, rounded_size) = if uncertain {
        (
            lower_bound_display_size(size),
            rounded_lower_bound_display_size(size),
        )
    } else {
        (
            DisplaySize(size as f64).to_string(),
            rounded_display_size(size),
        )
    };
    (format!("{prefix}{size}"), format!("{prefix}{rounded_size}"))
}

/// Formats an inexact size downward so an inclusive lower-bound marker never
/// claims more bytes than the scanner has established.
fn lower_bound_display_size(size: u128) -> String {
    let Some((unit, suffix)) = lower_bound_unit(size) else {
        return format!("{size}B");
    };
    let whole = size / unit;
    let tenths = (size % unit) * 10 / unit;
    format!("{whole}.{tenths}{suffix}")
}

fn rounded_lower_bound_display_size(size: u128) -> String {
    let Some((unit, suffix)) = lower_bound_unit(size) else {
        return format!("{size}B");
    };
    format!("{}{suffix}", size / unit)
}

fn lower_bound_unit(size: u128) -> Option<(u128, &'static str)> {
    if size >= 1_073_741_824 {
        Some((1_073_741_824, "G"))
    } else if size >= 1_048_576 {
        Some((1_048_576, "M"))
    } else if size >= 1024 {
        Some((1024, "K"))
    } else {
        None
    }
}

fn rounded_display_size(size: u128) -> String {
    let size = size as f64;
    if size > 999_999_999.0 {
        format!("{:.0}G", size / 1_073_741_824.0)
    } else if size > 999_999.0 {
        format!("{:.0}M", size / 1_048_576.0)
    } else if size > 999.0 {
        format!("{:.0}K", size / 1024.0)
    } else {
        format!("{size:.0}B")
    }
}

#[derive(Clone, Copy)]
struct LabelSpan {
    row: u32,
    left: u16,
    right: u16,
}

fn rendered_label_span(area: Rect, tile: &Tile, row: u32, text: &str) -> Option<LabelSpan> {
    let (top, bottom) = fully_owned_label_rows(tile);
    if row < u32::from(area.y) || row >= area_bottom_row(area) || row < top || row >= bottom {
        return None;
    }
    let width = text.cell_width();
    if width == 0 {
        return None;
    }
    // Entries read as plates: each line sits on the centre column of the entry,
    // so the text is centred on both axes and never crowds the focus ring.
    let left = tile.x.saturating_add(tile.width.saturating_sub(width) / 2);
    if left < area.x || left >= area.right() {
        return None;
    }
    let limit = tile
        .x
        .saturating_add(tile.width)
        .min(area.right())
        .saturating_sub(left);
    // `Buffer::set_stringn` stops before a grapheme that would cross its
    // clipping edge. Mirror that pass so a wide trailing grapheme does not
    // make occlusion claim a cell the renderer never changes.
    let mut remaining_width = limit;
    let mut rendered_width = 0;
    for grapheme in text
        .graphemes(true)
        .filter(|grapheme| !grapheme.contains(char::is_control))
    {
        let grapheme_width = grapheme.cell_width();
        if grapheme_width == 0 {
            continue;
        }
        let Some(remaining) = remaining_width.checked_sub(grapheme_width) else {
            break;
        };
        remaining_width = remaining;
        rendered_width += grapheme_width;
    }
    let right = left.saturating_add(rendered_width);
    (left < right).then_some(LabelSpan { row, left, right })
}

fn draw_line(buffer: &mut Buffer, area: Rect, tile: &Tile, row: u32, text: &str, style: Style) {
    let Some(span) = rendered_label_span(area, tile, row, text) else {
        return;
    };
    let Ok(row) = u16::try_from(span.row) else {
        return;
    };
    if buffer.cell_mut((span.left, row)).is_none() {
        return;
    }
    buffer.set_stringn(
        span.left,
        row,
        text,
        usize::from(span.right.saturating_sub(span.left)),
        style,
    );
}

/// Names the remainder inside its own region: how many entries it stands for
/// and what they weigh, so the grain is a quantity rather than a texture.
fn draw_overflow_label(
    buffer: &mut Buffer,
    region: OverflowRegion,
    overflow: MapOverflow,
    theme: Theme,
    backdrop: Color,
    ascii: bool,
) {
    let inset = u16::from(region.width() > 2);
    let width = usize::from(region.width().saturating_sub(inset.saturating_mul(2)));
    if width < 4 {
        return;
    }
    let entries = overflow.entries;
    let headline = [
        format!("{entries} entries too small to draw"),
        format!("{entries} small entries"),
        format!("{entries} small"),
        format!("{entries}"),
    ]
    .into_iter()
    .find(|candidate| candidate.width() <= width);
    let Some(headline) = headline else {
        return;
    };
    let x = region.x.saturating_add(inset);
    // The map can carry rows beyond the `u16` terminal address space. Do not
    // narrow until this exact write; an inaccessible label simply stays offscreen.
    let Ok(y) = u16::try_from(region.y) else {
        return;
    };
    if buffer.cell_mut((x, y)).is_none() {
        return;
    }
    buffer.set_stringn(
        x,
        y,
        &headline,
        width,
        Style::default()
            .fg(theme.text_secondary)
            .bg(backdrop)
            .add_modifier(Modifier::BOLD),
    );
    if region.height() < 2 {
        return;
    }
    let Some(detail_y) = region
        .y
        .checked_add(1)
        .and_then(|row| u16::try_from(row).ok())
    else {
        return;
    };
    if let Some(detail) = overflow_detail_line(overflow, width, ascii) {
        if buffer.cell_mut((x, detail_y)).is_none() {
            return;
        }
        buffer.set_stringn(
            x,
            detail_y,
            &detail,
            width,
            Style::default().fg(theme.text_muted).bg(backdrop),
        );
    }
}

fn overflow_detail_line(overflow: MapOverflow, width: usize, ascii: bool) -> Option<String> {
    let (size, rounded_size) = size_labels(overflow.bytes, overflow.uncertain, ascii);
    [format!("{size} total"), size, rounded_size]
        .into_iter()
        .find(|candidate| candidate.width() <= width)
}

fn draw_empty_surface(
    buffer: &mut Buffer,
    area: Rect,
    theme: Theme,
    palette: Option<MapPalette>,
    ascii: bool,
    show_label: bool,
) {
    let backdrop = palette.map_or_else(|| theme.map_surface(), MapPalette::backdrop);
    for position in area.positions() {
        buffer[position]
            .set_symbol(if ascii { "." } else { "·" })
            .set_style(Style::default().fg(theme.text_muted).bg(backdrop));
    }
    if !show_label {
        return;
    }
    let label = "Folder is empty";
    if area.width >= label.width() as u16 && area.height > 0 {
        let x = u32::from(area.x) + u32::from(area.width.saturating_sub(label.width() as u16)) / 2;
        let y = u32::from(area.y) + u32::from(area.height) / 2;
        let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y)) else {
            return;
        };
        if buffer.cell_mut((x, y)).is_none() {
            return;
        }
        buffer.set_string(
            x,
            y,
            label,
            Style::default()
                .fg(theme.text_primary)
                .bg(backdrop)
                .add_modifier(Modifier::BOLD),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    use crate::model::NodeId;
    use crate::theme::ThemeId;
    use crate::ui::palette::Oklch;

    use super::*;

    /// Entries are sized from their node id so that a fixture of several tiles
    /// spans the heat ramp the way a real folder does.
    fn tile(x: u16, y: u32, width: u16, height: u32, node_id: u32) -> Tile {
        Tile {
            x,
            y,
            width,
            height,
            node_id: NodeId(node_id),
            name: OsString::from(format!("entry-{node_id}")),
            size: u128::from(node_id) * 1024,
            apparent_size: u128::from(node_id) * 1024,
            descendants: None,
            percentage: 0.5,
            file_type: FileType::File,
            synthetic_kind: None,
            uncertain: false,
        }
    }

    fn render(
        tiles: &[Tile],
        area: Rect,
        selected: Option<usize>,
        theme: ThemeId,
        ascii: bool,
    ) -> Buffer {
        render_presentation(tiles, area, selected, theme, ascii, false)
    }

    fn render_presentation(
        tiles: &[Tile],
        area: Rect,
        selected: Option<usize>,
        theme: ThemeId,
        ascii: bool,
        monochrome: bool,
    ) -> Buffer {
        let mut buffer = Buffer::empty(area);
        DenseRectangleGrid::new(
            MapLayout {
                rectangles: tiles,
                departing: &[],
                overflow: None,
                selected_rect_index: selected,
                transitioning: false,
                show_empty_label: true,
            },
            Theme::for_id(theme),
            ascii,
            monochrome,
        )
        .render(area, &mut buffer);
        buffer
    }

    /// The same surface drawn mid-transition, where entries are interpolated
    /// toward their own targets and may briefly overlap.
    fn render_transitioning(
        tiles: &[Tile],
        departing: &[Tile],
        selected: Option<usize>,
        area: Rect,
        theme: ThemeId,
        ascii: bool,
        monochrome: bool,
    ) -> Buffer {
        let mut buffer = Buffer::empty(area);
        DenseRectangleGrid::new(
            MapLayout {
                rectangles: tiles,
                departing,
                overflow: None,
                selected_rect_index: selected,
                transitioning: true,
                show_empty_label: true,
            },
            Theme::for_id(theme),
            ascii,
            monochrome,
        )
        .render(area, &mut buffer);
        buffer
    }

    fn transition_label_occlusions(
        area: Rect,
        tiles: &[Tile],
        selected: Option<usize>,
        ascii: bool,
    ) -> Vec<bool> {
        label_occlusions(area, tiles, selected, ascii).occlusions
    }

    fn lightness_of(colour: Color) -> f32 {
        Oklch::from_color(colour)
            .expect("map ink is truecolour")
            .lightness
    }

    fn text_of(buffer: &Buffer) -> String {
        buffer.content.iter().fold(String::new(), |mut text, cell| {
            text.push_str(cell.symbol());
            text
        })
    }

    fn row_text(buffer: &Buffer, area: Rect, row: u16) -> String {
        (area.x..area.right()).fold(String::new(), |mut text, column| {
            text.push_str(buffer[(column, row)].symbol());
            text
        })
    }

    #[test]
    fn neighbours_tessellate_without_a_single_uncoloured_cell() {
        // Two entries, each four half-rows tall, fill a two-row surface exactly.
        let tiles = [tile(0, 0, 4, 4, 1), tile(4, 0, 4, 4, 2)];
        let area = Rect::new(0, 0, 8, 2);
        let buffer = render(&tiles, area, None, ThemeId::CatppuccinMocha, false);
        let backdrop = MapPalette::for_theme(Theme::for_id(ThemeId::CatppuccinMocha))
            .expect("mocha is truecolour")
            .backdrop();
        assert!(
            buffer.content.iter().all(|cell| cell.bg != backdrop),
            "packed entries must leave no backdrop showing"
        );
    }

    #[test]
    fn a_folder_whose_entries_are_all_too_small_is_not_an_empty_folder() {
        // Layout kept nothing: every entry fell below one cell. The map still
        // holds thousands of files and must say so.
        let area = Rect::new(0, 0, 40, 6);
        let mut buffer = Buffer::empty(area);
        DenseRectangleGrid::new(
            MapLayout {
                rectangles: &[],
                departing: &[],
                overflow: Some(MapOverflow {
                    x: 0,
                    y: 0,
                    entries: 4_000,
                    bytes: 16_384_000,
                    uncertain: false,
                }),
                selected_rect_index: None,
                transitioning: false,
                show_empty_label: true,
            },
            Theme::for_id(ThemeId::CatppuccinMocha),
            false,
            false,
        )
        .render(area, &mut buffer);
        let text = text_of(&buffer);
        assert!(
            text.contains("4000 entries too small to draw"),
            "the remainder must be named: {text}"
        );
        assert!(
            !text.contains("empty"),
            "a folder full of small entries is not empty: {text}"
        );
    }

    #[test]
    fn narrow_overflow_detail_keeps_its_unit() {
        let region = Rect::new(0, 0, 6, 2);
        let mut buffer = Buffer::empty(region);
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let overflow = MapOverflow {
            x: 0,
            y: 0,
            entries: 1,
            bytes: 16_384_000,
            uncertain: false,
        };
        let overflow_area =
            overflow_region(region, overflow).expect("the test overflow must intersect its buffer");
        draw_overflow_label(
            &mut buffer,
            overflow_area,
            overflow,
            theme,
            theme.map_surface(),
            false,
        );
        let detail = row_text(&buffer, region, 1);
        assert!(
            detail.contains('M'),
            "a narrow total must retain its unit: {detail:?}"
        );
        assert!(
            !detail.contains("15.6"),
            "the unmeasured fallback must not be clipped: {detail:?}"
        );
    }

    #[test]
    fn uncertain_overflow_details_preserve_unknown_and_lower_bound_semantics() {
        let unknown = MapOverflow {
            x: 0,
            y: 0,
            entries: 1,
            bytes: 0,
            uncertain: true,
        };
        assert_eq!(
            overflow_detail_line(unknown, 32, false).as_deref(),
            Some("? total"),
            "a zero lower bound must remain unknown"
        );

        let lower_bound = overflow_detail_line(
            MapOverflow {
                x: 0,
                y: 0,
                entries: 1,
                bytes: 4_096,
                uncertain: true,
            },
            32,
            false,
        )
        .expect("a wide overflow region must name its lower bound");
        assert!(
            lower_bound.starts_with('≥') && lower_bound.ends_with(" total"),
            "an inexact nonzero total must be a lower bound: {lower_bound}"
        );
    }

    #[test]
    fn ascii_lower_bounds_count_the_full_marker_before_width_selection() {
        let mut entry = tile(0, 0, 8, 4, 1);
        entry.size = 16_384_000;
        entry.uncertain = true;
        assert_eq!(
            tile_detail_line(&entry, 4, false).as_deref(),
            Some("≥15M"),
            "the single-cell marker fits the compact Unicode lower bound"
        );
        assert_eq!(
            tile_detail_line(&entry, 4, true),
            None,
            "the two-column ASCII marker must not be clipped into a four-column tile"
        );
        assert_eq!(
            tile_detail_line(&entry, 5, true).as_deref(),
            Some(">=15M"),
            "ASCII lower bounds must select a candidate after accounting for >="
        );

        let rendered = text_of(&render(
            &[entry.clone()],
            Rect::new(0, 0, 8, 2),
            None,
            ThemeId::CatppuccinMocha,
            true,
        ));
        assert!(
            rendered.contains(">=15M"),
            "ASCII lower bound missing: {rendered:?}"
        );
        assert!(rendered.is_ascii(), "ASCII mode emitted {rendered:?}");

        let overflow = MapOverflow {
            x: 0,
            y: 0,
            entries: 1,
            bytes: entry.size,
            uncertain: true,
        };
        assert_eq!(
            overflow_detail_line(overflow, 4, true),
            None,
            "overflow labels use the same ASCII-aware candidate widths"
        );
        assert_eq!(
            overflow_detail_line(overflow, 5, true).as_deref(),
            Some(">=15M"),
            "overflow lower bounds keep the inclusive ASCII marker"
        );
    }
    #[test]
    fn one_line_shaded_labels_keep_uncertainty_visible() {
        let mut entry = tile(0, 0, 20, 2, 1);
        entry.uncertain = true;
        for (presentation, theme, ascii, monochrome, marker) in [
            ("ASCII", ThemeId::CatppuccinMocha, true, true, ">="),
            ("monochrome", ThemeId::Monochrome, false, true, "≥"),
            ("high contrast", ThemeId::HighContrast, false, false, "≥"),
        ] {
            let rendered = text_of(&render_presentation(
                &[entry.clone()],
                Rect::new(0, 0, 20, 1),
                None,
                theme,
                ascii,
                monochrome,
            ));
            assert!(
                rendered.contains(marker),
                "the one-line {presentation} label must expose uncertainty: {rendered:?}"
            );
        }
    }

    #[test]
    fn narrow_ascii_labels_keep_uncertainty_markers_outside_truncation() {
        let mut entry = tile(0, 0, 8, 4, 1);
        entry.name = OsString::from("long-name");
        entry.size = 1_023;
        entry.uncertain = true;

        let label = tile_label(&entry, true).expect("an eight-column tile can carry a label");
        assert!(label.first.starts_with(">="));
        assert!(label.first.cell_width() <= 6);
    }

    #[test]
    fn shaded_siblings_keep_non_colour_boundaries() {
        let horizontal = [tile(0, 0, 4, 4, 1), tile(4, 0, 4, 4, 2)];
        for (presentation, theme, ascii, monochrome, top, edge, corner) in [
            (
                "ASCII",
                ThemeId::CatppuccinMocha,
                true,
                true,
                ASCII_SHADED_TOP,
                ASCII_SHADED_EDGE,
                ASCII_SHADED_TOP_EDGE,
            ),
            (
                "monochrome",
                ThemeId::Monochrome,
                false,
                true,
                SHADED_TOP,
                SHADED_EDGE,
                SHADED_TOP_EDGE,
            ),
            (
                "high contrast",
                ThemeId::HighContrast,
                false,
                false,
                SHADED_TOP,
                SHADED_EDGE,
                SHADED_TOP_EDGE,
            ),
        ] {
            let buffer = render_presentation(
                &horizontal,
                Rect::new(0, 0, 8, 2),
                None,
                theme,
                ascii,
                monochrome,
            );
            assert_eq!(
                buffer[(0, 0)].symbol(),
                top,
                "top boundary missing in {presentation}"
            );
            assert_eq!(
                buffer[(3, 0)].symbol(),
                corner,
                "corner boundary missing in {presentation}"
            );
            assert_eq!(
                buffer[(3, 1)].symbol(),
                edge,
                "edge boundary missing in {presentation}"
            );
            assert_eq!(
                buffer[(4, 0)].symbol(),
                top,
                "sibling boundary missing in {presentation}"
            );
        }

        let vertical = [tile(0, 0, 8, 4, 1), tile(0, 4, 8, 4, 2)];
        let buffer = render_presentation(
            &vertical,
            Rect::new(0, 0, 8, 4),
            None,
            ThemeId::Monochrome,
            false,
            true,
        );
        assert_eq!(
            buffer[(0, 2)].symbol(),
            SHADED_TOP,
            "stacked shaded siblings need a horizontal boundary"
        );
    }

    #[test]
    fn fractional_uncertain_sizes_round_every_label_down() {
        let (size, rounded_size) = size_labels(2_007, true, false);
        assert_eq!(size, "≥1.9K", "a lower bound must not round 1.96K up");
        assert_eq!(
            rounded_size, "≥1K",
            "the compact lower bound must floor too"
        );
    }

    #[test]
    fn overflow_region_keeps_full_u32_map_extent_until_terminal_output() {
        let area = Rect {
            x: 0,
            y: 50_000,
            width: 8,
            height: 50_000,
        };
        let region = overflow_region(
            area,
            MapOverflow {
                x: 0,
                y: 100_000,
                entries: 1,
                bytes: 0,
                uncertain: false,
            },
        )
        .expect("the logical overflow region must intersect the widened map");
        assert_eq!((region.y, region.bottom), (50_000, 100_000));
        assert_eq!(area_bottom_half_row(area), 200_000);
    }

    #[test]
    fn one_cell_carries_two_entries_at_a_half_row_boundary() {
        // The boundary falls mid-cell: row 1 is entry one on top, entry two below.
        let tiles = [tile(0, 0, 8, 3, 1), tile(0, 3, 8, 3, 2)];
        let area = Rect::new(0, 0, 8, 3);
        let buffer = render(&tiles, area, None, ThemeId::CatppuccinMocha, false);
        let boundary = &buffer[(0, 1)];
        assert_eq!(
            boundary.symbol(),
            HALF_CELL,
            "a split cell must stay a half-block"
        );
        assert_ne!(
            boundary.fg, boundary.bg,
            "each half of a split cell carries its own entry"
        );
    }

    #[test]
    fn flat_interiors_collapse_so_no_half_block_seam_can_show() {
        let tiles = [tile(0, 0, 8, 8, 1)];
        let area = Rect::new(0, 0, 8, 4);
        let buffer = render(&tiles, area, None, ThemeId::CatppuccinMocha, false);
        // Column 0 of row 1 sits inside the fill: no crown, no base, no trailing
        // edge, and clear of the label's hanging indent.
        let interior = &buffer[(0, 1)];
        assert_eq!(interior.symbol(), " ");
        assert_eq!(interior.fg, interior.bg);
    }

    #[test]
    fn entries_are_embossed_rather_than_separated_by_gaps() {
        let tiles = [tile(0, 0, 8, 6, 1)];
        let area = Rect::new(0, 0, 8, 3);
        let buffer = render(&tiles, area, None, ThemeId::CatppuccinMocha, false);
        let crown = buffer[(0, 0)].fg;
        let body = buffer[(0, 1)].bg;
        let edge = buffer[(7, 1)].bg;
        assert_ne!(crown, body, "the top half-row lifts out of the fill");
        assert_ne!(edge, body, "the trailing column darkens to divide entries");
    }

    #[test]
    fn entries_are_coloured_by_size_rather_than_by_name() {
        let mut small = tile(0, 0, 4, 4, 1);
        let mut large = tile(4, 0, 4, 4, 2);
        small.size = 4_096;
        large.size = 1_048_576;
        let area = Rect::new(0, 0, 8, 2);
        let buffer = render(
            &[small.clone(), large],
            area,
            None,
            ThemeId::CatppuccinMocha,
            false,
        );
        let cold = Oklch::from_color(buffer[(1, 0)].bg).expect("map ink is truecolour");
        let hot = Oklch::from_color(buffer[(5, 0)].bg).expect("map ink is truecolour");
        assert!(
            hot.hue < cold.hue,
            "the larger entry must sit further along the ramp: {} vs {}",
            hot.hue,
            cold.hue
        );

        // Same size, different name: the map must not invent a difference.
        let mut twin = tile(4, 0, 4, 4, 3);
        twin.size = small.size;
        twin.name = OsString::from("a-completely-different-name");
        let twins = render(&[small, twin], area, None, ThemeId::CatppuccinMocha, false);
        assert_eq!(
            twins[(1, 0)].bg,
            twins[(5, 0)].bg,
            "two entries of one size are one colour"
        );
    }

    #[test]
    fn the_ramp_is_spent_on_the_folder_in_front_of_the_reader() {
        // One entry dwarfs the rest: the small ones must still separate from
        // each other rather than collapsing into one cold colour.
        let mut giant = tile(0, 0, 4, 4, 1);
        let mut middle = tile(4, 0, 2, 4, 2);
        let mut small = tile(6, 0, 2, 4, 3);
        giant.size = 64 * 1_048_576;
        middle.size = 32_768;
        small.size = 4_096;
        let area = Rect::new(0, 0, 8, 2);
        let buffer = render(
            &[giant, middle, small],
            area,
            None,
            ThemeId::CatppuccinMocha,
            false,
        );
        let hot = Oklch::from_color(buffer[(1, 0)].bg).expect("map ink is truecolour");
        let warm = Oklch::from_color(buffer[(4, 0)].bg).expect("map ink is truecolour");
        let cold = Oklch::from_color(buffer[(6, 0)].bg).expect("map ink is truecolour");
        assert!(
            hot.hue < warm.hue && warm.hue < cold.hue,
            "the ramp must stay ordered: {} {} {}",
            hot.hue,
            warm.hue,
            cold.hue
        );
        assert!(
            cold.hue - hot.hue > 0.4,
            "a folder that spans four orders of magnitude spends the ramp: {} to {}",
            hot.hue,
            cold.hue
        );
    }

    #[test]
    fn a_folder_of_one_size_claims_neither_end_of_the_ramp() {
        let mut tiles = [tile(0, 0, 4, 4, 1), tile(4, 0, 4, 4, 2)];
        for entry in &mut tiles {
            entry.size = 8_192;
        }
        let scale = HeatScale::for_tiles(&tiles);
        for entry in &tiles {
            assert!(
                (scale.of(entry.size) - HeatScale::NEUTRAL).abs() < f32::EPSILON,
                "nothing to compare against must read as neutral"
            );
        }
    }

    #[test]
    fn zero_and_one_byte_entries_claim_distinct_ramp_ends() {
        let mut zero = tile(0, 0, 4, 4, 1);
        zero.size = 0;
        let mut one = tile(4, 0, 4, 4, 2);
        one.size = 1;
        let scale = HeatScale::for_tiles(&[zero.clone(), one.clone()]);
        assert!(
            scale.of(zero.size) < 0.000_1,
            "zero bytes must remain the cold endpoint"
        );
        assert!(
            (scale.of(one.size) - 1.0).abs() < 0.000_1,
            "one byte must remain the hot endpoint"
        );
    }

    #[test]
    fn semantic_entries_do_not_consume_heat_ramp_endpoints() {
        let mut cold = tile(0, 0, 8, 6, 1);
        cold.size = 4_096;
        let mut hot = tile(8, 0, 8, 6, 2);
        hot.size = 1_048_576;
        let mut uncertain = tile(16, 0, 8, 6, 3);
        uncertain.size = 0;
        uncertain.uncertain = true;
        let mut shared = tile(24, 0, 8, 6, 4);
        shared.size = 1_099_511_627_776;
        shared.synthetic_kind = Some(crate::model::SyntheticKind::Shared);
        let mut aggregate = tile(32, 0, 8, 6, 5);
        aggregate.size = 1_125_899_906_842_624;
        aggregate.synthetic_kind = Some(crate::model::SyntheticKind::Aggregate);
        let tiles = [cold.clone(), hot.clone(), uncertain, shared, aggregate];
        let area = Rect::new(0, 0, 40, 3);
        let buffer = render(&tiles, area, None, ThemeId::CatppuccinMocha, false);
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let Some(palette) = crate::ui::palette::derived_for(theme).1 else {
            panic!("the Catppuccin fixture needs a truecolour palette");
        };
        let scale = HeatScale::for_tiles(&[cold.clone(), hot.clone()]);
        let expected_cold = TileInk::resolve(&cold, theme, palette, Emphasis::Resting, scale);
        let expected_hot = TileInk::resolve(&hot, theme, palette, Emphasis::Resting, scale);
        assert_eq!(
            buffer[(1, 1)].bg,
            expected_cold.fill,
            "a semantic zero-byte entry must not warm the smallest ordinary entry"
        );
        assert_eq!(
            buffer[(9, 1)].bg,
            expected_hot.fill,
            "a semantic giant must not cool the largest ordinary entry"
        );
    }

    #[test]
    fn map_colours_never_borrow_the_danger_accent() {
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let palette = MapPalette::for_theme(theme).expect("mocha is truecolour");
        let tiles: Vec<Tile> = (0..24)
            .map(|index| {
                let mut entry = tile(0, 0, 8, 4, index);
                entry.name = OsString::from(format!("candidate-{index}"));
                entry
            })
            .collect();
        let scale = HeatScale::for_tiles(&tiles);
        for entry in &tiles {
            let ink = TileInk::resolve(entry, theme, palette, Emphasis::Resting, scale);
            assert_ne!(ink.fill, theme.text_danger);
            assert_ne!(ink.fill, theme.surface_danger);
        }
    }

    #[test]
    fn selection_lifts_the_cursor_and_sinks_every_other_entry() {
        let tiles = [tile(0, 0, 6, 6, 1), tile(6, 0, 6, 6, 2)];
        let area = Rect::new(0, 0, 12, 3);
        let resting = render(&tiles, area, None, ThemeId::CatppuccinMocha, false);
        let selected = render(&tiles, area, Some(0), ThemeId::CatppuccinMocha, false);
        let cursor = lightness_of(selected[(2, 1)].bg);
        let neighbour = lightness_of(selected[(8, 1)].bg);
        assert!(
            cursor > lightness_of(resting[(2, 1)].bg) + 0.05,
            "the cursor entry brightens: {cursor} vs {}",
            lightness_of(resting[(2, 1)].bg)
        );
        assert!(
            neighbour < lightness_of(resting[(8, 1)].bg) - 0.05,
            "every other entry dims: {neighbour} vs {}",
            lightness_of(resting[(8, 1)].bg)
        );
        assert!(
            cursor > neighbour + 0.15,
            "the cursor must be the brightest entry on the map: {cursor} vs {neighbour}"
        );
    }

    #[test]
    fn selected_tiles_keep_their_bottom_boundary_level() {
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let palette = MapPalette::for_theme(theme).expect("mocha is truecolour");
        let mut selected = tile(0, 0, 12, 6, 166);
        selected.size = 1_048_576;
        let scale = HeatScale::for_tiles(std::slice::from_ref(&selected));
        let ink = TileInk::resolve(&selected, theme, palette, Emphasis::Selected, scale);

        assert_eq!(
            ink.base, ink.fill,
            "selected bottom cells must not be darkened into a neighbouring entry"
        );
    }

    #[test]
    fn an_unselected_map_holds_every_entry_at_full_brightness() {
        let tiles = [tile(0, 0, 6, 6, 1), tile(6, 0, 6, 6, 2)];
        let area = Rect::new(0, 0, 12, 3);
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let palette = MapPalette::for_theme(theme).expect("mocha is truecolour");
        let buffer = render(&tiles, area, None, ThemeId::CatppuccinMocha, false);
        for tile in &tiles {
            let ink = TileInk::resolve(
                tile,
                theme,
                palette,
                Emphasis::Resting,
                HeatScale::for_tiles(&tiles),
            );
            assert_eq!(
                buffer[(tile.x + 2, 1)].bg,
                ink.fill,
                "with no cursor, nothing is dimmed"
            );
        }
    }

    #[test]
    fn dimming_keeps_the_hue_that_tells_two_entries_apart() {
        let tiles = [
            tile(0, 0, 4, 6, 1),
            tile(4, 0, 4, 6, 2),
            tile(8, 0, 4, 6, 3),
        ];
        let area = Rect::new(0, 0, 12, 3);
        let selected = render(&tiles, area, Some(0), ThemeId::CatppuccinMocha, false);
        assert_ne!(
            selected[(5, 1)].bg,
            selected[(9, 1)].bg,
            "sinking two entries must not merge them into one colour"
        );
    }

    #[test]
    fn ascii_presentation_stays_inside_ascii() {
        let tiles = [tile(0, 0, 8, 4, 1), tile(8, 0, 8, 4, 2)];
        let area = Rect::new(0, 0, 16, 2);
        let buffer = render(&tiles, area, None, ThemeId::CatppuccinMocha, true);
        let rendered = text_of(&buffer);
        assert!(rendered.is_ascii(), "ascii mode emitted {rendered:?}");
        assert!(
            rendered
                .trim()
                .chars()
                .any(|glyph| ASCII_SHADES.contains(&glyph.to_string().as_str())),
            "ascii mode must still fill entries"
        );
    }

    #[test]
    fn monochrome_falls_back_to_shading_density() {
        let tiles = [tile(0, 0, 8, 4, 1)];
        let area = Rect::new(0, 0, 8, 2);
        let buffer = render(&tiles, area, None, ThemeId::Monochrome, false);
        let rendered = text_of(&buffer);
        assert!(
            SHADES.iter().any(|shade| rendered.contains(shade)),
            "a themeless terminal must separate entries by density: {rendered:?}"
        );
    }

    #[test]
    fn monochrome_selected_tile_has_a_visible_reverse_fill() {
        let selected = tile(0, 0, 4, 2, 1);
        let buffer = render_presentation(
            &[selected],
            Rect::new(0, 0, 4, 1),
            Some(0),
            ThemeId::Monochrome,
            false,
            false,
        );
        let cell = &buffer[(0, 0)];
        assert_eq!(cell.symbol(), MONOCHROME_SELECTED_SHADE);
        assert!(cell.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn shaded_selection_uses_visible_fill_and_inverse_label_ink() {
        let mut selected = tile(0, 0, 20, 4, 1);
        selected.name = OsString::from("selected");
        let area = Rect::new(0, 0, 20, 2);
        let theme = Theme::for_id(ThemeId::HighContrast);
        let buffer = render_presentation(
            &[selected.clone()],
            area,
            Some(0),
            ThemeId::HighContrast,
            false,
            true,
        );
        assert_eq!(buffer[(0, 0)].symbol(), SHADES[3]);
        assert!(buffer[(0, 0)].modifier.contains(Modifier::BOLD));
        assert!(!buffer[(0, 0)].modifier.contains(Modifier::REVERSED));

        let label = tile_label(&selected, false).expect("selected tile should have a label");
        let span = rendered_label_span(area, &selected, label.top, &label.first)
            .expect("selected name should be visible");
        let row = u16::try_from(span.row).expect("label row should fit the fixture");
        let label_cell = &buffer[(span.left, row)];
        assert_eq!(label_cell.fg, theme.text_inverse);
        assert_eq!(label_cell.bg, theme.surface_selection);
        assert!(!label_cell.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn shaded_transition_keeps_departing_tiles_visible() {
        let departing = [tile(0, 0, 8, 4, 1)];
        let area = Rect::new(0, 0, 8, 2);
        for (presentation, ascii, monochrome, shade) in [
            ("ASCII", true, false, "+"),
            ("monochrome", false, true, "▓"),
        ] {
            let rendered = text_of(&render_transitioning(
                &[],
                &departing,
                None,
                area,
                ThemeId::CatppuccinMocha,
                ascii,
                monochrome,
            ));
            assert!(
                rendered.contains(shade),
                "an outgoing entry must remain visible in {presentation}: {rendered:?}"
            );
        }
    }

    #[test]
    fn transition_keeps_selected_tile_topmost_and_clears_unselected_modifiers() {
        let mut selected = tile(0, 0, 20, 4, 1);
        selected.name = OsString::from("selected");
        let mut covered = tile(0, 0, 20, 4, 2);
        covered.name = OsString::from("covered");
        let tiles = [selected.clone(), covered.clone()];
        let area = Rect::new(0, 0, 20, 2);
        assert_eq!(
            transition_label_occlusions(area, &tiles, Some(0), false),
            vec![false, true],
            "occlusion must follow the selected-last paint order"
        );
        for (presentation, monochrome) in [("composited", false), ("shaded", true)] {
            let buffer = render_transitioning(
                &tiles,
                &[],
                Some(0),
                area,
                ThemeId::CatppuccinMocha,
                false,
                monochrome,
            );
            let rendered = text_of(&buffer);
            assert!(
                rendered.contains("selected") && !rendered.contains("covered"),
                "the selected entry must own the top label in {presentation}: {rendered}"
            );
            if monochrome {
                let selected_cell = &buffer[(0, 0)];
                assert_eq!(selected_cell.symbol(), SHADES[3]);
                assert!(
                    selected_cell.modifier.contains(Modifier::BOLD),
                    "the visible tile must retain selected shading"
                );
                assert!(!selected_cell.modifier.contains(Modifier::REVERSED));
            }
        }

        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let ink = ShadedInk {
            shades: &SHADES,
            surface: theme.map_surface(),
            text: theme.text_primary,
        };
        let scale = HeatScale::for_tiles(&[selected.clone(), covered.clone()]);
        let mut direct = Buffer::empty(area);
        paint_shaded_tile(&mut direct, area, &selected, ink, true, scale);
        paint_shaded_tile(&mut direct, area, &covered, ink, false, scale);
        assert!(
            !direct[(0, 0)]
                .modifier
                .intersects(Modifier::BOLD | Modifier::REVERSED),
            "an unselected overpaint must remove inherited selection modifiers"
        );
    }

    #[test]
    fn settled_shading_keeps_selection_at_an_odd_half_row_boundary() {
        // These entries tessellate in half-rows but both touch terminal row 1.
        // The selected first entry must retain that shared cell after the tween
        // has settled, even though its unselected sibling comes later in source
        // order.
        let tiles = [tile(0, 0, 4, 3, 1), tile(0, 3, 4, 3, 2)];
        let area = Rect::new(0, 0, 4, 3);
        for (presentation, ascii, monochrome) in
            [("ASCII", true, false), ("monochrome", false, true)]
        {
            let selected_shade = if ascii { ASCII_SHADES[3] } else { SHADES[3] };
            let settled = render_presentation(
                &tiles,
                area,
                Some(0),
                ThemeId::CatppuccinMocha,
                ascii,
                monochrome,
            );
            let transitioning = render_transitioning(
                &tiles,
                &[],
                Some(0),
                area,
                ThemeId::CatppuccinMocha,
                ascii,
                monochrome,
            );
            let boundary = &settled[(0, 1)];
            assert_eq!(
                boundary.symbol(),
                selected_shade,
                "the selected tile must own the shared row in {presentation}"
            );
            assert!(
                boundary.modifier.contains(Modifier::BOLD),
                "the shared row must retain selection styling in {presentation}"
            );
            assert!(!boundary.modifier.contains(Modifier::REVERSED));
            assert_eq!(
                settled.content, transitioning.content,
                "settled and transitioning selection ownership must agree in {presentation}"
            );
        }
    }

    #[test]
    fn uncovered_interval_painting_preserves_disjoint_coverage_spans() {
        // The first span is wholly left of the query. The second is the only
        // predecessor that can cover its left edge; later disjoint spans leave
        // precisely the gaps that need painting.
        let ranges =
            std::collections::BTreeMap::from([(0, 2), (4, 7), (9, 11), (13, 15), (18, 20)]);
        let mut uncovered = Vec::new();
        assert!(paint_uncovered_intervals(&ranges, 5, 18, |start, end| {
            uncovered.push((start, end));
        }));
        assert_eq!(uncovered, vec![(7, 9), (11, 13), (15, 18)]);
    }

    #[test]
    fn fully_covered_transition_tile_is_skipped_without_changing_the_image() {
        let under = tile(0, 0, 4, 4, 1);
        let over = tile(0, 0, 4, 4, 2);
        let tiles = [under.clone(), over.clone()];
        let area = Rect::new(0, 0, 4, 2);
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let palette = MapPalette::for_theme(theme).expect("mocha is truecolour");
        let scale = HeatScale::for_ramp_tiles(&tiles);
        let under_ink = TileInk::resolve(&under, theme, palette, Emphasis::Resting, scale);
        let over_ink = TileInk::resolve(&over, theme, palette, Emphasis::Resting, scale);

        let mut culled = Buffer::empty(area);
        let mut coverage = HalfRowCoverage::new();
        assert!(paint_visible_tile(
            &mut culled,
            area,
            &over,
            &over_ink,
            &mut coverage,
        ));
        assert!(
            !paint_visible_tile(&mut culled, area, &under, &under_ink, &mut coverage),
            "a tile hidden by the full top rectangle must not be rasterized"
        );

        let mut normal = Buffer::empty(area);
        paint_tile(&mut normal, area, &under, &under_ink);
        paint_tile(&mut normal, area, &over, &over_ink);
        assert_eq!(
            culled.content, normal.content,
            "front-to-back culling must match ordinary opaque painting"
        );

        for (presentation, ascii, monochrome) in [
            ("composited", false, false),
            ("ASCII", true, false),
            ("monochrome", false, true),
        ] {
            let transition = render_transitioning(
                &tiles,
                &[],
                None,
                area,
                ThemeId::CatppuccinMocha,
                ascii,
                monochrome,
            );
            let settled = render_presentation(
                &tiles,
                area,
                None,
                ThemeId::CatppuccinMocha,
                ascii,
                monochrome,
            );
            assert_eq!(
                transition.content, settled.content,
                "culling must not change the final {presentation} transition image"
            );
        }
    }

    #[test]
    fn large_fully_covered_transition_label_pivot_uses_region_shortcut() {
        let top = 12;
        let bottom = top + 100_000;
        let left = 4;
        let right = 40;
        let mut coverage = TerminalRowCoverage::new();
        coverage.remember_fully_covered_region(top, bottom, left, right);

        assert!(
            !coverage.cover_region_if_needed(top, bottom, left, right),
            "an identical large label pivot must hit the cache before visiting its rows"
        );
        assert!(
            coverage.rows.is_empty(),
            "the label shortcut must avoid materializing every terminal row"
        );
    }

    #[test]
    fn fully_covered_transition_labels_short_circuit_duplicate_pivots() {
        let area = Rect::new(0, 0, 20, 20_000);
        let mut pivot = tile(
            0,
            0,
            area.width,
            u32::from(area.height) * u32::from(HALF_ROWS_PER_CELL),
            1,
        );
        pivot.name = OsString::from("pivot");
        let tiles = vec![pivot; 64];

        let occlusions = transition_label_occlusions(area, &tiles, None, false);
        assert!(
            occlusions[..tiles.len() - 1]
                .iter()
                .all(|&occluded| occluded),
            "every lower duplicate pivot label must be hidden"
        );
        assert!(
            !occlusions[tiles.len() - 1],
            "the frontmost duplicate pivot label must remain visible"
        );
    }

    #[test]
    fn labels_lead_with_the_name_and_follow_with_the_size() {
        let mut entry = tile(0, 0, 24, 4, 1);
        entry.name = OsString::from("payload.bin");
        entry.size = 4096;
        let area = Rect::new(0, 0, 24, 2);
        let buffer = render(&[entry], area, None, ThemeId::CatppuccinMocha, false);
        let rendered = text_of(&buffer);
        assert!(rendered.contains("payload.bin"), "{rendered:?}");
        assert!(rendered.contains('%'), "{rendered:?}");
    }

    #[test]
    fn detail_lines_choose_a_complete_size_before_clipping() {
        let mut entry = tile(0, 0, 10, 4, 1);
        entry.size = 4_096;
        entry.percentage = 1.0;
        let area = Rect::new(0, 0, 10, 2);
        let buffer = render(&[entry], area, None, ThemeId::CatppuccinMocha, false);
        let detail = row_text(&buffer, area, 1);
        assert!(
            detail.contains("4.0K"),
            "the complete size-only candidate must fit: {detail:?}"
        );
        assert!(
            !detail.contains('·') && !detail.contains('%'),
            "the oversized combined detail must not be clipped into the tile: {detail:?}"
        );
    }

    #[test]
    fn ascii_detail_line_uses_an_ascii_separator() {
        let mut entry = tile(0, 0, 24, 4, 1);
        entry.size = 4_096;
        entry.percentage = 0.5;

        assert_eq!(
            tile_detail_line(&entry, 22, true).as_deref(),
            Some("4.0K . 50%")
        );
    }

    #[test]
    fn detail_lines_round_sizes_before_dropping_the_measurement() {
        let mut entry = tile(0, 0, 6, 4, 1);
        entry.size = 16_384_000;
        entry.percentage = 1.0;
        let area = Rect::new(0, 0, 6, 2);
        let buffer = render(&[entry], area, None, ThemeId::CatppuccinMocha, false);
        let detail = row_text(&buffer, area, 1);
        assert!(
            detail.contains("16M"),
            "the rounded size must survive when the full size does not: {detail:?}"
        );
        assert!(
            !detail.contains('%'),
            "the percentage must not replace a size that has a measured form: {detail:?}"
        );
    }

    #[test]
    fn narrow_uncertain_details_never_fall_back_to_a_percentage() {
        let mut entry = tile(0, 0, 6, 4, 1);
        entry.size = 1_099_511_627_776;
        entry.percentage = 0.5;
        entry.uncertain = true;
        assert_eq!(
            tile_detail_line(&entry, 4, true),
            None,
            "an unfit lower bound must not become an exact-looking percentage"
        );

        let area = Rect::new(0, 0, 6, 2);
        for (ascii, monochrome) in [(false, false), (true, false), (false, true)] {
            let rendered = text_of(&render_presentation(
                &[entry.clone()],
                area,
                None,
                ThemeId::CatppuccinMocha,
                ascii,
                monochrome,
            ));
            assert!(
                !rendered.contains('%'),
                "a narrow uncertain detail must retain uncertainty in ASCII={ascii}, monochrome={monochrome}: {rendered:?}"
            );
        }
    }

    #[test]
    fn labels_sit_on_the_centre_column_of_their_entry() {
        let mut entry = tile(0, 0, 30, 4, 1);
        entry.name = OsString::from("payload.bin");
        entry.size = 4096;
        let area = Rect::new(0, 0, 30, 2);
        let buffer = render(&[entry], area, None, ThemeId::CatppuccinMocha, false);

        let row: Vec<char> = (0..area.width)
            .map(|x| buffer[(x, 0)].symbol().chars().next().unwrap_or(' '))
            .collect();
        let name: Vec<char> = "payload.bin".chars().collect();
        let left = row
            .windows(name.len())
            .position(|window| window == name.as_slice())
            .expect("name should be drawn");
        let right = usize::from(area.width) - left - name.len();
        assert!(
            left.abs_diff(right) <= 1,
            "label should be centred, got {left} columns left and {right} right"
        );
    }

    #[test]
    fn a_two_row_entry_is_still_labelled() {
        let mut entry = tile(0, 0, 20, 4, 1);
        entry.name = OsString::from("compact");
        let area = Rect::new(0, 0, 20, 4);
        let buffer = render(&[entry], area, None, ThemeId::CatppuccinMocha, false);
        assert!(text_of(&buffer).contains("compact"));
    }

    #[test]
    fn labels_only_use_fully_owned_terminal_rows() {
        // The first entry owns only row 1; its lower and upper boundary rows
        // share a cell with neighbours. Its detail must not overwrite row 2.
        let mut upper = tile(0, 1, 20, 4, 1);
        upper.name = OsString::from("upper");
        let lower = tile(0, 5, 20, 4, 2);
        let area = Rect::new(0, 0, 20, 5);
        let buffer = render(&[upper, lower], area, None, ThemeId::CatppuccinMocha, false);
        assert!(row_text(&buffer, area, 1).contains("upper"));
        assert!(
            (area.x..area.right()).all(|x| buffer[(x, 2)].symbol() == HALF_CELL),
            "the partially owned row must remain two half-blocks: {:?}",
            row_text(&buffer, area, 2)
        );
    }

    #[cfg(unix)]
    #[test]
    fn hostile_names_remain_escaped_and_marked_in_dense_labels() {
        use std::os::unix::ffi::OsStringExt as _;

        let mut hostile = tile(0, 0, 40, 4, 1);
        hostile.name = OsString::from_vec(b"bad\xffname".to_vec());
        let area = Rect::new(0, 0, 40, 2);
        let buffer = render(&[hostile], area, None, ThemeId::CatppuccinMocha, false);
        let rendered = text_of(&buffer);
        assert!(rendered.contains("bad\\xffname"));
        assert!(rendered.contains(crate::native_path::DECEPTIVE_DISPLAY_MARKER));
        assert!(!rendered.chars().any(char::is_control));
    }

    #[test]
    fn halfwidth_voicing_mark_filename_is_drawn() {
        let mut entry = tile(0, 0, 20, 4, 1);
        entry.name = OsString::from("\u{ff9e}");
        let rendered = text_of(&render(
            &[entry],
            Rect::new(0, 0, 20, 2),
            None,
            ThemeId::CatppuccinMocha,
            false,
        ));

        assert!(rendered.contains('\u{ff9e}'));
    }

    #[test]
    fn an_empty_folder_says_so() {
        let area = Rect::new(0, 0, 24, 4);
        let buffer = render(&[], area, None, ThemeId::CatppuccinMocha, false);
        assert!(text_of(&buffer).contains("Folder is empty"));
    }

    #[test]
    fn an_unconfirmed_empty_surface_does_not_say_empty() {
        let area = Rect::new(0, 0, 24, 4);
        let mut buffer = Buffer::empty(area);
        DenseRectangleGrid::new(
            MapLayout {
                rectangles: &[],
                departing: &[],
                overflow: None,
                selected_rect_index: None,
                transitioning: false,
                show_empty_label: false,
            },
            Theme::for_id(ThemeId::CatppuccinMocha),
            false,
            false,
        )
        .render(area, &mut buffer);
        assert!(!text_of(&buffer).contains("Folder is empty"));
    }

    #[test]
    fn offscreen_overlap_does_not_occlude_a_visible_transition_label() {
        let mut visible = tile(5, 0, 25, 4, 1);
        visible.name = OsString::from("visible");
        let mut offscreen = tile(0, 0, 10, 4, 2);
        offscreen.name = OsString::from("offscreen");
        let tiles = [visible.clone(), offscreen.clone()];
        let area = Rect::new(10, 0, 20, 2);

        assert_eq!(
            transition_label_occlusions(area, &tiles, None, false),
            vec![false, false],
            "overlap outside the viewport must not hide the visible label"
        );
        for (presentation, ascii, monochrome) in [
            ("composited", false, false),
            ("ASCII", true, false),
            ("monochrome", false, true),
        ] {
            let rendered = text_of(&render_transitioning(
                &tiles,
                &[],
                None,
                area,
                ThemeId::CatppuccinMocha,
                ascii,
                monochrome,
            ));
            assert!(
                rendered.contains("visible"),
                "the visible label must survive an offscreen overlap in {presentation}: {rendered}"
            );
        }
    }

    #[test]
    fn vertical_offscreen_overlap_does_not_occlude_a_visible_transition_label() {
        let mut visible = tile(0, 18, 20, 6, 1);
        visible.name = OsString::from("visible");
        let offscreen = tile(0, 16, 20, 4, 2);
        let tiles = [visible.clone(), offscreen];
        let area = Rect::new(0, 10, 20, 2);

        assert_eq!(
            transition_label_occlusions(area, &tiles, None, false),
            vec![false, false],
            "overlap above the viewport must not hide the visible label"
        );
        let rendered = text_of(&render_transitioning(
            &tiles,
            &[],
            None,
            area,
            ThemeId::CatppuccinMocha,
            false,
            false,
        ));
        assert!(
            rendered.contains("visible"),
            "the visible label must survive a vertical offscreen overlap: {rendered}"
        );
    }

    #[test]
    fn transition_occlusion_checks_rendered_label_cells_and_rows() {
        let area = Rect::new(0, 0, 20, 2);
        let mut horizontal = tile(0, 0, 20, 4, 1);
        horizontal.name = OsString::from("visible");
        let label = tile_label(&horizontal, false).expect("wide entry has a label");
        let span = rendered_label_span(area, &horizontal, label.top, &label.first)
            .expect("name is visible in the viewport");
        let detail_left = label
            .second
            .as_deref()
            .and_then(|second| rendered_label_span(area, &horizontal, label.top + 1, second))
            .map_or(span.left, |detail| detail.left);
        let left_cover_width = span
            .left
            .min(detail_left)
            .checked_sub(area.x)
            .expect("labels must be inset");
        let left_cover = tile(area.x, 0, left_cover_width, 4, 2);
        let left_only = [horizontal.clone(), left_cover];

        assert_eq!(
            transition_label_occlusions(area, &left_only, None, false),
            vec![false, false],
            "body coverage beside the name must not hide the lower label"
        );
        for (presentation, ascii, monochrome) in [
            ("composited", false, false),
            ("ASCII", true, false),
            ("monochrome", false, true),
        ] {
            let rendered = text_of(&render_transitioning(
                &left_only,
                &[],
                None,
                area,
                ThemeId::CatppuccinMocha,
                ascii,
                monochrome,
            ));
            assert!(
                rendered.contains("visible"),
                "the untouched name cells must remain visible in {presentation}: {rendered}"
            );
        }

        let one_cell_cover = tile(span.left, 0, 1, 4, 3);
        let touching = [horizontal.clone(), one_cell_cover];
        assert_eq!(
            transition_label_occlusions(area, &touching, None, false),
            vec![true, false],
            "covering one rendered name cell must hide the whole lower label"
        );

        let vertical_area = Rect::new(0, 0, 20, 6);
        let mut vertical = tile(0, 0, 20, 12, 4);
        vertical.name = OsString::from("visible");
        let top_row = tile(0, 0, 20, 2, 5);
        let vertical_tiles = [vertical, top_row];
        assert_eq!(
            transition_label_occlusions(vertical_area, &vertical_tiles, None, false),
            vec![false, false],
            "coverage on a fully owned but blank row must not hide centred label rows"
        );
        for (presentation, ascii, monochrome) in [
            ("composited", false, false),
            ("ASCII", true, false),
            ("monochrome", false, true),
        ] {
            let rendered = text_of(&render_transitioning(
                &vertical_tiles,
                &[],
                None,
                vertical_area,
                ThemeId::CatppuccinMocha,
                ascii,
                monochrome,
            ));
            assert!(
                rendered.contains("visible"),
                "the untouched centred rows must remain visible in {presentation}: {rendered}"
            );
        }
    }

    #[test]
    fn overlapping_entries_never_blend_their_names_into_one_word() {
        // Mid-transition each entry is interpolated toward its own target, so
        // neighbours pass through each other. Both names are centred in their
        // own entry, so before this was handled the later write landed inside
        // the earlier name and left a word belonging to neither entry.
        let mut under = tile(0, 0, 20, 2, 1);
        under.name = OsString::from("alpha");

        let mut over = tile(4, 0, 20, 2, 2);
        over.name = OsString::from("bravo");
        let area = Rect::new(0, 0, 24, 1);
        assert_eq!(
            transition_label_occlusions(area, &[under.clone(), over.clone()], None, false),
            vec![true, false],
            "the lower label must be hidden before either renderer writes text"
        );

        for (presentation, ascii, monochrome) in [
            ("composited", false, false),
            ("ASCII", true, false),
            ("monochrome", false, true),
        ] {
            let rendered = text_of(&render_transitioning(
                &[under.clone(), over.clone()],
                &[],
                None,
                area,
                ThemeId::CatppuccinMocha,
                ascii,
                monochrome,
            ));
            assert!(
                rendered.contains("bravo"),
                "the entry on top keeps its name in {presentation}: {rendered}"
            );
            assert!(
                !rendered.contains("alph"),
                "the lower name must not leave a corrupt fragment in {presentation}: {rendered}"
            );
        }

        // Settled layouts tessellate, so both names are drawn as before.
        let apart = [tile(0, 0, 10, 2, 1), tile(10, 0, 10, 2, 2)];
        let settled = text_of(&render(&apart, area, None, ThemeId::CatppuccinMocha, false));
        assert!(
            settled.contains("entry-1") && settled.contains("entry-2"),
            "entries that do not overlap keep every name: {settled}"
        );
    }
}
