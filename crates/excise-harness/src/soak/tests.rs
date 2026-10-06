//! Tests of the read-only soak that need no real `excise`: the source tripwire, what the
//! pseudo-terminal layer writes on its own, the bounded copy of a recording, the copy of the binary
//! and the summary, each made whole or stopped with nothing left behind, and the runs against
//! scripted programs.

use std::{
    cell::{Cell, RefCell},
    fs,
    io::{self, Read, Write},
    path::Path,
    rc::Rc,
    sync::LazyLock,
    time::{Duration, Instant},
};

use regex::Regex;

use super::{
    COPY_CHUNK, Copied, Interrupt, SoakError, copy_bounded, copy_program, digest_program,
    halt_reason, publish, saving_must_stop,
};
#[cfg(unix)]
use super::{FileIdentity, directory_for_the_copy};

// ---------------------------------------------------------------------------------------------
// The source tripwire.
//
// A soak runs on a real tree, so its code must not be able to delete or change one, and must not
// write a key to the program but through the choke point. The type of the root keeps a `SoakRoot`
// out of every API that takes a `FixtureRoot`; these tests keep the soak's code away from the APIs
// that take a path, by reading it.
//
// The scan reads every file below `src/soak`, nested ones too, and the two files of the xtask that
// hold the command (`xtask_sources`), as `code_of` leaves them. It looks for the names in
// `FORBIDDEN`, for the forms of a name that a plain search misses (`FORBIDDEN_FORMS`), for every
// way to write Backspace and Ctrl+H outside the allowlist (`BYTE_SPELLINGS`), and for the paths
// it reaches for. What decides what the soak starts, what it writes, and what it may write around
// the choke point is pinned: `PINS` names what only some lines may hold, and `BLOCKS` fixes the
// lines that decide the program of a session and the arguments of a scan. Both are looked for in
// the code without its comments and literals and with its spaces gone (`Scanned`), so that
// `fs :: /* */ rename` is `fs::rename`. `INJECTIONS` has a case for each other rule: code put into
// `mod.rs` that the scan must name.

/// What the soak's code must never name: each is a way to delete, to change a tree, to write to
/// the program around the choke point, or to reach what takes a path. (A name that some lines may
/// hold, such as `barrier` or `Command::new(`, is in `PINS`.)
const FORBIDDEN: &[(&str, &str)] = &[
    ("FixtureRoot", "the type that every deleting step takes"),
    ("mutate::", "the live mutators behind `fs_mutate`"),
    ("DeletionRequest", "the deletion protocol's request"),
    ("confirm_deletion", "the deletion protocol"),
    (
        "select_entry",
        "types a filter's name and erases it with Backspace",
    ),
    ("clear_filter", "erases a filter with Backspace"),
    ("send_barrier", "writes the barrier byte"),
    ("send_confirming", "the guarded confirmation"),
    ("note_raw_write", "a scenario's raw write"),
    (
        "session.send(",
        "a write that does not pass the choke point",
    ),
    (
        "resize(",
        "the soak never resizes, and a resize is an input",
    ),
    (
        "remove_tree",
        "`fixture::remove_tree` removes a tree by path, with no marker check",
    ),
    (
        "write_marker",
        "`fixture::write_marker` writes a harness marker by path",
    ),
    ("TreeGuard", "removes a tree when it is dropped"),
    ("remove_dir", "the soak deletes nothing itself"),
    ("remove_file", "the soak deletes nothing itself"),
    ("fs::rename", "the soak moves nothing in the tree"),
    (
        "fs::write(",
        "every file the soak makes is new (`private_file`), and none is overwritten",
    ),
    (
        "File::create(",
        "creating a file truncates one that is there",
    ),
    ("set_len(", "the soak truncates nothing"),
    (
        "std::fs::{",
        "an import that renames what it calls (`use std::fs::{rename}`) hides the call from this \
         scan, so `fs::` stays qualified",
    ),
    (
        "use std::fs::",
        "`fs::` stays qualified, so that this scan sees every call",
    ),
    ("crate::tui", "the interactive driver"),
    ("scenario::Step", "the scenario steps"),
    (
        "fs::copy",
        "a copy writes a file by its path, and replaces the one that is there",
    ),
    ("truncate(", "the soak truncates nothing"),
    (
        "super::super",
        "leaves the soak by a path this scan does not check: `crate::` is the way, and it does",
    ),
    (
        "TempPath::from_path",
        "removes the file at that path when it is dropped",
    ),
    (
        "persist(",
        "renames a temporary file over a path, and replaces what is there",
    ),
    ("chown", "changes who owns a file, by its path"),
    ("rustix", "system calls that delete and write by path"),
    (
        "portable_pty",
        "starts programs on its own: `PtySession::spawn` is the way, and its one literal is pinned",
    ),
    (
        "CommandBuilder",
        "starts programs on its own: `PtySession::spawn` is the way, and its one literal is pinned",
    ),
];

/// The names above in the forms that a plain search for them does not see: each is the name that a
/// sentence gives it, a pattern over the code, and why it is refused.
const FORBIDDEN_FORMS: &[(&str, &str, &str)] = &[
    (
        "::send",
        r"::\s*(?:r#)?send\b",
        "`PtySession::send`, named by its path or taken as a function value, writes to the \
         program and passes no choke point",
    ),
    (
        ".send",
        r"\.\s*(?:r#)?send\b",
        "a method named `send` is `PtySession::send` on any receiver, and writes to the program \
         around the choke point",
    ),
    (
        "send_input",
        r"\bsend_input\b\s*[^\s(]",
        "the choke point is called as `send_input(`, and a test counts those calls: its name in \
         any other use is a call that the count cannot see",
    ),
    (
        "#[path",
        r"#\s*!?\s*\[\s*path\b",
        "puts a module where this scan does not read",
    ),
    (
        "#[cfg_attr(.., path = ..)]",
        r"cfg_attr\s*\([^\]]*\bpath\b",
        "puts a module where this scan does not read",
    ),
    (
        "include!",
        r"\binclude\s*!",
        "brings in code from where this scan does not read",
    ),
    (
        "nix::",
        r"\bnix\s*::",
        "system calls that delete and write by path",
    ),
    (
        "unsafe",
        r"\bunsafe\b",
        "the soak has no unsafe code, and unsafe code could call anything",
    ),
    (
        "extern crate",
        r"\bextern\s+crate\b",
        "an alias of a crate hides what is called, as `use ... as` does",
    ),
    (
        "type X = ..",
        r"\btype\s+[A-Za-z_]\w*\s*(?:<[^=;]*>)?\s*=",
        "a `type` alias hides what is called, as `use ... as` does",
    ),
];

/// Why Backspace is named only where the allowlist is.
const BACKSPACE: &str = "Backspace is the one key that asks excise to delete: only the allowlist \
                         (keys.rs) says which bytes it refuses";
/// Why Ctrl+H is named only where the allowlist is.
const CTRL_H: &str = "Ctrl+H can be read as Backspace: only the allowlist (keys.rs) says which \
                      bytes it refuses";

/// Every way to write the two bytes that ask `excise` to delete, as patterns over the normal form
/// of the code (`normal_form`): Backspace is 127 (`0x7f`) and Ctrl+H is 8 (`0x08`). The number in
/// every base and with a type suffix is here, and so are the escapes, the names, and the two
/// conversions that make a byte out of a number. A bare decimal number is not: it is everywhere.
const BYTE_SPELLINGS: &[(&str, &str, &str)] = &[
    ("0x7f", r"\b0x0*7f(?:[^0-9a-f]|$)", BACKSPACE),
    ("0o177", r"\b0o0*177(?:[^0-7]|$)", BACKSPACE),
    ("0b1111111", r"\b0b0*1111111(?:[^01]|$)", BACKSPACE),
    ("127u8", r"\b0*127[ui]8\b", BACKSPACE),
    ("\\x7f", r"\\x7f", BACKSPACE),
    ("\\u{7f}", r"\\u\{0*7f\}", BACKSPACE),
    ("backspace", r"backspace", BACKSPACE),
    ("u8::from(127)", r"\bu8 ?:: ?from ?\( ?0*127 ?\)", BACKSPACE),
    (
        "char::from(127)",
        r"\bchar ?:: ?from ?\( ?0*127 ?\)",
        BACKSPACE,
    ),
    ("0x08", r"\b0x0*8(?:[^0-9a-f]|$)", CTRL_H),
    ("0o10", r"\b0o0*10(?:[^0-7]|$)", CTRL_H),
    ("0b1000", r"\b0b0*1000(?:[^01]|$)", CTRL_H),
    ("8u8", r"\b0*8[ui]8\b", CTRL_H),
    ("\\x08", r"\\x08", CTRL_H),
    ("\\u{8}", r"\\u\{0*8\}", CTRL_H),
    ("u8::from(8)", r"\bu8 ?:: ?from ?\( ?0*8 ?\)", CTRL_H),
    ("char::from(8)", r"\bchar ?:: ?from ?\( ?0*8 ?\)", CTRL_H),
];

/// The paths below `crate::` that the soak's code may name, as prefixes: what it measures with,
/// what it drives a terminal with, what it reports in, and nothing that takes a tree's path to
/// delete or write it (`fixture`: only the one function that opens a regular file for reading, and
/// waits for nothing), nothing that drives a session for a person (`tui`), and none of the
/// runners' steps (`scenario` but its `Profile`).
const ALLOWED_CRATE_PATHS: &[&str] = &[
    "events::",
    "fixture::sys::open_regular_file",
    "headless::document::",
    "headless::process::",
    "metrics::",
    "pty::",
    "report::",
    "run_support::",
    "runner::RunError",
    "runner::resolve_binary",
    "runner::live::",
    "safety::FileIdentity",
    "safety::NotTheFile",
    "safety::Scratch",
    "safety::UntrustedDirectory",
    "safety::available_bytes",
    "safety::check_private_directory",
    "safety::isolated_env",
    "scenario::Profile",
    "soak::",
];

/// The line of `session.rs` that overrides `Drive::barrier`, the one provided protocol of the
/// shared driver that writes to the program around the choke point (the barrier byte), to refuse
/// it.
const BARRIER_LINE: &str =
    "fn barrier(&mut self, _deadline: Instant) -> Result<Waited<()>, RunError> {";
/// The line of `headless.rs` that starts the binary under test.
const COMMAND_LINE: &str = "let mut command = Command::new(context.binary);";
/// The import of `headless.rs` that brings in the process builder.
const COMMAND_IMPORT: &str =
    "use std::{ffi::OsString, fmt::Write as _, process::Command, time::Instant};";
/// The line of `xtask/soak_build.rs` that starts cargo.
const CARGO_LINE: &str = "let mut command = Command::new(cargo);";
/// The line of `xtask/soak_build.rs` that starts `git rev-parse HEAD`.
const GIT_LINE: &str = "let mut command = Command::new(\"git\");";
/// The line of `xtask/soak_build.rs` that starts the process that leads the build's process group:
/// `cat`, which only waits on its standard input.
const CAT_LINE: &str = "let mut command = Command::new(\"cat\");";
/// What the block of `session.rs` that makes the `SpawnSpec` of a terminal session is called.
const SPAWN_LITERAL: &str = "the literal of the terminal session's `SpawnSpec`";

/// A group of names that the soak's code may hold only on pinned lines.
struct Pin {
    /// Why the names are refused anywhere else.
    why: &'static str,
    /// Whether the files of the xtask are held to it too.
    xtask: bool,
    /// Each name by the way a sentence gives it, a pattern over the tight form of the code
    /// (`Scanned`), and a statement that holds it, which a test of the scan puts into code.
    names: &'static [(&'static str, &'static str, &'static str)],
    /// The lines that may hold the names, each by the file, the line (trimmed, without its
    /// comments, literals as written), and how often the file holds it. A file that holds one a
    /// different number of times is refused: a second copy is, and so is a line that has gone.
    lines: &'static [(&'static str, &'static str, usize)],
}

/// Lines that must follow one another, exactly and once, in a file: where the soak decides what it
/// starts and with what.
struct Block {
    file: &'static str,
    what: &'static str,
    lines: &'static [&'static str],
}

/// The names that only pinned lines may hold. Every name has a statement, and a test puts it into
/// `mod.rs` (and into the xtask's files, where they are held to it) and expects the sentence.
const PINS: &[Pin] = &[
    Pin {
        why: "the one provided protocol that writes to the program around the choke point (the \
              barrier byte): the driver refuses it, and only the line that overrides it names it, \
              in any form",
        xtask: false,
        names: &[("barrier", r"\bbarrier\b", "driver.barrier(deadline);")],
        lines: &[("session.rs", BARRIER_LINE, 1)],
    },
    Pin {
        why: "starts a process: the soak starts the binary under test, on one line of \
              `headless.rs`, and the build of the command starts cargo, `git`, and `cat`, each on \
              one line of `xtask/soak_build.rs`, and nothing else is started this way",
        xtask: true,
        names: &[(
            "Command::new(",
            r"\bCommand::new\b",
            "Command::new(\"rm\");",
        )],
        lines: &[
            ("headless.rs", COMMAND_LINE, 1),
            ("xtask/soak_build.rs", CARGO_LINE, 1),
            ("xtask/soak_build.rs", GIT_LINE, 1),
            ("xtask/soak_build.rs", CAT_LINE, 1),
        ],
    },
    Pin {
        why: "is the process builder: it is imported and used where the one process of the soak is \
              started, so that a form of `Command::new` that a plain search misses cannot start \
              another",
        xtask: false,
        names: &[("Command", r"\bCommand\b", "use std::process::Command;")],
        lines: &[
            ("headless.rs", COMMAND_IMPORT, 1),
            ("headless.rs", COMMAND_LINE, 1),
        ],
    },
    Pin {
        why: "builds the process of the headless scan: its arguments, environment, directory, and \
              streams are decided on the pinned lines of `headless.rs` and nowhere else",
        xtask: false,
        names: &[
            (".arg(", r"\.arg\(", "command.arg(x);"),
            (".args(", r"\.args\(", "command.args(x);"),
            (".env(", r"\.env\(", "command.env(x, y);"),
            (".envs(", r"\.envs\(", "command.envs(x);"),
            (".env_remove(", r"\.env_remove\(", "command.env_remove(x);"),
            (".env_clear(", r"\.env_clear\(", "command.env_clear();"),
            (
                ".current_dir(",
                r"\.current_dir\(",
                "command.current_dir(x);",
            ),
            (".stdin(", r"\.stdin\(", "command.stdin(x);"),
            (".stdout(", r"\.stdout\(", "command.stdout(x);"),
            (".stderr(", r"\.stderr\(", "command.stderr(x);"),
            (
                ".process_group(",
                r"\.process_group\(",
                "command.process_group(0);",
            ),
            (".pre_exec(", r"\.pre_exec\(", "command.pre_exec(x);"),
            (".uid(", r"\.uid\(", "command.uid(0);"),
            (".gid(", r"\.gid\(", "command.gid(0);"),
        ],
        lines: &[
            ("headless.rs", ".args(&arguments)", 1),
            ("headless.rs", ".env_clear()", 1),
            (
                "headless.rs",
                ".envs(isolated_env(scratch, profile, false, None))",
                1,
            ),
            ("headless.rs", ".current_dir(scratch.cwd());", 1),
        ],
    },
    Pin {
        why: "decides what runs in a terminal: `spawn_spec` of `session.rs` makes the one \
              `SpawnSpec`, the one `PtySession::spawn` call starts it, and nothing else makes or \
              changes one",
        xtask: false,
        names: &[
            (
                "SpawnSpec {",
                r"\bSpawnSpec\{",
                "let spec = SpawnSpec { program };",
            ),
            (
                "SpawnSpec::",
                r"\bSpawnSpec::",
                "let spec = SpawnSpec::default();",
            ),
            (
                "PtySession::spawn(",
                r"\bPtySession::spawn\b",
                "PtySession::spawn(&spec);",
            ),
            (".program = ..", r"\.program=[^=]", "spec.program = other;"),
            (".args = ..", r"\.args=[^=]", "spec.args = other;"),
            (".env = ..", r"\.env=[^=]", "spec.env = other;"),
            (".cwd = ..", r"\.cwd=[^=]", "spec.cwd = other;"),
        ],
        lines: &[
            ("session.rs", ") -> SpawnSpec {", 1),
            ("session.rs", "SpawnSpec {", 1),
            (
                "session.rs",
                "let session = PtySession::spawn(&spawn_spec(",
                1,
            ),
        ],
    },
    Pin {
        why: "changes a mode: the soak makes its private copy of the program under test in a \
              directory of its own, asks for the mode 0700 for the directory as it is made (so \
              that it is private from the moment it exists), and gives the directory (by its path) \
              and the copy (on the open file) the mode 0700 exactly, which the umask filters when \
              they are made, on the three pinned lines of `mod.rs`; no other mode is changed",
        xtask: true,
        names: &[
            (
                "set_permissions",
                r"\bset_permissions\b",
                "fs::set_permissions(path, permissions);",
            ),
            (
                ".permissions(",
                r"\.permissions\(",
                "builder.permissions(permissions);",
            ),
        ],
        lines: &[
            ("mod.rs", "fs::set_permissions(path, owner_only())", 1),
            ("mod.rs", "file.set_permissions(owner_only())", 1),
            ("mod.rs", "builder.permissions(owner_only());", 1),
        ],
    },
    Pin {
        why: "makes, opens, or links a path: every file and directory the soak makes is new and is \
              made on a pinned line, and nothing else is opened for writing, copied, linked, or \
              made temporary",
        xtask: true,
        names: &[
            ("OpenOptions", r"\bOpenOptions\b", "OpenOptions::new();"),
            ("::options(", r"::options\b", "File::options();"),
            ("create_new", r"\bcreate_new\b", "File::create_new(path);"),
            ("DirBuilder", r"\bDirBuilder\b", "DirBuilder::new();"),
            ("create_dir", r"\bcreate_dir\b", "fs::create_dir(path);"),
            (
                "create_dir_all",
                r"\bcreate_dir_all\b",
                "fs::create_dir_all(path);",
            ),
            ("symlink", r"\bsymlink\b", "symlink(path, link);"),
            ("hard_link", r"\bhard_link\b", "fs::hard_link(path, link);"),
            (
                "set_modified",
                r"\bset_modified\b",
                "file.set_modified(time);",
            ),
            ("set_times", r"\bset_times\b", "file.set_times(times);"),
            ("CastWriter", r"\bCastWriter\b", "CastWriter::create(path);"),
            ("::copy", r"::copy\b", "io::copy(&mut from, &mut to);"),
            (
                "io::{copy}",
                r"\bio::\{[^}]*\bcopy\b",
                "use std::io::{self, copy};",
            ),
            (
                "os::unix::fs",
                r"\bos::unix::fs\b",
                "use std::os::unix::fs::PermissionsExt as _;",
            ),
            (
                "os::windows::fs",
                r"\bos::windows::fs\b",
                "use std::os::windows::fs::symlink_file;",
            ),
            ("tempfile", r"\btempfile\b", "tempfile::tempfile();"),
            (
                "tempfile_in",
                r"\btempfile_in\b",
                "builder.tempfile_in(dir);",
            ),
            ("tempdir", r"\btempdir\b", "builder.tempdir();"),
            ("tempdir_in", r"\btempdir_in\b", "builder.tempdir_in(dir);"),
            (
                "NamedTempFile",
                r"\bNamedTempFile\b",
                "NamedTempFile::new();",
            ),
            ("TempDir", r"\bTempDir\b", "TempDir::new();"),
            ("TempPath", r"\bTempPath\b", "TempPath::keep(path);"),
            (
                "persist_noclobber",
                r"\bpersist_noclobber\b",
                "file.persist_noclobber(path);",
            ),
            (
                "SpooledTempFile",
                r"\bSpooledTempFile\b",
                "SpooledTempFile::new(1);",
            ),
            (".prefix(", r"\.prefix\(", "builder.prefix(\"x\");"),
            (".suffix(", r"\.suffix\(", "builder.suffix(\".x\");"),
        ],
        lines: &[
            (
                "mod.rs",
                "recordings: Vec<(String, tempfile::TempPath)>,",
                1,
            ),
            (
                "mod.rs",
                "fs::create_dir_all(parent).map_err(io_error(\"cannot create the output directory\"))?;",
                1,
            ),
            ("mod.rs", "match fs::create_dir(out_root) {", 1),
            ("mod.rs", "use std::os::unix::fs::DirBuilderExt as _;", 1),
            (
                "mod.rs",
                "fs::DirBuilder::new().mode(0o700).create(path)",
                1,
            ),
            ("mod.rs", "fs::DirBuilder::new().create(path)", 1),
            ("mod.rs", "let mut options = fs::OpenOptions::new();", 1),
            ("mod.rs", "options.write(true).create_new(true);", 1),
            ("mod.rs", "use std::os::unix::fs::OpenOptionsExt as _;", 1),
            ("mod.rs", "use std::os::unix::fs::PermissionsExt as _;", 1),
            ("mod.rs", "_directory: tempfile::TempDir,", 1),
            (
                "mod.rs",
                "fn directory_for_the_copy(work_dir: &Path) -> io::Result<tempfile::TempDir> {",
                1,
            ),
            ("mod.rs", "let mut builder = tempfile::Builder::new();", 1),
            ("mod.rs", "builder.prefix(\"xh-soak-bin-\");", 1),
            ("mod.rs", "builder.tempdir_in(work_dir)", 1),
            (
                "mod.rs",
                "let mut file = tempfile::NamedTempFile::new_in(dir)?;",
                1,
            ),
            ("mod.rs", "file.persist_noclobber(dir.join(name))?;", 1),
            (
                "session.rs",
                "pub recording: Option<tempfile::TempPath>,",
                1,
            ),
            ("session.rs", "let file = tempfile::Builder::new()", 1),
            ("session.rs", ".prefix(\"xh-cast-\")", 1),
            ("session.rs", ".suffix(\".cast\")", 1),
            ("session.rs", ".tempfile_in(context.work_dir)", 1),
            ("session.rs", "recording: Option<tempfile::TempPath>,", 1),
            (
                "xtask/soak_build.rs",
                "use std::os::unix::fs::OpenOptionsExt as _;",
                1,
            ),
            (
                "xtask/soak_build.rs",
                "let mut options = fs::OpenOptions::new();",
                1,
            ),
            (
                "xtask/soak_build.rs",
                "options.write(true).create_new(true);",
                1,
            ),
            (
                "xtask/soak_build.rs",
                "use std::os::unix::fs::PermissionsExt as _;",
                1,
            ),
        ],
    },
];

