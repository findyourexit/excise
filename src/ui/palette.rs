//! Perceptual colour construction for the storage map.
//!
//! Map tiles are coloured by size: hue runs along a heat ramp from the smallest
//! entry in the folder to the largest, so colour says the same thing area does
//! and both stay put while a scan fills the map in. Lightness and chroma come
//! from the active theme's own focus accent, which keeps each theme's identity
//! without a hand-authored palette per theme and keeps every entry inside one
//! band, so no entry can out-shout its neighbours.
//!
//! Themes whose palette is not truecolour (monochrome, high contrast) return
//! [`None`] from [`MapPalette::for_theme`]; callers fall back to shading glyphs.

use std::cell::RefCell;
use std::time::Duration;

use ratatui::style::Color;

use crate::theme::Theme;

/// Perceptual lightness of map ink relative to the surrounding panel.
const DARK_TILE_LIFT: f32 = 0.30;
const LIGHT_TILE_DROP: f32 = 0.24;
const DARK_TILE_CEILING: f32 = 0.68;
const LIGHT_TILE_FLOOR: f32 = 0.44;
/// Hue, in turns, of the smallest entries on the map.
const HEAT_COLD_HUE: f32 = 0.72;
/// Hue, in turns, of the largest entry on the map.
///
/// The ramp runs downward from the cold end, so it passes cyan, green, and
/// yellow on the way to red rather than travelling through magenta.
const HEAT_HOT_HUE: f32 = 0.06;
/// Chroma at the hot end, relative to the band, so the entries worth acting on
/// carry the strongest colour on the map.
const HEAT_HOT_CHROMA: f32 = 1.4;
const MIN_CHROMA: f32 = 0.055;
const MAX_CHROMA: f32 = 0.105;
/// Files sit slightly lighter and much calmer than the folders that contain them.
const FILE_CHROMA_SCALE: f32 = 0.55;
const FILE_LIGHTNESS_LIFT: f32 = 0.05;
/// Selection moves away from the canvas while every other entry sinks toward
/// it, so brightness remains a cursor signal in both light and dark themes.
const SELECTION_LIGHTNESS_DISTANCE: f32 = 0.13;
const SELECTION_CHROMA_SCALE: f32 = 0.75;
const UNSELECTED_SINK: f32 = 0.42;
const UNSELECTED_CHROMA_SCALE: f32 = 0.62;
/// A baseline gap keeps selection away from the canvas before rendered contrast
/// is checked against every tone boundary.
const MIN_SELECTION_LIGHTNESS_GAP: f32 = 0.21;
/// Dense grid embosses a tile's top and trailing boundaries by these offsets.
/// Keep these in lockstep with its `TileInk` derivation: selection clearance is
/// calculated here, where the derived palette can cache both tone bands.
pub(crate) const TILE_CROWN_LIFT: f32 = 0.055;
pub(crate) const TILE_BASE_DROP: f32 = 0.055;
pub(crate) const TILE_SELECTED_BASE_DROP: f32 = 0.0;
pub(crate) const TILE_SELECTED_EDGE_DROP: f32 = 0.0;
pub(crate) const TILE_EDGE_DROP: f32 = 0.085;
/// Selected and unselected fills need the non-text contrast floor because the
/// map has no outline to carry the cursor when their brightness converges.
#[cfg(test)]
const MIN_SELECTION_CONTRAST: f32 = 3.0;
/// Leave a small margin for rounded boundary samples between cached endpoints.
const MIN_SELECTION_BOUNDARY_CONTRAST: f32 = 3.5;
/// Focus borders and other non-text emphasis must clear this contrast floor.
pub(crate) const MIN_FOCUS_CONTRAST: f32 = 3.0;
/// The name line needs normal text contrast against its rendered tile fill.
const LEAD_CONTRAST_FLOOR: f32 = 4.5;
/// The size and percentage line uses normal-size text and the normal contrast floor.
const DETAIL_CONTRAST_FLOOR: f32 = 4.5;
const DETAIL_TOWARD_FILL: f32 = 0.34;
const BOUNDARY_SAMPLE_COUNT: u16 = 256;

/// How far the remainder's stipple lifts off the canvas. One step is enough to
/// read as texture at a glance and small enough to stay behind every drawn
/// entry, however much of the map the remainder covers.
const GRAIN_LIFT: f32 = 0.06;

/// A colour in Oklch: perceptual lightness, chroma, and hue measured in turns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Oklch {
    pub lightness: f32,
    pub chroma: f32,
    pub hue: f32,
}

impl Oklch {
    /// Decomposes a truecolour value, returning [`None`] for palette colours.
    pub fn from_color(color: Color) -> Option<Self> {
        let Color::Rgb(red, green, blue) = color else {
            return None;
        };
        Some(Self::from_rgb(red, green, blue))
    }

