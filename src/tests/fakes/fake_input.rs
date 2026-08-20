use std::time::Duration;

use crossterm::event::Event;

use crate::error::AppError;
use crate::input::{InputEvent, InputSource};

pub struct TerminalEvents {
    events: Vec<Option<Event>>,
}

impl TerminalEvents {
    pub fn new(mut events: Vec<Option<Event>>) -> Self {
        events.reverse();
        Self { events }
    }
}

impl InputSource for TerminalEvents {
    fn poll(&mut self, _timeout: Duration) -> Result<bool, AppError> {
        Ok(!self.events.is_empty())
    }

    fn read(&mut self) -> Result<InputEvent, AppError> {
        self.events
            .pop()
            .map(|event| match event {
                Some(event) => InputEvent::Terminal(event),
                None => InputEvent::Barrier,
            })
            .ok_or_else(|| AppError::Invariant("fake input exhausted after poll".to_string()))
    }
}