/// The lines that decide the program of a terminal session and the process of a headless scan,
/// pinned line for line.
const BLOCKS: &[Block] = &[
    Block {
        file: "session.rs",
        what: SPAWN_LITERAL,
        lines: &[
            "SpawnSpec {",
            "program: context.binary.to_path_buf(),",
            "args: vec![context.root.path().as_os_str().to_owned()],",
            "env: isolated_env(scratch, profile, true, None),",
            "cwd: scratch.cwd(),",
            "cols: TERMINAL.0,",
            "rows: TERMINAL.1,",
            "drain_bytes_per_sec: None,",
            "recording: recording.map(Path::to_path_buf),",
            "title: Some(format!(\"soak ({profile})\")),",
            "}",
        ],
    },
    Block {
        file: "headless.rs",
        what: "the arguments of the headless scan",
        lines: &[
            "let arguments: Vec<OsString> = vec![",
            "\"--format\".into(),",
            "\"json\".into(),",
            "\"--output\".into(),",
            "scratch.report().into_os_string(),",
            "context.root.path().as_os_str().to_owned(),",
            "];",
        ],
    },
    Block {
        file: "headless.rs",
        what: "the process builder of the headless scan",
        lines: &[
            COMMAND_LINE,
            "command",
            ".args(&arguments)",
            ".env_clear()",
            ".envs(isolated_env(scratch, profile, false, None))",
            ".current_dir(scratch.cwd());",
        ],
    },
];

/// Lines of the xtask's files that the tables above refuse and that are fine, each by its file,
/// the line (trimmed, without its comments), and why. They are cut out of the code before it is
/// scanned by those tables, and each must be in its file exactly once.
const XTASK_LINES: &[(&str, &str, &str)] = &[
    (
        "xtask/soak.rs",
        r"Esc, q, and the y that answers the quit prompt: never Backspace, so it cannot\n\",
        "the prompt that the person confirms says which keys the soak sends, and that it never \
         sends Backspace",
    ),
    (
        "xtask/soak_build.rs",
        "if self.lines.send(kept).is_err() {",
        "hands one line of cargo's output over to the supervisor on a channel: nothing is written \
         to a program",
    ),
    (
        "xtask/soak_build.rs",
        "copy.set_permissions(fs::Permissions::from_mode(0o700))?;",
        "sets the mode of the private copy of the binary, which the command has just made in its \
         own scratch area and has open, to exactly 0700 (the mode that a file is made with goes \
         through the umask): it is a mode of that open file, and of no path",
    ),
];

/// A pattern that the scan looks for, the name that a sentence gives it, and why it is refused.
struct Pattern {
    label: &'static str,
    regex: Regex,
    why: &'static str,
}

/// The patterns of `table`, compiled.
fn compile(table: &[(&'static str, &'static str, &'static str)]) -> Vec<Pattern> {
    table
        .iter()
        .map(|&(label, pattern, why)| Pattern {
            label,
            regex: Regex::new(pattern).expect("a pattern"),
            why,
        })
        .collect()
}

/// `FORBIDDEN_FORMS`, compiled once.
static FORMS: LazyLock<Vec<Pattern>> = LazyLock::new(|| compile(FORBIDDEN_FORMS));
/// `BYTE_SPELLINGS`, compiled once.
static SPELLINGS: LazyLock<Vec<Pattern>> = LazyLock::new(|| compile(BYTE_SPELLINGS));
/// The patterns of the names of `PINS`, compiled once, in the order of the names.
static PIN_REGEXES: LazyLock<Vec<Vec<Regex>>> = LazyLock::new(|| {
    PINS.iter()
        .map(|pin| {
            pin.names
                .iter()
                .map(|(_, pattern, _)| Regex::new(pattern).expect("a pattern"))
                .collect()
        })
        .collect()
});

/// Whether each line of `text` begins inside a literal that the line before it did not end: a
/// string, raw or not, or a block comment. What such a line holds is what the literal says,
/// whatever it looks like: it is never a `#[cfg(test)]`, a `}`, or a line comment.
fn lines_inside_literals(text: &str) -> Vec<bool> {
    let chars: Vec<char> = text.chars().collect();
    let mut begins_inside = vec![false];
    let mut at = 0;
    while at < chars.len() {
        let end = token_end(&chars, at);
        if end == at + 1 {
            if chars[at] == '\n' {
                begins_inside.push(false);
            }
        } else {
            let breaks = chars[at..end].iter().filter(|c| **c == '\n').count();
            begins_inside.extend(std::iter::repeat_n(true, breaks));
        }
        at = end;
    }
    begins_inside
}

/// The index after the token that starts at `chars[at]`: a comment, a literal, or one character.
fn token_end(chars: &[char], at: usize) -> usize {
    let next = chars.get(at + 1).copied();
    match chars[at] {
        '/' if next == Some('/') => line_end(chars, at),
        '/' if next == Some('*') => block_comment_end(chars, at),
        '"' => string_end(chars, at + 1),
        'r' if starts_a_word(chars, at) => raw_string_end(chars, at).unwrap_or(at + 1),
        '\'' => char_literal_end(chars, at).unwrap_or(at + 1),
        _ => at + 1,
    }
}

/// The index of the line break that ends the line comment at `chars[at]`, or the end of the text.
fn line_end(chars: &[char], at: usize) -> usize {
    chars[at..]
        .iter()
        .position(|character| *character == '\n')
        .map_or(chars.len(), |offset| at + offset)
}

/// Whether `chars[at]` starts a word: no name runs into it, bar the `b` or `c` that prefixes a raw
/// string (`br"..."`).
fn starts_a_word(chars: &[char], at: usize) -> bool {
    let before = |back: usize| at.checked_sub(back).map(|index| chars[index]);
    let in_a_name = |character: Option<char>| {
        character.is_some_and(|character| character.is_alphanumeric() || character == '_')
    };
    match before(1) {
        Some('b' | 'c') => !in_a_name(before(2)),
        other => !in_a_name(other),
    }
}

/// The end of the block comment that opens at `chars[at]`. Block comments nest.
fn block_comment_end(chars: &[char], at: usize) -> usize {
    let mut depth = 0_u32;
    let mut cursor = at;
    while cursor < chars.len() {
        match (chars[cursor], chars.get(cursor + 1).copied()) {
            ('/', Some('*')) => {
                depth += 1;
                cursor += 2;
            }
            ('*', Some('/')) => {
                depth -= 1;
                cursor += 2;
                if depth == 0 {
                    return cursor;
                }
            }
            _ => cursor += 1,
        }
    }
    chars.len()
}

/// The end of the string whose contents begin at `chars[from]`.
fn string_end(chars: &[char], from: usize) -> usize {
    let mut cursor = from;
    while cursor < chars.len() {
        match chars[cursor] {
            '\\' => cursor += 2,
            '"' => return cursor + 1,
            _ => cursor += 1,
        }
    }
    chars.len()
}

/// The end of the raw string whose `r` is at `chars[at]`, or `None` when that `r` does not start
/// one (`r#type`).
fn raw_string_end(chars: &[char], at: usize) -> Option<usize> {
    let hashes = chars[at + 1..]
        .iter()
        .take_while(|character| **character == '#')
        .count();
    if chars.get(at + 1 + hashes) != Some(&'"') {
        return None;
    }
    let mut cursor = at + 2 + hashes;
    while cursor < chars.len() {
        if closes_raw_string(chars, cursor, hashes) {
            return Some(cursor + 1 + hashes);
        }
        cursor += 1;
    }
    Some(chars.len())
}

/// Whether the quote at `chars[quote]` is followed by `hashes` number signs: the end of a raw
/// string.
fn closes_raw_string(chars: &[char], quote: usize, hashes: usize) -> bool {
    chars[quote] == '"'
        && chars
            .get(quote + 1..quote + 1 + hashes)
            .is_some_and(|after| after.iter().all(|character| *character == '#'))
}

/// The end of the character literal that opens at `chars[at]`, or `None` when the quote opens a
/// lifetime or a label.
fn char_literal_end(chars: &[char], at: usize) -> Option<usize> {
    match (chars.get(at + 1).copied(), chars.get(at + 2).copied()) {
        (Some('\\'), _) => {
            let closing = chars
                .get(at + 3..)?
                .iter()
                .position(|character| *character == '\'')?;
            Some(at + 3 + closing + 1)
        }
        (Some(_), Some('\'')) => Some(at + 3),
        _ => None,
    }
}

/// `text` as code: without its comments and doc comments and without its test module, a
/// `#[cfg(test)]` module declared out of line (`mod tests;`) or in line (`mod tests {` in the first
/// column, to the `}` in the first column that is the last line of the file). Only a test module
/// is left out: everything else in the file is code, wherever it comes. Text that this cannot
/// read as that fails (a test module of another name, one written on one line, one that code
/// follows), so that what is left out is never more than a test module. A line that begins inside
/// a literal is what the literal says, and is code: neither a comment nor the start of a module.
fn code_of(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let in_literal = lines_inside_literals(text);
    let mut code = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if !in_literal[index] && line.trim() == "#[cfg(test)]" {
            index = end_of_test_module(&lines, &in_literal, index);
        } else {
            let comment = !in_literal[index] && line.trim_start().starts_with("//");
            if !comment {
                code.push(line);
            }
            index += 1;
        }
    }
    code.join("\n")
}

/// The index of the line after the test module that the `#[cfg(test)]` on `lines[attribute]`
/// opens. `in_literal` is `lines_inside_literals` of the same text.
fn end_of_test_module(lines: &[&str], in_literal: &[bool], attribute: usize) -> usize {
    let declaration = (attribute + 1..lines.len())
        .find(|&index| !lines[index].trim_start().starts_with("#["))
        .expect("an item follows `#[cfg(test)]`");
    let header = lines[declaration].trim_end();
    let Some(module) = header.trim_start().strip_prefix("mod ") else {
        panic!("`#[cfg(test)]` on `{header}`: only test modules are left out of the scan");
    };
    let name = module.split([' ', ';', '{']).next().unwrap_or_default();
    assert!(
        name == "tests",
        "`#[cfg(test)]` on `{header}`: the test module of a soak file is named `tests`, so that \
         nothing else is left out of the scan"
    );
    if header.trim_start() == "mod tests;" {
        return declaration + 1;
    }
    assert!(
        header == "mod tests {",
        "`#[cfg(test)]` on `{header}`: a test module is declared `mod tests;`, or opened by \
         `mod tests {{` alone on a line in the first column, so that this scan can see where it \
         ends"
    );
    let close = (declaration + 1..lines.len())
        .find(|&index| lines[index] == "}" && !in_literal[index])
        .expect("the test module is never closed by a `}` in the first column");
    if let Some(follower) = lines[close + 1..]
        .iter()
        .find(|line| !line.trim().is_empty())
    {
        panic!(
            "`{follower}` follows the test module, which must be the last item of the file, so \
             that what is left out of the scan is the module and nothing else"
        );
    }
    lines.len()
}

/// The `.rs` files below `directory`, nested ones included, as the path of each below `directory`
/// (with `/`) and its text, in the order of the paths. A link is not followed: it fails the scan,
/// since what it leads to can be anywhere.
fn sources_below(directory: &Path) -> Vec<(String, String)> {
    fn walk(directory: &Path, prefix: &str, into: &mut Vec<(String, String)>) {
        for entry in fs::read_dir(directory).expect("a source directory") {
            let entry = entry.expect("an entry");
            let path = entry.path();
            let kind = entry.file_type().expect("a file type");
            assert!(
                !kind.is_symlink(),
                "{} is a link, and a link can lead out of the directory this scan reads",
                path.display()
            );
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if kind.is_dir() {
                walk(&path, &relative, into);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                into.push((relative, fs::read_to_string(&path).expect("a source file")));
            }
        }
    }
    let mut sources = Vec::new();
    walk(directory, "", &mut sources);
    sources.sort();
    sources
}

/// The soak's own source files, by their paths below `src/soak`, as they are on disk: every one,
/// nested ones included, but this file.
fn soak_sources() -> Vec<(String, String)> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/soak");
    let mut sources = sources_below(&directory);
    sources.retain(|(name, _)| name != "tests.rs");
    sources
}

/// The files of the xtask that hold the command, the other half of the soak: it asks the person,
/// builds what the library soaks, and runs the soak.
const XTASK_FILES: [&str; 2] = ["soak.rs", "soak_build.rs"];

/// The files of `XTASK_FILES` as they are on disk, by the name this scan gives them
/// (`xtask/soak.rs`).
fn xtask_sources() -> Vec<(String, String)> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../xtask/src");
    XTASK_FILES
        .iter()
        .map(|file| {
            let text = fs::read_to_string(directory.join(file)).expect("an xtask source file");
            (format!("xtask/{file}"), text)
        })
        .collect()
}

/// Every file that the scan reads: the soak's, and the xtask's that hold the command.
fn all_sources() -> Vec<(String, String)> {
    let mut sources = soak_sources();
    sources.extend(xtask_sources());
    sources
}

/// Whether the file that this scan calls `name` is one of the xtask's.
fn is_xtask(name: &str) -> bool {
    name.starts_with("xtask/")
}

/// The `use` trees of `code` (`a::{b, c}` for `use a::{b, c};`), and the code without them.
fn split_uses(code: &str) -> (Vec<String>, String) {
    let mut trees = Vec::new();
    let mut rest = Vec::new();
    let mut open: Option<String> = None;
    for line in code.lines() {
        let trimmed = line.trim();
        if let Some(statement) = open.as_mut() {
            statement.push(' ');
            statement.push_str(trimmed);
        } else if let Some(tree) = ["use ", "pub use ", "pub(super) use ", "pub(crate) use "]
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix))
        {
            open = Some(tree.to_owned());
        } else {
            rest.push(line);
            continue;
        }
        if open
            .as_ref()
            .is_some_and(|statement| statement.ends_with(';'))
        {
            let statement = open.take().expect("an open statement");
            trees.push(statement.trim_end_matches(';').to_owned());
        }
    }
    (trees, rest.join("\n"))
}

/// `tail` below `prefix`; `self` is the prefix itself.
fn join(prefix: &str, tail: &str) -> String {
    if prefix.is_empty() {
        tail.to_owned()
    } else if tail.is_empty() || tail == "self" {
        prefix.to_owned()
    } else {
        format!("{prefix}::{tail}")
    }
}

/// `text` cut at the commas that are not inside braces.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0_u32;
    let mut start = 0;
    for (index, character) in text.char_indices() {
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
        .into_iter()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect()
}

/// Every path a `use` tree names: `a::{b, c::{d, e}}` is `a::b`, `a::c::d`, and `a::c::e`.
fn flatten(tree: &str, prefix: &str, into: &mut Vec<String>) {
    let tree = tree.trim();
    if let Some(open) = tree.find('{') {
        let head = tree[..open].trim().trim_end_matches("::");
        let inner = &tree[open + 1..tree.rfind('}').expect("a closing brace")];
        let prefix = join(prefix, head);
        for part in split_top_level(inner) {
            flatten(part, &prefix, into);
        }
    } else if !tree.is_empty() {
        into.push(join(prefix, tree));
    }
}

/// The paths that start with `crate::` in `code`, outside `use` statements, without the `crate::`.
fn qualified_crate_paths(code: &str) -> Vec<String> {
    code.split("crate::")
        .skip(1)
        .map(|after| {
            after
                .chars()
                .take_while(|character| {
                    character.is_alphanumeric() || matches!(character, '_' | ':')
                })
                .collect::<String>()
                .trim_end_matches(':')
                .to_owned()
        })
        .collect()
}

fn crate_path_is_allowed(path: &str) -> bool {
    ALLOWED_CRATE_PATHS
        .iter()
        .any(|allowed| path.starts_with(allowed) || path == allowed.trim_end_matches("::"))
}

/// `code` in the form in which the spellings of a number are alike: in lower case, without the
/// underscores that separate digits, and with one space for any run of white space.
fn normal_form(code: &str) -> String {
    let mut normal = String::with_capacity(code.len());
    let mut spaced = false;
    for character in code.chars().filter(|character| *character != '_') {
        if character.is_whitespace() {
            spaced = true;
        } else {
            if spaced && !normal.is_empty() {
                normal.push(' ');
            }
            spaced = false;
            normal.extend(character.to_lowercase());
        }
    }
    normal
}

/// The spellings of Backspace and Ctrl+H in `code`, each by its name and why it is refused.
fn byte_spellings(code: &str) -> Vec<(&'static str, &'static str)> {
    let normal = normal_form(code);
    SPELLINGS
        .iter()
        .filter(|spelling| spelling.regex.is_match(&normal))
        .map(|spelling| (spelling.label, spelling.why))
        .collect()
}

/// Whether `character` can be in a name.
fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// The code of one file in the form that the pinned names are looked for in.
#[derive(Default)]
struct Scanned {
    /// The lines that hold code, trimmed, without their comments and with their literals as
    /// written. A line that holds nothing else is not here.
    lines: Vec<String>,
    /// All the code in one piece: without any comment (one inside a line included), with each
    /// literal cut down to its quotes, and with a space only where two words would run together
    /// without it. `fs :: /* a */ rename (` is `fs::rename(` in it.
    tight: String,
    /// For each byte of `tight`, the index in `lines` of the line it is from.
    line_of: Vec<usize>,
}

/// Reads code into a `Scanned`, one token at a time.
#[derive(Default)]
struct Scanner {
    scanned: Scanned,
    /// The line being read, as it is written.
    line: String,
    /// Whether white space or a comment has come since the last character of the tight form.
    gap: bool,
    /// The last character of the tight form.
    last: Option<char>,
}

impl Scanner {
    /// Ends the line being read: it is kept when it holds anything.
    fn finish_line(&mut self) {
        let line = self.line.trim();
        if !line.is_empty() {
            self.scanned.lines.push(line.to_owned());
        }
        self.line.clear();
    }

    /// Adds `character` to the tight form, from the line being read.
    fn put(&mut self, character: char) {
        self.scanned.tight.push(character);
        let line = self.scanned.lines.len();
        self.scanned
            .line_of
            .extend(std::iter::repeat_n(line, character.len_utf8()));
        self.last = Some(character);
    }

    /// Adds a character of code to the tight form, with a space before it when both it and the
    /// character before it are in words and something stood between them.
    fn tighten(&mut self, character: char) {
        if self.gap && is_word_character(character) && self.last.is_some_and(is_word_character) {
            self.put(' ');
        }
        self.put(character);
        self.gap = false;
    }
}

/// `code` read into the forms that the pins are looked for in.
fn scan(code: &str) -> Scanned {
    let chars: Vec<char> = code.chars().collect();
    let mut scanner = Scanner::default();
    let mut at = 0;
    while at < chars.len() {
        let end = token_end(&chars, at);
        let token = &chars[at..end];
        match token {
            ['\n'] => {
                scanner.finish_line();
                scanner.gap = true;
            }
            [character] if character.is_whitespace() => {
                scanner.line.push(*character);
                scanner.gap = true;
            }
            [character] => {
                scanner.line.push(*character);
                scanner.tighten(*character);
            }
            // A comment: none of it is code, but a line break in it still ends a line.
            ['/', ..] => {
                scanner.gap = true;
                for _ in token.iter().filter(|character| **character == '\n') {
                    scanner.finish_line();
                }
            }
            // A literal: it stays as written in the line, and its quotes are all that is left of
            // it in the tight form.
            [first, ..] => {
                let quote = if *first == '\'' { '\'' } else { '"' };
                scanner.tighten(quote);
                scanner.tighten(quote);
                for character in token {
                    if *character == '\n' {
                        scanner.finish_line();
                    } else {
                        scanner.line.push(*character);
                    }
                }
            }
            [] => {}
        }
        at = end;
    }
    scanner.finish_line();
    scanner.scanned
}

/// What the pinned names are named by in the file `name`, whose code is `scanned`: a name on a
/// line that does not pin it, and a pinned line that the file holds a different number of times
/// than the scan pins it.
fn pin_violations(name: &str, scanned: &Scanned) -> Vec<String> {
    let mut found = Vec::new();
    for (pin, regexes) in PINS.iter().zip(&*PIN_REGEXES) {
        if is_xtask(name) && !pin.xtask {
            continue;
        }
        let pinned: Vec<(&str, usize)> = pin
            .lines
            .iter()
            .filter(|(file, _, _)| *file == name)
            .map(|&(_, line, count)| (line, count))
            .collect();
        for ((label, _, _), regex) in pin.names.iter().zip(regexes) {
            let stray = regex.find_iter(&scanned.tight).any(|hit| {
                let line = scanned.lines[scanned.line_of[hit.start()]].as_str();
                !pinned.iter().any(|(pinned_line, _)| *pinned_line == line)
            });
            if stray {
                found.push(format!("{name} names `{label}` ({})", pin.why));
            }
        }
        for (line, count) in pinned {
            let held = scanned
                .lines
                .iter()
                .filter(|candidate| candidate.as_str() == line)
                .count();
            if held != count {
                found.push(format!(
                    "{name} holds `{line}` {held} times, and the scan pins it to {count} ({})",
                    pin.why
                ));
            }
        }
    }
    found
}

/// What the file `name` holds otherwise than `BLOCKS` pins it: each block of the file must be in
/// `scanned` once, line for line.
fn block_violations(name: &str, scanned: &Scanned) -> Vec<String> {
    BLOCKS
        .iter()
        .filter(|block| block.file == name)
        .filter(|block| {
            let held = scanned
                .lines
                .windows(block.lines.len())
                .filter(|window| {
                    window
                        .iter()
                        .map(String::as_str)
                        .eq(block.lines.iter().copied())
                })
                .count();
            held != 1
        })
        .map(|block| {
            format!(
                "{name} holds {} otherwise than the scan pins it, line for line and once: {}",
                block.what,
                block.lines.join(" / ")
            )
        })
        .collect()
}