    #[must_use]
    pub fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        let (lightness, a, b) = linear_to_oklab(
            srgb_to_linear(red),
            srgb_to_linear(green),
            srgb_to_linear(blue),
        );
        let chroma = a.hypot(b);
        let hue = if chroma <= f32::EPSILON {
            0.0
        } else {
            b.atan2(a) / std::f32::consts::TAU
        };
        Self {
            lightness,
            chroma,
            hue: hue.rem_euclid(1.0),
        }
    }

    #[must_use]
    fn to_rgb(self) -> (u8, u8, u8) {
        let angle = self.hue.rem_euclid(1.0) * std::f32::consts::TAU;
        let (red, green, blue) = oklab_to_linear(
            self.lightness.clamp(0.0, 1.0),
            self.chroma.max(0.0) * angle.cos(),
            self.chroma.max(0.0) * angle.sin(),
        );
        (
            linear_to_srgb(red),
            linear_to_srgb(green),
            linear_to_srgb(blue),
        )
    }

    #[must_use]
    pub fn to_color(self) -> Color {
        let (red, green, blue) = self.to_rgb();
        Color::Rgb(red, green, blue)
    }

    #[must_use]
    pub fn shifted(self, lightness: f32, chroma_scale: f32) -> Self {
        Self {
            lightness: (self.lightness + lightness).clamp(0.0, 1.0),
            chroma: (self.chroma * chroma_scale).clamp(0.0, 0.37),
            hue: self.hue,
        }
    }

    /// Interpolates toward `other`, taking the shorter way around the hue circle.
    #[must_use]
    pub fn towards(self, other: Self, amount: f32) -> Self {
        let amount = amount.clamp(0.0, 1.0);
        let mut delta = other.hue - self.hue;
        if delta > 0.5 {
            delta -= 1.0;
        } else if delta < -0.5 {
            delta += 1.0;
        }
        Self {
            lightness: (other.lightness - self.lightness).mul_add(amount, self.lightness),
            chroma: (other.chroma - self.chroma).mul_add(amount, self.chroma),
            hue: delta.mul_add(amount, self.hue).rem_euclid(1.0),
        }
    }

    /// Label inks for text drawn on this tone: a lead colour and a softer detail.
    ///
    /// Both carry the tone's own hue when that can still clear their rendered
    /// contrast floor. A neutral fallback wins over a tinted label readers
    /// cannot distinguish from its tile.
    #[must_use]
    pub fn inks(self) -> (Color, Color) {
        let fill = self.to_rgb();
        let dark = Self {
            lightness: 0.14,
            chroma: self.chroma.min(0.045),
            hue: self.hue,
        };
        let light = Self {
            lightness: 0.98,
            chroma: self.chroma.min(0.030),
            hue: self.hue,
        };
        let lead = tinted_ink(fill, [dark.to_rgb(), light.to_rgb()], LEAD_CONTRAST_FLOOR)
            .map_or_else(|| strongest_neutral(fill), |ink| ink);
        let detail = tinted_ink(
            fill,
            [
                dark.towards(self, DETAIL_TOWARD_FILL).to_rgb(),
                light.towards(self, DETAIL_TOWARD_FILL).to_rgb(),
            ],
            DETAIL_CONTRAST_FLOOR,
        )
        .map_or_else(|| strongest_neutral(fill), |ink| ink);
        (color_from_rgb(lead), color_from_rgb(detail))
    }
}
fn color_from_rgb((red, green, blue): (u8, u8, u8)) -> Color {
    Color::Rgb(red, green, blue)
}

fn tinted_ink(
    fill: (u8, u8, u8),
    candidates: [(u8, u8, u8); 2],
    floor: f32,
) -> Option<(u8, u8, u8)> {
    let mut choice = None;
    let mut choice_contrast = f32::INFINITY;
    for candidate in candidates {
        let contrast = contrast_ratio(fill, candidate);
        if contrast >= floor && contrast < choice_contrast {
            choice = Some(candidate);
            choice_contrast = contrast;
        }
    }
    choice
}

fn strongest_neutral(fill: (u8, u8, u8)) -> (u8, u8, u8) {
    let black = (0, 0, 0);
    let white = (u8::MAX, u8::MAX, u8::MAX);
    if contrast_ratio(fill, black) >= contrast_ratio(fill, white) {
        black
    } else {
        white
    }
}

fn contrast_ratio(first: (u8, u8, u8), second: (u8, u8, u8)) -> f32 {
    let first = relative_luminance(first);
    let second = relative_luminance(second);
    let (lighter, darker) = if first >= second {
        (first, second)
    } else {
        (second, first)
    };
    (lighter + 0.05) / (darker + 0.05)
}

fn relative_luminance((red, green, blue): (u8, u8, u8)) -> f32 {
    0.2126f32.mul_add(
        srgb_to_linear(red),
        0.7152f32.mul_add(srgb_to_linear(green), 0.0722 * srgb_to_linear(blue)),
    )
}

/// Which role a tile plays, controlling how far it sits inside the band.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TileTone {
    Folder,
    File,
}

/// The map's derived colour band for one theme.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MapPalette {
    backdrop: Oklch,
    lightness: f32,
    chroma: f32,
    unselected_min_edge_lightness: f32,
    unselected_max_crown_lightness: f32,
    unselected_min_edge_luminance: f32,
    unselected_max_crown_luminance: f32,
}

/// How an entry sits relative to the map's cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Emphasis {
    /// Nothing is selected: every entry rests on the band.
    Resting,
    /// The cursor entry, lifted out of the band.
    Selected,
    /// Everything the cursor is not, sunk toward the canvas.
    Unselected,
}

impl MapPalette {
    /// Derives the band from a theme, or [`None`] when the theme is not truecolour.
    pub fn for_theme(theme: Theme) -> Option<Self> {
        let focus = Oklch::from_color(theme.focus)?;
        let panel = Oklch::from_color(theme.surface_panel)?;
        let light_theme = panel.lightness > 0.5;
        let lightness = if light_theme {
            (panel.lightness - LIGHT_TILE_DROP).max(LIGHT_TILE_FLOOR)
        } else {
            (panel.lightness + DARK_TILE_LIFT).min(DARK_TILE_CEILING)
        };
        // The canvas sits a touch *above* the panel rather than below it. Space
        // the treemap cannot allocate — rounding slivers, and the gaps that open
        // mid-zoom — then reads as part of the surface instead of as a hole
        // punched through the interface.
        let backdrop = Oklch {
            lightness: if light_theme {
                (panel.lightness - 0.04).max(0.70)
            } else {
                panel.lightness + 0.04
            },
            chroma: panel.chroma * 0.9,
            hue: panel.hue,
        };
        let file_lightness = (lightness + FILE_LIGHTNESS_LIFT).clamp(0.0, 1.0);
        let folder_unselected_lightness =
            (backdrop.lightness - lightness).mul_add(UNSELECTED_SINK, lightness);
        let file_unselected_lightness =
            (backdrop.lightness - file_lightness).mul_add(UNSELECTED_SINK, file_lightness);
        let folder_unselected_edge_lightness =
            (folder_unselected_lightness - TILE_EDGE_DROP).max(0.0);
        let file_unselected_edge_lightness = (file_unselected_lightness - TILE_EDGE_DROP).max(0.0);
        let folder_unselected_crown_lightness =
            (folder_unselected_lightness + TILE_CROWN_LIFT).min(1.0);
        let file_unselected_crown_lightness =
            (file_unselected_lightness + TILE_CROWN_LIFT).min(1.0);
        let mut palette = Self {
            backdrop,
            lightness,
            chroma: focus.chroma.clamp(MIN_CHROMA, MAX_CHROMA),
            unselected_min_edge_lightness: folder_unselected_edge_lightness
                .min(file_unselected_edge_lightness),
            unselected_max_crown_lightness: folder_unselected_crown_lightness
                .max(file_unselected_crown_lightness),
            unselected_min_edge_luminance: f32::INFINITY,
            unselected_max_crown_luminance: f32::NEG_INFINITY,
        };
        for step in 0..=BOUNDARY_SAMPLE_COUNT {
            let heat = f32::from(step) / f32::from(BOUNDARY_SAMPLE_COUNT);
            for tone in [TileTone::Folder, TileTone::File] {
                let unselected = palette.emphasised(palette.tile(heat, tone), Emphasis::Unselected);
                palette.unselected_min_edge_luminance =
                    palette
                        .unselected_min_edge_luminance
                        .min(relative_luminance(
                            unselected.shifted(-TILE_EDGE_DROP, 1.0).to_rgb(),
                        ));
                palette.unselected_max_crown_luminance = palette
                    .unselected_max_crown_luminance
                    .max(relative_luminance(
                        unselected.shifted(TILE_CROWN_LIFT, 1.0).to_rgb(),
                    ));
            }
        }
        Some(palette)
    }

