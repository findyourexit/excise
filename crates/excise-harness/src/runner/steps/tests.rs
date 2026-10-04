//! The steps that end on a frame event, run against scripted programs.
//!
//! The programs are shell scripts (`runner::scripted`) that write the event channel and the
//! terminal the way `excise` does, and decide when each of them is written. Some say in their
//! `hello` that they mark their frames, as `excise` does, and some do not, as a build from before
//! the marks does not.
//!
//! One group has a program that marks its frames, and a terminal that delivers a frame late: the
//! program reports the frame at once, and the bytes of the frame and its mark follow 300 ms later,
//! a hundred times the quiet that a read that has to guess would wait for. A step has to wait for
//! the mark, and these tests give the executor no frame window at all, so that nothing but the
//! mark can keep it from reading the screen before the frame. A program that reports a frame and
//! never marks it must make the step time out, and not pass on a guess.
//!
//! One group has a program that does not mark its frames and a terminal that delivers a frame
//! late, the way `ConPTY` does: the program reports the frame at once, and the bytes of the frame
//! follow 30 ms later, ten times the quiet that the bounded tail waits for. These tests give the
//! executor a frame window far longer than that delay, so that a loaded machine, which can stall
//! the script's `sleep` for well over the delay, cannot fail them: what they check is that a step
//! reads the terminal for the whole window, not how long the window is. Windows' window is chosen
//! from measured `ConPTY` delays (see
//! [`CONPTY_FRAME_WINDOW`](super::super::live::CONPTY_FRAME_WINDOW)).
//!
//! One group has a program that counts an input of its own before it is sent any, the way `excise`
//! does on Windows, and then draws a frame that does not count the key before the frame that does,
//! 100 ms later. These tests give the executor a frame window far shorter than that, so that
//! nothing but the input baseline keeps a step from taking the first of the two frames for the
//! answer and reading the screen before the second.
//!
//! One group has a program whose selected-item panel trails its header: the panel shows nothing
//! selected at first, and names the entry 300 ms later, the way a fresh map arms its cursor in a
//! frame after the one that says `COMPLETE`. A deletion with no dialog has to wait for the panel
//! to name the entry, and refuse only when it never does. Another has a panel that shows the entry
//! the screen had before a key moved the selection: the frame that counts the key is reported at
//! once, and the panel that shows the move is drawn 300 ms later. Backspace must never reach it.
//!
//! One group is about what a deletion may be asked of: a program that does not mark its frames is
//! refused before any key is sent, and so is a terminal that paints on its own timer, the way
//! `ConPTY` does, and a root whose ownership marker vanished.
//!
//! The last group checks `expect_budget` against metrics that the test records itself, so that the
//! limit a step applies is the only thing that can decide the result and no timing is involved.
//!
//! The programs are shell scripts, so these tests run on Unix. A script that waits for a resize
//! sleeps in short steps rather than in one `wait` for a background `sleep`: a shell may run the
//! trap of a signal that arrives just before its `wait` starts only once that `wait` is over.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::Path,
    time::{Duration, Instant},
};

use super::super::{
    budget::LatencyScale,
    exec::Executor,
    live::{CONPTY_FRAME_WINDOW, Drive, Needs, ProtocolError, Waited},
    outcome::{FailureCause, Stop},
    plan::prepare,
    scripted::{COMPLETE_HEADER, boxed, place, prelude, prelude_with},
};
use crate::{
    fixture::{FixtureCache, FixtureSpec, Fixtures, MARKER_FILE_NAME},
    pty::{PtySession, SpawnSpec},
    report::TimingWarning,
    safety::{FixtureRoot, FixtureSnapshot, Scratch, isolated_env},
    scenario::{Budget, Profile, Scenario},
};

/// The frame window of the tests whose terminal delivers a frame 30 ms after its event, for a
/// program that does not mark its frames: far longer than that, so a stalled script still
/// delivers within it.
const LATE_FRAME_WINDOW: Duration = Duration::from_secs(2);

/// The frame window of the tests with a program that counts an input of its own: long enough for
/// bytes written just before their event to arrive, and far below the 100 ms between the frames.
const SHORT_FRAME_WINDOW: Duration = Duration::from_millis(30);

/// A frame window of nothing, for the tests that must pass on the mark alone.
const NO_FRAME_WINDOW: Duration = Duration::ZERO;

/// `body` as a program that does not mark its frames, as a build from before the marks does not.
fn legacy(body: &str) -> String {
    format!("{}{body}", prelude(false))
}

/// `body` as a program that says it marks its frames.
fn marked(body: &str) -> String {
    format!("{}{body}", prelude(true))
}

/// On Enter, reports the frame at once and draws it 30 ms later.
const ENTER_DRAWS_LATE: &str = r"
printf READY
read line
report 1
/bin/sleep 0.03
printf UPDATED
/bin/sleep 30
";

/// On a resize, reports the frame at once and draws it 30 ms later. `READY` follows the trap, so a
/// scenario that waits for it cannot resize before the program listens.
const RESIZE_DRAWS_LATE: &str = r"
trap 'report 1; /bin/sleep 0.03; printf UPDATED' WINCH
printf READY
i=0
while [ $i -lt 600 ]; do
  /bin/sleep 0.05
  i=$((i + 1))
done
";

/// On Enter, reports the frame and writes its mark at once, and draws it 60 ms later, as `ConPTY`
/// does: it passes the mark on when it parses it, and paints the screen on its own timer.
const ENTER_MARKS_FIRST_PAINTS_LATER: &str = r"
printf READY
read line
report 1
mark
/bin/sleep 0.06
printf UPDATED
/bin/sleep 30
";

/// On Enter, reports the frame and writes its mark, and draws nothing: the screen already showed
/// what the frame drew.
const ENTER_MARKS_AND_PAINTS_NOTHING: &str = r"
printf READY
read line
report 1
mark
/bin/sleep 30
";

/// A key and a settle with a bound far above anything the tests wait for.
const KEY_THEN_SETTLE_WITH_A_LONG_BOUND: &str = r#"
[[steps]]
step = "key"
key = "enter"

[[steps]]
step = "settle"
timeout_ms = 20000
"#;

/// On Enter, reports the frame at once, and draws it and marks it 300 ms later.
const ENTER_DRAWS_AND_MARKS_LATE: &str = r"
printf READY
read line
report 1
/bin/sleep 0.3
printf UPDATED
mark
/bin/sleep 30
";

/// On a resize, reports the frame at once, and draws it and marks it 300 ms later.
const RESIZE_DRAWS_AND_MARKS_LATE: &str = r"
trap 'report 1; /bin/sleep 0.3; printf UPDATED; mark' WINCH
printf READY
i=0
while [ $i -lt 600 ]; do
  /bin/sleep 0.05
  i=$((i + 1))
done
";

/// On Enter, reports the frame and draws it at once, and never marks it.
const ENTER_DRAWS_AND_NEVER_MARKS: &str = r"
printf READY
read line
report 1
printf UPDATED
/bin/sleep 30
";

/// Counts one input of its own before it is sent any. When Enter arrives it reports a frame that
/// does not count the key, an animation tick say, and 100 ms later draws and reports the frame that
/// does.
const COUNTS_ITS_OWN_INPUT: &str = r"
report 1
printf READY
read line
report 1
/bin/sleep 0.1
printf UPDATED
report 2
/bin/sleep 30
";

/// [`COUNTS_ITS_OWN_INPUT`] for a program that marks its frames: each frame is marked as it is
/// reported, and the mark of the frame that counts the key follows the bytes it draws.
const COUNTS_ITS_OWN_INPUT_AND_MARKS: &str = r"
frame 1
printf READY
read line
frame 1
/bin/sleep 0.1
printf UPDATED
frame 2
/bin/sleep 30
";

/// Counts one input of its own before it is sent any. A resize makes it report a frame that does
/// not count the resize at once, and 100 ms later draw and report the frame that does. The key
/// after that is counted on top of both. The handler reads the key itself: a shell does not
/// reliably go back to what it was doing once a trap has run.
const COUNTS_ITS_OWN_INPUT_BEFORE_A_RESIZE: &str = r"
trap 'report 1; /bin/sleep 0.1; printf UPDATED; report 2; read line; printf DONE; report 3' WINCH
report 1
printf READY
i=0
while [ $i -lt 600 ]; do
  /bin/sleep 0.05
  i=$((i + 1))
done
";

fn scenario(steps: &str) -> Scenario {
    scenario_with("", steps)
}

/// A scenario on the `delete-file` fixture whose first step waits for `READY`, with `top_level`
/// fields (which come before the steps) and then `steps`.
fn scenario_with(top_level: &str, steps: &str) -> Scenario {
    let scenario = Scenario::from_toml_str(&format!(
        "schema_version = 1\nname = \"scripted-program\"\n\
         description = \"a scripted program\"\n\
         fixture = \"delete-file\"\nprofiles = [\"default\"]\n{top_level}\
         [[steps]]\nstep = \"wait_text\"\ntext = \"READY\"\n{steps}"
    ))
    .expect("a scenario");
    scenario.validate().expect("a valid scenario");
    scenario
}

