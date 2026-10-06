//! The keys a soak sends, and the one place that decides which.
//!
//! The soak runs `excise` on a real tree, so it must never ask `excise` to delete anything. Only
//! Backspace asks: `deletion_request` in `src/input/controls.rs` is reached from Backspace in the
//! loading, normal, and rebuilding modes, in every key preset, and configuration cannot rebind it
//! (custom keys are movement only). Every deletion dialog opens on an input that follows a
//! deletion request, so a session that never sends Backspace can never delete.
//!
//! That is a fact about the program, and the soak does not rest on it alone. Every byte the soak
//! writes to the program is checked, before it is written, against an **allowlist** of exactly the
//! keys it needs, and anything else is refused:
//!
//! | Key | Bytes | What the soak uses it for |
//! |---|---|---|
//! | `up`, `down`, `right`, `left` | `ESC [ A`, `B`, `C`, `D` | moving the cursor |
//! | `h`, `j`, `k`, `l` | the letter | the Vim preset's movement, the preset the soak runs under |
//! | `enter` | `0x0d` | opening the folder under the cursor |
//! | `esc` | `0x1b` | the probe key during the scan, and going back up |
//! | `q` | `q` | asking to quit |
//! | `y` | `y` | answering the quit prompt, and nothing else |
//!
//! Backspace (`0x7f`) and Ctrl+H (`0x08`, which the Windows console path may read as Backspace)
//! are named in the refusals because they are the bytes that matter, but they are refused for the
//! same reason as everything outside the table: they are not in it. The allowlist is matched
//! against a *whole write*: a write of `h` and a Backspace together is not an allowed key.
//!
//! The allowlist never composes an escape sequence either. The program's input parser joins the
//! bytes of an escape sequence across writes (`ESC [ 121 u` is `y`, whatever writes the bytes
//! come in), so a key that *begins* a sequence and text that continues it could spell a key that
//! no write holds. The table has no key that begins one and leaves it open: an arrow is the whole
//! of `ESC [ A`, `esc` is a lone `ESC` followed by a key that is not `[` or `O` (neither is in the
//! table), and `alt+[` and `alt+O` are not in it. A test runs every sequence of up to three
//! allowed keys through [`InputScan`](crate::pty::input::InputScan), the reader the harness's own
//! guards use, and checks that none of them can be read as a request for a deletion. The soak's
//! driver asks the same scan about every write before it makes it, as a second line behind the
//! table.

use std::fmt;

use thiserror::Error;

/// A key the soak may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    name: &'static str,
    bytes: &'static [u8],
}

impl Key {
    /// The up arrow.
    pub const UP: Self = Self::new("up", b"\x1b[A");
    /// The down arrow.
    pub const DOWN: Self = Self::new("down", b"\x1b[B");
    /// The right arrow.
    pub const RIGHT: Self = Self::new("right", b"\x1b[C");
    /// The left arrow.
    pub const LEFT: Self = Self::new("left", b"\x1b[D");
    /// `h`: left, in the Vim preset.
    pub const H: Self = Self::new("h", b"h");
    /// `j`: down, in the Vim preset.
    pub const J: Self = Self::new("j", b"j");
    /// `k`: up, in the Vim preset.
    pub const K: Self = Self::new("k", b"k");
    /// `l`: right, in the Vim preset.
    pub const L: Self = Self::new("l", b"l");
    /// Enter: opens the folder under the cursor.
    pub const ENTER: Self = Self::new("enter", b"\r");
    /// Escape: goes up a folder; at the root it flashes the path and moves nothing.
    pub const ESC: Self = Self::new("esc", b"\x1b");
    /// `q`: asks to quit.
    pub const Q: Self = Self::new("q", b"q");
    /// `y`: answers the quit prompt.
    pub const Y: Self = Self::new("y", b"y");

    /// Every key the soak may send: the allowlist.
    pub const ALLOWED: [Self; 12] = [
        Self::UP,
        Self::DOWN,
        Self::RIGHT,
        Self::LEFT,
        Self::H,
        Self::J,
        Self::K,
        Self::L,
        Self::ENTER,
        Self::ESC,
        Self::Q,
        Self::Y,
    ];

    const fn new(name: &'static str, bytes: &'static [u8]) -> Self {
        Self { name, bytes }
    }

