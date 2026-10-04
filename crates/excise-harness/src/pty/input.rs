//! What a program can read from the bytes written to its input, for the two questions the guards of
//! the scenario runner and of the interactive driver ask: can the bytes ask the program for a
//! deletion dialog, and can they confirm one?
//!
//! [`InputScan`] is fed every write to the program, in the order they are made: a step's own keys,
//! the protocols' (Backspace, `/`, the text of a filter, Enter, Esc, `q`, `y`), and the barrier
//! request, so that what it holds between writes is what the program's parser holds. It reads each
//! write ([`Reading`]) and the caller decides what that means: only a write of a step's own that
//! can ask for a deletion engages a scenario run, and the driver guards every input that can
//! confirm one.
//!
//! # By their bytes
//!
//! * One key asks `excise` for a deletion dialog: Backspace with no modifier (`key!(Backspace)` in
//!   `src/input/controls.rs`; Delete, Ctrl+H and every other key are bound to nothing of the kind).
//!   crossterm 0.29.0's Unix input parser reads `0x7F` as Backspace (`parse_event`) and `0x08` as
//!   Ctrl+H. The Windows console path builds its key events from console input records instead, and
//!   was not checked for it, so `0x08` counts too: a guard that was not needed costs a barrier, and
//!   one that is missing costs a deletion that nobody verified. A write that holds either byte
//!   anywhere asks.
//! * A deletion dialog is confirmed by `y` and by Enter (`key!(Enter) | key!(char 'y')` in
//!   `handle_keypress_delete_confirm_mode`). A dialog that asks for a typed phrase takes every
//!   other character into the phrase, `Y` included, and a line feed is the key that a terminal can
//!   send for Enter. A write that holds `y`, `Y`, a carriage return, or a line feed anywhere
//!   confirms.
//! * The escape byte and a confirmation in one write (`alt+y`, `alt+enter`:
//!   [`is_escape_then_confirmation`]) is a write that a program may read as two inputs, the escape
//!   and then the key. The escape closes a prompt, a deletion dialog that was queued behind the
//!   prompt then opens, and the key confirms it, with nothing able to come between the two bytes:
//!   not a screen read, and not an input barrier. The guards refuse it outright.
//!
//! # By escape sequences
//!
//! A key can be spelled with none of those bytes. crossterm's parser keeps the bytes of an
//! unfinished escape sequence from one read to the next (`Parser::advance` in
//! `event/source/unix/mio.rs`), and a terminal does not promise that a write is a read, so the
//! bytes of different writes can be one key press. `ESC [ 121 u` is `y`, `ESC [ 13 u` and
//! `ESC [ 57414 u` are Enter, and `ESC [ 97:121 ; 2 u` is `y` as well, because the shifted
//! alternate code replaces the key and clears Shift (`parse_csi_u_encoded_key_code` in
//! `event/sys/unix/parse.rs`). A report of a mouse click is another. The Windows console reads its
//! own input sequences (win32 input mode, `CSI … _`), which name any key by its virtual-key code.
//!
//! No key of the scenario vocabulary writes such a sequence whole, and typed text has no control
//! characters. A sequence is composed: one write begins it (`alt+[` is `ESC [`, and `esc` is an
//! `ESC` that a `[` in the same read continues) and later writes continue and finish it. So the
//! scan decodes nothing: **a sequence that one write begins and a later write continues or finishes
//! counts as both a request and a confirmation, whatever its bytes**, and so does every write that
//! continues it, however long the sequence grows. What the scan holds between writes is a lone
//! `ESC`, `ESC [` and the bytes after it up to a final byte (`0x40..=0x7E`), or `ESC O`, which one
//! more byte finishes. After a lone `ESC`, a next `[` or `O` continues the sequence, another `ESC`
//! ends it and begins one of its own, and any other byte ends it. A write that begins and finishes
//! a sequence of its own (an arrow key, `alt+up`) is neither.
//!
//! The barrier request (`0x1D`) is one of the writes. After a lone `ESC` the program reads the two
//! as Ctrl+Alt+5, which is the barrier, so a barrier between an `ESC` and the next write is what
//! separates them. After `ESC [` the byte joins the sequence and the program never answers it:
//! [`InputScan::holds_a_sequence`] says that a sequence is open, so that a guard refuses the write
//! that would continue it instead of writing a barrier that cannot be answered.
//!
//! The session also answers the program's cursor-position requests with sequences of its own
//! (`ESC [ row ; column R`), which the scan does not see. They end in `R`, so they cannot finish a
//! sequence that a step composed into a key press that confirms.