    /// The canvas the tiles are tessellated onto.
    #[must_use]
    pub fn backdrop(self) -> Color {
        self.backdrop.to_color()
    }

    /// The stipple the entries too small to draw are shown as.
    ///
    /// The remainder is the least important region of the map, so its texture
    /// is a lift off the canvas rather than an ink: enough to read as a field
    /// of entries, quiet enough that a folder made entirely of small files does
    /// not shimmer.
    #[must_use]
    pub fn grain(self) -> Color {
        self.backdrop.shifted(GRAIN_LIFT, 1.0).to_color()
    }

    /// A selected tile moves away from the canvas while its neighbours move
    /// toward it, so brightness remains a cursor signal in light and dark themes.
    /// Its floor is measured against the closest rendered unselected crown or
    /// edge in either tone band, rather than only its own source tone.
    #[must_use]
    pub fn emphasised(self, tone: Oklch, emphasis: Emphasis) -> Oklch {
        match emphasis {
            Emphasis::Resting => tone,
            Emphasis::Selected => {
                let direction = if tone.lightness >= self.backdrop.lightness {
                    1.0
                } else {
                    -1.0
                };
                let selected = tone.shifted(
                    SELECTION_LIGHTNESS_DISTANCE * direction,
                    SELECTION_CHROMA_SCALE,
                );
                let selected_boundary_offset = if direction > 0.0 {
                    TILE_SELECTED_EDGE_DROP
                } else {
                    TILE_CROWN_LIFT
                };
                let required_lightness = (self.nearest_unselected_boundary_lightness(direction)
                    + direction * (MIN_SELECTION_LIGHTNESS_GAP + selected_boundary_offset))
                    .clamp(0.0, 1.0);
                let selected = Oklch {
                    lightness: if direction > 0.0 {
                        selected.lightness.max(required_lightness)
                    } else {
                        selected.lightness.min(required_lightness)
                    },
                    ..selected
                };
                self.enforce_selection_contrast(selected, direction)
            }
            Emphasis::Unselected => Oklch {
                lightness: self.unselected_lightness(tone),
                chroma: tone.chroma * UNSELECTED_CHROMA_SCALE,
                hue: tone.hue,
            },
        }
    }

    fn unselected_lightness(self, tone: Oklch) -> f32 {
        (self.backdrop.lightness - tone.lightness).mul_add(UNSELECTED_SINK, tone.lightness)
    }

    fn nearest_unselected_boundary_lightness(self, direction: f32) -> f32 {
        if direction > 0.0 {
            self.unselected_max_crown_lightness
        } else {
            self.unselected_min_edge_lightness
        }
    }

    fn enforce_selection_contrast(self, selected: Oklch, direction: f32) -> Oklch {
        if self.selection_contrast_is_sufficient(selected, direction) {
            return selected;
        }

        let mut low = 0.0;
        let mut high = 1.0;
        for _ in 0..8 {
            let distance = f32::midpoint(low, high);
            let candidate = Oklch {
                lightness: (selected.lightness + direction * distance).clamp(0.0, 1.0),
                ..selected
            };
            if self.selection_contrast_is_sufficient(candidate, direction) {
                high = distance;
            } else {
                low = distance;
            }
        }
        let adjusted = Oklch {
            lightness: (selected.lightness + direction * high).clamp(0.0, 1.0),
            ..selected
        };
        if self.selection_contrast_is_sufficient(adjusted, direction) {
            adjusted
        } else {
            Oklch {
                lightness: if direction > 0.0 { 1.0 } else { 0.0 },
                chroma: 0.0,
                hue: selected.hue,
            }
        }
    }
    fn selection_contrast_is_sufficient(self, selected: Oklch, direction: f32) -> bool {
        let selected_boundary = if direction > 0.0 {
            selected.shifted(-TILE_SELECTED_EDGE_DROP, 1.0)
        } else {
            selected.shifted(TILE_CROWN_LIFT, 1.0)
        };
        let selected_luminance = relative_luminance(selected_boundary.to_rgb());
        let other_luminance = if direction > 0.0 {
            self.unselected_max_crown_luminance
        } else {
            self.unselected_min_edge_luminance
        };
        let contrast = if direction > 0.0 {
            (selected_luminance + 0.05) / (other_luminance + 0.05)
        } else {
            (other_luminance + 0.05) / (selected_luminance + 0.05)
        };
        contrast >= MIN_SELECTION_BOUNDARY_CONTRAST
    }

    /// Places one entry on the heat ramp.
    ///
    /// `heat` is the entry's size measured against the largest entry drawn
    /// beside it, so the ramp always spends its whole range on the folder in
    /// front of the reader: the biggest entry is red and the smallest is blue
    /// whether the folder holds two entries or two thousand.
    #[must_use]
    pub fn tile(self, heat: f32, tone: TileTone) -> Oklch {
        let heat = heat.clamp(0.0, 1.0);
        let base = Oklch {
            lightness: self.lightness,
            chroma: self.chroma * (HEAT_HOT_CHROMA - 1.0).mul_add(heat, 1.0),
            hue: (HEAT_HOT_HUE - HEAT_COLD_HUE).mul_add(heat, HEAT_COLD_HUE),
        };
        match tone {
            TileTone::Folder => base,
            TileTone::File => base.shifted(FILE_LIGHTNESS_LIFT, FILE_CHROMA_SCALE),
        }
    }

