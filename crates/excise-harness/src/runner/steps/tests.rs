//! The steps that end on a frame event, run against scripted programs.
//!
//! One group has a terminal that delivers a frame late, the way `ConPTY` does: the program reports
//! the frame at once, and the bytes of the frame follow 30 ms later. That is more than any delay
//! `ConPTY` showed once the program was running (see [`CONPTY_FRAME_WINDOW`]), and ten times the
//! quiet that the bounded tail waits for. These tests give the executor the frame window Windows
//! uses, which is what a Windows run of the same steps relies on.
//!
//! The other group has a program that counts an input of its own before it is sent any, the way
//! `excise` does on Windows, and then draws a frame that does not count the key before the frame
//! that does, 100 ms later. These tests give the executor a frame window far shorter than that, so
//! that nothing but the input baseline keeps a step from taking the first of the two frames for
//! the answer and reading the screen before the second.
//!
//! The programs are shell scripts, so these tests run on Unix.

use std::{collections::BTreeMap, time::Duration};

use super::super::{
    exec::{CONPTY_FRAME_WINDOW, Executor},
    outcome::Stop,
    plan::prepare,
};
use crate::{
    fixture::{FixtureCache, FixtureSpec, Fixtures},
    pty::{PtySession, SpawnSpec},
    safety::{FixtureRoot, FixtureSnapshot, Scratch, isolated_env},
    scenario::{Profile, Scenario},
};

/// The frame window of the tests with a program that counts an input of its own: long enough for
/// bytes written just before their event to arrive, and far below the 100 ms between the frames.
const SHORT_FRAME_WINDOW: Duration = Duration::from_millis(30);

/// What every script starts with: the `hello` event, and a `report` function that writes the event
/// of a frame that has consumed the given number of inputs.
const PRELUDE: &str = r#"
events="$EXCISE_TEST_EVENTS"
seq=0
printf '{"v":1,"kind":"hello","version":"test","pid":%s,"t_us":0}\n' "$$" > "$events"
report() {
  seq=$((seq + 1))
  printf '{"v":1,"kind":"frame","seq":%s,"inputs":%s,"t_us":1}\n' "$seq" "$1" >> "$events"
}
"#;

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
/bin/sleep 30 &
wait $!
wait
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

/// Counts one input of its own before it is sent any. A resize makes it report a frame that does
/// not count the resize at once, and 100 ms later draw and report the frame that does. The key
/// after that is counted on top of both. The handler reads the key itself: a shell does not
/// reliably go back to what it was doing once a trap has run.
const COUNTS_ITS_OWN_INPUT_BEFORE_A_RESIZE: &str = r"
trap 'report 1; /bin/sleep 0.1; printf UPDATED; report 2; read line; printf DONE; report 3' WINCH
report 1
printf READY
/bin/sleep 30 &
wait $!
wait
";

fn scenario(steps: &str) -> Scenario {
    let scenario = Scenario::from_toml_str(&format!(
        "schema_version = 1\nname = \"scripted-program\"\n\
         description = \"a scripted program\"\n\
         fixture = \"delete-file\"\nprofiles = [\"default\"]\n\
         [[steps]]\nstep = \"wait_text\"\ntext = \"READY\"\n{steps}"
    ))
    .expect("a scenario");
    scenario.validate().expect("a valid scenario");
    scenario
}

/// How a run of the steps ended, and the metrics it had recorded by then.
struct Run {
    result: Result<(), Stop>,
    metrics: BTreeMap<String, f64>,
}

/// Runs `steps` after the script has printed `READY`, in an executor whose frame window is
/// `frame_window`, and stops the program.
fn run(script: &str, steps: &str, frame_window: Duration) -> Run {
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
    let scenario = scenario(steps);
    let prepared = prepare(&scenario).expect("a runnable scenario");
    let session = PtySession::spawn(&SpawnSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), format!("{PRELUDE}{script}").into()],
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
        &scenario,
        &prepared,
        &fixture,
        &scratch,
        &baseline,
        scratch.store(),
        session,
    );
    executor.frame_window = frame_window;
    let result = executor.run();
    let metrics = executor.metrics();
    executor.session.kill();
    Run { result, metrics }
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
    let run = run(ENTER_DRAWS_LATE, SETTLE_THEN_EXPECT, CONPTY_FRAME_WINDOW);

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn resize_reads_a_frame_the_terminal_delivers_after_its_event() {
    let run = run(RESIZE_DRAWS_LATE, RESIZE_THEN_EXPECT, CONPTY_FRAME_WINDOW);

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn settle_waits_for_the_frame_that_counts_the_key_past_the_programs_own_input() {
    let run = run(COUNTS_ITS_OWN_INPUT, SETTLE_THEN_EXPECT, SHORT_FRAME_WINDOW);

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn a_key_is_timed_to_the_frame_that_counts_it_past_the_programs_own_input() {
    let run = run(
        COUNTS_ITS_OWN_INPUT,
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
        COUNTS_ITS_OWN_INPUT_BEFORE_A_RESIZE,
        RESIZE_THEN_KEY_THEN_SETTLE,
        SHORT_FRAME_WINDOW,
    );

    assert!(run.result.is_ok(), "{:?}", run.result);
}
