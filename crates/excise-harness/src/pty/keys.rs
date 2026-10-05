//! Key encoding: what a terminal writes to the pseudo-terminal for a key press.
//!
//! The encodings are the ones an xterm-compatible terminal sends, which is what the input reader
//! of `excise` (crossterm) decodes:
//!
//! | Key | Bytes |
//! |---|---|
//! | a character | its UTF-8 bytes |
//! | `ctrl` and a letter | the letter's control code (`ctrl+c` is `0x03`) |
//! | `ctrl` and `@`, space, `[`, `\`, `^`, `_`, `?` | `0x00`, `0x00`, `0x1b`, `0x1c`, `0x1e`, `0x1f`, `0x7f` |
//! | `ctrl` and `]` | none: `0x1d` is the input barrier request ([`BARRIER`]), which only the harness writes, so the key is an error |
//! | `alt` and a key | `ESC` followed by the bytes of the key |
//! | `enter` | carriage return (`0x0d`) |
//! | `esc` | `0x1b` |
//! | `backspace` | `0x7f` |
//! | `tab` | `0x09` |
//! | an arrow | `ESC [ A`, `B`, `C`, or `D` for up, down, right, left |
//! | `page_up`, `page_down` | `ESC [ 5 ~` and `ESC [ 6 ~` |
//!
//! An arrow or page key with modifiers is `ESC [ 1 ; m A` (`ESC [ 5 ; m ~` for a page key), where
//! `m` is 1, plus 2 for alt, plus 4 for ctrl.
//!
//! A terminal cannot tell some combinations apart, and neither can this encoding: `ctrl+i` is a tab,
//! `ctrl+m` is an enter, and `ctrl+[` is an escape. A combination with no encoding at all, such as
//! `ctrl+enter` or `ctrl+1`, is an error instead of a guess.

use thiserror::Error;

use crate::scenario::KeyName;

const ESCAPE: u8 = 0x1b;

/// The input barrier request: `0x1d`, the control code of `ctrl+]`.
///
/// While the test event channel is open, `excise` reads it as a request that is not a key: it
/// draws a frame whose `barriers` counts the requests it has read. A request that a frame answers
/// was read after everything written before it, and the frame shows what all of that did. A
/// terminal writes no such byte for a key a person presses. Only the harness writes it, and only
/// as a request that it counts: a `key` that wrote it would be a request that nothing counted,
/// and a later request could take that one's answer for its own. So `ctrl+]`, with or without
/// `alt`, has no encoding here ([`KeyError::Reserved`]).
pub const BARRIER: u8 = 0x1d;

/// A key or text that has no terminal encoding.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum KeyError {
    /// A terminal has no byte sequence for this key with these modifiers.
    #[error("`{key}` with {modifiers} has no terminal encoding")]
    Unencodable {
        /// The key as written in a scenario.
        key: String,
        /// The modifiers, for example `ctrl`.
        modifiers: &'static str,
    },
    /// The key is reserved for the harness.
    #[error(
        "`{key}` is the input barrier request that the harness writes to the program, so no key \
         step can write it"
    )]
    Reserved {
        /// The key as written in a scenario.
        key: String,
    },
    /// Typed text contains a control character.
    #[error(
        "typed text must not contain control characters, but it contains {character:?}; press \
         `enter`, `esc`, or `tab` with a `key` step instead"
    )]
    ControlCharacter {
        /// The offending character.
        character: char,
    },
}