    /// Pulls a semantic colour into the band, preserving only its hue signal.
    #[must_use]
    pub fn semantic(self, color: Color) -> Oklch {
        Oklch::from_color(color).map_or_else(
            || self.tile(0.5, TileTone::File),
            |source| Oklch {
                lightness: self.lightness,
                chroma: source.chroma.clamp(MIN_CHROMA, MAX_CHROMA * 1.4),
                hue: source.hue,
            },
        )
    }
}

/// Samples in one revolution of the border cycle.
pub(crate) const CYCLE_LEN: usize = 44;
/// Samples advanced per second, giving a revolution every 1.47 seconds.
const CYCLE_RATE: u128 = 30;
const MILLIS_PER_SECOND: u128 = 1_000;
const CYCLE_LIGHTNESS_SWING: f32 = 0.16;
const CYCLE_CHROMA_SWING: f32 = 0.22;
const CYCLE_HUE_SWING: f32 = 0.045;

/// The title chip foreground treatment paired with one animated cycle sample.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CycleInk {
    /// Truecolour samples use a measured foreground against their fill.
    Foreground(Color),
    /// Palette samples and forced monochrome output use terminal reverse video.
    Reversed,
}

/// A closed loop of accent shades that travels around a border.
///
/// Modelled on exabind's `selected_category` effect, which advances an index at
/// thirty samples per second. Each pane maps that phase evenly across its own
/// perimeter, so its last cell joins the first even when its length differs
/// from this cycle's sample count. The loop is generated in Oklch rather than
/// exabind's piecewise HSL table, avoiding a travelling seam.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ColorCycle {
    samples: [Color; CYCLE_LEN],
    chip_inks: [CycleInk; CYCLE_LEN],
}

impl ColorCycle {
    /// Returns whether this accent produces a changing truecolour cycle.
    #[must_use]
    pub(crate) const fn can_animate(accent: Color) -> bool {
        matches!(accent, Color::Rgb(_, _, _))
    }

    fn from_accent_against(accent: Color, panel: Color) -> Option<Self> {
        let base = Oklch::from_color(accent)?;
        let mut samples = [accent; CYCLE_LEN];
        for (index, sample) in samples.iter_mut().enumerate() {
            let angle = index as f32 / CYCLE_LEN as f32 * std::f32::consts::TAU;
            let candidate = Oklch {
                lightness: CYCLE_LIGHTNESS_SWING
                    .mul_add(angle.sin(), base.lightness)
                    .clamp(0.0, 1.0),
                chroma: base.chroma * CYCLE_CHROMA_SWING.mul_add((angle * 2.0).sin(), 1.0),
                hue: CYCLE_HUE_SWING.mul_add(angle.cos(), base.hue),
            };
            *sample = constrained_cycle_sample(candidate, base, accent, panel);
        }
        Some(Self {
            samples,
            chip_inks: [CycleInk::Reversed; CYCLE_LEN],
        })
    }

    fn solid(accent: Color) -> Self {
        Self {
            samples: [accent; CYCLE_LEN],
            chip_inks: [CycleInk::Reversed; CYCLE_LEN],
        }
    }

    fn for_theme_with_monochrome(theme: &Theme, monochrome: bool) -> Self {
        let mut cycle = match Self::from_accent_against(theme.focus, theme.surface_panel) {
            Some(cycle) => cycle,
            None => Self::solid(theme.focus),
        };
        for (sample, ink) in cycle.samples.iter().zip(&mut cycle.chip_inks) {
            *ink = title_chip_ink(*sample, theme, monochrome);
        }
        cycle
    }

    #[must_use]
    pub fn at(&self, step: usize) -> Color {
        self.samples[step % CYCLE_LEN]
    }

    /// Returns the sample at a border position, distributed around the full
    /// perimeter so the final cell joins the first without a fixed-loop seam.
    #[must_use]
    pub(crate) fn at_perimeter(&self, step: usize, position: usize, perimeter: usize) -> Color {
        self.samples[perimeter_phase(step, position, perimeter)]
    }

    /// Returns the precomputed title-chip treatment at a border position.
    #[must_use]
    pub(crate) fn chip_at_perimeter(
        &self,
        step: usize,
        position: usize,
        perimeter: usize,
    ) -> (Color, CycleInk) {
        let index = perimeter_phase(step, position, perimeter);
        (self.samples[index], self.chip_inks[index])
    }
}

fn perimeter_phase(step: usize, position: usize, perimeter: usize) -> usize {
    if perimeter == 0 {
        return step % CYCLE_LEN;
    }
    let position = position % perimeter;
    let position = u128::try_from(position).unwrap_or(0);
    let perimeter = u128::try_from(perimeter).unwrap_or(1);
    let offset =
        usize::try_from(position.saturating_mul(CYCLE_LEN as u128) / perimeter).unwrap_or(0);
    (step % CYCLE_LEN + offset) % CYCLE_LEN
}

fn title_chip_ink(fill: Color, theme: &Theme, monochrome: bool) -> CycleInk {
    if monochrome {
        return CycleInk::Reversed;
    }
    let Color::Rgb(red, green, blue) = fill else {
        return CycleInk::Reversed;
    };
    let rendered_fill = (red, green, blue);
    let preferred = if Oklch::from_rgb(red, green, blue).lightness > 0.58 {
        [theme.surface_base, theme.text_primary]
    } else {
        [theme.text_primary, theme.surface_base]
    };
    // Keep the existing semantic polarity when it is readable, but let a
    // measured contrast check select the other semantic role for light themes.
    for candidate in preferred {
        let Color::Rgb(red, green, blue) = candidate else {
            continue;
        };
        if contrast_ratio(rendered_fill, (red, green, blue)) >= LEAD_CONTRAST_FLOOR {
            return CycleInk::Foreground(candidate);
        }
    }
    CycleInk::Foreground(color_from_rgb(strongest_neutral(rendered_fill)))
}

