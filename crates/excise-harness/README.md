# excise-harness

> **Internal test tooling. Not a supported product interface.** Nothing in this crate is covered
> by the Excise v1 command-line, configuration, or report contracts. It is not published
> (`publish = false`), it is not shipped in release archives, and its scenario format, documents,
> and schemas may change without notice.

`excise-harness` is the black-box validation harness for Excise. The harness runs the `excise`
binary from the outside, through a pseudo-terminal or through `--format json`, and checks only what
a user or a script could observe. It never depends on the `excise` crate.

This crate defines the vocabulary the harness shares:

- [`scenario`](src/scenario): the typed model of TOML scenario files, with strict parsing and
  semantic validation.
- [`report`](src/report): the versioned machine-output documents, with JSON Schemas in
  [`schemas/`](schemas).

Runners (in-process, pseudo-terminal, headless) and the `xtask` commands build on this vocabulary
and on the [fixture generator](#fixtures), which creates and checks the trees they run against.

## Scenario files

A scenario is one TOML file. It names a generated fixture, the terminal and profiles to run under,
the steps to perform, and what must hold afterwards.

```toml
schema_version = 1
name = "delete-folder-lifecycle"
description = "Deleting a large folder removes it, leaves its siblings alone, and restores the terminal."
fixture = "delete-folder"
sentinels = ["keep-a.bin", "keep-b/keep.txt"]
profiles = ["default", "deterministic"]

[terminal]
cols = 120
rows = 40

[[steps]]
step = "wait_header"
state = "complete"
timeout_ms = 30000

[[steps]]
step = "select"
name = "victim"

[[steps]]
step = "delete"
name = "victim"
kind = "folder"

[[steps]]
step = "wait_fs_absent"
path = "victim"
timeout_ms = 60000

[[steps]]
step = "expect_fs"
present = ["keep-a.bin", "keep-b/keep.txt"]
absent = ["victim"]

[[steps]]
step = "quit"

[[steps]]
step = "expect_exit"
code = 0
terminal_restored = true
residue = "none"
```

### Strictness

Parsing rejects unknown fields at every level (top level, every step, and every nested table),
unknown enum values, and wrongly typed values. There are no aliases; the only defaults are the
documented ones (terminal size, `expect`, `timeout_ms`, `ctrl`, `alt`, `sentinels`, and
`budgets`). Names are `snake_case`, except profile names, which are kebab-case.

`Scenario::from_toml_str` and `Scenario::from_path` only parse. `Scenario::validate` then applies
the semantic rules below and reports every broken rule, not only the first. **A runner must call
`validate` and refuse to run a scenario that fails it.**

### Top-level fields

| Field | Required | Meaning |
|---|---|---|
| `schema_version` | yes | The format version. Only `1` is accepted. |
| `name` | yes | The scenario identifier: 1 to 64 lowercase ASCII letters, digits, `-`, or `_`, starting with a letter or digit. It becomes a file and directory name in run output. |
| `description` | yes | What the scenario demonstrates. |
| `fixture` | yes | The identifier of the fixture specification to generate, in the same shape as `name`. The scenario never names a directory. |
| `sentinels` | if any step is `delete` | Fixture-relative paths that must survive the scenario. |
| `scan_store_on_volume` | no | Points `EXCISE_SCAN_STORE_DIR` at a subdirectory of the fixture's attached volume instead of the scenario's own scratch area. Defaults to `false`. The fixture must declare exactly one `volume` part. See [Volumes](#volumes). |
| `profiles` | yes | A non-empty, duplicate-free list of profiles the scenario runs under. |
| `tier` | no | `"quick"` (default), `"full"`, or `"nightly"`: which `cargo xtask e2e` tier runs it. See [Tiers and platforms](#tiers-and-platforms). |
| `platforms` | no | The operating systems the scenario runs on, as `std::env::consts::OS` spells them. Defaults to all three. See [Tiers and platforms](#tiers-and-platforms). |
| `terminal` | no | The initial terminal size. Defaults to 120 columns by 40 rows; at least 32 by 8. |
| `expect` | no | `"pass"` (default) or `"fail"`. See [Expected failures](#expected-failures). |
| `fails_on` | no | The platforms `expect = "fail"` applies to; defaults to every platform in `platforms`. See [Expected failures](#expected-failures). |
| `slice` | if `expect = "fail"` | The id of the work slice that fixes the defect, for example `X2`: an uppercase letter followed by up to seven uppercase letters or digits. |
| `budgets` | no | Overrides for named budget limits. See [Budgets](#budgets). |
| `steps` | yes | The ordered steps; at least one. |

### Profiles

A profile is a named runner configuration. The runner, not the scenario, defines each profile's
environment, arguments, and terminal size.

| Profile | Intent |
|---|---|
| `default` | The user defaults. |
| `deterministic` | Reduced motion and a single scan thread. |
| `monochrome-ascii` | Monochrome output with ASCII symbols and borders. |
| `narrow` | A narrow terminal. |
| `mouse-keymaps` | Mouse input and the alternative movement keymaps. |

Every lifecycle scenario is expected to pass under `default` and `deterministic`. The other
profiles run on a representative subset.

### Tiers and platforms

`tier` is how often a scenario runs. `cargo xtask e2e --quick` runs only scenarios tagged `quick`
(the default); `--full` adds `full`; `--nightly` adds `nightly` too. `--scenario NAME` runs a named
scenario whatever its tier. The in-process runner (`cargo test`) runs only `quick` scenarios and
skips the others, with the reason. The quick tier still limits *profiles* to `default` and
`deterministic` (see [Profiles](#profiles) above); `--full` and `--nightly` run every profile a
scenario declares.

`platforms` is where the scenario runs at all, spelled as `std::env::consts::OS` spells them
(`linux`, `macos`, `windows`), the same as
[`expectations/headless.toml`](expectations/headless.toml). It defaults to every platform. Outside
its `platforms`, every runner skips the scenario, with the reason, even when it is named with
`--scenario`. For example, `tier = "nightly"` with `platforms = ["linux", "macos"]` runs only in
the nightly tier, and only on Linux and macOS.

Validation rejects `fails_on` without `expect = "fail"`, a name in either list this harness does
not know, a name in `fails_on` outside `platforms`, a repeated name, and a present-but-empty list
(omit the field instead, for the default).

### Expected failures

`expect = "fail"` marks a scenario that documents a known defect. It requires `slice`, the id of
the work slice that will fix the defect. The runner runs it like any other scenario and it **must
fail**. This is strict xfail:

| `expect` | The scenario | Verdict | Fails the run |
|---|---|---|---|
| `pass` | passes | `pass` | no |
| `pass` | fails | `fail` | yes |
| `fail` | fails | `xfail` | no |
| `fail` | passes | `xpass` | **yes** |

An `xpass` fails the run so that the change that fixes the defect is forced to flip the scenario to
`expect = "pass"` and drop `slice`. A scenario the harness could not run at all (fixture, spawn, or
isolation failure) has the verdict `error`, which also fails the run. `Verdict::resolve` and
`Verdict::blocks_run` encode this table.

`fails_on` restricts where `expect = "fail"` applies: a list of platforms, defaulting to every
platform in `platforms`. On a platform the scenario runs on but `fails_on` omits, it is an ordinary
`expect = "pass"` there: strict like any other platform, so an unexpected pass is fine but an
unexpected failure still fails the run. `Scenario::expect_on(os)` gives the effective expectation
on a platform, and both runners' verdicts call it instead of reading the raw `expect` field. For
example, `expect = "fail"` with `fails_on = ["macos"]` and `slice = "H5"` must fail on macOS and
must pass everywhere else the scenario runs.

### Budgets

`expect_budget` compares a recorded metric with a named budget. The scenario can override a
budget's limit in `[budgets]`; without an override the runner applies its default for the tier it
is running. Limits are finite, non-negative numbers in the unit shown.

```toml
[budgets]
max_stall_ms = 400
idle_output_bytes = 0
```

| Budget | Unit | Limits |
|---|---|---|
| `headless_scan_ratio` | ratio | Headless scan time divided by `du -sk` time on the same fixture. |
| `tui_complete_ratio` | ratio | Interactive time-to-COMPLETE divided by headless scan time. |
| `motion_complete_ratio` | ratio | Default-motion time-to-COMPLETE divided by deterministic time-to-COMPLETE. |
| `input_to_frame_p99_ms` | ms | The 99th percentile of input-to-frame latency. |
| `max_stall_ms` | ms | The longest gap without a frame while work is active. |
| `first_frame_ms` | ms | Time to the first frame. |
| `quit_ms` | ms | Time from confirming the quit to process exit. |
| `peak_rss_bytes` | bytes | Peak resident memory (peak footprint where the platform reports it). |
| `memory_ab_tolerance` | fraction | Allowed relative change in peak memory between builds, for example `0.05`. |
| `scan_store_quota_fraction` | fraction | The scan-store quota as a fraction of free scratch space, for example `0.75`. |
| `scan_store_bytes_per_entry` | bytes | Scan-store bytes per indexed entry. |
| `threads` | count | Threads in the process. |
| `fds` | count | Open file descriptors (handles on Windows). |
| `idle_output_bytes` | bytes | Terminal output while the program is idle. |
| `idle_cpu_ms` | ms | CPU time used while the program is idle. |
| `residue_files` | count | Files left in the scenario scratch directory. |
| `timing_ab_regression` | fraction | The tolerated timing regression between builds, for example `0.20`. |

Timing budgets are only meaningful from paired, interleaved A/B runs (see
[Output documents](#output-documents)); a single run's wall time is never compared across sessions.

### Steps

Steps are an array of tables. The `step` key names the step and selects the fields that follow.
Every step rejects fields it does not define.

```toml
[[steps]]
step = "wait_event"
event = "deletion_finished"
fields = { removed = { min = 1 }, failed = { eq = 0 } }
timeout_ms = 60000
```

**Timeouts.** Every step that waits takes an optional `timeout_ms`, the bound on that wait. It
defaults to 10 000 ms and must be between 1 and 1 800 000 (thirty minutes). When a bound elapses
the step fails and the runner kills the process group. The waiting steps are `wait_text`,
`wait_header`, `wait_event`, `select`, `delete`, `wait_fs_absent`, `wait_fs_present`,
`expect_exit`, `settle`, and `quit`. The other `expect_*` steps evaluate once against the current
state and never wait; put a `wait_*` or `settle` step before them.

| Step | Fields | What it does |
|---|---|---|
| `wait_text` | `text` or `regex` (exactly one), `region`, `timeout_ms` | Waits until the text or regular expression appears on the screen, or in the region. |
| `wait_header` | `state` (`scanning` or `complete`), `timeout_ms` | Waits until the header band reports the scan state. |
| `wait_event` | `event`, `fields`, `timeout_ms` | Waits until the event channel reports the event and every field test holds. |
| `key` | `key`, `ctrl`, `alt` | Presses one key. |
| `type` | `text` | Types literal text, one character at a time. |
| `select` | `name`, `timeout_ms` | Selects the entry by name through the filter. |
| `delete` | `name`, `kind` (`file` or `folder`), `wait_for` (`finished` or `started`), `timeout_ms` | Deletes the selected entry through the confirmation dialog. |
| `wait_fs_absent` | `path`, `timeout_ms` | Waits until the fixture-relative path no longer exists. |
| `wait_fs_present` | `path`, `timeout_ms` | Waits until the fixture-relative path exists. |
| `fs_mutate` | `op` (`appear`, `change`, `vanish`, `replace`), `path` | Changes the fixture while the program runs. |
| `resize` | `cols`, `rows` | Resizes the terminal. |
| `signal` | `signal` (`term`, `hup`, `quit`, `int`, `close`, `break`) | Delivers a signal or console event. `close` needs Windows; `break` is never deliverable (see below). |
| `expect_screen` | `contains`, `not_contains`, `regex`, `region` | Asserts what the screen shows now. |
| `expect_fs` | `present`, `absent` | Asserts which fixture-relative paths exist now. |
| `expect_exit` | `code`, `terminal_restored`, `residue`, `timeout_ms` | Waits for the program to exit and asserts how it ended. |
| `expect_budget` | `budget`, `metric` | Asserts that a recorded metric is within a budget. |
| `measure` | `name`, `marker` (`start` or `stop`) | Marks one end of a named measurement. |
| `settle` | `timeout_ms` | Waits until the program has processed everything sent so far. |
| `quit` | `timeout_ms` | Performs the ordinary confirmed quit. |

Details that a table cannot carry:

- **Screen text and regions.** Text is matched against the screen model, never against raw output
  bytes: each row's visible cells left to right with trailing spaces removed, rows joined with
  `\n`. `region` limits a match to part of the screen. `"header"` is the header band, the top three
  rows, whose state badge is the only valid completion signal. `"dialog"` is the modal dialog when
  one is open. `{ rows = [first, last] }` is an inclusive span of zero-based rows; `first` must not
  exceed `last`. `regex` values use the Rust `regex` crate's syntax. This crate does not depend on
  that crate, so `validate` does not compile them: a runner must compile every pattern before its
  first step and fail the scenario on a malformed one.
- **`wait_text`.** Exactly one of `text` and `regex`, and it must not be empty (an empty matcher
  would match everything).
- **`expect_screen` and `expect_fs`.** At least one list must be non-empty, and `contains`,
  `not_contains`, and `regex` entries must not be empty.
- **`key`.** `key` is one printable character (`"y"`, `"/"`, `" "`) or one of `enter`, `esc`,
  `backspace`, `tab`, `up`, `down`, `left`, `right`, `page_up`, `page_down`. Named keys are
  lowercase. `ctrl` and `alt` default to `false`.
- **`select`.** The runner opens the filter with `/`, types the name, presses Enter, then asserts
  that the selected item is exactly that entry.
- **`delete`.** The runner acts on the selected entry. It presses Backspace, parses the
  confirmation dialog, asserts that its title and path name that entry, `name` and `kind`, under the
  fixture root, asserts that every sentinel still exists, and only then presses `y`. Any mismatch
  fails the step and `y` is never sent. `wait_for` (default `"finished"`) controls when the step
  returns: `"finished"` waits for the deletion to finish; `"started"` returns as soon as the
  confirmation has closed the dialog, while the deletion keeps running, so a later step can act
  while it is still in progress (for example, delivering a `signal`).
- **`wait_event`.** `event` is one of `frame`, `scan_complete`, `deletion_finished`, `quit_prompt`,
  or `exit`: the kinds reported by the program's internal test event channel (which also opens with
  a `hello` line that the runner consumes itself). `fields` maps a numeric event field to a test:
  `{ eq = n }`, `{ min = n }`, or `{ max = n }` with exactly one key. The fields an event carries
  are `frame`: `seq`, `inputs`; `scan_complete`: `entries`; `deletion_finished`: `removed`,
  `failed`; and `exit`: `code`. Every event also carries `t_us`, the microseconds since the channel
  opened. Testing a field the event does not carry is a validation error.
- **`signal`.** `term`, `hup`, `quit`, and `int` are Unix signals; `close` and `break` are Windows
  console events. `close` closes the pseudo console, which Windows delivers to every attached
  process as `CTRL_CLOSE_EVENT`; this needs no unsafe call, so the runner can deliver it. `break`
  would need `GenerateConsoleCtrlEvent`, which does, so the runner reports it as unsupported on
  every platform. Whether the host can deliver a given signal at all is a runner concern, and an
  undeliverable signal must never be reported as a pass.
- **`resize`.** Unlike the initial terminal, a resize may go below 32 by 8 to exercise the resize
  message. Both dimensions must be non-zero.
- **`expect_exit`.** `terminal_restored = true` asserts that the alternate screen was left, the
  cursor is visible, and echo and canonical mode are on; `false` asserts that they are not.
  `residue = "none"` asserts that nothing is left in the scenario scratch directory. `code` is the
  process exit code. After a `close` event there is no console left to inspect: the runner accepts
  `terminal_restored = true` there without evaluating it, and refuses `false` before the run.
- **`measure` and `expect_budget`.** A `start` and a `stop` with the same `name` record the elapsed
  milliseconds as the metric `name`. Each name is used once; every `stop` needs an earlier `start`
  and every `start` needs a `stop`. `expect_budget` names a metric, built in or recorded by
  `measure`, and the budget it is checked against. Metric and measurement names use the identifier
  shape of `name`.
- **`quit`.** The runner presses `q`, waits for the quit prompt, and confirms. It does not assert
  the exit; follow it with `expect_exit`.

### Validation rules

`Scenario::validate` returns every broken rule as a typed error.

| Rule | Error |
|---|---|
| `schema_version` is `1` | `UnsupportedSchemaVersion` |
| `name` and `fixture` are identifiers | `InvalidIdentifier` |
| `profiles` is non-empty and duplicate-free | `NoProfiles`, `DuplicateProfile` |
| the terminal is at least 32 by 8 | `TerminalTooSmall` |
| `steps` is non-empty | `NoSteps` |
| `expect = "fail"` has a `slice`, and any `slice` is a slice id | `ExpectedFailureWithoutSlice`, `InvalidSlice` |
| budget overrides are finite and non-negative | `InvalidBudgetLimit` |
| `fails_on` requires `expect = "fail"` | `FailsOnWithoutExpectedFailure` |
| `platforms` and `fails_on` name only known platforms, without repeats, and are not present but empty | `UnknownPlatform`, `DuplicatePlatform`, `EmptyPlatformList` |
| `fails_on` names only platforms `platforms` includes | `FailsOnOutsidePlatforms` |
| a scenario with a `delete` step declares a sentinel | `DeleteWithoutSentinel` |
| every fixture-relative path is safe (see [Safety rules](#safety-rules)) | `InvalidPath` |
| every `timeout_ms` is between 1 and 1 800 000 | `TimeoutOutOfRange` |
| `wait_text` has exactly one non-empty matcher | `MissingMatcher`, `ConflictingMatchers`, `EmptyValue` |
| names, typed text, and assertion entries are not empty | `EmptyValue` |
| assertions check something | `NothingToCheck` |
| row regions run forward | `ReversedRows` |
| a resize has non-zero dimensions | `ZeroDimension` |
| a `wait_event` tests only fields its event carries | `UnknownEventField` |
| metric and measurement names are identifiers | `InvalidIdentifier` |
| measurements start once, stop once, and stop after starting | `MeasureRestarted`, `MeasureStopWithoutStart`, `MeasureNeverStopped` |

## Runner semantics

- **Waits are semantic.** A pseudo-terminal runner waits on the screen model (the header band,
  text), on the event channel, and on the file system, never on raw output bytes and never on the
  screen being idle. Raw-byte matching is unreliable: `COMPLETE` can appear in places other than
  the scan state, and a match can confirm the wrong deletion target. The header band's state badge
  is the only valid completion signal.
- **`settle`.** The in-process runner maps `settle` to the runtime's existing barrier input event
  (`InputEvent::Barrier`), which renders and then drains worker events, background deletion work,
  timers, and animation until the owner loop is quiescent. The pseudo-terminal runner has no such
  barrier and waits on semantic signals only: events, header state, and frame counts. Because the
  barrier drains work outside the production scheduling path, scheduling and throughput are judged
  only by the pseudo-terminal and headless runners.
- **Bounds.** Every wait is bounded. When a bound elapses the runner kills the process group,
  writes a failure bundle, and leaves nothing behind.
- **Sentinels.** The runner asserts every sentinel before it sends `y` for a `delete` step, and
  again after the last step.
- **Verdicts.** A run succeeds only if no result has a blocking verdict; see
  [Expected failures](#expected-failures).

## PTY runner

`excise_harness::runner` executes a validated scenario against a real `excise` process in a
pseudo-terminal. `run_scenario` runs one scenario under one profile; `run_e2e` runs the scenario ×
profile matrix behind `cargo xtask e2e`. Both take an already-materialized fixture root and refuse
a root without the ownership marker `.excise-harness-owned` before any process exists.

| Module | What it does |
|---|---|
| `pty` | The session: `portable-pty` with a `vt100` screen model, key encoding for every scenario key with Ctrl and Alt, `ESC[6n` cursor-position requests answered from the screen model, resize, an asciicast v2 recording with output (`"o"`) and input (`"i"`) events, and the terminal modes that failure bundles report. |
| `events` | A strict reader for the event channel (`EXCISE_TEST_EVENTS`, protocol v1). It reads complete lines only, rejects an unknown `v`, and requires the first event to be the `hello` of the process the runner started. |
| `metrics` | Latency, stalls, output volume, and resource use (below). |
| `safety` | Ownership markers, environment isolation, scratch areas, process-group kill, fixture snapshots, and residue checks. |
| `runner` | Step execution, the verdict map, failure bundles, and the matrix. |

**Isolation.** The child starts with an empty environment plus `TERM=xterm-256color`, `COLORTERM`,
`LANG`, and the profile's variables (see [Profiles](#profiles)). `HOME`, `EXCISE_CONFIG`, the
working directory, `EXCISE_SCAN_STORE_DIR`, and the temporary directory point into a fresh scratch
area, and `EXCISE_TEST_EVENTS` names a new file inside it. A scenario's `scan_store_on_volume`
overrides `EXCISE_SCAN_STORE_DIR` to a directory on an attached volume instead (see
[Volumes](#volumes)); nothing else about isolation changes. The scratch area is deleted after the
run unless `--keep-fixture` is given.

**Process group.** On Unix the child leads its own session, so its process group id is its pid.
A timeout, a failed step, or dropping the session kills the whole group with `SIGKILL`, and the
session waits until nothing is left. On Windows there is no group to signal: the child is ended
with the library's process termination, which does not reach descendants (`excise` starts none). A
job object would, but creating one needs `unsafe`, which this workspace allows only in
`src/os/windows.rs`.

**Steps.**

- **`settle`** waits for a `frame` event whose `inputs` counter is at least the number of input
  events the runner has sent and that was observed after the last one was written. It then reads
  the terminal output still in flight (at most 20 ms, ending after 3 ms of quiet) so that the
  screen model has caught up with the frame. A key that changes nothing draws no frame and never
  settles.
- **`select`** opens the filter with `/`, erases any text it opened with, types the name, checks
  the prompt, presses Enter, and waits until the inspector pane shows exactly that name.
- **`delete`** presses Backspace and reads the dialog. It presses `y` only when the dialog names
  exactly the requested entry, kind, and path and every sentinel exists. Any mismatch fails the
  step and no `y` is ever sent. `wait_for = "finished"` (the default) ends the step when the
  `deletion_finished` event and the first frame after it have been read, so the screen shows the
  result. `wait_for = "started"` ends it as soon as a frame shows the dialog has closed, without
  waiting for the deletion itself: the deletion keeps running after the step returns, so a step
  that needs its outcome waits for that separately (`wait_fs_absent`, `wait_event`). The program
  rebuilds its map after a deletion and treats a quit during the rebuild as a cancellation (exit
  code 130): a scenario that goes on to quit after a `"finished"` delete waits for the header to
  read `COMPLETE` first; one that quits after a `"started"` delete needs to reach the same point
  itself (`wait_fs_absent`, then `wait_event { event = "deletion_finished" }`, then a frame after
  it), since the step returned before any of that happened.
- **`quit`** presses `q`, waits for the quit dialog, and confirms with `y`.
- **`resize`** resizes the terminal and waits for the frame that answers it.
- **`wait_event`** matches any event read so far, including events before the step began.
- **`expect_exit`** also compares the fixture with its state before the run: only confirmed
  deletions may differ, and only by removal. A confirmed deletion excuses a removal anywhere at or
  below its target; it never excuses an addition or a change there or anywhere else, so an
  interrupted deletion (a signal mid-flight) that stopped at an entry boundary is exactly what
  passes: every entry of the target is either untouched or gone, never a changed one and never a
  newly created one, such as a private placeholder name a half-finished cleanup might leave.
  Residue is anything the run leaves in its scratch store and temporary directories, or elsewhere
  in the scratch area apart from the event file and the configuration.
- **`signal`** delivers `term`, `hup`, `quit`, and `int` to the child on Unix. `close` closes the
  pseudo-terminal's controlling side (`PtySession::close_console`): on Windows this closes the
  pseudo console, which Windows itself delivers to every attached process as `CTRL_CLOSE_EVENT`,
  and needs no `unsafe` call, so the runner delivers it there; Unix has nothing to close in the
  same sense, so the runner reports `close` as unsupported there. Nothing the program writes after
  a `close` reaches the screen model, so a later `expect_exit` checks the exit code and the
  residue but not the terminal. `break` would need `GenerateConsoleCtrlEvent`, which does need an
  `unsafe` call this workspace does not allow, so the runner reports it as unsupported everywhere.
- **`fs_mutate`** applies the fixture generator's mutator (`fixture::mutate::apply`) when the step
  runs. A refused mutation fails the step and changes nothing. An applied one is an intended
  change: `expect_exit` accepts differences at that path, below it, and in the directories the
  mutation created above it, and nothing else.
- A step's `timeout_ms` bounds the whole step, not each wait inside it.

**Measurements.** Each run reports finite, named metrics. The scenario `expect_budget` step and the
run summary use the same names.

| Metric | Meaning |
|---|---|
| `first_frame_ms`, `scan_complete_ms` | Spawn to the first `frame` event, and to `scan_complete`. `run_e2e` launches the binary once with `--version` before its first run, in an isolated environment with a 10 s limit, so the first measured session does not include the one-time code-signature assessment that macOS applies to a new binary (over 300 ms on its first launch against 5 to 8 ms afterwards). A warm-up that fails or times out stops the matrix with an error. |
| `input_to_frame_p50_ms`, `input_to_frame_p99_ms`, `input_to_frame_max_ms`, `input_samples` | For each key written while the previous one had been answered: the write to the first frame that reflects it. |
| `max_stall_ms` | The longest gap between two frames inside a window in which the program was active (scanning, or working on a deletion or an input). |
| `quit_ms`, `delete_ms` | The confirmation key to the exit of the process, and to `deletion_finished`. |
| `output_bytes`, `output_bytes_per_s`, `frames`, `inputs_sent` | Terminal output and its rate, frames drawn, and input events sent. |
| `peak_rss_bytes`, `user_ms`, `sys_ms` | Peak memory (the peak physical footprint on macOS, the peak resident set size on Linux) and the child's CPU time. |
| `threads`, `fds` | The most threads and descriptors seen in a sample taken every 50 ms (`libproc` on macOS, `/proc` on Linux; not sampled on Windows). |
| `scan_store_peak_bytes` | The peak total apparent size of the run's scan-store directory (`EXCISE_SCAN_STORE_DIR`), seen in a sample taken every 50 ms. |
| `scan_store_bytes_per_entry` | `scan_store_peak_bytes` divided by the `entries` of the `scan_complete` event; absent without one or the other, or when `scan_complete` reports zero entries. |

Timing values are only evidence when compared in paired, interleaved runs; see
[Output documents](#output-documents).

**Failure bundles.** A failed run leaves a directory with `failure.json` (the `harness-failure`
document), `session.cast`, `screen.txt`, `events.jsonl`, and `repro.txt`, which gives the exact
`excise` invocation and environment.

**Fixtures.** Each run gets a disposable copy of its scenario's fixture from the fixture generator
(`Fixtures::bundled().run_copy(id, dir)`), in a work directory under `/tmp` on Unix, where paths stay
short enough for the deletion dialog to show them whole, or under `EXCISE_E2E_TMPDIR`. The dialog is
at most 78 columns wide, and the Windows temporary directory is too long for it, so on Windows set
`EXCISE_E2E_TMPDIR` to a short directory; CI uses the runner's temporary directory. The copy is
removed when the run ends, and `--keep-fixture` keeps it. The runner checks ownership with the
generator's `verify_owned`, which refuses a root that is a symbolic link and a marker that is not a
regular file; `FixtureRoot` adds only the canonical spelling of the path.

## Running scenarios

```console
cargo xtask e2e [--quick|--full|--nightly] [--scenario NAME]... [--profile PROFILE]... [--repeat N] [--keep-fixture]
```

The command builds the `excise` release binary, or uses the one named by `EXCISE_E2E_BINARY`, loads
the scenarios in [`scenarios/`](scenarios), runs each selected one under its profiles, and prints a
verdict table. It exits non-zero on any `fail`, `xpass`, or `error`.

- `--quick` runs scenarios tagged `tier = "quick"` (the default) under the `default` and
  `deterministic` profiles only, and must stay within two minutes. `--full`, the default tier, adds
  `full` and every profile a scenario declares. `--nightly` adds `nightly` too.
- `--scenario` names a scenario and runs it whatever its tier, though a scenario outside its
  `platforms` is still skipped, with the reason, even when it is named. A scenario outside the
  selected tier or its `platforms` is otherwise skipped, with the reason, and printed.
- `--profile` narrows the matrix and may repeat, alongside `--scenario`. `--repeat N` runs each
  pair `N` times, which is how identical verdicts are shown.
- `--keep-fixture` keeps each run's fixture and scratch area and prints where they are.
- The summary is `target/excise-e2e/<run-id>/summary.json`, a `harness-summary` document, and
  `target/excise-e2e/latest` points at the newest run (a symbolic link, or on Windows a text file).
  Failure bundles sit beside it, one directory per failed run.

The negative control for the `delete` step is not a scenario. It is
[`tests/controls/delete-wrong-target.toml`](tests/controls/delete-wrong-target.toml): it selects a
folder and then asks `delete` for a different entry. `cargo xtask e2e` never runs it, because it is
not in `scenarios/`. `tests/harness_scenarios.rs` in the `excise` crate runs
`delete-folder-lifecycle` under every profile it names against the crate's own binary as part of
`cargo test`, along with a few one-off scenarios that exercise the runner itself, and runs the
control to assert that the `delete` step failed, that the last input was Backspace, that no `y`
appears among the recording's input events, and that every byte of the fixture is unchanged.

## Headless runner

```console
cargo xtask headless [--quick|--full] [--fixture ID]... [--class scale|identity|hostile|volumes]... [--profile default|deterministic] [--repeat N] [--timeout SECONDS] [--keep-scratch]
```

`excise_harness::headless` scans a fixture without a terminal, holds the report to the fixture's
oracle, and times the scan against `du -sk`. The exactness and throughput claims of the validation
program rest on it.

**A scan.** One run is `excise --format json --output <scratch>/scan-report.json <fixture-root>`,
under the isolation of the PTY runner: the environment is cleared and rebuilt from `TERM`,
`COLORTERM`, and `LANG`; `HOME`, `EXCISE_CONFIG`, the working directory, `TMPDIR`, and
`EXCISE_SCAN_STORE_DIR` are in a scratch area; and the root must carry the ownership marker. The
wait is bounded (`--timeout`, 900 s by default) and the process group is killed when the bound
passes. Afterwards the fixture is compared with the snapshot taken before the scan, and a scratch
area that holds anything but the report is residue. The run records the exit code, the wall time,
the CPU time, and, where the platform says, the peak memory.

**The report** is read only after it validates against the published
`docs/schemas/scan-report.schema.json` and the `native-path` schema it references. The file is
checked one entry at a time, so a large report never has to fit in memory twice. A report that
breaks the schema is `invalid-report`, a scan that ends without one is `no-report`, a scan that
does not end in time is `timeout`, and a scan that prints to standard output, although its report
goes to a file, is `unexpected-output`.

**The diff** joins every report entry to the oracle by its native path and holds the report to the
accounting contract (`docs/safety/accounting.md`, `docs/reports.md`). What an entry must say is
computed over the entries the report lists, so one missing entry is one discrepancy and not a wrong
number on every folder above it. The rules are the ones the oracle deliberately does not apply:

| Rule | What the report must say |
|---|---|
| Directory metadata is excluded | A directory's own size and blocks are in no total. |
| Allocation counts once per identity | Every name of a file shares one `(device, inode)` and one allocation. It counts once, at the lowest entry that holds every name the scan met; every name, and every directory below that one, shows 0. File length counts for every name. |
| Links are not followed | A link is an entry of its own: its length is its target text, and it has no descendants. |
| Reclaimable space | An identity is reclaimable where its allocation counts when every link the file system declares was met; otherwise the lower bound is 0 and the upper bound is the allocation. |
| Unknown is first class | An entry that is, or has below it, a directory that cannot be listed or a filesystem boundary is `uncertain` with a reason. Its lower bounds are exact and its upper bound is unknown or at least the lower bound. Every other entry is `complete` with exact bounds. |
| Scope | A mount point is a boundary: it is an `uncertain` record, and nothing below it is expected or accepted. |
| State and exit code | The document is `exact` when no entry is uncertain and `uncertain` otherwise, and the exit code is the one that goes with the state the report claims (0, 2, 3, 130). The summary counts links, unreadable folders, and boundaries as the tree has them, and no deletions. |
| Coverage | Every in-scope oracle entry is in the report exactly once, and the report lists nothing else. |

A discrepancy is typed (`root`, `state`, `exit-code`, `summary`, `missing`, `unexpected`,
`duplicate`, `out-of-scope`, `kind`, `identity`, `apparent-bytes`, `allocated-bytes`,
`reclaimable-bytes`, `descendants`, `entry-state`, `unscanned-reason`, plus the run's own
`timeout`, `no-report`, `invalid-report`, `residue`, `unexpected-output`, and `fixture-changed`).
The first 20 of each kind are listed and all are counted. Where the oracle has `null` facts
(Windows has neither identity nor allocation), the rules that need them are skipped and the rest
are applied.

**Fixtures.** `--quick` selects the fixtures of at most 10,000 planned entries and `--full`, the
default, those of at most 250,000. `--fixture` names fixtures and runs them whatever their size (the
1,000,000-entry fixture only ever runs by name), and `--class` selects every fixture that generates
a class. A fixture is scanned in its cached master, which the runner only reads, so a large fixture
is generated once. A fixture that `cargo clean` could not remove is never cached (see
[Cache and integrity](#cache-and-integrity)) and is scanned in a run copy instead, generated fresh
in the fixture's scratch area and removed when the fixture is done. A fixture with a volume part
also runs without privileges, but then the mount point is an empty directory, no boundary is
crossed, and the table says so. With `EXCISE_HARNESS_PRIVILEGED=1` the volumes are attached and the
boundary rule is exercised.

**The `du` reference.** The same warm tree is timed with `du -sk`, through the same supervised
process runner and in an empty environment, interleaved with the scans: a warm-up pair, which is
diffed and not timed, and then `--repeat N` pairs (five by default) as H, D, H, D, and so on. The
ratio of a pair is the scan's wall time over the wall time of the `du` that followed it, and the
table gives the median of the ratios and their minimum and maximum. The `du` flavor is found by a
probe, not by a version string, and the total it prints is checked against what the oracle predicts
from the raw facts, so that a ratio is never taken against a `du` that walked a different tree
(BSD `du` stops where a path passes `PATH_MAX`). The ratio is reported against the 3× budget of
the validation plan and not gated: gating belongs to the budget scenarios.

**Expected failures.** `expectations/headless.toml` lists the fixtures that fail the diff for a
known defect that is not yet fixed, with the semantics of `expect = "fail"`: a fixture that fails
with exactly the listed kinds is `xfail`, one that fails with other kinds is `fail`, and one that
no longer fails is `xpass` and fails the run, so that the entry is removed by the change that fixes
the defect. An entry names the platforms it applies to and the findings it documents.

**Output.** `target/excise-headless/<run-id>/summary.json` is a `harness-summary` with one
`headless-<fixture>` result per fixture, and `target/excise-headless/latest` points at the newest
run. The open `metrics` object of a result has `entries`, `runs`, `oracle_ms`, `generation_ms`,
`headless_ms` (the median, with `_min` and `_max`), `du_ms`, `headless_scan_ratio` (the median,
with `_min`, `_q1`, `_q3`, and `_max`), `du_kib`, `du_expected_kib`, `user_ms`, `sys_ms`,
`exit_code`, and `discrepancies` with one `discrepancies_<kind>` count per kind. A failing fixture
gets `headless-<fixture>/` beside the summary, with `discrepancies.txt`, `repro.txt`, and the
report. `tests/harness_headless.rs` in the `excise` crate runs the cheap fixtures that need no
privileges against the crate's own binary as part of `cargo test`, and runs a binary that writes a
report breaking the schema to assert that the run fails.

## Paired A/B benchmark

```console
cargo xtask bench-e2e --baseline <ref> [--baseline-binary PATH] [--candidate-binary PATH] [--fixture ID]... [--scenario NAME --profile PROFILE]... [--pairs N] [--seed S] [--timing-threshold FRACTION] [--memory-tolerance FRACTION] [--strict] [--timeout SECONDS]
```

`excise_harness::bench` builds (or accepts) two `excise` binaries and compares them with paired,
interleaved A/B runs on the same warm fixture. A single, unpaired timing never transfers across
sessions (the same binary and tree shape have measured 9.46 s one day and 3.8–3.9 s the next); only
a paired, interleaved comparison on one machine in one session counts as evidence.

**The two builds.** `--baseline <ref>` builds any git ref (a branch, a tag, or a SHA) in a
temporary detached worktree with its own `CARGO_TARGET_DIR`, release, `--locked`, then removes the
worktree. The built binary is cached by its resolved commit SHA under the target directory, so a
later run against the same commit does not rebuild. The candidate is always the current checkout's
release build. `--baseline-binary` and `--candidate-binary` each skip building that side and use
the given path instead (for tests, and for comparing prebuilt binaries); the document's
`baseline`/`candidate` `git_ref` records which was used.

**What is compared.** Every `--fixture ID` (repeatable) is a headless scan
(`excise --format json`, through the same supervised process runner the headless runner uses),
compared on wall time, user and system CPU, and peak memory. Every `--scenario NAME --profile
PROFILE` pair (repeatable) is a PTY scenario run, compared on `scan_complete_ms`, `first_frame_ms`,
`input_to_frame_p99_ms`, `max_stall_ms`, `peak_rss_bytes`, and any of its own `measure` names. A
fixture scan reuses one shared, warm root for every pair (the cached master, or one run copy when
the fixture cannot be cached, exactly as the [headless runner](#headless-runner) does); a scenario
gets a fresh copy of its fixture for every run, baseline and candidate alike, because a scenario may
delete or mutate it — what stays the same across its pairs is the fixture's spec and seed, not one
mutable tree. Metric names are qualified by their case (a fixture id, or `<scenario>-<profile>`),
for example `wide-1k__wall_time_ms` or `delete-folder-lifecycle-default__scan_complete_ms`, so one
run can compare several fixtures and scenarios without their metrics colliding.

**Pairs and statistics.** After one untimed warm-up pair (baseline, then candidate), `--pairs N`
(10 by default) measured pairs run the same way, interleaved baseline, candidate, baseline,
candidate, and so on. For every metric the document holds the per-pair candidate/baseline ratio,
their median, and a deterministic bootstrap 95% confidence interval of the median (2,000 resamples
seeded from `--seed`, so the same seed always gives the same interval; `0` when `--seed` is not
given).

**Verdicts.** A timing metric (every metric except `peak_memory_bytes`
and `peak_rss_bytes`) blocks when its median is more than `--timing-threshold` worse (`0.20` by
default, the `timing_ab_regression` budget) and its interval excludes 1.0 (no change); any other
worse median warns. A memory metric blocks when its median moves, either direction, more than
`--memory-tolerance` from 1.0 (`0.05` by default, the `memory_ab_tolerance` budget) and its
interval excludes 1.0; beyond tolerance without that confidence warns. The command exits non-zero
on any block.

**Context.** The document's `context` records both builds' resolved git SHAs and binary digests,
the toolchain (`rustc -Vv`), the host (hostname, OS and version, architecture, CPU model, logical
CPUs), every fixture compared (id, manifest hash, seed), the power state (`ac`, `battery`, or
`unknown`: macOS reads `pmset -g batt`, Linux reads `/sys/class/power_supply`, elsewhere is always
`unknown`), and the 1-minute load average at the start and the end. Other `excise` processes found
running are counted in `concurrent_excise_processes` and only warned about by default (the
maintainer usually has one session open); `--strict` aborts instead.

**Output.** `target/excise-bench-e2e/<run-id>/ab.json` is a `harness-ab` document (see
[Output documents](#output-documents)), and `target/excise-bench-e2e/latest` points at the newest
run. The command also prints a table: one row per metric, its median ratio, its confidence
interval, and its verdict.

```console
cargo xtask bench-e2e --baseline main --fixture wide-1k --pairs 5
```

## Safety rules

The harness only ever runs `excise` against fixtures it generated itself, and never against a real
path.

1. **Scenarios never name a root.** The runner supplies the fixture root. Every path in a scenario
   is relative to it.
2. **A fixture-relative path is canonical.** It is one or more `/`-separated normal components,
   spelled so that it names the same entry on every operating system. The rule is textual and
   stricter than any one system needs, so a scenario validates identically everywhere.
   `check_fixture_relative_path` is the one implementation, and `Scenario::validate` applies it to
   `sentinels`, `wait_fs_absent`, `wait_fs_present`, `fs_mutate`, and `expect_fs`. Each rule has
   its own typed `PathViolation`:
   - `Empty`: the path is never empty.
   - `Absolute`: it never starts with `/` or `\`.
   - `WindowsPrefix`: no component starts with a Windows drive designator such as `C:` or `C:x`.
     On Windows, joining a component that has a prefix but no root replaces the whole base path,
     so `a/C:x` would leave the fixture.
   - `Backslash`: it never contains a backslash; `/` is the only separator.
   - `CurrentDirectory`, `ParentDirectory`, `EmptyComponent`: no component is `.`, `..`, or empty
     (a doubled or trailing `/`).
   - `Colon`: no component contains `:`, which Windows uses for alternate data streams
     (`name:stream`). A drive designator is reported as `WindowsPrefix` instead.
   - `TrailingDotOrSpace`: no component ends with `.` or a space, which Windows silently removes.
   - `ReservedDeviceName`: no component is a Windows reserved device name: `CON`, `PRN`, `AUX`,
     `NUL`, `CONIN$`, `CONOUT$`, `COM0` to `COM9`, `LPT0` to `LPT9`, or the superscript forms
     `COM¹`, `COM²`, `COM³`, `LPT¹`, `LPT²`, and `LPT³` (U+00B9, U+00B2, U+00B3). Letters match
     without regard to ASCII case, and the name is the part of the component before its first
     `.`, ignoring trailing spaces, so both `nul` and `NUL.txt` are rejected. Near misses such as
     `COM10`, `COM⁴`, `CONIN`, and `CONOUT$x` are ordinary names.
3. **Resolution never follows symbolic links.** A runner resolves a fixture-relative path
   component by component without following links, so a link inside a fixture can never redirect a
   mutation or a check outside it.
4. **Deletion is guarded.** A scenario with a `delete` step must declare at least one sentinel. The
   `delete` step confirms only after the dialog matches the expected entry and the sentinels exist.
5. **Fixtures are owned.** The runner refuses any root without the harness ownership marker.
6. **Runs are isolated.** The runner clears the environment and rebuilds it from `TERM`,
   `COLORTERM`, `LANG`, and the profile's own settings. Each scenario gets a scratch `HOME`,
   `EXCISE_CONFIG`, working directory, and `EXCISE_SCAN_STORE_DIR`, so a theme commit, an export,
   or a killed run can never touch real state and residue checks are exact.

This crate enforces rules 1 and 2 and the sentinel requirement of rule 4: a scenario has no way to
name a root, and `Scenario::validate` rejects unsafe paths and unguarded deletions. Rule 3, the
run-time dialog and sentinel checks of rule 4, and rules 5 and 6 are obligations of the runner and
the fixture generator.

## Fixtures

The fixture generator builds the trees that scenarios run against. A fixture is a small TOML spec.
A seeded generator expands the spec into a manifest of every entry it will create, creates the
tree, and seals it with the ownership marker. An oracle then reads the tree back with `lstat`,
knowing nothing about the generator, so a generator that does not do what it says is caught. A
scenario names its fixture by id, and the id names the spec: `fixture = "node-modules-2k"` is
[`fixtures/node-modules-2k.toml`](fixtures/node-modules-2k.toml).

A runner takes an already materialized fixture root plus the scenario. It gets the root from
`Fixtures`:

```rust
let fixtures = Fixtures::bundled();
let copy = fixtures.run_copy("delete-folder", run_dir)?; // fresh, marked, removed on drop
let root = copy.root(); // what `excise` is pointed at
```

### Spec files

One file per fixture, `fixtures/<id>.toml`. Parsing rejects unknown fields at every level, and
loading validates the spec (sizes, counts, colliding roots, and at most 2,000,000 planned
entries), so an absurd spec never reaches the generator. `FixtureSpec::load(dir, id)`,
`load_bundled(id)`, `from_toml_str`, and `from_path` return typed errors.

| Field | Required | Meaning |
|---|---|---|
| `schema_version` | yes | `1`. |
| `id` | yes | 1 to 64 lowercase ASCII letters, digits, `-`, or `_`: the file name and the scenario's `fixture`. |
| `description` | yes | What the fixture is for. Not part of the spec hash. |
| `seed` | yes | The default seed. Names and sizes are a pure function of the spec and the seed. |
| `parts` | yes | At least one class generator. Each fills one top-level entry of the fixture, named by its `root`; roots are distinct. |

```toml fixture-spec
schema_version = 1
id = "node-modules-2k"
description = "The node_modules-shaped tree behind the scan-starvation finding."
seed = 1

[[parts]]
kind = "tree"
root = "node_modules"
depth = 4
fanout = 4
files_per_dir = 8
file_placement = "leaves"
dir_names = { style = "sequential", prefix = "pkg" }
file_names = { style = "sequential", prefix = "m", suffix = ".js" }
file_size = 1
```

Every part has a `kind` and a `root`, and rejects fields that are not its own. A *size* is a byte
count (`file_size = 1`) or an inclusive range drawn from the seed (`file_size = { min = 1, max = 64 }`);
a dense file is at most 1 GiB. A *name style* is one of:

- `{ style = "sequential", prefix = "", suffix = "", width = 0 }`: `prefix`, the index padded to
  `width` digits, `suffix`. Independent of the seed.
- `{ style = "hex", prefix = "", suffix = "", length = 8 }`: `length` hexadecimal digits that are a
  keyed permutation of the index, so names look random, never collide, and change with the seed.
- `{ style = "literal", name = "keep.txt" }`: one entry with exactly that name.

| `kind` | Class | Fields (defaults in parentheses) |
|---|---|---|
| `tree` | Scale | A directory tree of fixed fan-out: a wide directory (`depth = 0`), the `node_modules` shape, or many tiny files. `depth` (required, at most 32), `fanout` (1), `files_per_dir` (0), `file_placement` (`all`, or `leaves` for the deepest directories only), `dir_names` (sequential `d`), `file_names` (sequential `f` `.dat`), `file_size` (1). |
| `deep` | Scale | A chain of nested directories, created relative to open directory handles so that it can pass `PATH_MAX`. `depth` (required, at most 512), `name_len` (required, 5 to 255 bytes), `files_per_level` (0), `file_size` (1). The deepest path has `len(root) + depth × (name_len + 1)` bytes: choose it above 1,024 (macOS) or 4,096 (Linux). |
| `file` | Any | One top-level file, for sentinels. `size` (1). |
| `identity` | Identity | Features are off until asked for. Entries live in `links/`, `dangling/`, `symlinks/`, `loops/`, `sparse/`, and `clones/` below the root, and every link target is relative. `hard_link_groups` (0) of `links_per_group` (2, at most 64) names, spread over `link_spread` (2) directories `links/d0`, `links/d1`, and so on, of `link_file_size` (4096); `dangling_symlinks` (0); `valid_symlinks` (false: one link to a file and one to a directory); `symlink_loops` (a list of `self`, `pair`, and `directory`); `sparse_files` (a list of `{ apparent_mib, data_kib = 4 }`, above 16 MiB to stay sparse on APFS); `clones` (0) of one original of `clone_kib` (64) KiB. |
| `hostile` | Hostile | `features`, at least one and without repeats, of `control_names`, `bidi_names` (U+202E and friends), `escape_names` (ESC sequences), `newline_names`, `invalid_utf8_names`, `long_names` (255 bytes), `unreadable_dirs` (modes 000 and 100, with contents), and `unreadable_files` (modes 000 and 200). Each name feature creates one directory per name, holding a file of the same name. |
| `volume` | Volumes | A mount point: an empty directory in the master. `size_mib` (required, 8 to 1024), `files` (0) of `file_bytes` (1024) written once a volume is attached (see [Volumes](#volumes)). |

The specs the crate ships. Entries are planned entries; every generated fixture also holds the
marker.

| id | Entries | Purpose |
|---|---|---|
| `wide-1k` | 1,001 | Scale: one directory of 1,000 files. |
| `node-modules-2k` | 2,389 | The F1/F2 repro shape: `node_modules/pkg{0..3}` nested 4 levels, 8 one-byte `m{0..7}.js` files per leaf (2,390 entries with the marker). |
| `deep-past-path-max` | 122 | Scale: a 20-level chain of 240-byte names, past `PATH_MAX` on every platform. |
| `identity-small` | 34 | Hard links across directories, dangling and looping symlinks, a sparse file, and a clone. |
| `hostile-small` | 80 | Hostile names and unreadable entries. |
| `all-classes-small` | 253 | Every class once, in one fixture. |
| `delete-folder` | 5,014 | A 5,000-entry victim for deletion scenarios. |
| `delete-file` | 5 | A 48 KiB victim file, a sentinel beside it, and a folder below it with two more files that must survive. The in-process lifecycle scenario deletes the victim. |
| `navigate-folders` | 6 | Two folders and a file beside them, to drill into and back out of. |
| `mount-boundary` | 13 | An ordinary `outside/` tree and an empty mount point; a privileged run copy attaches a 16 MiB volume with 20 files. |
| `scan-store-quota` | 35,002 | A flat directory of 35,000 tiny files beside an empty mount point; a privileged run copy attaches an 8 MiB volume there for `scan_store_on_volume` (see [Volumes](#volumes)). |
| `tiny-files-50k` | 49,050 | 49 directories of 1,000 tiny files. |
| `tiny-files-1m` | 1,010,101 | One million tiny files. For nightly and manual tiers only: tests never generate it. |

### Determinism and the manifest

The plan is a pure function of the spec and the seed. Names and sizes come from the crate's own
SplitMix64 generator, seeded per part and per file, so adding a part never shifts the others. There
is no clock and no hash-map iteration anywhere in the output. The manifest lists every planned
entry in canonical order (component-wise byte order, so a directory precedes its contents): path,
kind, size, symlink target, hard-link group, permission override, sparse data length, clone source,
and the capability the entry needs. Its SHA-256, the *manifest hash*, is defined by the byte
encoding documented in `src/fixture/plan/mod.rs` and pinned by a test. The same spec and seed
therefore give the same manifest hash on every machine, and a different seed gives a different one
(except for a spec whose names and sizes use no seeded style, like `node-modules-2k`).

Some entries need a capability that not every file system has: `symlinks`, `hard_links`,
`sparse_files`, `clones`, `invalid_utf8_names`, `control_character_names`, and `restricted_modes`.
They are in the plan everywhere. The generator probes the file system once, skips the entries whose
capability is missing, and records each capability, the probe's evidence, and the number of skipped
entries in the marker. The marker also holds a *realized hash* over the entries that were created;
it equals the manifest hash exactly when nothing was skipped.

| Capability | macOS (APFS), observed | Linux, from the code | Windows, from the code |
|---|---|---|---|
| `symlinks` | yes | yes | not created: the Windows file layer has no symbolic links |
| `hard_links` | yes | yes | yes |
| `sparse_files` | yes (32 MiB occupies 16 KiB) | yes | decided by the probe |
| `clones` | yes: `fclonefileat`, no subprocess | only where the file system shares extents (`FICLONE`: btrfs, XFS with reflinks); skipped on ext4 and tmpfs | not created |
| `invalid_utf8_names` | no: APFS answers `EILSEQ` | yes on ext4 | no |
| `control_character_names` | yes | yes | no |
| `restricted_modes` | yes | yes; a root process still reads mode 000 entries, which the marker records as `running_as_root` | not created |

Only the macOS column has been observed. The Windows layer is compiled and type-checked for
`x86_64-pc-windows-msvc` but has never run; there the oracle also lacks `allocated`, `dev`, `ino`,
`nlink`, and `mode`, which stable `std` does not expose.

### Cache and integrity

`FixtureCache` keeps generated masters in `<target dir>/excise-fixtures.noindex/<spec hash>-<generator
version>/`, where the target dir is `CARGO_TARGET_DIR` or the workspace `target` and the spec hash
is 16 hexadecimal digits. The name changes with the spec, the seed, and the generator version, so a
stale entry is never mistaken for a current one. Generation happens in a `.partial-…` sibling that
is renamed into place once the marker, written last, has sealed it. Tests pass a temporary
directory as the cache root and never touch the shared cache.

The shared cache holds only trees that a path-based removal can remove, so that `cargo clean` and
`git worktree remove` can always remove the target directory. Such a removal fails on a directory
that cannot be listed (the hostile `unreadable_dirs` feature) and on a path longer than `PATH_MAX`
(a `deep` part), so `Fixtures::master` refuses a fixture with either
(`FixtureSpec::removable_by_path`), and a runner takes a run copy of it instead.

The marker `.excise-harness-owned` is a regular file (never a link) at every fixture root. Its JSON
records the generator version, spec id, spec hash, seed, role (`master` or `run-copy`), manifest
and realized hashes, entry counts, skipped counts, capabilities, generation time, and threads.
Before a cached master is reused it is verified, cheapest first:

1. the marker exists, parses, and names this generator version, spec id, spec hash, seed, and manifest hash;
2. the marker's realized hash agrees with the plan;
3. the top-level names equal the planned ones plus the marker;
4. the contents: every planned entry when the plan has at most 20,000 entries, otherwise a sample of
   64 chosen from the manifest hash. `Verify::Full` forces the full walk.

An entry that fails is removed and regenerated, a partial directory left by a killed generation is
never used, and a hit skips generation. Two processes that race to build the same entry agree on
one.

### Per-run copies

A cached master is read-only for runners. `Fixtures::run_copy(id, parent)` generates the same plan
fresh into a uniquely named directory below `parent`, with its own marker (`role = "run-copy"`).
Generating rather than copying makes the copy exact by construction, hostile modes and links
included. Dropping the copy removes it: `remove_tree` restores owner access to each directory before
it descends and works through directory handles, so it removes trees that contain mode 000
directories and trees deeper than `PATH_MAX`. `RunCopy::keep` opts out. `RunCopy::regenerate(part)`
removes one top-level entry and generates it again, so a deletion scenario can run again without a
new copy, and `RunCopy::oracle()` walks the copy as it is now.

### Oracle

`Oracle::collect(root)` walks the tree with `lstat`, never following a link, and reports raw facts
and none of Excise's accounting rules (`docs/safety/accounting.md`); the differential runner maps
those rules onto them. The document has `document_kind = "harness-fixture-oracle"` and
`schema_version = 1`:

| Field | Meaning |
|---|---|
| `platform` | `os`, and whether `identity` (`dev`, `ino`, `nlink`) and `allocation` (`allocated`) are filled in. |
| `entries` | The root (empty path) first, then every entry in canonical order. |
| `entries[].path` | Relative to the root, in the native-path encoding of Excise's JSON reports (`docs/schemas/native-path.schema.json`): `unix-bytes` with base64 data on Unix, so a name that is not valid UTF-8 round-trips. |
| `entries[].kind` | `directory`, `file`, `symlink`, or `other`. |
| `entries[].size` | `st_size`. For a symbolic link, the length of its target; for a directory, a property of the file system. |
| `entries[].allocated` | `st_blocks × 512`, or `null` where the platform cannot say. |
| `entries[].dev`, `ino`, `nlink`, `mode` | `st_dev`, `st_ino`, `st_nlink`, and `st_mode & 0o7777`, or `null`. |
| `entries[].readable` | Whether the walking process can open the entry, and list it if it is a directory. A fact about the process: root reads a mode 000 file. |
| `entries[].device_boundary` | The directory is on another device than its parent: a mount point. |
| `entries[].symlink_target` | The text of a symbolic link, in the same encoding as `path`. |
| `entries[].subtree` | For a directory, the totals below it: `entries`, `files`, `directories`, `symlinks`, `others`, `unreadable_directories`, `apparent_bytes`, `apparent_unique_bytes`, `allocated_bytes` (each `(dev, ino)` once), `directory_apparent_bytes`, and `directory_allocated_bytes`. |
| `hard_links` | Files with more than one name: `dev`, `ino`, `nlink`, and the `paths` the walk found. |

Read like `du`: `du -sk root` is `root.allocated + subtree.directory_allocated_bytes +
subtree.allocated_bytes`, rounded up to KiB, in every `du`. The apparent-size total depends on the
implementation, and the tests pin each rule exactly, with the expected value computed from the
per-entry facts:

| `du` | Command | Adds up `size` of | Unit |
|---|---|---|---|
| GNU coreutils 9.2 and later | `du --apparent-size -sb` | regular files and symbolic links only | bytes |
| GNU coreutils before 9.2 | `du --apparent-size -sb` | every entry, directories included | bytes |
| BSD (macOS) | `du -A -sk` | every entry, directories included, each rounded up to 512 bytes first | KiB, rounded up |

GNU 9.2 stopped counting directories and every other entry that is not a regular file or a symbolic
link (its NEWS: "`du --apparent` now counts apparent sizes only of regular files and symbolic
links"), so on such a system a tree is short by the `size` of its directories, 4,096 bytes each on
ext4. The tests do not read the version: they run `du` on a directory whose contents they know and
see which rule applies. Every `du` counts a hard-linked file once per `(dev, ino)`. For a directory
it cannot list, GNU adds the directory's own size and blocks after its `cannot read directory`
message (its source: "even if this directory is unreadable ... do let its size contribute to the
total") and BSD leaves it out; the oracle keeps it, flagged `readable = false`, so the caller
chooses. BSD `du` stops where a path passes `PATH_MAX`, so it sees only the top of a `deep`
fixture, while GNU `du` descends by descriptor; the oracle always walks all of it. The tests check
every class that `du` can walk.

### Live mutators

`mutate::apply(root, op, path)` performs a scenario's `fs_mutate` step at a step boundary. It refuses
a root without the marker, a path that breaks `check_fixture_relative_path` (`..`, an absolute path,
a backslash, a Windows drive or device name), any path that names the marker, and any path that has
a symbolic link in it: each component is opened relative to an open directory without following
links, and the last one is not followed either, so `vanish` removes a link and never its target.
Content is a function of the path, so a repeated run makes the same bytes.

| `op` | Effect |
|---|---|
| `appear` | Creates a regular file of 4,096 bytes, and any missing directories above it. Fails if the path exists. |
| `change` | Appends 4,096 bytes to an existing regular file: same inode, larger size. |
| `vanish` | Removes an existing entry. A directory goes with everything below it. |
| `replace` | Gives an existing entry a new identity under the same name. A file is replaced atomically, through a rename, by a new file of 1,024 bytes. A directory is swapped for a new empty one, so the name is briefly absent. |

### Volumes

A `volume` part puts only an empty mount-point directory in the master. `RunCopy::attach_volumes`
attaches a size-limited volume there and writes the part's files onto it, so a run has a real mount
boundary. The copy detaches its volumes before it removes its tree, and removal never crosses a
mount boundary: a directory on another device is an error, so an attached volume is never emptied by
accident.

Creating a volume needs privileges on Linux and Windows and touches host mount state everywhere, so
it needs an explicit opt-in: a `PrivilegedOptIn`, from `PrivilegedOptIn::from_env()` (the variable
`EXCISE_HARNESS_PRIVILEGED` set to exactly `1`) or `PrivilegedOptIn::granted()` for an operator's
explicit flag. Nothing attaches a volume by default, and the tests skip their volume steps without
the opt-in.

| OS | Mechanism | Privilege |
|---|---|---|
| macOS | `hdiutil create -type SPARSE -fs APFS`, then `hdiutil attach -mountpoint` | None. |
| Linux | A sparse image formatted with `mkfs.ext4` and mounted with `mount -o loop,nodev,nosuid` | Root, or `sudo -n`. Written from the tools' documentation and never run. |
| Windows | A `diskpart` script that creates and formats a VHD and assigns it to the folder | Elevated. Written from documentation and never run. |

```console
EXCISE_HARNESS_PRIVILEGED=1 cargo test -p excise-harness --locked --lib volume
```

**`cargo xtask e2e` and `scan_store_on_volume`.** The PTY runner (`run_e2e`/`run_scenario`)
always starts `excise` with `EXCISE_SCAN_STORE_DIR` inside its own per-run scratch area (see
[Isolation](#isolation)), on the same file system as everything else the scenario touches. A
scenario sets `scan_store_on_volume = true` to point it at `<mount>/store` on the fixture's
attached volume instead, so the scan store's own free-space math runs against a real, tiny,
disposable file system rather than the host disk. `select` skips any scenario whose fixture
`has_volumes()` without the `EXCISE_HARNESS_PRIVILEGED` opt-in, named or not (the same rule the
in-process runner already applies unconditionally, since it never attaches a volume at all); with
the opt-in, `run_one` attaches every volume part before the process starts, and
`scan_store_on_volume` additionally requires the fixture to declare exactly one of them. The runner
creates `<mount>/store` before it takes the fixture's baseline snapshot: the store then lives inside
the fixture tree, so `expect_exit`'s fixture diff reports anything excise leaves in it, while
`residue = "none"` still covers the scratch area. A scenario's steps cannot see free space, so
`tests/harness_scan_store_quota.rs` runs `scan-store-quota` against a real volume and checks that a
scan stopped at the quota left the volume's reserve free:

```console
EXCISE_HARNESS_PRIVILEGED=1 cargo test --locked --test harness_scan_store_quota
```

## Output documents

Machine output is versioned JSON. Every document carries a `document_kind` and a `schema_version`
(`1`), like the published Excise scan report, and has a draft 2020-12 schema with
`additionalProperties: false`.

| Document | `document_kind` | Schema | What it is |
|---|---|---|---|
| Summary | `harness-summary` | [`harness-summary.schema.json`](schemas/harness-summary.schema.json) | The result of one run: run id, tier, times, host, the binary's path and SHA-256, the git SHA, and a verdict, duration, and metrics for each scenario and profile. |
| Failure bundle | `harness-failure` | [`harness-failure.schema.json`](schemas/harness-failure.schema.json) | The evidence for one failed scenario: the failed step, expected and actual screen text, terminal modes, the recording path, resource use, the fixture hash and seed, and a command that reruns it. |
| A/B evidence | `harness-ab` | [`harness-ab.schema.json`](schemas/harness-ab.schema.json) | Paired, interleaved comparison of two builds: identities, trials, run order, per-metric samples, median ratio, bootstrap confidence interval and verdict, and the conditions it ran under (the fixtures compared, the host, the toolchain, the power state, the load average, and concurrent `excise` processes). |

Each schema's `$id` is
`https://github.com/findyourexit/excise/harness/schemas/<document_kind>-v1.json`. The Rust types
are `HarnessSummary`, `HarnessFailure`, and `HarnessAb` in `excise_harness::report`. They implement
`Document`, which carries the kind, the schema id, and the schema text, and renders the canonical
form: pretty-printed JSON in field order with a final newline.

The schemas live here, not in `docs/schemas`, because that directory is copied into release
archives and packages and these formats are not part of the product. The types and the schemas
reject unknown fields, so any change to a document's shape needs a new `schema_version`. The tests
keep the Rust types and the schemas in step: every schema compiles, its `$id`, `document_kind`, and
`schema_version` match the Rust constants, serialized samples validate, and every field a schema
declares is serialized by the types.

By convention a run writes its summary to `target/excise-e2e/<run-id>/summary.json`. Timing evidence
is only meaningful from paired, interleaved A/B runs on one host in one session; the `harness-ab`
document records the host, CPU, architecture, logical CPUs, toolchain, power state, load average,
the fixtures compared, and concurrent `excise` processes so a comparison can be judged.

## In-process runner

The in-process runner runs a scenario against the real owner loop inside the `excise` crate's own
test binary, with a ratatui `TestBackend` as the terminal. It is test-only code in
`src/tests/scenario_runner.rs`, so this crate still never depends on `excise`. The `excise` tests
run every scenario in [`scenarios/`](scenarios) that the runner can perform, under every profile
the scenario declares:

```console
cargo test -p excise --lib scenario_runner
```

The runner takes an already-materialized fixture root and refuses one that the harness does not
own (`verify_owned`: a real directory, not a link, holding `.excise-harness-owned` as a regular
file). The suite gets each root from [`Fixtures`](#fixtures):
`Fixtures::bundled().run_copy(id, parent)` generates a fresh, marked copy of the fixture the
scenario names, in a scratch directory of its own, and removes it when it drops. A run that deletes
or mutates therefore never touches the cached master.

### Profiles

The profiles are the in-process column of the contract, built from the same options through the
same configuration layers, without reading the environment or a configuration file:

| Profile | In-process configuration |
|---|---|
| `default` | The configuration defaults. |
| `deterministic` | Reduced motion, loading animation off, one scan thread. |
| `monochrome-ascii` | The monochrome theme, ASCII symbols and borders. |
| `narrow` | A backend 60 columns wide, with the scenario's rows. |
| `mouse-keymaps` | Mouse input, the Emacs key preset. |

### Steps

| Step | In-process behavior |
|---|---|
| `key`, `type` | Deliver key events. An uppercase letter carries Shift. An unmodified `e` or `E` outside a text prompt is refused: it would export a report into the process working directory, which the runner cannot isolate. |
| `settle` | Delivers one barrier. See [`settle`](#settle-and-waits). |
| `wait_text`, `expect_screen` | Match the screen text, or one region of it: `header` (the top three rows), `dialog`, or `rows`. |
| `wait_header` | Reads the state badge that ends the title row. `scanning` can only be seen before the first settle. |
| `select` | Opens the filter, replaces what it holds, types the name, applies it, and waits for the selected-item panel to name exactly that entry. The panel needs a terminal of at least 19 rows. |
| `delete` | Presses Backspace, parses the dialog, asserts its title and path name exactly this entry and kind under the fixture root (a path the dialog cut short cannot be checked), asserts every sentinel exists, and only then sends `y`. A mismatch fails the step and `y` is never sent. |
| `wait_fs_absent`, `wait_fs_present`, `expect_fs` | Resolve paths one component at a time, never through a symbolic link. |
| `fs_mutate` | Applies `appear`, `change`, `vanish`, or `replace` with the [live mutators](#live-mutators), once the program is at rest: unless the last thing delivered was a barrier, one comes first, so even a mutation that opens the scenario lands after the first scan has settled. Nothing tells the program, so what it does about the change is what the steps after this one observe. A refused mutation fails the step with the mutator's reason. |
| `resize` | Resizes the backend and delivers the resize event. |
| `quit` | Presses `q`, waits for the plain `Quit Excise?` prompt, and presses `y`. A prompt about pending deletion work fails the step. |
| `expect_exit` | Asserts the exit code the binary would return, and with `residue = "none"` that the run's scratch directory for its scan-store session is empty. `terminal_restored = true` is accepted and not evaluated: there is no terminal to inspect. |

Every sentinel is asserted again when the run is over.

A scenario with any other step is rejected before it runs, with `RunError::Unsupported { steps }`:

- `signal` needs a separate process to receive it.
- `wait_event` needs the event channel, which belongs to a separate process.
- `expect_budget` and `measure` judge timing and resources, which are never judged in-process.
- `expect_exit` with `terminal_restored = false`, and any step after `expect_exit`.
- `type` text with control characters.

The test suite selects only the scenarios the runner can perform, and skips the rest with the
reason: a scenario outside its `platforms`, a scenario whose `tier` is not `quick`, a step only
another runner can perform, a fixture that needs a scratch volume (only a privileged process
runner attaches one), or a fixture that plans more than 10,000 entries (the large fixtures are for
the runners built for them). A scenario file that does not parse, does not validate, is not named
after its scenario, or names a fixture with no loadable spec fails the suite instead of being
skipped.

### `settle` and waits

`settle` is the runtime's barrier: it renders, then drains worker events, background deletion
work, timers, and animation until the owner loop is quiescent. In-process time is virtual, and the
barrier drains work outside the production scheduling path, so **this runner never judges
scheduling, throughput, latency, memory, or any other budget**. The pseudo-terminal and headless
runners judge those.

Every wait is a bounded loop: check, and while the condition does not hold, deliver a barrier and
check again, until the step's `timeout_ms` (wall time) or the round limit is reached. Reaching a
bound fails the step with what was seen. A check runs on a fresh screen: when input was delivered
since the last barrier, a barrier comes first, so a scenario needs no `settle` before an
assertion and no barrier counts found by trial. A barrier itself waits for real scan and deletion
work and is not cut short by `timeout_ms`.

### Failures and verdicts

A failure names the step index and description, the reason, the expectation, and the screen text.
A run resolves to a `Verdict` with strict xfail: an `expect = "fail"` scenario that passes is an
`xpass` and fails the run.

### Adding a scenario

1. Write `scenarios/<name>.toml`, named after the scenario. Lifecycle scenarios declare the
   `default` and `deterministic` profiles.
2. Name a fixture from [`fixtures/`](fixtures), or add a spec there (see
   [Spec files](#spec-files)).
3. Use only the steps above. Put a `quit` and an `expect_exit` at the end, so the run ends the way
   a user would end it; a scenario that stops earlier is stopped by the runner.
4. Run `cargo test -p excise --lib scenario_runner -- --nocapture`. A scenario that this runner
   cannot perform is skipped, never silently dropped: the suite prints `SKIP <name>: <reason>` for
   each one.

## Working on the crate

```console
cargo test -p excise-harness --locked
cargo clippy -p excise-harness --all-targets --locked -- -D warnings
```

The crate follows the workspace lints: no `unsafe`, pedantic Clippy, and no `unwrap`. Development
guidance for the harness as a whole lives in
[`docs/development.md`](../../docs/development.md#validation-harness).
