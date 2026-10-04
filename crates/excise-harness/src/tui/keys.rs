//! The keys of `cargo xtask tui keys`, and which of them the driver must not send blindly.
//!
//! # Notation
//!
//! Each argument is one key, or one run of typed text:
//!
//! * a key name from the scenario vocabulary: `enter`, `esc`, `backspace`, `tab`, `up`, `down`,
//!   `left`, `right`, `page_up`, `page_down`;
//! * one printable character: `y`, `/`, `?`, ` `;
//! * either of them behind `ctrl+` and `alt+`, in any combination: `ctrl+c`, `alt+up`,
//!   `ctrl+alt+x`;
//! * `type:` and the text, typed one character at a time: `type:node_modules`.
//!
//! The bytes are the ones the scenario `key` and `type` steps send ([`crate::pty::keys`]), and
//! each character of typed text is an input event of its own, as in a scenario.
//!
//! # What is not sent blindly
//!
//! A key can confirm a deletion only while the program shows a deletion dialog. The driver sends
//! a key that the program can read as a confirmation ([`crate::pty::input::InputScan`]: `y`,
//! `Y`, Enter, a line feed, and the bytes that finish an escape sequence that an earlier key
//! began, as `alt+[` and `type:121u` do between them) only when it has seen that no such dialog
//! is open, on a screen that shows what the program has read: it asks the program with an input
//! barrier first, whatever the earlier keys were and however the terminal cut them into input
//! events. The rules and the evidence they need are in the supervisor.

use crate::{
    pty::keys::{encode_key, encode_text},
    scenario::KeyName,
};

/// What introduces typed text.
const TEXT_PREFIX: &str = "type:";

/// One input event to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    /// The key as written on the command line; one character for typed text.
    pub label: String,
    /// The bytes a terminal sends for it.
    pub bytes: Vec<u8>,
}

/// Parses the keys of a `keys` command into the input events to send.
///
/// # Errors
///
/// Returns why a key is not valid: no keys at all, an unknown key name, a modifier that has no
/// terminal encoding (`ctrl+enter`), a repeated modifier, or typed text that is empty or holds a
/// control character. Nothing is sent when any key is invalid.
pub fn parse_keys(tokens: &[String]) -> Result<Vec<Input>, String> {
    let mut inputs = Vec::new();
    for token in tokens {
        inputs.extend(parse_token(token)?);
    }
    if inputs.is_empty() {
        return Err(format!(
            "no keys were given: name keys such as `down` or `enter`, single characters, \
             `ctrl+` or `alt+` combinations, or `{TEXT_PREFIX}<text>`"
        ));
    }
    Ok(inputs)
}