fn constrained_cycle_sample(candidate: Oklch, base: Oklch, accent: Color, panel: Color) -> Color {
    let Some(panel_oklch) = Oklch::from_color(panel) else {
        return candidate.to_color();
    };
    let Some(base_contrast) = color_contrast(accent, panel) else {
        return candidate.to_color();
    };
    let required_contrast = base_contrast.max(MIN_FOCUS_CONTRAST);
    let candidate_color = candidate.to_color();
    if color_contrast(candidate_color, panel).is_some_and(|contrast| contrast >= required_contrast)
    {
        return candidate_color;
    }

    let away_from_panel = if panel_oklch.lightness >= base.lightness {
        -1.0
    } else {
        1.0
    };
    if let Some(sample) =
        nearest_constrained_cycle_sample(candidate, base, panel, required_contrast, away_from_panel)
    {
        return sample;
    }

    let Color::Rgb(panel_red, panel_green, panel_blue) = panel else {
        return accent;
    };
    let neutral = color_from_rgb(strongest_neutral((panel_red, panel_green, panel_blue)));
    if color_contrast(neutral, panel).is_some_and(|contrast| contrast >= required_contrast) {
        neutral
    } else {
        accent
    }
}

/// Finds the nearest chromatic sample that keeps the focus cycle away from its panel.
fn nearest_constrained_cycle_sample(
    candidate: Oklch,
    base: Oklch,
    panel: Color,
    required_contrast: f32,
    away_from_panel: f32,
) -> Option<Color> {
    const SEARCH_STEPS: u16 = 64;
    let start = (candidate.lightness - base.lightness).abs();
    let maximum = if away_from_panel < 0.0 {
        base.lightness
    } else {
        1.0 - base.lightness
    };
    let maximum = maximum.max(start);
    for step in 0..=SEARCH_STEPS {
        let fraction = f32::from(step) / f32::from(SEARCH_STEPS);
        let distance = (maximum - start).mul_add(fraction, start);
        let sample = Oklch {
            lightness: (base.lightness + away_from_panel * distance).clamp(0.0, 1.0),
            ..candidate
        }
        .to_color();
        if color_contrast(sample, panel).is_some_and(|contrast| contrast >= required_contrast) {
            return Some(sample);
        }
    }
    None
}

fn color_contrast(first: Color, second: Color) -> Option<f32> {
    match (first, second) {
        (
            Color::Rgb(first_red, first_green, first_blue),
            Color::Rgb(second_red, second_green, second_blue),
        ) => Some(contrast_ratio(
            (first_red, first_green, first_blue),
            (second_red, second_green, second_blue),
        )),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DerivedKey {
    focus: Color,
    panel: Color,
    surface_base: Color,
    text_primary: Color,
    monochrome: bool,
}

#[derive(Clone, Copy, Debug)]
struct DerivedPalette {
    key: DerivedKey,
    cycle: ColorCycle,
    map: Option<MapPalette>,
}

std::thread_local! {
    static DERIVED_PALETTE: RefCell<[Option<DerivedPalette>; 2]> =
        const { RefCell::new([None, None]) };
}

/// Returns the colour data whose derivation depends only on the active theme.
///
/// Rendering runs on one synchronous thread, so retaining one entry per
/// capability variant avoids rebuilding the accent loop and map band for every frame.
#[must_use]
pub(crate) fn derived_for(theme: Theme) -> (ColorCycle, Option<MapPalette>) {
    derived_for_with_monochrome(theme, false)
}

/// Returns the colour data for a terminal with or without colour output.
///
/// The capability participates in the cache key because an RGB theme still
/// needs explicit reverse-video title chips when the global monochrome pass
/// will remove its colours.
#[must_use]
pub(crate) fn derived_for_with_monochrome(
    theme: Theme,
    monochrome: bool,
) -> (ColorCycle, Option<MapPalette>) {
    let key = DerivedKey {
        focus: theme.focus,
        panel: theme.surface_panel,
        surface_base: theme.surface_base,
        text_primary: theme.text_primary,
        monochrome,
    };
    let slot = usize::from(monochrome);
    DERIVED_PALETTE.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some(derived) = memo[slot] {
            if derived.key == key {
                return (derived.cycle, derived.map);
            }
        }
        let derived = DerivedPalette {
            key,
            cycle: ColorCycle::for_theme_with_monochrome(&theme, monochrome),
            map: MapPalette::for_theme(theme),
        };
        memo[slot] = Some(derived);
        (derived.cycle, derived.map)
    })
}

/// The cycle sample `now` lands on, advancing at exabind's cadence.
#[must_use]
pub(crate) fn cycle_step(now: Duration) -> usize {
    let steps = now.as_millis().saturating_mul(CYCLE_RATE) / MILLIS_PER_SECOND;
    // `u128: From<usize>` does not exist, because a pointer's width is a
    // property of the target rather than of the language. The cycle length is a
    // small compile-time constant, so widening it is exact everywhere, and the
    // reduced phase is always back inside `usize` however narrow that is.
    let phase = steps % CYCLE_LEN as u128;
    usize::try_from(phase).unwrap_or(0)
}

