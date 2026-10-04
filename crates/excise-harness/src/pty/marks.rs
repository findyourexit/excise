//! The frame marks in the terminal output of `excise`.
//!
//! With the event channel open, `excise` follows every `frame` event with a mark in the terminal
//! output: `ESC ] 9471 ; excise-frame=<seq> BEL`, `seq` the number of the frame. The mark goes
//! through the same writer as the frame's bytes and is queued right after them. The `frame` event
//! cannot say where those bytes are: it is written when the frame is *queued* for the terminal, and
//! the terminal can still hold the bytes back for as long as it likes.
//!
//! What a reader that has reached a mark knows depends on the terminal. The program writes the mark
//! after every byte of its frame, and a Unix pseudo-terminal relays the output in order, so there a
//! reader that has read up to the mark has read every byte of that frame and none of the next:
//! reading up to the mark is exact, and so is the screen. `ConPTY` on Windows re-renders instead of
//! relaying. It parses the program's output into a buffer of its own, passes the mark through as
//! soon as it has parsed it, and paints the screen later, on its own timer, so the mark reaches the
//! reader before the paint of its frame. There the session's `frame_shown` says that the console
//! host has the frame, not that the screen model shows it, and no quiet-time rule can prove a paint
//! complete: a repaint can be split after a cursor or control prefix, and a paint begun before the
//! mark can be flushed after it, leaving a stale dialog on the screen.
//!
//! `SCREEN_IS_EXACT` in `runner::live` says which of the two a terminal is: true on Unix, false on
//! Windows. Where it is false, the harness confirms no deletion from the screen: the `delete` step,
//! in both modes, and the driver's `delete` and its confirmation guard refuse before any key. Reads
//! that decide nothing destructive (`settle`, `select`, `resize`, `wait_refresh`, the first frame)
//! wait a bounded time there instead: for the first output that is not a mark after the mark and
//! then a bounded quiet read, or for the frame window to pass with nothing painted. Where it is
//! true, the mark alone decides.
//!
//! [`MarkScanner`] finds the marks in the stream, so that the screen model is fed the bytes around
//! them and never a mark, and the session knows the latest frame whose mark it has read. It copes
//! with a mark that is split across reads, with several in one read, and with text that only begins
//! like one: that text passes through unchanged. At most the bytes of an unfinished mark are held
//! back from one read to the next, because the next read can complete them.

/// Escape, which every mark begins with.
const ESC: u8 = 0x1b;
/// What every mark begins with, up to the number.
const PREFIX: &[u8] = b"\x1b]9471;excise-frame=";
/// The byte that ends a mark.
const TERMINATOR: u8 = 0x07;
/// The most digits a frame number has: `u64::MAX` is twenty digits long.
const MAX_DIGITS: usize = 20;

/// One piece of a stream that [`MarkScanner::feed`] has split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Piece<'a> {
    /// Bytes that are not part of a mark, in the order of the stream.
    Bytes(&'a [u8]),
    /// A complete mark: the stream has delivered every byte of this frame.
    Frame(u64),
}

/// How the start of some bytes relates to a mark.
enum Match {
    /// The bytes begin with a whole mark of `len` bytes, for frame `seq`.
    Complete { len: usize, seq: u64 },
    /// Every byte so far fits a mark, and the bytes ended: the next read can finish it.
    Incomplete,
    /// The bytes are not the start of a mark.
    No,
}

/// Splits a terminal output stream into its bytes and its frame marks.
#[derive(Debug, Default)]
pub(super) struct MarkScanner {
    /// The end of the last read, held back because it fits the start of a mark and the read ended
    /// before the mark did: at most the longest mark that is not finished.
    held: Vec<u8>,
}

impl MarkScanner {
    /// Splits the next `bytes` of the stream and hands the pieces to `emit`, in stream order.
    ///
    /// Bytes that may be the start of a mark when the read ends are not emitted yet: they are
    /// emitted with the read that follows, as bytes if they turn out not to be a mark and as
    /// the mark if they are one.
    pub(super) fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(Piece<'_>)) {
        if self.held.is_empty() {
            let kept = split(bytes, &mut emit);
            self.held.extend_from_slice(&bytes[bytes.len() - kept..]);
            return;
        }
        let mut joined = std::mem::take(&mut self.held);
        joined.extend_from_slice(bytes);
        let kept = split(&joined, &mut emit);
        self.held.extend_from_slice(&joined[joined.len() - kept..]);
    }