/// How a run of the steps ended, the metrics it had recorded by then, the timing budgets it had
/// missed without failing for them, and how many inputs it had sent the program.
struct Run {
    result: Result<(), Stop>,
    metrics: BTreeMap<String, f64>,
    warnings: Vec<TimingWarning>,
    inputs_sent: u64,
    /// What a script recorded in its file `received`: the keys it was not expecting, if any.
    received: Option<String>,
    /// How many input barrier requests a script read, when it counts them: it writes a byte to
    /// its file `requests` for each.
    requests: usize,
}

/// Runs `steps` after the script has printed `READY`, in an executor whose frame window is
/// `frame_window`, and stops the program.
fn run(script: &str, steps: &str, frame_window: Duration) -> Run {
    run_with(script, steps, |executor| {
        executor.live.frame_window = frame_window;
    })
}

/// Runs `steps` after the script has printed `READY`, in an executor that `configure` has set up,
/// and stops the program.
fn run_with(script: &str, steps: &str, configure: impl FnOnce(&mut Executor<'_>)) -> Run {
    run_scenario(script, &scenario(steps), configure)
}

/// Runs `scenario` against the script, in an executor that `configure` has set up, and stops the
/// program.
fn run_scenario(
    script: &str,
    scenario: &Scenario,
    configure: impl FnOnce(&mut Executor<'_>),
) -> Run {
    run_scenario_in(|_| script.to_owned(), scenario, configure)
}

/// Like [`run_scenario`], for a script that needs to know where the fixture is: `script` is given
/// the root of the fixture the program runs against.
fn run_scenario_in(
    script: impl FnOnce(&Path) -> String,
    scenario: &Scenario,
    configure: impl FnOnce(&mut Executor<'_>),
) -> Run {
    let (mut run, recorded) = drive_scenario_in(script, scenario, |executor| {
        configure(executor);
        let result = executor.run();
        Run {
            result,
            metrics: executor.metrics(),
            warnings: std::mem::take(&mut executor.timing_warnings),
            inputs_sent: executor.live.inputs_sent(),
            received: None,
            requests: 0,
        }
    });
    run.received = recorded.received;
    run.requests = recorded.requests;
    run
}

/// What a script recorded in the working directory of its program.
struct Recorded {
    /// The keys it was not expecting, if any (the file `received`).
    received: Option<String>,
    /// How many barrier requests it read (the file `requests`).
    requests: usize,
}

/// Starts the script as the program of an executor for `scenario`, lets `drive` use the executor
/// as it likes, and stops the program. `script` is given the root of the fixture the program runs
/// against.
fn drive_scenario_in<T>(
    script: impl FnOnce(&Path) -> String,
    scenario: &Scenario,
    drive: impl FnOnce(&mut Executor<'_>) -> T,
) -> (T, Recorded) {
    let work = tempfile::tempdir().expect("a work directory");
    let fixture_copy = Fixtures::new(
        FixtureSpec::bundled_dir(),
        FixtureCache::at(work.path().join("cache")),
    )
    .run_copy("delete-file", work.path())
    .expect("a fixture copy");
    let fixture = FixtureRoot::open(fixture_copy.root()).expect("an owned fixture");
    let scratch = Scratch::create(work.path()).expect("a scratch area");
    let baseline = FixtureSnapshot::take(fixture.path()).expect("a snapshot");
    let prepared = prepare(scenario).expect("a runnable scenario");
    let session = PtySession::spawn(&SpawnSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script(fixture.path()).into()],
        env: isolated_env(&scratch, Profile::Default, true, None),
        cwd: scratch.cwd(),
        cols: 80,
        rows: 24,
        drain_bytes_per_sec: None,
        recording: None,
        title: None,
    })
    .expect("the program starts");
    let mut executor = Executor::new(
        scenario,
        &prepared,
        &fixture,
        &scratch,
        &baseline,
        scratch.store(),
        session,
    );
    let driven = drive(&mut executor);
    executor.live.session.kill();
    let received = std::fs::read_to_string(scratch.cwd().join("received"))
        .ok()
        .filter(|text| !text.is_empty());
    let requests =
        std::fs::read_to_string(scratch.cwd().join("requests")).map_or(0, |text| text.len());
    (driven, Recorded { received, requests })
}

const SETTLE_THEN_EXPECT: &str = r#"
[[steps]]
step = "key"
key = "enter"

[[steps]]
step = "settle"

[[steps]]
step = "expect_screen"
contains = ["UPDATED"]
"#;

const RESIZE_THEN_EXPECT: &str = r#"
[[steps]]
step = "resize"
cols = 100
rows = 30

[[steps]]
step = "expect_screen"
contains = ["UPDATED"]
"#;

/// Waits for the frame that counts the key by its event, so that what a key's latency is timed to
/// does not depend on what `settle` accepts.
const KEY_THEN_THE_FRAME_THAT_COUNTS_IT: &str = r#"
[[steps]]
step = "key"
key = "enter"

[[steps]]
step = "wait_event"
event = "frame"
fields = { inputs = { min = 2 } }
"#;

const RESIZE_THEN_KEY_THEN_SETTLE: &str = r#"
[[steps]]
step = "resize"
cols = 100
rows = 30

[[steps]]
step = "expect_screen"
contains = ["UPDATED"]

[[steps]]
step = "key"
key = "enter"

[[steps]]
step = "settle"
timeout_ms = 1000

[[steps]]
step = "expect_screen"
contains = ["DONE"]
"#;

#[test]
fn settle_reads_a_frame_the_terminal_delivers_after_its_event() {
    let run = run(
        &legacy(ENTER_DRAWS_LATE),
        SETTLE_THEN_EXPECT,
        LATE_FRAME_WINDOW,
    );

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn resize_reads_a_frame_the_terminal_delivers_after_its_event() {
    let run = run(
        &legacy(RESIZE_DRAWS_LATE),
        RESIZE_THEN_EXPECT,
        LATE_FRAME_WINDOW,
    );

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn settle_waits_for_the_mark_of_a_frame_the_terminal_delivers_late() {
    // No frame window, and a delay that no quiet read would span: only the mark can tell.
    let run = run(
        &marked(ENTER_DRAWS_AND_MARKS_LATE),
        SETTLE_THEN_EXPECT,
        NO_FRAME_WINDOW,
    );

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn settle_waits_for_the_paint_that_follows_a_mark_where_the_terminal_paints_later() {
    // `ConPTY` passes the mark on when it parses it and paints the screen on its own timer, so the
    // mark arrives before the content of its frame. A frame window that is not zero says that the
    // terminal does: the step waits for the paint and not only for the mark. The window is far
    // longer than the 60 ms that the paint takes, so a stalled script cannot turn the paint into
    // one that nothing waited for.
    let run = run(
        &marked(ENTER_MARKS_FIRST_PAINTS_LATER),
        SETTLE_THEN_EXPECT,
        LATE_FRAME_WINDOW,
    );

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn settle_ends_when_the_window_has_passed_if_nothing_is_painted_after_the_mark() {
    // Nothing needed painting, so the screen already shows the frame: the step ends once the
    // window has passed after the mark, and not at its own bound.
    let window = Duration::from_millis(400);
    let started = Instant::now();
    let run = run(
        &marked(ENTER_MARKS_AND_PAINTS_NOTHING),
        KEY_THEN_SETTLE_WITH_A_LONG_BOUND,
        window,
    );
    let took = started.elapsed();

    assert!(run.result.is_ok(), "{:?}", run.result);
    assert!(
        took >= window,
        "the step ended {took:?} after the run began, inside the window"
    );
    assert!(
        took < Duration::from_secs(10),
        "the step ran to its bound: {took:?}"
    );
}

#[test]
fn resize_waits_for_the_mark_of_a_frame_the_terminal_delivers_late() {
    let run = run(
        &marked(RESIZE_DRAWS_AND_MARKS_LATE),
        RESIZE_THEN_EXPECT,
        NO_FRAME_WINDOW,
    );

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn settle_does_not_pass_on_a_guess_when_a_frame_is_never_marked() {
    // The program draws and reports the frame and says it marks its frames, and no mark comes: the
    // step waits for the mark and runs out of time, where a read that guesses would have passed.
    let run = run(
        &marked(ENTER_DRAWS_AND_NEVER_MARKS),
        &SETTLE_THEN_EXPECT.replace("step = \"settle\"", "step = \"settle\"\ntimeout_ms = 700"),
        LATE_FRAME_WINDOW,
    );

    let Err(Stop::Fail(failure)) = &run.result else {
        panic!(
            "a frame that is never marked must fail the step: {:?}",
            run.result
        );
    };
    assert_eq!(failure.cause, FailureCause::Timeout, "{failure}");
    assert_eq!(failure.index, 2, "{failure}");
    assert!(
        failure
            .detail
            .contains("the program has reported frame 1 and the screen shows frame 0"),
        "{failure}"
    );
}

#[test]
fn settle_waits_for_the_frame_that_counts_the_key_past_the_programs_own_input() {
    let run = run(
        &legacy(COUNTS_ITS_OWN_INPUT),
        SETTLE_THEN_EXPECT,
        SHORT_FRAME_WINDOW,
    );

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn settle_waits_for_the_mark_of_the_frame_that_counts_the_key_past_the_programs_own_input() {
    // The frame before it is reported and marked, and does not count the key: its mark is no
    // answer.
    let run = run(
        &marked(COUNTS_ITS_OWN_INPUT_AND_MARKS),
        SETTLE_THEN_EXPECT,
        NO_FRAME_WINDOW,
    );

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn a_key_is_timed_to_the_frame_that_counts_it_past_the_programs_own_input() {
    let run = run(
        &legacy(COUNTS_ITS_OWN_INPUT),
        KEY_THEN_THE_FRAME_THAT_COUNTS_IT,
        SHORT_FRAME_WINDOW,
    );

    assert!(run.result.is_ok(), "{:?}", run.result);
    assert!(
        (run.metrics["input_samples"] - 1.0).abs() < f64::EPSILON,
        "{:?}",
        run.metrics
    );
    // The frame that counts the key is reported 100 ms after the key arrives. The frame before it
    // follows the key within a few milliseconds, and is what a latency measured without the
    // baseline would be timed to.
    assert!(
        run.metrics["input_to_frame_max_ms"] >= 90.0,
        "{:?}",
        run.metrics
    );
}

#[test]
fn a_resize_and_the_key_after_it_wait_for_their_frames_past_the_programs_own_input() {
    let run = run(
        &legacy(COUNTS_ITS_OWN_INPUT_BEFORE_A_RESIZE),
        RESIZE_THEN_KEY_THEN_SETTLE,
        SHORT_FRAME_WINDOW,
    );

    assert!(run.result.is_ok(), "{:?}", run.result);
}

/// The selected-item pane as `excise` draws it, as `printf` commands that put it at row 10.
fn panel(lines: &[&str]) -> String {
    const WIDTH: usize = 60;
    let tab = " SELECTED ITEM ";
    let mut rows = vec![format!(
        "▟{tab}{}▜",
        "▔".repeat(WIDTH - 2 - tab.chars().count())
    )];
    for line in lines {
        let padding = WIDTH - 2 - line.chars().count();
        rows.push(format!(
            "▏{}{line}{}▕",
            " ".repeat(padding / 2),
            " ".repeat(padding - padding / 2)
        ));
    }
    rows.push("▔".repeat(WIDTH));
    let mut script = String::new();
    for (offset, row) in rows.iter().enumerate() {
        writeln!(script, "printf '\\033[{};1H%s' '{row}'", 10 + offset)
            .expect("a string takes a write");
    }
    script
}

/// A program whose panel names `victim.bin` 300 ms after it first shows nothing selected. Its
/// terminal is raw before it says `READY`, so that the Backspace byte, which a line discipline
/// would take for an erase, reaches it; it starts a deletion that finishes at once. It marks its
/// frames as it reports them.
fn panel_arrives_late() -> String {
    marked(&format!(
        "stty raw -echo\nprintf READY\n{nothing}frame 0\n/bin/sleep 0.3\n{victim}frame 0\n\
         key\nframe 1\n\
         printf '{{\"v\":1,\"kind\":\"deletion_finished\",\"removed\":1,\"failed\":0,\"t_us\":2}}\\n' \
         >> \"$events\"\nframe 1\nkey\n/bin/sleep 30\n",
        nothing = panel(&["Choose an item to see its space and scan status."]),
        victim = panel(&["victim.bin", "◆ COMPLETE · file"]),
    ))
}

/// A program whose panel names another entry, and keeps naming it.
fn panel_names_another_entry() -> String {
    marked(&format!(
        "stty raw -echo\nprintf READY\n{other}frame 0\nkey\n/bin/sleep 30\n",
        other = panel(&["other.bin", "◆ COMPLETE · file"]),
    ))
}

/// A program whose panel names `victim.bin`, and whose selection moves to `other.bin` at the first
/// key: it reports the frame that counts the key at once, and draws its panel, and marks it, 300 ms
/// later. What it reads after that, a barrier request aside, is a Backspace that got through, and
/// it records it in `received`.
fn selection_moves_late() -> String {
    marked(&format!(
        "stty raw -echo\nprintf READY\n{victim}frame 0\n\
         key\nreport 1\n/bin/sleep 0.3\n{other}mark\nkey\necho \"$byte\" > received\n\
         /bin/sleep 30\n",
        victim = panel(&["victim.bin", "◆ COMPLETE · file"]),
        other = panel(&["other.bin", "◆ COMPLETE · file"]),
    ))
}

/// A scenario with no confirmation dialog whose only step deletes `victim.bin`, which the step
/// gives `timeout_ms` to be the panel's entry.
fn delete_victim_with_no_dialog(timeout_ms: u64) -> Scenario {
    scenario_with(
        "sentinels = [\"keep-a.bin\"]\ndisable_delete_confirmation = true\n",
        &format!(
            "\n[[steps]]\nstep = \"delete\"\nname = \"victim.bin\"\nkind = \"file\"\n\
             timeout_ms = {timeout_ms}\n"
        ),
    )
}

#[test]
fn a_deletion_with_no_dialog_waits_for_a_panel_that_trails_the_header() {
    let scenario = delete_victim_with_no_dialog(10_000);

    let run = run_scenario(&panel_arrives_late(), &scenario, |executor| {
        executor.live.frame_window = NO_FRAME_WINDOW;
    });

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn a_deletion_with_no_dialog_refuses_a_panel_that_never_names_the_entry() {
    let scenario = delete_victim_with_no_dialog(1_000);

    let run = run_scenario(&panel_names_another_entry(), &scenario, |executor| {
        executor.live.frame_window = NO_FRAME_WINDOW;
    });

    let Err(Stop::Fail(failure)) = &run.result else {
        panic!("another entry must be refused: {:?}", run.result);
    };
    assert_eq!(failure.cause, FailureCause::DeleteRefused, "{failure}");
    assert!(
        failure.detail.contains(
            "the selected-item panel shows `other.bin`, but the step deletes `victim.bin`"
        ),
        "{failure}"
    );
    assert_eq!(run.inputs_sent, 0, "Backspace was never sent");
}

#[test]
fn a_deletion_with_no_dialog_does_not_trust_the_panel_a_key_has_just_made_stale() {
    // The panel on the screen names `victim.bin`, and the frame that counts the key that moved the
    // selection has been reported and not delivered. The panel is what the screen showed before
    // the key, and Backspace would delete what is selected now, which is `other.bin`. The step asks
    // the program whether it has read the key, and the answer comes after the panel that shows it.
    let scenario = scenario_with(
        "sentinels = [\"keep-a.bin\"]\ndisable_delete_confirmation = true\n",
        "\n[[steps]]\nstep = \"key\"\nkey = \"tab\"\n\n\
         [[steps]]\nstep = \"delete\"\nname = \"victim.bin\"\nkind = \"file\"\ntimeout_ms = 1500\n",
    );

    let run = run_scenario(&selection_moves_late(), &scenario, |executor| {
        executor.live.frame_window = NO_FRAME_WINDOW;
    });

    let Err(Stop::Fail(failure)) = &run.result else {
        panic!("a stale panel must not be trusted: {:?}", run.result);
    };
    assert_eq!(failure.cause, FailureCause::DeleteRefused, "{failure}");
    assert!(
        failure.detail.contains(
            "the selected-item panel shows `other.bin`, but the step deletes `victim.bin`"
        ),
        "{failure}"
    );
    assert_eq!(
        run.inputs_sent, 1,
        "only the key that moved the selection was sent, never a Backspace"
    );
}

/// A program that reads the two bytes of `alt+esc` as two events, as a terminal can cut them.
/// `victim.bin` is selected inside a folder: the first Esc leaves it, and the map above selects the
/// folder it came from, which the panel still shows as `victim.bin`; the second Esc, read 400 ms
/// later, leaves the folder above, and the panel then names `parent`. What it reads after that, a
/// barrier request aside, is a Backspace that got through, and it records it in `received`.
fn escape_pair_read_in_two_events() -> String {
    marked(&format!(
        "stty raw -echo\nprintf READY\n{victim}frame 0\n\
         key\n[ \"$byte\" = 033 ] || exit 9\n{victim}frame 1\n\
         /bin/sleep 0.4\n\
         key\n[ \"$byte\" = 033 ] || exit 9\n{parent}frame 2\n\
         key\necho \"$byte\" > received\n/bin/sleep 30\n",
        victim = panel(&["victim.bin", "◆ COMPLETE · file"]),
        parent = panel(&["parent", "◆ COMPLETE · folder"]),
    ))
}

#[test]
fn a_deletion_with_no_dialog_does_not_send_backspace_behind_an_escape_pair_that_is_half_read() {
    // `key esc` with `alt` is the bytes ESC ESC in one write: one input, which a program can read
    // as one event or as two. The program here reads the first Esc, draws the frame that counts
    // the one input sent, with the panel naming `victim.bin`, and reads the second Esc 400 ms
    // later. Counting the inputs sent against the frames drawn takes the first frame for the whole
    // key: Backspace would be written behind the second Esc, which moves the selection to
    // `parent`, and delete that. The step asks the program whether it has read everything instead,
    // and the answer comes after the second Esc, with the panel that shows it.
    let scenario = scenario_with(
        "sentinels = [\"keep-a.bin\"]\ndisable_delete_confirmation = true\n",
        "\n[[steps]]\nstep = \"key\"\nkey = \"esc\"\nalt = true\n\n\
         [[steps]]\nstep = \"delete\"\nname = \"victim.bin\"\nkind = \"file\"\ntimeout_ms = 1500\n",
    );

    let run = run_scenario(&escape_pair_read_in_two_events(), &scenario, |executor| {
        executor.live.frame_window = NO_FRAME_WINDOW;
    });

    let Err(Stop::Fail(failure)) = &run.result else {
        panic!("a selection that moved must be refused: {:?}", run.result);
    };
    assert_eq!(failure.cause, FailureCause::DeleteRefused, "{failure}");
    assert!(
        failure
            .detail
            .contains("the selected-item panel shows `parent`, but the step deletes `victim.bin`"),
        "{failure}"
    );
    assert_eq!(
        run.inputs_sent, 1,
        "only the key was sent, never a Backspace"
    );
    assert_eq!(
        run.received, None,
        "no byte reached the program after the pair"
    );
}

#[test]
fn a_deletion_with_no_dialog_waits_for_the_program_to_answer_the_barrier_behind_its_backspace() {
    // The program reads the Backspace, reports the frame that counts it and the end of the
    // deletion, and answers no barrier after it. A frame that counts the Backspace is not an answer:
    // the step does not take it for one, and fails at its bound, saying what it waited for.
    let scenario = delete_victim_with_no_dialog(1_500);
    let script = marked(&format!(
        "stty raw -echo\nprintf READY\n{victim}frame 0\nkey\nframe 1\n\
         printf '{{\"v\":1,\"kind\":\"deletion_finished\",\"removed\":1,\"failed\":0,\"t_us\":2}}\\n' \
         >> \"$events\"\nframe 1\n/bin/sleep 30\n",
        victim = panel(&["victim.bin", "◆ COMPLETE · file"]),
    ));

    let run = run_scenario(&script, &scenario, |executor| {
        executor.live.frame_window = NO_FRAME_WINDOW;
    });

    let Err(Stop::Fail(failure)) = &run.result else {
        panic!("a frame is not an answer to a barrier: {:?}", run.result);
    };
    assert_eq!(failure.cause, FailureCause::Timeout, "{failure}");
    assert!(
        failure
            .expected
            .contains("an input barrier written behind the Backspace"),
        "{failure}"
    );
    assert_eq!(run.inputs_sent, 1, "the Backspace, which was the point");
}

/// A step that deletes `victim.bin`, whatever the program shows.
const DELETE_VICTIM: &str =
    "\n[[steps]]\nstep = \"delete\"\nname = \"victim.bin\"\nkind = \"file\"\ntimeout_ms = 3000\n";

#[test]
fn a_deletion_is_refused_before_any_key_when_the_program_does_not_mark_its_frames() {
    // The dialog a deletion reads, and the panel a deletion with no dialog reads, can each be a
    // frame behind the program, and nothing a program that does not mark its frames reports says
    // when they are not. Not one key is sent.
    for (what, top_level) in [
        ("with a dialog", "sentinels = [\"keep-a.bin\"]\n"),
        (
            "with no dialog",
            "sentinels = [\"keep-a.bin\"]\ndisable_delete_confirmation = true\n",
        ),
    ] {
        let scenario = scenario_with(top_level, DELETE_VICTIM);

        let run = run_scenario(&legacy("printf READY\n/bin/sleep 30\n"), &scenario, |_| {});

        let Err(Stop::Fail(failure)) = &run.result else {
            panic!(
                "{what}: a program that does not mark its frames: {:?}",
                run.result
            );
        };
        assert_eq!(
            failure.cause,
            FailureCause::DeleteRefused,
            "{what}: {failure}"
        );
        assert!(
            failure.detail.contains("frame_marks") && failure.detail.contains("no key was sent"),
            "{what}: {failure}"
        );
        assert_eq!(run.inputs_sent, 0, "{what}: nothing was sent");
    }
}

#[test]
fn a_deletion_is_refused_before_any_key_when_the_program_does_not_answer_a_barrier() {
    // A program that marks its frames and answers no barrier request (a build from between the two)
    // cannot say that it has read what it was written, so a confirmation could reach a dialog that
    // a key still unread opens. Not one key is sent, a barrier included.
    for (what, top_level) in [
        ("with a dialog", "sentinels = [\"keep-a.bin\"]\n"),
        (
            "with no dialog",
            "sentinels = [\"keep-a.bin\"]\ndisable_delete_confirmation = true\n",
        ),
    ] {
        let scenario = scenario_with(top_level, DELETE_VICTIM);
        let script = format!(
            "{}stty raw -echo\nprintf READY\nframe 0\nkey\necho \"$byte\" > received\n/bin/sleep 30\n",
            prelude_with(true, false)
        );

        let run = run_scenario(&script, &scenario, |_| {});

        let Err(Stop::Fail(failure)) = &run.result else {
            panic!(
                "{what}: a program that answers no barrier: {:?}",
                run.result
            );
        };
        assert_eq!(
            failure.cause,
            FailureCause::DeleteRefused,
            "{what}: {failure}"
        );
        assert!(
            failure.detail.contains("input_barrier") && failure.detail.contains("no key was sent"),
            "{what}: {failure}"
        );
        assert_eq!(run.inputs_sent, 0, "{what}: nothing was sent");
        assert_eq!(run.received, None, "{what}: not even a barrier was written");
    }
}

#[test]
fn a_deletion_is_refused_before_any_key_where_the_terminal_paints_on_its_own_timer() {
    // The program marks its frames and shows what a deletion needs, an empty map for the dialog
    // and the right entry's panel for the mode with no dialog. Its terminal, though, paints on its
    // own timer, the way `ConPTY` does, and no paint of such a terminal proves that the screen
    // shows the frame whose mark was read: a deletion is confirmed from no such screen. Not one
    // key is sent, a Backspace included.
    for (what, top_level, script) in [
        (
            "with a dialog",
            "sentinels = [\"keep-a.bin\"]\n",
            marked("stty raw -echo\nprintf READY\nframe 0\n/bin/sleep 30\n"),
        ),
        (
            "with no dialog",
            "sentinels = [\"keep-a.bin\"]\ndisable_delete_confirmation = true\n",
            marked(&format!(
                "stty raw -echo\nprintf READY\n{victim}frame 0\n/bin/sleep 30\n",
                victim = panel(&["victim.bin", "◆ COMPLETE · file"]),
            )),
        ),
    ] {
        let scenario = scenario_with(top_level, DELETE_VICTIM);

        let run = run_scenario(&script, &scenario, |executor| {
            executor.live.frame_window = CONPTY_FRAME_WINDOW;
        });

        let Err(Stop::Fail(failure)) = &run.result else {
            panic!(
                "{what}: a screen that a console host paints on its own timer: {:?}",
                run.result
            );
        };
        assert_eq!(
            failure.cause,
            FailureCause::DeleteRefused,
            "{what}: {failure}"
        );
        assert!(
            failure.detail.contains("on its own timer")
                && failure.detail.contains("no key was sent"),
            "{what}: {failure}"
        );
        assert_eq!(run.inputs_sent, 0, "{what}: nothing was sent");
    }
}

#[test]
fn a_deletion_with_no_dialog_sends_no_backspace_in_a_root_whose_marker_vanished() {
    // The program shows the panel of the right entry, and the marker is gone before it does:
    // whatever removed it, the root is no longer one that the harness owns, and the key that
    // deletes is Backspace.
    let scenario = delete_victim_with_no_dialog(5_000);

    let run = run_scenario_in(
        |root| {
            marked(&format!(
                "rm '{}'\nstty raw -echo\nprintf READY\n{victim}frame 0\nkey\n/bin/sleep 30\n",
                root.join(MARKER_FILE_NAME).display(),
                victim = panel(&["victim.bin", "◆ COMPLETE · file"]),
            ))
        },
        &scenario,
        |_| {},
    );

    let Err(Stop::Fail(failure)) = &run.result else {
        panic!(
            "a root without its marker must not be deleted in: {:?}",
            run.result
        );
    };
    assert_eq!(failure.cause, FailureCause::DeleteRefused, "{failure}");
    assert!(
        failure.detail.contains(MARKER_FILE_NAME) && failure.detail.contains("no key was sent"),
        "{failure}"
    );
    assert_eq!(run.inputs_sent, 0, "not even Backspace");
}

/// What every program of the tests below starts with: raw input, and `READY` on a row of its own,
/// below everything the program draws, so that no screen replaces it before the first step has
/// seen it.
const READY: &str = "stty raw -echo\nprintf '\\033[22;1HREADY'\n";

/// A dialog in the middle of the 80-column terminal of these tests, as `printf` commands that
/// draw it over whatever the screen shows.
fn dialog(title: &str, lines: &[&str]) -> String {
    place(8, 11, &boxed(title, 60, lines))
}

/// The header of a map whose scan is complete, as `printf` commands.
fn map() -> String {
    place(1, 1, &[COMPLETE_HEADER.to_owned()])
}

/// The deletion dialog of `victim.bin`, which `y` or Enter confirms.
fn deletion_dialog() -> String {
    dialog(
        "! DELETE FILE",
        &["/fixture/victim.bin", "[Enter/y] start    [n] cancel"],
    )
}

/// The quit prompt that offers `[y] Quit`, with nothing waiting beneath it.
fn plain_quit_prompt() -> String {
    dialog(
        "QUIT",
        &["Quit Excise?", "", "[y] Quit", "[Esc/q/n] Keep working"],
    )
}

/// The quit prompt of a program with a deletion check waiting, which offers no `[y] Quit`.
fn waiting_quit_prompt() -> String {
    dialog(
        "QUIT",
        &[
            "1 deletion check(s) are waiting.",
            "",
            "[c] Cancel checks and quit",
            "[Esc/q/n] Keep working",
        ],
    )
}

/// The filter prompt open and empty in the header, as `excise` draws it, as `printf` commands.
fn open_filter_prompt() -> String {
    place(1, 1, &["/ _  [Enter] apply  [Esc] cancel".to_owned()])
}

/// A scenario that presses Backspace, which the program can read as a request for a deletion
/// that no `delete` step verifies, and then runs `steps`.
fn after_a_raw_backspace(steps: &str) -> Scenario {
    scenario(&format!(
        "\n[[steps]]\nstep = \"key\"\nkey = \"backspace\"\n{steps}"
    ))
}

/// A program that reads one byte, which is the Backspace, shows `shown` as the frame that counts
/// it, and records the next byte that is not a barrier request, a keypress that got through, in
/// `received`.
fn after_backspace_shows(shown: &str) -> String {
    marked(&format!(
        "{READY}{map}frame 0\nkey\n{shown}frame 1\nkey\n\
         echo \"$byte\" > received\n/bin/sleep 30\n",
        map = map(),
    ))
}

/// What a run that was refused says: the failure, its cause checked. That no key that could
/// confirm went out is checked first, from the inputs the run sent, `inputs_sent`.
fn refusal(run: &Run, what: &str, inputs_sent: u64) -> String {
    assert_eq!(
        run.inputs_sent, inputs_sent,
        "{what}: only the keys before it were sent, never one that could confirm: {:?}",
        run.result
    );
    let Err(Stop::Fail(failure)) = &run.result else {
        panic!("{what}: the key must be refused: {:?}", run.result);
    };
    assert_eq!(
        failure.cause,
        FailureCause::DeleteRefused,
        "{what}: {failure}"
    );
    failure.detail.clone()
}

#[test]
fn a_quit_after_a_raw_backspace_presses_no_y_on_a_prompt_the_screen_shows_stale() {
    // The Backspace opened a deletion dialog, and the program was sent `q` while a prompt covered
    // it, so that the `q` uncovered the dialog. The program has reported the frame that counts the
    // `q`, and a `quit_prompt` event that belongs to the prompt before it, and has not drawn the
    // dialog: the screen still shows the plain quit prompt that an earlier `q` opened. Taking the
    // event and that screen for the answer to the `q` presses `y` in the dialog. The step asks the
    // program whether it has read everything, and reads the screen that answers: the dialog.
    let scenario = after_a_raw_backspace("\n[[steps]]\nstep = \"quit\"\ntimeout_ms = 3000\n");
    let script = marked(&format!(
        "{READY}{map}{prompt}frame 0\nkey\nkey\nreport 2\n\
         printf '{{\"v\":1,\"kind\":\"quit_prompt\",\"t_us\":2}}\\n' >> \"$events\"\n\
         /bin/sleep 0.4\n{dialog}mark\nkey\necho \"$byte\" > received\n/bin/sleep 30\n",
        map = map(),
        prompt = plain_quit_prompt(),
        dialog = deletion_dialog(),
    ));

    let run = run_scenario(&script, &scenario, |_| {});

    let detail = refusal(&run, "a stale quit prompt", 2);
    assert!(
        detail.contains("a deletion dialog is open") && detail.contains("no key was sent"),
        "{detail}"
    );
    assert_eq!(run.requests, 1, "behind a barrier");
    assert_eq!(run.received, None, "nothing reached the program after it");
}

#[test]
fn a_select_after_a_raw_backspace_types_no_y_into_a_filter_prompt_the_screen_shows_stale() {
    // The Backspace opened a deletion dialog, and the program has counted more inputs than the
    // runner has sent (a key it read as two events does that): its latest frame, which shows an
    // empty filter prompt from before the dialog, counts the Backspace and the `/` and one more.
    // The count says that the frame answers the `/` and the screen shows a filter prompt, so the
    // name `y` would be typed, and the next byte the program reads would be a `y` in the dialog.
    // The step asks the program whether it has read everything, and reads the screen that answers.
    let scenario =
        after_a_raw_backspace("\n[[steps]]\nstep = \"select\"\nname = \"y\"\ntimeout_ms = 3000\n");
    let script = marked(&format!(
        "answer_barriers=0\n{READY}{prompt}frame 0\nkey\nkey\nframe 3\nkey\n\
         if [ \"$byte\" = 035 ]; then\n{dialog}answer\nkey\nfi\n\
         echo \"$byte\" > received\n/bin/sleep 30\n",
        prompt = open_filter_prompt(),
        dialog = deletion_dialog(),
    ));

    let run = run_scenario(&script, &scenario, |_| {});

    let detail = refusal(&run, "a stale filter prompt", 2);
    assert!(
        detail.contains("a deletion dialog is open") && detail.contains("no key was sent"),
        "{detail}"
    );
    assert_eq!(run.requests, 1, "behind a barrier");
    assert_eq!(run.received, None, "nothing reached the program after it");
}

#[test]
fn a_key_that_could_confirm_is_refused_after_a_raw_backspace_while_a_dialog_is_open() {
    // The dialog is on the screen that answers the barrier, whichever step sends the key.
    for (what, steps) in [
        ("`key y`", "\n[[steps]]\nstep = \"key\"\nkey = \"y\"\n"),
        (
            "`key enter`",
            "\n[[steps]]\nstep = \"key\"\nkey = \"enter\"\n",
        ),
        ("`type y`", "\n[[steps]]\nstep = \"type\"\ntext = \"y\"\n"),
    ] {
        let scenario = after_a_raw_backspace(steps);

        let run = run_scenario(
            &after_backspace_shows(&deletion_dialog()),
            &scenario,
            |_| {},
        );

        let detail = refusal(&run, what, 1);
        assert!(
            detail.contains("a deletion dialog is open") && detail.contains("no key was sent"),
            "{what}: {detail}"
        );
        assert_eq!(run.requests, 1, "{what}: behind a barrier");
        assert_eq!(run.received, None, "{what}: nothing reached the program");
    }
}

#[test]
fn a_key_that_could_confirm_is_sent_after_a_raw_backspace_behind_a_barrier_when_no_dialog_is_open()
{
    // The Backspace opened nothing (the terminal is too small for the dialog, or nothing was
    // selected): the screen that answers the barrier shows no dialog, and the key goes out.
    let scenario = after_a_raw_backspace("\n[[steps]]\nstep = \"key\"\nkey = \"y\"\n");

    let run = run_scenario(&after_backspace_shows(""), &scenario, |_| {});

    assert!(run.result.is_ok(), "{:?}", run.result);
    assert_eq!(run.inputs_sent, 2, "the Backspace and the `y`");
    assert_eq!(run.requests, 1, "the `y` went out behind one barrier");
}

#[test]
fn a_run_that_never_asked_for_a_deletion_sends_its_keys_at_once_and_writes_no_barrier() {
    // Until a key of the scenario can be read as a request for a deletion, nothing is pending
    // that a confirmation could meet, and every key goes out as it always did.
    let scenario = scenario(
        "\n[[steps]]\nstep = \"key\"\nkey = \"y\"\n\n[[steps]]\nstep = \"key\"\nkey = \"enter\"\n",
    );
    let script = marked(&format!(
        "{READY}{map}frame 0\nkey\nkey\n\
         echo \"$byte\" > received\n/bin/sleep 30\n",
        map = map(),
    ));

    let run = run_scenario(&script, &scenario, |_| {});

    assert!(run.result.is_ok(), "{:?}", run.result);
    assert_eq!(run.inputs_sent, 2);
    assert_eq!(run.requests, 0, "no barrier was written");
}

#[test]
fn an_escape_and_a_confirmation_in_one_key_are_refused_after_a_raw_backspace() {
    // `alt+y` is one write that a program may read as `Esc` and `y`: the escape can close a prompt
    // that covers a dialog, and the `y` confirms what it uncovers. No barrier can come between the
    // two bytes, so no screen can be read between them either.
    let scenario = after_a_raw_backspace("\n[[steps]]\nstep = \"key\"\nkey = \"y\"\nalt = true\n");

    let run = run_scenario(&after_backspace_shows(""), &scenario, |_| {});

    let detail = refusal(&run, "`alt+y`", 1);
    assert!(
        detail.contains("the escape byte") && detail.contains("no key was sent"),
        "{detail}"
    );
    assert_eq!(run.requests, 0, "not even a barrier was written");
    assert_eq!(run.received, None);
}

/// A program that reads the Backspace, shows `shown` as the frame that counts it, and reads the
/// two bytes of `alt+[`, `ESC` and `[`. Then it leaves barrier requests unanswered, as a program
/// in the middle of an escape sequence does (it takes the request for a byte of the sequence),
/// and records the next byte, the one that would go on with the sequence, in `received`.
fn after_backspace_and_alt_bracket(shown: &str) -> String {
    marked(&format!(
        "{READY}{map}frame 0\nkey\n{shown}frame 1\nkey\nkey\nanswer_barriers=0\nkey\n\
         echo \"$byte\" > received\n/bin/sleep 30\n",
        map = map(),
    ))
}

/// A program that reads the Backspace, shows `shown` as the frame that counts it, and reads one
/// byte more, the `ESC` of an `esc` key. Then it answers barrier requests, as `excise` does,
/// records the next byte that is not one in `received`, and draws `RECORDED`.
fn after_backspace_and_an_escape(shown: &str) -> String {
    marked(&format!(
        "{READY}{map}frame 0\nkey\n{shown}frame 1\nkey\nkey\n\
         echo \"$byte\" > received\nprintf RECORDED\n/bin/sleep 30\n",
        map = map(),
    ))
}

#[test]
fn a_sequence_that_a_key_begins_and_text_finishes_is_refused_after_a_raw_backspace() {
    // The Backspace opened a deletion dialog. `alt+[` is `ESC [`, and the text that follows is the
    // rest of `ESC [ 121 u`, which the program reads as `y` (`13u` and `57414u` are Enter, and
    // `97:121;2u` is `y` through its shifted alternate), though no write holds a `y`, an Enter, or
    // a line feed. The program joins the bytes of a sequence across writes, so the first byte of
    // the text is the one that continues it, whatever it is, and no barrier can be written behind
    // `ESC [`: the program takes it for a byte of the sequence and never answers it.
    for text in ["121u", "13u", "57414u", "97:121;2u"] {
        let what = format!("`alt+[` and `{text}`");
        let scenario = after_a_raw_backspace(&format!(
            "\n[[steps]]\nstep = \"key\"\nkey = \"[\"\nalt = true\n\
             \n[[steps]]\nstep = \"type\"\ntext = \"{text}\"\n"
        ));

        let run = run_scenario(
            &after_backspace_and_alt_bracket(&deletion_dialog()),
            &scenario,
            |_| {},
        );

        let detail = refusal(&run, &what, 2);
        assert!(
            detail.contains("escape sequence") && detail.contains("no key was sent"),
            "{what}: {detail}"
        );
        assert_eq!(run.requests, 0, "{what}: no barrier was written behind it");
        assert_eq!(run.received, None, "{what}: nothing finished the sequence");
    }
}

#[test]
fn a_sequence_that_a_key_begins_is_refused_even_where_no_dialog_is_open() {
    // The refusal does not depend on what the screen shows: with `ESC [` unfinished, whatever is
    // written next can finish it as a key that confirms, and no screen can be read after a
    // barrier that the program cannot answer.
    let scenario = after_a_raw_backspace(
        "\n[[steps]]\nstep = \"key\"\nkey = \"[\"\nalt = true\n\
         \n[[steps]]\nstep = \"type\"\ntext = \"121u\"\n",
    );

    let run = run_scenario(&after_backspace_and_alt_bracket(""), &scenario, |_| {});

    let detail = refusal(&run, "`alt+[` and `121u`, with no dialog", 2);
    assert!(
        detail.contains("escape sequence") && detail.contains("no key was sent"),
        "{detail}"
    );
    assert_eq!(run.requests, 0, "no barrier was written behind it");
    assert_eq!(run.received, None, "nothing finished the sequence");
}

#[test]
fn a_lone_escape_and_the_text_after_it_are_refused_after_a_raw_backspace_while_a_dialog_is_open() {
    // `esc` and then `[121u`: a program that reads the `ESC` and the `[` in one read reads
    // `ESC [`, and the rest finishes `ESC [ 121 u`. The `[` can continue a sequence, so it goes
    // out behind a barrier, which keeps the two apart, and only if the screen that answers shows
    // no deletion dialog.
    let scenario = after_a_raw_backspace(
        "\n[[steps]]\nstep = \"key\"\nkey = \"esc\"\n\
         \n[[steps]]\nstep = \"type\"\ntext = \"[121u\"\n",
    );

    let run = run_scenario(
        &after_backspace_and_an_escape(&deletion_dialog()),
        &scenario,
        |_| {},
    );

    let detail = refusal(&run, "`esc` and `[121u`", 2);
    assert!(
        detail.contains("a deletion dialog is open") && detail.contains("no key was sent"),
        "{detail}"
    );
    assert_eq!(run.requests, 1, "behind a barrier");
    assert_eq!(run.received, None, "the bracket was not sent");
}

#[test]
fn a_lone_escape_and_the_text_after_it_are_kept_apart_by_a_barrier_where_no_dialog_is_open() {
    // The Backspace opened nothing: the bracket goes out behind one barrier, which separates it
    // from the escape, so no sequence forms and the rest of the text is only text.
    let scenario = after_a_raw_backspace(
        "\n[[steps]]\nstep = \"key\"\nkey = \"esc\"\n\
         \n[[steps]]\nstep = \"type\"\ntext = \"[121u\"\n\
         \n[[steps]]\nstep = \"wait_text\"\ntext = \"RECORDED\"\n",
    );

    let run = run_scenario(&after_backspace_and_an_escape(""), &scenario, |_| {});

    assert!(run.result.is_ok(), "{:?}", run.result);
    assert_eq!(
        run.inputs_sent, 7,
        "the Backspace, the escape, and the five characters"
    );
    assert_eq!(run.requests, 1, "the bracket went out behind one barrier");
    assert_eq!(
        run.received.as_deref().map(str::trim),
        Some("133"),
        "the bracket is the byte the program read after the barrier"
    );
}

#[test]
fn the_writes_of_the_protocols_are_scanned_with_the_keys_of_the_scenario() {
    // Nothing that a protocol writes begins a sequence, so none of them is a way for the scan to
    // lose one: `select`, `delete`, and `quit` write `/`, the characters of a name, Enter,
    // Backspace, `q`, and `y`, and never the escape byte (the driver's `clear_filter` does, and
    // the scan sees it as it sees any other write). A write that is made through the protocols'
    // path is read all the same.
    let scenario = scenario("");
    let ((before, after), _) = drive_scenario_in(
        |_| marked(&format!("{READY}{}frame 0\n/bin/sleep 30\n", map())),
        &scenario,
        |executor| {
            executor
                .send_input(&[0x1b])
                .expect("the escape is written like any write of a protocol");
            let before = executor.live.reading_of(b"[");
            executor
                .send_input(b"x")
                .expect("the next write is written");
            let after = executor.live.reading_of(b"[");
            (before, after)
        },
    );

    assert!(
        before.request && before.confirmation,
        "an escape written by a protocol is held, so that a `[` after it continues a sequence"
    );
    assert!(
        !after.request && !after.confirmation,
        "a write that is not a continuation ends it"
    );
}

#[test]
fn no_key_that_could_confirm_is_sent_after_a_raw_backspace_where_the_screen_cannot_be_trusted() {
    // A terminal that paints on its own timer, a program that does not mark its frames, and one
    // that answers no barrier: none of them gives a screen to read a dialog from, so a key that
    // could confirm one is refused, as a `delete` step is.
    let steps = "\n[[steps]]\nstep = \"key\"\nkey = \"y\"\n";
    let marks_and_answers = after_backspace_shows("");
    let no_barrier = format!(
        "{}{READY}{}frame 0\nkey\nkey\necho \"$byte\" > received\n/bin/sleep 30\n",
        prelude_with(true, false),
        map(),
    );
    let no_marks = format!(
        "{}{READY}{}frame 0\nkey\nkey\necho \"$byte\" > received\n/bin/sleep 30\n",
        prelude(false),
        map(),
    );
    for (what, script, frame_window, said) in [
        (
            "a terminal that paints on its own timer",
            &marks_and_answers,
            CONPTY_FRAME_WINDOW,
            "on its own timer",
        ),
        (
            "a program that does not answer a barrier",
            &no_barrier,
            NO_FRAME_WINDOW,
            "input_barrier",
        ),
        (
            "a program that does not mark its frames",
            &no_marks,
            NO_FRAME_WINDOW,
            "frame_marks",
        ),
    ] {
        let scenario = after_a_raw_backspace(steps);

        let run = run_scenario(script, &scenario, |executor| {
            executor.live.frame_window = frame_window;
        });

        let detail = refusal(&run, what, 1);
        assert!(
            detail.contains(said) && detail.contains("no key was sent"),
            "{what}: {detail}"
        );
        assert_eq!(run.requests, 0, "{what}: no barrier was written");
        assert_eq!(run.received, None, "{what}: nothing reached the program");
    }
}

#[test]
fn a_quit_after_a_raw_backspace_refuses_a_screen_that_shows_no_plain_quit_prompt() {
    // The prompt that lists a waiting check offers `[c]`, not `[y] Quit`, and a program that
    // ignored the `q` shows no prompt at all: either way the `y` is not sent.
    for (what, shown, said) in [
        (
            "a prompt that lists a waiting check",
            waiting_quit_prompt(),
            "the dialog offers something else",
        ),
        ("no prompt", String::new(), "no quit dialog is open"),
    ] {
        let scenario = after_a_raw_backspace("\n[[steps]]\nstep = \"quit\"\ntimeout_ms = 3000\n");
        let script = marked(&format!(
            "{READY}{map}frame 0\nkey\nkey\n{shown}frame 2\nkey\n\
             echo \"$byte\" > received\n/bin/sleep 30\n",
            map = map(),
        ));

        let run = run_scenario(&script, &scenario, |_| {});

        let Err(Stop::Fail(failure)) = &run.result else {
            panic!("{what}: the `y` must not be sent: {:?}", run.result);
        };
        assert_eq!(failure.cause, FailureCause::Mismatch, "{what}: {failure}");
        assert!(failure.detail.contains(said), "{what}: {failure}");
        assert_eq!(run.inputs_sent, 2, "{what}: the Backspace and the `q`");
        assert_eq!(run.requests, 1, "{what}: behind a barrier");
        assert_eq!(run.received, None, "{what}: nothing reached the program");
    }
}

#[test]
fn a_quit_after_a_raw_backspace_confirms_the_plain_prompt_that_the_exact_screen_shows() {
    // The engaged quit still quits: the screen that answers the barrier shows the plain prompt and
    // no deletion dialog, and the `y` goes out behind that one barrier.
    let scenario = after_a_raw_backspace("\n[[steps]]\nstep = \"quit\"\ntimeout_ms = 3000\n");
    let script = marked(&format!(
        "{READY}{map}frame 0\nkey\nkey\n{prompt}frame 2\nkey\n\
         echo \"$byte\" > received\n/bin/sleep 30\n",
        map = map(),
        prompt = plain_quit_prompt(),
    ));

    let run = run_scenario(&script, &scenario, |_| {});

    assert!(run.result.is_ok(), "{:?}", run.result);
    assert_eq!(run.inputs_sent, 3, "the Backspace, the `q`, and the `y`");
    assert_eq!(run.requests, 1, "the `y` went out behind one barrier");
}

/// Asks an engaged executor to send `y` as the text of a filter, against a program that draws
/// `shown` and then answers barrier requests, and returns what it said, how many inputs it had
/// sent by then, and what the program recorded.
fn send_a_filter_text_after_a_raw_backspace(
    shown: &str,
) -> (Result<Instant, ProtocolError>, u64, Recorded) {
    let script = marked(&format!(
        "{READY}{shown}frame 0\nkey\necho \"$byte\" > received\n\
         /bin/sleep 30\n"
    ));
    let ((asked, inputs_sent), recorded) = drive_scenario_in(
        |_| script,
        &scenario(""),
        |executor| {
            executor.run().expect("the program is ready");
            // The Backspace need not be written: a scenario that wrote one is what engages the
            // executor, and the barrier is what the bytes of the filter are held behind.
            executor.live.note_raw_write(&[0x7f]);
            let asked =
                executor.send_confirming(b"y", Executor::deadline(5_000), Needs::FilterPrompt);
            (asked, executor.live.inputs_sent())
        },
    );
    (asked, inputs_sent, recorded)
}

#[test]
fn text_for_the_filter_is_sent_in_an_engaged_run_only_to_a_filter_prompt_the_exact_screen_shows() {
    let (asked, inputs_sent, recorded) =
        send_a_filter_text_after_a_raw_backspace(&open_filter_prompt());
    assert!(asked.is_ok(), "{asked:?}");
    assert_eq!(inputs_sent, 1, "the text was sent");
    assert_eq!(recorded.requests, 1, "behind one barrier");

    let (asked, inputs_sent, recorded) = send_a_filter_text_after_a_raw_backspace(&map());
    let Err(ProtocolError::Unmet(unmet)) = asked else {
        panic!("a map with no filter prompt must not be typed into: {asked:?}");
    };
    assert_eq!(unmet.cause, FailureCause::Mismatch);
    assert!(
        matches!(unmet.waited, Waited::Ready(()))
            && unmet.observed.contains("the filter prompt is not open")
            && unmet.observed.contains("no key was sent"),
        "{unmet:?}"
    );
    assert_eq!(inputs_sent, 0, "nothing was sent");
    assert_eq!(recorded.requests, 1, "the barrier was written and answered");
}

/// What a program writes when its map has caught up with a deletion: `shown` on the screen, the
/// `refresh_finished` event with `outcome`, and the frame, marked, that carries the text, counting
/// `inputs`.
fn refreshed(shown: &str, outcome: &str, inputs: u32) -> String {
    format!(
        "printf '{shown}'\n\
         printf '{{\"v\":1,\"kind\":\"refresh_finished\",\"outcome\":\"{outcome}\",\"t_us\":3}}\\n' \
         >> \"$events\"\nframe {inputs}\n"
    )
}

/// A program that starts a deletion with no dialog at Backspace and reports at once that it
/// removed `removed` entries. `before` is what it writes ahead of the key, and `after` what it
/// writes 300 ms after the report, when its map catches up with the deletion, if it does. It marks
/// its frames as it reports them, and answers the barrier written ahead of the key and the one
/// written behind it.
fn deletes_then(removed: u64, before: &str, after: &str) -> String {
    marked(&format!(
        "stty raw -echo\nprintf READY\n{victim}frame 0\n{before}\
         key\nframe 1\n\
         printf '{{\"v\":1,\"kind\":\"deletion_finished\",\"removed\":{removed},\"failed\":0,\
         \"t_us\":2}}\\n' >> \"$events\"\nframe 1\nserve\n\
         /bin/sleep 0.3\n{after}/bin/sleep 30\n",
        victim = panel(&["victim.bin", "◆ COMPLETE · file"]),
    ))
}

/// A scenario with no confirmation dialog that deletes `victim.bin` and then runs `then`.
fn delete_victim_then(then: &str) -> Scenario {
    scenario_with(
        "sentinels = [\"keep-a.bin\"]\ndisable_delete_confirmation = true\n",
        &format!(
            "\n[[steps]]\nstep = \"delete\"\nname = \"victim.bin\"\nkind = \"file\"\n\
             timeout_ms = 10000\n{then}"
        ),
    )
}

const WAIT_REFRESH_THEN_EXPECT_THE_REFRESHED_MAP: &str = r#"
[[steps]]
step = "wait_refresh"
timeout_ms = 10000

[[steps]]
step = "expect_screen"
contains = ["REFRESHED"]
"#;

fn run_without_a_frame_window(script: &str, scenario: &Scenario) -> Run {
    run_scenario(script, scenario, |executor| {
        executor.live.frame_window = NO_FRAME_WINDOW;
    })
}

#[test]
fn wait_refresh_returns_once_the_map_has_caught_up_with_the_deletion() {
    let scenario = delete_victim_then(WAIT_REFRESH_THEN_EXPECT_THE_REFRESHED_MAP);

    let run = run_without_a_frame_window(
        &deletes_then(1, "", &refreshed("REFRESHED", "published", 1)),
        &scenario,
    );

    assert!(run.result.is_ok(), "{:?}", run.result);
}

/// The control for the test above: the `delete` step returns on the frame after the report, 300 ms
/// before the map catches up, so the screen a step reads then is the map before the refresh.
#[test]
fn without_wait_refresh_the_screen_is_still_the_map_before_the_refresh() {
    let scenario =
        delete_victim_then("\n[[steps]]\nstep = \"expect_screen\"\ncontains = [\"REFRESHED\"]\n");

    let run = run_without_a_frame_window(
        &deletes_then(1, "", &refreshed("REFRESHED", "published", 1)),
        &scenario,
    );

    let Err(Stop::Fail(failure)) = &run.result else {
        panic!("the refresh had not happened yet: {:?}", run.result);
    };
    assert_eq!(failure.cause, FailureCause::Mismatch, "{failure}");
}

/// A refresh that ended before the deletion started is not the one the deletion owes. A step that
/// took it for the answer would return at once, and a quit after it would land in the rebuild.
#[test]
fn a_refresh_that_ended_before_the_deletion_is_no_answer() {
    let scenario = delete_victim_then(
        "\n[[steps]]\nstep = \"wait_refresh\"\ntimeout_ms = 10000\n\n\
         [[steps]]\nstep = \"expect_screen\"\ncontains = [\"LATE\"]\n",
    );

    let run = run_without_a_frame_window(
        &deletes_then(
            1,
            &refreshed("EARLY", "published", 0),
            &refreshed("LATE", "published", 1),
        ),
        &scenario,
    );

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn a_deletion_that_removed_nothing_owes_no_refresh_and_wait_refresh_does_not_wait_for_one() {
    // The program never reports a refresh, and the step's bound is far longer than the test
    // takes: only returning at once passes.
    let scenario = delete_victim_then("\n[[steps]]\nstep = \"wait_refresh\"\ntimeout_ms = 5000\n");

    let started = std::time::Instant::now();
    let run = run_without_a_frame_window(&deletes_then(0, "", ""), &scenario);

    assert!(run.result.is_ok(), "{:?}", run.result);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the step waited for a refresh that nothing owed"
    );
}

#[test]
fn a_refresh_that_fails_fails_the_step_instead_of_passing_on_a_map_that_is_gone() {
    let scenario = delete_victim_then(WAIT_REFRESH_THEN_EXPECT_THE_REFRESHED_MAP);

    let run = run_without_a_frame_window(
        &deletes_then(1, "", &refreshed("REFRESHED", "failed", 1)),
        &scenario,
    );

    let Err(Stop::Fail(failure)) = &run.result else {
        panic!("a failed refresh must fail the step: {:?}", run.result);
    };
    assert_eq!(failure.cause, FailureCause::Mismatch, "{failure}");
    assert!(failure.detail.contains("`failed`"), "{failure}");
}

#[test]
fn a_refresh_that_never_ends_fails_the_step_at_its_bound() {
    let scenario = delete_victim_then("\n[[steps]]\nstep = \"wait_refresh\"\ntimeout_ms = 700\n");

    let run = run_without_a_frame_window(&deletes_then(1, "", ""), &scenario);

    let Err(Stop::Fail(failure)) = &run.result else {
        panic!(
            "a refresh that never ends must fail the step: {:?}",
            run.result
        );
    };
    assert_eq!(failure.cause, FailureCause::Timeout, "{failure}");
    assert!(failure.expected.contains("`refresh_finished`"), "{failure}");
}

/// A program that is ready and then waits: the budget tests need a session to check against, not
/// a program that does anything.
const READY_AND_WAIT: &str = r"
printf READY
/bin/sleep 30
";

/// A step that checks the metric `probe_ms`, which a test records, against `budget`.
fn expect_probe_within(budget: &str) -> String {
    format!("\n[[steps]]\nstep = \"expect_budget\"\nbudget = \"{budget}\"\nmetric = \"probe_ms\"\n")
}

/// Runs `steps`, which check `probe_ms`, recorded as `value`, in an executor whose latency scale is
/// `scale` and whose timing is informational when `informational`.
fn run_probes(steps: &str, value: f64, scale: f64, informational: bool) -> Run {
    run_with(READY_AND_WAIT, steps, |executor| {
        executor.recorder.record_metric("probe_ms", value);
        executor.latency_scale = LatencyScale::new(scale).expect("a valid scale");
        executor.timing_informational = informational;
    })
}

/// Runs a step that checks `probe_ms`, recorded as `value`, against `budget`, in an executor
/// whose latency scale is `scale` and whose timing is strict.
fn run_probe(budget: &str, value: f64, scale: f64) -> Run {
    run_probes(&expect_probe_within(budget), value, scale, false)
}

/// The budgets that are memory, count, residue, and CPU contracts, each with a value just over its
/// limit: a latency scale of 100 would hide it if it applied, and so would a warning.
fn contract_budgets_just_over() -> [(&'static str, f64); 4] {
    [
        ("peak_rss_bytes", 512.0 * 1024.0 * 1024.0 + 1.0),
        ("idle_output_bytes", 1.0),
        ("residue_files", 1.0),
        ("idle_cpu_ms", 51.0),
    ]
}

#[test]
fn a_scaled_latency_budget_passes_what_the_strict_budget_fails() {
    // 300 ms is over the strict first-frame limit of 250 ms and under twice it.
    let strict = run_probe("first_frame_ms", 300.0, 1.0);
    let scaled = run_probe("first_frame_ms", 300.0, 2.0);

    let Err(Stop::Fail(failure)) = &strict.result else {
        panic!("the strict limit must fail 300 ms: {:?}", strict.result);
    };
    assert!(
        failure.expected.contains("at most 250"),
        "{}",
        failure.expected
    );
    assert!(scaled.result.is_ok(), "{:?}", scaled.result);
}

#[test]
fn a_latency_failure_under_a_scale_names_the_scaled_limit_and_the_scale() {
    let run = run_probe("first_frame_ms", 600.0, 2.0);

    let Err(Stop::Fail(failure)) = &run.result else {
        panic!("600 ms is over twice the limit: {:?}", run.result);
    };
    assert!(
        failure.expected.contains("at most 500") && failure.expected.contains("scaled by 2"),
        "{}",
        failure.expected
    );
}

#[test]
fn a_scale_never_loosens_a_memory_count_residue_or_cpu_budget() {
    for (budget, value) in contract_budgets_just_over() {
        let run = run_probe(budget, value, 100.0);

        assert!(
            matches!(run.result, Err(Stop::Fail(_))),
            "{budget} must still fail {value} under a scale of 100: {:?}",
            run.result
        );
    }
}

/// The warning that a step checking `probe_ms` records when `budget` is missed.
fn warning(budget: Budget, value: f64, limit: f64) -> TimingWarning {
    TimingWarning {
        budget,
        metric: "probe_ms".to_owned(),
        value,
        limit,
    }
}

#[test]
fn informational_timing_passes_a_missed_latency_budget_and_records_it() {
    // 300 ms is over the strict first-frame limit of 250 ms.
    let strict = run_probe("first_frame_ms", 300.0, 1.0);
    let informational = run_probes(&expect_probe_within("first_frame_ms"), 300.0, 1.0, true);

    assert!(
        matches!(strict.result, Err(Stop::Fail(_))) && strict.warnings.is_empty(),
        "a strict run fails the miss and records no warning: {:?}",
        strict.result
    );
    assert!(informational.result.is_ok(), "{:?}", informational.result);
    assert_eq!(
        informational.warnings,
        [warning(Budget::FirstFrameMs, 300.0, 250.0)]
    );
}

#[test]
fn informational_timing_records_nothing_for_a_latency_budget_that_is_met() {
    let run = run_probes(&expect_probe_within("first_frame_ms"), 250.0, 1.0, true);

    assert!(run.result.is_ok(), "{:?}", run.result);
    assert!(run.warnings.is_empty(), "{:?}", run.warnings);
}

#[test]
fn informational_timing_holds_a_scaled_run_to_its_scaled_limit() {
    // 300 ms is over the strict limit and under twice it; 600 ms is over twice it.
    let within = run_probes(&expect_probe_within("first_frame_ms"), 300.0, 2.0, true);
    let over = run_probes(&expect_probe_within("first_frame_ms"), 600.0, 2.0, true);

    assert!(within.result.is_ok() && within.warnings.is_empty());
    assert!(over.result.is_ok(), "{:?}", over.result);
    assert_eq!(
        over.warnings,
        [warning(Budget::FirstFrameMs, 600.0, 500.0)],
        "the warning names the limit the run was held to"
    );
}

#[test]
fn informational_timing_never_excuses_a_memory_count_residue_or_cpu_budget() {
    for (budget, value) in contract_budgets_just_over() {
        let run = run_probes(&expect_probe_within(budget), value, 1.0, true);

        assert!(
            matches!(run.result, Err(Stop::Fail(_))),
            "{budget} must still fail {value} when timing is informational: {:?}",
            run.result
        );
        assert!(run.warnings.is_empty(), "{budget}: {:?}", run.warnings);
    }
}

#[test]
fn informational_timing_does_not_excuse_a_metric_that_was_never_recorded() {
    let steps = "\n[[steps]]\nstep = \"expect_budget\"\nbudget = \"first_frame_ms\"\n\
                 metric = \"never_recorded\"\n";
    let run = run_probes(steps, 0.0, 1.0, true);

    assert!(
        matches!(run.result, Err(Stop::Fail(_))),
        "a latency budget with no metric to judge fails: {:?}",
        run.result
    );
    assert!(run.warnings.is_empty(), "{:?}", run.warnings);
}

#[test]
fn a_step_after_a_warning_still_fails_the_run_and_the_warning_stays_recorded() {
    // Step 0 waits for READY, step 1 misses a latency budget, and step 2 misses a residue budget.
    let steps = format!(
        "{}{}",
        expect_probe_within("first_frame_ms"),
        expect_probe_within("residue_files")
    );
    let run = run_probes(&steps, 300.0, 1.0, true);

    let Err(Stop::Fail(failure)) = &run.result else {
        panic!("the residue budget must fail: {:?}", run.result);
    };
    assert_eq!(failure.index, 2, "{failure}");
    assert_eq!(
        run.warnings,
        [warning(Budget::FirstFrameMs, 300.0, 250.0)],
        "what the run measured before it failed is still evidence"
    );
}