fn srgb_to_linear(channel: u8) -> f32 {
    let value = f32::from(channel) / 255.0;
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(value: f32) -> u8 {
    let value = value.clamp(0.0, 1.0);
    let encoded = if value <= 0.003_130_8 {
        value * 12.92
    } else {
        value.powf(1.0 / 2.4).mul_add(1.055, -0.055)
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Björn Ottosson's linear sRGB to Oklab transform.
fn linear_to_oklab(red: f32, green: f32, blue: f32) -> (f32, f32, f32) {
    let long = 0.051_445_993f32.mul_add(blue, 0.412_221_46f32.mul_add(red, 0.536_332_55 * green));
    let medium = 0.107_396_96f32.mul_add(blue, 0.211_903_5f32.mul_add(red, 0.680_699_5 * green));
    let short = 0.629_978_7f32.mul_add(blue, 0.088_302_46f32.mul_add(red, 0.281_718_85 * green));
    let long = long.cbrt();
    let medium = medium.cbrt();
    let short = short.cbrt();
    (
        (-0.004_072_047f32).mul_add(short, 0.210_454_26f32.mul_add(long, 0.793_617_8 * medium)),
        0.450_593_7f32.mul_add(short, 1.977_998_5f32.mul_add(long, -(2.428_592_2 * medium))),
        (-0.808_675_77f32).mul_add(short, 0.025_904_037f32.mul_add(long, 0.782_771_77 * medium)),
    )
}

/// Björn Ottosson's Oklab to linear sRGB transform.
fn oklab_to_linear(lightness: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let long = 0.215_803_76f32
        .mul_add(b, 0.396_337_78f32.mul_add(a, lightness))
        .powi(3);
    let medium = (-0.063_854_17f32)
        .mul_add(b, (-0.105_561_346f32).mul_add(a, lightness))
        .powi(3);
    let short = (-1.291_485_5f32)
        .mul_add(b, (-0.089_484_18f32).mul_add(a, lightness))
        .powi(3);
    (
        0.230_969_94f32.mul_add(short, 4.076_741_7f32.mul_add(long, -(3.307_711_6 * medium))),
        (-0.341_319_38f32).mul_add(short, (-1.268_438f32).mul_add(long, 2.609_757_4 * medium)),
        1.707_614_7f32.mul_add(
            short,
            (-0.004_196_086_3f32).mul_add(long, -(0.703_418_6 * medium)),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeId;

    fn channels(color: Color) -> (u8, u8, u8) {
        match color {
            Color::Rgb(red, green, blue) => (red, green, blue),
            other => panic!("expected truecolour, got {other:?}"),
        }
    }

    fn assert_ink_contrast(
        id: ThemeId,
        tone: TileTone,
        heat: f32,
        emphasis: Emphasis,
        fill: Oklch,
    ) {
        let rendered_fill = fill.to_rgb();
        let (lead, detail) = fill.inks();
        assert!(
            contrast_ratio(rendered_fill, channels(lead)) >= LEAD_CONTRAST_FLOOR,
            "{id:?} {tone:?} at heat {heat} with {emphasis:?} has unreadable lead ink"
        );
        assert!(
            contrast_ratio(rendered_fill, channels(detail)) >= DETAIL_CONTRAST_FLOOR,
            "{id:?} {tone:?} at heat {heat} with {emphasis:?} has unreadable detail ink"
        );
    }

    #[test]
    fn oklch_round_trips_within_one_channel_step() {
        for sample in [
            (30u8, 30u8, 46u8),
            (166, 227, 161),
            (243, 139, 168),
            (255, 255, 255),
            (0, 0, 0),
            (7, 54, 66),
        ] {
            let (red, green, blue) = sample;
            let (out_red, out_green, out_blue) =
                channels(Oklch::from_rgb(red, green, blue).to_color());
            assert!(
                out_red.abs_diff(red) <= 1
                    && out_green.abs_diff(green) <= 1
                    && out_blue.abs_diff(blue) <= 1,
                "{sample:?} round-tripped to {:?}",
                (out_red, out_green, out_blue)
            );
        }
    }

    #[test]
    fn every_truecolour_theme_derives_a_map_band() {
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            let derived = MapPalette::for_theme(theme);
            if matches!(id, ThemeId::Monochrome | ThemeId::HighContrast) {
                assert!(derived.is_none(), "{id:?} must fall back to shading");
                continue;
            }
            let palette = derived.unwrap_or_else(|| panic!("{id:?} has no map band"));
            assert_ne!(
                palette.backdrop(),
                theme.surface_panel,
                "{id:?} backdrop must separate the map from its pane"
            );
        }
    }

    #[test]
    fn derived_palette_invalidates_when_the_theme_changes() {
        let dark = Theme::for_id(ThemeId::ExciseDark);
        let light = Theme::for_id(ThemeId::ExciseLight);
        let (dark_cycle, dark_palette) = derived_for(dark);
        let (light_cycle, light_palette) = derived_for(light);
        assert_ne!(dark_cycle.at(0), light_cycle.at(0));
        let (Some(dark_palette), Some(light_palette)) = (dark_palette, light_palette) else {
            panic!("built-in truecolour themes need a map palette");
        };
        assert_ne!(dark_palette.backdrop(), light_palette.backdrop());
        let (dark_cycle_again, _) = derived_for(dark);
        assert_eq!(dark_cycle_again.at(0), dark_cycle.at(0));
    }

    #[test]
    fn tiles_share_one_lightness_so_no_entry_dominates() {
        let palette = MapPalette::for_theme(Theme::for_id(ThemeId::CatppuccinMocha))
            .expect("mocha is truecolour");
        let lightnesses: Vec<f32> = (0..16)
            .map(|step| palette.tile(step as f32 / 16.0, TileTone::Folder).lightness)
            .collect();
        let first = lightnesses[0];
        for lightness in lightnesses {
            assert!((lightness - first).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn the_ramp_runs_from_blue_at_the_bottom_to_red_at_the_top() {
        let palette = MapPalette::for_theme(Theme::for_id(ThemeId::CatppuccinMocha))
            .expect("mocha is truecolour");
        let Color::Rgb(cold_r, _, cold_b) = palette.tile(0.0, TileTone::Folder).to_color() else {
            panic!("map ink is truecolour");
        };
        let Color::Rgb(hot_r, _, hot_b) = palette.tile(1.0, TileTone::Folder).to_color() else {
            panic!("map ink is truecolour");
        };
        assert!(
            cold_b > cold_r,
            "the smallest entry reads blue: {cold_r},{cold_b}"
        );
        assert!(
            hot_r > hot_b,
            "the largest entry reads red: {hot_r},{hot_b}"
        );
    }

    #[test]
    fn every_step_up_in_size_moves_further_along_the_ramp() {
        let palette = MapPalette::for_theme(Theme::for_id(ThemeId::CatppuccinMocha))
            .expect("mocha is truecolour");
        let steps: Vec<Oklch> = (0..32)
            .map(|step| palette.tile(step as f32 / 31.0, TileTone::Folder))
            .collect();
        for pair in steps.windows(2) {
            assert!(
                pair[1].hue < pair[0].hue,
                "hue must fall as entries grow: {} then {}",
                pair[0].hue,
                pair[1].hue
            );
            assert!(
                pair[1].chroma >= pair[0].chroma,
                "colour must not weaken as entries grow: {} then {}",
                pair[0].chroma,
                pair[1].chroma
            );
        }
    }

    #[test]
    fn the_ramp_does_not_pass_through_magenta() {
        let palette = MapPalette::for_theme(Theme::for_id(ThemeId::CatppuccinMocha))
            .expect("mocha is truecolour");
        for step in 0..64 {
            let hue = palette.tile(step as f32 / 63.0, TileTone::Folder).hue;
            assert!(
                (HEAT_HOT_HUE..=HEAT_COLD_HUE).contains(&hue),
                "hue {hue} left the ramp"
            );
        }
    }

    #[test]
    fn every_truecolour_theme_keeps_selection_and_labels_distinguishable() {
        for id in ThemeId::ALL {
            let (_, palette) = derived_for(Theme::for_id(id));
            let Some(palette) = palette else {
                assert!(
                    matches!(id, ThemeId::Monochrome | ThemeId::HighContrast),
                    "{id:?} unexpectedly has no map palette"
                );
                continue;
            };
            for tone in [TileTone::Folder, TileTone::File] {
                for step in 0_u8..=64 {
                    let heat = f32::from(step) / 64.0;
                    let resting = palette.tile(heat, tone);
                    let selected = palette.emphasised(resting, Emphasis::Selected);
                    let unselected = palette.emphasised(resting, Emphasis::Unselected);
                    let resting_distance = (resting.lightness - palette.backdrop.lightness).abs();
                    assert!(
                        (selected.lightness - palette.backdrop.lightness).abs() > resting_distance,
                        "{id:?} {tone:?} at heat {heat} did not move selection away from the canvas"
                    );
                    assert!(
                        (unselected.lightness - palette.backdrop.lightness).abs()
                            < resting_distance,
                        "{id:?} {tone:?} at heat {heat} did not sink neighbours toward the canvas"
                    );
                    assert!(
                        (selected.lightness - unselected.lightness).abs()
                            >= MIN_SELECTION_LIGHTNESS_GAP - f32::EPSILON,
                        "{id:?} {tone:?} at heat {heat} fell below the enforced selection gap"
                    );
                    assert!(
                        contrast_ratio(selected.to_rgb(), unselected.to_rgb())
                            >= MIN_SELECTION_CONTRAST,
                        "{id:?} {tone:?} at heat {heat} loses cursor separation after rendering"
                    );
                    assert_eq!(palette.emphasised(resting, Emphasis::Resting), resting);
                    for (emphasis, fill) in [
                        (Emphasis::Resting, resting),
                        (Emphasis::Selected, selected),
                        (Emphasis::Unselected, unselected),
                    ] {
                        assert_ink_contrast(id, tone, heat, emphasis, fill);
                    }
                }
            }
        }
    }

    #[test]
    fn selected_hot_folder_edge_clears_an_unselected_file_crown() {
        let palette = MapPalette::for_theme(Theme::for_id(ThemeId::ExciseDark))
            .expect("excise dark is truecolour");
        let selected = palette.emphasised(palette.tile(1.0, TileTone::Folder), Emphasis::Selected);
        let unselected =
            palette.emphasised(palette.tile(1.0, TileTone::File), Emphasis::Unselected);
        let selected_edge = selected.shifted(-TILE_SELECTED_EDGE_DROP, 1.0).to_rgb();
        let unselected_crown = unselected.shifted(TILE_CROWN_LIFT, 1.0).to_rgb();
        assert!(
            contrast_ratio(selected_edge, unselected_crown) >= MIN_SELECTION_CONTRAST,
            "a selected hot folder edge must clear its unselected file neighbour's crown"
        );
    }

    #[test]
    fn selection_clears_the_contrast_floor_against_every_unselected_tone_band() {
        for id in ThemeId::ALL {
            let Some(palette) = MapPalette::for_theme(Theme::for_id(id)) else {
                continue;
            };
            for selected_tone in [TileTone::Folder, TileTone::File] {
                for selected_step in 0_u8..=64 {
                    let selected_heat = f32::from(selected_step) / 64.0;
                    let selected = palette.emphasised(
                        palette.tile(selected_heat, selected_tone),
                        Emphasis::Selected,
                    );
                    for unselected_tone in [TileTone::Folder, TileTone::File] {
                        for unselected_step in 0_u8..=64 {
                            let unselected_heat = f32::from(unselected_step) / 64.0;
                            let unselected = palette.emphasised(
                                palette.tile(unselected_heat, unselected_tone),
                                Emphasis::Unselected,
                            );
                            assert!(
                                (selected.lightness - unselected.lightness).abs()
                                    >= MIN_SELECTION_LIGHTNESS_GAP - f32::EPSILON,
                                "{id:?}: selected {selected_tone:?} at {selected_heat} is too close \
                                 to unselected {unselected_tone:?} at {unselected_heat}"
                            );
                            assert!(
                                contrast_ratio(selected.to_rgb(), unselected.to_rgb())
                                    >= MIN_SELECTION_CONTRAST,
                                "{id:?}: selected {selected_tone:?} at {selected_heat} loses cursor \
                                 contrast against unselected {unselected_tone:?} at {unselected_heat}"
                            );
                            let (
                                selected_boundary,
                                unselected_boundary,
                                selected_boundary_name,
                                unselected_boundary_name,
                            ) = if selected.lightness >= palette.backdrop.lightness {
                                (
                                    selected.shifted(-TILE_SELECTED_EDGE_DROP, 1.0).to_rgb(),
                                    unselected.shifted(TILE_CROWN_LIFT, 1.0).to_rgb(),
                                    "edge",
                                    "crown",
                                )
                            } else {
                                (
                                    selected.shifted(TILE_CROWN_LIFT, 1.0).to_rgb(),
                                    unselected.shifted(-TILE_EDGE_DROP, 1.0).to_rgb(),
                                    "crown",
                                    "edge",
                                )
                            };
                            let ratio = contrast_ratio(selected_boundary, unselected_boundary);
                            assert!(
                                ratio >= MIN_SELECTION_CONTRAST,
                                "{id:?}: selected {selected_tone:?} {selected_boundary_name} at \
                                 {selected_heat} ({selected_boundary:?}) loses cursor contrast \
                                 against unselected {unselected_tone:?} {unselected_boundary_name} \
                                 at {unselected_heat} ({unselected_boundary:?}), ratio {ratio}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn dark_backdrops_remain_lifted_above_their_panels() {
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            let Some(palette) = MapPalette::for_theme(theme) else {
                continue;
            };
            let panel = Oklch::from_color(theme.surface_panel)
                .expect("a truecolour map palette has a truecolour panel");
            if panel.lightness <= 0.5 {
                assert!(
                    palette.backdrop.lightness > panel.lightness,
                    "{id:?}: dark map backdrop must stay lifted above its panel"
                );
            }
        }
    }

    #[test]
    fn ink_falls_back_to_the_stronger_neutral_when_tints_miss_the_floor() {
        let tile = Oklch {
            lightness: 0.57,
            chroma: 0.09,
            hue: 0.031_25,
        };
        let rendered_fill = tile.to_rgb();
        let (lead, _) = tile.inks();
        assert_eq!(channels(lead), (u8::MAX, u8::MAX, u8::MAX));
        assert!(contrast_ratio(rendered_fill, channels(lead)) >= LEAD_CONTRAST_FLOOR);
    }

    #[test]
    fn title_chip_inks_meet_the_name_contrast_floor() {
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            let cycle = ColorCycle::for_theme_with_monochrome(&theme, false);
            for step in 0..CYCLE_LEN {
                let (fill, ink) = cycle.chip_at_perimeter(0, step, CYCLE_LEN);
                match (fill, ink) {
                    (
                        Color::Rgb(fill_red, fill_green, fill_blue),
                        CycleInk::Foreground(Color::Rgb(ink_red, ink_green, ink_blue)),
                    ) => assert!(
                        contrast_ratio(
                            (fill_red, fill_green, fill_blue),
                            (ink_red, ink_green, ink_blue)
                        ) >= LEAD_CONTRAST_FLOOR,
                        "{id:?} title chip sample {step} has unreadable foreground"
                    ),
                    (Color::Rgb(..), CycleInk::Foreground(other)) => {
                        panic!("{id:?} title chip must use a truecolour foreground, got {other:?}");
                    }
                    (Color::Rgb(..), CycleInk::Reversed) => {
                        panic!("{id:?} truecolour title chip must use a measured foreground");
                    }
                    (_, CycleInk::Reversed) => {}
                    (_, CycleInk::Foreground(_)) => {
                        panic!("{id:?} palette title chip must retain terminal reverse video");
                    }
                }
            }
        }
    }

    #[test]
    fn forced_monochrome_reverses_rgb_chips_without_reusing_the_colour_cache() {
        let theme = Theme::for_id(ThemeId::CatppuccinMocha);
        let (colour_cycle, _) = derived_for_with_monochrome(theme, false);
        assert!(matches!(
            colour_cycle.chip_at_perimeter(0, 0, CYCLE_LEN).1,
            CycleInk::Foreground(Color::Rgb(..))
        ));

        let (monochrome_cycle, _) = derived_for_with_monochrome(theme, true);
        for step in 0..CYCLE_LEN {
            assert_eq!(
                monochrome_cycle.chip_at_perimeter(0, step, CYCLE_LEN).1,
                CycleInk::Reversed,
                "forced monochrome must explicitly reverse RGB chip sample {step}"
            );
        }

        let (colour_cycle_again, _) = derived_for_with_monochrome(theme, false);
        assert!(matches!(
            colour_cycle_again.chip_at_perimeter(0, 0, CYCLE_LEN).1,
            CycleInk::Foreground(Color::Rgb(..))
        ));
    }

    #[test]
    fn only_truecolour_focus_cycles_request_activity() {
        assert!(ColorCycle::can_animate(
            Theme::for_id(ThemeId::ExciseLight).focus
        ));
        assert!(!ColorCycle::can_animate(
            Theme::for_id(ThemeId::HighContrast).focus
        ));
        assert!(!ColorCycle::can_animate(
            Theme::for_id(ThemeId::Monochrome).focus
        ));
    }

    #[test]
    fn the_border_cycle_closes_on_itself() {
        let cycle = ColorCycle::from_accent_against(Color::Rgb(166, 227, 161), Color::Reset)
            .expect("truecolour accent");
        let first = Oklch::from_color(cycle.at(0)).expect("sample is truecolour");
        let last = Oklch::from_color(cycle.at(CYCLE_LEN - 1)).expect("sample is truecolour");
        assert!(
            (first.lightness - last.lightness).abs() < 0.05,
            "a seam would travel around the border once per revolution"
        );
        assert_eq!(cycle.at(0), cycle.at(CYCLE_LEN));
        assert!(
            (0..CYCLE_LEN).any(|step| cycle.at(step) != cycle.at(0)),
            "the cycle must actually vary"
        );
    }

    #[test]
    fn light_theme_cycle_does_not_jump_through_neutral_fallbacks() {
        let theme = Theme::for_id(ThemeId::CatppuccinLatte);
        let cycle = ColorCycle::for_theme_with_monochrome(&theme, false);

        for step in 0..CYCLE_LEN {
            let current = cycle.at(step);
            assert!(!matches!(
                current,
                Color::Rgb(0, 0, 0) | Color::Rgb(u8::MAX, u8::MAX, u8::MAX)
            ));
            let next = cycle.at(step + 1);
            let current_lightness = Oklch::from_color(current)
                .expect("truecolour cycle sample")
                .lightness;
            let next_lightness = Oklch::from_color(next)
                .expect("truecolour cycle sample")
                .lightness;
            assert!(
                (current_lightness - next_lightness).abs() < 0.25,
                "cycle seam at step {step} is too large"
            );
        }
    }

    #[test]
    fn the_focus_cycle_clears_the_absolute_contrast_floor() {
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            let Some(base_contrast) = color_contrast(theme.focus, theme.surface_panel) else {
                continue;
            };
            let required_contrast = base_contrast.max(MIN_FOCUS_CONTRAST);
            let cycle = ColorCycle::for_theme_with_monochrome(&theme, false);
            for step in 0..CYCLE_LEN {
                assert!(
                    color_contrast(cycle.at(step), theme.surface_panel)
                        .is_some_and(|contrast| contrast >= required_contrast),
                    "{id:?} focus sample {step} falls below the absolute contrast floor"
                );
            }
        }
    }

    #[test]
    fn the_cycle_keeps_its_cadence_after_long_uptime() {
        assert_eq!(cycle_step(Duration::ZERO), 0);
        assert_eq!(cycle_step(Duration::from_millis(100)), 3);
        assert_eq!(cycle_step(Duration::from_secs(1)), 30);
        let long_uptime = Duration::from_secs(1_u64 << 24);
        assert_eq!(
            cycle_step(long_uptime + Duration::from_millis(34)),
            (cycle_step(long_uptime) + 1) % CYCLE_LEN
        );
        assert!(cycle_step(Duration::MAX) < CYCLE_LEN);
    }

    #[test]
    fn non_truecolour_accents_have_no_cycle() {
        assert!(ColorCycle::from_accent_against(Color::Reset, Color::Reset).is_none());
        assert!(ColorCycle::from_accent_against(Color::Cyan, Color::Reset).is_none());
    }
}