/// The escape byte.
const ESCAPE: u8 = 0x1b;
/// The byte of Backspace.
const DELETE: u8 = 0x7f;
/// The control code of Ctrl+H, which the Windows console path may read as Backspace.
const BACKSPACE_CONTROL: u8 = 0x08;
/// The bytes that end a CSI sequence.
const FINAL_BYTES: std::ops::RangeInclusive<u8> = 0x40..=0x7e;

/// Whether `byte` confirms a deletion dialog or could be read as a key that does: `y`, `Y`, a
/// carriage return, or a line feed.
const fn is_confirmation_byte(byte: u8) -> bool {
    matches!(byte, b'y' | b'Y' | b'\r' | b'\n')
}

/// Whether `bytes` are the escape byte followed by bytes that hold a confirmation (`alt+y`,
/// `alt+enter`): one write that a program may read as two inputs, the escape and then the key.
///
/// The escape closes a prompt, a deletion dialog that was queued behind the prompt then opens,
/// and the key confirms it, with nothing able to come between the two bytes: not a screen read,
/// and not an input barrier.
#[must_use]
pub fn is_escape_then_confirmation(bytes: &[u8]) -> bool {
    matches!(
        bytes.split_first(),
        Some((&ESCAPE, rest)) if rest.iter().copied().any(is_confirmation_byte)
    )
}

/// What the bytes written to a program so far can be read as, as far as the write that was just
/// made, or is about to be, goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    /// They can be read as a request for a deletion dialog.
    pub request: bool,
    /// They can be read as the confirmation of one.
    pub confirmation: bool,
}

/// What the program's parser may be part-way through reading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Held {
    /// Nothing: every byte so far is a key of its own, or part of a sequence that has ended.
    #[default]
    Nothing,
    /// A lone `ESC`, last. The program may have read it already, as the Esc key; it may read it
    /// with the next byte as Alt and that key; and it may read it as the start of a sequence.
    Escape,
    /// `ESC [` and the bytes after it, up to a final byte: a CSI sequence that is not finished.
    Csi,
    /// `ESC O`, which one more byte finishes.
    Ss3,
}

/// The bytes written to a program's input, scanned for what it can read of them (see the module
/// documentation). It keeps the little that the program's parser keeps between reads and nothing
/// else, so it takes no more room for a sequence that never finishes than for one that does.
#[derive(Debug, Clone, Copy, Default)]
pub struct InputScan {
    held: Held,
}

impl InputScan {
    /// What a write of `bytes` would be read as, next, given everything written so far. Nothing
    /// changes: it is what a guard asks before it decides to write.
    #[must_use]
    pub fn peek(&self, bytes: &[u8]) -> Reading {
        scan(self.held, bytes).0
    }

    /// Notes a write of `bytes` as made, and returns what it is read as. It is called before the
    /// write, so that bytes that may have reached the program count even if the write then fails.
    pub fn note(&mut self, bytes: &[u8]) -> Reading {
        let (reading, held) = scan(self.held, bytes);
        self.held = held;
        reading
    }

    /// Whether an escape sequence is open that the next write continues, whatever it holds, and
    /// that no barrier can be written behind: the program reads a barrier request that follows
    /// `ESC [` or `ESC O` as a part of the sequence (or drops it), and never answers it. A lone
    /// `ESC` is not one: a barrier ends it.
    #[must_use]
    pub const fn holds_a_sequence(&self) -> bool {
        matches!(self.held, Held::Csi | Held::Ss3)
    }
}