    /// The bytes held back now: the start of a mark whose end has not come.
    #[cfg(test)]
    pub(super) fn held(&self) -> &[u8] {
        &self.held
    }
}

/// Emits the pieces of `data`, and returns how many bytes at its end it did not emit because they
/// may be the start of a mark.
fn split(data: &[u8], emit: &mut impl FnMut(Piece<'_>)) -> usize {
    // `plain` is where the bytes not emitted yet begin; the search for the next escape goes on
    // from `at`.
    let mut plain = 0;
    let mut at = 0;
    while let Some(offset) = data[at..].iter().position(|&byte| byte == ESC) {
        let start = at + offset;
        match match_mark(&data[start..]) {
            Match::Complete { len, seq } => {
                if plain < start {
                    emit(Piece::Bytes(&data[plain..start]));
                }
                emit(Piece::Frame(seq));
                plain = start + len;
                at = plain;
            }
            Match::Incomplete => {
                if plain < start {
                    emit(Piece::Bytes(&data[plain..start]));
                }
                return data.len() - start;
            }
            Match::No => at = start + 1,
        }
    }
    if plain < data.len() {
        emit(Piece::Bytes(&data[plain..]));
    }
    0
}

/// Whether `rest`, which begins with an escape, begins with a mark.
fn match_mark(rest: &[u8]) -> Match {
    let compared = rest.len().min(PREFIX.len());
    if rest[..compared] != PREFIX[..compared] {
        return Match::No;
    }
    let Some(after) = rest.get(PREFIX.len()..) else {
        return Match::Incomplete;
    };
    let digits = after
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits > MAX_DIGITS {
        return Match::No;
    }
    match after.get(digits) {
        None => Match::Incomplete,
        Some(&TERMINATOR) if digits > 0 => std::str::from_utf8(&after[..digits])
            .ok()
            .and_then(|text| text.parse().ok())
            .map_or(Match::No, |seq| Match::Complete {
                len: PREFIX.len() + digits + 1,
                seq,
            }),
        Some(_) => Match::No,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the screen model and the session are told, with neighbouring bytes joined, so that
    /// the result does not depend on where the reads ended.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Item {
        Bytes(Vec<u8>),
        Frame(u64),
    }

    fn bytes(text: &[u8]) -> Item {
        Item::Bytes(text.to_vec())
    }

    fn push_bytes(items: &mut Vec<Item>, more: &[u8]) {
        match items.last_mut() {
            Some(Item::Bytes(joined)) => joined.extend_from_slice(more),
            _ => items.push(bytes(more)),
        }
    }

    /// Feeds `reads` to one scanner, one after the other, and returns what it emitted and the
    /// scanner, which may still hold the end of the stream.
    fn scan(reads: &[&[u8]]) -> (Vec<Item>, MarkScanner) {
        let mut scanner = MarkScanner::default();
        let mut items: Vec<Item> = Vec::new();
        for read in reads {
            scanner.feed(read, |piece| match piece {
                Piece::Frame(seq) => items.push(Item::Frame(seq)),
                Piece::Bytes(more) => push_bytes(&mut items, more),
            });
            assert!(
                scanner.held().len() <= PREFIX.len() + MAX_DIGITS,
                "only the start of a mark is held back: {:?}",
                scanner.held()
            );
        }
        (items, scanner)
    }

    /// What `reads` come to when the stream ends after them: what the scanner still holds is
    /// bytes of the stream, as a screen that never saw the rest of a mark would have them.
    fn finished(reads: &[&[u8]]) -> Vec<Item> {
        let (mut items, scanner) = scan(reads);
        if !scanner.held().is_empty() {
            push_bytes(&mut items, scanner.held());
        }
        items
    }

    /// The stream whole, as one read.
    fn whole(stream: &[u8]) -> Vec<Item> {
        finished(&[stream])
    }

    #[test]
    fn a_mark_between_bytes_is_found_and_taken_out_of_them() {
        assert_eq!(
            whole(b"ab\x1b]9471;excise-frame=7\x07cd"),
            [bytes(b"ab"), Item::Frame(7), bytes(b"cd")]
        );
    }

    #[test]
    fn marks_at_either_end_and_back_to_back_leave_no_empty_pieces() {
        assert_eq!(
            whole(
                b"\x1b]9471;excise-frame=1\x07\x1b]9471;excise-frame=2\x07x\x1b]9471;excise-frame=30\x07"
            ),
            [
                Item::Frame(1),
                Item::Frame(2),
                bytes(b"x"),
                Item::Frame(30)
            ]
        );
        assert_eq!(whole(b""), []);
    }

    #[test]
    fn a_mark_between_escape_sequences_leaves_them_whole() {
        assert_eq!(
            whole(b"\x1b[31mred\x1b]9471;excise-frame=2\x07\x1b[0m"),
            [bytes(b"\x1b[31mred"), Item::Frame(2), bytes(b"\x1b[0m")]
        );
    }

    #[test]
    fn an_escape_in_front_of_a_mark_is_not_part_of_it() {
        assert_eq!(
            whole(b"\x1b\x1b]9471;excise-frame=5\x07"),
            [bytes(b"\x1b"), Item::Frame(5)]
        );
    }

    #[test]
    fn several_marks_in_one_read_are_all_found_in_order() {
        let (items, scanner) = scan(&[
            b"a\x1b]9471;excise-frame=1\x07b\x1b]9471;excise-frame=2\x07\x1b]9471;excise-frame=3\x07c",
        ]);

        assert_eq!(
            items,
            [
                bytes(b"a"),
                Item::Frame(1),
                bytes(b"b"),
                Item::Frame(2),
                Item::Frame(3),
                bytes(b"c")
            ]
        );
        assert!(scanner.held().is_empty());
    }

    #[test]
    fn a_mark_split_at_every_possible_place_is_still_found() {
        let stream = b"head\x1b]9471;excise-frame=1234\x07tail";
        let expected = [bytes(b"head"), Item::Frame(1234), bytes(b"tail")];
        assert_eq!(whole(stream), expected);

        for cut in 0..=stream.len() {
            let (items, scanner) = scan(&[&stream[..cut], &stream[cut..]]);
            assert_eq!(items, expected, "cut after {cut} bytes");
            assert!(scanner.held().is_empty(), "cut after {cut} bytes");
        }
        // One byte per read, the narrowest reads there are.
        let singles: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(finished(&singles), expected);
    }

    #[test]
    fn the_start_of_a_mark_at_the_end_of_a_read_is_held_back_until_the_next_read() {
        let mut scanner = MarkScanner::default();
        let mut seen = Vec::new();
        let mut take = |piece: Piece<'_>| match piece {
            Piece::Bytes(more) => push_bytes(&mut seen, more),
            Piece::Frame(seq) => seen.push(Item::Frame(seq)),
        };

        scanner.feed(b"abc\x1b]9471;exc", &mut take);
        assert_eq!(scanner.held(), b"\x1b]9471;exc");

        scanner.feed(b"ise-frame=3", &mut take);
        assert_eq!(scanner.held(), b"\x1b]9471;excise-frame=3");

        scanner.feed(b"\x07def", &mut take);
        assert!(scanner.held().is_empty());
        assert_eq!(
            seen,
            [bytes(b"abc"), Item::Frame(3), bytes(b"def")],
            "nothing was emitted twice or lost"
        );
    }

    #[test]
    fn a_start_that_does_not_become_a_mark_is_released_unchanged_with_what_follows() {
        let (items, scanner) = scan(&[b"abc\x1b]94", b"zzz"]);

        assert_eq!(items, [bytes(b"abc\x1b]94zzz")]);
        assert!(scanner.held().is_empty());

        let (items, scanner) = scan(&[b"\x1b]9471;excise-frame=12", b"x\x07"]);
        assert_eq!(items, [bytes(b"\x1b]9471;excise-frame=12x\x07")]);
        assert!(scanner.held().is_empty());
    }

    #[test]
    fn a_lone_escape_at_the_end_of_a_read_is_held_and_then_released() {
        let (items, scanner) = scan(&[b"a\x1b", b"[2J"]);

        assert_eq!(items, [bytes(b"a\x1b[2J")]);
        assert!(scanner.held().is_empty());
    }

    #[test]
    fn text_that_only_begins_like_a_mark_passes_through_unchanged() {
        for text in [
            // No digits.
            &b"\x1b]9471;excise-frame=\x07"[..],
            // A character after the digits that is not the terminator.
            b"\x1b]9471;excise-frame=12x\x07",
            b"\x1b]9471;excise-frame=12;\x07",
            b"\x1b]9471;excise-frame=1 2\x07",
            // Another number, another word, another sequence.
            b"\x1b]9472;excise-frame=1\x07",
            b"\x1b]9471;excise-frames=1\x07",
            b"\x1b]9471;excise-frame:1\x07",
            b"\x1b]9471excise-frame=1\x07",
            b"\x1b[9471;excise-frame=1\x07",
            b"\x1b]0;excise-frame=1\x07",
            // The string terminator of another style of OSC is not the terminator of a mark.
            b"\x1b]9471;excise-frame=1\x1b\\",
            // A number too long for a frame number, and one of twenty digits that does not fit.
            b"\x1b]9471;excise-frame=123456789012345678901\x07",
            b"\x1b]9471;excise-frame=99999999999999999999\x07",
        ] {
            let mut stream = b"<".to_vec();
            stream.extend_from_slice(text);
            stream.extend_from_slice(b">");

            assert_eq!(
                whole(&stream),
                [Item::Bytes(stream.clone())],
                "{}",
                String::from_utf8_lossy(&stream).escape_debug()
            );
        }
    }

    #[test]
    fn the_largest_frame_number_is_a_mark() {
        let stream = format!("\x1b]9471;excise-frame={}\x07", u64::MAX);

        assert_eq!(whole(stream.as_bytes()), [Item::Frame(u64::MAX)]);
    }

    #[test]
    fn what_cannot_be_a_mark_is_never_held_back() {
        // None of these can grow into a mark whatever follows, so they go on at once.
        for read in [
            &b"plain text"[..],
            b"\x1b[31m",
            b"\x1b[",
            b"\x1b]0;title\x07",
            b"\x1b]9472",
            b"\x1b\x1bx",
        ] {
            let (_, scanner) = scan(&[read]);

            assert!(
                scanner.held().is_empty(),
                "{:?} was held back",
                String::from_utf8_lossy(read)
            );
        }
    }

    /// The same stream split anywhere gives the same items: a deterministic walk over streams made
    /// of the pieces that matter (whole marks, parts of marks, plain bytes, other escape
    /// sequences), each cut at every place.
    #[test]
    fn where_the_reads_end_never_changes_what_is_found() {
        const FRAGMENTS: [&[u8]; 13] = [
            b"a",
            b"\x1b",
            b"]",
            b"9471;",
            b"excise-frame=",
            b"12",
            b"\x07",
            b"\x1b]9471;excise-frame=",
            b"\x1b]9471;excise-frame=5\x07",
            b"\x1b]9471;excise-frame=60\x07",
            b"\x1b[31m",
            b"\x1b\\",
            b"x",
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move |bound: usize| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            usize::try_from(state >> 33).expect("a small number") % bound
        };

        for _ in 0..400 {
            let mut stream = Vec::new();
            for _ in 0..=next(8) {
                stream.extend_from_slice(FRAGMENTS[next(FRAGMENTS.len())]);
            }
            let expected = whole(&stream);
            // What came out is the stream with the marks taken out of it, and nothing else.
            let mut rebuilt = Vec::new();
            for item in &expected {
                match item {
                    Item::Bytes(more) => rebuilt.extend_from_slice(more),
                    Item::Frame(seq) => {
                        rebuilt.extend_from_slice(
                            format!("\x1b]9471;excise-frame={seq}\x07").as_bytes(),
                        );
                    }
                }
            }
            assert_eq!(rebuilt, stream, "the pieces rebuild the stream");

            for cut in 0..=stream.len() {
                assert_eq!(
                    finished(&[&stream[..cut], &stream[cut..]]),
                    expected,
                    "{:?} cut after {cut}",
                    String::from_utf8_lossy(&stream)
                );
            }
        }
    }
}
