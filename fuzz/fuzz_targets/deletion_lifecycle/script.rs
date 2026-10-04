//! Decodes a fuzz input into a fixture shape and a script of steps.
//!
//! The first four bytes choose the shape of the fixture and the runtime's settings; every byte
//! after them is one step, or the first byte of one. The language is meant to be read: the bytes
//! that seeds use are printable.
//!
//! | Byte | Step |
//! |---|---|
//! | `h` `j` `k` `l` | the keys `h` `j` `k` `l` (left, down, up, right on the default key preset) |
//! | `>` `<` | Enter and Esc |
//! | `d` | Backspace: ask to delete the selected entry |
//! | `y` `n` `q` `c` `s` `w` | those keys: confirm, cancel, quit, and the answers of the quit prompt |
//! | `+` `-` `0` `?` `[` `]` `!` | zoom in, zoom out, reset zoom, help, Page Up, Page Down, Ctrl-C |
//! | `.` | settle: wait until the runtime has nothing left to do (a barrier) |
//! | `/` *p* | open the filter, type pattern *p*, and apply it |
//! | `~` *p* | open the filter, type pattern *p*, and cancel it |
//! | `r` *s* | resize the terminal to size *s* |
//! | `m` *k* *a* *b* *c* | settle, then change the file system (mutation *k*, operands *a* *b* *c*) |
//! | `A` *k* *a* *b* *c* | settle, then arm mutation *k* to run when the executor takes the next plan |
//! | `^` | race: do not settle before the next step (see Scheduling) |
//! | space, tab, line breaks | ignored |
//!
//! # Scheduling
//!
//! Every key, filter, and resize step waits for a settle: the runtime, its worker threads, and
//! the scan store finish what the step before began, and only then does the step happen. What
//! a step does therefore depends on the script and the fixture alone, and an input replays the
//! same way every time (the trace digest that `EXCISE_FUZZ_TRACE=1` prints says so).
//!
//! `^` takes the settle before the next step away: the step is delivered while the runtime may
//! still be working on the one before it, a quit right behind a confirmation or a key right behind
//! a request, and which of the two the runtime handles first is up to the machine. `^` is the
//! only source of nondeterminism in the language, so an input that holds one may not replay as
//! it ran. `.`, `m`, and `A` settle first whatever precedes them, and `^` before them changes
//! nothing.
//!
//! # The rest
//!
//! Any other byte is one of the keys above, chosen by its value, so every input decodes. A
//! missing operand is zero. An operand that chooses among *n* things chooses the one its value
//! numbers, from zero, wrapping at *n*, and an ASCII digit counts as its own digit, so `3` is
//! three. The script ends after [`MAX_STEPS`] steps, and always continues with the fixed tail
//! that closes every run: settle, four Esc, then `q` `c` `s` `y`.

use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The most steps one input may decode to.
pub const MAX_STEPS: usize = 48;

/// The index an operand byte chooses among `len` things (`len` is not zero).
pub fn select(operand: u8, len: usize) -> usize {
    let number = if operand.is_ascii_digit() {
        operand - b'0'
    } else {
        operand
    };
    usize::from(number) % len
}

/// What the first four bytes of an input choose.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    /// How the fixture's entries are named, and so how they sort: 0 to 3.
    pub names: u8,
    /// How large the fixture's files are: 0 to 3.
    pub sizes: u8,
    /// Which optional parts the fixture has: 0 to 3.
    pub extra: u8,
    /// `--disable-delete-confirmation`: the session's reduced confirmation mode.
    pub reduced_confirmation: bool,
    /// Full motion, where the runtime animates, instead of reduced motion.
    pub full_motion: bool,
    /// Allocated sizes instead of apparent ones.
    pub allocated_sizes: bool,
}

/// A key the script presses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Left,
    Right,
    Up,
    Down,
    Enter,
    Esc,
    Backspace,
    PageUp,
    PageDown,
    CtrlC,
    Char(char),
}

