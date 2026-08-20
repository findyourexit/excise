#![no_main]

use excise::{TerminalState, TerminalTransition};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut state = TerminalState::Inactive;
    for byte in data {
        let transition = match byte % 3 {
            0 => TerminalTransition::EnterRaw,
            1 => TerminalTransition::EnterAlternate,
            _ => TerminalTransition::Restore,
        };
        if let Ok(next) = state.transition(transition) {
            state = next;
        }
        if state == TerminalState::Restored {
            assert_eq!(
                state
                    .transition(TerminalTransition::Restore)
                    .expect("restore must be idempotent"),
                TerminalState::Restored
            );
        }
    }
});