/// `code` of the xtask file `name` without the lines that `XTASK_LINES` pins for it. A pinned line
/// that the file does not hold exactly once is a sentence in `found`.
fn without_xtask_lines(name: &str, code: &str, found: &mut Vec<String>) -> String {
    let pinned: Vec<(&str, &str)> = XTASK_LINES
        .iter()
        .filter(|(file, _, _)| *file == name)
        .map(|&(_, line, why)| (line, why))
        .collect();
    for &(line, why) in &pinned {
        let held = code
            .lines()
            .filter(|candidate| candidate.trim() == line)
            .count();
        if held != 1 {
            found.push(format!(
                "{name} holds `{line}` {held} times, and the scan pins it to 1 ({why})"
            ));
        }
    }
    code.lines()
        .filter(|candidate| !pinned.iter().any(|&(line, _)| candidate.trim() == line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `path` is a function of `std::fs`, imported by name (`std::fs::write`, `std::fs::*`): a
/// call of it is then not written with `fs::`, and the names above do not see it. A type
/// (`std::fs::File`) is fine, and so is `std::fs` itself.
fn imports_an_fs_function(path: &str) -> bool {
    path.strip_prefix("std::fs::")
        .is_some_and(|rest| !rest.starts_with(char::is_uppercase))
}

/// What the soak's `use` statements and paths reach for: a path below `crate::` that is not on the
/// list (in the soak: in the xtask `crate::` is another crate), an alias, a function of `std::fs`
/// imported by name, and `super::` in `mod.rs`, which there is the crate root.
fn path_violations(name: &str, code: &str) -> Vec<String> {
    let mut found = Vec::new();
    let (trees, rest) = split_uses(code);
    let mut paths = Vec::new();
    for tree in &trees {
        flatten(tree, "", &mut paths);
    }
    let mut named: Vec<String> = qualified_crate_paths(&rest);
    for path in &paths {
        let (path, alias) = path.split_once(" as ").unwrap_or((path, "_"));
        if alias != "_" {
            found.push(format!(
                "{name} renames `{path}` as `{alias}`: an alias hides what is called"
            ));
        }
        if imports_an_fs_function(path) {
            found.push(format!(
                "{name} imports `{path}`: `fs::` stays qualified, so that this scan sees every \
                 call"
            ));
        }
        if let Some(module) = path.strip_prefix("crate::") {
            named.push(module.to_owned());
        }
    }
    if !is_xtask(name) {
        for module in named {
            if !crate_path_is_allowed(&module) {
                found.push(format!(
                    "{name} uses `crate::{module}`, which is not on the list of what the soak may use"
                ));
            }
        }
        if name == "mod.rs" && code.contains("super::") {
            found.push(format!(
                "{name} names `super::` (in `mod.rs` it is the crate root, outside the soak: write \
                 `crate::`, which this scan checks)"
            ));
        }
    }
    found
}

/// Every way the soak's code reaches for something it must not, one sentence each. The names, the
/// forms, and the spellings of a byte are looked for in the code as it is written, and in its tight
/// form (`Scanned`), so that a comment or a space inside a path does not hide one.
fn violations(sources: &[(String, String)]) -> Vec<String> {
    let mut found = Vec::new();
    for (name, raw) in sources {
        let mut code = code_of(raw);
        if is_xtask(name) {
            code = without_xtask_lines(name, &code, &mut found);
        }
        let scanned = scan(&code);
        for (token, why) in FORBIDDEN {
            if code.contains(token) || scanned.tight.contains(token) {
                found.push(format!("{name} names `{token}` ({why})"));
            }
        }
        for form in &*FORMS {
            if form.regex.is_match(&code) || form.regex.is_match(&scanned.tight) {
                found.push(format!("{name} names `{}` ({})", form.label, form.why));
            }
        }
        if name != "keys.rs" {
            let mut spellings = byte_spellings(&code);
            for spelling in byte_spellings(&scanned.tight) {
                if !spellings.contains(&spelling) {
                    spellings.push(spelling);
                }
            }
            for (label, why) in spellings {
                found.push(format!("{name} names `{label}` ({why})"));
            }
        }
        found.extend(path_violations(name, &code));
        found.extend(pin_violations(name, &scanned));
        found.extend(block_violations(name, &scanned));
    }
    found
}

/// The text of `file` among `sources`, to change.
fn text_of<'a>(sources: &'a mut [(String, String)], file: &str) -> &'a mut String {
    let (_, text) = sources
        .iter_mut()
        .find(|(name, _)| name == file)
        .expect("the file is scanned");
    text
}

/// `sources`, with `replacement` where `anchor` is, once, in `file`.
fn with_replaced(
    sources: &[(String, String)],
    file: &str,
    anchor: &str,
    replacement: &str,
) -> Vec<(String, String)> {
    let mut changed = sources.to_vec();
    let text = text_of(&mut changed, file);
    assert!(text.contains(anchor), "{file} has no `{anchor}`");
    *text = text.replacen(anchor, replacement, 1);
    changed
}

/// `sources`, with `line` before everything else in `file`.
fn with_prepended(sources: &[(String, String)], file: &str, line: &str) -> Vec<(String, String)> {
    let mut changed = sources.to_vec();
    text_of(&mut changed, file).insert_str(0, &format!("{line}\n"));
    changed
}

/// Whether `found` has a sentence about `file` that names `token`.
fn says(found: &[String], file: &str, token: &str) -> bool {
    let wanted = format!("{file} names `{token}`");
    found.iter().any(|sentence| sentence.starts_with(&wanted))
}

#[test]
fn the_soaks_code_names_nothing_that_deletes_mutates_or_writes_around_the_choke_point() {
    let sources = all_sources();
    let names: Vec<&str> = sources.iter().map(|(name, _)| name.as_str()).collect();
    for expected in [
        "facts.rs",
        "headless.rs",
        "interrupt.rs",
        "keys.rs",
        "limits.rs",
        "mod.rs",
        "quirks.rs",
        "root.rs",
        "session.rs",
        "store_watch.rs",
        "xtask/soak.rs",
        "xtask/soak_build.rs",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} was not scanned: {names:?}"
        );
    }

    assert_eq!(violations(&sources), Vec::<String>::new());
}

#[test]
fn the_scan_reads_the_code_that_follows_the_declaration_of_the_test_module_too() {
    // `mod.rs` declares its test module (`mod tests;`) before its imports, `run_soak`, `check`,
    // and the writing of the outputs. A scan that stopped at that line read none of them.
    let sources = soak_sources();
    let code_of_file = |wanted: &str| {
        let (_, raw) = sources
            .iter()
            .find(|(name, _)| name == wanted)
            .expect("a source file");
        code_of(raw)
    };

    let module = code_of_file("mod.rs");
    for needed in [
        "use crate::{",
        "fn run_soak",
        "fn check(",
        "fn write_results",
        "fn copy_bounded",
    ] {
        assert!(
            module.contains(needed),
            "the scanned code of mod.rs lacks `{needed}`"
        );
    }
    assert!(!module.contains("mod tests"), "the test module is left out");
    let session = code_of_file("session.rs");
    assert!(
        session.contains("fn send_input"),
        "the choke point is scanned"
    );
    assert!(
        !session.contains("with_program"),
        "the test module of session.rs is left out"
    );
}

/// Code put right after `mod tests;` in `mod.rs`, the line where an earlier version of the scan
/// stopped reading, and a part of the sentence that the scan must say about it. Every rule of
/// `FORBIDDEN`, `FORBIDDEN_FORMS`, and `BYTE_SPELLINGS` has a case here, and each case fails when
/// its rule is taken out. The names of `PINS` carry a statement of their own, which
/// `every_name_that_a_pin_refuses_is_refused_where_no_line_pins_it` puts into code, and a few
/// cases here too, for the forms that a plain search misses.
const INJECTIONS: &[(&str, &str)] = &[
    (
        "fn injected() { crate::fixture::remove_tree(Path::new(\"x\")).ok(); }",
        "remove_tree",
    ),
    (
        "fn injected() { crate::fixture::tree::TreeGuard::new(\"x\"); }",
        "TreeGuard",
    ),
    ("use crate::fixture::marker::write_marker;", "write_marker"),
    (
        "use crate::fixture::FixtureSpec;",
        "crate::fixture::FixtureSpec",
    ),
    ("use crate::fixture::sys::Dir;", "crate::fixture::sys::Dir"),
    ("use crate::report::QuirkKind as Kind;", "renames"),
    ("use std::fs::{rename};", "std::fs::{"),
    (
        "fn injected(live: &mut Live) { live.send_barrier(); }",
        "send_barrier",
    ),
    (
        "fn injected(d: &mut Driver) { d.barrier(deadline); }",
        "names `barrier`",
    ),
    (
        "fn injected(d: &mut Driver) { d.barrier (deadline).ok(); }",
        "names `barrier`",
    ),
    (
        "fn injected(d: &mut Driver) {\n    d.barrier\n        (deadline)\n        .ok();\n}",
        "names `barrier`",
    ),
    (
        "fn injected(d: &mut Driver) { d.r#barrier(deadline).ok(); }",
        "names `barrier`",
    ),
    (
        "fn injected() { let call = Driver::barrier; drop(call); }",
        "names `barrier`",
    ),
    (
        "fn injected(d: &mut Driver) { Drive::barrier(d, deadline).ok(); }",
        "names `barrier`",
    ),
    (
        "fn injected(d: &mut Driver) { <Driver as Drive>::barrier(d, deadline).ok(); }",
        "names `barrier`",
    ),
    (
        "fn injected() { let ping = <Driver<'_> as Drive>::barrier; drop(ping); }",
        "names `barrier`",
    ),
    (
        "fn injected(live: &mut Live) { live.resize(1, 1); }",
        "resize(",
    ),
    ("fn injected() { crate::tui::open(); }", "crate::tui"),
    ("use crate::safety::FixtureRoot;", "FixtureRoot"),
    (
        "fn injected() { crate::runner::mutate::appear(); }",
        "mutate::",
    ),
    (
        "fn injected(request: DeletionRequest) {}",
        "DeletionRequest",
    ),
    (
        "fn injected(d: &mut Driver) { d.confirm_deletion(deadline); }",
        "confirm_deletion",
    ),
    (
        "fn injected(d: &mut Driver) { d.select_entry(deadline); }",
        "select_entry",
    ),
    (
        "fn injected(d: &mut Driver) { d.clear_filter(deadline); }",
        "clear_filter",
    ),
    (
        "fn injected(d: &mut Driver) { d.send_confirming(b\"j\", deadline); }",
        "send_confirming",
    ),
    (
        "fn injected(live: &mut Live) { live.note_raw_write(b\"j\"); }",
        "note_raw_write",
    ),
    (
        "fn injected(live: &mut Live) { live.session.send(b\"j\").ok(); }",
        "session.send(",
    ),
    (
        "fn injected() { std::fs::remove_dir_all(\"x\").ok(); }",
        "remove_dir",
    ),
    (
        "fn injected() { std::fs::remove_file(\"x\").ok(); }",
        "remove_file",
    ),
    (
        "fn injected() { std::fs::rename(\"x\", \"y\").ok(); }",
        "fs::rename",
    ),
    (
        "fn injected() { std::fs::write(\"x\", b\"y\").ok(); }",
        "fs::write(",
    ),
    (
        "fn injected() { std::fs::File::create(\"x\").ok(); }",
        "File::create(",
    ),
    (
        "fn injected(file: &File) { file.set_len(0).ok(); }",
        "set_len(",
    ),
    (
        "fn injected() { std::fs::set_permissions(\"x\", permissions).ok(); }",
        "set_permissions",
    ),
    ("use std::fs::rename;", "use std::fs::"),
    ("use crate::scenario::Step;", "scenario::Step"),
    (
        "fn injected() { std::fs::copy(\"x\", \"y\").ok(); }",
        "names `fs::copy`",
    ),
    (
        "fn injected(options: &mut OpenOptions) { options.truncate(true); }",
        "names `truncate(`",
    ),
    (
        "use super::super::fixture::FixtureSpec;",
        "names `super::super`",
    ),
    (
        "fn injected() { tempfile::TempPath::from_path(\"x\"); }",
        "names `TempPath::from_path`",
    ),
    (
        "fn injected(file: NamedTempFile) { file.persist(\"x\").ok(); }",
        "names `persist(`",
    ),
    (
        "fn injected() { std::os::unix::fs::chown(\"x\", None, None).ok(); }",
        "names `chown`",
    ),
    ("use rustix::fs::unlink;", "names `rustix`"),
    ("use portable_pty::CommandBuilder;", "names `portable_pty`"),
    (
        "fn injected(live: &mut Live) { PtySession::send(&mut live.session, &[1]).ok(); }",
        "names `::send`",
    ),
    (
        "fn injected(port: &mut PtySession) { port.send(&[1]).ok(); }",
        "names `.send`",
    ),
    (
        "fn injected(live: &mut Live) {\n    live.session\n        .send(&[1])\n        .ok();\n}",
        "names `.send`",
    ),
    (
        "fn injected(port: &mut PtySession) { port.r#send(&[1]).ok(); }",
        "names `.send`",
    ),
    (
        "fn injected(port: &mut PtySession) { PtySession::r#send(port, &[1]).ok(); }",
        "names `::send`",
    ),
    (
        "fn injected() { let write = PtySession::send; drop(write); }",
        "names `::send`",
    ),
    (
        "fn injected(d: &mut Driver) { PtySession::send(&mut d.live.session, &[0x7F]).ok(); }",
        "names `::send`",
    ),
    (
        "fn injected(d: &mut Driver) { PtySession::send(&mut d.live.session, &[0x7F]).ok(); }",
        "names `0x7f`",
    ),
    (
        "fn injected() { let write = Live::send_input; drop(write); }",
        "names `send_input`",
    ),
    (
        "#[path = \"../pty/session.rs\"] mod sneaky;",
        "names `#[path`",
    ),
    (
        "#[cfg_attr(unix, path = \"../pty/session.rs\")] mod sneaky;",
        "names `#[cfg_attr(.., path = ..)]`",
    ),
    ("include!(\"../pty/session.rs\");", "names `include!`"),
    (
        "fn injected() { nix::unistd::unlink(\"x\").ok(); }",
        "names `nix::`",
    ),
    (
        "fn injected() { unsafe { std::hint::unreachable_unchecked() } }",
        "names `unsafe`",
    ),
    ("extern crate std as s;", "names `extern crate`"),
    ("type Created = std::fs::File;", "names `type X = ..`"),
    ("type Created<T> = std::fs::File<T>;", "names `type X = ..`"),
    (
        "fn injected() { std::process::Command::new(\"rm\").status().ok(); }",
        "names `Command::new(`",
    ),
    (
        "fn injected() { std::process::Command::new::<&str>(\"rm\"); }",
        "names `Command::new(`",
    ),
    (
        "fn injected() { std::process::Command :: /* x */ new (\"rm\"); }",
        "names `Command::new(`",
    ),
    (
        "fn injected() { let make = std::process::Command::new; drop(make); }",
        "names `Command::new(`",
    ),
    (
        "fn injected() { let make = [\"rm\"].map(std::process::Command::new); drop(make); }",
        "names `Command::new(`",
    ),
    (
        "fn injected() { let make = <std::process::Command>::new(\"rm\"); drop(make); }",
        "names `Command`",
    ),
    ("use std::process::Command;", "names `Command`"),
    ("use std::process::{Command as C};", "names `Command`"),
    (
        "fn injected() -> SpawnSpec { SpawnSpec { program: PathBuf::from(\"/bin/rm\") } }",
        "names `SpawnSpec {`",
    ),
    (
        "fn injected() { let _ = CommandBuilder::new(\"rm\"); }",
        "names `CommandBuilder`",
    ),
    (
        "fn injected() { std::fs::/**/rename(\"x\", \"y\").ok(); }",
        "names `fs::rename`",
    ),
    (
        "fn injected() { std :: fs :: rename (\"x\", \"y\").ok(); }",
        "names `fs::rename`",
    ),
    (
        "fn injected() { std::fs::write /* c */ (\"x\", b\"y\").ok(); }",
        "names `fs::write(`",
    ),
    (
        "fn injected() { std::fs::File::create /* c */ (\"x\").ok(); }",
        "names `File::create(`",
    ),
    (
        "fn injected(live: &mut Live) { live.session /* c */ . /* c */ send(&[1]).ok(); }",
        "names `.send`",
    ),
    (
        "fn injected() { PtySession::/* c */send(port, &[1]).ok(); }",
        "names `::send`",
    ),
    (
        "fn injected(live: &mut Live) { live./* c */resize(1, 1); }",
        "resize(",
    ),
    (
        "fn injected() { let byte = u8 :: /* c */ from(127); }",
        "names `u8::from(127)`",
    ),
    ("use super::fixture::FixtureSpec;", "names `super::`"),
    ("use std::{fs::{write}, io};", "imports `std::fs::write`"),
    ("use std::{fs::write, io};", "imports `std::fs::write`"),
    ("use std::{fs::*, io};", "imports `std::fs::*`"),
    (
        "/* a comment\n// */ fn injected() { crate::fixture::remove_tree(Path::new(\"x\")); }",
        "remove_tree",
    ),
    (
        "const S: &str = \"\n#[cfg(test)]\nmod tests {\n\";\n\
         fn injected() { crate::fixture::remove_tree(Path::new(\"x\")); }",
        "remove_tree",
    ),
    ("const INJECTED: u8 = 0x7F;", "names `0x7f`"),
    ("const INJECTED: u8 = 0x7f_u8;", "names `0x7f`"),
    ("const INJECTED: u8 = 0x007f;", "names `0x7f`"),
    ("const INJECTED: u8 = 0x_7f;", "names `0x7f`"),
    ("const INJECTED: u8 = 0o177;", "names `0o177`"),
    ("const INJECTED: u8 = 0o0177;", "names `0o177`"),
    ("const INJECTED: u8 = 0b1111111;", "names `0b1111111`"),
    ("const INJECTED: u8 = 0b0111_1111;", "names `0b1111111`"),
    ("const INJECTED: u8 = 127u8;", "names `127u8`"),
    ("const INJECTED: u8 = 127_u8;", "names `127u8`"),
    ("const INJECTED: u8 = 1_27u8;", "names `127u8`"),
    ("const INJECTED: u8 = 127i8;", "names `127u8`"),
    (r"const INJECTED: u8 = b'\x7f';", r"names `\x7f`"),
    (r#"const INJECTED: u8 = b"\x7F";"#, r"names `\x7f`"),
    (r"const INJECTED: u8 = '\u{7f}';", r"names `\u{7f}`"),
    (r"const INJECTED: u8 = '\u{007F}';", r"names `\u{7f}`"),
    (r"const INJECTED: u8 = '\u{00_7f}';", r"names `\u{7f}`"),
    ("const INJECTED: u8 = \"Backspace\";", "names `backspace`"),
    ("const INJECTED: u8 = \"BACKSPACE\";", "names `backspace`"),
    ("const INJECTED: u8 = backspace;", "names `backspace`"),
    (
        "const INJECTED: u8 = u8::from(127);",
        "names `u8::from(127)`",
    ),
    (
        "const INJECTED: u8 = u8::from( 127 );",
        "names `u8::from(127)`",
    ),
    (
        "const INJECTED: u8 = u8 :: from ( 127 );",
        "names `u8::from(127)`",
    ),
    (
        "const INJECTED: u8 = char::from(127);",
        "names `char::from(127)`",
    ),
    (
        "const INJECTED: u8 = char::from( 127 );",
        "names `char::from(127)`",
    ),
    ("const INJECTED: u8 = 0x08;", "names `0x08`"),
    ("const INJECTED: u8 = 0x8;", "names `0x08`"),
    ("const INJECTED: u8 = 0x008;", "names `0x08`"),
    ("const INJECTED: u8 = 0x08_u8;", "names `0x08`"),
    ("const INJECTED: u8 = 0o10;", "names `0o10`"),
    ("const INJECTED: u8 = 0o010;", "names `0o10`"),
    ("const INJECTED: u8 = 0b1000;", "names `0b1000`"),
    ("const INJECTED: u8 = 0b0000_1000;", "names `0b1000`"),
    ("const INJECTED: u8 = 8u8;", "names `8u8`"),
    ("const INJECTED: u8 = 8_u8;", "names `8u8`"),
    ("const INJECTED: u8 = 08u8;", "names `8u8`"),
    (r"const INJECTED: u8 = b'\x08';", r"names `\x08`"),
    (r#"const INJECTED: u8 = b"\x08";"#, r"names `\x08`"),
    (r"const INJECTED: u8 = '\u{8}';", r"names `\u{8}`"),
    (r"const INJECTED: u8 = '\u{08}';", r"names `\u{8}`"),
    (r"const INJECTED: u8 = '\u{0008}';", r"names `\u{8}`"),
    ("const INJECTED: u8 = u8::from(8);", "names `u8::from(8)`"),
    (
        "const INJECTED: u8 = char::from(8);",
        "names `char::from(8)`",
    ),
    (
        "const INJECTED: u8 = char::from( 8 );",
        "names `char::from(8)`",
    ),
];

#[test]
fn the_scan_catches_a_call_put_where_an_earlier_version_of_it_stopped_reading() {
    let sources = soak_sources();
    for (injected, expected) in INJECTIONS {
        let changed = with_replaced(
            &sources,
            "mod.rs",
            "mod tests;\n",
            &format!("mod tests;\n{injected}\n"),
        );
        let found = violations(&changed);
        assert!(
            found.iter().any(|sentence| sentence.contains(expected)),
            "`{injected}` after `mod tests;` was not caught: {found:?}"
        );
    }
}

#[test]
fn the_bytes_that_ask_for_a_deletion_are_named_only_in_the_allowlist() {
    for (name, raw) in soak_sources() {
        if name == "keys.rs" {
            continue;
        }
        let spellings = byte_spellings(&code_of(&raw));
        assert!(
            spellings.is_empty(),
            "{name} names {spellings:?}: only the allowlist (keys.rs) says which bytes it refuses"
        );
    }
}

#[test]
fn one_call_writes_to_the_program_and_it_is_the_one_behind_the_allowlist() {
    let mut writes: Vec<String> = soak_sources()
        .iter()
        .flat_map(|(name, raw)| {
            code_of(raw)
                .lines()
                .filter(|line| line.contains("send_input("))
                .map(|line| format!("{name}: {}", line.trim()))
                .collect::<Vec<_>>()
        })
        .collect();
    writes.sort();

    assert_eq!(
        writes,
        [
            "session.rs: fn send_input(&mut self, bytes: &[u8]) -> Result<Instant, RunError> {",
            "session.rs: let sent = match self.live.send_input(key.bytes()) {",
            "session.rs: self.send_input(key.bytes())",
        ],
        "every send of a key is a call of the choke point with the bytes of an allowed key, and \
         the only write of those bytes is the one the choke point makes after it has vetted them"
    );
}

#[test]
fn the_one_removal_the_soak_makes_is_replacing_a_link_that_a_run_made() {
    // The soak removes nothing itself (see the forbidden names above). Its output directory can be
    // inside the tree it soaks, and the one thing it ever removes there is `latest`, when it is
    // what an earlier run made: that is `point_latest_at` (`run_support.rs`, which has tests of
    // its own for what it removes and what it leaves alone), called once.
    let calls: Vec<String> = soak_sources()
        .iter()
        .flat_map(|(name, raw)| {
            code_of(raw)
                .lines()
                .filter(|line| line.contains("point_latest_at("))
                .map(|line| format!("{name}: {}", line.trim()))
                .collect::<Vec<_>>()
        })
        .collect();

    assert_eq!(
        calls,
        ["mod.rs: point_latest_at(request.out_root, &run_id).map_err(|source| SoakError::Io {"]
    );
}

#[test]
fn every_rule_of_the_scan_has_a_case_in_the_injections() {
    let names = FORBIDDEN.iter().map(|(token, _)| *token);
    let forms = FORBIDDEN_FORMS.iter().map(|(label, _, _)| *label);
    let spellings = BYTE_SPELLINGS.iter().map(|(label, _, _)| *label);
    for label in names.chain(forms).chain(spellings) {
        assert!(
            INJECTIONS
                .iter()
                .any(|(_, expected)| expected.contains(label)),
            "no case in INJECTIONS expects `{label}`"
        );
    }
}

/// Source text that is Backspace, in every way to write the byte 127 (`0x7f`).
const BACKSPACE_AS_WRITTEN: &[&str] = &[
    "0x7f",
    "0x7F",
    "0x7f_u8",
    "0x007f",
    "0x_7f",
    "0x7fi8",
    "0o177",
    "0o0177",
    "0o1_77",
    "0b1111111",
    "0b0111_1111",
    "127u8",
    "127_u8",
    "1_27u8",
    "0127u8",
    "127i8",
    r"b'\x7f'",
    r"b'\x7F'",
    r#"b"\x7f""#,
    r"'\u{7f}'",
    r"'\u{7F}'",
    r"'\u{007f}'",
    r"'\u{00_7f}'",
    r"'\u{00007f}'",
    "Backspace",
    "BACKSPACE",
    "backspace",
    "KeyName::Backspace",
    "u8::from(127)",
    "u8::from( 127 )",
    "u8 :: from ( 127 )",
    "u8::from(0127)",
    "char::from(127)",
    "char::from( 127 )",
];

/// Source text that is Ctrl+H, in every way to write the byte 8 (`0x08`).
const CTRL_H_AS_WRITTEN: &[&str] = &[
    "0x08",
    "0x8",
    "0x008",
    "0x08_u8",
    "0x0_8",
    "0x8i8",
    "0o10",
    "0o010",
    "0o1_0",
    "0b1000",
    "0b0000_1000",
    "0b01000",
    "8u8",
    "8_u8",
    "08u8",
    "8i8",
    r"b'\x08'",
    r#"b"\x08""#,
    r"'\u{8}'",
    r"'\u{08}'",
    r"'\u{0008}'",
    r"'\u{00_08}'",
    "u8::from(8)",
    "u8::from( 8 )",
    "char::from(8)",
    "char::from( 8 )",
    "char::from(08)",
];

/// Source text of numbers and names that look a little like Backspace and Ctrl+H, and are not.
const NOT_THOSE_BYTES: &[&str] = &[
    "127",
    "8",
    "0",
    "7",
    "0x7",
    "0x7e",
    "0x80",
    "0x18",
    "0x7fe",
    "0x87",
    "0x1b",
    "0x1d",
    "0xff",
    "0o7",
    "0o11",
    "0o17",
    "0o100",
    "0o1770",
    "0b100",
    "0b10000",
    "0b111111",
    "0b11111111",
    "18u8",
    "28u8",
    "80u8",
    "1270u8",
    "127u16",
    "8u16",
    "127usize",
    "0u8",
    "8.0f32",
    r"'\x1b'",
    r"'\x07'",
    r"'\x80'",
    r"'\u{1b}'",
    r"'\u{87}'",
    r"'\u{7f0}'",
    "u8::from(18)",
    "u8::from(80)",
    "u8::from(1270)",
    "char::from(80)",
    "char::from(1270)",
    "Backspac",
    "Back space",
];

#[test]
fn every_way_to_write_backspace_and_ctrl_h_is_named_and_other_numbers_are_not() {
    for (spellings, why) in [
        (BACKSPACE_AS_WRITTEN, BACKSPACE),
        (CTRL_H_AS_WRITTEN, CTRL_H),
    ] {
        for spelling in spellings {
            let found = byte_spellings(&format!("let byte = {spelling};"));
            assert!(
                found.iter().any(|(_, found_why)| *found_why == why),
                "`{spelling}` is not named as {why}: {found:?}"
            );
        }
    }
    for other in NOT_THOSE_BYTES {
        let found = byte_spellings(&format!("let byte = {other};"));
        assert!(
            found.is_empty(),
            "`{other}` is named as a byte it is not: {found:?}"
        );
    }
}

#[test]
fn a_literal_that_goes_on_over_a_line_break_holds_the_next_line_as_its_content() {
    let text = r##"let a = "one
two";
let b = r#"three
"four"
"#;
/* five
six */
let c = 1;
"##;

    assert_eq!(
        lines_inside_literals(text),
        [false, true, false, true, true, false, true, false, false]
    );
}

#[test]
fn a_quote_that_is_a_lifetime_or_a_character_does_not_open_a_string() {
    let text = r#"fn f<'a>(x: &'a str) -> char { '"' }
fn g() -> char { '\'' }
let label = 'outer: loop {};
let byte = b'"';
// it's a comment
let end = 1;
"#;

    assert!(lines_inside_literals(text).iter().all(|inside| !inside));
}

#[test]
fn block_comments_nest_and_a_string_holds_what_looks_like_a_comment() {
    let text = r#"let url = "http://x"; // c
/* a /* b
*/ still
*/ let x = 1;
"#;

    assert_eq!(
        lines_inside_literals(text),
        [false, false, true, true, false]
    );
}

#[test]
fn only_a_test_module_is_left_out_of_the_code_and_only_what_it_holds() {
    let text = "\
// a comment
use a;

/// A doc comment.
fn kept() {}

#[cfg(test)]
mod tests;

fn also_kept() {}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    fn hidden() {}
}
";

    assert_eq!(
        code_of(text),
        "use a;\n\nfn kept() {}\n\n\nfn also_kept() {}\n"
    );
}

#[test]
fn a_test_module_header_inside_a_string_hides_nothing() {
    // The string makes `#[cfg(test)]` and `mod tests {` look like the head of a test module that
    // ends at the last line, and hides the call between them from a scan that reads lines only.
    let text = r#"fn last() {
    let s = "
#[cfg(test)]
mod tests {
";
    crate::fixture::remove_tree(p);
}
"#;

    assert!(code_of(text).contains("remove_tree"));
}

#[test]
fn a_line_that_begins_inside_a_block_comment_is_not_a_line_comment() {
    let text = "/* a comment\n// */ crate::fixture::remove_tree(p);\n";

    assert!(code_of(text).contains("remove_tree"));
}

#[test]
#[should_panic(expected = "only test modules are left out")]
fn a_cfg_test_on_anything_but_a_module_is_refused() {
    let _ = code_of("fn kept() {}\n\n#[cfg(test)]\nfn only_in_tests() {}\n");
}

#[test]
#[should_panic(expected = "is named `tests`")]
fn a_test_module_that_is_not_named_tests_is_refused() {
    // A module of another name, here on one line, would end the skipping at the next `}` in the
    // first column, wherever that is.
    let _ = code_of("fn kept() {}\n\n#[cfg(test)]\nmod t {}\n\nfn hidden() {}\n}\n");
}

#[test]
#[should_panic(expected = "is named `tests`")]
fn an_out_of_line_test_module_that_is_not_named_tests_is_refused() {
    let _ = code_of("fn kept() {}\n\n#[cfg(test)]\nmod helpers;\n");
}

#[test]
#[should_panic(expected = "alone on a line in the first column")]
fn a_test_module_written_on_one_line_is_refused() {
    let _ = code_of("fn kept() {}\n\n#[cfg(test)]\nmod tests {}\n\nfn hidden() {}\n}\n");
}

#[test]
#[should_panic(expected = "alone on a line in the first column")]
fn a_test_module_whose_opening_brace_shares_its_line_is_refused() {
    let _ = code_of("#[cfg(test)]\nmod tests { fn hidden() {}\n}\n");
}

#[test]
#[should_panic(expected = "must be the last item of the file")]
fn an_in_line_test_module_that_code_follows_is_refused() {
    let _ = code_of("#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n\nfn not_hidden() {}\n");
}

#[test]
#[should_panic(expected = "is never closed")]
fn an_in_line_test_module_that_is_never_closed_is_refused() {
    let _ = code_of("#[cfg(test)]\nmod tests {\n    fn t() {}\n");
}

#[test]
fn the_scan_reads_every_rust_file_below_the_soak_directory_nested_ones_included() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    fs::create_dir_all(directory.path().join("sub/deeper")).expect("directories");
    for (path, text) in [
        ("mod.rs", "mod sub;"),
        ("tests.rs", "// the tests of the module"),
        ("sub/mod.rs", "mod deeper;"),
        ("sub/tests.rs", "// the tests of a nested module"),
        ("sub/deeper/leaf.rs", "fn leaf() {}"),
        ("notes.txt", "not code"),
    ] {
        fs::write(directory.path().join(path), text).expect("a file");
    }

    let found: Vec<String> = sources_below(directory.path())
        .into_iter()
        .map(|(name, _)| name)
        .collect();

    assert_eq!(
        found,
        [
            "mod.rs",
            "sub/deeper/leaf.rs",
            "sub/mod.rs",
            "sub/tests.rs",
            "tests.rs"
        ]
    );
    assert!(
        soak_sources().iter().all(|(name, _)| name != "tests.rs"),
        "only this file is left out of the scan"
    );
}

