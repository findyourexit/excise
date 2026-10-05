//! The steps that end on a frame event, run against scripted programs.
//!
//! One group has a terminal that delivers a frame late, the way `ConPTY` does: the program reports
//! the frame at once, and the bytes of the frame follow 30 ms later, ten times the quiet that the
//! bounded tail waits for. These tests give the executor a frame window far longer than that
//! delay, so that a loaded machine, which can stall the script's `sleep` for well over the delay,
//! cannot fail them: what they check is that a step reads the terminal for the whole window, not
//! how long the window is. Windows' window is chosen from measured `ConPTY` delays (see
//! [`CONPTY_FRAME_WINDOW`](super::super::exec::CONPTY_FRAME_WINDOW)).
//!
//! The other group has a program that counts an input of its own before it is sent any, the way
//! `excise` does on Windows, and then draws a frame that does not count the key before the frame
//! that does, 100 ms later. These tests give the executor a frame window far shorter than that, so
//! that nothing but the input baseline keeps a step from taking the first of the two frames for
//! the answer and reading the screen before the second.
//!
//! One group has a program whose selected-item panel trails its header: the panel shows nothing
//! selected at first, and names the entry 300 ms later, the way a fresh map arms its cursor in a
//! frame after the one that says `COMPLETE`. A deletion with no dialog has to wait for the panel
//! to name the entry, and refuse only when it never does.
//!
//! The last group checks `expect_budget` against metrics that the test records itself, so that the
//! limit a step applies is the only thing that can decide the result and no timing is involved.
//!
//! The programs are shell scripts, so these tests run on Unix. A script that waits for a resize
//! sleeps in short steps rather than in one `wait` for a background `sleep`: a shell may run the
//! trap of a signal that arrives just before its `wait` starts only once that `wait` is over.

use std::{collections::BTreeMap, fmt::Write as _, time::Duration};

use super::super::{
    budget::LatencyScale,
    exec::Executor,
    outcome::{FailureCause, Stop},
    plan::prepare,
};
use crate::{
    fixture::{FixtureCache, FixtureSpec, Fixtures},
    pty::{PtySession, SpawnSpec},
    report::TimingWarning,
    safety::{FixtureRoot, FixtureSnapshot, Scratch, isolated_env},
    scenario::{Budget, Profile, Scenario},
};

/// The frame window of the tests whose terminal delivers a frame 30 ms after its event: far longer
/// than that, so a stalled script still delivers within it.
const LATE_FRAME_WINDOW: Duration = Duration::from_secs(2);

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
i=0
while [ $i -lt 600 ]; do
  /bin/sleep 0.05
  i=$((i + 1))
done
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

/// How a run of the steps ended, the metrics it had recorded by then, and the timing budgets it
/// had missed without failing for them.
struct Run {
    result: Result<(), Stop>,
    metrics: BTreeMap<String, f64>,
    warnings: Vec<TimingWarning>,
}

