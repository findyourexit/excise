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

Runners (in-process, pseudo-terminal, headless), the fixture generator, and the `xtask` commands
build on this vocabulary.

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
| `profiles` | yes | A non-empty, duplicate-free list of profiles the scenario runs under. |
| `terminal` | no | The initial terminal size. Defaults to 120 columns by 40 rows; at least 32 by 8. |
| `expect` | no | `"pass"` (default) or `"fail"`. See [Expected failures](#expected-failures). |
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
| `delete` | `name`, `kind` (`file` or `folder`), `timeout_ms` | Deletes the selected entry through the confirmation dialog. |
| `wait_fs_absent` | `path`, `timeout_ms` | Waits until the fixture-relative path no longer exists. |
| `wait_fs_present` | `path`, `timeout_ms` | Waits until the fixture-relative path exists. |
| `fs_mutate` | `op` (`appear`, `change`, `vanish`, `replace`), `path` | Changes the fixture while the program runs. |
| `resize` | `cols`, `rows` | Resizes the terminal. |
| `signal` | `signal` (`term`, `hup`, `quit`, `int`, `close`, `break`) | Delivers a signal or console event. |
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
  fails the step and `y` is never sent.
- **`wait_event`.** `event` is one of `frame`, `scan_complete`, `deletion_finished`, `quit_prompt`,
  or `exit`: the kinds reported by the program's internal test event channel (which also opens with
  a `hello` line that the runner consumes itself). `fields` maps a numeric event field to a test:
  `{ eq = n }`, `{ min = n }`, or `{ max = n }` with exactly one key. The fields an event carries
  are `frame`: `seq`, `inputs`; `scan_complete`: `entries`; `deletion_finished`: `removed`,
  `failed`; and `exit`: `code`. Every event also carries `t_us`, the microseconds since the channel
  opened. Testing a field the event does not carry is a validation error.
- **`signal`.** `term`, `hup`, `quit`, and `int` are Unix signals; `close` and `break` are Windows
  console events. Whether the host can deliver one is a runner concern, and an undeliverable signal
  must never be reported as a pass.
- **`resize`.** Unlike the initial terminal, a resize may go below 32 by 8 to exercise the resize
  message. Both dimensions must be non-zero.
- **`expect_exit`.** `terminal_restored = true` asserts that the alternate screen was left, the
  cursor is visible, and echo and canonical mode are on; `false` asserts that they are not.
  `residue = "none"` asserts that nothing is left in the scenario scratch directory. `code` is the
  process exit code.
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

## Output documents

Machine output is versioned JSON. Every document carries a `document_kind` and a `schema_version`
(`1`), like the published Excise scan report, and has a draft 2020-12 schema with
`additionalProperties: false`.

| Document | `document_kind` | Schema | What it is |
|---|---|---|---|
| Summary | `harness-summary` | [`harness-summary.schema.json`](schemas/harness-summary.schema.json) | The result of one run: run id, tier, times, host, the binary's path and SHA-256, the git SHA, and a verdict, duration, and metrics for each scenario and profile. |
| Failure bundle | `harness-failure` | [`harness-failure.schema.json`](schemas/harness-failure.schema.json) | The evidence for one failed scenario: the failed step, expected and actual screen text, terminal modes, the recording path, resource use, the fixture hash and seed, and a command that reruns it. |
| A/B evidence | `harness-ab` | [`harness-ab.schema.json`](schemas/harness-ab.schema.json) | Paired, interleaved comparison of two builds: identities, fixture hash, trials, run order, per-metric samples, median ratio, bootstrap confidence interval and verdict, and the conditions it ran under. |

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
document records the host, CPU, toolchain, power state, and concurrent `excise` processes so a
comparison can be judged.

## Working on the crate

```console
cargo test -p excise-harness --locked
cargo clippy -p excise-harness --all-targets --locked -- -D warnings
```

The crate follows the workspace lints: no `unsafe`, pedantic Clippy, and no `unwrap`. Development
guidance for the harness as a whole lives in
[`docs/development.md`](../../docs/development.md#validation-harness).