#[test]
fn a_file_in_a_nested_directory_is_held_to_the_rules_too() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    fs::create_dir(directory.path().join("sub")).expect("a directory");
    fs::write(
        directory.path().join("sub/leaf.rs"),
        "fn write_it(port: &mut PtySession) { port.send(&[1]).ok(); }\n",
    )
    .expect("a file");

    let found = violations(&sources_below(directory.path()));

    assert!(says(&found, "sub/leaf.rs", ".send"), "{found:?}");
}

#[cfg(unix)]
#[test]
#[should_panic(expected = "is a link")]
fn a_link_below_the_soak_directory_stops_the_scan() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    std::os::unix::fs::symlink(directory.path(), directory.path().join("sneaky")).expect("a link");

    let _ = sources_below(directory.path());
}

/// Whether `found` has a sentence that says `file` holds `line` this many times.
fn holds(found: &[String], file: &str, line: &str, times: usize) -> bool {
    let wanted = format!("{file} holds `{line}` {times} times");
    found.iter().any(|sentence| sentence.starts_with(&wanted))
}

/// `sources`, with the first line of `file` that is `line` (trimmed) changed into what `change`
/// makes of it.
fn with_line_changed(
    sources: &[(String, String)],
    file: &str,
    line: &str,
    change: impl Fn(&str) -> String,
) -> Vec<(String, String)> {
    let mut changed = sources.to_vec();
    let text = text_of(&mut changed, file);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let index = lines
        .iter()
        .position(|candidate| candidate.trim() == line)
        .unwrap_or_else(|| panic!("{file} has no line `{line}`"));
    lines[index] = change(&lines[index]);
    *text = lines.join("\n") + "\n";
    changed
}

/// `sources`, with the line of `block` at `index` changed into what `change` makes of it. The
/// lines of the block are looked for in its file from the first of them on, so that a comment or
/// a blank line between two of them does not matter.
fn with_block_line_changed(
    sources: &[(String, String)],
    block: &Block,
    index: usize,
    change: impl Fn(&str) -> String,
) -> Vec<(String, String)> {
    let mut changed = sources.to_vec();
    let text = text_of(&mut changed, block.file);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut from = lines
        .iter()
        .position(|candidate| candidate.trim() == block.lines[0])
        .unwrap_or_else(|| panic!("{} has no block `{}`", block.file, block.what));
    for (at, pinned) in block.lines.iter().enumerate() {
        let found = from
            + lines[from..]
                .iter()
                .position(|candidate| candidate.trim() == *pinned)
                .unwrap_or_else(|| panic!("{} lacks `{pinned}` in `{}`", block.file, block.what));
        if at == index {
            lines[found] = change(&lines[found]);
            break;
        }
        from = found + 1;
    }
    *text = lines.join("\n") + "\n";
    changed
}