/// Reads `bytes` as a write that follows what `held` stands for, and says what the program holds
/// after it.
fn scan(mut held: Held, bytes: &[u8]) -> (Reading, Held) {
    let mut reading = Reading {
        request: false,
        confirmation: false,
    };
    // Whether `held` is a sequence that an earlier write began, which this write still continues.
    // One that this write begins is not.
    let mut earlier = held != Held::Nothing;
    for &byte in bytes {
        reading.request |= matches!(byte, DELETE | BACKSPACE_CONTROL);
        reading.confirmation |= is_confirmation_byte(byte);
        let continues = earlier
            && match held {
                Held::Nothing => false,
                Held::Escape => matches!(byte, b'[' | b'O'),
                Held::Csi | Held::Ss3 => true,
            };
        if continues {
            reading.request = true;
            reading.confirmation = true;
        }
        held = match (held, byte) {
            (Held::Nothing | Held::Escape, ESCAPE) => Held::Escape,
            (Held::Escape, b'[') => Held::Csi,
            (Held::Escape, b'O') => Held::Ss3,
            (Held::Csi, _) if !FINAL_BYTES.contains(&byte) => Held::Csi,
            _ => Held::Nothing,
        };
        earlier = continues && matches!(held, Held::Csi | Held::Ss3);
    }
    (reading, held)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{pty::keys::encode_key, scenario::KeyName};

    const NEITHER: Reading = Reading {
        request: false,
        confirmation: false,
    };
    const BOTH: Reading = Reading {
        request: true,
        confirmation: true,
    };

    /// What one write of `bytes` is read as, to a program that has been written nothing.
    fn reading(bytes: &[u8]) -> Reading {
        InputScan::default().note(bytes)
    }

    /// What each of `writes` is read as, in order, to a program that has been written nothing.
    fn readings(writes: &[&[u8]]) -> Vec<Reading> {
        let mut scan = InputScan::default();
        writes.iter().map(|write| scan.note(write)).collect()
    }

    /// Spellings of a key press, none of which has a `y`, an Enter, or a Backspace byte in it:
    /// `y`, `Y`, Enter twice over (`CR` and the keypad's), `y` through its shifted alternate, `y`
    /// in the Windows console's win32 input mode (virtual key, scan code, character, key down,
    /// control keys, repeat count), and Backspace.
    const SPELLINGS: [&[u8]; 7] = [
        b"\x1b[121u",
        b"\x1b[89u",
        b"\x1b[13u",
        b"\x1b[57414u",
        b"\x1b[97:121;2u",
        b"\x1b[89;21;121;1;0;1_",
        b"\x1b[127u",
    ];

    #[test]
    fn a_delete_byte_asks_for_a_deletion_wherever_it_stands() {
        for bytes in [
            &[0x7f][..],
            &[0x1b, 0x7f],
            b"ab\x7fcd",
            &[0x08],
            &[0x1b, 0x08],
        ] {
            let read = reading(bytes);
            assert!(read.request, "{bytes:?}");
            assert!(!read.confirmation, "{bytes:?}");
        }
    }

    #[test]
    fn a_confirmation_is_y_enter_and_what_a_terminal_can_send_for_them() {
        for bytes in [&b"y"[..], b"Y", b"\r", b"\n", b"\x1by", b"\x1b\r", b"xyz"] {
            let read = reading(bytes);
            assert!(read.confirmation, "{bytes:?}");
            assert!(!read.request, "{bytes:?}");
        }
    }

    #[test]
    fn text_and_the_other_keys_are_neither() {
        for bytes in [
            &b"n"[..],
            b"q",
            b"/",
            b"victim.bin",
            "é→ÿ".as_bytes(),
            b"\t",
            &[0x1b],
            &[0x03],
            &[0x1b, b'x'],
            b"\x1b[A",
            b"\x1b[5~",
            b"\x1b[1;5A",
            b"\x1bOA",
            b"\x1b\x1b[A",
            b"",
        ] {
            assert_eq!(reading(bytes), NEITHER, "{bytes:?}");
        }
    }

    #[test]
    fn the_keys_of_the_vocabulary_are_read_as_one_write_that_nothing_continues() {
        let mut names: Vec<KeyName> = [
            "enter",
            "esc",
            "backspace",
            "tab",
            "up",
            "down",
            "left",
            "right",
            "page_up",
            "page_down",
        ]
        .iter()
        .map(|name| name.parse().expect("a key name"))
        .collect();
        names.extend((0x20_u8..0x7f).map(|byte| KeyName::Char(char::from(byte))));

        let (mut asking, mut confirming, mut compound, mut opening) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for key in names {
            for (ctrl, alt) in [(false, false), (true, false), (false, true), (true, true)] {
                let Ok(bytes) = encode_key(key, ctrl, alt) else {
                    continue;
                };
                let label = format!(
                    "{}{}{key}",
                    if alt { "alt+" } else { "" },
                    if ctrl { "ctrl+" } else { "" },
                );
                let mut scan = InputScan::default();
                let read = scan.note(&bytes);
                assert!(
                    !(read.request && read.confirmation),
                    "{label}: a write that nothing continues is not both"
                );
                if read.request {
                    asking.push(label.clone());
                }
                if read.confirmation {
                    confirming.push(label.clone());
                }
                if is_escape_then_confirmation(&bytes) {
                    compound.push(label.clone());
                }
                if scan.holds_a_sequence() {
                    opening.push(label);
                }
            }
        }
        for list in [&mut asking, &mut confirming, &mut compound, &mut opening] {
            list.sort();
        }

        // Backspace itself, `?` and `h` with ctrl (`0x7F` and `0x08`, `h` in either case), each
        // with and without alt.
        assert_eq!(
            asking,
            [
                "alt+backspace",
                "alt+ctrl+?",
                "alt+ctrl+H",
                "alt+ctrl+h",
                "backspace",
                "ctrl+?",
                "ctrl+H",
                "ctrl+h",
            ]
            .map(str::to_owned)
        );
        // `y`, `Y`, Enter, and `m` and `j` with ctrl (a carriage return and a line feed, in either
        // case), each with and without alt.
        assert_eq!(
            confirming,
            [
                "Y",
                "alt+Y",
                "alt+ctrl+J",
                "alt+ctrl+M",
                "alt+ctrl+j",
                "alt+ctrl+m",
                "alt+enter",
                "alt+y",
                "ctrl+J",
                "ctrl+M",
                "ctrl+j",
                "ctrl+m",
                "enter",
                "y",
            ]
            .map(str::to_owned)
        );
        // The ones with alt are the escape byte and a confirmation in one write.
        assert_eq!(
            compound,
            [
                "alt+Y",
                "alt+ctrl+J",
                "alt+ctrl+M",
                "alt+ctrl+j",
                "alt+ctrl+m",
                "alt+enter",
                "alt+y",
            ]
            .map(str::to_owned)
        );
        // The only keys that leave a sequence open, for a later write to continue, are the two
        // that begin one and nothing else.
        assert_eq!(opening, ["alt+O", "alt+["].map(str::to_owned));
    }

    #[test]
    fn a_sequence_that_one_write_begins_and_a_later_one_finishes_is_both_whatever_its_bytes() {
        for spelling in SPELLINGS {
            // Cut at every place, in two writes: the one that begins it is neither, and the one
            // that continues it is both.
            for cut in 1..spelling.len() {
                let (begin, rest) = spelling.split_at(cut);
                assert_eq!(
                    readings(&[begin, rest]),
                    [NEITHER, BOTH],
                    "{} cut after {cut}",
                    String::from_utf8_lossy(spelling)
                );
            }
            // A byte at a time: every write after the second byte continues it, and so does the
            // second.
            let singles: Vec<&[u8]> = spelling.chunks(1).collect();
            let mut expected = vec![NEITHER];
            expected.extend(std::iter::repeat_n(BOTH, spelling.len() - 1));
            assert_eq!(
                readings(&singles),
                expected,
                "{}",
                String::from_utf8_lossy(spelling)
            );
            // The write after it starts afresh.
            let mut with_a_word_after = singles.clone();
            with_a_word_after.push(b"n");
            assert_eq!(
                readings(&with_a_word_after).last(),
                Some(&NEITHER),
                "{}",
                String::from_utf8_lossy(spelling)
            );
        }
    }

    #[test]
    fn a_sequence_cut_in_three_is_both_in_the_second_and_the_third_write() {
        let spelling = SPELLINGS[4];
        for first in 1..spelling.len() - 1 {
            for second in first + 1..spelling.len() {
                assert_eq!(
                    readings(&[
                        &spelling[..first],
                        &spelling[first..second],
                        &spelling[second..]
                    ]),
                    [NEITHER, BOTH, BOTH],
                    "cut after {first} and {second}"
                );
            }
        }
    }

    #[test]
    fn a_lone_escape_is_continued_by_a_bracket_or_an_o_and_by_nothing_else() {
        assert_eq!(readings(&[b"\x1b", b"x"]), [NEITHER, NEITHER]);
        assert_eq!(readings(&[b"\x1b", b"["]), [NEITHER, BOTH]);
        assert_eq!(readings(&[b"\x1b", b"O"]), [NEITHER, BOTH]);
        assert_eq!(readings(&[b"\x1b", b"[121u"]), [NEITHER, BOTH]);
        assert_eq!(readings(&[b"\x1b", b"n", b"[121u"]), [NEITHER; 3]);
        // A second `ESC` ends the first, which the program reads as an Esc key, and begins a
        // sequence of its own, which a `[` then continues.
        assert_eq!(readings(&[b"\x1b", b"\x1b"]), [NEITHER, NEITHER]);
        assert_eq!(
            readings(&[b"\x1b", b"\x1b", b"["]),
            [NEITHER, NEITHER, BOTH]
        );
        assert_eq!(
            readings(&[b"\x1b", b"\x1b[", b"1"]),
            [NEITHER, NEITHER, BOTH]
        );
        assert_eq!(readings(&[b"\x1b\x1b", b"[121u"]), [NEITHER, BOTH]);
    }

    #[test]
    fn the_barrier_ends_a_lone_escape_and_cannot_end_a_sequence() {
        let barrier: &[u8] = &[0x1d];
        // The program reads `ESC` and the barrier as Ctrl+Alt+5: nothing is left for `[` to
        // continue.
        assert_eq!(
            readings(&[b"\x1b", barrier, b"[", b"1"]),
            [NEITHER; 4],
            "a barrier between the two keeps them apart"
        );
        // After `ESC [` the barrier joins the sequence, whose next byte it continues.
        let mut scan = InputScan::default();
        scan.note(b"\x1b[");
        assert!(scan.holds_a_sequence());
        assert_eq!(scan.note(barrier), BOTH);
        assert!(scan.holds_a_sequence());
        assert_eq!(scan.note(b"u"), BOTH);
        assert!(!scan.holds_a_sequence());
        // After `ESC O` it is the one byte that finishes it.
        let mut scan = InputScan::default();
        scan.note(b"\x1bO");
        assert!(scan.holds_a_sequence());
        assert_eq!(scan.note(barrier), BOTH);
        assert!(!scan.holds_a_sequence());
    }

    #[test]
    fn a_write_that_begins_and_finishes_a_sequence_of_its_own_is_neither() {
        for bytes in [
            &b"\x1b[A"[..],
            b"\x1b[5~",
            b"\x1b[1;7A",
            b"\x1bOA",
            b"\x1b\x1b[A",
            b"\x1b[",
            b"\x1b",
            b"\x1bO",
        ] {
            assert_eq!(reading(bytes), NEITHER, "{bytes:?}");
        }
        // Behind a sequence that has finished, too.
        assert_eq!(
            readings(&[b"\x1b[12", b"1u", b"\x1b[B", b"\x1b[5~"]),
            [NEITHER, BOTH, NEITHER, NEITHER]
        );
    }

    #[test]
    fn a_write_that_finishes_one_sequence_and_begins_another_continues_only_the_first() {
        assert_eq!(
            readings(&[b"\x1b[1", b"u\x1b[", b"2", b"u", b"n"]),
            [NEITHER, BOTH, BOTH, BOTH, NEITHER]
        );
        let mut scan = InputScan::default();
        scan.note(b"\x1b[1");
        scan.note(b"u\x1b[");
        assert!(
            scan.holds_a_sequence(),
            "the second is open, begun by the same write"
        );
        // Another `ESC` inside a sequence is a byte of it (it is not a final byte), and the
        // bracket that follows ends it.
        assert_eq!(readings(&[b"\x1b[1", b"\x1b[A"]), [NEITHER, BOTH]);
    }

    #[test]
    fn a_sequence_that_never_finishes_stays_open_and_every_write_that_continues_it_is_both() {
        let mut scan = InputScan::default();
        assert_eq!(scan.note(b"\x1b["), NEITHER);
        for _ in 0..2000 {
            assert_eq!(scan.note(b"1"), BOTH);
            assert!(scan.holds_a_sequence());
        }
        assert_eq!(scan.note(b"u"), BOTH);
        assert!(!scan.holds_a_sequence());
        assert_eq!(scan.note(b"x"), NEITHER);
    }

    #[test]
    fn peeking_at_a_write_changes_nothing() {
        let mut scan = InputScan::default();
        scan.note(b"\x1b");
        assert_eq!(scan.peek(b"["), BOTH);
        assert_eq!(scan.peek(b"["), BOTH, "the escape is still held");
        assert!(!scan.holds_a_sequence());
        assert_eq!(scan.peek(b"x"), NEITHER);
        assert_eq!(scan.note(b"["), BOTH);
        assert!(scan.holds_a_sequence());
        assert_eq!(scan.peek(b"1"), BOTH);
        assert!(scan.holds_a_sequence());
    }

    #[test]
    fn the_escape_byte_before_a_confirmation_is_one_write_that_a_barrier_cannot_split() {
        for bytes in [&b"\x1by"[..], b"\x1bY", b"\x1b\r", b"\x1b\n"] {
            assert!(is_escape_then_confirmation(bytes), "{bytes:?}");
        }
        for bytes in [
            &b"y"[..],
            b"\r",
            b"\x1b",
            b"\x1bn",
            b"\x1b[A",
            b"x\x1by",
            b"",
        ] {
            assert!(!is_escape_then_confirmation(bytes), "{bytes:?}");
        }
    }
}
