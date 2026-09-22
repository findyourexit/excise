use std::collections::BTreeMap;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use tachyonfx::{Effect, Interpolation, SimpleRng, fx, pattern::CheckerboardPattern};

pub const ACTIVE_FRAME_INTERVAL: Duration = Duration::from_millis(33);
const MEDIUM_FRAME_INTERVAL: Duration = Duration::from_millis(50);
const LARGE_FRAME_INTERVAL: Duration = Duration::from_millis(66);
/// Persistent focus and modal chrome redraw at this cadence; short effects and
/// geometry keep the higher cadence needed to look continuous.
const PERSISTENT_ACTIVITY_FRAME_INTERVAL: Duration = Duration::from_millis(125);
const SMALL_SURFACE_CELLS: u32 = 4_000;
const MEDIUM_SURFACE_CELLS: u32 = 12_000;
/// How long the map takes to settle after a layout it can interpolate: a resize,
/// or a streaming scan batch that moves entries around.
pub const ROUTINE_MOTION: Duration = Duration::from_millis(160);
/// How long a drill in or out takes. Longer than a resize because the whole map
/// is replaced: the eye needs the extra frames to follow the entry it chose into
/// its contents, or back out of them.
pub const NAVIGATION_MOTION: Duration = Duration::from_millis(260);
/// How long the measured map surface takes to emerge from the scan field.
pub const SCAN_REVEAL_MOTION: Duration = Duration::from_millis(280);
/// Stable seed keeps the target's checkerboard departure coherent between redraws.
const DELETION_DISSOLVE_SEED: u32 = 0x0D3E_1E7E;

/// Applies the deterministic departure dissolve to a freshly painted map layer.
pub(crate) fn dissolve_deletion_departure(
    now: Duration,
    started_at: Duration,
    duration: Duration,
    buffer: &mut Buffer,
    area: Rect,
    destination: Style,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let duration = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
    let elapsed = now.saturating_sub(started_at);
    let elapsed =
        tachyonfx::Duration::from_millis(u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX));
    let mut effect = fx::dissolve_to(destination, (duration, Interpolation::SineOut))
        .with_area(area)
        .with_pattern(CheckerboardPattern::new(1, 0.5))
        .with_rng(SimpleRng::new(DELETION_DISSOLVE_SEED));
    effect.process(elapsed, buffer, area);
}

/// Effects left in the scheduler are one-shot acknowledgements of an event the
/// interface cannot otherwise show, and they are painted over the header band
/// alone. Navigation deliberately has no full-screen effect: map geometry and
/// the selected tile's own travelling edge supply movement without obscuring
/// the surface being read.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum EffectKey {
    #[default]
    Completion,
    Error,
    DeletionResult,
}

#[derive(Clone, Copy, Debug)]
enum EffectKind {
    Completion,
    Error,
    DeletionResult,
    Cancel,
}

impl EffectKind {
    fn build(self) -> Option<Effect> {
        match self {
            Self::Completion | Self::DeletionResult => Some(fx::fade_from_fg(Color::Green, 240)),
            Self::Error => Some(fx::fade_from_fg(Color::Red, 200)),
            Self::Cancel => None,
        }
    }
}

#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "activity and geometry gates are independent scheduler state"
)]
pub struct AnimationScheduler {
    effects: Vec<(EffectKey, Effect)>,
    pending: BTreeMap<EffectKey, EffectKind>,
    last_tick: Duration,
    frame_interval: Duration,
    enabled: bool,
    activity_requested: bool,
    activity: bool,
    /// Whether requested activity is a short-lived animation rather than chrome.
    activity_fast: bool,
    activity_suspended: bool,
    geometry: bool,
}

impl AnimationScheduler {
    #[must_use]
    pub fn new(reduced_motion: bool, monochrome: bool, now: Duration) -> Self {
        Self {
            effects: Vec::new(),
            pending: BTreeMap::new(),
            last_tick: now,
            frame_interval: ACTIVE_FRAME_INTERVAL,
            enabled: !reduced_motion && !monochrome,
            activity_requested: false,
            activity: false,
            activity_fast: false,
            activity_suspended: false,
            geometry: false,
        }
    }

    #[must_use]
    pub(crate) const fn animations_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_activity(&mut self, active: bool) {
        self.set_activity_with_cadence(active, true);
    }