impl Key {
    pub fn event(self) -> KeyEvent {
        let (code, modifiers) = match self {
            Self::Left => (KeyCode::Left, KeyModifiers::NONE),
            Self::Right => (KeyCode::Right, KeyModifiers::NONE),
            Self::Up => (KeyCode::Up, KeyModifiers::NONE),
            Self::Down => (KeyCode::Down, KeyModifiers::NONE),
            Self::Enter => (KeyCode::Enter, KeyModifiers::NONE),
            Self::Esc => (KeyCode::Esc, KeyModifiers::NONE),
            Self::Backspace => (KeyCode::Backspace, KeyModifiers::NONE),
            Self::PageUp => (KeyCode::PageUp, KeyModifiers::NONE),
            Self::PageDown => (KeyCode::PageDown, KeyModifiers::NONE),
            Self::CtrlC => (KeyCode::Char('c'), KeyModifiers::CONTROL),
            Self::Char(character) => (KeyCode::Char(character), KeyModifiers::NONE),
        };
        KeyEvent::new(code, modifiers)
    }

    /// Whether the key can confirm a deletion dialog.
    pub fn confirms(self) -> bool {
        matches!(self, Self::Enter | Self::Char('y'))
    }

    /// Whether the key can dismiss a deletion dialog.
    pub fn cancels(self) -> bool {
        matches!(self, Self::Esc | Self::Char('n' | 'q') | Self::CtrlC)
    }
}

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Char(character) => write!(formatter, "'{character}'"),
            other => write!(formatter, "{other:?}"),
        }
    }
}

/// One way the script changes the live tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationKind {
    CreateFile,
    CreateFolder,
    Remove,
    Rename,
    FileToFolder,
    FileToSymlink,
    HardLinkAdd,
    HardLinkRemove,
    Grow,
    FileToHardLink,
}

impl MutationKind {
    const ORDER: [Self; 10] = [
        Self::CreateFile,
        Self::CreateFolder,
        Self::Remove,
        Self::Rename,
        Self::FileToFolder,
        Self::FileToSymlink,
        Self::HardLinkAdd,
        Self::HardLinkRemove,
        Self::Grow,
        Self::FileToHardLink,
    ];

    fn decode(byte: u8) -> Self {
        match byte {
            b'f' => Self::CreateFile,
            b'd' => Self::CreateFolder,
            b'x' => Self::Remove,
            b'm' => Self::Rename,
            b'D' => Self::FileToFolder,
            b'S' => Self::FileToSymlink,
            b'L' => Self::HardLinkAdd,
            b'U' => Self::HardLinkRemove,
            b'g' => Self::Grow,
            b'H' => Self::FileToHardLink,
            other => Self::ORDER[select(other, Self::ORDER.len())],
        }
    }
}

/// A mutation and the bytes that choose what it acts on. Applying it picks entries of the live
/// tree by these bytes, so the same operation does something different once the tree differs.
#[derive(Clone, Copy, Debug)]
pub struct Op {
    pub kind: MutationKind,
    pub a: u8,
    pub b: u8,
    pub c: u8,
}

impl fmt::Display for Op {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?}({}, {}, {})",
            self.kind, self.a, self.b, self.c
        )
    }
}

/// The patterns a filter step can type.
pub const FILTER_PATTERNS: [&str; 12] = [
    "a", "txt", "*.txt", "**/*.txt", "*.log", "*.bin", "a*", "*", "alpha", "hard-a", "nowhere", "[",
];

/// The terminal sizes a resize step can pick, as (columns, rows). Deletion needs at least
/// 50 x 15, so some of them refuse it.
pub const SIZES: [(u16, u16); 8] = [
    (80, 24),
    (120, 40),
    (60, 20),
    (50, 15),
    (49, 14),
    (40, 10),
    (200, 60),
    (100, 30),
];

#[derive(Clone, Copy, Debug)]
pub enum Step {
    Key(Key),
    Filter { pattern: u8, commit: bool },
    Resize(u8),
    Settle,
    Mutate(Op),
    Arm(Op),
    Race,
}

impl fmt::Display for Step {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key(key) => write!(formatter, "key {key}"),
            Self::Filter { pattern, commit } => write!(
                formatter,
                "filter {:?} and {}",
                FILTER_PATTERNS[select(*pattern, FILTER_PATTERNS.len())],
                if *commit { "apply" } else { "cancel" }
            ),
            Self::Resize(size) => {
                let (columns, rows) = SIZES[select(*size, SIZES.len())];
                write!(formatter, "resize to {columns}x{rows}")
            }
            Self::Settle => formatter.write_str("settle"),
            Self::Mutate(op) => write!(formatter, "settle, then mutate {op}"),
            Self::Arm(op) => write!(formatter, "settle, then arm {op}"),
            Self::Race => formatter.write_str("race: no settle before the next step"),
        }
    }
}

