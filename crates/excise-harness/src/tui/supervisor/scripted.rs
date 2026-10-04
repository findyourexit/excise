//! The guard of `keys` and the protocol of `delete`, against scripted programs.
//!
//! The programs are shell scripts (see `crate::runner::scripted`) that write the event channel and
//! the terminal the way `excise` does, and that decide when. That puts a program in the states the
//! guard exists for: it has read some of the bytes it was sent and not the rest (a key that a
//! terminal cut into two input events), and the terminal has not delivered a frame the program has
//! drawn. A script waits for the test to say go (a byte on its input that no key of the driver is,
//! or a file that the test creates, where the program must leave the bytes it was sent unread)
//! where a pause would make the test depend on the speed of the machine.
//!
//! Nothing here confirms a deletion: a test that could would be the defect it looks for. Every
//! test either asks the guard whether a confirmation could be sent, and never sends one, or runs
//! the protocol against a program that never reads a confirmation, and checks that none was sent.
//! The one exception is the control that proves the protocol still confirms what it verified: it
//! runs on a copy of a fixture that the test made and owns, against a script that deletes nothing.

use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use tempfile::TempDir;

use super::*;
use crate::{
    fixture::{FixtureCache, FixtureSpec, MARKER_FILE_NAME},
    runner::{
        live::CONPTY_FRAME_WINDOW,
        scripted::{COMPLETE_HEADER, prelude, prelude_with, screen},
    },
    scenario::Profile,
};

/// How long a test lets the guard wait for an answer that never comes: more than the second that
/// the guard once waited after a Backspace whatever the frames said, so that a guard that still
/// waits on the clock cannot pass for one that waits on the program.
const PATIENCE: Duration = Duration::from_secs(3);
/// How long a test waits for a scripted program to do something it was told to do.
const SCRIPT_LIMIT: Duration = Duration::from_secs(20);
/// The byte a test sends to tell a script to go on: not a key of the driver.
const GO: &[u8] = b"!";
/// `GO` as a script reads it (`od` prints octal).
const GO_OCTAL: &str = "041";
/// A barrier request as a script reads it (`od` prints octal): the byte `0x1d`.
const REQUEST_OCTAL: &str = "035";

const CONFIRMATION_LINE: &str = "[Enter/y] start    [n] cancel";

/// The map, as a screen that a script draws.
fn map() -> String {
    screen(COMPLETE_HEADER, None)
}

/// The map of the folder above this one, which a program shows after the Esc that leaves the
/// folder it was in: the same map, with a word that a test can wait for.
fn parent_map() -> String {
    screen(" EXCISE  /scripted  ◆ COMPLETE  ABOVE", None)
}

/// The deletion dialog of `path`, over the map.
fn deletion_dialog(path: &str) -> String {
    screen(
        COMPLETE_HEADER,
        Some(("! DELETE FILE", &[path, CONFIRMATION_LINE])),
    )
}

/// The quit prompt, over the map.
fn quit_prompt() -> String {
    screen(
        COMPLETE_HEADER,
        Some(("QUIT", &["Quit Excise?", "[y] Quit    [n] Stay"])),
    )
}

/// What a script is told of the world it runs in.
struct World {
    /// The canonical root of the fixture, as the program would show it in a dialog.
    root: PathBuf,
    /// The ownership marker of the fixture.
    marker: PathBuf,
    /// The file that a script waits for ([`World::wait_for_release`]) and that
    /// [`Scripted::release`] creates.
    release: PathBuf,
}

impl World {
    /// The path the dialog of `victim.bin` shows.
    fn victim(&self) -> String {
        self.root.join("victim.bin").display().to_string()
    }

    /// Script lines that wait until the test calls [`Scripted::release`], and read nothing
    /// meanwhile: whatever the program was sent stays unread in the terminal's input. The wait ends
    /// when the test says so, and the script ends with the session, so it is bounded by the test.
    fn wait_for_release(&self) -> String {
        format!(
            "until [ -e '{}' ]; do sleep 0.01; done\n",
            self.release.display()
        )
    }
}

/// What a script does with a key: waits for one, and runs `then` if it is the byte `octal` (`177` is
/// Backspace, `003` Ctrl+C, `167` `w`), and ends the program with a status the test would notice if
/// it is not.
fn on_key(octal: &str, then: &str) -> String {
    format!("key\n[ \"$byte\" = {octal} ] || exit 9\n{then}\n")
}

/// What a script does to read a barrier request itself, at its own moment: it sees the byte
/// (`answer_barriers=0`), counts the request, and reports a frame, counting `inputs` inputs, that
/// answers it, which a later `mark` puts on the screen. Ends the program with a status the test
/// would notice if the next byte is not a request.
fn on_request(inputs: u64) -> String {
    format!("key\n[ \"$byte\" = {REQUEST_OCTAL} ] || exit 9\nrequest\nreport {inputs}\n")
}

/// A session around a scripted program: everything a `Session` has, but the program.
struct Scripted {
    session: Session,
    world: World,
    /// Kept for as long as the session: the state directory, and the work directory with the
    /// fixture, the scratch area, and the workspace in it.
    _state: TempDir,
    _work: TempDir,
}