    /// Requests redraws for persistent chrome or a short-lived visual effect.
    pub(crate) fn set_activity_with_cadence(&mut self, active: bool, fast: bool) {
        self.activity_requested = active;
        self.activity_fast = active && fast;
        self.activity = active && self.enabled && !self.activity_suspended;
    }

    /// Temporarily prevents persistent activity from keeping the render loop awake.
    ///
    /// Finite acknowledgement effects and map geometry continue to drain while
    /// activity is suspended. Resuming restores the latest requested activity
    /// state, so the barrier does not require an extra dirty render.
    pub(crate) fn set_activity_suspended(&mut self, suspended: bool) {
        self.activity_suspended = suspended;
        self.activity = !suspended && self.enabled && self.activity_requested;
    }

    /// Marks whether the map is mid-tween.
    ///
    /// Layout transitions are motion the colour pipeline knows nothing about, so
    /// this is deliberately independent of the monochrome-driven `enabled` flag:
    /// a two-colour terminal still moves its entries. Reduced motion settles the
    /// tween in the board itself, which clears this on the next frame.
    pub const fn set_geometry_active(&mut self, active: bool) {
        self.geometry = active;
    }

    pub fn set_accessibility(&mut self, reduced_motion: bool, monochrome: bool) {
        self.enabled = !reduced_motion && !monochrome;
        if reduced_motion {
            self.geometry = false;
        }
        if !self.enabled {
            self.activity_requested = false;
            self.activity = false;
            self.activity_fast = false;
            self.cancel_all();
        }
    }

    pub fn schedule_completion(&mut self) {
        self.schedule(EffectKey::Completion, EffectKind::Completion);
    }

    pub fn schedule_error(&mut self) {
        self.schedule(EffectKey::Error, EffectKind::Error);
    }

    pub fn schedule_deletion_result(&mut self) {
        self.schedule(EffectKey::DeletionResult, EffectKind::DeletionResult);
    }

    pub fn cancel_all(&mut self) {
        for key in [
            EffectKey::Completion,
            EffectKey::Error,
            EffectKey::DeletionResult,
        ] {
            self.pending.insert(key, EffectKind::Cancel);
        }
    }

    /// Processes the acknowledgement effects for one rendered frame.
    ///
    /// Effects are painted over the header band alone, but the cost of a frame
    /// scales with the whole terminal, so `area` is the paint target and `surface`
    /// selects the baseline cadence.
    pub fn process(&mut self, now: Duration, buffer: &mut Buffer, area: Rect, surface: Rect) {
        self.frame_interval = match u32::from(surface.width) * u32::from(surface.height) {
            0..=SMALL_SURFACE_CELLS => ACTIVE_FRAME_INTERVAL,
            cells if cells <= MEDIUM_SURFACE_CELLS => MEDIUM_FRAME_INTERVAL,
            _ => LARGE_FRAME_INTERVAL,
        };
        let effects_were_running = !self.effects.is_empty();
        let has_pending = !self.pending.is_empty();
        // A session that sat idle for minutes must not charge that wait to an
        // effect scheduled on this frame: the whole effect would elapse before it
        // is ever drawn. Effects already in flight keep their real elapsed time.
        if has_pending && !effects_were_running {
            self.last_tick = now;
        }

        let elapsed = now.saturating_sub(self.last_tick);
        let elapsed_ms = u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX);
        let elapsed = tachyonfx::Duration::from_millis(elapsed_ms);
        let pending = std::mem::take(&mut self.pending);

        // A pending key either replaces or cancels its in-flight acknowledgement.
        // Remove it before advancing the rest so a restart starts at age zero and
        // a cancellation remains non-painting.
        self.effects.retain(|(key, _)| !pending.contains_key(key));

        for (_, effect) in &mut self.effects {
            effect.process(elapsed, buffer, area);
        }
        self.effects.retain(|(_, effect)| effect.running());