/// The bytes a terminal sends for pressing `key` with the given modifiers.
///
/// # Errors
///
/// Returns [`KeyError::Unencodable`] for a combination that has no terminal encoding.
pub fn encode_key(key: KeyName, ctrl: bool, alt: bool) -> Result<Vec<u8>, KeyError> {
    let unencodable = || KeyError::Unencodable {
        key: key.to_string(),
        modifiers: "ctrl",
    };
    if ctrl && key == KeyName::Char(']') {
        return Err(KeyError::Reserved {
            key: if alt { "alt+ctrl+]" } else { "ctrl+]" }.to_owned(),
        });
    }
    let mut bytes = Vec::with_capacity(8);
    match key {
        KeyName::Char(character) if ctrl => {
            bytes.push(control_code(character).ok_or_else(unencodable)?);
        }
        KeyName::Char(character) => {
            let mut utf8 = [0_u8; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut utf8).as_bytes());
        }
        KeyName::Enter | KeyName::Esc | KeyName::Backspace | KeyName::Tab if ctrl => {
            return Err(unencodable());
        }
        KeyName::Enter => bytes.push(b'\r'),
        KeyName::Esc => bytes.push(ESCAPE),
        KeyName::Backspace => bytes.push(0x7f),
        KeyName::Tab => bytes.push(b'\t'),
        KeyName::Up => cursor_key(&mut bytes, b'A', ctrl, alt),
        KeyName::Down => cursor_key(&mut bytes, b'B', ctrl, alt),
        KeyName::Right => cursor_key(&mut bytes, b'C', ctrl, alt),
        KeyName::Left => cursor_key(&mut bytes, b'D', ctrl, alt),
        KeyName::PageUp => tilde_key(&mut bytes, 5, ctrl, alt),
        KeyName::PageDown => tilde_key(&mut bytes, 6, ctrl, alt),
    }
    let is_sequence = matches!(
        key,
        KeyName::Up
            | KeyName::Down
            | KeyName::Left
            | KeyName::Right
            | KeyName::PageUp
            | KeyName::PageDown
    );
    if alt && !is_sequence {
        bytes.insert(0, ESCAPE);
    }
    Ok(bytes)
}

/// The bytes of `text`, one entry per character, so each character is its own key press.
///
/// # Errors
///
/// Returns [`KeyError::ControlCharacter`] if `text` contains a control character.
pub fn encode_text(text: &str) -> Result<Vec<Vec<u8>>, KeyError> {
    text.chars()
        .map(|character| {
            if character.is_control() {
                return Err(KeyError::ControlCharacter { character });
            }
            let mut utf8 = [0_u8; 4];
            Ok(character.encode_utf8(&mut utf8).as_bytes().to_vec())
        })
        .collect()
}

