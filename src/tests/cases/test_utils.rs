use ::std::sync::{Arc, Mutex};
use crossterm::event::KeyModifiers;
use crossterm::event::{Event, KeyCode, KeyEvent};

use crate::tests::fakes::{TerminalEvent, TerminalEvents, TestBackend};

macro_rules! key {
    (char $x:expr) => {
        Event::Key(KeyEvent {
            code: KeyCode::Char($x),
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    };
    (ctrl $x:expr) => {
        Event::Key(KeyEvent {
            code: KeyCode::Char($x),
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    };
    ($x:ident) => {
        Event::Key(KeyEvent {
            code: KeyCode::$x,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    };
}

pub fn wait_and_quit_events(barrier_count: usize, quit_after_confirm: bool) -> TerminalEvents {
    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, barrier_count).collect();
    events.push(Some(key!(ctrl 'c')));
    if quit_after_confirm {
        events.push(None);
        events.push(Some(key!(char 'y')));
    }
    TerminalEvents::new(events)
}

type BackendWithStreams = (
    Arc<Mutex<Vec<TerminalEvent>>>,
    Arc<Mutex<Vec<String>>>,
    TestBackend,
);
pub fn test_backend_factory(w: u16, h: u16) -> BackendWithStreams {
    let terminal_events: Arc<Mutex<Vec<TerminalEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let terminal_draw_events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let backend = TestBackend::new(
        terminal_events.clone(),
        terminal_draw_events.clone(),
        Arc::new(Mutex::new(w)),
        Arc::new(Mutex::new(h)),
    );
    (terminal_events, terminal_draw_events, backend)
}

pub fn assert_terminal_lifecycle(events: &[TerminalEvent]) {
    use TerminalEvent::{Clear, Draw, Flush, HideCursor, ShowCursor};

    assert!(
        events.len() >= 7,
        "terminal lifecycle was incomplete: {events:?}"
    );
    assert_eq!(&events[..2], &[Clear, HideCursor]);
    assert_eq!(&events[events.len() - 2..], &[Clear, ShowCursor]);
    let frames = &events[2..events.len() - 2];
    assert_eq!(frames.len() % 3, 0, "incomplete draw frame: {frames:?}");
    assert!(
        frames
            .chunks_exact(3)
            .all(|frame| frame == [Draw, HideCursor, Flush]),
        "unexpected terminal frame sequence: {frames:?}"
    );
}