#[test]
fn every_name_that_a_pin_refuses_is_refused_where_no_line_pins_it() {
    let sources = all_sources();
    for pin in PINS {
        for (label, _, statement) in pin.names {
            let injected = format!("fn injected() {{ {statement} }}");
            let changed = with_replaced(
                &sources,
                "mod.rs",
                "mod tests;\n",
                &format!("mod tests;\n{injected}\n"),
            );
            let found = violations(&changed);
            assert!(
                says(&found, "mod.rs", label),
                "`{injected}` after `mod tests;` was not caught: {found:?}"
            );
            if pin.xtask {
                for file in ["xtask/soak.rs", "xtask/soak_build.rs"] {
                    let found = violations(&with_prepended(&sources, file, &injected));
                    assert!(
                        says(&found, file, label),
                        "`{injected}` in {file} was not caught: {found:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_pinned_line_is_held_exactly_as_often_as_the_scan_pins_it() {
    let sources = all_sources();
    for pin in PINS {
        for &(file, line, count) in pin.lines {
            let gone = with_line_changed(&sources, file, line, |_| String::new());
            let found = violations(&gone);
            assert!(
                holds(&found, file, line, count - 1),
                "`{line}` gone from {file} was not caught: {found:?}"
            );

            let twice = with_line_changed(&sources, file, line, |text| format!("{text}\n{text}"));
            let found = violations(&twice);
            assert!(
                holds(&found, file, line, count + 1),
                "`{line}` twice in {file} was not caught: {found:?}"
            );

            let elsewhere = if file == "limits.rs" {
                "facts.rs"
            } else {
                "limits.rs"
            };
            let found = violations(&with_prepended(&sources, elsewhere, line));
            assert!(
                found
                    .iter()
                    .any(|sentence| sentence.starts_with(&format!("{elsewhere} names `"))),
                "`{line}` in {elsewhere} was not caught: {found:?}"
            );
        }
    }
}

#[test]
fn a_block_that_the_scan_pins_is_refused_when_a_line_of_it_changes_goes_or_comes() {
    let sources = soak_sources();
    for block in BLOCKS {
        let refused = |changed: &[(String, String)]| {
            let wanted = format!("{} holds {}", block.file, block.what);
            violations(changed)
                .iter()
                .any(|sentence| sentence.starts_with(&wanted))
        };
        let last = block.lines.len() - 1;
        for index in 0..=last {
            let changed =
                with_block_line_changed(&sources, block, index, |text| format!("{text} x"));
            assert!(
                refused(&changed),
                "line {index} of {} changed was not caught",
                block.what
            );
        }
        // The first and the last lines are the brackets of the block: without one of them the
        // text is not Rust, so only the lines between them are taken away.
        for index in 1..last {
            let gone = with_block_line_changed(&sources, block, index, |_| String::new());
            assert!(
                refused(&gone),
                "line {index} of {} gone was not caught",
                block.what
            );
        }
        for index in 0..last {
            let more =
                with_block_line_changed(&sources, block, index, |text| format!("{text}\nanother,"));
            assert!(
                refused(&more),
                "a line after line {index} of {} was not caught",
                block.what
            );
        }
    }
}

#[test]
fn the_one_command_the_soak_starts_is_the_binary_under_test_in_headless_rs() {
    let sources = all_sources();
    let second = "fn injected() { Command::new(\"rm\"); }";

    for (what, changed, name) in [
        (
            "a second call in the same file",
            with_prepended(&sources, "headless.rs", second),
            "headless.rs",
        ),
        (
            "another program on the allowed line",
            with_replaced(
                &sources,
                "headless.rs",
                COMMAND_LINE,
                "let mut command = Command::new(\"rm\");",
            ),
            "headless.rs",
        ),
        (
            "the allowed line in another file",
            with_prepended(&sources, "mod.rs", COMMAND_LINE),
            "mod.rs",
        ),
        (
            "the line that starts cargo in the soak",
            with_prepended(&sources, "mod.rs", CARGO_LINE),
            "mod.rs",
        ),
        (
            "the line that starts git in the soak",
            with_prepended(&sources, "mod.rs", GIT_LINE),
            "mod.rs",
        ),
        (
            "the line that starts cat in the soak",
            with_prepended(&sources, "mod.rs", CAT_LINE),
            "mod.rs",
        ),
        (
            "the line that starts git in the rest of the command",
            with_prepended(&sources, "xtask/soak.rs", GIT_LINE),
            "xtask/soak.rs",
        ),
        (
            "a second call in the build of the command",
            with_prepended(&sources, "xtask/soak_build.rs", second),
            "xtask/soak_build.rs",
        ),
        (
            "the line that starts cargo in the rest of the command",
            with_prepended(&sources, "xtask/soak.rs", CARGO_LINE),
            "xtask/soak.rs",
        ),
        (
            "the line that starts the binary in the command",
            with_prepended(&sources, "xtask/soak.rs", COMMAND_LINE),
            "xtask/soak.rs",
        ),
    ] {
        let found = violations(&changed);
        assert!(
            says(&found, name, "Command::new("),
            "{what} was not caught: {found:?}"
        );
    }
}

#[test]
fn the_one_barrier_the_soak_names_is_the_override_that_refuses_it_in_session_rs() {
    let sources = soak_sources();
    let call = "fn injected(d: &mut Driver) { d.barrier(deadline).ok(); }";
    let value = "fn injected() { let ping = <Driver<'_> as Drive>::barrier; drop(ping); }";
    let override_twice = format!("`{BARRIER_LINE}` 2 times");

    for (what, changed, name, expected) in [
        (
            "a call in the same file",
            with_prepended(&sources, "session.rs", call),
            "session.rs",
            "names `barrier`",
        ),
        (
            "the value form in the same file",
            with_prepended(&sources, "session.rs", value),
            "session.rs",
            "names `barrier`",
        ),
        (
            "another signature on the allowed line",
            with_replaced(
                &sources,
                "session.rs",
                BARRIER_LINE,
                "fn barrier(&mut self, deadline: Instant) -> Result<Waited<()>, RunError> {",
            ),
            "session.rs",
            "names `barrier`",
        ),
        (
            "a second override in the same file",
            with_prepended(&sources, "session.rs", BARRIER_LINE),
            "session.rs",
            override_twice.as_str(),
        ),
        (
            "the allowed line in another file",
            with_prepended(&sources, "mod.rs", BARRIER_LINE),
            "mod.rs",
            "names `barrier`",
        ),
    ] {
        let found = violations(&changed);
        assert!(
            found
                .iter()
                .any(|sentence| sentence.starts_with(name) && sentence.contains(expected)),
            "{what} was not caught: {found:?}"
        );
    }
}

#[test]
fn the_terminal_session_is_started_from_one_pinned_literal_by_one_call() {
    let sources = soak_sources();
    let program = "program: context.binary.to_path_buf(),";
    let arguments = "args: vec![context.root.path().as_os_str().to_owned()],";
    let flag = "args: vec![context.root.path().as_os_str().to_owned(), \"--disable-delete-confirmation\".into()],";
    let session = |anchor: &str, replacement: &str| {
        with_replaced(&sources, "session.rs", anchor, replacement)
    };
    let injected = |statement: &str| {
        with_replaced(
            &sources,
            "mod.rs",
            "mod tests;\n",
            &format!("mod tests;\nfn injected() {{ {statement} }}\n"),
        )
    };

    for (what, changed, expected) in [
        (
            "a program of another path",
            session(program, "program: PathBuf::from(\"/bin/rm\"),"),
            SPAWN_LITERAL,
        ),
        (
            "a program that is not the binary under test",
            session(program, "program: other.binary.to_path_buf(),"),
            SPAWN_LITERAL,
        ),
        (
            "a field written as a shorthand",
            session(program, "program,"),
            SPAWN_LITERAL,
        ),
        (
            "a field that is not in the list",
            session(
                "drain_bytes_per_sec: None,",
                "drain_bytes_per_sec: None,\n        extra: 1,",
            ),
            SPAWN_LITERAL,
        ),
        (
            "a field that is gone",
            session("cols: TERMINAL.0,", ""),
            SPAWN_LITERAL,
        ),
        (
            "a flag among the arguments",
            session(arguments, flag),
            SPAWN_LITERAL,
        ),
        (
            "an assignment to the program",
            injected("spec.program = other;"),
            "names `.program = ..`",
        ),
        (
            "a second literal",
            injected("let spec = SpawnSpec { program };"),
            "names `SpawnSpec {`",
        ),
        (
            "a spec made by a function",
            injected("let spec = SpawnSpec::default();"),
            "names `SpawnSpec::`",
        ),
        (
            "a second call that starts a program",
            injected("PtySession::spawn(&spec);"),
            "names `PtySession::spawn(`",
        ),
    ] {
        let found = violations(&changed);
        assert!(
            found.iter().any(|sentence| sentence.contains(expected)),
            "{what} was not caught: {found:?}"
        );
    }
}

#[test]
fn the_process_of_the_headless_scan_is_pinned_argument_by_argument_and_call_by_call() {
    let sources = soak_sources();
    let headless = |anchor: &str, replacement: &str| {
        with_replaced(&sources, "headless.rs", anchor, replacement)
    };
    let injected = |statement: &str| {
        with_replaced(
            &sources,
            "mod.rs",
            "mod tests;\n",
            &format!("mod tests;\nfn injected() {{ {statement} }}\n"),
        )
    };
    let envs = ".envs(isolated_env(scratch, profile, false, None))";

    for (what, changed, expected) in [
        (
            "another argument",
            headless(
                "\"json\".into(),",
                "\"json\".into(),\n        \"--disable-delete-confirmation\".into(),",
            ),
            "the arguments of the headless scan",
        ),
        (
            "an argument that is gone",
            headless("\"--format\".into(),", ""),
            "the arguments of the headless scan",
        ),
        (
            "an argument added by a call",
            headless(".env_clear()", ".env_clear()\n        .arg(\"--x\")"),
            "names `.arg(`",
        ),
        (
            "arguments of another list",
            headless(".args(&arguments)", ".args([\"--x\"])"),
            "names `.args(`",
        ),
        (
            "a variable that is added",
            headless(
                ".env_clear()",
                ".env_clear()\n        .env(\"HOME\", \"/\")",
            ),
            "names `.env(`",
        ),
        (
            "a variable that is taken out",
            headless(".env_clear()", ".env_remove(\"HOME\")"),
            "names `.env_remove(`",
        ),
        (
            "the whole environment of the person",
            headless(envs, ".envs(std::env::vars())"),
            "names `.envs(`",
        ),
        (
            "another directory",
            headless(".current_dir(scratch.cwd());", ".current_dir(\"/\");"),
            "names `.current_dir(`",
        ),
        (
            "the person's streams",
            headless(
                ".current_dir(scratch.cwd());",
                ".current_dir(scratch.cwd())\n        .stdin(Stdio::inherit());",
            ),
            "names `.stdin(`",
        ),
        (
            "another process group",
            injected("command.process_group(1);"),
            "names `.process_group(`",
        ),
        (
            "a process that runs before the program",
            injected("command.pre_exec(hook);"),
            "names `.pre_exec(`",
        ),
        ("another user", injected("command.uid(0);"), "names `.uid(`"),
        (
            "another group",
            injected("command.gid(0);"),
            "names `.gid(`",
        ),
    ] {
        let found = violations(&changed);
        assert!(
            found.iter().any(|sentence| sentence.contains(expected)),
            "{what} was not caught: {found:?}"
        );
    }
}

/// Code put into a copy of each file of the xtask that holds the command, and a part of the
/// sentence that the scan must say about it. The command asks the person, builds, and runs the
/// soak: it may read the file system, and it may not write, delete, or start a program but where
/// a line is pinned.
const XTASK_INJECTIONS: &[(&str, &str)] = &[
    (
        "fn injected() { std::fs::write(\"x\", b\"y\").ok(); }",
        "names `fs::write(`",
    ),
    (
        "fn injected() { std::fs::rename(\"x\", \"y\").ok(); }",
        "names `fs::rename`",
    ),
    (
        "fn injected() { std::fs::remove_file(\"x\").ok(); }",
        "names `remove_file`",
    ),
    (
        "fn injected() { std::fs::remove_dir_all(\"x\").ok(); }",
        "names `remove_dir`",
    ),
    (
        "fn injected() { std::fs::File::create(\"x\").ok(); }",
        "names `File::create(`",
    ),
    (
        "fn injected() { let _ = std::fs::OpenOptions::new(); }",
        "names `OpenOptions`",
    ),
    (
        "fn injected() { std::fs::create_dir(\"x\").ok(); }",
        "names `create_dir`",
    ),
    (
        "fn injected() { std::fs::create_dir_all(\"x\").ok(); }",
        "names `create_dir_all`",
    ),
    (
        "fn injected() { std::fs::set_permissions(\"x\", permissions).ok(); }",
        "names `set_permissions`",
    ),
    (
        "fn injected() { std::os::unix::fs::symlink(\"x\", \"y\").ok(); }",
        "names `symlink`",
    ),
    (
        "fn injected() { std::fs::copy(\"x\", \"y\").ok(); }",
        "names `fs::copy`",
    ),
    (
        "fn injected() { std::io::copy(&mut from, &mut to).ok(); }",
        "names `::copy`",
    ),
    (
        "fn injected() { unsafe { std::hint::unreachable_unchecked() } }",
        "names `unsafe`",
    ),
    ("include!(\"x.rs\");", "names `include!`"),
    ("#[path = \"x.rs\"] mod sneaky;", "names `#[path`"),
    (
        "fn injected(port: &mut Port) { port.send(&[1]).ok(); }",
        "names `.send`",
    ),
    (
        "fn injected() { let write = Port::send; drop(write); }",
        "names `::send`",
    ),
    ("const INJECTED: u8 = 0x7F;", "names `0x7f`"),
    ("const INJECTED: u8 = 0x08;", "names `0x08`"),
    (r"const INJECTED: u8 = b'\x7f';", r"names `\x7f`"),
    ("const INJECTED: Key = Key::Backspace;", "names `backspace`"),
    (
        "fn injected() { std::process::Command::new(\"rm\").status().ok(); }",
        "names `Command::new(`",
    ),
];

#[test]
fn the_files_of_the_xtask_that_hold_the_command_are_held_to_the_rules_too() {
    let sources = all_sources();
    for file in ["xtask/soak.rs", "xtask/soak_build.rs"] {
        for (injected, expected) in XTASK_INJECTIONS {
            let found = violations(&with_prepended(&sources, file, injected));
            assert!(
                found
                    .iter()
                    .any(|sentence| sentence.starts_with(file) && sentence.contains(expected)),
                "`{injected}` in {file} was not caught: {found:?}"
            );
        }
    }
}

#[test]
fn the_command_may_read_the_file_system() {
    let sources = all_sources();
    let reads = "fn injected() { let _ = (std::fs::symlink_metadata(\"x\"), std::fs::canonicalize(\"x\"), std::fs::metadata(\"x\")); }";
    for file in ["xtask/soak.rs", "xtask/soak_build.rs"] {
        assert_eq!(
            violations(&with_prepended(&sources, file, reads)),
            Vec::<String>::new(),
            "{file}"
        );
    }
}

#[test]
fn a_line_that_is_fine_in_the_command_is_fine_once_and_only_there() {
    let sources = all_sources();
    for &(file, line, _) in XTASK_LINES {
        let gone = with_line_changed(&sources, file, line, |_| String::new());
        let found = violations(&gone);
        assert!(
            holds(&found, file, line, 0),
            "`{line}` gone from {file} was not caught: {found:?}"
        );

        let twice = with_line_changed(&sources, file, line, |text| format!("{text}\n{text}"));
        let found = violations(&twice);
        assert!(
            holds(&found, file, line, 2),
            "`{line}` twice in {file} was not caught: {found:?}"
        );

        let found = violations(&with_prepended(&sources, "mod.rs", line));
        assert!(
            found
                .iter()
                .any(|sentence| sentence.starts_with("mod.rs names `")),
            "`{line}` in the soak was not caught: {found:?}"
        );
    }
}

/// The names that the scan must refuse, each as a sentence gives it, whatever the tables above
/// hold: a name that is in `FORBIDDEN`, in `FORBIDDEN_FORMS`, or in a pin is refused, and a name
/// that is in none of them is a gap.
const NAMES_THE_SCAN_MUST_REFUSE: &[&str] = &[
    // The one write to the program that does not pass the choke point, and the ways to name it.
    "barrier",
    "send_barrier",
    "::send",
    ".send",
    "session.send(",
    // What starts a process, and what builds one.
    "Command::new(",
    "Command",
    ".arg(",
    ".args(",
    ".env(",
    ".envs(",
    ".env_remove(",
    ".env_clear(",
    ".current_dir(",
    ".stdin(",
    ".stdout(",
    ".stderr(",
    ".process_group(",
    ".pre_exec(",
    ".uid(",
    ".gid(",
    // What starts a program in a terminal, and what decides what it is.
    "SpawnSpec {",
    "SpawnSpec::",
    "PtySession::spawn(",
    ".program = ..",
    "portable_pty",
    "CommandBuilder",
    // What makes, opens, copies, links, or removes a path.
    "OpenOptions",
    "create_new",
    "DirBuilder",
    "create_dir",
    "create_dir_all",
    "symlink",
    "hard_link",
    "set_modified",
    "set_times",
    "CastWriter",
    "::copy",
    "fs::copy",
    "truncate(",
    "os::unix::fs",
    "os::windows::fs",
    "tempfile",
    "tempfile_in",
    "tempdir",
    "tempdir_in",
    "NamedTempFile",
    "persist_noclobber",
    "TempDir",
    "TempPath",
    "remove_file",
    "remove_dir",
    "fs::rename",
    "fs::write(",
    "File::create(",
    // What changes a mode.
    "set_permissions",
    // What leaves the soak by a path that this scan does not read.
    "super::super",
    "#[path",
    "include!",
    "unsafe",
];

#[test]
fn the_scan_refuses_every_name_that_it_was_asked_to() {
    let labels: Vec<&str> = FORBIDDEN
        .iter()
        .map(|(token, _)| *token)
        .chain(FORBIDDEN_FORMS.iter().map(|(label, _, _)| *label))
        .chain(
            PINS.iter()
                .flat_map(|pin| pin.names.iter().map(|(label, _, _)| *label)),
        )
        .collect();

    for name in NAMES_THE_SCAN_MUST_REFUSE {
        assert!(labels.contains(name), "no rule of the scan names `{name}`");
    }
}

#[test]
fn super_leaves_the_soak_in_mod_rs_and_super_super_leaves_it_in_every_file() {
    let sources = soak_sources();
    let line = "use super::keys::Key;";

    let beside = violations(&with_prepended(&sources, "headless.rs", line));
    assert!(
        !says(&beside, "headless.rs", "super::"),
        "`super::` is the soak in a file beside `mod.rs`: {beside:?}"
    );
    let at_the_top = violations(&with_prepended(&sources, "mod.rs", line));
    assert!(says(&at_the_top, "mod.rs", "super::"), "{at_the_top:?}");
    let leaving = violations(&with_prepended(
        &sources,
        "headless.rs",
        "use super::super::fixture::FixtureSpec;",
    ));
    assert!(says(&leaving, "headless.rs", "super::super"), "{leaving:?}");
}

#[test]
fn an_import_of_a_type_from_std_fs_is_fine_and_one_of_a_function_is_not() {
    let sources = soak_sources();

    for (line, refused) in [
        ("use std::{fs::File, io};", false),
        ("use std::{fs, io};", false),
        ("use std::{fs::{self, File}, io};", false),
        ("use std::{fs::write, io};", true),
        ("use std::{fs::{write}, io};", true),
        ("use std::{fs::{self, rename}, io};", true),
        ("use std::{fs::*, io};", true),
    ] {
        let found = violations(&with_prepended(&sources, "headless.rs", line));
        let imported = found
            .iter()
            .any(|sentence| sentence.starts_with("headless.rs imports `std::fs::"));
        assert_eq!(imported, refused, "`{line}`: {found:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// What the pseudo-terminal layer writes on its own.

#[test]
fn the_terminal_answers_nothing_but_a_cursor_position_request_and_only_with_a_position_report() {
    // The layer answers what the program asks of a terminal, outside the soak's choke point and
    // outside the scan of what is written to the program. So the answer must be one thing only:
    // `ESC [ row ; col R` for `ESC [ 6 n`, and nothing for any other input, whatever it asks.
    fn corpus() -> Vec<Vec<u8>> {
        let mut inputs: Vec<Vec<u8>> = Vec::new();
        for byte in 0..=u8::MAX {
            inputs.push(vec![byte]);
            inputs.push(vec![0x1b, byte]);
            inputs.push(vec![0x1b, b'[', byte]);
            inputs.push(vec![0x1b, b'[', b'?', byte]);
            inputs.push(vec![0x1b, b']', byte, 0x07]);
            inputs.push(vec![0x1b, b'P', byte, 0x1b, b'\\']);
        }
        for number in 0..=50_u32 {
            for private in ["", "?", ">"] {
                for intermediate in ["", "$"] {
                    for last in b'@'..=b'~' {
                        inputs.push(
                            format!("\x1b[{private}{number}{intermediate}{}", char::from(last))
                                .into_bytes(),
                        );
                    }
                }
            }
            inputs.push(format!("\x1b]{number};?\x07").into_bytes());
            inputs.push(format!("\x1b]{number};?\x1b\\").into_bytes());
        }
        inputs
    }
    fn is_a_position_report(reply: &[u8]) -> bool {
        let Some(body) = reply
            .strip_prefix(b"\x1b[")
            .and_then(|rest| rest.strip_suffix(b"R"))
        else {
            return false;
        };
        let parts: Vec<&[u8]> = body.split(|byte| *byte == b';').collect();
        parts.len() == 2
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.iter().all(u8::is_ascii_digit))
    }

    let mut answered = 0;
    for input in corpus() {
        let reply = crate::pty::Screen::new(24, 80).process(&input);
        if input == b"\x1b[6n" {
            assert!(is_a_position_report(&reply), "{reply:02x?}");
            answered += 1;
        } else {
            assert!(
                reply.is_empty(),
                "{input:02x?} was answered with {reply:02x?}"
            );
        }
    }
    assert_eq!(answered, 1, "the one request is in the corpus once");

    let mut screen = crate::pty::Screen::new(24, 80);
    assert_eq!(screen.process(b"\x1b[10;20H\x1b[6n"), b"\x1b[10;20R");
}

// ---------------------------------------------------------------------------------------------
// The bounded copy of a recording.

/// A writer whose bytes the test can read after the copy has taken it.
#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A reader that sets the interrupt while it is being read: the person pressing Ctrl+C in the
/// middle of a copy.
struct InterruptedWhileReading<'a> {
    data: &'a [u8],
    reads: usize,
    interrupted_by_read: usize,
    interrupt: &'a Interrupt,
}

impl Read for InterruptedWhileReading<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        if self.reads == self.interrupted_by_read {
            self.interrupt.trigger();
        }
        let length = self.data.len().min(buffer.len());
        buffer[..length].copy_from_slice(&self.data[..length]);
        self.data = &self.data[length..];
        Ok(length)
    }
}

#[test]
fn a_recording_is_copied_whole_in_chunks_when_nothing_stops_it() {
    let data: Vec<u8> = (0..10).collect();
    let sink = Sink::default();

    let copied =
        copy_bounded(&mut data.as_slice(), 4, || Ok(sink.clone()), &mut || false).expect("a copy");

    assert_eq!(copied, Copied::Whole(10));
    assert_eq!(*sink.0.borrow(), data);
}

#[test]
fn an_interrupt_that_arrives_during_the_copy_cuts_it_between_two_chunks() {
    let data: Vec<u8> = (0..20).collect();
    let interrupt = Interrupt::new();
    let sink = Sink::default();
    let mut source = InterruptedWhileReading {
        data: &data,
        reads: 0,
        interrupted_by_read: 3,
        interrupt: &interrupt,
    };

    let copied = copy_bounded(&mut source, 4, || Ok(sink.clone()), &mut || {
        saving_must_stop(&interrupt, Instant::now() + Duration::from_secs(60))
    })
    .expect("a copy");

    // The third chunk was being read when the interrupt came and was written; the fourth was
    // read to see whether anything was left, and was not.
    assert_eq!(copied, Copied::Cut(12));
    assert_eq!(*sink.0.borrow(), data[..12]);
    assert!(interrupt.is_set());
}

#[test]
fn a_stop_that_comes_after_the_last_byte_was_copied_cuts_nothing() {
    let data: Vec<u8> = (0..8).collect();
    let interrupt = Interrupt::new();
    let sink = Sink::default();
    let mut source = InterruptedWhileReading {
        data: &data,
        reads: 0,
        interrupted_by_read: 2,
        interrupt: &interrupt,
    };

    let copied = copy_bounded(&mut source, 4, || Ok(sink.clone()), &mut || {
        interrupt.is_set()
    })
    .expect("a copy");

    assert_eq!(copied, Copied::Whole(8));
    assert_eq!(*sink.0.borrow(), data);
}

#[test]
fn a_stop_before_the_first_chunk_leaves_no_file_behind() {
    let data: Vec<u8> = (0..10).collect();
    let opened = Cell::new(false);

    let copied = copy_bounded(
        &mut data.as_slice(),
        4,
        || {
            opened.set(true);
            Ok(Sink::default())
        },
        &mut || true,
    )
    .expect("a copy");

    assert_eq!(copied, Copied::Cut(0));
    assert!(
        !opened.get(),
        "the file is made when its first chunk is written"
    );
}

#[test]
fn saving_stops_at_an_interrupt_or_a_clock_that_ran_out_and_not_before() {
    let interrupt = Interrupt::new();
    let later = Instant::now() + Duration::from_secs(60);
    let earlier = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .expect("an instant in the past");

    assert!(!saving_must_stop(&interrupt, later));
    assert!(saving_must_stop(&interrupt, earlier));
    interrupt.trigger();
    assert!(saving_must_stop(&interrupt, later));
}

// ---------------------------------------------------------------------------------------------
// The copy of the binary, its digest, and the summary: each is made whole, or stopped with nothing
// left behind.

/// The names of the entries of `directory`, in order.
fn names_in(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .expect("a directory")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// `length` bytes that differ from their neighbours, so that a copy that went wrong somewhere shows
/// in what it holds.
fn bytes_of_length(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| u8::try_from(index % 251).expect("below 251"))
        .collect()
}

/// The size of the copy of the binary in `work` while it is being made, or `None` before its first
/// chunk is written.
fn size_of_the_copy(work: &Path) -> Option<u64> {
    let directory = fs::read_dir(work).ok()?.next()?.ok()?.path();
    let copy = fs::read_dir(directory).ok()?.next()?.ok()?.path();
    Some(fs::metadata(copy).ok()?.len())
}

#[test]
fn the_binary_is_copied_whole_a_chunk_at_a_time_and_its_digest_is_the_copys() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let binary = dir.path().join("excise");
    let content = bytes_of_length(2 * COPY_CHUNK + 12_345);
    fs::write(&binary, &content).expect("a binary");
    let work = dir.path().join("work");
    fs::create_dir(&work).expect("a work directory");
    let mut asked = 0;

    let copy = copy_program(&binary, None, &work, &mut || {
        asked += 1;
        None
    })
    .expect("a copy");

    assert_eq!(fs::read(&copy.path).expect("the copy"), content);
    assert!(
        asked >= 4,
        "what ends the run is asked before each of the three chunks, and at the end: {asked}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = fs::metadata(&copy.path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "private to its owner, and executable");
    }
    let digest = digest_program(&copy.path, &mut || None).expect("a digest");
    assert_eq!(
        digest,
        crate::run_support::sha256_file(&binary).expect("the digest of the original")
    );
}

#[test]
fn an_interrupt_that_comes_while_the_binary_is_being_copied_ends_the_copy_and_leaves_nothing() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let binary = dir.path().join("excise");
    fs::write(&binary, bytes_of_length(3 * COPY_CHUNK)).expect("a binary");
    let work = dir.path().join("work");
    fs::create_dir(&work).expect("a work directory");
    let interrupt = Interrupt::new();
    let mut asked = 0;
    let mut written_when_it_came = None;

    let result = copy_program(&binary, None, &work, &mut || {
        asked += 1;
        if asked == 3 {
            // The person presses Ctrl+C while the copy is under way: two chunks are on the disk.
            written_when_it_came = size_of_the_copy(&work);
            interrupt.trigger();
        }
        halt_reason(&interrupt, Instant::now() + Duration::from_secs(60))
    });

    let Err(error) = result else {
        panic!("a copy that the person interrupted went on");
    };
    assert!(matches!(error, SoakError::Interrupted), "{error:?}");
    assert_eq!(
        asked, 3,
        "the copy ended at the first look after the interrupt"
    );
    assert!(
        written_when_it_came.is_some_and(|size| size > 0),
        "the copy was under way: {written_when_it_came:?}"
    );
    assert!(
        names_in(&work).is_empty(),
        "the partial copy and its directory are gone"
    );
}

#[test]
fn a_bound_that_has_passed_ends_the_copy_before_it_starts_and_says_so() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let binary = dir.path().join("excise");
    fs::write(&binary, bytes_of_length(2 * COPY_CHUNK)).expect("a binary");
    let work = dir.path().join("work");
    fs::create_dir(&work).expect("a work directory");
    let passed = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .expect("an instant in the past");
    let interrupt = Interrupt::new();
    let mut asked = 0;

    let result = copy_program(&binary, None, &work, &mut || {
        asked += 1;
        halt_reason(&interrupt, passed)
    });

    let Err(error) = result else {
        panic!("a copy that the bound had ended went on");
    };
    assert!(matches!(error, SoakError::BoundPassed), "{error:?}");
    assert_eq!(asked, 1, "not a chunk was copied");
    assert!(
        names_in(&work).is_empty(),
        "the directory of the copy is gone"
    );
}

/// Writes to the file at `path` in place until `identity` says that it is not the file any more,
/// which it does as soon as the clock of the file system has moved on from the moment the file was
/// made (that clock ticks in steps, so one write can land in the tick of the last change). Five
/// seconds at most.
#[cfg(unix)]
fn write_in_place_until_it_shows(path: &Path, identity: FileIdentity) {
    let give_up = Instant::now() + Duration::from_secs(5);
    loop {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("opened for writing");
        file.write_all(b"\n# written in place\n").expect("written");
        drop(file);
        if identity.check(path).is_err() {
            return;
        }
        assert!(
            Instant::now() < give_up,
            "the write did not show in the identity of the file"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A binary of three chunks in `dir`, which its maker has finished with and taken the identity of,
/// and an empty work directory beside it.
#[cfg(unix)]
fn made_binary(dir: &Path) -> (std::path::PathBuf, FileIdentity, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;

    let binary = dir.join("excise");
    fs::write(&binary, bytes_of_length(3 * COPY_CHUNK)).expect("a binary");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).expect("its mode");
    let made = FileIdentity::of(&fs::File::open(&binary).expect("opened")).expect("its identity");
    let work = dir.join("work");
    fs::create_dir(&work).expect("a work directory");
    (binary, made, work)
}

#[cfg(unix)]
#[test]
fn a_binary_that_is_the_file_its_maker_made_is_copied_whole_and_the_copy_is_the_file_it_took() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (binary, made, work) = made_binary(dir.path());

    let copy = copy_program(&binary, Some(made), &work, &mut || None).expect("a copy");

    assert_eq!(
        fs::read(&copy.path).expect("the copy"),
        fs::read(&binary).expect("the binary")
    );
    // The identity of the copy was read once the copy was whole: its writes and its change of mode
    // were over, so that nothing has moved its change time since.
    assert!(copy.verify().is_ok(), "{:?}", copy.verify().err());
}

#[cfg(unix)]
#[test]
fn a_binary_that_another_file_replaced_after_it_was_made_is_refused_before_a_byte_is_copied() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (binary, made, work) = made_binary(dir.path());
    // As another build does, or as another user could: a new file is moved over it.
    let other = dir.path().join("other");
    fs::write(&other, bytes_of_length(COPY_CHUNK)).expect("another binary");
    fs::rename(&other, &binary).expect("replaced");

    let result = copy_program(&binary, Some(made), &work, &mut || None);

    let Err(SoakError::BinaryReplaced { why, .. }) = result else {
        panic!("a binary that was replaced was copied");
    };
    assert_eq!(why, "it is another file");
    assert!(
        names_in(&work).is_empty(),
        "no copy, and no directory for one"
    );
}

#[cfg(unix)]
#[test]
fn a_binary_that_was_written_in_place_after_it_was_made_is_refused_by_its_change_time() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (binary, made, work) = made_binary(dir.path());
    write_in_place_until_it_shows(&binary, made);

    let result = copy_program(&binary, Some(made), &work, &mut || None);

    let Err(SoakError::BinaryReplaced { why, .. }) = result else {
        panic!("a binary that was written in place was copied");
    };
    assert_eq!(why, "it was changed after it was made");
    assert!(names_in(&work).is_empty());
}