        // Start each new acknowledgement on this frame without running unrelated
        // effects a second time. Keyed slots make replacement explicit, so only a
        // fresh effect receives the zero-age paint.
        for (key, effect) in pending {
            if let Some(mut effect) = effect.build() {
                effect.process(tachyonfx::Duration::ZERO, buffer, area);
                if effect.running() {
                    self.effects.push((key, effect));
                }
            }
        }
        self.last_tick = now;
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.activity || self.geometry || !self.pending.is_empty() || !self.effects.is_empty()
    }

    /// Frame spacing the next redraw should honour.
    ///
    /// Large surfaces relax the effect cadence because every frame reprocesses the
    /// whole buffer, but a map tween is the one motion whose smoothness the eye
    /// tracks across the entire screen, so it keeps the fast cadence.
    #[must_use]
    fn frame_interval(&self) -> Duration {
        let persistent_only = self.activity
            && !self.activity_fast
            && self.effects.is_empty()
            && self.pending.is_empty();
        let interval = if persistent_only {
            self.frame_interval.max(PERSISTENT_ACTIVITY_FRAME_INTERVAL)
        } else {
            self.frame_interval
        };
        if self.geometry {
            interval.min(ACTIVE_FRAME_INTERVAL)
        } else {
            interval
        }
    }

    #[must_use]
    pub fn next_frame_at(&self) -> Option<Duration> {
        self.is_running()
            .then(|| self.last_tick.saturating_add(self.frame_interval()))
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn pending_slots(&self) -> usize {
        self.pending.len()
    }
    fn schedule(&mut self, key: EffectKey, effect: EffectKind) {
        if self.enabled {
            self.pending.insert(key, effect);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn painted_buffer(area: Rect) -> Buffer {
        let mut buffer = Buffer::empty(area);
        let style = Style::default().fg(Color::Red).bg(Color::Blue);
        for position in area.positions() {
            buffer[position].set_symbol("x").set_style(style);
        }
        buffer
    }

    #[test]
    fn departure_checkerboard_dissolves_to_the_canvas_by_its_deadline() {
        let area = Rect::new(0, 0, 12, 6);
        let canvas = Style::default().fg(Color::Green).bg(Color::Green);

        let mut middle = painted_buffer(area);
        dissolve_deletion_departure(
            Duration::from_millis(45),
            Duration::ZERO,
            crate::state::DELETION_DEPARTURE_MIN_DURATION,
            &mut middle,
            area,
            canvas,
        );
        let cleared = middle
            .content
            .iter()
            .filter(|cell| cell.symbol() == " ")
            .count();
        assert!(cleared > 0);
        assert!(cleared < middle.content.len());

        let mut finished = painted_buffer(area);
        dissolve_deletion_departure(
            crate::state::DELETION_DEPARTURE_MIN_DURATION,
            Duration::ZERO,
            crate::state::DELETION_DEPARTURE_MIN_DURATION,
            &mut finished,
            area,
            canvas,
        );
        assert!(finished.content.iter().all(|cell| {
            cell.symbol() == " " && cell.fg == Color::Green && cell.bg == Color::Green
        }));
    }

    #[test]
    fn frame_cadence_matrix_scales_by_surface_and_activity() {
        enum Mode {
            Effect,
            Persistent,
            Geometry,
        }

        let cases = [
            (Rect::new(0, 0, 80, 24), Mode::Effect, ACTIVE_FRAME_INTERVAL),
            (
                Rect::new(0, 0, 160, 50),
                Mode::Effect,
                MEDIUM_FRAME_INTERVAL,
            ),
            (Rect::new(0, 0, 200, 80), Mode::Effect, LARGE_FRAME_INTERVAL),
            (
                Rect::new(0, 0, 200, 80),
                Mode::Persistent,
                PERSISTENT_ACTIVITY_FRAME_INTERVAL,
            ),
            (
                Rect::new(0, 0, 200, 80),
                Mode::Geometry,
                ACTIVE_FRAME_INTERVAL,
            ),
        ];

        for (surface, mode, expected) in cases {
            let header = Rect::new(0, 0, surface.width, 3);
            let mut buffer = Buffer::empty(surface);
            let mut scheduler = AnimationScheduler::new(false, false, Duration::ZERO);
            match mode {
                Mode::Effect => scheduler.schedule_completion(),
                Mode::Persistent => scheduler.set_activity_with_cadence(true, false),
                Mode::Geometry => scheduler.set_geometry_active(true),
            }
            scheduler.process(Duration::ZERO, &mut buffer, header, surface);
            assert_eq!(scheduler.next_frame_at(), Some(expected));
        }
    }
}