/// Runs `steps` after the script has printed `READY`, in an executor whose frame window is
/// `frame_window`, and stops the program.
fn run(script: &str, steps: &str, frame_window: Duration) -> Run {
    run_with(script, steps, |executor| {
        executor.frame_window = frame_window;
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
        scenario,
        &prepared,
        &fixture,
        &scratch,
        &baseline,
        scratch.store(),
        session,
    );
    configure(&mut executor);
    let result = executor.run();
    let metrics = executor.metrics();
    let warnings = std::mem::take(&mut executor.timing_warnings);
    executor.session.kill();
    Run {
        result,
        metrics,
        warnings,
    }
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
    let run = run(ENTER_DRAWS_LATE, SETTLE_THEN_EXPECT, LATE_FRAME_WINDOW);

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn resize_reads_a_frame_the_terminal_delivers_after_its_event() {
    let run = run(RESIZE_DRAWS_LATE, RESIZE_THEN_EXPECT, LATE_FRAME_WINDOW);

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
/// would take for an erase, reaches it; it starts a deletion that finishes at once.
fn panel_arrives_late() -> String {
    format!(
        "stty raw -echo\nprintf READY\n{nothing}report 0\n/bin/sleep 0.3\n{victim}report 0\n\
         dd bs=1 count=1 > /dev/null 2>&1\nreport 1\n\
         printf '{{\"v\":1,\"kind\":\"deletion_finished\",\"removed\":1,\"failed\":0,\"t_us\":2}}\\n' \
         >> \"$events\"\nreport 1\n/bin/sleep 30\n",
        nothing = panel(&["Choose an item to see its space and scan status."]),
        victim = panel(&["victim.bin", "◆ COMPLETE · file"]),
    )
}

/// A program whose panel names another entry, and keeps naming it.
fn panel_names_another_entry() -> String {
    format!(
        "printf READY\n{other}report 0\n/bin/sleep 30\n",
        other = panel(&["other.bin", "◆ COMPLETE · file"]),
    )
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
        executor.frame_window = SHORT_FRAME_WINDOW;
    });

    assert!(run.result.is_ok(), "{:?}", run.result);
}

#[test]
fn a_deletion_with_no_dialog_refuses_a_panel_that_never_names_the_entry() {
    let scenario = delete_victim_with_no_dialog(1_000);

    let run = run_scenario(&panel_names_another_entry(), &scenario, |executor| {
        executor.frame_window = SHORT_FRAME_WINDOW;
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
}

/// What a program writes when its map has caught up with a deletion: `shown` on the screen, the
/// `refresh_finished` event with `outcome`, and the frame that carries the text, counting `inputs`.
fn refreshed(shown: &str, outcome: &str, inputs: u32) -> String {
    format!(
        "printf '{shown}'\n\
         printf '{{\"v\":1,\"kind\":\"refresh_finished\",\"outcome\":\"{outcome}\",\"t_us\":3}}\\n' \
         >> \"$events\"\nreport {inputs}\n"
    )
}

/// A program that starts a deletion with no dialog at Backspace and reports at once that it
/// removed `removed` entries. `before` is what it writes ahead of the key, and `after` what it
/// writes 300 ms after the report, when its map catches up with the deletion, if it does.
fn deletes_then(removed: u64, before: &str, after: &str) -> String {
    format!(
        "stty raw -echo\nprintf READY\n{victim}report 0\n{before}\
         dd bs=1 count=1 > /dev/null 2>&1\nreport 1\n\
         printf '{{\"v\":1,\"kind\":\"deletion_finished\",\"removed\":{removed},\"failed\":0,\
         \"t_us\":2}}\\n' >> \"$events\"\nreport 1\n\
         /bin/sleep 0.3\n{after}/bin/sleep 30\n",
        victim = panel(&["victim.bin", "◆ COMPLETE · file"]),
    )
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

fn run_short_window(script: &str, scenario: &Scenario) -> Run {
    run_scenario(script, scenario, |executor| {
        executor.frame_window = SHORT_FRAME_WINDOW;
    })
}

#[test]
fn wait_refresh_returns_once_the_map_has_caught_up_with_the_deletion() {
    let scenario = delete_victim_then(WAIT_REFRESH_THEN_EXPECT_THE_REFRESHED_MAP);

    let run = run_short_window(
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

    let run = run_short_window(
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

    let run = run_short_window(
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
    let run = run_short_window(&deletes_then(0, "", ""), &scenario);

    assert!(run.result.is_ok(), "{:?}", run.result);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the step waited for a refresh that nothing owed"
    );
}

#[test]
fn a_refresh_that_fails_fails_the_step_instead_of_passing_on_a_map_that_is_gone() {
    let scenario = delete_victim_then(WAIT_REFRESH_THEN_EXPECT_THE_REFRESHED_MAP);

    let run = run_short_window(
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

    let run = run_short_window(&deletes_then(1, "", ""), &scenario);

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