#[cfg(unix)]
#[test]
fn a_binary_that_is_written_while_it_is_copied_is_refused_when_the_copy_is_done() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (binary, made, work) = made_binary(dir.path());
    let mut asked = 0;

    // It is the file that was made when it is opened, and again after its first chunk is copied:
    // somebody writes to it then, so that what was copied is a mixture.
    let result = copy_program(&binary, Some(made), &work, &mut || {
        asked += 1;
        if asked == 2 {
            write_in_place_until_it_shows(&binary, made);
        }
        None
    });

    let Err(SoakError::BinaryReplaced { why, .. }) = result else {
        panic!("a binary that was written while it was copied was taken");
    };
    assert_eq!(why, "it was changed after it was made");
    assert!(asked >= 2, "the write came in the middle of the copy");
    assert!(
        names_in(&work).is_empty(),
        "the copy of a mixture is gone with its directory"
    );
}

#[cfg(unix)]
#[test]
fn a_link_put_where_the_made_binary_was_is_not_followed() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (binary, made, work) = made_binary(dir.path());
    // A file with the same bytes, which is not the file that was made, and a link to it.
    let same = dir.path().join("same");
    fs::copy(&binary, &same).expect("a file with the same bytes");
    fs::remove_file(&binary).expect("removed");
    std::os::unix::fs::symlink(&same, &binary).expect("a link");

    let result = copy_program(&binary, Some(made), &work, &mut || None);

    let Err(SoakError::Io { context, .. }) = result else {
        panic!("a link where the binary was made was followed");
    };
    assert_eq!(context, "cannot read the binary");
    assert!(names_in(&work).is_empty());
}

/// Set in the child process of the umask test, to the umask it runs under, written in octal.
#[cfg(unix)]
const UMASK_CHILD: &str = "XH_SOAK_TEST_UMASK_CHILD";

/// Set in the child process of the umask test, to the directory that it works in.
#[cfg(unix)]
const UMASK_CHILD_DIR: &str = "XH_SOAK_TEST_UMASK_CHILD_DIR";

/// The umask of a process cannot be changed by one test without changing it for every other that
/// runs at the same time in the same process, and this test needs no such change: it runs itself
/// again in a child process that a shell starts under each of the umasks, which clear what the
/// copy of the binary needs from its owner: the read bit, all three bits, the write bit, and the
/// execute bit. A file made with `0700` under any of them has less than that, which is what the
/// copy was left with before it was given its mode after it was made.
#[cfg(unix)]
#[test]
fn the_copy_of_the_binary_and_its_directory_are_exactly_private_under_a_restrictive_umask() {
    use std::{
        os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
        path::PathBuf,
        process::Command,
    };

    const NAME: &str =
        "the_copy_of_the_binary_and_its_directory_are_exactly_private_under_a_restrictive_umask";
    let mode_of = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;

    if let (Some(umask), Some(dir)) = (
        std::env::var_os(UMASK_CHILD),
        std::env::var_os(UMASK_CHILD_DIR),
    ) {
        // The child: the shell that started it set the umask.
        let umask = u32::from_str_radix(umask.to_str().expect("text"), 8).expect("an octal umask");
        let dir = PathBuf::from(dir);
        let probe = dir.join("probe");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&probe)
            .expect("a probe file");
        assert_eq!(
            mode_of(&probe),
            0o700 & !umask,
            "the umask is in force, and a file made with 0700 has less than that"
        );
        assert_ne!(mode_of(&probe), 0o700);
        let binary = dir.join("excise");

        let copy = copy_program(&binary, None, &dir.join("work"), &mut || None).expect("a copy");

        assert_eq!(mode_of(&copy.path), 0o700, "the copy");
        assert_eq!(
            mode_of(copy.path.parent().expect("a directory")),
            0o700,
            "the directory of the copy"
        );
        assert_eq!(
            digest_program(&copy.path, &mut || None).expect("a digest"),
            crate::run_support::sha256_file(&binary).expect("the digest of the original"),
            "the copy can be read"
        );
        let status = Command::new(&copy.path).status().expect("the copy runs");
        assert_eq!(status.code(), Some(7), "the copy can be run");
        println!("xh-umask-child: the copy and its directory are 0700 under umask {umask:04o}");
        return;
    }

    // The parent: one child for each umask. The binary and the work directory are made here, under
    // the umask of this process, so that the child can read the one and make the copy in the
    // other.
    let this_test = format!(
        "{}::{NAME}",
        module_path!().split_once("::").map_or("", |(_, path)| path)
    );
    for umask in ["477", "577", "277", "177"] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let binary = dir.path().join("excise");
        fs::write(&binary, "#!/bin/sh\nexit 7\n").expect("a script");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("executable");
        fs::create_dir(dir.path().join("work")).expect("a work directory");

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("umask {umask} && exec \"$0\" \"$@\""))
            .arg(std::env::current_exe().expect("the test binary"))
            .args(["--exact", &this_test, "--nocapture", "--test-threads=1"])
            .env(UMASK_CHILD, umask)
            .env(UMASK_CHILD_DIR, dir.path())
            .output()
            .expect("the test binary runs");

        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success() && said.contains("xh-umask-child: "),
            "under umask {umask}: {said}"
        );
    }
}

/// The directory that the copy of the binary is made in is private to its owner from the moment it
/// exists, and not only once it has been given its mode: a directory that is made with the default
/// has what the umask leaves of `0777`, which under the usual umask lets every user on the machine
/// enter it, and one of them who watched the scratch directory could put an entry in it, under the
/// name of the copy, before the mode was changed. The test makes the directory by the function that
/// `copy_program` makes it by, and no more than that, under the umasks that leave others a way in
/// (and shows that the default would). It runs itself again in a child process that a shell starts
/// under each umask.
#[cfg(unix)]
#[test]
fn the_directory_of_the_copy_is_private_from_the_moment_it_is_made_under_a_permissive_umask() {
    use std::{os::unix::fs::PermissionsExt as _, path::PathBuf, process::Command};

    const NAME: &str =
        "the_directory_of_the_copy_is_private_from_the_moment_it_is_made_under_a_permissive_umask";
    let mode_of = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;

    if let (Some(umask), Some(dir)) = (
        std::env::var_os(UMASK_CHILD),
        std::env::var_os(UMASK_CHILD_DIR),
    ) {
        // The child: the shell that started it set the umask.
        let umask = u32::from_str_radix(umask.to_str().expect("text"), 8).expect("an octal umask");
        let work = PathBuf::from(dir).join("work");
        let default = tempfile::Builder::new()
            .prefix("xh-default-")
            .tempdir_in(&work)
            .expect("a directory made with the default mode");
        assert_eq!(
            mode_of(default.path()),
            0o777 & !umask,
            "the umask is in force, and a directory made with the default has what it leaves"
        );
        assert_ne!(
            mode_of(default.path()) & 0o005,
            0,
            "and that lets others enter it, which is what the default would have left the copy with"
        );

        let made = directory_for_the_copy(&work).expect("a directory for the copy");

        assert_eq!(
            mode_of(made.path()),
            0o700,
            "private to its owner as it is made, before any change of mode"
        );
        println!("xh-umask-child: the directory is 0700 as it is made under umask {umask:04o}");
        return;
    }

    // The parent: one child for each umask. The work directory is made here, under the umask of
    // this process, so that the child can enter it.
    let this_test = format!(
        "{}::{NAME}",
        module_path!().split_once("::").map_or("", |(_, path)| path)
    );
    for umask in ["000", "002", "022"] {
        let dir = tempfile::tempdir().expect("a temporary directory");
        fs::create_dir(dir.path().join("work")).expect("a work directory");

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("umask {umask} && exec \"$0\" \"$@\""))
            .arg(std::env::current_exe().expect("the test binary"))
            .args(["--exact", &this_test, "--nocapture", "--test-threads=1"])
            .env(UMASK_CHILD, umask)
            .env(UMASK_CHILD_DIR, dir.path())
            .output()
            .expect("the test binary runs");

        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success() && said.contains("xh-umask-child: "),
            "under umask {umask}: {said}"
        );
    }
}

#[test]
fn the_interrupt_is_named_before_the_bound_when_both_have_come() {
    let interrupt = Interrupt::new();
    let later = Instant::now() + Duration::from_secs(60);
    let passed = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .expect("an instant in the past");

    assert!(halt_reason(&interrupt, later).is_none());
    assert!(matches!(
        halt_reason(&interrupt, passed),
        Some(SoakError::BoundPassed)
    ));
    interrupt.trigger();
    assert!(matches!(
        halt_reason(&interrupt, later),
        Some(SoakError::Interrupted)
    ));
    assert!(matches!(
        halt_reason(&interrupt, passed),
        Some(SoakError::Interrupted)
    ));
}

#[test]
fn the_digest_of_the_copy_is_taken_a_chunk_at_a_time_and_a_stop_between_two_ends_it() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let copy = dir.path().join("excise");
    fs::write(&copy, bytes_of_length(200_000)).expect("a binary");
    let mut asked = 0;

    let result = digest_program(&copy, &mut || {
        asked += 1;
        (asked == 3).then_some(SoakError::BoundPassed)
    });

    let Err(error) = result else {
        panic!("a digest that the bound had ended went on: {result:?}");
    };
    assert!(matches!(error, SoakError::BoundPassed), "{error:?}");
    assert_eq!(asked, 3, "two chunks were hashed, and the third was not");
}

#[test]
fn a_summary_that_cannot_be_written_whole_is_never_published_and_leaves_nothing() {
    let dir = tempfile::tempdir().expect("a temporary directory");

    // The disk fills up in the middle of the document.
    let result = publish(dir.path(), "summary.json", |file| {
        file.write_all(b"{\n  \"document_kind\": \"harness-soak\",\n  \"run_id\": ")?;
        Err(io::Error::other("the disk is full"))
    });

    let error = result.expect_err("the write failed");
    assert_eq!(error.to_string(), "the disk is full");
    assert_eq!(
        names_in(dir.path()),
        Vec::<String>::new(),
        "neither a summary.json that is not whole, nor the temporary file it was written in"
    );
}

#[test]
fn a_summary_that_is_written_whole_is_published_under_its_name_and_only_that() {
    let dir = tempfile::tempdir().expect("a temporary directory");

    publish(dir.path(), "summary.json", |file| file.write_all(b"{}\n")).expect("published");

    assert_eq!(names_in(dir.path()), ["summary.json"]);
    let path = dir.path().join("summary.json");
    assert_eq!(fs::read(&path).expect("the summary"), b"{}\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "private to its owner");
    }
}

#[test]
fn a_summary_is_never_published_over_what_is_already_under_its_name() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let in_the_way = dir.path().join("summary.json");

    // A directory is there.
    fs::create_dir(&in_the_way).expect("a directory");
    let error = publish(dir.path(), "summary.json", |file| file.write_all(b"{}\n"))
        .expect_err("a directory is in the way");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
    assert!(in_the_way.is_dir(), "the directory is as it was");
    assert_eq!(
        names_in(dir.path()),
        ["summary.json"],
        "no temporary file is left"
    );
    fs::remove_dir(&in_the_way).expect("cleaned up");

    // A file is there: it is not replaced, whatever it holds.
    fs::write(&in_the_way, b"an earlier document\n").expect("a file");
    let error = publish(dir.path(), "summary.json", |file| file.write_all(b"{}\n"))
        .expect_err("a file is in the way");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
    assert_eq!(
        fs::read(&in_the_way).expect("the file"),
        b"an earlier document\n"
    );
    assert_eq!(
        names_in(dir.path()),
        ["summary.json"],
        "no temporary file is left"
    );
}

/// Runs of the soak against programs that are shell scripts, so that every way a run can go is made
/// to happen with nothing but a temporary tree.
#[cfg(unix)]
mod runs {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        os::unix::fs::{DirBuilderExt as _, PermissionsExt as _},
        path::{Path, PathBuf},
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use super::names_in;
    use crate::{
        report::{
            Document as _, HarnessSoak, QuirkKind, SoakOutcome, SoakPhase, SoakRounds, tui::ExitVia,
        },
        safety::{FileIdentity, Untrusted},
        soak::{
            Interrupt, Limits, SoakError, SoakReport, SoakRequest, SoakRoot, check, run_soak,
            write_results,
        },
    };

    /// A temporary tree to soak, a work directory, and the scripts that stand in for `excise`.
    /// Dropping it ends every sleeper that its scripts wrote down, so that a test that fails
    /// leaves nothing running.
    ///
    /// What the scripts start in the background is `sleeper`, a link to `sleep` that belongs to
    /// this world alone, and not `sleep` itself. A process id says nothing of who has it once its
    /// process is gone, since another process can be given it, a `sleep` of somebody else's
    /// included. The command line of a process says what it was started as, so it is what a
    /// process is told to be a sleeper of this world by ([`is_running_as`]).
    struct World {
        dir: tempfile::TempDir,
    }

    impl World {
        fn new() -> Self {
            let dir = tempfile::Builder::new()
                .prefix("xt-soak-")
                .tempdir()
                .expect("a temporary directory");
            let tree = dir.path().join("tree/Zebra-Quarterly-Ledger");
            fs::create_dir_all(&tree).expect("a tree");
            fs::write(tree.join("Payroll-7731.dat"), b"x").expect("a file");
            // Private to its owner whatever the umask is: the soak refuses a scratch directory that
            // a group or everybody can write.
            fs::DirBuilder::new()
                .mode(0o700)
                .create(dir.path().join("work"))
                .expect("a work directory");
            std::os::unix::fs::symlink("/bin/sleep", dir.path().join("sleeper"))
                .expect("a link to sleep");
            Self { dir }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        /// The command that the scripts start in the background: a link to `sleep` in this world's
        /// directory, which no other process is started as.
        fn sleeper(&self) -> PathBuf {
            self.path("sleeper")
        }

        /// An executable script, standing in for `excise`.
        fn script(&self, body: &str) -> PathBuf {
            let path = self.path("excise");
            fs::write(&path, format!("#!/bin/sh\n{body}")).expect("a script");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("executable");
            path
        }

        /// A program that starts a child in its own process group and then hangs, writing the
        /// child's process id to `pids`. With `only_in_a_terminal` a headless scan (`--format
        /// json ...`) ends at once instead.
        fn hang(&self, only_in_a_terminal: bool) -> PathBuf {
            let skip = if only_in_a_terminal {
                "[ \"$1\" = --format ] && exit 0\n"
            } else {
                ""
            };
            self.script(&format!(
                "{skip}'{}' 600 &\necho $! >> '{}'\nwait\n",
                self.sleeper().display(),
                self.path("pids").display()
            ))
        }

        /// The process ids the scripts wrote down.
        fn pids(&self) -> Vec<u32> {
            fs::read_to_string(self.path("pids"))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()
        }
    }

    impl Drop for World {
        /// Ends what the scripts left running. A run that the behavior under test failed to end,
        /// or an assertion that fails before `assert_gone`, must not leave a sleeper behind for ten
        /// minutes when the temporary directory goes. Only a process that is running as this
        /// world's sleeper is signalled: a recorded id that another process has taken is not.
        fn drop(&mut self) {
            let sleeper = self.sleeper();
            for pid in self.pids() {
                if is_running_as(&sleeper, pid) {
                    let _ = crate::safety::kill_process(pid);
                }
            }
        }
    }

    /// What a test varies about a soak of the world's tree.
    struct Soak<'a> {
        binary: &'a Path,
        /// Where the run's directory is made; `out` beside the tree when none.
        out_root: Option<PathBuf>,
        rounds: u32,
        limits: Limits,
        record: bool,
    }

    impl<'a> Soak<'a> {
        fn new(binary: &'a Path) -> Self {
            Self {
                binary,
                out_root: None,
                rounds: 1,
                limits: Limits::default(),
                record: false,
            }
        }

        fn run(
            &self,
            world: &World,
            interrupt: &Interrupt,
            progress: &mut dyn FnMut(&str),
        ) -> Result<SoakReport, SoakError> {
            let root = SoakRoot::open(world.path("tree")).expect("a root");
            let out_root = self.out_root.clone().unwrap_or_else(|| world.path("out"));
            run_soak(
                &SoakRequest {
                    root: &root,
                    binary: self.binary,
                    binary_identity: None,
                    out_root: &out_root,
                    work_dir: &world.path("work"),
                    rounds: self.rounds,
                    limits: self.limits,
                    started: Instant::now(),
                    record: self.record,
                    git_sha: &"0".repeat(40),
                    interrupt,
                },
                progress,
            )
        }
    }

    fn soak(
        world: &World,
        binary: &Path,
        rounds: u32,
        limits: Limits,
        record: bool,
        interrupt: &Interrupt,
    ) -> Result<SoakReport, SoakError> {
        Soak {
            rounds,
            limits,
            record,
            ..Soak::new(binary)
        }
        .run(world, interrupt, &mut |_| {})
    }

    /// Every entry below `root` by relative path, with what identifies it: `directory`, a file's
    /// bytes, or a link's target.
    fn state_of(root: &Path) -> BTreeMap<String, String> {
        fn walk(dir: &Path, relative: &str, into: &mut BTreeMap<String, String>) {
            for entry in fs::read_dir(dir).expect("a directory") {
                let entry = entry.expect("an entry");
                let name = entry.file_name().to_string_lossy().into_owned();
                let key = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                let kind = entry.file_type().expect("a file type");
                if kind.is_symlink() {
                    let target = fs::read_link(entry.path()).expect("a link");
                    into.insert(key, format!("link to {}", target.display()));
                } else if kind.is_dir() {
                    into.insert(key.clone(), "directory".to_owned());
                    walk(&entry.path(), &key, into);
                } else {
                    let bytes = fs::read(entry.path()).expect("a file");
                    into.insert(key, format!("file: {}", String::from_utf8_lossy(&bytes)));
                }
            }
        }
        let mut into = BTreeMap::new();
        walk(root, "", &mut into);
        into
    }

    /// Whether the process is there and is not a zombie.
    fn alive(pid: u32) -> bool {
        let output = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .stderr(Stdio::null())
            .output()
            .expect("ps runs");
        let state = String::from_utf8_lossy(&output.stdout);
        output.status.success() && !state.trim().is_empty() && !state.trim().starts_with('Z')
    }