/// A decoded input.
#[derive(Clone, Debug)]
pub struct Script {
    pub shape: Shape,
    pub steps: Vec<Step>,
}

/// The keys any byte without a step of its own stands for.
const KEYS: [Key; 24] = [
    Key::Left,
    Key::Right,
    Key::Up,
    Key::Down,
    Key::Enter,
    Key::Esc,
    Key::Backspace,
    Key::Char('y'),
    Key::Char('n'),
    Key::Char('q'),
    Key::Char('c'),
    Key::Char('s'),
    Key::Char('w'),
    Key::Char('h'),
    Key::Char('j'),
    Key::Char('k'),
    Key::Char('l'),
    Key::Char('+'),
    Key::Char('-'),
    Key::Char('0'),
    Key::Char('?'),
    Key::PageUp,
    Key::PageDown,
    Key::CtrlC,
];

/// What closes every run, so that the interface has to be navigable for the run to end:
/// the runtime must be idle, any modal must give way to Esc, and then `q` opens the quit prompt
/// whose answer is `y` when nothing is running, `c` while checks wait, and `s` during a removal.
pub const TAIL: [Step; 9] = [
    Step::Settle,
    Step::Key(Key::Esc),
    Step::Key(Key::Esc),
    Step::Key(Key::Esc),
    Step::Key(Key::Esc),
    Step::Key(Key::Char('q')),
    Step::Key(Key::Char('c')),
    Step::Key(Key::Char('s')),
    Step::Key(Key::Char('y')),
];

/// Decodes `data`. Every input decodes, and decodes the same way every time.
pub fn decode(data: &[u8]) -> Script {
    let mut bytes = data.iter().copied();
    let mut header = || bytes_next(&mut bytes);
    let names = header() % 4;
    let sizes = header() % 4;
    let extra = header() % 4;
    let flags = header() % 8;
    let shape = Shape {
        names,
        sizes,
        extra,
        reduced_confirmation: flags & 1 != 0,
        full_motion: flags & 2 != 0,
        allocated_sizes: flags & 4 != 0,
    };

    let mut steps = Vec::new();
    while steps.len() < MAX_STEPS {
        let Some(byte) = bytes.next() else { break };
        let step = match byte {
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            b'.' => Step::Settle,
            b'/' => Step::Filter {
                pattern: bytes_next(&mut bytes),
                commit: true,
            },
            b'~' => Step::Filter {
                pattern: bytes_next(&mut bytes),
                commit: false,
            },
            b'r' => Step::Resize(bytes_next(&mut bytes)),
            b'm' => Step::Mutate(read_op(&mut bytes)),
            b'A' => Step::Arm(read_op(&mut bytes)),
            b'^' => Step::Race,
            b'>' => Step::Key(Key::Enter),
            b'<' => Step::Key(Key::Esc),
            b'd' => Step::Key(Key::Backspace),
            b'[' => Step::Key(Key::PageUp),
            b']' => Step::Key(Key::PageDown),
            b'!' => Step::Key(Key::CtrlC),
            b'h' | b'j' | b'k' | b'l' | b'y' | b'n' | b'q' | b'c' | b's' | b'w' | b'+' | b'-'
            | b'0' | b'?' => Step::Key(Key::Char(char::from(byte))),
            other => Step::Key(KEYS[usize::from(other) % KEYS.len()]),
        };
        steps.push(step);
    }
    Script { shape, steps }
}

fn bytes_next(bytes: &mut impl Iterator<Item = u8>) -> u8 {
    bytes.next().unwrap_or(0)
}

fn read_op(bytes: &mut impl Iterator<Item = u8>) -> Op {
    Op {
        kind: MutationKind::decode(bytes_next(bytes)),
        a: bytes_next(bytes),
        b: bytes_next(bytes),
        c: bytes_next(bytes),
    }
}