    /// The key's name, as it is written in the table above.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// The bytes a terminal writes for the key.
    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        self.bytes
    }

    /// The allowed key whose bytes are exactly `bytes`: the decision that every write passes.
    ///
    /// # Errors
    ///
    /// Returns the [`Refusal`] for a write that is not exactly one allowed key. Backspace
    /// (`0x7f`), Ctrl+H (`0x08`), and the input barrier request (`0x1d`) anywhere in the write are
    /// named, because they are the bytes that matter; every other write is refused for not being
    /// in the table.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Refusal> {
        if let Some(key) = Self::ALLOWED.iter().find(|key| key.bytes == bytes) {
            return Ok(*key);
        }
        Err(if bytes.contains(&0x7f) {
            Refusal::Backspace
        } else if bytes.contains(&0x08) {
            Refusal::ControlH
        } else if bytes.contains(&0x1d) {
            Refusal::Barrier
        } else {
            Refusal::NotAllowed {
                write: describe(bytes),
            }
        })
    }
}

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name)
    }
}

/// Why a write was refused. Nothing was written.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Refusal {
    /// Backspace (`0x7f`): the one key that asks for a deletion.
    #[error("Backspace (0x7f) asks excise to delete, and a soak never asks")]
    Backspace,
    /// Ctrl+H (`0x08`), which a console may read as Backspace.
    #[error("Ctrl+H (0x08) can be read as Backspace, and a soak never asks excise to delete")]
    ControlH,
    /// The input barrier request, which only the harness's own protocols write.
    #[error("0x1d is the input barrier request, which a soak does not use")]
    Barrier,
    /// Anything else that is not one allowed key.
    #[error("{write} is not one of the keys a soak sends ({})", allowed())]
    NotAllowed {
        /// The write, as text.
        write: String,
    },
    /// The program could read the write as the continuation of an escape sequence that an earlier
    /// write began, which is how a key can be spelled with no byte that names it.
    #[error("the write would continue an escape sequence that an earlier write began")]
    Composes,
    /// A deletion dialog is on the screen, so nothing that could confirm one is sent.
    #[error("a deletion dialog is on the screen, so no key that could confirm it is sent")]
    DeletionDialog,
}

