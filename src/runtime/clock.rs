use std::cell::Cell;
use std::time::{Duration, Instant};

pub trait Clock {
    fn now(&self) -> Duration;

    fn advance_to(&self, _deadline: Duration) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct SystemClock {
    started: Instant,
}

impl SystemClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.started.elapsed()
    }
}

#[derive(Debug, Default)]
pub struct VirtualClock {
    now: Cell<Duration>,
}

impl VirtualClock {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            now: Cell::new(Duration::ZERO),
        }
    }

    pub fn advance(&self, elapsed: Duration) {
        self.now.set(self.now.get().saturating_add(elapsed));
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> Duration {
        self.now.get()
    }

    fn advance_to(&self, deadline: Duration) -> bool {
        if deadline > self.now.get() {
            self.now.set(deadline);
        }
        true
    }
}
