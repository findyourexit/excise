use std::collections::BTreeMap;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use tachyonfx::{Effect, EffectManager, fx};

pub const ACTIVE_FRAME_INTERVAL: Duration = Duration::from_millis(33);
const MEDIUM_FRAME_INTERVAL: Duration = Duration::from_millis(50);
const LARGE_FRAME_INTERVAL: Duration = Duration::from_millis(66);
const SMALL_SURFACE_CELLS: u32 = 4_000;
const MEDIUM_SURFACE_CELLS: u32 = 12_000;
pub const ROUTINE_MOTION: Duration = Duration::from_millis(160);
pub const EXCEPTIONAL_MOTION: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum EffectKey {
    #[default]
    Navigation,
    Focus,
    StateChange,
    ScanProgress,
    Aggregation,
    Completion,
    Error,
    DeletionResult,
}

#[derive(Clone, Copy, Debug)]
enum EffectKind {
    Navigation,
    Focus,
    StateChange,
    ScanProgress,
    Aggregation,
    Completion,
    Error,
    DeletionResult,
    Cancel,
}

impl EffectKind {
    fn build(self) -> Effect {
        match self {
            Self::Navigation => fx::fade_from_fg(Color::DarkGray, millis(ROUTINE_MOTION)),
            Self::Focus => fx::fade_from_fg(Color::Gray, 120),
            Self::StateChange => fx::fade_from_fg(Color::Black, millis(EXCEPTIONAL_MOTION)),
            Self::ScanProgress => fx::fade_from_fg(Color::DarkGray, 100),
            Self::Aggregation => fx::fade_from_fg(Color::Yellow, 220),
            Self::Completion | Self::DeletionResult => fx::fade_from_fg(Color::Green, 240),
            Self::Error => fx::fade_from_fg(Color::Red, 200),
            Self::Cancel => fx::consume_tick(),
        }
    }
}

#[derive(Debug)]
pub struct AnimationScheduler {
    manager: EffectManager<EffectKey>,
    pending: BTreeMap<EffectKey, EffectKind>,
    last_tick: Duration,
    frame_interval: Duration,
    enabled: bool,
}

impl AnimationScheduler {
    #[must_use]
    pub fn new(reduced_motion: bool, monochrome: bool, now: Duration) -> Self {
        Self {
            manager: EffectManager::default(),
            pending: BTreeMap::new(),
            last_tick: now,
            frame_interval: ACTIVE_FRAME_INTERVAL,
            enabled: !reduced_motion && !monochrome,
        }
    }

    pub fn set_accessibility(&mut self, reduced_motion: bool, monochrome: bool) {
        self.enabled = !reduced_motion && !monochrome;
        if !self.enabled {
            self.cancel_all();
        }
    }

    pub fn schedule_navigation(&mut self) {
        self.schedule(EffectKey::Navigation, EffectKind::Navigation);
    }

    pub fn schedule_focus(&mut self) {
        self.schedule(EffectKey::Focus, EffectKind::Focus);
    }

    pub fn schedule_state_change(&mut self) {
        self.schedule(EffectKey::StateChange, EffectKind::StateChange);
    }

    pub fn schedule_scan_progress(&mut self) {
        self.schedule(EffectKey::ScanProgress, EffectKind::ScanProgress);
    }

    pub fn schedule_aggregation(&mut self) {
        self.schedule(EffectKey::Aggregation, EffectKind::Aggregation);
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
            EffectKey::Navigation,
            EffectKey::Focus,
            EffectKey::StateChange,
            EffectKey::ScanProgress,
            EffectKey::Aggregation,
            EffectKey::Completion,
            EffectKey::Error,
            EffectKey::DeletionResult,
        ] {
            self.pending.insert(key, EffectKind::Cancel);
        }
    }

    pub fn process(&mut self, now: Duration, buffer: &mut Buffer, area: Rect) {
        self.frame_interval = match u32::from(area.width) * u32::from(area.height) {
            0..=SMALL_SURFACE_CELLS => ACTIVE_FRAME_INTERVAL,
            cells if cells <= MEDIUM_SURFACE_CELLS => MEDIUM_FRAME_INTERVAL,
            _ => LARGE_FRAME_INTERVAL,
        };
        for (key, effect) in std::mem::take(&mut self.pending) {
            self.manager.add_unique_effect(key, effect.build());
        }

        let elapsed = now.saturating_sub(self.last_tick);
        let elapsed_ms = u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX);
        self.manager
            .process_effects(tachyonfx::Duration::from_millis(elapsed_ms), buffer, area);
        self.last_tick = now;
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.pending.is_empty() || self.manager.is_running()
    }

    #[must_use]
    pub fn next_frame_at(&self) -> Option<Duration> {
        self.is_running()
            .then(|| self.last_tick.saturating_add(self.frame_interval))
    }

    #[must_use]
    pub fn pending_slots(&self) -> usize {
        self.pending.len()
    }

    fn schedule(&mut self, key: EffectKey, effect: EffectKind) {
        if self.enabled {
            self.pending.insert(key, effect);
        }
    }
}

const fn millis(duration: Duration) -> u32 {
    let millis = duration.as_millis();
    if millis > u32::MAX as u128 {
        u32::MAX
    } else {
        millis as u32
    }
}