impl Scripted {
    /// A session whose program runs `body` after `prelude(frame_marks)`, on a copy of the
    /// `delete-file` fixture.
    fn start(frame_marks: bool, body: impl FnOnce(&World) -> String) -> Self {
        Self::start_with(&prelude(frame_marks), body)
    }

    /// A session whose program runs `body` after `prelude`, on a copy of the `delete-file`
    /// fixture.
    fn start_with(prelude: &str, body: impl FnOnce(&World) -> String) -> Self {
        let state = tempfile::tempdir().expect("a state directory");
        // A short path: a deletion dialog cuts a path that does not fit its width.
        let work = tempfile::Builder::new()
            .prefix("xt-")
            .rand_bytes(4)
            .tempdir_in("/tmp")
            .expect("a work directory");
        let dir = SessionDir::create(state.path()).expect("a session directory");
        let nonce = "0123456789abcdef0123456789abcdef";
        let workspace = Workspace::create(work.path(), dir.id(), nonce).expect("a workspace");
        let copy = Fixtures::new(
            FixtureSpec::bundled_dir(),
            FixtureCache::at(work.path().join("cache")),
        )
        .run_copy("delete-file", &workspace.path())
        .expect("a fixture copy");
        let fixture = FixtureRoot::open(copy.root()).expect("an owned fixture");
        let scratch = Scratch::create(&workspace.path()).expect("a scratch area");
        let world = World {
            root: fixture.path().to_path_buf(),
            marker: fixture.path().join(MARKER_FILE_NAME),
            release: work.path().join("release"),
        };
        let size = Size {
            cols: 120,
            rows: 40,
        };
        let script = format!("{prelude}\n{}", body(&world));
        let program = PtySession::spawn(&SpawnSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            env: isolated_env(&scratch, Profile::Default, true, None),
            cwd: scratch.cwd(),
            cols: size.cols,
            rows: size.rows,
            drain_bytes_per_sec: None,
            recording: None,
            title: None,
        })
        .expect("the scripted program starts");
        let config = Config {
            session: dir.id().to_string(),
            fixture: "delete-file".to_owned(),
            profile: Profile::Default,
            cols: size.cols,
            rows: size.rows,
            record: None,
            idle_timeout_ms: 60_000,
            binary: PathBuf::from("/bin/sh"),
            work_base: work.path().to_path_buf(),
            started_at: "2026-01-01T00:00:00Z".to_owned(),
            nonce: nonce.to_owned(),
        };
        let live = Live::new(program, scratch.events());
        Self {
            session: Session {
                dir,
                config,
                size,
                live,
                fixture,
                run_copy: Some(copy),
                scratch: Some(scratch),
                workspace,
                baseline: None,
                intended_deletions: Vec::new(),
                reported: 0,
                last_activity: Instant::now(),
                ended: None,
            },
            world,
            _state: state,
            _work: work,
        }
    }

    /// Reads the program until the screen shows `text`.
    fn wait_for(&mut self, text: &str) {
        let deadline = Instant::now() + SCRIPT_LIMIT;
        loop {
            self.session.pump().expect("the program is read");
            if self.session.live.session.screen().text().contains(text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the screen never showed {text:?}:\n{}",
                self.session.live.session.screen().text()
            );
            self.session
                .live
                .session
                .wait_activity(Duration::from_millis(5))
                .expect("the program is read");
        }
    }

    /// Reads the program until the script has written the file `name` in its working directory,
    /// and returns what is in it.
    fn wait_for_file(&mut self, name: &str) -> String {
        let written = self.cwd().join(name);
        let deadline = Instant::now() + SCRIPT_LIMIT;
        loop {
            self.session.pump().expect("the program is read");
            if let Ok(text) = fs::read_to_string(&written)
                && !text.is_empty()
            {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "the script never wrote {name}:\n{}",
                self.session.live.session.screen().text()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The working directory of the program, where it writes the files a test looks for.
    fn cwd(&self) -> PathBuf {
        self.session.scratch.as_ref().expect("a scratch area").cwd()
    }

    /// Sends `bytes` the way a command of the driver does: counted, written.
    fn press(&mut self, bytes: &[u8]) {
        self.session.send_input(bytes).expect("the key is written");
    }

    /// Tells the script to go on, with a byte on its input. Not an input of the driver: nothing
    /// counts it.
    fn go(&mut self) {
        self.session
            .live
            .session
            .send(GO)
            .expect("the byte is written");
    }

    /// Lets a script that waits for it ([`World::wait_for_release`]) read on.
    fn release(&self) {
        fs::write(&self.world.release, b"").expect("the release file is written");
    }

    /// What the guard says to a confirmation key now, after waiting up to `limit` for the program.
    fn verdict_within(&mut self, limit: Duration) -> Result<(), String> {
        self.session.confirmation_is_safe(Instant::now() + limit)
    }

    /// What the guard says to a confirmation key now, after waiting [`PATIENCE`] for the program.
    fn verdict(&mut self) -> Result<(), String> {
        self.verdict_within(PATIENCE)
    }

    fn inputs_sent(&self) -> u64 {
        self.session.live.inputs_sent()
    }

    /// How many barrier requests the script has read: it writes a byte for each.
    fn requests(&self) -> usize {
        fs::read_to_string(self.cwd().join("requests")).map_or(0, |text| text.len())
    }

    /// Whether the script has read a byte it should not have: it records what it was not
    /// expecting in `received`.
    fn received(&self) -> Option<String> {
        fs::read_to_string(self.cwd().join("received"))
            .ok()
            .filter(|text| !text.is_empty())
    }

    fn screen_text(&self) -> String {
        self.session.live.session.screen().text()
    }

    /// Makes the terminal one that paints on its own timer, as `ConPTY` does on Windows: its
    /// frame window is not zero, and its screen is not exact.
    fn on_a_console_host(&mut self) {
        self.session.live.frame_window = CONPTY_FRAME_WINDOW;
    }
}

/// The program is on the map, and has reported its first frame. It answers barrier requests as
/// `excise` does, and records any key it is sent in `received`.
fn idle_on_the_map(frame_marks: bool) -> Scripted {
    let mut program = Scripted::start(frame_marks, |_| {
        format!(
            "stty raw -echo\n{}frame 0\nwhile :; do key; echo \"$byte\" >> received; done\n",
            map()
        )
    });
    program.wait_for("COMPLETE");
    program
}

#[test]
fn a_confirmation_is_refused_while_the_second_event_of_an_escape_prefixed_key_is_unread() {
    // `keys alt+backspace`, then `keys y`. The key is the bytes ESC and DEL, and a program can read
    // them as one event, Alt+Backspace, or as two: Esc, which leaves the folder it is in, and then
    // Backspace, which asks for a deletion dialog. The program here has read the Esc and drawn
    // the frame for it, which counts the one input that was sent, and has not read the Backspace
    // behind it. Counting the inputs sent against the frames drawn takes that frame for the whole
    // key: the screen shows no dialog, and a `y` now would meet the dialog that the Backspace is
    // about to open, which nobody has seen.
    let mut program = Scripted::start(true, |world| {
        format!(
            "stty raw -echo\n{map}frame 0\n{esc}{wait}key\n[ \"$byte\" = 177 ] || exit 9\n\
             {dialog}frame 2\nkey\necho \"$byte\" > received\nsleep 60\n",
            map = map(),
            esc = on_key("033", &format!("{}frame 1", parent_map())),
            wait = world.wait_for_release(),
            dialog = deletion_dialog(&world.victim()),
        )
    });
    program.wait_for("COMPLETE");
    program.press(&[0x1b, 0x7f]);
    program.wait_for("ABOVE");

    let refused = program.verdict_within(Duration::from_millis(500));

    let message =
        refused.expect_err("the Backspace of the key is not read, whatever the frames say");
    assert!(
        message.contains("has not said that it read the keys before this one"),
        "{message}"
    );
    assert!(
        !program.screen_text().contains("DELETE"),
        "the screen shows no dialog, and that is exactly the trap"
    );
    assert_eq!(program.inputs_sent(), 1, "the guard sent no key");

    // The program reads the Backspace, and the dialog is open: a confirmation is refused for that.
    program.release();
    let refused = program.verdict().expect_err("the dialog is open");
    assert!(refused.contains("a deletion dialog is open"), "{refused}");
    assert_eq!(program.inputs_sent(), 1, "the guard sent no key");
    assert_eq!(program.received(), None, "no key reached the program");
}

#[test]
fn a_confirmation_is_refused_while_the_frame_that_answers_the_barrier_is_not_on_the_screen() {
    // `keys backspace`, `keys ctrl+c`, `keys w y`: Backspace opens the deletion dialog, Ctrl+C puts
    // the quit prompt over it, and `w` closes the prompt, so that the dialog comes back. The
    // program has read all three and reported the frame that answers the barrier behind them, and
    // the bytes of that frame have not reached the terminal: the screen still shows the quit
    // prompt, and a `y` now would meet a dialog that nobody has seen.
    let mut program = Scripted::start(true, |world| {
        format!(
            "stty raw -echo\n{map}frame 0\nanswer_barriers=0\n{backspace}{ctrl_c}{w}{request}\
             key\n[ \"$byte\" = {GO_OCTAL} ] || exit 9\n{dialog}mark\nanswer_barriers=1\nkey\n\
             echo \"$byte\" > received\nsleep 60\n",
            map = map(),
            backspace = on_key(
                "177",
                &format!("{}frame 1", deletion_dialog(&world.victim()))
            ),
            ctrl_c = on_key("003", &format!("{}frame 2", quit_prompt())),
            w = on_key("167", ""),
            request = on_request(3),
            dialog = deletion_dialog(&world.victim()),
        )
    });
    program.wait_for("COMPLETE");
    for key in [&[0x7f][..], &[0x03], b"w"] {
        program.press(key);
    }

    let refused = program.verdict_within(Duration::from_millis(500));

    let message =
        refused.expect_err("the dialog is not on the screen yet, whatever the clock says");
    assert!(
        message.contains("did not come, and show on the screen, in time"),
        "{message}"
    );
    assert!(
        program.screen_text().contains("QUIT"),
        "the screen is the one the test set up: the quit prompt"
    );

    // The terminal delivers the frame. The dialog is on the screen, and a confirmation is refused
    // for that.
    program.go();
    let refused = program.verdict().expect_err("the dialog is open");
    assert!(refused.contains("a deletion dialog is open"), "{refused}");
    assert_eq!(program.inputs_sent(), 3, "the guard sent nothing");
    assert_eq!(program.received(), None, "no key reached the program");
}

#[test]
fn a_confirmation_is_refused_long_after_a_backspace_whose_dialog_is_not_on_the_screen() {
    // More than a second after the Backspace, the program has reported the frame that answers the
    // barrier, and its bytes have not arrived. The time that has passed says nothing about the
    // dialog the Backspace opened.
    let mut program = Scripted::start(true, |world| {
        format!(
            "stty raw -echo\n{map}frame 0\nanswer_barriers=0\n{backspace}{request}\
             key\n[ \"$byte\" = {GO_OCTAL} ] || exit 9\n{dialog}mark\nanswer_barriers=1\nkey\n\
             echo \"$byte\" > received\nsleep 60\n",
            map = map(),
            backspace = on_key("177", ""),
            request = on_request(1),
            dialog = deletion_dialog(&world.victim()),
        )
    });
    program.wait_for("COMPLETE");
    program.press(&[0x7f]);
    std::thread::sleep(Duration::from_millis(1200));

    let refused = program.verdict_within(Duration::from_millis(500));

    let message = refused.expect_err("the dialog is not on the screen yet");
    assert!(
        message.contains("did not come, and show on the screen, in time"),
        "{message}"
    );
    assert!(
        !program.screen_text().contains("DELETE"),
        "the screen shows no dialog, and that is exactly the trap"
    );

    program.go();
    let refused = program.verdict().expect_err("the dialog is open");
    assert!(refused.contains("a deletion dialog is open"), "{refused}");
    assert_eq!(program.received(), None, "no key reached the program");
}

#[test]
fn a_confirmation_is_allowed_once_the_program_answers_the_barrier_and_no_dialog_is_open() {
    // A Backspace in the filter prompt erases text and opens nothing. Once the program has read
    // everything and the screen shows the frame that says so, the screen can be believed about
    // dialogs, and it shows none. The frame that answers is held back until the test says go.
    let mut program = Scripted::start(true, |_| {
        format!(
            "stty raw -echo\n{map}frame 0\nanswer_barriers=0\n{backspace}{request}\
             key\n[ \"$byte\" = {GO_OCTAL} ] || exit 9\n{map}mark\nanswer_barriers=1\nkey\n\
             echo \"$byte\" > received\nsleep 60\n",
            map = map(),
            backspace = on_key("177", "report 1"),
            request = on_request(1),
        )
    });
    program.wait_for("COMPLETE");
    program.press(&[0x7f]);

    // The frame is reported, and its bytes come when the test says.
    program
        .verdict_within(Duration::from_millis(300))
        .expect_err("the frame is not on the screen yet");
    program.go();

    program
        .verdict()
        .expect("the screen shows the frame that answers the barrier, and no dialog");
    assert_eq!(program.inputs_sent(), 1, "the guard sent no key");
    assert_eq!(program.received(), None, "no key reached the program");
}

#[test]
fn a_confirmation_is_allowed_on_a_map_with_no_dialog_once_the_barrier_is_answered() {
    // Nothing that could present a dialog was sent, and none is on the screen. The guard asks all
    // the same: what the program has read is not something the driver can know from what it sent.
    let mut program = idle_on_the_map(true);
    program.press(b"x");
    program.press(b"/");

    program
        .verdict_within(Duration::from_millis(500))
        .expect("the program answered the barrier, and none is on the screen");

    assert_eq!(program.requests(), 1, "one request was written, and read");
    assert_eq!(
        program.inputs_sent(),
        2,
        "the barrier is not an input of the driver"
    );
}

#[test]
fn a_barrier_that_is_never_answered_is_not_repeated() {
    // A program that is not reading answers nothing, and a second request behind the first could
    // take the answer to the first for its own. The guard waits for the one that is outstanding
    // and writes no other: the script sees one request, and then the byte the test sends.
    let mut program = Scripted::start(true, |_| {
        format!(
            "stty raw -echo\n{map}frame 0\nanswer_barriers=0\nkey\n[ \"$byte\" = {REQUEST_OCTAL} ] \
             || exit 9\nrequest\nkey\n[ \"$byte\" = {GO_OCTAL} ] || exit 9\nprintf done >> finished\n\
             sleep 60\n",
            map = map(),
        )
    });
    program.wait_for("COMPLETE");

    for _ in 0..3 {
        let refused = program
            .verdict_within(Duration::from_millis(300))
            .expect_err("the program answers nothing");
        assert!(
            refused.contains("has not said that it read the keys before this one"),
            "{refused}"
        );
    }
    program.go();

    // The next byte the script read after the one request was the one the test sent, not a second
    // request: it would have exited with a status the test notices.
    assert_eq!(program.wait_for_file("finished"), "done");
    assert_eq!(program.requests(), 1);
}

impl Scripted {
    /// The command `keys` with `tokens`, as the supervisor runs it, given `timeout` to wait for
    /// the frame that counts the keys.
    fn keys(&mut self, tokens: &[&str], timeout: Duration) -> Handled {
        let tokens: Vec<String> = tokens.iter().map(|token| (*token).to_owned()).collect();
        self.session.keys(&tokens, timeout)
    }
}

/// A program that reads a Backspace and shows the deletion dialog as the frame that counts it,
/// reads the two bytes of `alt+[`, and then leaves barrier requests unanswered, as a program in
/// the middle of an escape sequence does (it takes the request for a byte of the sequence). The
/// next byte, the one that would go on with the sequence, is recorded in `received`.
fn in_the_middle_of_an_escape_sequence(world: &World) -> String {
    format!(
        "stty raw -echo\n{map}frame 0\n{backspace}{escape}{bracket}answer_barriers=0\nkey\n\
         echo \"$byte\" > received\nsleep 60\n",
        map = map(),
        backspace = on_key(
            "177",
            &format!("{}frame 1", deletion_dialog(&world.victim()))
        ),
        escape = on_key("033", ""),
        bracket = on_key("133", ""),
    )
}

#[test]
fn keys_refuses_the_text_that_would_finish_an_escape_sequence_a_key_began() {
    // `keys backspace`, then `keys alt+[ type:121u`. The Backspace opened a deletion dialog.
    // `alt+[` is `ESC [`, and the text after it is the rest of `ESC [ 121 u`, which the program
    // reads as `y`, with no `y` in any key: the program joins the bytes of a sequence across
    // writes. The key that begins the sequence is sent, and the first character of the text, which
    // continues it, is refused: no barrier can be written behind `ESC [`, since the program takes
    // it for a byte of the sequence and never answers it.
    let mut program = Scripted::start(true, in_the_middle_of_an_escape_sequence);
    program.wait_for("COMPLETE");
    let opened = program.keys(&["backspace"], Duration::from_secs(5));
    assert!(opened.document.ok, "{:?}", opened.document.error);
    assert!(
        program.screen_text().contains("DELETE"),
        "the dialog is open"
    );

    let refused = program.keys(&["alt+[", "type:121u"], Duration::from_secs(2));

    let error = refused
        .document
        .error
        .expect("the text that would finish the sequence is refused");
    assert_eq!(error.kind, TuiErrorKind::Refused);
    assert!(
        error.message.contains("escape sequence") && error.message.contains("`close`"),
        "{}",
        error.message
    );
    let sent: Vec<&str> = error
        .sent
        .as_ref()
        .expect("what was sent")
        .iter()
        .map(|key| key.key.as_str())
        .collect();
    assert_eq!(sent, ["alt+["], "only the key that begins the sequence");
    assert_eq!(program.requests(), 0, "no barrier can be written behind it");
    assert_eq!(program.received(), None, "nothing finished the sequence");
    assert!(
        program.screen_text().contains("DELETE"),
        "the dialog remains"
    );
}

#[test]
fn keys_refuses_in_a_command_of_its_own_what_would_finish_a_sequence_an_earlier_command_began() {
    // The scan that every write feeds belongs to the session, not to the command: `alt+[` in one
    // command, and the text that finishes `ESC [ 121 u` in the next.
    let mut program = Scripted::start(true, in_the_middle_of_an_escape_sequence);
    program.wait_for("COMPLETE");
    let opened = program.keys(&["backspace"], Duration::from_secs(5));
    assert!(opened.document.ok, "{:?}", opened.document.error);
    // Nothing can be confirmed with `alt+[` yet, so it is sent. The program draws no frame for it
    // (it has read no key), and the command gives up waiting for one.
    let began = program.keys(&["alt+["], Duration::from_secs(1));
    assert!(began.document.ok, "{:?}", began.document.error);

    let refused = program.keys(&["type:121u"], Duration::from_secs(2));

    let error = refused.document.error.expect("the text is refused");
    assert_eq!(error.kind, TuiErrorKind::Refused);
    assert!(
        error.message.contains("escape sequence"),
        "{}",
        error.message
    );
    assert!(
        error.sent.as_ref().is_some_and(Vec::is_empty),
        "no key of the command was sent"
    );
    assert_eq!(program.requests(), 0, "no barrier can be written behind it");
    assert_eq!(program.received(), None, "nothing finished the sequence");
}

#[test]
fn a_bracket_after_an_escape_the_driver_wrote_itself_goes_out_behind_a_barrier() {
    // The driver writes escapes of its own: the one that dismisses a dialog, and the one that
    // closes an empty filter prompt. The scan sees them like any write, so the `[` that a command
    // sends next, which a program could join to the escape, goes out behind a barrier that keeps
    // the two apart.
    let mut program = idle_on_the_map(true);
    program.press(&[0x1b]);

    let handled = program.keys(&["[", "type:121u"], Duration::from_secs(1));

    assert!(handled.document.ok, "{:?}", handled.document.error);
    assert_eq!(
        program.requests(),
        1,
        "one barrier, between the escape and the bracket"
    );
    let deadline = Instant::now() + SCRIPT_LIMIT;
    let received = loop {
        let text = program.received().unwrap_or_default();
        if text.lines().count() >= 6 || Instant::now() > deadline {
            break text;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        received.lines().collect::<Vec<_>>(),
        ["033", "133", "061", "062", "061", "165"],
        "the program read the escape, the bracket, and the text, in order"
    );
}

#[test]
fn a_program_that_does_not_mark_its_frames_is_never_sent_a_confirmation() {
    // Nothing says when the screen shows the dialog this program has, so the guard cannot know:
    // not with no key sent, and not with a frame that counts every key.
    let mut program = Scripted::start(false, |_| {
        format!(
            "stty raw -echo\n{map}frame 0\n{backspace}{map}frame 1\nsleep 60\n",
            map = map(),
            backspace = on_key("177", ""),
        )
    });
    program.wait_for("COMPLETE");
    program
        .verdict_within(Duration::from_millis(300))
        .expect_err("no key was sent, and the program does not mark its frames all the same");
    program.press(&[0x7f]);

    let refused = program
        .verdict_within(Duration::from_millis(300))
        .expect_err("a program that does not mark its frames");

    assert!(refused.contains("does not mark its frames"), "{refused}");
    assert!(refused.contains("frame_marks"), "{refused}");
    assert_eq!(program.inputs_sent(), 1, "the guard sent nothing");
    assert_eq!(program.requests(), 0, "not even a barrier");
}

#[test]
fn a_program_that_does_not_answer_the_barrier_is_never_sent_a_confirmation() {
    // A program that marks its frames and does not answer barrier requests (a build from between
    // the two) has nothing to say that it has read what it was sent, so nothing that could confirm
    // a deletion is sent to it, and no barrier either: it would answer nothing.
    let mut program = Scripted::start_with(&prelude_with(true, false), |_| {
        format!(
            "stty raw -echo\n{map}frame 0\nkey\necho \"$byte\" > received\nsleep 60\n",
            map = map()
        )
    });
    program.wait_for("COMPLETE");

    let refused = program
        .verdict_within(Duration::from_millis(300))
        .expect_err("a program that does not answer the barrier");

    assert!(refused.contains("input barrier"), "{refused}");
    assert!(refused.contains("input_barrier"), "{refused}");
    assert!(refused.contains("no key was sent"), "{refused}");
    assert_eq!(program.inputs_sent(), 0, "the guard sent nothing");
    assert_eq!(program.requests(), 0, "not even a barrier");
    assert_eq!(program.received(), None);
}

#[test]
fn delete_refuses_the_ownership_marker_before_sending_anything() {
    let mut program = idle_on_the_map(true);
    let inside = format!("{MARKER_FILE_NAME}/inside");

    for name in [MARKER_FILE_NAME, inside.as_str()] {
        let error = program
            .session
            .delete_entry(name, EntryKind::File, Duration::from_secs(5))
            .expect_err("the marker is no target");

        assert_eq!(error.kind, TuiErrorKind::Refused, "{name}");
        assert!(
            error.message.contains(MARKER_FILE_NAME),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("no key was sent"),
            "{}",
            error.message
        );
        assert_eq!(
            program.inputs_sent(),
            0,
            "{name}: nothing was written to the program"
        );
        assert_eq!(program.requests(), 0, "{name}: not even a barrier");
        assert!(
            program.world.marker.is_file(),
            "{name}: the marker is still there"
        );
    }
}

#[test]
fn delete_refuses_a_program_that_does_not_mark_its_frames_before_sending_anything() {
    let mut program = idle_on_the_map(false);

    let error = program
        .session
        .delete_entry("victim.bin", EntryKind::File, Duration::from_secs(5))
        .expect_err("a program that does not mark its frames");

    assert_eq!(error.kind, TuiErrorKind::Refused);
    assert!(error.message.contains("frame_marks"), "{}", error.message);
    assert!(
        error.message.contains("no key was sent"),
        "{}",
        error.message
    );
    assert_eq!(
        program.inputs_sent(),
        0,
        "not even the keys that select the entry"
    );
}

#[test]
fn delete_refuses_a_program_that_does_not_answer_the_barrier_before_sending_anything() {
    let mut program = Scripted::start_with(&prelude_with(true, false), |_| {
        format!(
            "stty raw -echo\n{map}frame 0\nkey\necho \"$byte\" > received\nsleep 60\n",
            map = map()
        )
    });
    program.wait_for("COMPLETE");

    let error = program
        .session
        .delete_entry("victim.bin", EntryKind::File, Duration::from_secs(5))
        .expect_err("a program that does not answer the barrier");

    assert_eq!(error.kind, TuiErrorKind::Refused);
    assert!(error.message.contains("input_barrier"), "{}", error.message);
    assert!(
        error.message.contains("no key was sent"),
        "{}",
        error.message
    );
    assert_eq!(
        program.inputs_sent(),
        0,
        "not even the keys that select the entry"
    );
    assert_eq!(program.requests(), 0, "not even a barrier");
    assert_eq!(program.received(), None);
}

#[test]
fn delete_starts_from_a_map_with_no_dialog_open_and_sends_no_key_to_find_out() {
    // A deletion dialog is open when the command arrives. After the barrier the screen says so,
    // and the command refuses before it sends a key: the barrier is not an input.
    let mut program = Scripted::start(true, |world| {
        format!(
            "stty raw -echo\n{dialog}frame 0\nkey\necho \"$byte\" > received\nsleep 60\n",
            dialog = deletion_dialog(&world.victim()),
        )
    });
    program.wait_for("DELETE FILE");

    let error = program
        .session
        .delete_entry("victim.bin", EntryKind::File, Duration::from_secs(5))
        .expect_err("a dialog is open");

    assert_eq!(error.kind, TuiErrorKind::Conflict);
    assert!(
        error.message.contains("a deletion dialog is open"),
        "{}",
        error.message
    );
    assert_eq!(program.inputs_sent(), 0, "no key was sent");
    assert_eq!(program.requests(), 1, "the program was asked, once");
    assert_eq!(program.received(), None);
}

/// The request to delete `victim.bin` of `root`, which is what the dialogs of these tests show.
fn request<'a>(root: &'a FixtureRoot, sentinels: &'a [String]) -> DeletionRequest<'a> {
    DeletionRequest {
        name: "victim.bin",
        kind: EntryKind::File,
        fixture: root,
        sentinels,
        relative: "victim.bin",
        confirm_with: ConfirmKey::Y,
    }
}

#[test]
fn the_protocol_does_not_confirm_in_a_root_whose_marker_vanished_after_the_run_began() {
    // The program shows the right dialog for the right entry, and the marker is gone by then:
    // whatever removed it, the root is no longer one that the harness owns.
    let mut program = Scripted::start(true, |world| {
        format!(
            "stty raw -echo\n{map}frame 0\n{backspace}key\necho \"$byte\" > received\nsleep 60\n",
            map = map(),
            backspace = on_key(
                "177",
                &format!(
                    "rm '{}'\n{}frame 1",
                    world.marker.display(),
                    deletion_dialog(&world.victim())
                )
            ),
        )
    });
    program.wait_for("COMPLETE");
    let root = program.session.fixture.clone();
    let sentinels = vec!["keep-a.bin".to_owned()];

    let outcome = program
        .session
        .confirm_deletion(&request(&root, &sentinels), Instant::now() + PATIENCE);

    let Err(ProtocolError::Unmet(unmet)) = outcome else {
        panic!("a root without its marker must not be confirmed in");
    };
    assert_eq!(unmet.cause, FailureCause::DeleteRefused);
    assert!(
        unmet.observed.contains(MARKER_FILE_NAME),
        "{}",
        unmet.observed
    );
    assert!(
        unmet.observed.contains("no confirmation key was sent"),
        "{}",
        unmet.observed
    );
    assert_eq!(
        program.inputs_sent(),
        1,
        "only the Backspace that opens the dialog was sent"
    );
    assert_eq!(
        program.received(),
        None,
        "no confirmation reached the program"
    );
    assert!(
        !program.world.marker.exists(),
        "the premise of the test: the marker is gone"
    );
}

#[test]
fn the_protocol_waits_for_a_dialog_that_arrives_late_and_then_confirms_what_it_verified() {
    // The control for the tests above: the same program and request with the marker in place, and
    // the frame that opens the dialog drawn 300 ms after it is reported. The protocol writes a
    // barrier behind the Backspace, which the program reads once the dialog is drawn; it waits for
    // the mark of the frame that answers, reads the dialog, verifies it, and only then sends the
    // confirmation, which the script records.
    let mut program = Scripted::start(true, |world| {
        format!(
            "stty raw -echo\n{map}frame 0\n{backspace}{dialog}mark\nkey\necho \"$byte\" > confirmed\nsleep 60\n",
            map = map(),
            backspace = on_key("177", "report 1\nsleep 0.3"),
            dialog = deletion_dialog(&world.victim()),
        )
    });
    program.wait_for("COMPLETE");
    let root = program.session.fixture.clone();
    let sentinels = vec!["keep-a.bin".to_owned()];

    let outcome = program
        .session
        .confirm_deletion(&request(&root, &sentinels), Instant::now() + SCRIPT_LIMIT);

    let confirmed = match outcome {
        Ok(confirmed) => confirmed,
        Err(error) => panic!("the dialog is the right one: {error:?}"),
    };
    assert_eq!(confirmed.verified.relative, "victim.bin");
    assert_eq!(
        program.inputs_sent(),
        2,
        "the Backspace, and then the confirmation"
    );
    assert_eq!(
        program.requests(),
        1,
        "one barrier, written behind the Backspace"
    );
    // The script read the confirmation, and wrote what it was: `y`, in octal.
    assert_eq!(program.wait_for_file("confirmed").trim(), "171");
}

/// A program with `ConPTY`'s order. It shows dialog A, the dialog of `victim.bin`; then, for Esc,
/// Down (three bytes) and Backspace, it reports the frame and writes its mark at once, and paints
/// none of them: a console host that has the frames and has not painted them. The dialog that the
/// Backspace opens is another entry's, and the screen would still show dialog A.
fn conpty_program(world: &World) -> String {
    format!(
        "stty raw -echo\n{dialog}frame 0\n{esc}key\nkey\nkey\nframe 2\n{backspace}sleep 60\n",
        dialog = deletion_dialog(&world.victim()),
        esc = on_key("033", "frame 1"),
        backspace = on_key("177", "frame 3"),
    )
}

#[test]
fn nothing_is_confirmed_where_the_console_host_paints_on_its_own_timer() {
    // The screen shows dialog A, which is the dialog of the entry the request is for, and the
    // marks of the frames of Esc and Down have come with no paint after them: the program can be
    // on another entry's dialog by now, and a console host that paints on its own timer gives no
    // way to tell. The protocol sends nothing, not even the Backspace that opens a dialog.
    let mut program = Scripted::start(true, conpty_program);
    program.wait_for("DELETE FILE");
    program.on_a_console_host();
    program.press(&[0x1b]);
    program.press(b"\x1b[B");
    let root = program.session.fixture.clone();
    let sentinels = vec!["keep-a.bin".to_owned()];

    let outcome = program
        .session
        .confirm_deletion(&request(&root, &sentinels), Instant::now() + SCRIPT_LIMIT);

    let Err(ProtocolError::Unmet(unmet)) = outcome else {
        panic!("a screen that a console host paints on its own timer must not be confirmed from");
    };
    assert_eq!(unmet.cause, FailureCause::DeleteRefused);
    assert!(
        unmet.observed.contains("on its own timer") && unmet.observed.contains("no key was sent"),
        "{}",
        unmet.observed
    );
    assert_eq!(
        program.inputs_sent(),
        2,
        "Esc and Down, which the test sent: the protocol sent no Backspace and no confirmation"
    );
    assert_eq!(program.requests(), 0, "and no barrier");
}

#[test]
fn the_guard_sends_no_confirmation_where_the_console_host_paints_on_its_own_timer() {
    // Nothing was sent that could present a dialog and none is shown: on a terminal whose screen
    // is exact that is a screen to confirm from (see
    // `a_confirmation_is_allowed_on_a_map_with_no_dialog_once_the_barrier_is_answered`). Here it is
    // not, however long the guard waits.
    let mut program = idle_on_the_map(true);
    program.on_a_console_host();

    let refused = program
        .verdict_within(PATIENCE)
        .expect_err("a console host that paints on its own timer");

    assert!(
        refused.contains("on its own timer") && refused.contains("no key was sent"),
        "{refused}"
    );
    assert_eq!(program.inputs_sent(), 0, "the guard sent nothing");
    assert_eq!(program.requests(), 0, "not even a barrier");
}

#[test]
fn delete_refuses_a_console_host_that_paints_on_its_own_timer_before_sending_anything() {
    let mut program = idle_on_the_map(true);
    program.on_a_console_host();

    let error = program
        .session
        .delete_entry("victim.bin", EntryKind::File, Duration::from_secs(5))
        .expect_err("a console host that paints on its own timer");

    assert_eq!(error.kind, TuiErrorKind::Refused);
    assert!(
        error.message.contains("on its own timer") && error.message.contains("no key was sent"),
        "{}",
        error.message
    );
    assert_eq!(
        program.inputs_sent(),
        0,
        "not even the keys that select the entry"
    );
    assert_eq!(program.requests(), 0, "not even a barrier");
}

#[test]
fn the_reads_a_deletion_depends_on_are_not_satisfied_by_a_paint_on_a_console_host() {
    // The frame that counts the key is reported and marked, and the screen is painted after the
    // mark: a console host that has painted. The read that decides nothing destructive is
    // satisfied by that paint. The read a deletion depends on, which is the mark of the frame
    // that answers its barrier, is satisfied by nothing there, behind the refusal that a deletion
    // makes first; where the screen is exact it is the mark.
    let mut program = Scripted::start(true, |_| {
        format!(
            "stty raw -echo\n{map}frame 0\nkey\n{map}frame 1\nprintf PAINTED\nsleep 60\n",
            map = map()
        )
    });
    program.wait_for("COMPLETE");
    program.press(b"x");
    program.wait_for("PAINTED");

    let live = &mut program.session.live;
    let frame = live.latest_frame_seq().expect("the frame of the key");
    assert!(live.screen_is_exact(), "a Unix pseudo-terminal's screen is");
    assert!(live.screen_reflects_inputs());
    assert!(live.screen_surely_shows(frame));

    live.frame_window = CONPTY_FRAME_WINDOW;

    assert!(!live.screen_is_exact());
    assert!(
        live.screen_reflects_inputs(),
        "a paint followed the mark, which is all a read that decides nothing destructive asks for"
    );
    assert!(
        !live.screen_surely_shows(frame),
        "no paint of a console host proves the screen shows the frame"
    );
}
