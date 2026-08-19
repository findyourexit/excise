use crossterm::event::Event;

use crate::input::InputEvent;

pub struct TerminalEvents {
    events: Vec<Option<Event>>,
}

impl TerminalEvents {
    pub fn new(mut events: Vec<Option<Event>>) -> Self {
        events.reverse();
        Self { events }
    }
}

impl Iterator for TerminalEvents {
    type Item = InputEvent;

    fn next(&mut self) -> Option<InputEvent> {
        self.events.pop().map(|event| match event {
            Some(event) => InputEvent::Terminal(event),
            None => InputEvent::Barrier,
        })
    }
}
