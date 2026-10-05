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
- [`counts`](src/counts): the deterministic counts of a build, their history on the `bench-data`
  branch, and the pull-request comment of count deltas.

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
| `cgroup_memory_cap` | no | Spawns `excise` under the Linux cgroup memory cap instead of only sampling its memory. Defaults to `false`. Needs `EXCISE_HARNESS_CGROUP=1` and a host that can do it; every runner skips a scenario that sets this without both, with the reason, even when it is named. See [Linux cgroup memory cap](#linux-cgroup-memory-cap). |
| `profiles` | yes | A non-empty, duplicate-free list of profiles the scenario runs under. |
| `tier` | no | `"quick"` (default), `"full"`, or `"nightly"`: which `cargo xtask e2e` tier runs it. See [Tiers and platforms](#tiers-and-platforms). |
| `platforms` | no | The operating systems the scenario runs on, as `std::env::consts::OS` spells them. Defaults to all three. See [Tiers and platforms](#tiers-and-platforms). |
| `terminal` | no | The initial terminal size, and an optional cap (`drain_bytes_per_sec`) on how fast the pseudo-terminal runner drains this scenario's output (see [Terminal throughput](#pty-runner)). Size defaults to 120 columns by 40 rows, at least 32 by 8; the drain cap defaults to unthrottled. |
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
| `reduced-motion` | Reduced motion only; thread count is unchanged. The motion baseline for the ratio comparisons (see [Comparison files](#comparison-files)), so a real motion slowdown cannot hide behind `deterministic`'s extra threads. |
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
budget's limit in `[budgets]`; without an override the runner applies the fixed default below,
which is the strict limit the validation program holds locally and in the nightly tier. The
scenarios therefore never say which tier they run in.

A run can carry a **latency scale**, `cargo xtask e2e --latency-scale 2`, which multiplies the
limit of the four latency budgets (`input_to_frame_p99_ms`, `max_stall_ms`, `first_frame_ms`, and
`quit_ms`), whether the limit is the default or a scenario's override. Pull-request CI runs the
quick tier and some `full` scenarios at 2, because a hosted runner is busier than the machines the
strict budgets are held on. A scale is a finite number of at least 1. It never touches any other
budget: memory, thread and descriptor counts, scan-store bytes, idle output and CPU, residue, and
the ratios are contracts that a busy machine does not excuse. A scenario that expects to fail on
the platform it runs on (`expect = "fail"`) keeps the strict budgets whatever the scale is: the
defect it documents is measured against them, and a looser limit that the defect stays within would
read as `xpass`, the signal that it is fixed. A scaled run says so: the summary carries
`latency_budget_scale`, and the verdict table's last line names the factor. A strict run records
neither.

A run can also hold **timing informational**: `--timing-informational` on `cargo xtask e2e`,
`compare`, and `headless`, for a hosted machine that is slower than the one the budgets were set
on. A timing verdict that would block is then a warning and does not fail the run: one of a
scenario's four latency budgets (after any latency scale), a comparison's median ratio over its
limit, or a headless scan's ratio against `du` over its budget. The verdict table lists each
warning (`WARN`) and counts them on its last line; each scenario or fixture result in
`summary.json` carries its own in `timing_warnings` (`budget`, `metric`, `value`, and `limit`), and
the summary says `timing_informational`; the exit status ignores them. Nothing else is excused:
the other budgets, a wait that times out, a failed step, residue, an oracle diff, a comparison
whose run never completed within its bound, and a scan or `du` that does not end in time fail as
before. Whatever is expected to fail (`expect = "fail"`, a comparison's `expect`, an
`[[expect_ratio_fail]]` entry) keeps its strict verdict, because excusing its miss would read as
`xpass`, the signal that the defect is fixed. A strict run records neither field.

```toml
[budgets]
max_stall_ms = 400
idle_output_bytes = 0
```

| Budget | Unit | Limits |
|---|---|---|
| `headless_scan_ratio` | ratio | Headless scan time divided by `du -sk` time on the same fixture. |
| `tui_complete_ratio` | ratio | Interactive time-to-COMPLETE divided by headless scan time. |
| `motion_complete_ratio` | ratio | Default-motion time-to-COMPLETE divided by reduced-motion time-to-COMPLETE. |
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

`tui_complete_ratio` and `motion_complete_ratio` are ratios between two runs, so no single run's
`expect_budget` can compute them: see [Comparison files](#comparison-files) for how a comparison
file checks one. Every other budget here is a single run's own metric, checked directly by
`expect_budget`.

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
| `idle` | `after_ms`, `window_ms` | Sends nothing, waits `after_ms`, then measures over `window_ms` the terminal output bytes and the child's live CPU time, recording them as `idle_output_bytes` and `idle_cpu_ms`. Not one of the waiting steps above: both durations are unconditional, so there is no `timeout_ms`. |
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
  opened. Testing a field the event does not carry is a validation error. A `frame`'s `inputs` is
  the program's own count, which can include inputs the runner did not send (see [Runner
  semantics](#runner-semantics)), so a test on it reads the program's number, not the runner's.
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
- **`idle`.** Pseudo-terminal only: the in-process runner has no separate process to sample and
  skips any scenario that uses it. The child's live CPU time is not sampled on Windows (see
  [Measurements](#pty-runner)); `idle_cpu_ms` is simply not recorded there, so an `expect_budget`
  step checking it reports the metric as not recorded rather than a value.
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
- **Frames reflect inputs.** The runner counts the input events it writes, and a `frame` event
  carries `inputs`, the number of input events the program had consumed when it drew that frame.
  The program may have counted some the runner did not send before the first: on Windows `excise`
  receives `ConPTY`'s first window-size event before any key is sent, so `inputs` is already 1 in
  its first frames, where on macOS it is 0. The runner therefore takes the `inputs` of the latest
  frame, or 0 before the first, when it writes its first input, and calls it the input baseline. A
  frame reflects the inputs sent so far when its counter is at least the baseline plus their number
  and it was observed after the last one was written. `settle`, every other step that waits for a
  frame, and the `input_to_frame_*` metrics use that rule. Without the baseline, on Windows a frame
  drawn after a key by anything else (an animation tick, say) would count as the key's answer: a
  `settle` could return before the program had handled the key, and the latency of a key was the
  time to the next frame. An input that the program counts of its own only after the first one was
  written is not accounted for.
- **Bounds.** Every wait is bounded. When a bound elapses the runner kills the process group,
  writes a failure bundle, and leaves nothing behind. A step that times out also reports the
  session's diagnostics in its failure detail and in the bundle's `screen.txt`: how many output
  bytes arrived and when the first one did, whether the child is still alive, how many `ESC[6n`
  cursor-position requests were answered, and the bounded head and tail of the raw output stream
  (`PtySession::diagnostics`). An empty screen at the deadline reads as zero bytes and zero
  answered requests: the program's output never reached the screen model at all. When
  `EXCISE_HARNESS_DIAGNOSTIC_COMMAND` names a command (every `{pid}` in it replaced with the
  child's process id, for example `cdb -pv -p {pid} -c "~*k 40; qd"` on Windows or
  `sample {pid} 2` on macOS), a timed-out step also runs it non-invasively against the child and
  appends its output to the same failure detail and `screen.txt`, bounded to 15 s and 64 KiB
  (`PtySession::sample_process`). The command is split into words as a POSIX shell would split
  it, but never run through one, so quote a Windows path: an unquoted backslash is dropped.
  Unset, the variable costs one environment lookup and nothing else.
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
| `pty` | The session: `portable-pty` with a `vt100` screen model, key encoding for every scenario key with Ctrl and Alt, `ESC[6n` cursor-position requests answered from the screen model, resize, an asciicast v2 recording with output (`"o"`) and input (`"i"`) events, a token-bucket cap on how fast it drains the child's output (see [Terminal throughput](#pty-runner)), the terminal modes that failure bundles report, and the diagnostics (output timing, the count of answered cursor-position requests, and the bounded head and tail of the raw stream) a timed-out step's failure reports. |
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

**Terminal throughput.** A scenario's `[terminal]` table, or a comparison's own field, can set
`drain_bytes_per_sec`: it caps how fast the reader thread takes bytes from the pseudo-terminal,
with a token bucket rather than a sleep per chunk, so it paces throughput without distorting
timing. A PTY's kernel buffer blocks a write once it fills, so capping the drain rate below what
the program writes puts the same backpressure on it that a real slow terminal does — this is how a
comparison reproduces a regression that only a real terminal's pace exposes, which an unthrottled
pseudo-terminal cannot. The cap stops applying, and the reader drains the rest of the output as
fast as the operating system allows, the instant `PtySession::kill` is called (a timeout, a failed
step, an ordinary run ending, and dropping the session all go through it): a process that still has
output queued when its terminal closes keeps writing until it is read, so a cap that outlived the
kill could delay or deadlock the reap. A step under a cap should wait on the event channel
(`wait_event`), not the screen (`wait_text`, `wait_header`): the screen lags the capped reader, so
a screen-based wait can time out long after the event it is really waiting for already fired.

### Linux cgroup memory cap

A scenario with `cgroup_memory_cap = true` is spawned under
`systemd-run --scope -p MemoryMax=<limit> -p MemorySwapMax=0 --collect --unit=<name> [--user]`
instead of only being sampled for memory: cgroup v2's own OOM killer enforces `<limit>` (the
scenario's effective `peak_rss_bytes` budget), rather than this crate finding out after the fact
that a scan used too much. `--user` is added only when a reachable user service manager and bus
are found (`$XDG_RUNTIME_DIR/bus`); otherwise the scope is created in the system manager instead,
which needs whatever privilege the host already grants for that (root, or a polkit rule) — this
crate never elevates privileges itself. `--collect` unloads the transient unit whether the wrapped
program succeeds or fails, so nothing but the scope's own (automatic) cgroup removal is needed for
no residue.

This needs the `EXCISE_HARNESS_CGROUP=1` opt-in, and a host that can do it (Linux, `systemd-run` on
`PATH`, cgroup v2 mounted): every runner skips a scenario that asks for it without both, with the
reason, even when it is named, the same way a fixture with a volume part needs
`EXCISE_HARNESS_PRIVILEGED`. Off (the default), nothing here changes how a scenario runs.

Backgrounding `systemd-run --scope` directly and comparing its own pid against the wrapped
program's self-reported pid shows they are the same: the wrapped program *becomes* the
`systemd-run` process (an `execve`, not the fork `systemd-run(1)`'s own wording, "a scope command
is executed by systemd-run itself as parent process," suggests), keeping its pid, its process
group (so the existing process-group kill is unaffected), and its cgroup throughout.
`peak_rss_bytes` and `idle_cpu_ms` therefore already read the right process with no extra step;
`memory.peak` is read from that same pid, at the same "exited, not yet reaped" moment the existing
peak-memory sampling already uses, and reported as the metric `cgroup_memory_peak_bytes`. `--user`
needs `XDG_RUNTIME_DIR` in `systemd-run`'s own environment before it can reach the bus to register
the scope; an isolated spawn environment does not carry it by default, so both runners add it back
whenever `--user` is chosen. `safety::cgroup` has the whole mechanism, including the command
construction and the `memory.peak` parsing, unit-tested on every platform even though the wrap
itself only ever runs on Linux.

The headless runner wraps every scan the same way while `EXCISE_HARNESS_CGROUP=1` is set: unlike a
scenario, a fixture has no per-run opt-in field, so the environment variable alone decides it.

`cgroup_memory_peak_bytes` is reported, not gated: cgroup v2 enforces `MemoryMax` by construction,
so a scan's own `memory.peak` can never exceed the limit it was capped at, and a step that checked
it against that same budget could never fail. What the cap actually proves, which `peak_rss_bytes`
cannot from outside the process, is that the kernel kills a scan that needs more anonymous memory
than the limit allows — a scan that completes and exits normally under the cap already
demonstrates that it did not. The figure is still worth reading: on a large, many-file tree it can
sit at the cap even while `peak_rss_bytes` stays far below it, because cgroup v2 accounting counts
reclaimable page cache (file and directory contents the scan reads) toward the cgroup's usage, and
the kernel is free to fill headroom with that cache before reclaiming it. A reading at the cap is
therefore not by itself a sign that `excise` is close to its own limit; `peak_rss_bytes` already
checks that directly.

**Steps.**

- **`settle`** waits for a `frame` event that reflects every input event the runner has sent, as
  defined under [Runner semantics](#runner-semantics). It then reads the terminal output still in
  flight, so that the screen model has caught up with the frame and the step after it can read the
  screen once. On Unix it reads until the output has been quiet for 3 ms, for at most 20 ms. On
  Windows it first reads for 100 ms and then does the same, because the event says that the
  program drew, not that the drawing has arrived: `ConPTY` keeps its own copy of the screen and
  sends what changed in paints that are typically 16 ms apart, so a frame drawn soon after a paint
  reaches the harness late. In the recordings of failed runs on GitHub-hosted Windows runners, 30
  isolated frames arrived 4 to 22 ms after their event (median 12.5 ms), and the gaps between the
  paints of a console that was being redrawn were 15.6 ms at the median, 23 ms at the 99th
  percentile, and 54 ms at most, while the program was starting. The delays are upper bounds,
  because the program's clock and the recording's differ by an offset that only the moments the
  keys were sent bound. A key that changes nothing draws no frame and never settles.
- **`select`** opens the filter with `/`, erases any text it opened with, types the name, checks
  the prompt, presses Enter, and waits until the inspector pane shows exactly that name.
- **`delete`** presses Backspace and reads the dialog. It presses `y` only when the dialog names
  exactly the requested entry, kind, and path and every sentinel exists. Any mismatch fails the
  step and no `y` is ever sent. `wait_for = "finished"` (the default) ends the step when the
  `deletion_finished` event and the first frame after it have been read and the output in flight
  has been read as after `settle`, so the screen shows the result. `wait_for = "started"` ends it
  as soon as a frame shows the dialog has closed, without waiting for the deletion itself: the
  deletion keeps running after the step returns, so a step
  that needs its outcome waits for that separately (`wait_fs_absent`, `wait_event`). The program
  rebuilds its map after a deletion and treats a quit during the rebuild as a cancellation (exit
  code 130): a scenario that goes on to quit after a `"finished"` delete waits for the header to
  read `COMPLETE` first; one that quits after a `"started"` delete needs to reach the same point
  itself (`wait_fs_absent`, then `wait_event { event = "deletion_finished" }`, then a frame after
  it), since the step returned before any of that happened.
- **`quit`** presses `q`, waits for the quit dialog, and confirms with `y`.
- **`resize`** resizes the terminal and waits for the frame that answers it, then reads the output
  in flight as `settle` does.
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
- **`idle`** sends nothing, waits `after_ms`, then samples the terminal output byte count and the
  child's live CPU time, waits `window_ms`, and samples both again. The difference is recorded as
  `idle_output_bytes` and `idle_cpu_ms`. The live CPU sample (`metrics::live_cpu_ms`) is a point-in-
  time read of the still-running child, not `RUSAGE_CHILDREN`, which only answers after a child is
  reaped and so cannot isolate a window; it works on macOS and Linux and is not sampled on Windows,
  so `idle_cpu_ms` is simply absent there.
- A step's `timeout_ms` bounds the whole step, not each wait inside it.

**Measurements.** Each run reports finite, named metrics. The scenario `expect_budget` step and the
run summary use the same names.

| Metric | Meaning |
|---|---|
| `first_frame_ms`, `scan_complete_ms` | Spawn to the first `frame` event, and to `scan_complete`. `run_e2e` launches the binary once with `--version` before its first run, in an isolated environment with a 10 s limit, so the first measured session does not include the one-time code-signature assessment that macOS applies to a new binary (over 300 ms on its first launch against 5 to 8 ms afterwards). A warm-up that fails or times out stops the matrix with an error. |
| `input_to_frame_p50_ms`, `input_to_frame_p99_ms`, `input_to_frame_max_ms`, `input_samples` | For each key written while the previous one had been answered: the write to the first frame that reflects it, as defined under [Runner semantics](#runner-semantics): its `inputs` counter, less the inputs the program counted of its own before the first key, covers the key. |
| `max_stall_ms` | The longest gap between two frames inside a window in which the program was active (scanning, or working on a deletion or an input). |
| `quit_ms`, `delete_ms` | The confirmation key to the exit of the process, and to `deletion_finished`. |
| `output_bytes`, `output_bytes_per_s`, `frames`, `inputs_sent` | Terminal output and its rate, frames drawn, and input events sent. |
| `peak_rss_bytes`, `user_ms`, `sys_ms` | Peak memory (the peak physical footprint on macOS, the peak resident set size on Linux) and the child's CPU time. |
| `cgroup_memory_peak_bytes` | The Linux cgroup v2 `memory.peak` of the scope a `cgroup_memory_cap` scenario ran in. See [Linux cgroup memory cap](#linux-cgroup-memory-cap). |
| `idle_output_bytes`, `idle_cpu_ms` | Terminal output bytes and the child's live CPU time over an `idle` step's window; `idle_cpu_ms` is absent on Windows. |
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
cargo xtask e2e [--quick|--full|--nightly] [--scenario NAME]... [--profile PROFILE]... [--repeat N] [--latency-scale FACTOR] [--timing-informational] [--keep-fixture]
```

The command builds the `excise` release binary, or uses the one named by `EXCISE_E2E_BINARY`, loads
the scenarios in [`scenarios/`](scenarios), runs each selected one under its profiles, and prints a
verdict table. It exits non-zero on any `fail`, `xpass`, or `error`, and on a quick tier that takes
longer than its budget on the reference machine (see [Quick-tier time](#quick-tier-time)).

- `--quick` runs scenarios tagged `tier = "quick"` (the default) under the `default` and
  `deterministic` profiles only, and must stay within two minutes, which a run of the whole tier
  checks (see [Quick-tier time](#quick-tier-time)). `--full`, the default tier, adds `full` and
  every profile a scenario declares. `--nightly` adds `nightly` too.
- `--scenario` names a scenario and runs it whatever its tier, though a scenario outside its
  `platforms` is still skipped, with the reason, even when it is named. A scenario outside the
  selected tier or its `platforms` is otherwise skipped, with the reason, and printed.
- `--profile` narrows the matrix and may repeat, alongside `--scenario`. `--repeat N` runs each
  pair `N` times, which is how identical verdicts are shown.
- `--latency-scale FACTOR` multiplies the latency budgets' limits (see [Budgets](#budgets)); `1`, the
  default, keeps them strict.
- `--timing-informational` reports a missed latency budget as a warning instead of failing the run
  (see [Budgets](#budgets)); a scenario that is expected to fail keeps its strict verdict. It
  composes with `--latency-scale`: a warning is a miss of the scaled limit.
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

### Quick-tier time

Agents and developers run the quick tier on every iteration, and it grows one scenario at a time,
so a run of the whole tier times itself: `cargo xtask e2e --quick` with no `--scenario`, no
`--profile`, and no `--repeat` (a narrower or longer run is not the tier, and has no time to hold
to a budget). The clock starts just before the first launch of the binary under test, the warm-up,
and stops when the last run has ended. The verdict table's last line prints the time against the
budget of two minutes (`quick tier: 22.6 s of 120 s`), and the summary records it in milliseconds
as `quick_tier_ms`, a field that every other run leaves out. The runs' own times in the table add up
to less than the tier's: each run also generates and removes its own copy of its fixture, and the
warm-up comes first.

The budget is the constant `QUICK_BUDGET` in `src/runner/e2e.rs`, beside the tier's other
constants. A tier over it:

- **fails the run on the reference machine**, the one whose speed the budget was set on. It says so
  by setting `EXCISE_HARNESS_REFERENCE=1` (exactly `1`, the convention of `EXCISE_HARNESS_CGROUP`
  and `EXCISE_HARNESS_PRIVILEGED`); CI runners do not set it. The failure names the budget and the
  five slowest runs, each with the time the table shows for it, and the table prints it as a
  `FAIL` line.
- **warns everywhere else**: the table prints the same message as a `WARN` line and the run
  passes, because a hosted runner or a loaded machine is slower than the one the budget was set on.

A run that already failed keeps its failure and still reports its time. `--timing-informational`
does not excuse the budget: the machine decides, not the flag. When a change makes the tier slow,
the message names the runs to trim: give a heavy scenario `tier = "full"`, or make it cheaper.

## Comparison files

```console
cargo xtask compare [--quick|--full|--nightly] [--comparison NAME]... [--pairs N] [--seed S] [--timing-informational]
```

`motion_complete_ratio` and `tui_complete_ratio` are ratios between two runs, so one scenario's
`expect_budget` cannot check them: a single run has nothing to divide. A comparison file names the
two sides, and `excise_harness::comparison` drives them paired and interleaved, reusing the same
primitives `cargo xtask bench-e2e` uses to run one side
([`crate::bench::cases::run_scenario_once`]/[`run_fixture_once`]) and to summarize a ratio
([`crate::bench::pairing`], [`crate::bench::bootstrap`]). One file, `comparisons/<name>.toml`, is
one comparison.

```toml
schema_version = 1
name = "motion-complete-node-modules-2k"
description = "Default-motion time-to-COMPLETE against reduced-motion time-to-COMPLETE."
fixture = "node-modules-2k"
budget = "motion_complete_ratio"
pairs = 5
timeout_ms = 60000
tier = "full"
platforms = ["macos"]
```

### Fields

| Field | Required | Meaning |
|---|---|---|
| `schema_version` | yes | The format version. Only `1` is accepted. |
| `name` | yes | The comparison identifier, identifier-shaped like a scenario's `name`. It must match the file's name. |
| `description` | yes | What the comparison demonstrates. |
| `fixture` | yes | The identifier of the fixture specification both sides run against. |
| `budget` | yes | `motion_complete_ratio` or `tui_complete_ratio`: the ratio this comparison checks. No other budget is a ratio between two runs. |
| `profile` | only for `tui_complete_ratio` | Which interactive profile is the candidate, compared against a headless scan of the same fixture. Omitted for `motion_complete_ratio`, whose two sides are fixed by the budget's own definition: `default` (candidate) against `reduced-motion` (baseline). |
| `pairs` | no | How many interleaved baseline/candidate pairs to run. Defaults to 5. |
| `timeout_ms` | yes | The bound each side's run gets to reach `COMPLETE` (or finish its scan), between 1 and the scenario format's `MAX_TIMEOUT_MS`. Choose it with headroom over the slower side's healthy time, not just the faster side's: the budgets need headroom beyond measurement noise, not a bound that only a perfectly quiet machine clears. |
| `tier` | no | `"quick"` (default), `"full"`, or `"nightly"`: which `cargo xtask compare` tier runs it, exactly as a scenario's `tier` selects `cargo xtask e2e`. |
| `platforms` | no | The operating systems the comparison runs on. Defaults to every platform the harness knows. |
| `expect` | no | `"pass"` (default) or `"fail"`, with the same strict-xfail semantics as a scenario (see [Expected failures](#expected-failures)). |
| `fails_on` | no | The platforms `expect = "fail"` applies to; defaults to every platform in `platforms`. |
| `slice` | if `expect = "fail"` | The id of the work slice that fixes the defect. |
| `limit` | no | Overrides the budget's default limit (1.25 for both ratio budgets). |
| `drain_bytes_per_sec` | no | Caps how fast each side's interactive run drains its output, in bytes per second, so the comparison can simulate a slow terminal (see [Terminal throughput](#pty-runner)). Defaults to unthrottled. A headless baseline has no pseudo-terminal and ignores it. |

### The two sides

The **candidate** is always an interactive run: the comparison builds a minimal scenario against
`fixture` whose only step is `wait_event { event = "scan_complete" }` bounded by `timeout_ms` (the
event channel, not the screen, so a drain cap cannot make the wait lag the measurement; see
[Terminal throughput](#pty-runner)), and runs it through the PTY runner under the candidate profile
(`Comparison::candidate_profile`). The **baseline** is either another profile of the same minimal
scenario (`motion_complete_ratio`: `reduced-motion`) or a headless scan of the same fixture
(`tui_complete_ratio`: no profile runs the interface at all) (`Comparison::baseline_profile`). The
candidate's metric is `scan_complete_ms` (spawn to the `scan_complete` event); the baseline's is
`scan_complete_ms` for another profile, or a headless scan's `wall_time_ms` for
`tui_complete_ratio`.

### The timeout-as-failure rule

A run that never reaches `COMPLETE` (or never finishes its scan) within `timeout_ms` is F2's own
symptom under default motion: scan ingestion starving so badly that the interface never signals
completion. Counting it as a harness error would be wrong twice over — it would abort the whole
comparison instead of judging it, and a scenario author could never write down "this must not
happen" as a budget. Instead, a run that does not complete counts as a **failed ratio**: its
milliseconds are capped at the bound for the median and the confidence interval the report shows,
and the comparison's outcome is `Failed` regardless of what that capped number comes out to,
because non-completion is the defect on its own, not merely evidence of a large ratio.

### Pairs and statistics

`pairs` (5 by default) interleaved trials run baseline, candidate, baseline, candidate, and so on
([`crate::bench::pairing::interleaving`]), exactly as `cargo xtask bench-e2e` interleaves its two
builds. For every pair the report holds the candidate/baseline ratio, their median
([`crate::bench::bootstrap::median_ratio`]), and a deterministic bootstrap 95% confidence interval
of the median ([`crate::bench::bootstrap::bootstrap_ci`], seeded from `--seed`, `0` by default).

### Verdicts

The outcome is `Passed` when every run completed and the median ratio is at most the limit (its own
`limit` override, or the budget's default of 1.25); otherwise `Failed`, whether because a run never
completed or because the ratio itself is too high. Strict xfail then applies exactly as it does for
a scenario (see [Expected failures](#expected-failures)): `expect = "fail"` with a failing outcome
is `xfail`; a passing outcome there is `xpass`, which fails the run.

With `--timing-informational` (see [Budgets](#budgets)), a median ratio over the limit is a
warning, not a failure: the comparison passes, the table lists it (`WARN`) and counts it on its
last line, and nothing is written to disk, because a comparison writes no summary. A run that
never completed within its bound is a timeout, not a ratio, and still fails, and a comparison that
is expected to fail keeps its strict verdict.

### Running comparisons

`cargo xtask compare` loads every file in `comparisons/`, selects by tier and platform exactly as
`cargo xtask e2e` selects scenarios (a comparison named with `--comparison` runs whatever its tier;
one outside its `platforms` is always skipped, with the reason), runs each selected comparison, and
prints a table of its verdict, pairs, how many runs completed on each side, the median ratio, its
confidence interval, and the limit. It exits non-zero on any `fail` or `xpass`.

## Headless runner

```console
cargo xtask headless [--quick|--full] [--fixture ID]... [--class scale|identity|hostile|volumes]... [--profile default|deterministic] [--repeat N] [--timeout SECONDS] [--timing-informational] [--keep-scratch]
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
the CPU time, and, where the platform says, the peak memory. While `EXCISE_HARNESS_CGROUP=1` is
set and this host can do it, every scan also runs under the Linux cgroup memory cap; see
[Linux cgroup memory cap](#linux-cgroup-memory-cap).

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
1,000,000- and 10,000,000-entry fixtures only ever run by name), and `--class` selects every fixture that generates
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
(BSD `du` stops where a path passes `PATH_MAX`). The ratio is checked against the 15× budget
(`RATIO_BUDGET`), gated only on a fixture whose oracle entry count (fixed by its spec and seed) is
at least `MIN_GATED_ENTRIES` (2,000): below it the ratio is reported but never gated. The gate
reads a count, not a measured time, so which fixtures are gated never depends on how loaded the
machine was; an earlier, `du`-time-based threshold let one fixture's ratio verdict flip between
runs under load. The budget was set from measurements on macOS and Linux once the per-run
durable writes were gone and the report writer was buffered.

**Informational timing.** With `--timing-informational`, a gated ratio over the 15x budget that no
`[[expect_ratio_fail]]` entry documents is a warning when it was measured against a valid `du`
reference (every measured `du` exited with code 0, and none printed a total the oracle
contradicts): the fixture passes on the ratio, the table lists it (`WARN`) and counts it on its
last line, its result in `summary.json` carries a `timing_warnings` entry (`headless_scan_ratio`,
the median, and the budget), and the summary says `timing_informational`. A ratio measured against
a `du` that failed, was killed, or walked only part of the tree keeps its strict verdict. The
oracle diff, the memory budget, and a scan that does not end in time still fail the fixture as
before, and a ratio that an entry documents keeps its strict verdict.

**Expected failures.** `expectations/headless.toml` has three independent tables.
`[[expect_fail]]` lists the fixtures that fail the oracle diff for a known defect that is not yet
fixed, with the semantics of `expect = "fail"`: a fixture that fails with exactly the listed kinds
is `xfail`, one that fails with other kinds is `fail`, and one that no longer fails is `xpass` and
fails the run, so that the entry is removed by the change that fixes the defect.
`[[expect_ratio_fail]]` is the same semantics for the scan-time ratio, applied only where the
fixture is gated (above): over budget and listed is `xfail`; over budget and not listed is `fail`,
exactly like an undocumented diff discrepancy, so a platform the entry does not name must stay
within budget; within budget while listed is `xpass`. All three tables name the platforms an entry
applies to and the findings it documents; `[[expect_fail]]` also names the exact discrepancy
kinds. No `[[expect_ratio_fail]]` entry is needed today: every gated fixture is within the budget
on macOS and Linux, and Windows has no `du` reference (above), so no ratio is ever measured there.
A fixture gated on a platform with a `du` reference that nobody has measured yet needs its own
entry once someone does, or the run fails there until then.

**Peak-memory gate.** Every fixture's run is also held to the memory contract (`peak_rss_bytes` of
any round at most 512 MiB, `MEMORY_BUDGET_BYTES`), the same way and in the same file, under
`[[expect_memory_fail]]`: `fixture`, `platforms`, `findings`, and `reason`, with the same strict
semantics as `[[expect_ratio_fail]]` above, minus `kinds` (there is only the one check). The check's
own verdict (kept as `memory_verdict`, for the verdict table's block below) is folded into the
fixture's overall `verdict` with `combine_verdicts` the moment it is measured, exactly like the
ratio budget: a fixture can fail it while its oracle diff is clean, and the other way around;
either one fails the run. Nothing is checked on a platform this crate has no safe way to sample
memory on (Windows).

**Output.** `target/excise-headless/<run-id>/summary.json` is a `harness-summary` with one
`headless-<fixture>` result per fixture, and `target/excise-headless/latest` points at the newest
run. The open `metrics` object of a result has `entries`, `runs`, `oracle_ms`, `generation_ms`,
`headless_ms` (the median, with `_min` and `_max`), `du_ms`, `headless_scan_ratio` (the median,
with `_min`, `_q1`, `_q3`, and `_max`), `headless_scan_ratio_gated` (1 when the ratio was gated
against the budget, 0 when it was only reported), `du_kib`, `du_expected_kib`, `user_ms`, `sys_ms`,
`peak_rss_bytes`, `cgroup_memory_peak_bytes` (while the cgroup cap measured it),
`memory_budget_bytes` (the limit `peak_rss_bytes` was checked against), `exit_code`, and
`discrepancies` with one `discrepancies_<kind>` count per kind. A failing fixture
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

## Counts

```console
cargo xtask counts [--out FILE] [--repeat N] [--fixture ID]... [--timeout SECONDS] [--pull-request NUMBER --base-sha SHA --head-sha SHA]
cargo xtask counts-record --record FILE --remote URL [--branch NAME] [--attempts N]
cargo xtask counts-comment --artifact FILE --expect-head-sha SHA --out DIR [--history DIR] [--repo DIR]
```

`excise_harness::counts` counts what a build of `excise` costs without timing anything: numbers
that, for a given binary and fixture, do not depend on timing or load. Timing is only evidence
from the paired [A/B benchmark](#paired-ab-benchmark); these are the costs two commits compare on
exactly, so a difference is always real.

**What is counted.** `wide-1k`, `node-modules-2k`, `identity-small`, and `tiny-files-50k`, in that
order (`--fixture` names others, at most 16), each under the `deterministic` profile, so that no
count depends on how many processors the machine has. Each is scanned headless and then, on Linux
and macOS, run in a pseudo-terminal until its scan completes, which checks that scan and counts
what it leaves behind.

| Count | Taken from | Why it does not depend on timing |
|---|---|---|
| `entries` | The headless report's `summary.scanned_entries`. | A function of the tree and the accounting; it moves only when one of them does, so a change is flagged as unexpected. |
| `scan_store_bytes` | The headless report's `summary.scan_store_bytes`: the quota's bytes in use once the scan is published. | The published store is a function of the scanned records and the program reports it, so no sampler races a writer. Identical under one scan thread and several, on a machine at a load average of 40, and at two fixture paths of different length. |
| `residue_files` | The scratch areas of both runs after a normal exit. | Zero unless the program leaks. |

**The interactive scan has to be the one that was counted.** `scan_complete` also follows a scan
that recoverable failures left inexact, and it carries only an entry count. So the run fails unless
the entries it reports are those of the headless scan of the same fixture (which was required to
end exact) and the header badge reads `COMPLETE` once the frame that shows how the scan ended has
been drawn. The whole session, quitting included, ends by `--timeout` counted from the start that
the session recorded before it launched the program, not by an allowance of its own, and an event
or an exit that the harness first sees after that moment is not accepted.

**Determinism is checked, not assumed.** Every fixture is counted `--repeat` times (twice by
default). The run fails, naming the fixture, the count, and every value it took, if any count
differs between the runs or one run lacks it. Left out on purpose: the peaks of descriptors,
threads, and scan-store bytes *during* a scan, which the scenario runner samples every 50 ms and
which gave 19, 22, and 24 descriptors, 8 or 10 threads, and two store peaks (1,556,638 and
1,410,393 bytes) for one scan on one machine; the size of the JSON report, because every entry
repeats the root path (965,680 and 1,008,860 bytes for one fixture at two paths 19 characters
apart); and wall time, CPU, latency, and memory peaks, which are for A/B.

**Left out: the descriptors and threads of the idle program.** An earlier version also took the
descriptors and threads (`idle_fds`, `idle_threads`) that the program holds once its scan is
complete. Nothing outside the program says that it has finished what follows its scan, and no
window of silence proves it: a worker that is blocked, or not scheduled, for longer than the
window yields a value that is too early, and two runs agree on it, which is the one thing the check
above cannot see. They can return when the program says so itself: an `idle` event on the test
event channel, written after the work that follows its scan is done, would be the barrier.

**The document** is `harness-counts` (see [Output documents](#output-documents)): the commit and
its commit time, the runner (`os`, `os_version`, `arch`), the toolchain, a pull request's number,
base commit, and head commit when it is one, and per case the fixture's id, manifest hash, and
seed, the profile, and an open map of counts. It holds no time of the run, so counting one commit
twice writes the same bytes. It is written by `cargo xtask counts` to
`target/excise-counts/counts.json` (`--out` names another file).

**History.** `counts-record` appends a record to the orphan `bench-data` branch:
`records/<os>/<first two hex digits of the commit>/<commit>.json`, one file per commit. A file per
commit, rather than a JSON Lines file per operating system, because a comment finds a base
commit's record by name without reading an ever-growing file, two writers never edit one file (a
push that loses a race is retried on the new tip, never merged), every record is exactly one
document held to the same schema as an artifact, and a record is never edited: a commit that has
one is left alone. The first commit of the branch adds a README and nothing after it changes
anything but by adding one record. A push that is rejected is retried, up to `--attempts` (five)
times in all.

**Comparison.** `counts-comment` compares a pull request's document with the record of its base
commit, or of the nearest ancestor on the first-parent chain that has one (the base commit itself
and up to 200 commits before it; the comment says which and how far). Records are per operating
system, and a comparison only ever uses one taken on the same system: the same fixture does not
count the same everywhere (`identity-small` counted 23 entries and 14,783 scan-store bytes on a
hosted Linux runner and 27 and 16,441 on macOS). A cost that moves by more than 5% of its base value
is flagged (the arithmetic is on whole numbers, so exactly 5% is not flagged); any change in a
fixture's `entries` is flagged as unexpected; a fixture whose hash differs is not compared. A
record that exists and cannot be used is passed over and named on stderr. The comment is at most
60,000 bytes (GitHub refuses 65,536): the rows and notes that do not fit are left out, and it says
how many.

**Untrusted input.** A pull request's counts are an artifact that its own workflow run uploaded,
and a fork controls that workflow. Before the artifact is downloaded, and as the last step before
it, `.github/scripts/check-count-artifact.sh` lists the run's artifacts and refuses anything but
exactly one, named `pr-counts`, of at most 64 KiB compressed (the download unpacks all of an
artifact, and its size is the author's choice). `counts::artifact::read_untrusted` is the only way
one is read: a regular file, never a link, of at most 64 KiB, that is UTF-8 and JSON and passes the
schema and the rules the schema cannot say (a fixture appears once per profile; no count exceeds
2^53 - 1). It is read by a step that has no token in its environment, and the history it compares
with is a working copy of `origin/bench-data` that the workflow's checkout already holds
(`--history`), so the one step that parses what the pull request made could not use one. Every
error it returns is one line of printable ASCII, so text from a document can neither forge a
workflow command nor fill a log. The comment shows text from a document only inside code spans (a
`<` becomes `?`, a backtick `'`, a pipe is escaped), so it cannot link, mention, embed, or add a
second marker; numbers and commits are formatted from checked values. The posting script
(`.github/scripts/post-count-comment.sh`) is given the pull request number and the base commit that
the document names, and checks them, with the repository, the head commit, the head repository,
and the head branch, again. It believes the number only if the API says that pull request is open,
still at the commit the triggering run was for, from the repository and branch that run was for,
and still based on the commit that the counts were compared with: an artifact cannot name a base
of its own choosing, and a run for a base that the pull request no longer has cannot comment. Where
the run's payload lists its pull requests, the number must be one of them; the
payload does so for a pull request in this repository and leaves the list empty for one from a
fork, which the head and base checks then tell apart from any other (GitHub allows no two open pull
requests with the same head and base). It edits only the bot's own comment that begins with the
marker.

**What a comment cannot vouch for.** The counts of a pull request are measured by the pull
request's own code, so a fork can write any numbers that pass the schema; the comment informs and
gates nothing, and a reviewer who wants the counts of a head runs `cargo xtask counts` on it. The
workflow that is followed is found by its name, so a workflow of the same name in a fork can upload
an artifact of the same name, which is held to the same checks as the real one.

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
loading validates the spec (sizes, counts, colliding roots, and at most 10,100,000 planned
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
| `node-modules-50k` | 49,738 | The F1/F2 repro shape at larger scale: `node_modules/pkg{0..7}` nested 4 levels, 11 one-byte `m{0..10}.js` files per leaf (4,681 directories, 45,056 files, counting the root and the marker). |
| `deep-past-path-max` | 122 | Scale: a 60-level chain of 90-byte names with one file per level; the deepest path is 5,464 bytes, past `PATH_MAX` on every platform. |
| `identity-small` | 34 | Hard links across directories, dangling and looping symlinks, a sparse file, and a clone. |
| `hostile-small` | 80 | Hostile names and unreadable entries. |
| `all-classes-small` | 253 | Every class once, in one fixture. |
| `delete-folder` | 5,014 | A 5,000-entry victim for deletion scenarios. |
| `delete-file` | 5 | A 48 KiB victim file, a sentinel beside it, and a folder below it with two more files that must survive. The in-process lifecycle scenario deletes the victim. |
| `navigate-folders` | 6 | Two folders and a file beside them, to drill into and back out of. |
| `mount-boundary` | 13 | An ordinary `outside/` tree and an empty mount point; a privileged run copy attaches a 16 MiB volume with 20 files. |
| `scan-store-quota` | 35,002 | A flat directory of 35,000 tiny files beside an empty mount point; a privileged run copy attaches an 8 MiB volume there for `scan_store_on_volume` (see [Volumes](#volumes)). |
| `selection-drift` | 20,052 | A 5,000,000-byte file beside a folder of 20,000 tiny files that totals 21,000,000 bytes of content, 81,920,000 bytes of disk allocation (Excise's default view): the file is the largest entry when first measured, the folder once it is fully scanned. |
| `tiny-files-50k` | 49,050 | 49 directories of 1,000 tiny files. |
| `tiny-files-250k` | 249,250 | 249 directories of 1,000 tiny files: the full tier's memory-contract fixture, scanned within the 512 MiB peak-memory budget. |
| `tiny-files-1m` | 1,010,101 | One million tiny files. For nightly and manual tiers only: tests never generate it. |
| `tiny-files-10m` | 10,010,101 | Ten million tiny files, 1,000 in each of 10,000 leaf directories. For the weekly tier only: tests never generate it. Its plan alone takes about 0.9 GB of memory and 19 seconds to expand (measured, debug build), and on disk it needs ten million inodes and about 10 GB of 1 KiB blocks, which no hosted runner's own file system has (the weekly workflow builds one). |

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

| Document | `document_kind` | `schema_version` | Written by | Schema | What it is |
|---|---|---|---|---|---|
| Summary | `harness-summary` | 1 | `cargo xtask e2e` and `cargo xtask headless` (one schema, both commands; see below) | [`harness-summary.schema.json`](schemas/harness-summary.schema.json) | The result of one run: run id, tier, times, host, the binary's path and SHA-256, the git SHA, the latency scale when it is not 1 (`latency_budget_scale`), whether the run held timing informational (`timing_informational`), how long the whole quick tier took when the run was that, in milliseconds (`quick_tier_ms`; see [Quick-tier time](#quick-tier-time)), and a verdict, duration, an open map of named metrics, and the timing budgets missed without failing (`timing_warnings`) for each scenario and profile (or, under `headless`, each fixture). The four fields are optional and left out when they have nothing to say (a strict run has no scale, no flag, and no warnings, and only a run of the whole quick tier has a time), so they are additive and the version stays 1. |
| Failure bundle | `harness-failure` | 1 | `cargo xtask e2e` | [`harness-failure.schema.json`](schemas/harness-failure.schema.json) | The evidence for one failed scenario: the failed step, expected and actual screen text, terminal modes, the session's diagnostics (present only when the step timed out; see [Runner semantics](#runner-semantics)), the recording path, resource use, the fixture hash and seed, and a command that reruns it. |
| A/B evidence | `harness-ab` | 1 | `cargo xtask bench-e2e` | [`harness-ab.schema.json`](schemas/harness-ab.schema.json) | Paired, interleaved comparison of two builds: identities, trials, run order, per-metric samples, median ratio, bootstrap confidence interval and verdict, and the conditions it ran under (the fixtures compared, the host, the toolchain, the power state, the load average, and concurrent `excise` processes). |
| Counts | `harness-counts` | 1 | `cargo xtask counts` (the history job's record is the same document) | [`harness-counts.schema.json`](schemas/harness-counts.schema.json) | The deterministic counts of one build: the commit and its time, the runner, the toolchain, a pull request's number, base, and head when it is one, and per fixture and profile the fixture's hash and seed and an open map of counts (see [Counts](#counts)). It holds nothing that depends on the run, so a commit counted twice gives the same bytes. |

`cargo xtask headless` writes a `harness-summary`, not a separate document kind: a headless run is
one more kind of scenario result, named `headless-<fixture>`, whose open `metrics` map carries the
oracle-diff and `du -sk` figures instead of pseudo-terminal ones (see
[headless's own **Output**](#headless-runner) for the names). `cargo xtask compare` writes no
document at all: it only prints the verdict table, because a ratio-budget comparison's evidence
(the paired samples and the ratio) is exactly a `harness-ab` row running against this crate's own
binary, and a future slice may fold it into one if that evidence needs to be kept.

A `metrics` map is deliberately open: the harness does not constrain which names appear, and the
schemas say so rather than enumerating them (a scenario's own `measure` names, a fixture's class,
or a future metric all pass through unchanged). Fixed-shape fields (identities, verdicts, the
profile and tier enums, the session diagnostics object) are fully enumerated and reject an unknown
member or an undeclared field.

Each schema's `$id` is
`https://github.com/findyourexit/excise/harness/schemas/<document_kind>-v1.json`. The Rust types
are `HarnessSummary`, `HarnessFailure`, `HarnessAb`, and `HarnessCounts` in `excise_harness::report`. They implement
`Document`, which carries the kind, the schema id, and the schema text, and renders the canonical
form: pretty-printed JSON in field order with a final newline.

The schemas live here, not in `docs/schemas`, because that directory is copied into release
archives and packages and these formats are not part of the product. The types and the schemas
reject unknown fields, so any change to a document's shape needs a new `schema_version`. The tests
keep the Rust types and the schemas in step: every schema compiles, its `$id`, `document_kind`, and
`schema_version` match the Rust constants, every writer's real output (not a hand-built sample)
validates, and every field a schema declares is serialized by the types. A negative test per schema
proves a plausibly wrong document (an unknown verdict, a missing required field) is rejected, so a
loosened schema fails the suite.

Three more documents carry the same `document_kind`/`schema_version` convention but are not part of
this family: the fixture ownership marker (`harness-fixture-marker`, `fixture::marker::Marker`,
written to every fixture root as `.excise-harness-owned`; see [Fixtures](#fixtures)), the fixture
manifest (`harness-fixture-manifest`, `fixture::plan::Manifest`), and the independent oracle
(`harness-fixture-oracle`, `fixture::oracle::Oracle`). All three are generator- and runner-internal
bookkeeping, read back only by this crate itself to validate a cached fixture or diff a scan
against the tree it actually found; the manifest is never written to disk at all, and the oracle is
walked and compared in memory, never persisted. None is evidence a run hands to an agent or to CI,
so none has a schema file here.

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
| `reduced-motion` | Reduced motion and loading animation off; thread count unchanged. |
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
- `idle` needs a separate process to measure output and CPU on.
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
