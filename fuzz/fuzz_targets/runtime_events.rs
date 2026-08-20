#![no_main]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use excise::error::AppError;
use excise::input::{InputEvent, InputSource};
use excise::runtime::{RuntimeSettings, VirtualClock, run};
use libfuzzer_sys::fuzz_target;
use ratatui::backend::TestBackend;

struct ScriptedInput {
    events: VecDeque<InputEvent>,
}

impl InputSource for ScriptedInput {
    fn poll(&mut self, _timeout: Duration) -> Result<bool, AppError> {
        Ok(!self.events.is_empty())
    }

    fn read(&mut self) -> Result<InputEvent, AppError> {
        self.events
            .pop_front()
            .ok_or_else(|| AppError::Invariant("fuzz input exhausted after poll".to_string()))
    }
}

fuzz_target!(|data: &[u8]| {
    let root = fixture_root();
    let mut events = VecDeque::from([InputEvent::Barrier]);
    for byte in data.iter().take(32) {
        let event = match byte % 10 {
            0 => Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            1 => Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            2 => Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            3 => Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            4 => Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            5 => Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            6 => Event::Key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE)),
            7 => Event::Key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE)),
            8 => Event::Resize(u16::from(*byte) + 1, 24),
            _ => Event::Key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)),
        };
        events.push_back(InputEvent::Terminal(event));
        events.push_back(InputEvent::Barrier);
    }
    events.push_back(InputEvent::Terminal(Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
    ))));
    events.push_back(InputEvent::Barrier);
    events.push_back(InputEvent::Terminal(Event::Key(KeyEvent::new(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
    ))));

    let settings = RuntimeSettings {
        root: root.clone(),
        scan_threads: 1,
        event_capacity: 16,
        cross_filesystems: false,
        exclusions: Vec::new(),
        memory_mib: excise::model::DEFAULT_PROCESS_MIB,
        apparent_size: true,
        disable_delete_confirmation: false,
        reduced_motion: true,
        theme: excise::theme::ThemeId::ExciseDark,
        ascii: false,
        mouse: false,
        keymap: excise::config::KeyPreset::Vim,
        custom_keys: None,
        monochrome: true,
        animate_loading: false,
        config_path: None,
        monochrome_locked: true,
    };
    run(
        TestBackend::new(80, 24),
        Box::new(ScriptedInput { events }),
        settings,
        Box::new(VirtualClock::new()),
    )
    .expect("bounded owner-loop sequence must not fail");
});

fn fixture_root() -> &'static PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("excise-fuzz-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("fuzz root should be created");
        root
    })
}