    /// Whether the process `pid` is running and was started as `sleeper`: its command line says
    /// what it was started as, and a process that has taken an id since (a `sleep` too, when it
    /// was not started as this one) says something else. A zombie, which has ended, is not
    /// running.
    fn is_running_as(sleeper: &Path, pid: u32) -> bool {
        Command::new("ps")
            .args(["-ww", "-o", "stat=", "-o", "args=", "-p", &pid.to_string()])
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|output| {
                let text = String::from_utf8_lossy(&output.stdout);
                let text = text.trim();
                output.status.success()
                    && !text.starts_with('Z')
                    && text.contains(&*sleeper.to_string_lossy())
            })
    }

    /// Waits until the process `pid` is running as `sleeper`: a process that was started a moment
    /// ago has the command line of the program that started it until it has run its own.
    fn wait_until_running_as(sleeper: &Path, pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !is_running_as(sleeper, pid) {
            assert!(
                Instant::now() < deadline,
                "the process {pid} did not come up as {}",
                sleeper.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Fails unless every one of `pids` has ended, or is somebody else's now, within `within`: none
    /// is running as a sleeper of `world`.
    fn assert_gone_within(world: &World, pids: &[u32], within: Duration) {
        let sleeper = world.sleeper();
        let deadline = Instant::now() + within;
        while pids.iter().any(|&pid| is_running_as(&sleeper, pid)) {
            assert!(
                Instant::now() < deadline,
                "a program the soak started is still running: {pids:?}"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn assert_gone(world: &World, pids: &[u32]) {
        assert_gone_within(world, pids, Duration::from_secs(10));
    }

    /// A process that a test started, ended and reaped when it is dropped, so that a test that
    /// fails leaves nothing running.
    struct Started(std::process::Child);

    impl Started {
        fn sleep(program: &Path, seconds: &str) -> Self {
            Self(
                Command::new(program)
                    .arg(seconds)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("sleep starts"),
            )
        }

        fn id(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Started {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn dropping_the_world_ends_the_processes_its_scripts_left_running() {
        let world = World::new();
        let script = world.script(&format!(
            "'{}' 600 &\necho $! >> '{}'\n",
            world.sleeper().display(),
            world.path("pids").display()
        ));
        let status = Command::new(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("the script runs");
        assert!(status.success());
        let pids = world.pids();
        assert_eq!(pids.len(), 1, "the script wrote down its sleeper");
        wait_until_running_as(&world.sleeper(), pids[0]);
        let sleeper = world.sleeper();

        drop(world);

        let deadline = Instant::now() + Duration::from_secs(10);
        while is_running_as(&sleeper, pids[0]) {
            assert!(Instant::now() < deadline, "the sleeper is still running");
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn dropping_the_world_leaves_a_sleep_that_its_scripts_did_not_start_alone() {
        let world = World::new();
        // Stands for a `sleep` of somebody else's that has taken a recorded id since the program
        // that had it ended: the same program under the same name, and not the same process.
        let other = Started::sleep(Path::new("/bin/sleep"), "598");
        fs::write(world.path("pids"), format!("{}\n", other.id())).expect("a record of the id");

        drop(world);

        assert!(
            alive(other.id()),
            "a sleep that the scripts did not start was killed"
        );
    }

    #[test]
    fn dropping_the_world_leaves_a_process_that_is_not_a_sleep_alone() {
        let world = World::new();
        // Stands for a process that has taken a recorded id since the program that had it ended.
        let mut other = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cat starts");
        fs::write(world.path("pids"), format!("{}\n", other.id())).expect("a record of the id");

        drop(world);

        let left_alone = alive(other.id());
        other.kill().expect("cat is ended");
        other.wait().expect("cat is reaped");
        assert!(left_alone, "a process that is not a sleep was killed");
    }

    #[test]
    fn a_process_is_a_sleeper_of_the_world_by_the_command_it_was_started_as_and_by_nothing_else() {
        let world = World::new();
        let sleeper = world.sleeper();
        let mut ours = Started::sleep(&sleeper, "598");
        let somebody_elses = Started::sleep(Path::new("/bin/sleep"), "597");
        wait_until_running_as(&sleeper, ours.id());

        assert!(
            alive(somebody_elses.id()) && !is_running_as(&sleeper, somebody_elses.id()),
            "a sleep that is running is not this world's by being a sleep"
        );
        assert!(!is_running_as(&sleeper, std::process::id()));
        // `assert_gone` is still a check: it fails for a sleeper of the world that is running...
        let waited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_gone_within(&world, &[ours.id()], Duration::from_millis(200));
        }));
        assert!(waited.is_err(), "a running sleeper was taken for gone");
        // ...and passes for an id that something else has, which is gone as far as the world goes.
        assert_gone_within(&world, &[somebody_elses.id()], Duration::from_millis(200));
        ours.0.kill().expect("the sleeper is ended");
        ours.0.wait().expect("the sleeper is reaped");
        assert_gone_within(&world, &[ours.id()], Duration::from_secs(10));
    }

    #[test]
    fn a_program_that_hangs_is_killed_with_its_process_group_at_the_bound_and_the_quirk_log_says_so()
     {
        let world = World::new();
        let binary = world.hang(false);
        let limits = Limits {
            headless_scan: Duration::from_secs(1),
            first_frame: Duration::from_secs(1),
            ..Limits::for_run(Duration::from_mins(5))
        };

        let report = soak(&world, &binary, 1, limits, false, &Interrupt::new()).expect("a soak");

        assert_eq!(report.outcome(), SoakOutcome::Finished);
        let scan = &report.document.headless[0];
        assert!(scan.timed_out, "the scan passed its bound");
        assert_eq!(scan.signal, Some(9), "and was killed");
        assert_eq!(report.document.tui.len(), 2);
        for session in &report.document.tui {
            assert_eq!(session.timed_out_phase, Some(SoakPhase::FirstFrame));
            assert_eq!(session.exit.via, ExitVia::Killed);
        }
        let timeouts = report
            .quirks
            .quirks()
            .iter()
            .filter(|quirk| quirk.kind == QuirkKind::Timeout)
            .count();
        assert_eq!(timeouts, 3, "one for the scan and one for each session");
        let text = fs::read_to_string(report.run_dir.join("quirks.txt")).expect("quirks.txt");
        assert!(
            text.contains("[timeout] round 1, headless default: the scan passed its bound of 1 s"),
            "{text}"
        );
        assert!(
            text.contains("round 1, tui deterministic: the first_frame phase passed its bound")
                && text.contains("process group was killed"),
            "{text}"
        );
        // The child each script started is in the script's process group, and it is gone too.
        let pids = world.pids();
        assert_eq!(pids.len(), 3);
        assert_gone(&world, &pids);
    }

    #[test]
    fn an_interrupt_ends_the_run_where_it_waits_and_kills_what_it_started() {
        for only_in_a_terminal in [false, true] {
            let world = World::new();
            let binary = world.hang(only_in_a_terminal);
            let interrupt = Interrupt::new();
            let started = Instant::now();

            let report = thread::scope(|scope| {
                scope.spawn(|| {
                    thread::sleep(Duration::from_millis(700));
                    interrupt.trigger();
                });
                soak(
                    &world,
                    &binary,
                    1,
                    Limits::for_run(Duration::from_mins(10)),
                    false,
                    &interrupt,
                )
            })
            .expect("a soak");

            assert!(
                started.elapsed() < Duration::from_secs(60),
                "only the interrupt could end a run with no bound in sight"
            );
            assert_eq!(report.outcome(), SoakOutcome::Interrupted);
            assert_eq!(report.document.rounds.completed, 0);
            assert_eq!(
                report.document.tui.is_empty(),
                !only_in_a_terminal,
                "the scan was cut short, or the session was"
            );
            assert_gone(&world, &world.pids());

            // What finished is still written, and the document says how the run ended.
            let written = fs::read_to_string(report.run_dir.join("summary.json"))
                .expect("summary.json is written");
            assert_eq!(
                HarnessSoak::from_json_str(&written)
                    .expect("a document")
                    .outcome,
                SoakOutcome::Interrupted
            );
            assert!(report.run_dir.join("quirks.txt").is_file());
        }
    }

    #[test]
    fn a_request_that_cannot_be_carried_out_is_refused_before_anything_starts() {
        let world = World::new();
        let marker = world.path("started");
        let binary = world.script(&format!("echo started >> '{}'\n", marker.display()));
        let interrupt = Interrupt::new();

        let no_rounds = soak(&world, &binary, 0, Limits::default(), false, &interrupt);
        assert!(matches!(no_rounds, Err(SoakError::Request(_))));

        let root = SoakRoot::open(world.path("tree")).expect("a root");
        let inside = world.path("tree/work");
        fs::create_dir(&inside).expect("a directory inside the tree");
        let scratch_inside_the_root = run_soak(
            &SoakRequest {
                root: &root,
                binary: &binary,
                binary_identity: None,
                out_root: &world.path("out"),
                work_dir: &inside,
                rounds: 1,
                limits: Limits::default(),
                started: Instant::now(),
                record: false,
                git_sha: &"0".repeat(40),
                interrupt: &interrupt,
            },
            &mut |_| {},
        );
        assert!(matches!(
            scratch_inside_the_root,
            Err(SoakError::ScratchInsideRoot(_))
        ));

        assert!(!marker.exists(), "the program was started");
        assert!(!world.path("out").exists(), "something was written");
    }

    #[test]
    fn a_scratch_directory_that_others_can_change_is_refused_before_anything_starts() {
        let world = World::new();
        let marker = world.path("started");
        let binary = world.script(&format!("echo started >> '{}'\n", marker.display()));
        let interrupt = Interrupt::new();
        let work = world.path("work");
        let root = SoakRoot::open(world.path("tree")).expect("a root");
        let out_root = world.path("out");
        let request = SoakRequest {
            root: &root,
            binary: &binary,
            binary_identity: None,
            out_root: &out_root,
            work_dir: &work,
            rounds: 1,
            limits: Limits::default(),
            started: Instant::now(),
            record: false,
            git_sha: &"0".repeat(40),
            interrupt: &interrupt,
        };

        // Everybody can write it, and it is not sticky: anybody can rename what is in it.
        fs::set_permissions(&work, fs::Permissions::from_mode(0o777)).expect("opened up");
        let open = run_soak(&request, &mut |_| {});

        let Err(SoakError::UntrustedScratch(refusal)) = open else {
            panic!("a scratch directory that everybody can write was taken: {open:?}");
        };
        assert_eq!(refusal.why, Untrusted::WorldWritable);
        assert!(
            open_text(&refusal.to_string()).contains("can be written by everybody"),
            "{refusal}"
        );
        assert!(!marker.exists(), "the program was started");
        assert!(!world.path("out").exists(), "something was written");
        assert!(
            names_in(&work).is_empty(),
            "a copy of the binary or a scratch area was made"
        );
        // The same for the group, and not for a sticky directory, which stops another user from
        // renaming what a person made in it, and not for a private one.
        fs::set_permissions(&work, fs::Permissions::from_mode(0o770)).expect("opened to the group");
        assert!(matches!(
            check(&request),
            Err(SoakError::UntrustedScratch(refusal)) if refusal.why == Untrusted::GroupWritable
        ));
        for mode in [0o1777, 0o700] {
            fs::set_permissions(&work, fs::Permissions::from_mode(mode)).expect("its mode");
            assert!(check(&request).is_ok(), "{mode:o}");
        }
    }

    /// `text` as it reads when a line was broken anywhere.
    fn open_text(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn a_copy_of_the_program_that_something_replaces_between_two_phases_is_not_run_again() {
        let world = World::new();
        let ran = world.path("replacement-ran");
        // The headless scan replaces its own copy, by a rename, with a program that records that it
        // ran. The copy is made in a directory that only the soak can write, so only the program
        // itself can do this: it stands for what another user could do to a scratch directory
        // that others can change, which the soak refuses, and the second line behind that.
        let binary = world.script(&format!(
            "if [ \"$1\" = --format ]; then\n\
             printf '#!/bin/sh\\necho ran >> {}\\n' > \"$0.new\"\n\
             chmod 755 \"$0.new\"\n\
             mv -f \"$0.new\" \"$0\"\n\
             exit 0\n\
             fi\n",
            ran.display()
        ));

        let report = soak(
            &world,
            &binary,
            1,
            Limits::default(),
            false,
            &Interrupt::new(),
        )
        .expect("a soak");

        assert_eq!(report.outcome(), SoakOutcome::Failed);
        let failure = report.failure.as_deref().expect("a failure");
        assert!(
            failure.contains("is not the file the soak made (it is another file)")
                && failure.contains("nothing more was run"),
            "{failure}"
        );
        assert_eq!(report.document.headless.len(), 1, "the scan ran");
        assert!(report.document.tui.is_empty(), "no session was started");
        assert!(!ran.exists(), "the program that replaced the copy was run");
    }

    #[test]
    fn a_copy_of_the_program_that_something_writes_to_between_two_phases_is_not_run_again() {
        let world = World::new();
        let ran = world.path("rewritten-ran");
        // The headless scan writes to its own copy in place: it appends a line that records that
        // it ran, over a little more than a second, which is longer than the step of any file
        // system's clock. The file stays the same file, with the same owner and the same mode, so
        // only its change time says that it was written. The copy is made in a directory that only
        // the soak can write, so only the program itself can do this, as above.
        let binary = world.script(&format!(
            "if [ \"$1\" = --format ]; then\n\
             i=0\n\
             while [ $i -lt 12 ]; do\n\
             echo 'echo ran >> {}' >> \"$0\"\n\
             sleep 0.1\n\
             i=$((i + 1))\n\
             done\n\
             exit 0\n\
             fi\n",
            ran.display()
        ));

        let report = soak(
            &world,
            &binary,
            1,
            Limits::default(),
            false,
            &Interrupt::new(),
        )
        .expect("a soak");

        assert_eq!(report.outcome(), SoakOutcome::Failed);
        let failure = report.failure.as_deref().expect("a failure");
        assert!(
            failure.contains("is not the file the soak made (it was changed after it was made)")
                && failure.contains("nothing more was run"),
            "{failure}"
        );
        assert_eq!(report.document.headless.len(), 1, "the scan ran");
        assert!(report.document.tui.is_empty(), "no session was started");
        assert!(
            !ran.exists(),
            "the program that was written to was run again"
        );
    }

    #[test]
    fn a_scan_whose_report_grows_past_its_cap_is_killed_with_its_process_group_and_the_log_says_so()
    {
        let world = World::new();
        // The scan writes four mebibytes to its report (the fourth argument) and then hangs, with
        // a child in its process group.
        let binary = world.script(&format!(
            "if [ \"$1\" = --format ]; then\n\
             head -c 4194304 /dev/zero > \"$4\"\n\
             '{}' 600 &\n\
             echo $! >> '{}'\n\
             wait\n\
             fi\n\
             exit 0\n",
            world.sleeper().display(),
            world.path("pids").display()
        ));
        let limits = Limits {
            // Without the cap, this scan ends at its bound, as a timeout.
            headless_scan: Duration::from_secs(8),
            report_bytes: 1 << 20,
            ..Limits::for_run(Duration::from_mins(5))
        };
        let started = Instant::now();

        let report = soak(&world, &binary, 1, limits, false, &Interrupt::new()).expect("a soak");

        let scan = &report.document.headless[0];
        assert!(
            started.elapsed() < Duration::from_secs(7),
            "the scan was not cut short at its report: {:?}",
            started.elapsed()
        );
        assert!(!scan.timed_out, "the cap ended the scan, and not the bound");
        assert_eq!(scan.signal, Some(9), "it was killed");
        assert!(scan.report.is_none(), "no report was read");
        assert_eq!(
            report.outcome(),
            SoakOutcome::Finished,
            "a soak gates nothing"
        );
        let text = fs::read_to_string(report.run_dir.join("quirks.txt")).expect("quirks.txt");
        assert!(
            text.contains(
                "[report_too_large] round 1, headless default: the scan's report passed 1048576 \
                 bytes in its scratch area"
            ),
            "{text}"
        );
        assert!(
            !text.contains("[timeout] round 1, headless"),
            "the bound did not pass: {text}"
        );
        let counted: u64 = report
            .document
            .quirks
            .iter()
            .filter(|count| count.kind == QuirkKind::ReportTooLarge)
            .map(|count| count.count)
            .sum();
        assert_eq!(counted, 1);
        // The child that the script started is in its process group, and it is gone too, and the
        // report went with the scratch area.
        let pids = world.pids();
        assert_eq!(pids.len(), 1);
        assert_gone(&world, &pids);
        assert!(names_in(&world.path("work")).is_empty());
    }

    #[test]
    fn a_bound_too_long_for_the_clock_to_count_is_refused_before_anything_starts() {
        let world = World::new();
        let marker = world.path("started");
        let binary = world.script(&format!("echo started >> '{}'\n", marker.display()));
        let interrupt = Interrupt::new();

        let refused = soak(
            &world,
            &binary,
            1,
            Limits::for_run(Duration::MAX),
            false,
            &interrupt,
        );

        let Err(SoakError::Request(text)) = refused else {
            panic!("a bound that cannot be counted was taken: {refused:?}");
        };
        assert!(text.contains("too long to count"), "{text}");
        assert!(!marker.exists(), "the program was started");
        assert!(!world.path("out").exists(), "something was written");
        assert!(
            fs::read_dir(world.path("work"))
                .expect("the work directory")
                .next()
                .is_none(),
            "a copy of the binary or a scratch area was made"
        );
    }

    #[test]
    fn a_commit_that_is_not_one_is_refused_before_anything_starts() {
        let world = World::new();
        let marker = world.path("started");
        let binary = world.script(&format!("echo started >> '{}'\n", marker.display()));
        let root = SoakRoot::open(world.path("tree")).expect("a root");
        let interrupt = Interrupt::new();

        for git_sha in [
            "/Users/x/Private/Payroll-7731".to_owned(),
            "0123456789ABCDEF0123456789ABCDEF01234567".to_owned(),
            "0".repeat(39),
            "0".repeat(41),
            String::new(),
        ] {
            let refused = run_soak(
                &SoakRequest {
                    root: &root,
                    binary: &binary,
                    binary_identity: None,
                    out_root: &world.path("out"),
                    work_dir: &world.path("work"),
                    rounds: 1,
                    limits: Limits::default(),
                    started: Instant::now(),
                    record: false,
                    git_sha: &git_sha,
                    interrupt: &interrupt,
                },
                &mut |_| {},
            );
            let Err(SoakError::Request(text)) = refused else {
                panic!("`{git_sha}` was taken for a commit: {refused:?}");
            };
            assert!(
                !text.contains("Payroll"),
                "the refusal repeats what it was given: {text}"
            );
        }

        assert!(!marker.exists(), "the program was started");
        assert!(!world.path("out").exists(), "something was written");
    }

    #[test]
    fn a_binary_that_is_replaced_while_the_soak_runs_is_not_the_one_that_runs() {
        let world = World::new();
        let binary = world.path("excise");
        let started = world.path("original-ran");
        let swapped = world.path("swapped-ran");
        // What cargo does to a binary it builds again: a new file is written beside it, made
        // executable, and moved over it. The first scan does it, while the soak runs.
        let original = world.script(&format!(
            "echo started >> '{started}'\n\
             if [ \"$1\" = --format ]; then\n\
             echo '#!/bin/sh' > '{binary}.new'\n\
             echo \"echo started >> '{swapped}'\" >> '{binary}.new'\n\
             chmod +x '{binary}.new'\n\
             mv '{binary}.new' '{binary}'\n\
             fi\n\
             exit 3\n",
            started = started.display(),
            swapped = swapped.display(),
            binary = binary.display(),
        ));
        assert_eq!(original, binary, "the script is where the test replaces it");
        let digest = crate::run_support::sha256_file(&original).expect("the digest of the script");

        let report = soak(
            &world,
            &original,
            1,
            Limits::default(),
            false,
            &Interrupt::new(),
        )
        .expect("a soak");

        assert!(
            !swapped.exists(),
            "the program that replaced the original ran"
        );
        let starts = fs::read_to_string(&started)
            .expect("the original ran")
            .lines()
            .count();
        assert_eq!(
            starts,
            report.document.headless.len() + report.document.tui.len(),
            "the original is started once by every scan and session"
        );
        assert_eq!(
            report.document.excise_sha256, digest,
            "the digest is that of the original, the file that ran"
        );
        let left: Vec<String> = fs::read_dir(world.path("work"))
            .expect("the work directory")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with("xh-soak-bin-"))
            .collect();
        assert!(left.is_empty(), "the copy of the binary stays: {left:?}");
    }

    /// The identity of the file at `path`, as the maker of a binary takes it, from the open file.
    fn identity_of(path: &Path) -> FileIdentity {
        FileIdentity::of(&fs::File::open(path).expect("the file opens")).expect("its identity")
    }

    /// A soak of the world's tree with a binary that its maker made and took the identity of.
    fn soak_made(
        world: &World,
        binary: &Path,
        identity: FileIdentity,
    ) -> Result<SoakReport, SoakError> {
        let root = SoakRoot::open(world.path("tree")).expect("a root");
        run_soak(
            &SoakRequest {
                root: &root,
                binary,
                binary_identity: Some(identity),
                out_root: &world.path("out"),
                work_dir: &world.path("work"),
                rounds: 1,
                limits: Limits::default(),
                started: Instant::now(),
                record: false,
                git_sha: &"0".repeat(40),
                interrupt: &Interrupt::new(),
            },
            &mut |_| {},
        )
    }

    #[test]
    fn a_binary_that_is_the_file_its_maker_made_is_soaked() {
        let world = World::new();
        let ran = world.path("ran");
        let binary = world.script(&format!("echo ran >> '{}'\nexit 0\n", ran.display()));
        let identity = identity_of(&binary);

        let report = soak_made(&world, &binary, identity).expect("a soak");

        assert_eq!(report.outcome(), SoakOutcome::Finished);
        assert!(ran.exists(), "the program ran");
    }

    #[test]
    fn a_binary_that_was_replaced_or_written_after_it_was_made_is_refused_and_nothing_runs() {
        let world = World::new();
        let ran = world.path("ran");
        let binary = world.script(&format!("echo made >> '{}'\n", ran.display()));
        let identity = identity_of(&binary);
        // Written in place first: the same inode, owner, and mode, and another change time.
        let give_up = Instant::now() + Duration::from_secs(5);
        while identity.check(&binary).is_ok() {
            let mut script = fs::read(&binary).expect("the script");
            script.extend_from_slice(b"# written in place\n");
            fs::write(&binary, script).expect("rewritten");
            assert!(Instant::now() < give_up, "the write did not show");
            thread::sleep(Duration::from_millis(10));
        }

        let written = soak_made(&world, &binary, identity);

        let Err(SoakError::BinaryReplaced { why, .. }) = written else {
            panic!("a binary that was written in place was soaked: {written:?}");
        };
        assert_eq!(why, "it was changed after it was made");
        // Then another file is moved over it.
        let other = world.path("other");
        fs::write(
            &other,
            format!("#!/bin/sh\necho replaced >> '{}'\n", ran.display()),
        )
        .expect("a script");
        fs::set_permissions(&other, fs::Permissions::from_mode(0o755)).expect("executable");
        fs::rename(&other, &binary).expect("replaced");

        let replaced = soak_made(&world, &binary, identity);

        let Err(error @ SoakError::BinaryReplaced { .. }) = replaced else {
            panic!("a binary that was replaced was soaked: {replaced:?}");
        };
        let text = error.to_string();
        assert!(
            text.contains("is not the file that was made for the soak (it is another file)")
                && text.contains("so nothing was run"),
            "{text}"
        );
        assert!(!ran.exists(), "a program ran");
        assert!(!world.path("out").exists(), "something was written");
        assert!(
            names_in(&world.path("work")).is_empty(),
            "a copy of the binary or a scratch area was made"
        );
    }

    #[test]
    fn a_scratch_directory_below_one_that_others_can_change_is_refused_for_the_one_above() {
        // The shape of a build sandbox whose temporary directory belongs to another user: the
        // scratch directory is the person's own and private, and it is a directory above it that
        // decides.
        let world = World::new();
        let marker = world.path("started");
        let binary = world.script(&format!("echo started >> '{}'\n", marker.display()));
        let open = world.path("open");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&open)
            .expect("a directory");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).expect("opened up");
        let work = open.join("work");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&work)
            .expect("a private directory below it");
        let root = SoakRoot::open(world.path("tree")).expect("a root");

        let refused = run_soak(
            &SoakRequest {
                root: &root,
                binary: &binary,
                binary_identity: None,
                out_root: &world.path("out"),
                work_dir: &work,
                rounds: 1,
                limits: Limits::default(),
                started: Instant::now(),
                record: false,
                git_sha: &"0".repeat(40),
                interrupt: &Interrupt::new(),
            },
            &mut |_| {},
        );

        let Err(error @ SoakError::UntrustedScratch(_)) = refused else {
            panic!("a scratch directory below an open one was taken: {refused:?}");
        };
        let SoakError::UntrustedScratch(refusal) = &error else {
            unreachable!("matched above");
        };
        assert_eq!(refusal.why, Untrusted::WorldWritable);
        assert_eq!(
            refusal.directory,
            fs::canonicalize(&open).expect("a canonical path"),
            "it is the directory above the scratch directory that is named"
        );
        // What the refusal says names the way out, and the way out that suits the reason.
        let text = open_text(&error.to_string());
        assert!(
            text.contains("the scratch directory is not private to you")
                && text.contains("EXCISE_E2E_TMPDIR")
                && text.contains("make it private (`chmod go-w` is the usual fix)"),
            "{text}"
        );
        assert!(!marker.exists(), "the program was started");
        assert!(!world.path("out").exists(), "something was written");
        assert!(names_in(&work).is_empty(), "a scratch area was made");
    }

    #[test]
    fn a_program_that_fails_is_a_quirk_and_the_soak_still_finishes_every_round() {
        let world = World::new();
        let binary = world.script("exit 3\n");

        let report = soak(
            &world,
            &binary,
            2,
            Limits::default(),
            true,
            &Interrupt::new(),
        )
        .expect("a soak");

        assert_eq!(report.outcome(), SoakOutcome::Finished);
        assert_eq!(
            report.document.rounds,
            SoakRounds {
                requested: 2,
                completed: 2
            }
        );
        assert_eq!(report.document.headless.len(), 2);
        assert_eq!(report.document.tui.len(), 4);
        assert_eq!(report.document.headless[0].exit_code, Some(3));
        for kind in [
            QuirkKind::NonZeroExit,
            QuirkKind::ReportUnreadable,
            QuirkKind::ExitedEarly,
        ] {
            assert!(report.quirks.has(kind), "{kind}");
        }

        // What a run writes: the document, the log, and (asked for) one recording per session.
        let mut names: Vec<String> = fs::read_dir(&report.run_dir)
            .expect("the run's directory")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "1-tui-default.cast",
                "1-tui-deterministic.cast",
                "2-tui-default.cast",
                "2-tui-deterministic.cast",
                "quirks.txt",
                "summary.json"
            ]
        );
        let summary = fs::read_to_string(report.run_dir.join("summary.json")).expect("summary");
        assert_eq!(
            HarnessSoak::from_json_str(&summary).expect("a document that validates"),
            report.document
        );
        let dir = world.dir.path().to_string_lossy().into_owned();
        assert!(
            !summary.contains(&dir) && !summary.contains('/'),
            "{summary}"
        );
        for (path, mode) in [
            (report.run_dir.clone(), 0o700),
            (report.run_dir.join("summary.json"), 0o600),
            (report.run_dir.join("quirks.txt"), 0o600),
        ] {
            let found = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(found, mode, "{}", path.display());
        }
        assert!(world.path("out/latest/summary.json").is_file());

        // Without `record` there is no recording. A run's id is its second and its process, so
        // the second run is made in a world of its own.
        let other = World::new();
        let quiet = soak(
            &other,
            &other.script("exit 3\n"),
            1,
            Limits::default(),
            false,
            &Interrupt::new(),
        )
        .expect("a soak");
        assert!(!quiet.run_dir.join("1-tui-default.cast").exists());
    }

    /// Makes every directory below `dir` usable by its owner again, so that a directory that a
    /// program locked can be removed with the area it is in.
    fn open_up(dir: &Path) {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                open_up(&entry.path());
            }
        }
    }

    /// Opens up everything below the directory it holds when it is dropped, so that a test that
    /// fails with a locked directory in the work directory does not leave it behind.
    struct OpenUpOnDrop(PathBuf);

    impl Drop for OpenUpOnDrop {
        fn drop(&mut self) {
            open_up(&self.0);
        }
    }

    #[test]
    fn a_scan_that_leaves_a_folder_in_the_place_of_its_report_is_a_quirk_and_the_soak_finishes() {
        let world = World::new();
        let _open_up = OpenUpOnDrop(world.path("work"));
        // The report's place holds a folder, and in it a file and a folder that cannot be read.
        let binary = world.script(
            "if [ \"$1\" = --format ]; then\n\
             mkdir -p \"$4/locked/inner\" && : > \"$4/plain\" && chmod 000 \"$4/locked\"\n\
             exit 0\nfi\nexit 3\n",
        );

        let report = soak(
            &world,
            &binary,
            1,
            Limits::default(),
            false,
            &Interrupt::new(),
        )
        .expect("a soak");
        open_up(&world.path("work"));

        assert_eq!(report.outcome(), SoakOutcome::Finished);
        assert_eq!(report.failure, None, "a harness error ended the run");
        let scan = &report.document.headless[0];
        assert_eq!(
            scan.residue_files, 0,
            "what the scan left in the report's place is the report's, and is not residue"
        );
        assert!(scan.report.is_none());
        assert!(report.quirks.has(QuirkKind::ReportUnreadable));
        assert!(!report.quirks.has(QuirkKind::HarnessError));
        assert!(!report.quirks.has(QuirkKind::Residue));
        let text = fs::read_to_string(report.run_dir.join("quirks.txt")).expect("quirks.txt");
        assert!(text.contains("not a regular file"), "{text}");
    }

    #[test]
    fn a_scan_that_leaves_more_than_the_look_reads_is_counted_in_the_document_as_at_least_that_many()
     {
        let world = World::new();
        let binary = world.script(
            "if [ \"$1\" = --format ]; then\n\
             i=0\nwhile [ \"$i\" -lt 10100 ]; do : > \"f$i\"; i=$((i+1)); done\n\
             exit 0\nfi\nexit 3\n",
        );

        let report = soak(
            &world,
            &binary,
            1,
            Limits::default(),
            false,
            &Interrupt::new(),
        )
        .expect("a soak");

        assert_eq!(report.outcome(), SoakOutcome::Finished);
        let counted = report.document.headless[0].residue_files;
        assert!(
            (9_980..=10_000).contains(&counted),
            "the look reads at most 10,000 entries, and the document counts what it found: {counted}"
        );
        let text = fs::read_to_string(report.run_dir.join("quirks.txt")).expect("quirks.txt");
        assert!(
            text.contains("[residue] round 1, headless default: the scan left at least 99")
                && text.contains("(the look at it was cut short at 10000 entries)"),
            "{text}"
        );
    }

    #[test]
    fn a_soak_of_a_tree_that_holds_its_output_directory_adds_new_entries_there_and_replaces_latest()
    {
        let world = World::new();
        let out = world.path("tree/target/excise-soak");
        fs::create_dir_all(out.join("earlier-run")).expect("an earlier run");
        fs::write(
            out.join("earlier-run/summary.json"),
            b"{\"an\": \"earlier run\"}\n",
        )
        .expect("its summary");
        std::os::unix::fs::symlink("earlier-run", out.join("latest")).expect("its link");
        fs::write(world.path("tree/target/notes.txt"), b"keep me\n").expect("a file beside it");
        let binary = world.script("exit 3\n");
        let before = state_of(&world.path("tree"));

        let report = Soak {
            out_root: Some(out),
            record: true,
            ..Soak::new(&binary)
        }
        .run(&world, &Interrupt::new(), &mut |_| {})
        .expect("a soak");

        let after = state_of(&world.path("tree"));
        let run = format!("target/excise-soak/{}", report.document.run_id);
        let expected: BTreeSet<String> = [
            "",
            "/summary.json",
            "/quirks.txt",
            "/1-tui-default.cast",
            "/1-tui-deterministic.cast",
        ]
        .iter()
        .map(|tail| format!("{run}{tail}"))
        .collect();
        let added: BTreeSet<String> = after
            .keys()
            .filter(|key| !before.contains_key(*key))
            .cloned()
            .collect();
        assert_eq!(
            added, expected,
            "the run's directory and its files are the only new entries"
        );
        let removed: Vec<&String> = before
            .keys()
            .filter(|key| !after.contains_key(*key))
            .collect();
        assert!(removed.is_empty(), "removed: {removed:?}");
        let changed: Vec<&str> = before
            .iter()
            .filter(|(key, value)| after.get(*key) != Some(*value))
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(
            changed,
            ["target/excise-soak/latest"],
            "everything that existed is byte for byte as it was, but the link"
        );
        assert_eq!(
            after["target/excise-soak/latest"],
            format!("link to {}", report.document.run_id)
        );
    }

    #[test]
    fn a_latest_that_is_not_a_link_a_run_made_is_left_alone_and_nothing_starts() {
        let world = World::new();
        let out = world.path("tree/target/excise-soak");
        fs::create_dir_all(&out).expect("an output directory");
        fs::write(out.join("latest"), b"my notes, not a run\n").expect("a file named latest");
        let marker = world.path("started");
        let binary = world.script(&format!("echo started >> '{}'\n", marker.display()));
        let before = state_of(&world.path("tree"));

        let refusal = Soak {
            out_root: Some(out),
            ..Soak::new(&binary)
        }
        .run(&world, &Interrupt::new(), &mut |_| {})
        .expect_err("a latest that is a file");

        assert!(matches!(refusal, SoakError::Io { .. }), "{refusal:?}");
        assert!(refusal.to_string().contains("latest"), "{refusal}");
        assert!(!marker.exists(), "the program was started");
        assert_eq!(
            state_of(&world.path("tree")),
            before,
            "something in the tree changed"
        );
    }

    #[test]
    fn an_interrupt_before_the_recordings_are_saved_saves_none_and_the_summary_says_so() {
        let world = World::new();
        let binary = world.script("exit 3\n");
        let interrupt = Interrupt::new();

        let report = Soak {
            record: true,
            ..Soak::new(&binary)
        }
        .run(&world, &interrupt, &mut |line| {
            if line.starts_with("saving") {
                interrupt.trigger();
            }
        })
        .expect("a soak");

        assert_eq!(report.outcome(), SoakOutcome::Interrupted);
        let mut names: Vec<String> = fs::read_dir(&report.run_dir)
            .expect("the run's directory")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["quirks.txt", "summary.json"],
            "no recording was started"
        );
        let written = fs::read_to_string(report.run_dir.join("summary.json")).expect("summary");
        let document = HarnessSoak::from_json_str(&written).expect("a document");
        assert_eq!(document.outcome, SoakOutcome::Interrupted);
        assert_eq!(document.rounds.completed, 1, "the round itself had ended");
    }

    #[test]
    fn the_bound_of_the_run_does_not_cost_the_recordings_of_the_sessions_it_cut_short() {
        let world = World::new();
        let binary = world.hang(true);

        let report = Soak {
            record: true,
            limits: Limits::for_run(Duration::from_secs(2)),
            ..Soak::new(&binary)
        }
        .run(&world, &Interrupt::new(), &mut |_| {})
        .expect("a soak");

        assert_eq!(report.outcome(), SoakOutcome::Interrupted);
        let cast = report.run_dir.join("1-tui-default.cast");
        assert!(
            fs::metadata(&cast).expect("the recording was saved").len() > 0,
            "the recording of the session the bound cut short is kept"
        );
        assert_gone(&world, &world.pids());
    }

    #[test]
    fn a_bound_that_has_passed_when_the_binary_is_copied_starts_nothing_and_leaves_nothing() {
        let world = World::new();
        let marker = world.path("started");
        let binary = world.script(&format!("echo started >> '{}'\n", marker.display()));

        // A bound of a nanosecond has passed before the copy of the binary is started.
        let refused = soak(
            &world,
            &binary,
            1,
            Limits::for_run(Duration::from_nanos(1)),
            false,
            &Interrupt::new(),
        );

        let Err(error) = refused else {
            panic!("a bound that had passed was not noticed: {refused:?}");
        };
        assert!(matches!(error, SoakError::BoundPassed), "{error:?}");
        assert!(error.to_string().contains("bound"), "{error}");
        assert!(!marker.exists(), "the program was started");
        assert!(!world.path("out").exists(), "something was written");
        assert!(
            names_in(&world.path("work")).is_empty(),
            "a copy of the binary or a scratch area was left"
        );
    }

    #[test]
    fn an_interrupt_before_the_binary_is_copied_starts_nothing_and_leaves_nothing() {
        let world = World::new();
        let marker = world.path("started");
        let binary = world.script(&format!("echo started >> '{}'\n", marker.display()));
        let interrupt = Interrupt::new();
        interrupt.trigger();

        let refused = soak(&world, &binary, 1, Limits::default(), false, &interrupt);

        let Err(error) = refused else {
            panic!("an interrupt was not noticed: {refused:?}");
        };
        assert!(matches!(error, SoakError::Interrupted), "{error:?}");
        assert!(error.to_string().contains("interrupted"), "{error}");
        assert!(!marker.exists(), "the program was started");
        assert!(!world.path("out").exists(), "something was written");
        assert!(
            names_in(&world.path("work")).is_empty(),
            "a copy of the binary or a scratch area was left"
        );
    }

    #[test]
    fn the_bound_counts_from_the_instant_the_request_gives_and_not_from_when_the_call_was_made() {
        let world = World::new();
        let marker = world.path("started");
        let binary = world.script(&format!("echo started >> '{}'\n", marker.display()));
        let root = SoakRoot::open(world.path("tree")).expect("a root");
        let interrupt = Interrupt::new();
        // The bound began 50 ms ago, so the bound of 10 ms had passed before the call was made, as
        // it has when the build before it took that long. A clock that began with the call would
        // give the run all of its bound again, and the scans would start.
        let started = Instant::now()
            .checked_sub(Duration::from_millis(50))
            .expect("an instant in the past");
        let request = SoakRequest {
            root: &root,
            binary: &binary,
            binary_identity: None,
            out_root: &world.path("out"),
            work_dir: &world.path("work"),
            rounds: 1,
            limits: Limits::for_run(Duration::from_millis(10)),
            started,
            record: false,
            git_sha: &"0".repeat(40),
            interrupt: &interrupt,
        };

        let refused = run_soak(&request, &mut |_| {});

        let Err(error) = refused else {
            panic!("the bound was counted from after the call: {refused:?}");
        };
        assert!(matches!(error, SoakError::BoundPassed), "{error:?}");
        assert!(!marker.exists(), "the program was started");
        assert!(!world.path("out").exists(), "something was written");
        assert!(
            names_in(&world.path("work")).is_empty(),
            "a copy of the binary or a scratch area was made"
        );
    }

    #[test]
    fn the_document_records_the_bound_as_it_was_set_and_dates_the_run_from_its_start() {
        let world = World::new();
        let binary = world.script("exit 3\n");
        let root = SoakRoot::open(world.path("tree")).expect("a root");
        let interrupt = Interrupt::new();
        // The bound began three seconds ago, as it does when a build came before the call: the
        // run is dated from there, and the bound is the five minutes that were asked for, not
        // what the three seconds left of them.
        let started = Instant::now()
            .checked_sub(Duration::from_secs(3))
            .expect("an instant in the past");
        let request = SoakRequest {
            root: &root,
            binary: &binary,
            binary_identity: None,
            out_root: &world.path("out"),
            work_dir: &world.path("work"),
            rounds: 1,
            limits: Limits::for_run(Duration::from_mins(5)),
            started,
            record: false,
            git_sha: &"0".repeat(40),
            interrupt: &interrupt,
        };

        let report = run_soak(&request, &mut |_| {}).expect("a soak");

        assert_eq!(report.document.limits.run_ms, 300_000);
        let (started_at, finished_at) = (
            seconds_of(&report.document.started_at),
            seconds_of(&report.document.finished_at),
        );
        assert!(
            finished_at - started_at >= 3,
            "the run is dated from after the three seconds that came before the call: {} to {}",
            report.document.started_at,
            report.document.finished_at
        );
    }

    /// The seconds into the day of an RFC 3339 time (`2026-10-07T12:34:56.789Z`), with the day
    /// counted in, so that two times of one run, which cannot be a day apart, are told apart.
    fn seconds_of(stamp: &str) -> i64 {
        let day: i64 = stamp[8..10].parse().expect("a day");
        let hours: i64 = stamp[11..13].parse().expect("hours");
        let minutes: i64 = stamp[14..16].parse().expect("minutes");
        let seconds: i64 = stamp[17..19].parse().expect("seconds");
        day * 86_400 + hours * 3_600 + minutes * 60 + seconds
    }

    #[test]
    fn a_round_whose_last_session_ended_before_an_interrupt_is_counted_and_the_run_finished() {
        let world = World::new();
        let binary = world.script("exit 3\n");
        let interrupt = Interrupt::new();

        // The person presses Ctrl+C just after the last session of the only round ended, while
        // the line that says so is written: the three entries of the round are all there, and
        // none was cut short.
        let report = Soak::new(&binary)
            .run(&world, &interrupt, &mut |line| {
                if line.starts_with("round 1/1: session (deterministic)") {
                    interrupt.trigger();
                }
            })
            .expect("a soak");

        assert!(
            interrupt.is_set(),
            "the interrupt came after the last session ended"
        );
        assert_eq!(report.document.headless.len(), 1);
        assert_eq!(report.document.tui.len(), 2);
        assert_eq!(
            report.document.rounds,
            SoakRounds {
                requested: 1,
                completed: 1
            }
        );
        assert_eq!(report.outcome(), SoakOutcome::Finished);
    }

    #[test]
    fn an_interrupt_after_a_whole_round_ends_the_run_before_the_next_round_starts() {
        let world = World::new();
        let binary = world.script("exit 3\n");
        let interrupt = Interrupt::new();

        let report = Soak {
            rounds: 2,
            ..Soak::new(&binary)
        }
        .run(&world, &interrupt, &mut |line| {
            if line.starts_with("round 1/2: session (deterministic)") {
                interrupt.trigger();
            }
        })
        .expect("a soak");

        assert_eq!(
            report.document.rounds,
            SoakRounds {
                requested: 2,
                completed: 1
            },
            "the first round was whole, and the second never started"
        );
        assert_eq!(report.outcome(), SoakOutcome::Interrupted);
        assert_eq!(report.document.headless.len(), 1);
        assert_eq!(report.document.tui.len(), 2);
    }

    #[test]
    fn a_summary_that_cannot_be_published_leaves_no_file_under_the_name_of_a_finished_run() {
        let world = World::new();
        let binary = world.script("exit 3\n");
        let report = soak(
            &world,
            &binary,
            1,
            Limits::default(),
            false,
            &Interrupt::new(),
        )
        .expect("a soak");
        // The name that the summary goes by is taken, by a directory.
        let blocked = world.path("blocked");
        fs::create_dir_all(blocked.join("summary.json")).expect("a directory in the way");

        let error = write_results(&blocked, &report.document, &report.quirks)
            .expect_err("the summary cannot be published");

        assert!(matches!(error, SoakError::Io { .. }), "{error:?}");
        assert!(error.to_string().contains("summary.json"), "{error}");
        assert_eq!(
            names_in(&blocked),
            ["quirks.txt", "summary.json"],
            "the log was written first, and no temporary file of the summary is left"
        );
        assert!(
            blocked.join("summary.json").is_dir(),
            "what was there is as it was"
        );
    }

    #[test]
    fn every_metric_a_scripted_session_records_is_one_the_schema_lists() {
        let world = World::new();
        let binary = world.script("exit 3\n");

        let report = soak(
            &world,
            &binary,
            1,
            Limits::default(),
            false,
            &Interrupt::new(),
        )
        .expect("a soak");

        let schema: serde_json::Value =
            serde_json::from_str(HarnessSoak::SCHEMA_JSON).expect("the schema is JSON");
        let listed: BTreeSet<&str> = schema["$defs"]["metric_name"]["enum"]
            .as_array()
            .expect("the names are an enumeration")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        let recorded: BTreeSet<&str> = report
            .document
            .tui
            .iter()
            .flat_map(|session| session.metrics.keys().map(String::as_str))
            .collect();
        assert!(!recorded.is_empty(), "a session records something");
        let unlisted: Vec<&&str> = recorded.difference(&listed).collect();
        assert!(
            unlisted.is_empty(),
            "recorded, and not listed by the schema: {unlisted:?}"
        );
    }
}