/// The control code of `character`, if a terminal has one.
fn control_code(character: char) -> Option<u8> {
    match character {
        'a'..='z' | 'A'..='Z' => Some(character.to_ascii_lowercase() as u8 - b'a' + 1),
        '@' | ' ' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// The xterm modifier parameter: 1, plus 2 for alt, plus 4 for ctrl.
const fn modifier_parameter(ctrl: bool, alt: bool) -> u8 {
    1 + if alt { 2 } else { 0 } + if ctrl { 4 } else { 0 }
}

fn cursor_key(bytes: &mut Vec<u8>, final_byte: u8, ctrl: bool, alt: bool) {
    bytes.extend_from_slice(&[ESCAPE, b'[']);
    if ctrl || alt {
        bytes.extend_from_slice(b"1;");
        bytes.push(b'0' + modifier_parameter(ctrl, alt));
    }
    bytes.push(final_byte);
}

fn tilde_key(bytes: &mut Vec<u8>, code: u8, ctrl: bool, alt: bool) {
    bytes.extend_from_slice(&[ESCAPE, b'[', b'0' + code]);
    if ctrl || alt {
        bytes.push(b';');
        bytes.push(b'0' + modifier_parameter(ctrl, alt));
    }
    bytes.push(b'~');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> KeyName {
        text.parse().expect("a valid key name")
    }

    fn encode(text: &str, ctrl: bool, alt: bool) -> Vec<u8> {
        encode_key(key(text), ctrl, alt).expect("an encodable key")
    }

    #[test]
    fn plain_characters_are_their_utf8_bytes() {
        assert_eq!(encode("y", false, false), b"y");
        assert_eq!(encode("/", false, false), b"/");
        assert_eq!(encode(" ", false, false), b" ");
        assert_eq!(encode("E", false, false), b"E");
        assert_eq!(encode("é", false, false), "é".as_bytes());
        assert_eq!(encode("→", false, false), "→".as_bytes());
    }

    #[test]
    fn control_characters_are_control_codes() {
        assert_eq!(encode("c", true, false), [0x03]);
        assert_eq!(encode("n", true, false), [0x0e]);
        assert_eq!(encode("F", true, false), [0x06], "case does not matter");
        assert_eq!(encode("@", true, false), [0x00]);
        assert_eq!(encode(" ", true, false), [0x00]);
        assert_eq!(encode("[", true, false), [0x1b]);
        assert_eq!(encode("\\", true, false), [0x1c]);
        assert_eq!(encode("^", true, false), [0x1e]);
        assert_eq!(encode("_", true, false), [0x1f]);
        assert_eq!(encode("?", true, false), [0x7f]);
    }

    #[test]
    fn the_input_barrier_request_is_not_a_key() {
        for alt in [false, true] {
            let error = encode_key(key("]"), true, alt).expect_err("reserved for the harness");
            assert!(
                matches!(&error, KeyError::Reserved { key } if key.ends_with("ctrl+]")),
                "{error}"
            );
        }
        // Only that one key is reserved: `]` itself and the neighbouring control codes are not.
        assert_eq!(encode("]", false, false), b"]");
        assert_eq!(encode("\\", true, false), [0x1c]);
        assert_eq!(encode("^", true, false), [0x1e]);
        assert_eq!(BARRIER, 0x1d);
        assert_eq!(control_code(']'), Some(BARRIER));
    }

    #[test]
    fn alt_prefixes_an_escape() {
        assert_eq!(encode("x", false, true), [0x1b, b'x']);
        assert_eq!(encode("f", true, true), [0x1b, 0x06]);
        assert_eq!(encode("enter", false, true), [0x1b, b'\r']);
        assert_eq!(encode("esc", false, true), [0x1b, 0x1b]);
        assert_eq!(encode("backspace", false, true), [0x1b, 0x7f]);
        assert_eq!(
            encode("é", false, true),
            [&[0x1b][..], "é".as_bytes()].concat()
        );
    }

    #[test]
    fn named_keys_have_their_usual_bytes() {
        assert_eq!(encode("enter", false, false), b"\r");
        assert_eq!(encode("esc", false, false), [0x1b]);
        assert_eq!(encode("backspace", false, false), [0x7f]);
        assert_eq!(encode("tab", false, false), b"\t");
    }

    #[test]
    fn arrows_and_page_keys_are_escape_sequences() {
        assert_eq!(encode("up", false, false), b"\x1b[A");
        assert_eq!(encode("down", false, false), b"\x1b[B");
        assert_eq!(encode("right", false, false), b"\x1b[C");
        assert_eq!(encode("left", false, false), b"\x1b[D");
        assert_eq!(encode("page_up", false, false), b"\x1b[5~");
        assert_eq!(encode("page_down", false, false), b"\x1b[6~");
    }

    #[test]
    fn modifiers_use_the_xterm_parameter() {
        assert_eq!(encode("up", true, false), b"\x1b[1;5A");
        assert_eq!(encode("left", false, true), b"\x1b[1;3D");
        assert_eq!(encode("down", true, true), b"\x1b[1;7B");
        assert_eq!(encode("page_down", true, false), b"\x1b[6;5~");
        assert_eq!(encode("page_up", false, true), b"\x1b[5;3~");
    }

    #[test]
    fn a_combination_with_no_encoding_is_an_error_not_a_guess() {
        for (text, ctrl) in [
            ("enter", true),
            ("esc", true),
            ("tab", true),
            ("backspace", true),
            ("1", true),
            ("é", true),
        ] {
            let error = encode_key(key(text), ctrl, false).expect_err("no such encoding");
            assert!(
                matches!(error, KeyError::Unencodable { .. }),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn typed_text_is_one_key_press_per_character() {
        assert_eq!(
            encode_text("a/é").expect("text"),
            [b"a".to_vec(), b"/".to_vec(), "é".as_bytes().to_vec()]
        );
    }

    #[test]
    fn typed_text_may_not_smuggle_in_a_control_character() {
        for text in ["a\nb", "tab\there", "\u{1b}[A", "del\u{7f}"] {
            let error = encode_text(text).expect_err("control characters are refused");
            assert!(
                matches!(error, KeyError::ControlCharacter { .. }),
                "{text:?}"
            );
        }
    }
}