/// The names of the allowed keys, for a message.
fn allowed() -> String {
    Key::ALLOWED
        .iter()
        .map(|key| key.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `bytes` for a message: printable ASCII as it is, every other byte in hexadecimal.
fn describe(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "an empty write".to_owned();
    }
    let hex: Vec<String> = bytes.iter().map(|byte| format!("0x{byte:02x}")).collect();
    format!("the bytes {}", hex.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pty::{
            input::InputScan,
            keys::{BARRIER, encode_key},
        },
        scenario::KeyName,
    };

    #[test]
    fn the_allowlist_is_exactly_the_arrows_the_vim_letters_enter_escape_q_and_y() {
        let names: Vec<&str> = Key::ALLOWED.iter().map(|key| key.name()).collect();

        assert_eq!(
            names,
            [
                "up", "down", "right", "left", "h", "j", "k", "l", "enter", "esc", "q", "y"
            ]
        );
        for key in Key::ALLOWED {
            assert_eq!(Key::from_bytes(key.bytes()), Ok(key), "{key}");
        }
    }

    #[test]
    fn backspace_and_ctrl_h_are_refused_by_name() {
        assert_eq!(Key::from_bytes(&[0x7f]), Err(Refusal::Backspace));
        assert_eq!(Key::from_bytes(&[0x08]), Err(Refusal::ControlH));
        assert_eq!(Key::from_bytes(&[BARRIER]), Err(Refusal::Barrier));
        // Anywhere in a write, alone or with an allowed key.
        assert_eq!(Key::from_bytes(b"\x1b\x7f"), Err(Refusal::Backspace));
        assert_eq!(Key::from_bytes(b"h\x7f"), Err(Refusal::Backspace));
        assert_eq!(Key::from_bytes(b"\x7fy"), Err(Refusal::Backspace));
        assert_eq!(Key::from_bytes(b"q\x08"), Err(Refusal::ControlH));
        for refusal in [Refusal::Backspace, Refusal::ControlH, Refusal::Barrier] {
            assert!(!refusal.to_string().is_empty());
        }
    }

    #[test]
    fn the_keys_the_scenario_vocabulary_writes_for_backspace_and_ctrl_h_are_refused() {
        let backspace = encode_key(KeyName::Backspace, false, false).expect("backspace");
        let alt_backspace = encode_key(KeyName::Backspace, false, true).expect("alt+backspace");
        let ctrl_h = encode_key(KeyName::Char('h'), true, false).expect("ctrl+h");
        let ctrl_question = encode_key(KeyName::Char('?'), true, false).expect("ctrl+?");

        assert_eq!(backspace, [0x7f]);
        assert_eq!(ctrl_h, [0x08]);
        for bytes in [backspace, alt_backspace, ctrl_h, ctrl_question] {
            assert!(Key::from_bytes(&bytes).is_err(), "{bytes:02x?}");
        }
    }

    #[test]
    fn every_other_key_the_vocabulary_can_write_is_refused_unless_it_is_in_the_table() {
        let mut accepted = Vec::new();
        for name in all_key_names() {
            for (ctrl, alt) in [(false, false), (true, false), (false, true), (true, true)] {
                let Ok(bytes) = encode_key(name, ctrl, alt) else {
                    continue;
                };
                if Key::from_bytes(&bytes).is_ok() {
                    accepted.push(bytes);
                }
            }
        }
        accepted.sort();
        accepted.dedup();

        let mut table: Vec<Vec<u8>> = Key::ALLOWED
            .iter()
            .map(|key| key.bytes().to_vec())
            .collect();
        table.sort();
        assert_eq!(
            accepted, table,
            "the vocabulary reaches the table and nothing more"
        );
    }

    #[test]
    fn every_single_byte_but_the_allowed_ones_is_refused() {
        for byte in 0..=u8::MAX {
            let allowed = matches!(byte, b'h' | b'j' | b'k' | b'l' | b'q' | b'y' | b'\r' | 0x1b);

            assert_eq!(Key::from_bytes(&[byte]).is_ok(), allowed, "0x{byte:02x}");
        }
    }

    #[test]
    fn a_write_that_is_more_than_one_key_or_less_is_refused() {
        for write in [
            &b""[..],
            b"qy",
            b"hh",
            b"\x1b[A\x1b[A",
            b"\x1b[",
            b"\x1b[121u",
            b"\x1bO",
            b"\x1b\r",
            b"\x1by",
            b"[",
            b"O",
            b"\r\n",
            b"/",
            b"Y",
            b"n",
            b"e",
            b"\t",
            b"\x1b[5~",
            b"\x1b[1;3A",
            b"\x03",
            "ü".as_bytes(),
        ] {
            assert!(Key::from_bytes(write).is_err(), "{write:02x?}");
        }
    }

    #[test]
    fn no_sequence_of_allowed_keys_can_be_read_as_a_request_for_a_deletion() {
        // The scan is the reader the harness's own guards use. Whatever the keys are written
        // after, and however many of them follow one another, it must never read the soak's
        // writes as a request (Backspace, Ctrl+H, or the continuation of an open sequence).
        fn walk(scan: InputScan, depth: usize) {
            for key in Key::ALLOWED {
                assert!(
                    !scan.peek(key.bytes()).request,
                    "{key} can be read as a request after what was written before it"
                );
                let mut next = scan;
                let reading = next.note(key.bytes());
                assert!(!reading.request, "{key}");
                assert!(
                    !next.holds_a_sequence(),
                    "{key} leaves an escape sequence open"
                );
                if depth > 1 {
                    walk(next, depth - 1);
                }
            }
        }

        walk(InputScan::default(), 3);
    }

    /// Every key name the scenario vocabulary has.
    fn all_key_names() -> Vec<KeyName> {
        let mut names = vec![
            KeyName::Enter,
            KeyName::Esc,
            KeyName::Backspace,
            KeyName::Tab,
            KeyName::Up,
            KeyName::Down,
            KeyName::Left,
            KeyName::Right,
            KeyName::PageUp,
            KeyName::PageDown,
        ];
        names.extend((0x20_u8..0x7f).map(|byte| KeyName::Char(char::from(byte))));
        names
    }
}