fn parse_token(token: &str) -> Result<Vec<Input>, String> {
    if let Some(text) = token.strip_prefix(TEXT_PREFIX) {
        if text.is_empty() {
            return Err(format!(
                "`{TEXT_PREFIX}` needs the text to type after the colon"
            ));
        }
        let typed = encode_text(text).map_err(|error| format!("`{token}`: {error}"))?;
        return Ok(text
            .chars()
            .zip(typed)
            .map(|(character, bytes)| Input {
                label: character.to_string(),
                bytes,
            })
            .collect());
    }

    let (mut ctrl, mut alt) = (false, false);
    let mut rest = token;
    loop {
        let (modifier, name, after) = if let Some(after) = rest.strip_prefix("ctrl+") {
            (&mut ctrl, "ctrl", after)
        } else if let Some(after) = rest.strip_prefix("alt+") {
            (&mut alt, "alt", after)
        } else {
            break;
        };
        if *modifier {
            return Err(format!("`{token}` names `{name}+` twice"));
        }
        *modifier = true;
        rest = after;
    }
    let key: KeyName = rest
        .parse()
        .map_err(|error| format!("`{token}`: {error}"))?;
    let bytes = encode_key(key, ctrl, alt).map_err(|error| format!("`{token}`: {error}"))?;
    Ok(vec![Input {
        label: token.to_owned(),
        bytes,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::input::{InputScan, Reading, is_escape_then_confirmation};

    fn tokens(text: &[&str]) -> Vec<String> {
        text.iter().map(|token| (*token).to_owned()).collect()
    }

    fn bytes(text: &[&str]) -> Vec<Vec<u8>> {
        parse_keys(&tokens(text))
            .expect("valid keys")
            .into_iter()
            .map(|input| input.bytes)
            .collect()
    }

    #[test]
    fn named_keys_and_characters_are_encoded_as_a_scenario_encodes_them() {
        assert_eq!(
            bytes(&[
                "enter",
                "esc",
                "backspace",
                "tab",
                "down",
                "page_up",
                "/",
                "y",
                " ",
                "é"
            ]),
            [
                b"\r".to_vec(),
                vec![0x1b],
                vec![0x7f],
                b"\t".to_vec(),
                b"\x1b[B".to_vec(),
                b"\x1b[5~".to_vec(),
                b"/".to_vec(),
                b"y".to_vec(),
                b" ".to_vec(),
                "é".as_bytes().to_vec(),
            ]
        );
    }

    #[test]
    fn modifiers_combine_in_either_order() {
        assert_eq!(bytes(&["ctrl+c"]), [vec![0x03]]);
        assert_eq!(bytes(&["alt+x"]), [vec![0x1b, b'x']]);
        assert_eq!(bytes(&["ctrl+alt+up"]), bytes(&["alt+ctrl+up"]));
        assert_eq!(bytes(&["ctrl+alt+up"]), [b"\x1b[1;7A".to_vec()]);
    }

    #[test]
    fn typed_text_is_one_input_per_character_labelled_by_it() {
        let inputs = parse_keys(&tokens(&["/", "type:a é", "enter"])).expect("valid keys");

        assert_eq!(
            inputs
                .iter()
                .map(|input| input.label.as_str())
                .collect::<Vec<_>>(),
            ["/", "a", " ", "é", "enter"]
        );
        assert_eq!(inputs[3].bytes, "é".as_bytes());
    }

    #[test]
    fn a_key_the_notation_does_not_have_is_refused_and_nothing_is_parsed() {
        for (token, reason) in [
            ("Enter", "unknown key"),
            ("pgup", "unknown key"),
            ("", "unknown key"),
            ("ctrl+alt", "unknown key"),
            ("ctrl+enter", "no terminal encoding"),
            ("ctrl++", "no terminal encoding"),
            ("ctrl+ctrl+c", "twice"),
            ("alt+alt+c", "twice"),
            ("type:", "needs the text"),
            ("type:two\nlines", "control character"),
        ] {
            let error = parse_keys(&tokens(&["down", token])).expect_err(token);

            assert!(error.contains(reason), "{token:?}: {error}");
        }
        let none = parse_keys(&[]).expect_err("no keys");
        assert!(none.contains("no keys were given"), "{none}");
    }

    /// What each key of `texts` is read as, in order, by a program that has been written nothing
    /// before: the scan that the supervisor keeps for a session, which every write feeds.
    fn readings(texts: &[&str]) -> Vec<Reading> {
        let mut scan = InputScan::default();
        parse_keys(&tokens(texts))
            .expect("valid keys")
            .into_iter()
            .map(|input| scan.note(&input.bytes))
            .collect()
    }

    const NEITHER: Reading = Reading {
        request: false,
        confirmation: false,
    };
    const BOTH: Reading = Reading {
        request: true,
        confirmation: true,
    };

    #[test]
    fn every_way_to_send_a_confirmation_is_one() {
        for token in [
            "y",
            "Y",
            "enter",
            "ctrl+m",
            "ctrl+j",
            "alt+y",
            "alt+enter",
            "type:y",
        ] {
            assert!(
                readings(&[token]).iter().any(|read| read.confirmation),
                "{token} must count as a confirmation"
            );
        }
    }

    #[test]
    fn a_confirmation_behind_the_escape_byte_is_compound_and_nothing_else_is() {
        for token in ["alt+y", "alt+Y", "alt+enter", "alt+ctrl+j", "ctrl+alt+m"] {
            let inputs = parse_keys(&tokens(&[token])).expect(token);

            assert!(
                inputs
                    .iter()
                    .all(|input| is_escape_then_confirmation(&input.bytes)),
                "{token} is one input of the escape byte and a confirmation"
            );
        }
        for token in [
            "y",
            "Y",
            "enter",
            "ctrl+j",
            "esc",
            "alt+n",
            "alt+backspace",
            "ctrl+alt+y",
            "type:y",
        ] {
            let inputs = parse_keys(&tokens(&[token])).expect(token);

            assert!(
                inputs
                    .iter()
                    .all(|input| !is_escape_then_confirmation(&input.bytes)),
                "{token} is not"
            );
        }
    }

    #[test]
    fn keys_that_cannot_confirm_are_not_confirmations() {
        for token in [
            "n",
            "esc",
            "down",
            "up",
            "page_down",
            "tab",
            "backspace",
            "ctrl+y",
            "alt+n",
            "alt+x",
            "alt+up",
            "ctrl+alt+up",
            "x",
            "/",
            "type:no",
        ] {
            assert!(
                readings(&[token]).iter().all(|read| !read.confirmation),
                "{token} must not count as a confirmation"
            );
        }
    }

    #[test]
    fn only_backspace_asks_for_a_deletion() {
        for (token, asks) in [
            ("backspace", true),
            ("alt+backspace", true),
            ("ctrl+h", true),
            ("esc", false),
            ("q", false),
            ("n", false),
            ("w", false),
            ("y", false),
            ("enter", false),
            ("/", false),
            ("up", false),
            ("alt+x", false),
        ] {
            assert_eq!(readings(&[token])[0].request, asks, "{token}");
        }
    }

    #[test]
    fn keys_that_compose_a_sequence_are_read_as_both_from_the_one_that_continues_it() {
        // `alt+[` is `ESC [`, and `type:121u` is the characters `1`, `2`, `1` and `u`, an input of
        // each: the program reads `ESC [ 121 u` as `y`, with no `y` anywhere.
        assert_eq!(
            readings(&["alt+[", "type:121u"]),
            [NEITHER, BOTH, BOTH, BOTH, BOTH]
        );
        // The same through a lone `esc`, which the next `[` turns into a sequence.
        assert_eq!(
            readings(&["esc", "[", "type:13u"]),
            [NEITHER, BOTH, BOTH, BOTH, BOTH]
        );
        // A key that opens `ESC O` and the key after it.
        assert_eq!(readings(&["alt+O", "x", "x"]), [NEITHER, BOTH, NEITHER]);
        // Keys that begin and finish a sequence of their own do not.
        assert_eq!(
            readings(&["up", "alt+up", "page_down", "ctrl+alt+up", "x"]),
            [NEITHER; 5]
        );
    }

    #[test]
    fn the_input_barrier_request_cannot_be_sent_as_a_key() {
        for token in ["ctrl+]", "alt+ctrl+]", "ctrl+alt+]"] {
            let error = parse_keys(&tokens(&[token])).expect_err(token);

            assert!(error.contains("input barrier request"), "{token}: {error}");
        }
        // The key itself, and its neighbours, are ordinary.
        for token in ["]", "ctrl+\\", "ctrl+^", "type:]"] {
            assert!(parse_keys(&tokens(&[token])).is_ok(), "{token}");
        }
    }
}
