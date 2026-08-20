use std::collections::BTreeMap;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use tachyonfx::{Effect, EffectManager, fx};

pub const ACTIVE_FRAME_INTERVAL: Duration = Duration::from_millis(33);
pub const ROUTINE_MOTION: Duration = Duration::from_millis(160);
pub const EXCEPTIONAL_MOTION: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum EffectKey {
    #[default]
    Navigation,
    StateChange,
}

#[derive(Clone, Copy, Debug)]
enum EffectKind {
    Navigation,
    StateChange,
    Cancel,
}

impl EffectKind {
    fn build(self) -> Effect {
        match self {
            Self::Navigation => fx::fade_from_fg(Color::DarkGray, millis(ROUTINE_MOTION)),
            Self::StateChange => fx::fade_from_fg(Color::Black, millis(EXCEPTIONAL_MOTION)),
            Self::Cancel => fx::consume_tick(),
        }
    }
}

#[derive(Debug)]
pub struct AnimationScheduler {
    manager: EffectManager<EffectKey>,
    pending: BTreeMap<EffectKey, EffectKind>,
    last_tick: Duration,
    enabled: bool,
}

impl AnimationScheduler {
    #[must_use]
    pub fn new(reduced_motion: bool, monochrome: bool, now: Duration) -> Self {
        Self {
            manager: EffectManager::default(),
            pending: BTreeMap::new(),
            last_tick: now,
            enabled: !reduced_motion && !monochrome,
        }
    }

    pub fn schedule_navigation(&mut self) {
        if self.enabled {
            self.pending
                .insert(EffectKey::Navigation, EffectKind::Navigation);
        }
    }

    pub fn schedule_state_change(&mut self) {
        if self.enabled {
            self.pending
                .insert(EffectKey::StateChange, EffectKind::StateChange);
        }
    }

    pub fn cancel_all(&mut self) {
        self.pending
            .insert(EffectKey::Navigation, EffectKind::Cancel);
        self.pending
            .insert(EffectKey::StateChange, EffectKind::Cancel);
    }

    pub fn process(&mut self, now: Duration, buffer: &mut Buffer, area: Rect) {
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
            .then(|| self.last_tick.saturating_add(ACTIVE_FRAME_INTERVAL))
    }

    #[must_use]
    pub fn pending_slots(&self) -> usize {
        self.pending.len()
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
