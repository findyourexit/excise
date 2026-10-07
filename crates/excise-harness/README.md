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
- [`shape`](src/shape): the shape of a tree as aggregates only, and a fixture specification built
  from it: the library behind the `excise-shape` binary (see [Shape profiles](#shape-profiles)).

Runners (in-process, pseudo-terminal, headless) and the `xtask` commands build on this vocabulary
and on the [fixture generator](#fixtures), which creates and checks the trees they run against.

The [interactive driver](#interactive-driver) (`tui`) drives one live session at a time, for
exploring the interface before a scenario is written.

The [version sweep](#version-sweep) (`sweep`) runs every published version through the checks a
published release can take, and tabulates which of the defects fixed in the release under
development each one shows.

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
documented ones (terminal size, `expect`, `timeout_ms`, `ctrl`, `alt`, `sentinels`,
`disable_delete_confirmation`, `confirm_with`, and `budgets`). Names are `snake_case`, except
profile names, which are kebab-case.

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
| `disable_delete_confirmation` | no | Starts `excise` in its session-only reduced-confirmation mode, as `--disable-delete-confirmation` does: Backspace starts a deletion at once, with no dialog, and the header shows `! REDUCED DELETE GUARD`. A typed field and not an argument list, because an argument could point the program at a root the harness does not own: the runners add this one flag in front of the fixture root they always pass (the in-process runner sets the same setting). Defaults to `false`. It changes what the `delete` step does: see [Steps](#steps). |
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

The pseudo-terminal runner also skips a scenario that has a `delete` step, in either mode, and one
that presses Backspace itself with a `key` or `type` step, or composes an escape sequence from its
keys (a key that asks for a deletion dialog that no `delete` step verifies; see
[`key`](#steps)), wherever the terminal's screen cannot be tied to a frame exactly: on Windows,
where the capability `SCREEN_IS_EXACT` is false (see [`settle`](#pty-runner)). It prints the reason
(`SKIP name: ...`), also when the scenario is named with `--scenario`, and the scenario's
`platforms` stay as they are. The in-process runner is not affected: `cargo test` runs the
scenario there, on Windows too.

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
`wait_header`, `wait_event`, `select`, `delete`, `wait_refresh`, `wait_fs_absent`,
`wait_fs_present`, `expect_exit`, `settle`, and `quit`. The other `expect_*` steps evaluate once
against the current state and never wait; put a `wait_*` or `settle` step before them.

| Step | Fields | What it does |
|---|---|---|
| `wait_text` | `text` or `regex` (exactly one), `region`, `timeout_ms` | Waits until the text or regular expression appears on the screen, or in the region. |
| `wait_header` | `state` (`scanning` or `complete`), `timeout_ms` | Waits until the header band reports the scan state. |
| `wait_event` | `event`, `fields`, `timeout_ms` | Waits until the event channel reports the event and every field test holds. |
| `key` | `key`, `ctrl`, `alt` | Presses one key. |
| `type` | `text` | Types literal text, one character at a time. |
| `select` | `name`, `timeout_ms` | Selects the entry by name through the filter. |
| `delete` | `name`, `kind` (`file` or `folder`), `path`, `confirm_with` (`y` or `enter`), `wait_for` (`finished` or `started`), `timeout_ms` | Deletes the selected entry through the confirmation dialog, or with Backspace alone when the scenario disables the confirmation. |
| `wait_refresh` | `timeout_ms` | Waits until the map on screen has caught up with the deletions confirmed so far. |
| `wait_fs_absent` | `path`, `timeout_ms` | Waits until the fixture-relative path no longer exists. |
| `wait_fs_present` | `path`, `timeout_ms` | Waits until the fixture-relative path exists. |
| `fs_mutate` | `op` (`appear`, `change`, `vanish`, `replace`), `path` | Changes the fixture while the program runs. |
| `resize` | `cols`, `rows` | Resizes the terminal. |
| `signal` | `signal` (`term`, `hup`, `quit`, `int`, `close`, `break`) | Delivers a signal or console event. `close` needs Windows; `break` is never deliverable (see below). |
| `expect_screen` | `contains`, `not_contains`, `regex`, `region` | Asserts what the screen shows now. |
| `expect_fs` | `present`, `absent` | Asserts which fixture-relative paths exist now. |
| `expect_config` | `key`, `equals` | Asserts a string setting in the configuration file the program saved. |
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
  lowercase. `ctrl` and `alt` default to `false`. A key sent right behind a lone `esc` can be read
  with it as Alt plus that key, so put a `settle` after `esc` before any other key (`quit` sends
  `q`). `ctrl` with `]` is not a key: its byte, `0x1d`, is the input barrier request that the runner
  writes itself (see [`settle`](#pty-runner)), so a step that names it is refused before the run,
  as a combination with no encoding is.

  **A key that asks for a deletion.** Backspace opens a deletion dialog, and a `key` step is not
  the protocol of `delete`: nothing verifies the dialog it opens, and a later `y`, Enter, filter
  text, or the `y` of `quit` would confirm whatever a screen that may lag the program shows. The
  runner reads every byte it writes to the program, in order (a step's own, the protocols', and
  the barrier requests: `pty::input::InputScan`), and a `key` or `type` step whose bytes the
  program can read as Backspace (`backspace`, `alt+backspace`, `ctrl+h`, which the Windows console
  path may read as one) therefore *engages the run*, to its end. Until one does, nothing changes:
  every key is sent at once, and `quit` presses `y` on the frame event that counts its `q`. In an
  engaged run every write that can be read as a confirmation (it holds `y`, `Y`, Enter, or a line
  feed: a `key` or `type` step, the name and the Enter of `select`, the `y` of `quit`) is sent only
  after the runner has written an input barrier behind everything before it and the screen shows
  the frame that answers it, and only if that exact screen shows no deletion dialog and what the
  step needs: the open filter prompt for `select`, and the plain quit prompt (`[y] Quit`, no
  waiting check listed) for `quit`. Otherwise the step fails with `DeleteRefused` (a mismatch, when
  it is the prompt that is not there) and the key is not sent. `alt+y` and its like, the escape
  byte and a confirmation in one write, are refused outright: a program may read them as two
  inputs, and no barrier can come between them.

  **Keys that compose a key.** The program joins the bytes of an escape sequence across writes:
  `alt+[` and the text `121u` after it are `ESC [ 121 u`, which it reads as `y` (`13u` and
  `57414u` are Enter, `97:121;2u` is `y` again, and the Windows console reads sequences of its own
  that name any key), and no write holds a `y`, Enter, line feed, or Backspace byte. The scan
  decodes none of this. A sequence that one write begins (`alt+[`, `alt+O`, or an `esc` that the
  next write's `[` or `O` turns into one) and a later write continues counts as both a request for
  a deletion and a confirmation, whatever its bytes, and so does every write that continues it
  until it finishes: the write that continues it engages the run. Behind `ESC [` or `ESC O` it is
  refused, as no barrier can be written there (the program takes the barrier for a byte of the
  sequence and never answers it). Behind a lone `esc` it goes out as any write that could confirm
  does, behind a barrier, which keeps the two apart. A key that begins and finishes a sequence of
  its own (an arrow, a page key, `alt+up`) is neither. Send each key as one `key` step. A program
  that does not mark its frames or answer a barrier, and a terminal whose screen is not exact, are
  refused as `delete` refuses them, and the pseudo-terminal runner skips a scenario that asks for
  a deletion with a key, or that composes a sequence between its keys, where the screen is not
  exact (see [Tiers and platforms](#tiers-and-platforms)). No bundled scenario composes a
  sequence.
- **`select`.** The runner opens the filter with `/`, types the name, presses Enter, then asserts
  that the selected item is exactly that entry. In an engaged run (see `key`) the name and the
  Enter are sent only behind a barrier, to an exact screen that shows the open filter prompt and no
  deletion dialog; a character that would continue an escape sequence that a `key` step began and
  nothing has finished is refused.
- **`delete`.** The runner acts on the selected entry. It presses Backspace, parses the
  confirmation dialog, asserts that its title and path name that entry, `name` and `kind`, under the
  fixture root, asserts that every sentinel still exists, checks the fixture root's ownership marker
  once more, and only then presses `y`, or Enter with `confirm_with = "enter"` (the dialog offers
  both: `[Enter/y] start`). Any mismatch fails the step and the confirmation key is never sent. The
  dialog is read after an input barrier that the runner writes behind the Backspace: `excise`
  answers it with a frame once it has read the Backspace and every key before it, and the screen,
  once it shows that frame (the frame marks say when, where the screen is exact: a Unix
  pseudo-terminal, `SCREEN_IS_EXACT`), shows what those keys did, however the terminal cut their
  bytes into input events (see [`settle`](#pty-runner)). A program whose `hello` does not say that
  it marks its frames and answers the barrier is refused before any key is sent. The ownership
  marker `.excise-harness-owned` is never a target: validation rejects a `delete` step whose `path`
  (or `name`, when there is no `path`) is the marker or lies inside it. `path` says where the entry
  is: relative to the fixture root, `/`-separated, and ending in `name` (it defaults to `name`
  itself, an entry directly below the root); with a dialog, the path the dialog shows must be
  exactly the fixture root and `path`.
  `wait_for` (default `"finished"`) controls when the step returns: `"finished"` waits for the
  deletion to finish; `"started"` returns as soon as the confirmation has closed the dialog, while
  the deletion keeps running, so a later step can act while it is still in progress (for example,
  delivering a `signal`).

  **Where the screen is not exact** (`SCREEN_IS_EXACT` is false: Windows, see
  [`settle`](#pty-runner)) the step refuses, in both modes, before any key, not even Backspace. It
  fails with `DeleteRefused` and says that the terminal repaints the screen on its own timer (the
  console host of Windows), so the harness cannot tie the screen to a frame and confirms no deletion
  from it, and that the scenario runs in-process under `cargo test`. The reads that follow the
  Backspace stay as a second line of defence: nothing can satisfy them there. `cargo xtask e2e`
  skips every scenario that has a `delete` step there (see [Running scenarios](#running-scenarios)).

  **Without a dialog** (`disable_delete_confirmation = true`) Backspace alone starts the deletion,
  so the step has no dialog to read and no key to hold back. It checks what it can before it presses
  Backspace. It asks the program for an input barrier, so that the panel it reads shows what every
  key sent so far did, and then waits, within its `timeout_ms`, for the selected-item panel to show
  an entry of this `name` and `kind` (the pseudo-terminal runner reads a panel that can trail the
  header: a fresh map arms its cursor in the frame that ends the scan, and a terminal may deliver a
  frame in pieces);
  a panel that never does fails the step and says what it showed. Then the entry that `path` names
  must exist on disk with that kind, every sentinel must exist, and no sentinel may be that entry or
  lie inside it. The panel never shows where an entry lives, so an entry below a folder needs
  `path`, and `name` and `kind` must belong to one entry in the whole fixture: a panel that shows a
  file called `twin.bin` could be either of two, and Backspace deletes whichever is selected, so the
  step refuses and says where the twins are (give each entry a name of its own, or keep the
  confirmation on, whose dialog shows the path). The step then checks the fixture's ownership marker
  once more, presses Backspace, asks for a barrier behind it, and fails if any dialog opened: a
  confirmation dialog means the program did not honour the mode, and no key is sent
  to confirm it. After that it waits as `"finished"` does. `wait_for = "started"` and
  `confirm_with = "enter"` have nothing to act on there and are validation errors. A terminal too
  narrow to draw the selected-item panel cannot run this step.
  At about 60 columns the status row cuts a label that precedes the filter prompt (the reduced-guard
  label, or `! ELEVATED` on an elevated Windows session such as the hosted runner), so `select`
  cannot read the prompt there; a scenario at that width leaves selected the entry that the fresh
  map selects (the largest) instead of choosing one.
- **`wait_refresh`.** A deletion that removed entries leaves the map listing them until the program
  replaces it, either with a rebuild of the whole map (after a file that may have had another
  link: one whose last link the program did not see go) or with a map that leaves them out. A
  quit while a rebuild is running cancels it and exits 130, not 0; one before a rebuild that the
  map is owed has started exits 2, and one before an overlay lands exits with the map as it
  stands. Nothing on the screen says when
  it is over: the header reads `COMPLETE` from the map as it was. The step waits until the program
  says so, and then for a frame that shows it (see [PTY runner](#pty-runner) for how, and
  [In-process runner](#in-process-runner) for the barrier that stands in for it there). A deletion
  that removed nothing, a refusal, owes no refresh, so the step returns at once after it. Put it
  after every `delete` that anything which ends the run (`quit`, `expect_exit`, a measurement of
  the quit) follows; it needs a `delete` step before it.
- **`expect_config`.** `key` is a dotted path into the configuration file the program saved: every
  name but the last is a table, and the last is a setting whose value is a string, which `equals`
  must match exactly (`key = "runtime.theme"`, `equals = "excise-light"`). Names are lowercase ASCII
  letters, digits, `_`, and `-`. The step reads the file as it is now and never waits, so put a
  `settle` before it. A process run reads the scratch file `EXCISE_CONFIG` names; the in-process
  runner gives the program a file of its own. A file that is not TOML, a missing table or setting,
  or a value that is not a string fails the step and says which.
- **`wait_event`.** `event` is one of `frame`, `scan_complete`, `deletion_finished`, `quit_prompt`,
  or `exit`: the kinds reported by the program's internal test event channel (which also opens with
  a `hello` line and reports `refresh_finished`; the runner consumes both itself, the second for
  `wait_refresh`). `fields` maps a numeric event field to a test:
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
- **`quit`.** The runner presses `q`, waits for the quit prompt, and confirms. In an engaged run
  (see `key`) it confirms only behind a barrier, on an exact screen that shows the plain quit
  prompt and no deletion dialog. It does not assert the exit; follow it with `expect_exit`.

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
| a `delete` step's `path` ends in the entry's `name` | `PathNotNamed` |
| a `delete` step's entry (its `path`, or its `name` directly below the root) is not the ownership marker `.excise-harness-owned` or anything inside it: the marker is what makes the directory a fixture | `DeletesTheMarker` |
| in a scenario with `disable_delete_confirmation`, a `delete` step has neither `wait_for = "started"` nor `confirm_with = "enter"`: there is no dialog to close or to confirm | `StartedWithoutDialog`, `ConfirmWithoutDialog` |
| a `wait_refresh` has a `delete` step before it: nothing else owes a refresh | `RefreshWithoutDelete` |
| an `expect_config` key is a dotted path of lowercase names, and it expects a non-empty string | `InvalidConfigKey`, `EmptyValue` |
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

### The deletion and lifecycle scenarios

Deletion, its confirmation, cancellation, refusal, and what the interface says and allows around it
are pinned by these scenarios, all `quick`, under `default` and `deterministic`, in both runners,
except that on Windows the pseudo-terminal runner skips the ones that have a `delete` step or press
Backspace themselves (see [Tiers and platforms](#tiers-and-platforms)), so only `cargo test` runs
them there, in-process. Each
asserts the facts by name (what is on disk, what the dialog names, what the interface reports, how
the run ends) and not whole frames.

A scenario that deletes something waits with `wait_refresh` before it asserts the outcome and
before it quits: until the program has replaced the map, the map still lists what was removed, and
a quit while a rebuild is running exits 130, and one before it has started exits 2.

What the interface reports is read from the header's status row (`Last deletion: 1 deleted · 0
changed · 0 missing · 0 failed · 0 not run`), which shows it after a deletion whatever is
selected. The selected-item panel words the same report shorter (`Last deletion: 1 removed`), but
only while an entry is selected, and what is selected afterwards depends on the platform: macOS
cannot prove that a file it removed left no other link behind, so it rebuilds the map, which drops
the filter and puts the cursor back on the largest entry, while Linux and Windows swap in a map
without the removed entry, which keeps the filter and, once the cursor has moved, selects nothing.
Four scenarios read the panel's wording instead of the row, for two reasons. The row gives way to a
more urgent status, and the `refused` fixture has one (`Scan complete · 1 path unreadable`):
nothing is removed there, so the map is not refreshed and the entry stays selected on every
platform. And at 60 columns the row is cut in the middle to fit beside the size of the scan store,
and how much of it is kept depends on the disk and on the labels in front of it (the reduced-guard
label, and `! ELEVATED` on an elevated Windows session such as the hosted runner), so both
60-column scenarios read the panel, which they can because they never move the cursor, and the
cursor lands on the largest entry again on every platform. For the same labels, the two
reduced-confirmation scenarios that read the row run 160 columns wide, where it is never cut. A
scenario never asserts the size of the store, which depends on the disk, or a path separator,
which depends on the platform.

| Scenario | Fixture | What it pins |
|---|---|---|
| `delete-file-lifecycle` | `delete-file` | A confirmed deletion removes only that file, the header reports one entry deleted and none failed, the map stays navigable, and quitting restores the terminal. |
| `delete-file-slow-terminal` | `delete-file` | The same on a terminal that drains 20,000 bytes a second, so that the screen trails the program by a good part of a second: the `delete` step reads its dialog from a screen that shows the frame the program drew for the Backspace, and still deletes exactly its target. `full` tier; Linux and macOS. |
| `delete-folder-lifecycle` | `delete-folder` | The same for a folder of 5,011 entries. |
| `delete-file-while-scanning` | `delete-file-while-scanning` | A file deleted while the first scan is still running leaves the scan and its map alone: the header goes on counting entries under a `SCANNING` badge, a key is answered, and once the scan ends a map without the file replaces the first one (a rebuild behind it instead, where the removal cannot be described). `full` tier; Linux and macOS. |
| `delete-tree-confirmed-with-enter` | `nested` | Enter confirms, and a folder goes with the folder inside it while the files beside it stay. |
| `delete-tree-narrow-terminal` | `nested` | The same in a terminal 60 columns wide. Linux and macOS: on Windows the fixture's path (under the runner's temporary directory, every backslash doubled on screen) is longer than a 60-column dialog shows, and the `delete` step refuses a path it cannot read whole. |
| `delete-file-cancelled` | `delete-file` | The first Backspace of a fresh map opens the confirmation for the largest entry; `n` closes it, and nothing is touched. |
| `delete-file-terminal-too-small` | `delete-file` | In a terminal 49 columns wide Backspace opens an error dialog that says to resize, and nothing is deleted. |
| `delete-file-reduced-confirmation`, `delete-tree-reduced-confirmation`, `delete-tree-narrow-terminal-reduced-confirmation` | `delete-file`, `nested` | `disable_delete_confirmation`: the header says the guard is reduced, Backspace deletes at once with no dialog, and the program reports what it removed. |
| `delete-refused-by-permission`, `delete-refused-reduced-confirmation` | `refused` | A file in a folder that cannot be changed is not removed, the selected-item panel reports `0 removed, 1 skipped`, and the exit code is 3 (a partial result). Linux and macOS. |
| `exit-prompt-keeps-pending-deletion` | `delete-file` | The exit prompt over a waiting confirmation names the waiting check, and keeping work brings the same confirmation back. |
| `theme-commit-saves` | `delete-file` | A committed theme is saved to the configuration file (`expect_config`), and quitting then shows only the ordinary prompt. |

`src/tests/cases/ui.rs` holds only layout snapshots and two tests of frames that neither runner can
see: the frame between a confirmed deletion and its result, and the filter prompt while a deletion
runs. A scenario that needs a step only the pseudo-terminal runner performs (`wait_event`,
`signal`, and so on) is skipped in-process, so these use none of them.

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
| `pty` | The session: `portable-pty` with a `vt100` screen model, key encoding for every scenario key with Ctrl and Alt, what the bytes written to the program can be read as (a request for a deletion, the confirmation of one: `input`), `ESC[6n` cursor-position requests answered from the screen model, resize, an asciicast v2 recording with output (`"o"`) and input (`"i"`) events, a token-bucket cap on how fast it drains the child's output (see [Terminal throughput](#pty-runner)), the frame marks it takes out of the output before the screen model sees it (and the latest frame whose mark it has read; on Windows `ConPTY` delivers a mark before the paint of its frame, so the screen cannot be tied to a frame exactly, see [`settle`](#pty-runner)), the terminal modes that failure bundles report, and the diagnostics (output timing, the count of answered cursor-position requests, and the bounded head and tail of the raw stream) a timed-out step's failure reports. |
| `events` | A strict reader for the event channel (`EXCISE_TEST_EVENTS`, protocol v1). It reads complete lines only, rejects an unknown `v`, and requires the first event to be the `hello` of the process the runner started. The `frame_marks` of the `hello` says whether the program marks its frames; a program from before the marks omits it. |
| `metrics` | Latency, stalls, output volume, and resource use (below). |
| `safety` | Ownership markers, environment isolation, scratch areas, process-group kill, fixture snapshots, and residue checks. |
| `runner` | Step execution, the verdict map, failure bundles, and the matrix. |

**Isolation.** The child starts with an empty environment plus `TERM=xterm-256color`, `COLORTERM`,
`LANG`, and the profile's variables (see [Profiles](#profiles)). `HOME`, `EXCISE_CONFIG`, the
working directory, `EXCISE_SCAN_STORE_DIR`, and the temporary directory point into a fresh scratch
area, and `EXCISE_TEST_EVENTS` names a new file inside it. A scenario's `scan_store_on_volume`
overrides `EXCISE_SCAN_STORE_DIR` to a directory on an attached volume instead (see
[Volumes](#volumes)); nothing else about isolation changes. The scratch area is private to its
owner on Unix (its root and directories have mode `0700` and its configuration file `0600`,
whatever the umask is), and is deleted after the run unless `--keep-fixture` is given.

**Process group.** On Unix the child leads its own session, so its process group id is its pid.
A timeout, a failed step, or dropping the session kills the whole group with `SIGKILL`, and the
session waits until nothing is left. So does the end of the child: its exit is seen without
reaping it (`waitid` with `WNOWAIT`), while its pid still names its group, and the group is killed
then, once, before the child is reaped, so a child that ends and leaves a descendant that ignores
the hang-up (a `nohup` job) does not leave it running. On Windows there is no group to signal: the
child is ended with the library's process termination, which does not reach descendants (`excise`
starts none). A job object would, but creating one needs `unsafe`, which this workspace allows
only in `src/os/windows.rs`.

**Reading the terminal.** The reader thread forwards what the terminal gives and ends with the
end of the output: a read that returns nothing, or that fails because the program's side of the
terminal is closed (`EIO` on Unix, a broken pipe on Windows). Any other failed read is not an end:
it reaches the session, and the caller's next `pump` or `wait_activity` returns it (`PtyError::Read`)
as a harness error, once. One `pump` takes in at most 1 MiB of output and leaves the rest for the
next call, so that a program that writes without pause cannot keep a caller from looking at its
deadlines, which it does between calls.

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
  defined under [Runner semantics](#runner-semantics), and then until the screen can be taken to
  show that frame, so that the step after it can read the screen once. The event says that the
  program drew, not that the drawing has arrived: the program writes it when it queues the frame for
  its terminal writer thread, which can hold the bytes back for as long as the terminal likes. So
  while the event channel is open `excise` follows every `frame` event with a mark in the terminal
  output, queued behind the frame's own bytes through the same writer
  (`ESC ] 9471 ; excise-frame=<seq> BEL`, see `docs/development.md`), and says so in its `hello`
  (`frame_marks`). The session takes the marks out of the stream before the screen model sees it
  and records the latest. On a Unix pseudo-terminal, which relays the output in order, the mark
  follows its frame's bytes, so the screen shows frame `seq` exactly when the session has read its
  mark, and the step waits for that, bounded by its `timeout_ms`: no quiet window, no guess.

  On Windows `ConPTY` re-renders the output instead of relaying it: it parses what the program
  writes into a buffer of its own, passes the mark through as soon as it has parsed it, and paints
  the screen later, on its own timer. So the mark reaches the harness before the paint of its frame,
  and reading it says that the console host has the frame, not that the screen model shows it.
  Observed in CI run 37376817720 (the Windows pseudo-terminal tier): the marks do reach the harness,
  and they arrive before the paint of their frame (in one recording, mark 10 at 0.133 s and the
  9,756-byte paint that shows frame 10 at 0.142 s).

  Whether the pseudo-terminal can tie its screen to a frame exactly is a capability,
  `SCREEN_IS_EXACT` in `crates/excise-harness/src/runner/live.rs`: true on Unix, where a frame's
  mark follows its bytes, so the screen is exact once the mark is read, and false on Windows. There
  no quiet-time rule can prove a `ConPTY` paint complete: a repaint can be split after a cursor or
  control prefix, and a paint begun before the mark can be flushed after it, leaving a stale dialog
  on the screen.

  Where the capability is false, the harness never confirms a deletion from the screen. The `delete`
  step, in both modes (with the dialog and with `disable_delete_confirmation`), and the interactive
  driver's `delete` and its confirmation guard refuse before any key, not even Backspace: the step
  fails with `DeleteRefused` (the driver with the error kind `refused`) and says that the terminal
  repaints the screen on its own timer (the console host of Windows), so the harness cannot tie the
  screen to a frame and confirms no deletion from it, and that the scenario runs in-process under
  `cargo test`. The reads that follow the Backspace in the protocol stay as a second line of
  defence; nothing can satisfy them there. `cargo xtask e2e` skips every scenario that has a
  `delete` step, in either mode, or that presses Backspace itself or composes an escape sequence
  with a `key` or `type` step (see `key` under [Steps](#steps)), where the capability is false,
  printing the reason (`SKIP name: ...`), also when the scenario is named with `--scenario`, as it
  does for a scenario outside its `platforms`.

  Reads that decide nothing destructive wait a bounded time on Windows instead: `settle`, `select`,
  `resize`, `wait_refresh`, the first frame, and the `expect_*` steps after a `settle`. After the
  mark they wait for the first output that is not a mark (the mark of another frame arriving first
  does not count), and then for the bounded quiet read, output quiet for 3 ms for at most 20 ms; or
  for the frame window (`CONPTY_FRAME_WINDOW`, 100 ms) to pass after the mark with nothing painted,
  which is taken to mean that nothing needed painting. Every part is bounded by the step's
  `timeout_ms`, so a mark that never comes still makes the step time out. Where the capability is
  true, as on Unix, the mark alone decides, as above.

  A program that does not mark its frames (a build from before the marks) gets the read that
  guesses: on Unix until the output has been quiet for 3 ms, for at most 20 ms; on Windows first
  100 ms of reading and then the same, because `ConPTY` keeps its own copy of the screen and sends
  what changed in paints that are typically 16 ms apart, so a frame drawn soon after a paint
  reaches the harness late. In the recordings of failed runs on GitHub-hosted Windows runners, 30
  isolated frames arrived 4 to 22 ms after their event (median 12.5 ms), and the gaps between the
  paints of a console that was being redrawn were 15.6 ms at the median, 23 ms at the 99th
  percentile, and 54 ms at most, while the program was starting. The delays are upper bounds,
  because the program's clock and the recording's differ by an offset that only the moments the
  keys were sent bound. Nothing that could confirm a deletion relies on that guess: `delete`
  refuses such a program. A key that changes nothing draws no frame and never settles.
- **The input barrier** is how `delete`, and the interactive driver's guard, know that the program
  has read what they wrote. Counting the inputs sent against the `inputs` of a frame cannot say so:
  a terminal does not promise that one write is one input event. `ESC DEL` reaches the program as
  Alt+Backspace when it reads both bytes at once and as Esc and then Backspace when it does not, and
  an `ESC` read together with the `ESC [ A` of an arrow is Esc, `[`, `A`, so a frame can count the
  inputs sent while events they became are still unread. While the event channel is open, `excise`
  reads the byte `0x1d` (`ctrl+]`, which no `key` step can write) as a *barrier request*: it is read
  in order with the rest of the input, it is not counted in `inputs`, and a frame is drawn for it
  that carries `barriers`, the number of requests read. The runner writes a request behind the keys
  it needs read, waits for the first frame whose `barriers` reaches the number of requests written,
  and then for the screen to show that frame (its mark). The screen then shows what every key before
  the request did, however the terminal cut their bytes into events, and the key that confirms a
  deletion is the next thing written. At most one request is outstanding: one that is never
  answered is waited for before another is written, so that an answer cannot be taken for the
  answer to a later request. `hello` says `input_barrier` when the program answers; whatever could
  confirm a deletion refuses a program that does not, before any key. A resize is a signal and not
  a byte, so the barrier does not order it: the `resize` step waits for the frame that counts it. A
  scenario that has pressed Backspace itself, or has composed a key from an escape sequence, writes
  one too, before each key that could confirm what that Backspace opened (see `key` under
  [Steps](#steps)); no barrier can be written behind an escape sequence that is still open, as the
  program takes it for a byte of the sequence, so a write that would continue one is refused.
- **`select`** opens the filter with `/`, erases any text it opened with, types the name, checks
  the prompt, presses Enter, and waits until the inspector pane shows exactly that name. In an
  engaged run (see `key` under [Steps](#steps)) the name's characters and the Enter are sent only
  behind a barrier, on an exact screen that shows the open prompt and no deletion dialog.
- **`delete`** presses Backspace and reads the dialog after an input barrier written behind it, from
  a screen that shows the frame that answers the barrier, as the frame marks say where
  `SCREEN_IS_EXACT` is true (Unix). Where it is false (Windows) the
  step refuses before any key, not even Backspace, in both modes: see `settle`. It presses `y` only
  when the dialog names exactly the requested entry, kind, and path, every sentinel exists, and the
  fixture still carries its ownership marker. Any mismatch fails the step and no `y` is ever sent,
  and so does a program that does not mark its frames. The waits after the confirming key read the
  screen as `settle` does.
  `wait_for = "finished"` (the default) ends the step when the `deletion_finished` event and the
  first frame after it have been read and the screen shows that frame, so the screen shows the
  result. `wait_for = "started"` ends it
  as soon as a frame shows the dialog has closed, without waiting for the deletion itself: the
  deletion keeps running after the step returns, so a step
  that needs its outcome waits for that separately (`wait_fs_absent`, `wait_event`). The map still
  lists what the deletion removed when the step returns, and the program treats a quit while a
  rebuild of that map is running as a cancellation (exit code 130; before the rebuild has started
  the exit code is 2, and before an overlay lands the program exits with the map as it stands): a
  scenario that goes on to quit waits
  with `wait_refresh` first, after a `"started"` delete as after a `"finished"` one. The header
  cannot stand in for it, because it reads `COMPLETE` from the map as it was.
- **`wait_refresh`** needs every deletion the scenario has confirmed to have reported
  (`deletion_finished`), and then the program's `refresh_finished` event after the last report that
  removed anything: `published` once the map without the removed entries is the one on screen, or
  `failed`, which fails the step, when no map could be shown. The program reports it from its own
  state (no rebuild and no publication is owed), so it holds whether the map was rebuilt or swapped
  for one without the entries, and a refresh that ended before the deletion cannot satisfy it,
  which is why no `wait_event` can name it. A deletion that removed nothing owes none, so after one
  the step waits for nothing but a frame. It ends as `delete` does after its confirming key, on a
  frame after the event, once the screen shows it, as `settle` reads it.
- **`quit`** presses `q`, waits for the quit dialog, and confirms with `y`. In an engaged run it
  does not take the frame event that counts the `q`, and a screen, for the answer: it writes a
  barrier behind the `q` and confirms only if the exact screen that answers shows the plain quit
  prompt and no deletion dialog.
- **`resize`** resizes the terminal and waits for the frame that answers it, then until the screen
  shows it, as `settle` does.
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
regular file; `FixtureRoot` adds only the canonical spelling of the path, and
`FixtureRoot::verify_owned` repeats the check, which the runners do right before each key that
confirms or starts a deletion: a marker that vanished or was replaced after the run began fails the
step and no key is sent.

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
  `platforms` is still skipped, with the reason, even when it is named, and so is a scenario that
  has a `delete` step, presses Backspace itself, or composes an escape sequence from its keys
  wherever the terminal's screen cannot be tied to a frame exactly (Windows: `SCREEN_IS_EXACT` is
  false, see [`settle`](#pty-runner)); `cargo test`
  runs it in-process there. A scenario outside the selected tier or its `platforms` is otherwise
  skipped, with the reason (`SKIP name: reason`), and printed.
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

The negative controls for the `delete` step are not scenarios. They are
[`tests/controls/delete-wrong-target.toml`](tests/controls/delete-wrong-target.toml) and
[`tests/controls/delete-reduced-wrong-target.toml`](tests/controls/delete-reduced-wrong-target.toml)
(the same, in a scenario that disables the confirmation): each selects a folder and then asks
`delete` for a different entry. Two more ask for an entry the step cannot bind to what it names:
[`tests/controls/delete-default-path-nested-entry.toml`](tests/controls/delete-default-path-nested-entry.toml)
selects `docs/twin.bin` and gives no `path`, which means `twin.bin` directly below the root, and
[`tests/controls/delete-reduced-ambiguous-name.toml`](tests/controls/delete-reduced-ambiguous-name.toml)
disables the confirmation over a fixture with two files called `twin.bin`, so that the panel cannot
say which one Backspace would delete. `cargo xtask e2e` never runs them, because they are not in
`scenarios/`. `tests/harness_scenarios.rs` in the `excise` crate runs `delete-folder-lifecycle`
under every profile it names against the crate's own binary as part of `cargo test`, along with a
few one-off scenarios that exercise the runner itself, and runs each control to assert that the
`delete` step failed, that Backspace was the last input when a dialog is open and was never sent
when there is none, that no `y` appears among the recording's input events, and that every byte of
the fixture is unchanged. On Windows `tests/harness_scenarios.rs` asserts that the `delete` step
refuses before any key, with the `ConPTY` reason, and that the fixture is unchanged, so a Windows
run is a positive check of the rule under [`settle`](#pty-runner); on Unix nothing changes.

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
cargo xtask headless [--quick|--full] [--fixture ID]... [--fixture-dir DIR] [--class scale|identity|hostile|volumes]... [--profile default|deterministic] [--repeat N] [--timeout SECONDS] [--timing-informational] [--keep-scratch]
```

`excise_harness::headless` scans a fixture without a terminal, holds the report to the fixture's
oracle, and times the scan against `du -sk`. The exactness and throughput claims of the validation
program rest on it.

**Your own specs.** `--fixture-dir DIR` takes the specs from a directory of your own, `DIR/<id>.toml`,
instead of the bundled ones, so that a scan can run on a fixture shaped like a tree of yours (see
[Shape profiles](#shape-profiles)). The ids the run knows are then those of `DIR` alone: a
`--fixture` that names a bundled id is refused as unknown, with no `--fixture` every spec of `DIR`
that fits the tier's size limit runs, and a `DIR` that is not a directory is an error. Generated
masters are cached below the target directory as for any fixture, under a name that holds the
hash of the spec, so two specs with one id never share a cache entry. No documented defect applies
to them (`expectations/headless.toml` names bundled fixtures only), so a scan of one must pass.

**A spec is not trusted to be one.** A directory of your own can hold anything, so a spec is
opened without following a link, must be a regular file, and is read up to 1 MiB
(`MAX_SPEC_BYTES`: the largest bundled spec is under 1.5 KiB, and one `excise-shape spec` writes
is a few tens of KiB at most). A link, a FIFO, a folder, a device, or a larger file is refused with
a message that names it, and none is waited on or read in full. The run reads every spec of `DIR`
to learn its ids and classes, so one such file stops it before anything is scanned; `bench-e2e`
reads the specs of the `--fixture` ids it is given the same way.

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
`docs/schemas/scan-report.schema.json` and the `native-path` schema it references (a report that
declares `schema_version` 1, the report of Excise 1.0.0 through 1.2.4, is held to
`legacy/scan-report-v1.schema.json` instead, which the [version sweep](#version-sweep) needs to
read the published releases, and any other version is refused). The file is checked one entry at a
time, so a large report never has to fit in memory twice. A report that breaks the schema is
`invalid-report`, a scan that ends without one is `no-report`, a scan that does not end in time is
`timeout`, and a scan that prints to standard output, although its report goes to a file, is
`unexpected-output`.

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
cargo xtask bench-e2e --baseline <ref> [--baseline-binary PATH] [--candidate-binary PATH] [--fixture ID]... [--fixture-dir DIR] [--scenario NAME --profile PROFILE]... [--pairs N] [--seed S] [--timing-threshold FRACTION] [--memory-tolerance FRACTION] [--strict] [--timeout SECONDS]
```

`excise_harness::bench` builds (or accepts) two `excise` binaries and compares them with paired,
interleaved A/B runs on the same warm fixture. A single, unpaired timing never transfers across
sessions (the same binary and tree shape have measured 9.46 s one day and 3.8–3.9 s the next); only
a paired, interleaved comparison on one machine in one session counts as evidence.

**Your own specs.** `--fixture-dir DIR` takes the ids of `--fixture` from a directory of specs of
your own, as it does for [`headless`](#headless-runner): `--fixture home-50k --fixture-dir
target/excise-specs` compares the two builds on a tree shaped like your home. An id that `DIR`
does not hold is refused, and a `DIR` that is not a directory is an error. A `--scenario` is not
affected: a scenario names a bundled fixture, so it keeps the bundled specs. Because `ab.json`
names a fixture by its id alone, a `--fixture` of `DIR` whose id is also the bundled fixture of a
selected `--scenario` is refused before any case runs, unless the two specs are the same apart
from their description: rename the spec in `DIR`.

**The two builds.** `--baseline <ref>` builds any git ref (a branch, a tag, or a SHA) in a
temporary detached worktree with its own `CARGO_TARGET_DIR`, release, `--locked`, then removes the
worktree with `git worktree remove --force` and checks with `git worktree list` that git no longer
lists it (`git worktree prune` is never run; see **Removing the worktree** under
[Version sweep](#version-sweep)). The built binary is cached by its resolved commit SHA under the
target directory (`excise-bench-e2e-baselines/<sha>/`), so a later run against the same commit does
not rebuild.
The build is the shared ref builder, `xtask/src/refs.rs`, which `cargo xtask sweep` uses too, under
its `Inherited` toolchain policy: `bench-e2e` runs `$CARGO` (`cargo` when that is unset) in the
environment it was started in, so `RUSTUP_TOOLCHAIN` still decides and the baseline is compiled by
the same compiler as the candidate, whatever toolchain file the baseline's own commit holds (the
sweep pins each ref to its own toolchain instead; see [Version sweep](#version-sweep)). The build is
bounded as the sweep's are: after 60 minutes it is killed with everything it started. Its output
goes to `excise-bench-e2e-baselines/baseline-build.log` below the target directory (each build
truncates it) and not to the terminal: the build runs in a process group of its own, which is not
the terminal's foreground group, and a write to the terminal would stop it where the terminal sets
`tostop`. Before a build, and not when the baseline is cached, `bench-e2e` prints on standard error
`building the baseline <ref>; its output is in <path>`, and a build that fails carries the last 30
lines of the log in its error. The ref is resolved once, to its commit, and that commit is what is
announced and what is built, so a branch that moves in between changes neither. The candidate is
always the current checkout's release build.
`--baseline-binary` and `--candidate-binary` each skip building that side and use the given path
instead (for tests, and for comparing prebuilt binaries); the document's `baseline`/`candidate`
`git_ref` records which was used.

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

**A baseline from before the frame marks.** A scenario that has a `delete` step, or presses
Backspace itself with a `key` or `type` step, or composes an escape sequence from its keys, is
compared only against a baseline whose `hello`
says `frame_marks` and `input_barrier` (see [`settle`](#pty-runner) and the input barrier below
it): the `delete` step refuses any other program before it sends a key, Backspace included
(`DeleteRefused`), and so does the first key after a Backspace of the scenario's own that could
confirm what it opened (the `y` of `quit` among them), and `bench-e2e` stops with that error
instead of a verdict. `--fixture` cases and scenarios that have neither have no such need, so a
baseline from before them can still be compared on those.

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

## Version sweep

```console
cargo xtask sweep [--refs REF...] [--quick|--full] [--fixture ID]... [--rounds N] [--seed S] [--timeout SECONDS]
```

`excise_harness::sweep` runs every published version of `excise` through the checks a published
release can take, and tabulates which of the defects fixed in the release under development each
one shows: one row per defect, one column per version, and in each cell `affected`,
`not-affected`, or `not-measurable`, with the reason, the measured value, and where the evidence
is. A changelog entry that names the versions a defect affects is a claim about every published
release, and this is the measurement of it. `xtask/src/sweep.rs` parses the command line, builds
each ref, prints the grid, and turns what went wrong into an exit status; `xtask/src/refs.rs`
builds a ref, and `bench-e2e` uses it too. The sweep is a manual tier: CI runs it only when a
maintainer dispatches `weekly.yml` with `job` set to `version-sweep`, and never on a pull request
(see [Version sweep](../../docs/development.md#version-sweep) in `docs/development.md` for how to
run it and what CI runs).

**What it builds.** With no `--refs`, every published release tag, exactly `v1.N.N` (a
pre-release such as `v1.4.0-rc.1` and every other tag is ignored), oldest first, and then `HEAD`.
`--refs` names refs (a tag, a branch, or a commit) up to the next flag, and may be given again.
The last ref is the candidate: every ratio is taken to it, so name the oldest first. A ref is a
commit, built from a checkout of it, so uncommitted changes are in no build, the candidate's
included; and it is the commit the ref resolved to when the sweep planned its builds, so a branch
or `HEAD` that moves while the sweep runs changes nothing it builds. A repository with no `v1.N.N`
tag fails with the advice to `git fetch --tags`. The sweep never reads `EXCISE_E2E_BINARY`: it
builds every ref itself.

**How a ref is built.** Every ref is resolved to its commit (an annotated tag peels to the commit
it points at) and planned before the first build starts, so a ref that does not resolve, or a
toolchain that is not installed (below), fails the sweep at once with every such problem listed.
The commit that was planned is what is built: it is checked out, detached, into a worktree
directly below `target/excise-sweep-worktrees/` and built there with
`cargo build --release --locked -p excise`, with `CARGO_TARGET_DIR` set to
`target/excise-sweep-builds/<sha>/`; the binary is `<sha>/release/excise` below it. The binary is
cached by commit SHA, so a second run builds nothing for a commit it has built (the detail says
`built (cached)`, and the run has no log for it); a cached binary is trusted only when
`<sha>/toolchain.txt` names the channel the ref pins now, so one whose build never finished is
built again. A build that fails is a result and not the end of the sweep: the version's column says
`the build failed`, with the last 30 lines of cargo's output in the reason and all of it in
`builds/<label>.log` (see **Output**), and the other versions go on.

**Removing the worktree.** The worktree is removed when the build ends, whether it succeeded,
failed, ran out of time, or panicked, with `git worktree remove --force`. The sweep never runs
`git worktree prune`: it forgets every worktree of the repository whose directory is missing (an
unmounted disk's, a moved directory's), with its index, its detached `HEAD`, and its reflog, not
only this one. When git refuses (a locked worktree, a directory it cannot empty) or leaves
something behind, the builder deletes the worktree's own directory and then the one administrative
entry git keeps for it, `<git-common-dir>/worktrees/<name>`, and nothing else. It checks the entry
before it deletes anything: it must be a real directory, not a link, directly inside
`<git-common-dir>/worktrees/`, and its `gitdir` file must name this worktree's `.git`. A check that
fails deletes nothing, the directory included, so that a person sees both, and the error says why.
The removal counts only when the directory, the entry, and the worktree's line in
`git worktree list --porcelain` are all gone. A worktree that cannot be removed is an error that
names `git worktree remove --force DIR` and warns that prune would also forget every other
worktree whose directory is missing.

**Bounds.** Nothing is waited for without one. A `cargo build` has a deadline of 60 minutes, and
every `git` and `rustup` command the builder runs has one of 5 minutes. A command that is still
running at its deadline is killed with everything it started: `SIGKILL` goes to its process group
on Unix, and on Windows, which has no group to signal, to the command alone. A build that ran out
of time fails as one that failed does: its worktree is removed, and its column says
`the build failed`, with `timed out after 60 minutes and was killed` and the end of its log. A
build that exits by itself, whether it succeeded or failed, has whatever it left running in its
process group killed too (`SIGKILL`, on Unix), so that nothing a build started outlives it, and
nothing is left to hold its worktree when that is removed. The group is signalled before the build
is reaped, while the build is still a zombie whose id no other process can be given, so the signal
reaches the group's own members or nothing. The `git` and `rustup` commands get no
such kill. `git` can start a detached `git gc --auto`, which puts itself in a session of its own,
so that a kill of the group would not reach it anyway, and it runs the user's hooks, whose
background work is the user's own configuration of their repository: killing it would break their
maintenance. The short `rustup` probes start nothing that outlives them. Every command reads from
the null device, and on Unix runs in a process group of its own so that the group can be killed
whole. On Unix, then, Ctrl-C (or any signal sent to the sweep's process group) reaches the sweep
and not a build that is running: an interrupted sweep leaves its build running, with no deadline,
until it ends, and one that is stuck has to be stopped by hand (see below). A run that is killed
(Ctrl-C, a timeout, `SIGKILL`) also cannot remove the worktree it was building in.
To see the build, run `ps -axo pid,pgid,command | grep '[c]argo build'` (macOS and Linux): it is
`cargo build --release --locked -p excise` (under `rustup run <channel>` when the ref pins a
toolchain), its working directory is a worktree below `target/excise-sweep-worktrees/`, and its
lines share one `pgid`. A `cargo build` of your own matches too. To stop the sweep's, send its whole
group SIGTERM with `kill -TERM -- -PGID`, `PGID` being that column. Then remove the worktree, whose
directory is `<sha>-<pid>-<n>` below `target/excise-sweep-worktrees/` (`git worktree list` shows
it), with `git worktree remove --force target/excise-sweep-worktrees/<dir>`, which removes the
directory and git's entry for it together. Do not run `git worktree prune` for it.

**Each ref's own toolchain.** The sweep judges a release by the compiler the release was built and
tested with, so each ref is built with the toolchain its own commit pins
(`ToolchainPolicy::RefPinned`). The channel is read from the commit, not from the working tree: the
`channel` of its `rust-toolchain.toml`, or the one the legacy `rust-toolchain` names. The build runs
as `rustup run <channel> cargo build ...`, with `RUSTUP_TOOLCHAIN`, `CARGO`, `RUSTC`, and
`RUSTDOC` removed from its environment (the candidate's too), so that nothing the caller runs
under can override a pin, and with `RUSTUP_AUTO_INSTALL=0`: the sweep never installs a toolchain.
A pin that is not installed is an error that names the command that installs it, for example
`rustup toolchain install 1.88.0` (v1.0.0 and v1.0.1 pin 1.88.0; every later tag and `HEAD` pin
1.98.0). A ref that pins nothing is built by `cargo` from `PATH`, and its channel is the word
`default`. The `rustc --version` of each build is recorded in the document
(`versions[].toolchain`), in `table.txt`, and in `<sha>/toolchain.txt`. The shell that runs the
sweep must not export `RUSTUP_TOOLCHAIN`: the sweep removes it from every build it makes, but the
`cargo` that builds and runs `xtask` itself honors it, and would run the sweep with another
toolchain than the checkout pins. `bench-e2e` uses the opposite policy, `Inherited`, for its
baseline (see [Paired A/B benchmark](#paired-ab-benchmark)).

**The checks.** Each runs one build only through the harness: on a generated fixture that carries
the ownership marker, in the isolated environment of the other runners (the environment rebuilt
from `TERM`, `COLORTERM`, and `LANG`, and a scratch `HOME`, configuration, working directory,
temporary directory, and scan-store directory), in a pseudo-terminal of its own process group
where it needs one, with every wait bounded, and it ends the program before it returns. A check
reads only what a person at the terminal or a script could see: the screen, the exit status, the
terminal modes, the bytes written to the terminal, the program's CPU time and open descriptors, the
scratch area, and the report. None uses the test event channel (the sweep never sets
`EXCISE_TEST_EVENTS`), frame marks, or the input barrier, which a published release does not have;
the candidate is run the same way, so that every column is measured with the same instruments. A
check that cannot run on a build says why (`not-run`: the header never reads `COMPLETE`) and is not
a finding, and nor is a variant of a check that cannot be carried out (the filter prompt does not
open): the check records which and why, and the cell that rests on it says so. A check the harness
could not carry out (a spawn, a fixture, or an isolation failure) is `errored`, and the command
fails (see **Exit status**).

| Check | Fixture and profile | What it does | What it reads | Rows |
|---|---|---|---|---|
| `--help` | every build | Asks for `--help` (20 s bound). | The flags it lists: `--format` and `--output` (without both the build has no headless scan, and no headless check or timing runs on it) and `--scan-store-dir` (earlier builds keep their scan data in the temporary directory, which the isolation points into the scratch area, so the residue checks look there). Recorded in `versions[].traits`, with the report version the build wrote. | all |
| `headless-oracle` | `node-modules-2k`, `hostile-small`, `deep-past-path-max`; the full tier adds `identity-small`, `all-classes-small`, `wide-1k`; `default` | One scan, `excise --format json --output <report> <root>`, as the [headless runner](#headless-runner) does it. | The report, read by its own version (below) and diffed against the fixture's oracle; the exit status; the scratch area; the free space of the volume the scratch areas are on. | F10, F11, F7 |
| `signal-term`, `signal-hup`, `signal-quit` (not on macOS; see **Signals**) | `navigate-folders`, `deterministic` | Waits for `COMPLETE` and for the output to go quiet, delivers the signal, and waits up to 15 s for the exit. | The exit status (a confirmed quit exits 130), the terminal modes (restored), the scratch area (nothing left behind). | F6 |
| `signal-close` (Windows, in place of the signals above) | same | The same, with the console close event. | The same, except the terminal, which a closed console cannot show. | F24, F6 |
| `kill-restart` | `navigate-folders`, `deterministic` | Kills the program with `SIGKILL` once it is idle, starts another in the same scratch area, and waits for its scan to complete. | What the scratch area held after the kill, and which of that is still there after the second scan. | F4 |
| `idle` | `navigate-folders`, `default` | Sends nothing after `COMPLETE`, and looks for 5 s starting 3.2 s after it. | The bytes written to the terminal and the CPU time the program used in that window. | F3 |
| `filter` | `delete-folder`, `deterministic` | Run one reads the selected-item pane at `COMPLETE` with no key pressed, moves to `victim` with the arrows, opens it with Enter, types `/`, `part00`, and Enter, and watches for 1.5 s. Run two types the same at the root. If run one's scan does not reach `COMPLETE` the check did not run. A variant that cannot be carried out (the folder does not open, the filter prompt does not open, or run two's scan does not reach `COMPLETE`) is recorded as `did not run: <why>`, in the check's notes (`` filter `part00` at the root: ... ``) and in its evidence, which also holds the screen each variant ended on, and it is not counted as the program surviving. | The entry the pane names, against the largest entry of the fixture's oracle (see **F23b** for what a match shows); whether the program ended after the Enter, in each variant, and with what exit status. | F23b, F22 |
| `selection-drift` (full tier) | `selection-drift`, `deterministic` | Two attempts per version: waits until the pane shows `big-file.bin` while the scan runs, presses Down, and reads the pane again after `COMPLETE` and the settling. | Whether the pane then names another entry than before. An attempt whose scan never reaches `COMPLETE` after the Down (it outlasts the bound, or the program ends) reads nothing after it and is counted apart, and a pane that names no entry is not a different entry. The record has `attempts`, `precondition_met` (the pane showed `big-file.bin` before `COMPLETE`), `reached_complete` (the scan then reached `COMPLETE` and was read), and `drifted`; its note and the last line of its evidence file give the same counts in one sentence. | F5 |
| `descriptors` (full tier) | `wide-1k`, then `tiny-files-50k`; `deterministic` | Scans each to `COMPLETE`. | The most descriptors the program held at the samples taken while it ran. | F13 |

**Signals.** The F6 checks send the signals the platform allows, and the F6 cell is decided from
the ones that were sent (`signals_for`, in `sweep/signals.rs`): SIGTERM and SIGHUP on macOS;
SIGTERM, SIGHUP, and SIGQUIT on Linux and the other Unix systems; the console close on Windows.
SIGQUIT is not sent on macOS. A release that has no handler for it is ended by the default action,
which macOS treats as a crash, and its crash reporter writes a report into the person's own
`~/Library/Logs/DiagnosticReports`, whatever the limit on core files is. That folder is not the
sweep's, and the sweep must not clean it, so it sends nothing that would fill it. The macOS cell
says so in each of its states (`affected`, `not-affected`, and `not-measurable`), at the end of its
reason, unless the build failed and the cell says only that: `` SIGQUIT is not sent on macOS: its
default action makes macOS write a crash report into the person's own
`~/Library/Logs/DiagnosticReports` ``. The run says it on stderr once, when it starts the interface
checks. A signal that is not sent is never required. One that is sent and whose check did not run
keeps the cell `not-measurable` and is named (`SIGHUP: <why>`), and one unclean signal makes the
cell `affected`. On Unix the sweep sets its soft limit on the size of a core file to 0 before it
runs its first check, and every program it runs inherits it, so that on Linux a SIGQUIT leaves no
core file. A machine that pipes core dumps to a handler (a `kernel.core_pattern` that begins with
`|`) does not enforce that limit (`core(5)`), and what such a handler keeps is its own.

**How a report is read.** `ScanDocument::read` reads the `schema_version` a report declares and
holds the report to that version's schema: version 1 (Excise 1.0.0 through 1.2.4) to
`legacy/scan-report-v1.schema.json`, a byte-for-byte copy of the schema those ten releases
published, and version 3 (1.3.0 and later) to `docs/schemas/scan-report.schema.json`. A build is
held to the contract it published, not to the current one; any other version is refused. A
version-1 report has no scan store: its summary gives the memory limit of its in-memory model
(`model_bytes`, `model_limit_bytes`), which the typed values read into the scan-store fields.

**Which fixtures.** The quick tier (`--quick`) is the checks above on the small fixtures. The full
tier (`--full`, the default) adds the oracle scans of `identity-small`, `all-classes-small`, and
`wide-1k`, and the two checks that need a large tree: `selection-drift` (20,053 entries) and
`descriptors` (49,050 entries on the large tree), which bound their waits at 600 s, or at
`--timeout` when that is longer. `--fixture ID` (repeatable) replaces the fixtures of the oracle
scans and the timings; the interface checks keep theirs. A row that reads a fixture
the run left out says so and is `not-measurable`: F1, F2, F16, and F18 read `node-modules-2k`, F10
reads `hostile-small`, and F11 reads `deep-past-path-max`.

**No deletion, ever.** A published release can be told to delete what it shows, so the sweep never
tells one to. A deletion dialog opens on one key, Backspace, and a program that has none open has
nothing to confirm. The keys a check sends are a closed vocabulary (`/`, Enter, the four arrows, and
the characters of a name: letters, digits, `_`, `.`, and `-`), and every write is read through
`pty::input::InputScan` first, which says whether the program could read the bytes so far as a
request for a deletion (Backspace, Ctrl+H, or an escape sequence that a later write would finish):
a write that could is refused before it is sent. With no request ever sent, no dialog exists for
the Enter that opens a folder or applies a filter to meet. A check never confirms a quit either: it
ends a program with a signal or `SIGKILL`. This is the guarantee the scenario runner gives a build
without frame marks, and the sweep goes further: it has no `delete` step and never asks. As a
second line, every fixture a build runs on is compared with what it was before, and a difference
stops the whole sweep at once, without writing the table, because a build ran that deletes or
rewrites what it was only to read. The headless scans share each fixture between all the versions,
so every fixture they scan is held to the plan it was generated from before the first scan runs and
again after each version's scans, which keeps a difference from being put down to a version that
predates it; the message names the fixture and what differs, and the version that had just scanned
it, when one had. A cached master is checked in full (its marker, its top-level names, and every
entry), and a run copy (a fixture that cannot be cached) by the oracle's comparison of a walk of it
with the plan, and its marker. Each interface check compares its fixture with a snapshot taken
before the check, and the paired timings compare the timing fixture with a snapshot taken before
the first run and another after the last.
A comparison sees which entries there are and each one's kind, size, and link target (the check
against the plan also each one's permission bits). It does not read the contents of a file, and it
cannot see below a folder it cannot list, which `hostile-small` makes on purpose, so nothing below
one of those is held to anything. So the deletion rows (F9, F14, F19, and F21) are `not-measurable`
for every version, the candidate's included, with the reason `no confirmation is sent to a build
without frame marks`: what a deletion shows needs a confirmed deletion, which only a scenario's
`delete` step sends, to a build that marks its frames.

**The paired timing.** A single timing never transfers across sessions (the same binary and tree
shape have measured 9.46 s one day and 3.8 s the next), so a version is compared with the candidate
only through rounds that ran every version one after another, in one session, on the same warm
fixture (the cached master, which the scans only read). A round runs the versions in turn,
starting from a different version every round, so that no version always follows the same one and a
drift of the machine over a round falls on every version in turn. The order of every round is
recorded (`measurements[].order`). In its turn a version runs every run of its phase back to back,
so that what two runs measure of one version is measured in the same round. There are two phases,
each with an untimed warm-up round of its own and then the `--rounds` measured rounds (5 by
default):

1. Against a terminal that reads as fast as the program writes. In its turn a version scans the
   fixture headless and then, back to back, runs the interface under `default` to `COMPLETE`. The
   scan feeds three series: its wall time (`headless_wall_ms`), the time its report took to write
   (`report_write_ms`), and the wall time less that (`headless_scan_ms`). `du -sk` is timed once
   per round, beside the scans. F18 asks how much later the interface reaches `COMPLETE` than the
   headless scan of the same version, and the scan is `headless_scan_ms`, without the writing of
   its report (see **The report write**). Both timings are taken in one turn and held to each
   other round by round, and not to timings taken in another phase, when the machine may have been
   busy with something else.
2. Against a terminal that reads 150,000 bytes a second. In its turn a version runs the interface
   under `default` and then under `reduced-motion`, so that default motion is held to reduced
   motion of the same version in the same round (F2).

| Metric | What it is | On | Rows |
|---|---|---|---|
| `headless_wall_ms` | The wall time of `excise --format json --output <report> <root>`: the whole command, the writing of the report included. A scan finished when it ended within `--timeout` with exit code 0, 2, or 3. Also reported against `du -sk`. | `node-modules-2k` | F1 |
| `report_write_ms` | How long the report took to write: from the first change in the length of the `--output` file to its last, polled every 0.5 ms and once more after the process has exited (see **The report write**); one value for each scan. A scan that finished and whose report was seen to change in length fewer than twice gives none, and never 0 ms. | same | F16 |
| `headless_scan_ms` | The wall time of a scan less its `report_write_ms`, from the same run: the scan, with what the process does before and after it. A run gives it only as **The report write** says, and otherwise has no sample of it. | same | F18, as the time the interface is held to |
| `tui_complete_ms` | From the spawn to the header reading `COMPLETE`, in a pseudo-terminal. A run finished when the header read `COMPLETE` within `--timeout`; a program that ended first did not finish. Under `default` against a terminal that reads as fast as the program writes, and under `default` and `reduced-motion` against one that reads 150,000 bytes a second. | same | F18 (`default`, fast terminal, over the same version's `headless_scan_ms` of the same round), F2 (both profiles, slow terminal) |

**The report write.** A release writes its report after its scan, and the write can be most of the
wall time of a headless run. In a smoke run (3 rounds on `node-modules-2k`, one session, so an
illustration and not a figure to compare) the wall time and the part of it spent writing the report
were 1362 ms and 1261 ms for v1.0.0, 4925 ms and 1275 ms for v1.3.0, and 154 ms and 7 ms for `HEAD`.
A release with a slow write (F1, F16) therefore takes longer to finish headless than to scan, and
an interface held to that wall time is held to a time that is partly the report: v1.3.0's
interface took 0.8 times its wall time, which favours exactly the releases with the slow write. So
F18 is held to `headless_scan_ms` and never to the wall time.

The write is measured from outside, once for each scan (so for each version in each round). A
thread polls the size of the `--output` file every 0.5 ms while the process runs, and once more
after it has exited, and notes when the size changed. It asks whether the process has exited before
it reads the size, so that a report that grows between one read and the exit is read as it ended up
and its last change is not left out. `report_write_ms` is the time from the first change to the
last, accurate to about a millisecond, and `headless_scan_ms` is the wall time of the same run less
it. That reads the write because every published release creates the output file only after its
scan, so the first byte of the report is the end of the scan and the growth of the file is the
write, in v1.0.0 too: in `src/cli.rs`, `scan_headless` is called before `write_scan_report`, which
calls `File::create` (the lines are 78 and 144 in v1.0.0 to v1.2.1, 79 and 145 in v1.2.2 to
v1.2.4, and 82 and 148 in v1.3.0), and the candidate's `scan_headless_with_stop_signals` likewise
comes before its `write_scan_report`. A release that streamed its report while it scanned would
break that, and its write would be as long as its scan. The measurement has limits: the half
millisecond of the poll, and a little more for a last growth that is read only after the process
has exited, which is timed at that read; what the process does after its last byte (exit,
clean-up), which stays in the scan time; and a report whose size was seen to change fewer than
twice, which says nothing about how long it took and is not a write of 0 ms. A report that is
written whole between two reads is seen to change once, and so is one that is whole at the read
after the exit: the scan had ended, and how long the write took is not known.

A run gives a scan time without its report only when the scan finished (it ended within `--timeout`
with exit code 0, 2, or 3), the file was seen to change at least twice, and at least 1 ms of the
wall time is left once the write is taken off. Otherwise the run has no sample of
`headless_scan_ms`: that round of it is a round without a sample, never a reading of 0 ms.
`report_write_ms` is a sample only for a scan that finished and whose file was seen to change at
least twice. A scan that did not finish is censored (see below): its `report_write_ms` is how long
the report had taken to write by then, flagged as not finished, and its scan time is known, as the
least it took (the length of the run), only when no byte of the report had been seen, because the
scan was then still going. A run that was killed or failed after the report began to appear, in
one change or in several, says nothing about how long its scan took. A round without a scan time
gives no pair for F18, so a version with too few rounds in which the write could be told apart from
the scan is `not-measurable` for F18, and its cell says in how many of how many rounds it could
not.

**Rounds are logical.** Every timing is kept in the slot of the round it was taken in, with whether
it finished, for each metric of the run. A round in which a version did not run (it was given up
on, below, or its runs could not be carried out) is a slot with no sample: it is neither a sample
nor a pair, and it never moves a later round against another. So is a round in which a run has no
usable reading of a series (a report seen to change in length fewer than twice gives no
`report_write_ms` and no `headless_scan_ms`): it is no reading of 0 ms, and no error either. A ratio
is made of the rounds in which both of its sides have a sample.

**A run that does not finish is censored.** A run that does not finish within `--timeout` (120 s by
default) is kept at how long it ran (the bound, when it ran out of time; the time to the exit, when
the program ended first) and flagged as not completed. That is the least it took, and no more is
known. A round in which both sides finished gives a ratio. The median of a version's ratios to the
candidate, and its deterministic bootstrap 95% confidence interval (`bench::bootstrap`, the
statistics `bench-e2e` uses, seeded from `--seed`, `0` by default), rest on those rounds alone, and
`pairs` counts them. The other rounds say what they can and no more:

* If only the numerator's run did not finish, the round gives a *lower bound* on its ratio (the
  least it took over the denominator's time). That can show a version slower and never not slower.
* If only the denominator's run did not finish, it gives an *upper bound*. That can show a version
  not slower and never slower.
* If neither finished, the round says nothing. It is counted (`both_censored`) and never made a
  ratio, so two runs that both ran to the bound are not a ratio of 1.0.

**Giving up.** A run that cannot finish costs the whole bound each time. After two measured runs in
a row (`GIVE_UP_AFTER`) of one kind of run of a version (its headless scan, or one of its interface
runs) in which a run did not finish, that kind is not run again for the version, and the rounds it
misses are recorded as skipped. The warm-up round runs but does not count towards it, so a version
that is given up on has two runs that ran out of time on record. A skipped round is neither a
sample nor a pair, and a version given up on in its interface runs keeps its headless samples, and
the other way round. A run that could not be carried out at all (a spawn or isolation failure) also
ends its kind of run for that version, but is no skipped round: its check is `errored`, and the
cells that need it say why.

**What the rounds decide.** The policy is `bench-e2e`'s, applied to the rounds in which both sides
finished, with what the other rounds can say added:

* A version is `affected` when the median of at least three such rounds (`MIN_ROUNDS`) is more than
  20% above one (the `timing_ab_regression` budget), its interval lies above one, and no round in
  which the candidate's run did not finish could lower it; or when at least two rounds
  (`MIN_CENSORED`) in which its own run did not finish and the candidate's did have a lower bound
  over the same 20%. One such round is a slow moment, and two are not.
* It is `not-affected` when the median of at least three such rounds is at most one, and no round
  in which its own run did not finish could raise it; or when at least two rounds in which the
  candidate's run did not finish and its own did have an upper bound of at most one, and no round
  in which its own run did not finish.
* Rounds that say slower beside rounds that say not slower settle nothing.
* Anything else is `not-measurable`: fewer than three rounds that finished on both sides with
  nothing else to settle it, a ratio between the two thresholds, a median that rounds in which a
  run did not finish could overturn, or runs that ran out of time on both sides.

A single run is never compared.

**The table.** `sweep/defects.rs` lists the rows: the product defects fixed in the release, each
tied by the words it opens with to its entry in `CHANGELOG.md` newer than v1.3.0 (a test holds
every row to a line that is there, and no two rows to the same one). The ids are the numbers of the
validation program's findings, so there is no row for F17 (withdrawn), F20 (accepted and not
fixed), F25 (introduced after v1.3.0 by unreleased work), F26 (deferred past the release), or F23a
and F27 to F30 (the harness, not the product).

| Row | Defect | Decided by | `affected` when |
|---|---|---|---|
| F1 | Per-run durable writes dominate scanning | `headless_wall_ms` (the whole command); the value also gives the ratio to `du -sk` | slower than the candidate (see **The paired timing**) |
| F2 | Scan ingestion stalls when the terminal reads slower than excise writes | `tui_complete_ms` at 150,000 bytes a second, `default` and `reduced-motion` | slower than the candidate in either mode (the value also gives default over reduced motion, over the rounds in which both finished) |
| F3 | Idle animation loop | `idle` | any byte of output, or more than 50 ms of CPU, in the window |
| F4 | Leftover scan-store directories | `kill-restart` | something the kill left is still there after the second scan |
| F5 | Selection drift after COMPLETE | `selection-drift` (full tier) | reproduced: after `COMPLETE` the pane names another entry than before in at least one attempt; never `not-affected` |
| F6 | Signals skip all cleanup | `signal-*` | a signal that is not a confirmed quit: not exit 130, the terminal not restored, or something left behind (SIGTERM and SIGHUP on macOS; SIGQUIT too on the other Unix systems; the console close on Windows) |
| F7 | The macOS scan-store quota is about 256x too large | `headless-oracle` | the scan-store limit the report states is larger than the free space of the volume (a version-1 report has no scan store: `not-affected`) |
| F8 | Windows owner loop stalls after every key release | not measurable | |
| F9 | A deletion can leave an empty file under a deleted file's name, and the folder survives | not measurable | |
| F10 | Unreadable folders, and every folder above them, reported complete with exact bounds | `headless-oracle` on `hostile-small` | an `entry-state` discrepancy |
| F11 | Nothing deeper than PATH_MAX is scanned | `headless-oracle` on `deep-past-path-max` | a `missing` discrepancy |
| F12 | A full scratch volume reported as a generic failure | not measurable | |
| F13 | Open descriptors grow with the tree | `descriptors` (full tier) | the large tree held more than 24 descriptors beyond the small one |
| F14 | The deletion-history export blocks the interface for seconds after a large deletion | not measurable | |
| F15 | On Windows a scan can end on "scan results unavailable" instead of COMPLETE | not measurable | |
| F16 | The headless report is written unbuffered | `report_write_ms` (a scan whose report was seen to change in length fewer than twice gives no sample) | slower than the candidate (see **The paired timing**) |
| F18 | The interface reaches COMPLETE 1.2-2.4x later than headless | `tui_complete_ms` over the same version's `headless_scan_ms` of the same round: its headless scan without the writing of its report (see **The report write**) | over the 1.25 budget (`tui_complete_ratio`), by the rules of **The paired timing** with 1.25 in place of 1.2 and of 1; `not-affected` at or under it. A round in which the write could not be told apart from the scan gives no pair, so a version with too few such rounds is `not-measurable`, and the cell says in how many of how many rounds it could not |
| F19 | A folder with a path longer than PATH_MAX cannot be deleted | not measurable | |
| F21 | On macOS deletion planning panics on a device node with major 128 or more, or a negative size | not measurable | |
| F22 | A filter whose matches lie more than one level below the current folder crashes the program | `filter` | the program ended after the filter, at the root or inside `victim`, whatever the other variant did, also when it did not run; `not-affected` only when both variants ran and the program survived both |
| F23b | The untouched cursor does not land on the largest entry at COMPLETE | `filter` (the pane before any key) | the pane names another entry, or none, on any platform. A pane on the largest entry is `not-affected` on Windows only, and `not-measurable` on macOS and Linux (see **F23b**) |
| F24 | On Windows closing the console can end with an NTSTATUS instead of exit code 130 | `signal-close`, on Windows | the exit code is not 130 |

**F23b.** The check reads the selected-item pane at `COMPLETE` with no key pressed, on
`delete-folder`, and holds the entry it names to the largest entry of the fixture's oracle
(`victim`). A pane on another entry, or on none, is positive evidence of the defect on any
platform: `affected`. A pane on the largest entry is `not-affected` on Windows only. The defect is a
cursor that stays on a small entry the scan listed on its first page while much larger ones
appeared (the changelog: "The map kept the entry it selected on the scan's first page selected by
name for as long as that entry was listed, so a small file found early kept the cursor after much
larger entries appeared"), so it needs a small entry to be listed before the largest one, and the
order in which a file system lists a folder is its own. The program found the defect on Windows,
which lists a folder in name order, so `keep-a.bin` comes before `victim`. The file systems of
macOS and Linux list in an order of their own, and the check reads only the pane at `COMPLETE`: it
does not see the listing order. Whether the check exercises the mechanism there depends on that
order, so a clean reading is a claim about the platform it was read on and does not show the defect
absent. The cell is then `not-measurable`, names the platform (`... does not show the defect absent
on macos: ...`), and says that a cursor stuck on a small entry would be `affected` on any platform.

**Cells.** `affected` and `not-affected` come only from a check that ran, and a `not-measurable`
cell always gives its reason (`HarnessSweep::check` refuses one that does not). The candidate is
the reference: its cell in a timing row (F1, F2, F16) is `not-affected` by definition. In every
other row it is held to the same rule as the rest, and a row whose candidate cell is `affected`
carries a note that the candidate shows the defect too, so the run does not show it fixed. A cell
gives the rule that decided it (`reason`), what was measured (`value`: for a timing, for example
`14x the candidate (95% CI 12.0-15.0, 5 rounds)`, the rounds being those in which both sides
finished, followed by what the others show), and where the evidence is (`evidence`). When a timing
cell rests on runs that did not finish, its reason quotes them: how many runs a version made, how
many did not finish within the bound, in how many of those rounds the candidate's run did, and how
many rounds were skipped (`2 of its 2 measured runs did not finish within the bound, 2 of them in
rounds where the candidate's run did (the candidate finished 5 of its 5 runs); 3 rounds skipped`). A
cell is a claim about the platform the sweep ran on: where a clean reading shows a defect absent on
one platform only, the cell says so and is `not-measurable` elsewhere (see **F23b**).

| A cell is `not-measurable` with the reason | Rows | What would measure it |
|---|---|---|
| `the build failed: ...` | every row that needs the binary, on a version whose build failed or whose binary does not answer `--help` | A build that works; the log is `builds/<label>.log`. A build that does not finish within 60 minutes fails with `timed out after 60 minutes and was killed`. |
| `no confirmation is sent to a build without frame marks` | F9, F14, F19, F21, on every version | A confirmed deletion, which only a scenario's `delete` step sends, to a build that marks its frames: `cargo xtask e2e` runs those against the current build. |
| `needs a scratch volume that fills during the scan, ...` | F12 | A privileged attached volume: `EXCISE_HARNESS_PRIVILEGED=1`, which AGENTS.md allows only for a task about volumes. The cell names `EXCISE_HARNESS_PRIVILEGED=1 cargo xtask e2e --scenario scan-store-quota`, the closest check there is; it runs the current build. |
| `Windows only (...); this sweep ran on <os>. ...` | F8 (the key-release events of the Windows console) and F15 (another program holding a scan-store file) on every platform; F24 off Windows | A run on Windows, which measures F24 (`signal-close`); F8 and F15 need what a sweep does not do, and the cell sends the row to a Windows run. |
| `the reproduction needs the 20,053-entry selection-drift fixture, which only the full tier runs: use cargo xtask sweep --full`, and the same for the 49,050-entry `tiny-files-50k` | F5, F13, in the quick tier | `--full`. |
| `not reproduced in N attempts (the precondition held in P; the scan reached COMPLETE after it in C and did not in I, where nothing was read after COMPLETE) ...` | F5 | Nothing: the defect was observed once and does not reproduce on demand, so a run that does not reproduce it does not show a version unaffected, and an attempt that never reached `COMPLETE` shows neither the drift nor its absence. A run that reproduces it is `affected`. |
| `inconclusive: ... more rounds would settle it` | F1, F2, F16, F18 | More rounds (`--rounds`): at least three rounds finished on both sides, but the ratio is not more than 20% above one with an interval above one, and not at or below one (F18: over the budget, but its interval reaches it). |
| `N rounds finished on both sides, which is too few to settle it: ...` | F1, F2, F16, F18 | More rounds, or runs that finish: a ratio needs at least three rounds in which both sides finished, and rounds in which a run did not finish settle a cell only when at least two show it. The reason adds the rounds in which neither side finished, which say nothing. |
| `the candidate's run did not finish in N rounds in which its run did, and those rounds are left out of the median, which they could lower`, `its run did not finish in N rounds in which the candidate's run did, which says it is slower there and keeps the other rounds from showing it is not`, `the rounds disagree: ...` | F1, F2, F16, F18 | Runs that finish: the median leaves out the rounds in which a run did not finish, and those could move it the other way, or some rounds say slower and others say not slower. |
| `... was not timed in this run`, `the interface was not timed to COMPLETE without a slow terminal in this run`, `its runs could not be carried out: ...`, `no round of it produced a timing`, `no round has a candidate timing to compare with`, `no round gave a headless scan time to hold the interface to: ...`, `no round has both a headless scan time and an interface time` | F1, F2, F16, F18 | `node-modules-2k` among the `--fixture` fixtures, or runs that succeed (an `errored` check says why they did not); for F16 and F18, scans whose report is seen to grow (see **The report write**). |
| `fixture ... was not run in this sweep`, `the build's --help lists no --format and --output, so it has no headless scan`, `the scan of ... wrote no report: ...`, `the report of ... could not be read: ...` | F10, F11 | The fixture among the `--fixture` fixtures; a build that takes the flags; a scan that finishes within `--timeout`. |
| `no headless report was read, so there is no scan-store limit to read`, `the platform cannot say how much space is free on the scratch volume` | F7 | A readable report; a platform that can say (the sweep reads the free space of a volume on Unix only). |
| a check that did not run: `the header did not read COMPLETE within ...`, `the program ended before COMPLETE`, `the program ended while it was left alone`, `the program ended during the idle window`, `the large tree did not reach COMPLETE within the bound`, `the selected-item pane was not found on the screen`, `the fixture's largest entry could not be worked out`, `the signals that ran were clean, but not every one ran: SIGHUP: <why>` (on macOS every F6 reason ends with the sentence that SIGQUIT is not sent) | F3, F4, F5, F6, F13, F22, F23b | A longer `--timeout`, or a build that gets as far as the check. |
| `neither filter ran: at the root: <why>; inside victim: <why>`, `only one of the two filters ran: the filter <place> did not run (<why>), so the program surviving the other does not show the version unaffected` | F22 | A build that gets as far as both variants of the filter (the notes of the `filter` check say which did not run, and why). A program that ended after the filter in either variant is `affected` whatever the other did. |
| `the report write could not be told apart from the scan in N of the M rounds the scan was timed in (the report was seen to change in size fewer than twice, or its write left no time for a scan), so those rounds give no scan time without the report` | F18 (added to the reason of a `not-measurable` cell, and to the value of the others) | Scans whose report is seen to change in size at least twice, with a write that leaves at least 1 ms for the scan; for a scan that did not finish, a longer `--timeout`. |
| `the selected-item pane shows `victim`, the largest entry, at COMPLETE, which does not show the defect absent on <os>: ...` | F23b, on macOS and Linux | A run on Windows, which lists a folder in name order (`keep-a.bin` before `victim`). The check does not see the listing order elsewhere. |

**Output.** A run writes `target/excise-sweep/<run-id>/` (`target` is `CARGO_TARGET_DIR` when that
is set), where the run id is the UTC start time and the process id (`20261006T091500Z-31337`), and
`target/excise-sweep/latest` points at the newest run (a symbolic link on Unix, a text file holding
the run id elsewhere). The directory is made before the first build, so that the build logs live in
it. Scratch areas are made below `EXCISE_E2E_TMPDIR` (`/tmp` on Unix when it is not set), as for
the other runners, and are removed as each check ends. The fixtures are the cached masters in the
fixture cache below the target directory, which the checks only read. The run directory holds:

```text
sweep.json                   the harness-sweep document
table.txt                    the grid, then the detail behind every cell
builds/<label>.log           cargo's output of each ref built in this run
evidence/<label>/<name>.txt  one file per check; <name> is <check>[-<fixture>][-<profile>]
```

An evidence file holds the observation in full and the last screen the check saw (for an oracle
scan, the first discrepancies of the diff; for the filter check, a line and the final screen of each
variant, those that did not run included).

`<label>` names a version's files, and the ref as it was typed never does. It is the ref with every
character outside `A-Za-z0-9._-` replaced by `_` (a non-ASCII one too, and a leading `.` or `-`),
cut to 64 characters, then `-` and the first 12 characters of the commit: `v1.3.0` at a commit that
begins `0123456789ab` is `v1.3.0-0123456789ab`, and one of its evidence files is
`evidence/v1.3.0-0123456789ab/headless-oracle-hostile-small-default.txt`. A label is never empty,
never begins with `.` or `-`, and holds no separator, so no ref (`feature/x`, `a/../b`,
`v1.4.0^{/fix}`) can make a subdirectory, leave the run directory, or hide a file. `xtask` and the
harness make it with one function, `excise_harness::sweep::label`, so the log and the evidence of a
version share it. Two versions whose labels are the same, with letters compared without case
(`release/1.0` and `release_1.0` at one commit, or `Main` and `main`), would keep their files under
one name and are refused before any fixture is prepared: the command exits 1 with no table, after
the builds, which stay cached. Name one of them by its commit.

`table.txt` lists the builds (commit, toolchain, built or cached or failed) and then, for every
defect, its changelog entry, how it is measured, and every cell with its `value`, `why`, and
`evidence`. The command prints the grid on stdout (`AFF` affected, `ok` not affected, `n/m` not
measurable, then the title of every defect and a legend), then the lines `table:` and `document:`
with the two paths; its progress and its problems go to stderr.

**What it leaves behind.** By the time the command returns, whether it finished, failed, or stopped
on a changed fixture, the scratch areas and fixture copies it made (below `EXCISE_E2E_TMPDIR`, or
`/tmp` on Unix) are removed, on every platform, and so is the worktree of every build: the
directory that held them, `target/excise-sweep-worktrees/`, is left empty. What stays is what it is
for: the run directory, the cached builds (`target/excise-sweep-builds/`), and the cached fixtures
(`target/excise-fixtures.noindex/`). A sweep that is killed (Ctrl-C, `SIGKILL`, power loss) runs no
cleanup. It leaves the scratch areas (`xh-scratch-*`) and fixture copies
(`<fixture>-<process id>-<number>`) it was using, at most one build worktree, and a build that was
running, which goes on until it ends (see **Bounds**). The signals it sends (see **Signals**) leave
nothing on macOS, where none of them makes the system write a crash report, and no core file on
Linux, except on a machine that pipes core dumps to a handler, whose files are its own. A build that
crashes by itself can still make the operating system write a crash report: on macOS into the
person's own `~/Library/Logs/DiagnosticReports`, which the sweep neither prevents nor deletes. It
deletes nothing in the person's home folder.

**Exit status.** The command exits 0 when every build was made and every check could be carried
out. It exits 1 when a build failed or a check was `errored`, and still writes and prints the
table, because a version without a binary is a column of `not-measurable` cells; each problem is
named on stderr (`problem: ...`). It exits 1 without a table when the command line is not valid
(the usage line follows the message), when a ref does not resolve or a pinned toolchain is
missing (before the first build), when a fixture cannot be made or does not exist, when a fixture
is not what its plan says or a build or a check changed one (see **No deletion, ever**), when two
versions would keep their files under one name (see **Output**), and when the output cannot be
written. A check that did not run (`not-run`) is not a problem: it leaves `not-measurable` cells
that give its reason.

**The document.** `sweep.json` is a `harness-sweep` document (see
[Output documents](#output-documents)). `versions` are the columns: the ref, its commit, the
toolchain, the SHA-256 of the binary, how the build went, and what its `--help` lists.
`measurements` are the paired timings, one for each metric (`headless_wall_ms`, `report_write_ms`,
`headless_scan_ms`, and `tui_complete_ms`), fixture, profile, and terminal speed: the `rounds` that
were planned, the versions that ran in each round in the order they ran (`order`), `du`'s time in
each round (`du_samples`, for the scan's wall time only: one entry for each round, `null` for a
round it could not be timed in, and absent when it could not be timed in any), and a `series` for
each version that ran. A series has the `rounds` its samples were taken in (counted from 0, after
the warm-up; a round in which the run had no usable reading is not among them), the `samples` in
milliseconds, whether each `completed` within the bound, how many rounds were `skipped`, the
`median` of the samples that completed, and its `ratios`, by what each is taken against:
`candidate`, `du` (a scan over `du -sk`), `headless-scan` (an interface run over the headless scan
of the same version without its report write, in the same round), and `reduced-motion` (default
over reduced motion, against the slow terminal, for the same version in the same round). A ratio
has `pairs`, the rounds in which both sides finished, and, when there are any, their `median` and
its `bootstrap_ci`; `lower_bounds` and `upper_bounds`, one for each round in which only the
numerator's or only the denominator's run did not finish; and `both_censored`, how many rounds
neither side finished in. `checks` are the observations of every other check on every version
(`ran`, `not-run`, or `errored`, with its metrics, notes, and evidence file), `rows` is the table,
and `context` is the conditions the run had: the host, the load average at the start and the end,
the other `excise` processes found running (the run warns about them, as `bench-e2e` does, so close
them first), the commit of the checkout, the rounds, the seed, the rate of the slow terminal, the
timeout, and the fixtures. A cell's `evidence` lists paths below the run directory and JSON
pointers into the document (`#/checks/12`, `#/measurements/0`).

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

## Interactive driver

`cargo xtask tui` drives one live `excise` session on a fixture, step by step, so that a person or
an agent can explore the interface and then turn what they saw into a scenario. `excise_harness::tui`
does the work; `xtask/src/tui.rs` only parses the command line, finds or builds the release binary,
and prints the one document the command made. It reuses the harness's own parts and forks none of
them: the fixture generator and its run copies, the isolated environment, `PtySession` with its
screen model, the scenario key encoding, the asciicast recorder, the event reader, and, shared with
the scenario runner in `runner::live`, the wait that `settle` performs and the whole protocol of the
`delete` step.

```console
cargo xtask tui open --fixture delete-file [--profile NAME] [--size COLSxROWS] [--record] [--idle-timeout DURATION]
cargo xtask tui keys SESSION KEY... [--timeout DURATION]
cargo xtask tui delete SESSION --name PATH --kind folder|file [--timeout DURATION]
cargo xtask tui screen SESSION
cargo xtask tui events SESSION [--since N]
cargo xtask tui close SESSION
cargo xtask tui list
```

Every command prints exactly one [`harness-tui`](#output-documents) document on stdout and nothing
else there. A failure is a document too (`ok` is `false` and `error.kind` says why). The status is
0 on success, 2 for a command line that is not valid, and 1 for any other failure. A duration is
written `250ms`, `30s`, `15m`, or `2h`.

| Command | What it does |
|---|---|
| `open` | Makes a run copy of the fixture spec `--fixture` names (the id of a file in `fixtures/`, never a path; a spec that needs a volume is refused), starts `excise` on it in a pseudo-terminal with the profile's settings and the event channel, waits for the first frame, and prints the session id and the first screen. The scan is usually still running then. The size is 120x40 unless a profile fixes the width; `--record` writes an asciicast; the idle timeout is 15 minutes (1 s to 24 h). |
| `keys` | Sends keys in order: the scenario key names (`enter`, `esc`, `backspace`, `tab`, the arrows, `page_up`, `page_down`, or one character, with `ctrl+` or `alt+` before it) and `type:TEXT` for text. It waits for a frame that counts every input sent, as `settle` does, and prints the screen and the events since the previous command. `settled: false` means no frame counted them within `--timeout` (2 s): a key that changes nothing draws none. |
| `delete` | The protocol of the `delete` step for `--name` (a path from the fixture root) and `--kind`: it selects the entry through the filter, presses Backspace, verifies the dialog against exactly that entry, kind, and path and against every sentinel, and only then confirms, then waits for the deletion, and for the rebuilt map as long as the timeout allows (`--timeout` bounds all of it, 60 s by default; the `screen` of the document says whether the map was rebuilt). The filter selects among the entries of the folder the session shows, so an entry elsewhere fails at once with `not_in_view` and nothing is sent: navigate first (`esc` goes up a folder; a folder opens when it is selected with `/`, `type:NAME`, `enter`, and `enter` is pressed again). The sentinels are the entries of the fixture's first two levels (at most 64) that are neither the target nor inside or above it; a fixture with none refuses the deletion. Afterwards the fixture is compared with its state before the first deletion, and a change that no confirmed deletion explains fails the command with `fixture_changed`. A wait that runs out is a failure document whose `confirmed` says whether the confirmation key was sent; the session stays open, and after a confirmation the program is still deleting, so read `screen` or `events`, or `close` it. |
| `screen` | The screen model now: text, cursor, terminal modes, header badge, the selected entry, the filter prompt, the boxes, and the dialog. |
| `events` | The event channel's records from `--since` (all of them when it is not given), frames included. |
| `close` | Quits the program the way a user does (`q`, then the key the quit dialog offers) and kills its process group if it has not ended within a bounded wait. Prints the exit status, whether the terminal modes were restored, the fixture's changes, what the program left in its scratch area, and whether everything was removed. With `--record` the asciicast is kept and its path printed. |
| `list` | The open sessions. A session whose supervisor died is reported under `stale` and cleaned. |

**Sessions.** One supervisor process per session owns the pseudo-terminal, the screen model, and
the event reader, so a session outlives the command that opened it. The commands talk to it through
files in `target/excise-tui/<id>/`, never through a socket or a port, so nothing is reachable from
another machine: a request is a file in `req/`, and its reply is the file of the same name in
`rep/`. The directory is private to its owner (mode `0700`; a directory that is not is made so, or
refused). The supervisor is `xtask` itself, started by `open` as `xtask tui supervise <session
directory>` in its own process group with no terminal; its standard error is `supervisor.log` in the
session directory. A session ends on `close`, when the program ends, and after its idle timeout, and
each time the supervisor removes the run copy, the scratch area, and the session directory (a
recording is kept). The run copy lives in `xh-tui-<id>/` under the base the scenario runner uses
(`EXCISE_E2E_TMPDIR`, or `/tmp`), because the deletion dialog is at most 78 columns wide. An `open`
that fails, or that is not ready after 15 minutes, ends its supervisor and removes everything it
made. The supervisor serves one command at a time: a command that gets no reply in time withdraws
its request if the supervisor has not taken it, so that nothing runs after it reported the timeout,
and says so when the supervisor had taken it. A recording is large (7 MB for a session of 12
seconds on the smallest fixture, because the program redraws constantly while it animates), so
record only a session you mean to keep.

**A session that does not clean up.** The command that reads a session's last reply (`close`, a
`keys` whose key quit the program, a command that found the program gone) waits up to 15 s for the
supervisor to remove the session directory. If it is still there, the document says so and `list`
cleans it: `close` in its `cleanup`, and every other reply as a failure that keeps the exit and the
screen, so a session that did not clean up is never reported as one that did.

**Stale sessions.** The supervisor holds an exclusive lock on `supervisor.lock` for as long as it
lives, so a free lock means it is gone, with no process id to reuse. `open`, `list`, and every
command that names a session look for such sessions and clean them. Cleaning kills and removes
things, so it acts only on what it can prove is the session's: a process id goes to another
process once its process ends, and anyone can run a tool against the path `open` printed.

- `open` makes a random mark for the session (its *nonce*) and records it in `config.json` before
  the supervisor exists. The supervisor puts it in the program's environment (`XH_TUI_SESSION`)
  and in the name of the marker of the workspace, and, once the program shows the mark in its
  environment, records the program's process id with its start time and executable in
  `state.json`. A process that is still being started shows its parent's command line and
  environment, or none, so a reading made earlier would describe the wrong process.
- The program of a stale session is killed, with its process group when it leads one, only if the
  system shows the mark in its environment and nothing recorded of the process contradicts that;
  it is looked at again after its group is known and immediately before the signal. The recorded
  identity only rules a process out, because a start time in whole seconds and a path are shared by
  another process that takes the id in the same second: a process with another start time,
  another executable, or an environment without the mark is left alone. A program the supervisor
  never got to record is found by the mark among the processes that have the run copy in their
  arguments. A process whose environment the system does not show is left alone, whatever was
  recorded of it, and so is the session: it is reported under `stale` with `cleaned: false` and
  the reason, and `list` reports it again.
- A workspace is removed only if it holds the marker of the session and its nonce: an empty file
  whose name holds both, so it is there whole or not at all, whenever its maker ends. The marker is
  the last thing in the workspace to go, and `config.json` is the last thing in the session
  directory, so a removal that was interrupted leaves what the next sweep needs to finish it (an
  empty workspace with no marker, which a supervisor killed between making and marking it leaves,
  goes after a minute). A directory in `target/excise-tui/` that is named like a session but
  whose configuration is missing, unreadable, or names another session is left and reported; only
  the skeleton that a killed `open` leaves is removed (the request and reply directories, empty,
  either or both, and the temporary file the configuration is written through). Every command
  refuses a `target/excise-tui` that is a link, and a session never takes an id whose recording is
  still there, so a kept recording is never written over.

**Events.** `open`, `keys`, `delete`, and `close` do not list every frame since the previous
command, because a program that animates draws about twenty a second. They summarize the frames
(`events.frames`: how many, the time of the first and of the last, and the last frame record in
full) and list every other record in full and in order. `events.since` and `events.next` still
cover the whole run, and `events --since N` returns every record, frames included.

**Selecting.** The program keeps the text of the filter: `/` opens it with the text of the filter
before, and `keys` types after that text, so erase it with `backspace` first (the `screen`
document's `filter.input` shows what is there). A filter that is applied again unchanged selects
nothing. `delete` takes the entry the session has selected, and otherwise clears the filter and
selects the entry through it, so neither state makes it fail. A later deletion in a session is
checked against sentinels that are still there: an entry an earlier deletion removed is no
sentinel.

**Reading the screen.** `keys` reads the screen once the frame that counts the keys is on it, which
the program's frame marks say exactly on the Unix pseudo-terminals the driver runs on
(`SCREEN_IS_EXACT`, see `settle`), and then once the output has been quiet for 25 ms (for at most
250 ms), because a program goes on drawing after a key. What the screen shows about the keys
themselves is therefore exact; what the program drew later may not be, so read `screen` again when
it does not show what the keys should have done. A program that does not mark its frames gets only
the quiet read, and `keys` then sends it no key that could confirm a deletion.

**Safety.** Confirmations are sent only by `delete`. `keys` refuses `y`, `enter`, and every other
key that could confirm a deletion dialog while one is shown, and until the screen is known to show
what the program has read. What could confirm one is read from every byte the session has written,
in order, across commands (`pty::input::InputScan`: the keys, the barrier requests, and the
driver's own writes), because the program joins the bytes of an escape sequence across writes:
`alt+[` and then `type:121u` are `ESC [ 121 u`, which it reads as `y`, with no `y`, Enter, or line
feed in any key. A key that begins a sequence is sent, as nothing can be confirmed with it yet
(`alt+[` and `alt+O` do, and an `esc` that the next `[` or `O` turns into one); a key that
continues one counts as a confirmation whatever its bytes. Behind `ESC [` or `ESC O` it is refused
with the error kind `refused`, as no input barrier can be written behind it (the program takes the
request for a byte of the sequence and never answers it), and so is every later key: close the
session and open another. Behind a lone `esc` it is guarded as any key that could confirm. Before
such a key it writes an input barrier request behind every key sent so far and waits for the
program to answer it and for the screen to show the frame that answers it, so the program has read
them all, however the terminal cut their bytes into input events
(`alt+backspace` can reach a program as `esc` and then `backspace`), and the screen shows what they
did. A dialog opens on an input and never by itself, so a screen that shows that frame shows every
dialog there is, however slowly the terminal delivers it, and nothing is queued behind the request;
the clock decides nothing. A key that changes nothing draws no frame and needs none, because the
request makes one. When the answer does not come, and show on the screen, within the timeout, the
key is refused. On a terminal where the screen is not exact `delete` and this guard refuse before
any key, with the error kind `refused` (the driver does not run on Windows; the tests check it
against a terminal that paints on its own timer). For the same reason `keys` refuses such a key in a
command that sent `backspace` before it: `keys backspace enter` is refused, and so is erasing a
filter's text and applying it in one command, because nobody has read the dialog Backspace opens. A
confirmation behind the escape byte (`alt+y`, `alt+enter`) is one input that a program may read as
two, so the escape could close a prompt that covers a deletion dialog and the key confirm the dialog
it uncovers with no request possible between them: it is refused always, and `esc` and the key are
sent as two keys. A program that does not mark its frames or does not answer an input barrier says
nothing about what it has read, so `keys` sends it no key that could confirm a deletion, and
`delete` refuses it before any key is sent. `ctrl+]` is not a key (its byte is the request). `delete`
refuses the ownership marker `.excise-harness-owned`, and anything inside it, as a target, and checks
the marker once more right before the confirmation key. `delete` waits for a deletion that an earlier
command confirmed and gave up waiting for, because the program can queue another behind it and the
`deletion_finished` it waits for has to be its own; a failure's `confirmed` is true when the
confirmation key was attempted and nothing says that it did not arrive. The run copy carries the
ownership marker, and every runner refuses a root without it.
Every wait is bounded. The program's process group is killed when a wait that the session cannot go
on without runs out (the first frame, the quit, the end of the session); a `keys` or a `delete`
that runs out reports it and leaves the session open.

**Platforms.** macOS and Linux. On Windows the module builds, and every command fails with a
document whose `error.kind` is `unsupported_platform` and names the platform: the driver needs Unix
process groups and file locks.

**Tests.** `xtask/tests/tui.rs` runs the real commands against the `excise` binary `cargo test`
built: a session from `open` to `close`, a deletion with every sentinel surviving, a folder
deletion, two deletions in one session after a selection and a cancelled dialog, `delete` on an
entry outside the open folder, a confirmation key refused in a real deletion dialog and in the
command that opens it, a quit through `keys`, an idle timeout, a killed supervisor reported stale
and cleaned, a stale session whose program is found by its mark when its identity was not
recorded, a stale cleanup that leaves alone the process that took over a recorded id, a
confirmation behind the escape byte refused while a deletion waits behind the quit prompt, text
that would finish an escape sequence refused behind a deletion dialog, a state
directory that is a link that no command follows, the cleanup of a test, which signals no process
because a state file names its id, a long idle between commands, two sessions side by side, an
`open` whose program ends at once, and command lines that are not valid, an argument that is not
text among them. Each test has its own state,
work directory, and fixture copy, so they are safe in parallel, and each checks that no process,
session directory, or run copy is left. Every document every test sees is validated against the
schema and read back by the harness's reader. A test waits for what a key does by reading `screen`
until it shows it, because the program can draw a frame after the one that counts a key. Two tests
put the screen behind the program with a pseudo-terminal that drains 20,000 bytes a second, set
through the variable `EXCISE_HARNESS_TUI_DRAIN_BYTES_PER_SEC`, which the supervisor reads when a
session starts and which is not an option of any command: one refuses a confirmation after the keys
that uncover a dialog and after a Backspace more than a second old, and one deletes exactly its
target.

## Shape profiles

```console
excise-shape profile <ROOT> [--output FILE] [--cross-filesystems]
excise-shape spec <PROFILE> --id ID --entries N [--seed S] [--max-file-bytes BYTES] [--output FILE]
```

`excise-shape` is the binary of this crate (`src/bin/excise-shape.rs`; the code is
`excise_harness::shape`). It exists so that benchmarks and scenarios can behave like a tree you
care about, such as a home directory, without anyone running Excise on it: `profile` measures the
*shape* of a tree as aggregates only, and `spec` turns a profile into a fixture specification that
the generator builds at any size. Install it from a checkout with
`cargo install --path crates/excise-harness --bin excise-shape`, or from the repository with
`cargo install --locked --git https://github.com/findyourexit/excise excise-harness --bin excise-shape`
(`publish = false` does not stop an install from Git; a binary target is all it needs).
`excise-shape help profile` and `excise-shape help spec` print the details below.

**Local only.** A profile of a real tree describes it, in aggregate, and stays private to its
owner until the owner shares it: keep it out of version control. `target/` is ignored by Git, so
`target/excise-profiles/` for profiles and `target/excise-specs/` for the specs built from them are
the places for them. Nothing here is committed or published, and the specs in `fixtures/` are
built from no real tree. The [performance report form](../../.github/ISSUE_TEMPLATE/performance_report.yml)
asks for a profile of a slow tree, and nothing else of the tree, for the same reason. What
`--output` makes, and what the file gets, is under [`profile`](#profile).

**Safety.** This repository's tests, scripts, and agents never run `excise-shape` on a real path
(`~`, a project, a mounted volume): they profile fixtures the harness generated and scratch trees a
test made. A profile of a real home is its owner's to make, and an agent may build specs from a
profile it is given. The walk is safe to run on a real tree, because it only reads (below), and
this repository's own rule is stricter than that.

### `profile`

`profile` walks `ROOT` and writes a `harness-shape-profile` document ([below](#the-profile-document))
to standard output, or to `--output FILE`. A summary of counts goes to standard error. It exits 0
when it wrote the profile, 1 when it could not (the root cannot be opened or listed, or `FILE`
cannot be made), and 2 for a command line it cannot read. No message of it names `ROOT` or
anything below it: an error says that the root cannot be opened or listed, and why, and never
where, and an output that exists is refused without being named, because it can be below `ROOT`.

- **The root.** The path of `ROOT` is resolved like any path you type: a link among its components
  is followed, as every program follows one. Its last component must be a folder itself, and not
  a link to one, on Unix and on Windows: on Unix it is opened without following a link, and on
  Windows it is opened as a reparse point and what was opened is looked at. A separator or a `.`
  after that last name makes no difference: `link/`, `link//`, and `link/.` are `link`, and are
  refused when it is a link, because a system resolves a link that a path names with a separator
  after it whatever the open asks for (POSIX resolves such a path as if a `.` were appended to it),
  so the walk takes the separators off before it opens the root; on Windows that is a junction
  written `link\`, or written `\\?\C:\...\link\.` in the verbatim form, where std keeps each `.`
  as a component of its own, one that ends the path too, and the walk takes those off as well. A
  path that is only a root, a drive, a share, or `.`, or that ends in `..`, has no last name to
  take them off, and is opened as it is. Nothing below `ROOT` is followed.
- **It only reads.** The walk writes nothing, opens no file, and reads no content. It asks for the
  metadata of each entry (`lstat`) through the handle of the folder that holds it, so how deep a
  folder is does not matter. `--output` makes its one new file only after the walk has ended, so a
  profile never counts its own output, wherever `FILE` is, `ROOT` included.
- **It follows no link below the root.** A symbolic link is an entry of its own, and the walk goes
  no further than the link: it neither enters it, nor reads where it points, nor asks whether the
  target exists. A call that follows a link leaves the tree, where it can start an automount or
  wait on a mount that does not answer, so the only question the walk puts about an entry is
  `lstat` of the entry itself, and about a folder it has opened, `fstat` of its handle. A profile
  therefore counts the links and says nothing of where they point: it has no count of links that
  dangle. A link whose target is a FIFO, a folder that cannot be searched, nothing, or itself is
  one link, and no error of the walk.
- **It trusts what it opened, as far as the platform lets it tell.** A folder is inspected when
  its parent is listed and opened later, and in between it can be replaced. On Unix the walk opens
  it without following a link, takes its device and inode from the handle it opened, and counts a
  folder that is not the one it inspected in `unreadable.errors`, with the entries that vanished,
  without listing it. On Windows it opens each folder as a reparse point and refuses a link or a
  junction where a folder was, and that is the whole of the check: stable Rust has no file ID to
  compare, and this crate has no `unsafe` code, so a folder replaced by another ordinary folder in
  that window is walked, and what it holds is counted. That is the tree changing under the walk,
  which no walk of a live tree prevents, and nothing outside the tree is reached through a link.
  The walk holds every folder it is inside open, with the right to list it and every sharing mode
  but delete: while it runs, none of those folders can be renamed, deleted, or replaced by a
  junction. A folder it has not opened yet is not held.
- **It stays on one file system.** A folder on another file system is counted as a folder, counted
  in `walk.mount_points_skipped`, and not entered, unless you pass `--cross-filesystems`. Where
  the platform reports no device and inode numbers (Windows), it can find no mount point and no
  hard link, and the profile says so in `platform.identity`.
- **It streams, with a few handles.** It keeps no entry of the tree. What the walk holds at one
  moment is three things and a few counters, and its memory grows with those three and with
  nothing else of the tree. All of it is in memory: the walk spills nothing to disk, so a tree for
  which any of them does not fit in what follows cannot be profiled.
  1. *The names of the folder being listed*, all of them at once, whatever they name: a folder is
     read to its end before its first entry is counted. Each name is dropped as its entry is
     counted, so the name of a file or a link does not outlive the listing. This grows with the
     widest folder, which is what costs: one of a million files with names of 24 bytes took 73 MB
     at its peak (7 MB of it the process's own; measured on macOS, with a debug build, on a
     scratch folder of that size), about 66 bytes an entry and more for longer names, while a
     million entries in folders of a thousand cost a thousand names at a time.
  2. *The names of the subfolders still to visit, in each folder on the current path* from `ROOT`
     to the folder being walked. A folder that has been listed keeps those names and no others
     until it has entered each subfolder: roughly 100 bytes a name (an estimate from the sizes of
     what holds it, not a measurement). The folders on a path each hold their own, so this grows
     with the subfolders that wait along a path, and not with the widest folder's alone: in a
     comb, a chain of folders that each hold many subfolders besides the next one of the chain,
     the folders waiting along the chain can approach the number of folders in the tree.
  3. *The identity of every file that has more than one name*, from the first of its names the
     walk meets until the walk ends: a device and an inode number, how many names the file system
     says it has, how many the walk met, and the class of its size. That is 40 bytes, and roughly
     60 with the slack of the tree they are kept in (an estimate from the sizes of the
     structures, not a measurement). Never a name: the walk keeps the file's identity, and counts
     its names. This grows with the number of such files, up to every file of the tree.

  Besides these it keeps the name of each folder on the current path, to open a folder it gave up
  the handle of again by name, and a fixed set of counters and histograms, which are bounded
  whatever the tree is. On Unix
  the walk keeps at most 32 folder handles open at once (`HANDLE_BUDGET`; a listing opens one more
  while it reads), however deep the tree is, down to 4,096 levels, where it stops entering folders.
  That is well below the 256 descriptors that are a process's soft limit in a macOS shell, so a
  chain of 300 folders is walked in full there. It keeps the handle of the root, which it opens the
  others again from, and of the folders it was in last, and a folder with no subfolder left lets go
  of its own. When it comes back to a folder whose handle it gave up, it opens it again by name from
  the nearest folder above it that it still holds, without following a link, and checks that every
  folder it opened on the way is the one it recorded (the same device and inode); one that is not
  is counted like a folder that changed, and what was left to enter below it is not walked. On
  Windows it holds every folder it is inside, as above, and has no budget.
- **It never stops at an entry it cannot read.** A folder it cannot list, an entry that vanishes
  before the walk reaches it, and an input or output error are counted (`unreadable`), and the
  walk goes on.
- **A hard-linked file keeps the class of its size.** The names of a file have one size, so the
  names of a group lie in one class of `file_sizes`, and the walk records the class (see
  `hard_links.group_sizes_by_file_size` below): that of the size the first name had when the walk
  met it. A name met with another size, because the file changed while the walk ran, is a file of
  another class, which `file_sizes` counts there, and is not a name of the group; so the names of
  a class's groups are always among the files of that class. A file is remembered by its device
  and inode and never by a name.
- **It keeps no name.** A name is measured, in bytes, and dropped. On Windows, where a name is
  UTF-16 that can hold a surrogate that is half of no pair, the walk reaches the entry by its
  native name and measures it as WTF-8: the length of its UTF-8 when it is valid, and 3 bytes for
  each such surrogate. The document has no field that could hold a name, a path, a link target, an
  owner, or a timestamp, and the types and the schema reject any other.

`--output FILE` makes a new file: `FILE` must not exist, it is never written over, and a link at
its name is refused. The message for a `FILE` that exists names no path, because `FILE` can be
below `ROOT`. Folders above `FILE` that already exist are taken as they are *when the file is
made*, after the walk, links included, as for any path you type: a link among them is followed,
and so is one that replaced a folder of the path while the walk ran, or in the moment between the
look at the path and the write, because the path is looked at only when the file is made. What is
never a link is what this makes: each folder that is missing is made on its own, one at a time and
never recursively, so that anything that appears at its name first, a link included, is refused,
and the file is created new, which refuses a link at its name. On Unix the file is readable and
writable by its owner only (mode 0600), and the folders it makes are 0700. On Windows the file and
those folders get the permissions of the folder they are made in: an owner-only ACL needs `unsafe`
code or a Windows security dependency, which this crate has neither of, and a profile is private
because of what it holds, not because of its permissions. This is `cli::write_new_file`; `spec
--output` uses it too.

### The profile document

Every histogram is `{ "count", "total", "max", "buckets" }`: how many values it counts, their sum
and their largest, and a table from the smallest value of a bucket to how many values fell in it.
What a bucket spans depends on the histogram: a *class* is `0` or a power of two and holds the
values from it up to the next (`4096` holds 4,096 to 8,191), and a *length* bucket is one name
length from 1 to 255 bytes, a longer name counted as 255.

| Field | What it holds |
|---|---|
| `document_kind`, `schema_version` | `harness-shape-profile` and `1`. |
| `platform` | `os`, and `identity`: whether the platform reported a device and an inode for every entry. |
| `walk` | `cross_filesystems`, and `mount_points_skipped`. |
| `entries` | `total`, `directories`, `files`, `symlinks`, and `others` (sockets, FIFOs, devices) below the root; the root itself is not counted. |
| `max_depth`, `depths` | The deepest level, and per depth from 1 the `directories`, `files`, `symlinks`, and `others` there, and the `bytes` of the regular files (apparent size, every name counted). |
| `children_per_directory`, `subdirectories_per_directory`, `files_per_directory` | Histograms in classes of what a folder directly holds, over the folders that could be listed. |
| `file_sizes` | A histogram in classes of the apparent size of every regular file, every name of a hard-linked file counted. |
| `name_lengths` | `directories`, `files`, and `symlinks`: histograms of length, in bytes. |
| `hard_links` | `groups` (files with a link count above 1, each counted once), `names` found, `incomplete_groups` (names outside the walk), `group_sizes`, a histogram in classes, and `group_sizes_by_file_size`: the same groups by the class of the size of their file, a table from the smallest value of a class to a histogram like `group_sizes`. The histograms of the classes add up to `group_sizes`, and the names of a class are at most the files that `file_sizes` counts in it. |
| `symbolic_links` | `count`: how many symbolic links there are. Nothing of where they point: the walk never asks. |
| `unreadable` | `directories` that could not be listed because of their permissions, and `errors` of any other kind: an entry that vanished, on Unix a folder that was not the one inspected when the walk opened it (Windows cannot tell, see [`profile`](#profile)), an input or output error, a folder deeper than the walk goes, or a folder that is its own ancestor. |

A profile that disagrees with itself (kinds that do not add up, buckets that do not hold the
histogram's count, a depth that is missing) is refused by `excise-shape spec`, and the schema
rejects a document with a member it does not declare. A real home profile is a few tens of
kilobytes at most, and a 35-entry fixture's is 2.5 KB.

### `spec`

`spec` reads a profile and writes a fixture specification (TOML) with one `shaped` part (see
[Fixtures](#spec-files)), scaled to `--entries N`: a home of millions of entries at 50,000 for a
quick run, or at 1,000,000 for a full one. `--id` is the id of the spec, and the file is named
`ID.toml`. `--seed` (default 1) is the spec's default seed, `--max-file-bytes` (default 16,384, at
most 1 GiB) cuts the size of a file because the generator writes every byte, and `--output` writes
the file as `profile` does. The same profile, `N`, and seed always give the same spec, and so the
same manifest hash.

- **The counts scale exactly.** The directories, files, and symbolic links at each depth are
  scaled by `N` over the profile's entries, with the method of largest remainders, so the spec
  plans exactly `N` entries, the part's root directory included, and the fixture holds one more,
  the marker. A profile scaled far down keeps its depth: every level keeps a folder, so `N` must be
  at least one more than the levels the spec keeps (the part's root directory is one entry), which
  are the profile's depth, or 32 if it is deeper because a spec holds no more levels, and at most
  10,100,000.
- **The shape does not scale.** What a folder holds, how large a file is, and how long a name is
  keep their histograms whatever `N` is: scaling a tree down means fewer folders, not smaller ones.
- **Links scale with their files.** Every symbolic link points at a file of the part, and none
  dangles: a profile says nothing of where a link pointed, because the walk never asks (a part with
  no file has nothing to point at, and its links point at nothing). Hard links
  are scaled class of file size by class, because the names of a group are one file and so lie
  in one class of the file sizes: the files a class had and the files the spec plans in it give
  the scale, and the groups the profile had in it are scaled by it, each size of group by its
  count, rounded like every other count, and the names of the class with them. Four groups of
  four names are four groups of four names again, and not groups of whatever size their class of
  sizes allows. The groups of a class are among the files of the class, so a group that the files
  of a class have no room for (a profile scaled down far) is left out, the largest first, and the
  spec still builds. A file whose other names lay outside the tree that was profiled has no
  group to be in, so it is a plain file, and the cut at `--max-file-bytes` puts every class above
  it in the class of the cut. The generator takes the names of a class's groups from the files of
  that class, so the histogram of file sizes is kept exactly and the seed decides which files are
  the names, not where the groups are.
- **A link points at its file by the shortest relative path:** the file's own name for a link
  beside it, and otherwise `..` up to the nearest folder the two share and the way down from
  there. A plan in which a target would still be longer than 1,023 bytes (macOS's limit, the lowest
  of the three) is refused, by `spec` before it writes a file and by the generator.
- **Some things are left out.** Sockets, FIFOs, devices, folders that could not be listed, and
  mount points cannot be generated, and a fixture with one could not be cached or removed like any
  other.

Keep the spec in a directory of your own and point a runner at it:

```console
excise-shape profile <ROOT> --output target/excise-profiles/home.json
excise-shape spec target/excise-profiles/home.json --id home-50k --entries 50000 --output target/excise-specs/home-50k.toml
cargo xtask headless --fixture-dir target/excise-specs --fixture home-50k
cargo xtask bench-e2e --baseline main --fixture-dir target/excise-specs --fixture home-50k
```

### How closely a built tree follows its profile

The round trip test builds a tree, profiles it, builds a spec for several sizes, generates it, and
profiles that. The generated tree agrees with the profile within these, which are the numbers the
test holds it to. They were set from the distances the test measures, with room to spare, and not
from what the generator could be made to pass.

| What | Held to |
|---|---|
| Entries | Exactly `N`, and the marker. |
| The share of folders, files, and links among the entries | Within 0.01. |
| The share of the entries at each depth | Within 0.005. |
| The histograms of what a folder holds | A total variation distance of at most 0.15: the zeros and the tail are there, and the classes between them move a little, because a level shares its entries among its folders in proportion to weights drawn from the histogram. |
| The histogram of file sizes | At most 0.02. |
| The histograms of name lengths | At most 0.03: a name starts with the digits that tell a folder's entries apart, so it is never shorter than they need. |
| Links | Every one points at a file of the generated tree: none dangles. |
| Groups of hard links, class of file size by class | The share of a class's files that are names of linked files within 0.03, with `1 / files` more for the rounding; and each size of group the profile's count scaled with the files of the class, rounded, give or take 1. |
| A histogram of fewer than 50 values | Not compared. |

### What the tests prove

- The profile of every class of generated fixture is what the oracle counts for the same tree:
  kinds, depths, what a folder holds, sizes, name lengths, links, and unreadable entries, worked out
  by plain code that shares nothing with the walk.
- No name leaks. A profile of a tree whose names are distinctive, hostile ones included, holds
  none of them, as text or as bytes, and the schema has no member that could hold one.
- The profile of a generated fixture validates against its schema, which rejects an unknown member.
- Profile, spec, generate, profile agrees within the table above, and the same profile, size, and
  seed give the same spec and the same manifest hash.
- The walk follows no link below the root, enters no loop, and does not cross a mount point
  unless asked. A walker told that the root is on another device than its folders makes the
  mount-point decision without a mount; the test with a real volume runs only with
  `EXCISE_HARNESS_PRIVILEGED=1`.
- A link is counted and nothing behind it is touched: a link to a FIFO, to a file in a folder that
  cannot be searched, to nothing, through a file, or to itself is one link and no error, and the
  profile has no field that says where a link points.
- A folder replaced after it was inspected, by another folder or by a link, is counted and not
  listed on Unix; a folder opened again that is not the one recorded is not walked, and a walk that
  never gave a handle up never notices. On Windows a link or a junction put where a folder was is
  refused, and another ordinary folder is walked.
- A chain of 300 folders and combs of 100 levels are walked in full by a walk that is allowed four
  folder handles, with what the oracle counts and what a walk with no budget finds, and the walker
  counts its handles: no more than the budget is ever open.
- No message of `profile` names the root, for a missing root, a file, a folder that cannot be
  opened, and an output that exists below the root; an output inside the root is made after the
  walk and is not counted in its profile; and a missing folder that becomes a link before it is
  made is refused, and nothing is written through it.
- A shaped part 32 levels deep with folder names of 255 bytes is built, with link targets of 8
  bytes; a link that would need 8 KiB is refused by the plan.
- Where the cache is decides whether a fixture is cached, by one rule, to the byte, for every kind
  of part. The same spec, one whose longest path is 512 bytes, as a `shaped` part and as a `tree`
  part, is cached under a short cache root and refused under one that leaves it less room
  (`NotCacheable`, and nothing generated in the cache); the headless suite scans it in a fresh copy
  on every run, and a `bench-e2e` fixture case shares one run copy. A cache that leaves the spec
  exactly 1,023 bytes in all (its own path as it resolves, a separator, the longest name the cache
  gives a directory, 57 bytes, a separator, and the longest path of the spec) caches it, and the
  deepest path of the tree can be named whole; one byte more refuses it. A `tree` of four folders
  named with 255 bytes plans 1,028 bytes, which no root leaves room for: it is never cached, the
  facade says `NotCacheable`, and a run copy is made. Every bundled fixture that a removal could
  ever take is cacheable below a root that leaves it exactly its longest path and refused one byte
  below, and the four that it could not (`deep-past-path-max`, `all-classes-small`,
  `hostile-small`, and `refused`) are refused below every root. The longest path every kind of part
  can plan, worked out from its fields, is never below the longest path of its plan, whatever the
  seed, over the bundled specs and specs at the edges of each kind, and is that path exactly for
  every kind but `shaped` and `identity` (whose longest names can land in other folders than the
  longest folder name); the marker's temporary name, 25 bytes, is a path of every spec. The root
  counts as it is spelled (Unix only): `<dir>//cache` is a byte longer than `<dir>/cache` and
  `<dir>/./cache` two, a relative root is the current directory and the text after it, and a spec
  that fits to the byte below the normalized spelling is refused below the same length spelled with
  `//` or `/./`. The cache is measured where it leads, and by every pathname the system works on to
  get there: below a short link to a deep directory the same spec is refused, whether the cache
  directory exists yet or not, and below a link to a short directory it is held. A cache below a
  short link to a long path that ends in a link back to a short directory is short as it is
  written and as it resolves, and is measured by the content of the link and what is left of the
  path after it: with a deep folder of 441 bytes it caches the part, and with one of 442 it
  refuses it. A cache below a link that leads to a name that is not there holds nothing (the link
  exists, so the cache cannot make it), whether the link is the cache directory or above it, and
  that is no error that stops a run: a run copy is made instead. So does a cache that is not a
  folder: a root that is a file, a link to a file, a link whose content goes on after a file with
  `/`, `/.`, or `/../real`, and a root written `<dir>/file/` or `<dir>/file/.`; through the facade
  the file is left as it was, nothing is created, and a run copy is made. The resolver is tested
  apart: a link leads where `fs::canonicalize` leads it (absolute, relative, through `..`, to
  another link, with separators and dots in its content), the pathname of each link counts as it is
  written, a link that leads nowhere, a loop, a chain of 41 links (a chain of 8 resolves), and every
  name that exists and is not a folder are refused, and on macOS the system refuses exactly the
  pathnames the resolver counts as longer than 1,023 bytes. The tests that make a link run on Unix
  only (on Windows a link to a directory needs a privilege, and a junction needs `cmd /C mklink
  /J`, which was not tried with a target 520 bytes long); the length of a root as it resolves, that
  of an empty root, and that a root that is a file has none, are pinned everywhere.
- On Windows, in CI's Windows job: a junction put where a folder was is not entered, a folder the
  walk is inside cannot be renamed, and a file whose name holds a lone surrogate is reached and
  measured as WTF-8.
- A directory of specs of your own gives the fixtures `headless` scans, and an id it does not hold
  is refused. A spec in it that is a symbolic link (whether it leads somewhere or not), a FIFO
  (without waiting for a writer to open it), or a folder is refused with a message that names it;
  a file of exactly 1 MiB is read and one byte more is refused unread; and `headless` stops on any
  of them before it scans anything. The tests load the spec directly and through the `Fixtures`
  facade that `headless` and `bench-e2e` share.
- A trailing separator does not turn a link into a folder: `profile` refuses `<link>/`, `<link>//`,
  and `<link>/.` as it refuses `<link>`, and walks the folder itself written those ways, and a
  folder reached through a link among the components; on Windows the same with a junction written
  with a trailing backslash, and written in the verbatim form with a `.` after it
  (`\\?\<abs>\junction\.`, `\\?\<abs>\junction\.\`, and with the `.` repeated), which std keeps as
  a component of its own. Both tests are Windows-only, a unit test of what the walk opens for each
  of those spellings and the junction test, and are first exercised by CI's Windows job.
- A folder that has been listed keeps the names of its subfolders and no other name, whatever else
  it held.
- The profile says in which class of file size each group lay, and it is what the oracle counts:
  for the same fixture, the groups by the class of the size of their file are the oracle's, and a
  name met with another size than the first is not a name of its group.
- Every group of hard links a spec asks for is made, whatever the seed, class by class: the
  histogram of file sizes is kept (the planned files are in the classes of the histogram, the same
  number in each, the cut at `max_file_bytes` counted), each class has the groups of its histogram
  and the names it says, a group's names share a size, and the sizes of a class's groups add up
  to its names. Groups that a class has no files for are refused, naming the field, and not
  dropped or cut short; and a layout that no rule of thumb packs, three classes of ten files with
  groups of 6, 2, 2 and 4, 3, 3 and 3, 3, 2, 2 names, is built exactly, because the spec says in
  which class each group is, and so is four groups of four names at 17 entries.
- The groups of two shaped parts are two groups, and the tree has two files with two names each,
  not one with four.
- A comparison in which one fixture id would name two trees (a `--fixture-dir` spec and the
  bundled fixture of a selected scenario) is refused before anything runs.
- `spec` takes the fewest entries a deep profile needs, 33 for a profile deeper than 32 levels,
  and its help says so.

## Read-only soak

`cargo xtask soak ROOT [--rounds N] [--timeout DURATION] [--record]` runs `excise` on a real tree,
headless and in a terminal, in a way that cannot delete, and records metrics and quirks that gate
nothing. The maintainer used to validate a change with `cargo run --release ~/`: a demanding real
tree, but manual, unrepeatable, and able to delete real data. The soak keeps the signal and removes
the danger.

It is **human-only**. `xtask/src/soak.rs` refuses unless standard input and standard output are
terminals and the person types the root's canonical path at its prompt, and until then it starts no
`git`, no build of `excise`, and no scan. That holds from the moment `xtask` runs, and no earlier:
`cargo xtask` is an alias of `cargo run --locked --package xtask --` (`.cargo/config.toml`), so
cargo has built and started `xtask` before it can read a terminal, as it has for every `cargo
xtask` command, and the prompt says so. Agents never run it (`AGENTS.md`).

It runs on macOS and Linux and refuses everywhere else: it has to end everything it starts, with an
interrupt handler and by killing a program's whole process group, and Windows has neither. The
library builds and its tests run on Windows too (where the screen is not exact, `ConPTY` paints on
its own timer, so latency comes from frame events and no decision rests on the screen). The
library entry, `excise_harness::soak::run_soak`, takes the root directly, and the tests use it, on
fixtures and scratch trees only. The code is [`src/soak`](src/soak); `xtask/src/soak.rs` asks and
prints, and `xtask/src/soak_build.rs` builds the binary.

### Why it cannot delete

Three things, each covered by a test, and the second does not depend on the first.

1. **The type of the root.** A soak's root is a `SoakRoot` (`soak/root.rs`): canonical, a
   directory, not a link. Every step and protocol that deletes or mutates a fixture (the `delete`
   and `fs_mutate` steps, `DeletionRequest`, the interactive driver) takes a `FixtureRoot`, which a
   `SoakRoot` is not and converts into none, so the compiler refuses to hand it a real tree; three
   `compile_fail` doctests in `soak/root.rs` say so. The type does not seal the path
   (`fixture::remove_tree` takes a plain `&Path`), so a test (`soak/tests.rs`) reads the source of
   every file below `src/soak` and of the two files of the command, test modules left out, and
   fails for a forbidden name (`FixtureRoot`, `remove_tree`, `fs::rename`, `fs::write`, ...), for
   every way to write Backspace or Ctrl+H outside the allowlist, for a write to the program that
   does not pass the choke point, and for a `crate::` path that is not on a list. The few names
   that some line must hold (starting the one binary under test, opening the soak's own new
   files) are allowed only on those lines, exactly as often as they are pinned.
2. **The keys.** Only Backspace asks `excise` for a deletion, in every key preset, and
   configuration cannot rebind it. The soak does not rest on that: every key it writes goes through
   `Driver::send_input` (`soak/session.rs`), which sends only `Key::ALLOWED` (`soak/keys.rs`): the
   four arrows, `h j k l`, `enter`, `esc`, `q`, and the `y` that answers the quit prompt. It
   matches a whole write, so `h` and a Backspace together are not a key, and it refuses everything
   else, Backspace (`0x7f`), Ctrl+H (`0x08`), and the input barrier (`0x1d`) by name, before it
   writes. It sends nothing that could begin an escape sequence and leave it open (the program
   joins sequences across writes: `ESC [ 121 u` is `y`). Behind the table the driver asks the
   harness's own `InputScan`, and it refuses `Enter` and `y` while the screen shows text that
   reads as a deletion dialog. Nothing the soak does can open one, so that text is a name in the
   tree: it is noted as a quirk (`dialog_text`), the folder is not opened, the program is killed
   instead of quit, and the soak does not fail. (The refusal stays broad on purpose: a screen read
   that missed a real dialog would be the dangerous mistake, and one that sees a dialog that is
   not there costs a measurement.) The protocols of `runner::live` that the soak shares send their
   keys through the same function, and the one that writes around it, the input barrier, is
   overridden to refuse. Two writes are not keys and do not pass it: the terminal layer's answer to
   a cursor position request (`ESC [ row ; col R`, pinned by a test), and the line feed and
   end-of-file that closing a session writes after its program was killed.
3. **The isolation.** The program runs in the environment of every harness run
   (`safety::isolated_env`): a scratch `HOME`, configuration, working directory, temporary
   directory, and scan store, all outside the root. Its only argument is the root: never
   `--disable-delete-confirmation`, never a mouse or custom key setting, and the profiles are
   `default` and `deterministic`.

### What it runs, and what it writes

It runs the release build of this checkout and nothing else (`cargo build --release --locked
--offline -p excise`; `EXCISE_E2E_BINARY` is not read, because one left in a shell would run
another program on a real tree). The binary that runs is a private copy of the one executable that
cargo reports for that build, made in the scratch directory with mode `0700`, in a directory that
is made `0700` too, so that it is private from the moment it exists; `summary.json`
carries its SHA-256. The build's copy and the soak's own are both identified by what they were
when whole (device, inode, owner, mode, and change time, read from the open file once its last
write was made), and the identity is checked again where it matters: the build hands the soak its
copy's identity with its path, and the soak opens that path without following a link and runs
nothing unless the open file is that file, unchanged, before it copies it and after; its own copy
is looked at again before each scan and session. A file that was replaced, or written in place
(which moves the change time and nothing else), is not run.

- **The root.** Nothing is written in it, except in the places the prompt names when the root
  contains them. The checkout's target directory: cargo replaces its own build files there, and the
  soak adds `excise-soak/` and the link `latest`. Cargo's home directory (`CARGO_HOME`, else
  `$HOME/.cargo`): cargo updates its own files there, its usage database (`.global-cache`), its lock
  files (`.package-cache` and `.package-cache-mutate`), and the source of a crate it had downloaded
  and not yet unpacked; the build is offline, so it downloads nothing. `cargo xtask soak ~` from a
  checkout under the home directory has both. A root that lies *inside* the target directory or
  cargo's home is refused. The code that the build runs (build scripts, procedural macros, a
  configured linker, `RUSTC`, `RUSTFLAGS`, `[env]`) is the person's own, runs with their rights,
  and is not confined; the prompt says so.
- **The scratch directory** (`EXCISE_E2E_TMPDIR`, else `/tmp`) holds the copy of the binary and
  every scratch area, outside the root. It must be private to the person: it and every directory
  above it is on a file system that enforces ownership, is owned by the person or by root, and is
  not writable by its group or by everybody, unless its sticky bit is set (OpenSSH's `StrictModes`
  rule; `/tmp` passes). The command refuses one that is not before it asks for the path, and
  `run_soak` checks again, because another user who can rename entries in it could put another
  program where the copy was. On macOS a volume mounted with "Ignore ownership" (the default for an
  external disk) is refused (`MNT_IGNORE_OWNERSHIP`, read with `statfs`): `stat` shows the caller
  as the owner of every entry there and every user is treated as the owner, so no mode makes a
  directory private; the way out is another directory, or `sudo diskutil enableOwnership` on the
  volume. Linux has no such flag, and for its local file systems the kernel's permission check uses
  the owner and the mode that `stat` reports, so the check asks nothing more there; it does not
  recognize a file system that decides for itself (FUSE with `allow_other` and without
  `default_permissions`, network and virtual-machine shares). The check reads owners and mode
  bits, not access control lists. The target and output directories get a note at the prompt and
  no refusal. Each scratch area in it is private by construction as well, whatever the umask is:
  its root and the directories in it are made with mode `0700` and its configuration file with
  `0600` (`Scratch::create`, which every runner uses), so that another user cannot read the report
  of a whole tree that is written there, or put a link or a file where the program will open one.
- **The output directory**, `target/excise-soak/<run-id>/` below the resolved target directory,
  which must not be a link; `latest` is replaced only when a run made it. Its files are private to
  their owner (see [Output](#output)).
- **A scan's report** goes to the scratch area, where only `state`, `accounting`, and `summary`
  are read from it, and is deleted with the area. It is large on a real tree, so the soak needs
  room there, and its size is watched while the scan runs and looked at once more when the scan
  has ended: above 16 GiB (or a quarter of the free space, if that is less) the scan's process
  group is killed, or a report that passed it between two looks is not read, and the quirk
  `report_too_large` is noted either way, since a tree of very deep folders makes a report grow
  with the square of its depth. The thread that looks is the soak's own and is not waited for: a
  scratch directory that has stopped answering (a network mount that hung) cannot hold the run
  past its bound with it. When its last look does not come back within 2 seconds the size of the
  report is not known, the report is not read, and the quirk `report_unreadable` says so.

### What a run does

A round is one headless scan and one session in a pseudo-terminal under each of the `default` and
`deterministic` profiles; `--rounds N` runs N of them in turn.

- **Headless** (`soak/headless.rs`): `excise --format json --output <scratch>/scan-report.json
  <root>`, supervised by `headless::process` (its own process group, wall time, CPU time, peak
  memory). Of the report only `state`, `accounting`, and `summary` are kept (`soak/facts.rs`); the
  four `last_*` text fields can be paths, so only whether each held anything is recorded.
- **Session** (`soak/session.rs`): the first frame; then an `Esc` at a time while the scan runs,
  each timed to the frame that counts it; the end of the scan under any badge (`COMPLETE`, or the
  label of an uncertain result, which a real home directory nearly always gets and which is a
  quirk); the largest folder opened with `Enter` and left with `Esc` (when the largest entry is a
  file, the cursor is first walked to a folder with the arrow keys, at most 48 keys); and the
  shared confirmed quit. A frame is read from the screen as its mark left it, and an entry is told
  by its whole pane. The metrics are the time to the first frame and to the end of the scan, the
  latency of the probes, the longest gap between frames, frames and output bytes, peak memory, and
  the times of the drill, the way up, and the quit.
- **Bounds.** The whole run (`--timeout`, 30 minutes by default) counts from the instant the person
  confirmed, the build included, and every phase has a bound of its own. A phase that passes its
  bound ends its program with the program's whole process group, and the run goes on. Ctrl+C,
  `Ctrl+\`, Ctrl+Z, a termination request, or a hang-up stops the run where it waits and keeps what
  finished; nothing is started once the soak has seen it (a program that it was already starting
  when the key was pressed is ended at its next look), and the run ends as an interrupted one.
  Ctrl+Z stops the run and not the command: a command that it stopped would stop its clock and its
  watch on the report while a scan, in a process group of its own, went on. `cargo xtask soak`
  runs the soak by `exec` (on Unix `cargo run` replaces itself with the binary), so Ctrl+Z reaches
  only the soak, which ends the run as Ctrl+C does and returns. A launcher that stays in front of
  it (`make`, `just`, `sh -c`, a script) is stopped by the terminal as usual; `fg` resumes it, and
  it ends with the command.

  A second Ctrl+C or a second `Ctrl+\`, the two that a person sends by pressing a key again, ends
  the command at once, as it would without a handler, but only once the exit is armed: the person
  has asked to stop and no program is held. The soak holds a program (the lookup of the commit,
  the build, a scan, or a session) from before it starts it until it has been ended with its
  process group and waited for, or given up on after 5 seconds (below), and lets it go before it
  reads a report, looks at what the program left, finishes a recording, or removes a scratch area.
  The last program let go of arms the exit at once, and a thread of the command's own arms it
  within 25 ms when none is held, so a soak blocked in the file system between two programs still
  ends at the second press. Until the exit is armed a second press is the same request. The build,
  the scans, and the sessions run in process groups that the signal of a terminal does not reach,
  so a command that ended before would leave one running with nothing to bound it. When it ends
  the command it cleans nothing up: the scratch areas (`xh-*` in the scratch directory) stay.
  While a program is held, a scratch mount that stops answering can keep the soak from acting on
  any press: a session reads its events there, a scan hears of an interrupt only through the
  thread that watches its report there, and a program being started can block there. A scan still
  ends at its bound; a session waits for the mount. To end the soak sooner, send `kill -KILL` to it
  from another terminal, then to the scans and sessions it started, which run from its copy of the
  binary (`pkill -KILL -f xh-soak-bin`). A process in uninterruptible I/O on a hung file system
  ends only when the file system answers. A hang-up, a termination request, and Ctrl+Z
  never end the command: one hang-up of a terminal delivers SIGHUP twice, milliseconds apart (the
  shell resends it to its jobs, then the kernel sends it to the old foreground group), and tools
  send SIGTERM in pairs, so a second one is not a second request, and ending the command there
  would orphan a headless scan, which runs in a process group of its own, with no bound and no cap
  on its report. No wait after a kill is unbounded: a process that SIGKILL does not end is stuck
  where a signal does not reach it (a hung mount), so after 5 seconds the soak says that it could
  not be ended, as a harness error, leaves it unreaped, and signals it no more; a process whose
  exit was published within those 5 seconds is not reported stuck, however late the supervisor
  looks.
- **Exit status.** 0 once the rounds have run, whatever the metrics say; non-zero for a refused
  start, a harness error, or an interrupted run.

### Output

`target/excise-soak/<run-id>/` holds `quirks.txt` and `summary.json`, and with `--record` a
`<round>-tui-<profile>.cast` for each session; then `latest` is pointed at the run.

- `summary.json` is a `harness-soak` document
  ([`schemas/harness-soak.schema.json`](schemas/harness-soak.schema.json), `report/soak.rs`,
  version 1): metrics and counts only, with no path, no name from the tree, no host name, and no
  free text, so that it can be shared. Every string in it is an enumerated word, a constant, or has
  a pattern, and so is every key of a session's `metrics`; a test fails for a path or a name
  written into any string of a sample. It is written last, whole or not at all, so it says how the
  run ended, and a run directory without it is one that did not end.
- `quirks.txt` is local only. It holds anything unexpected, with its circumstances: an error
  dialog, text that reads as a deletion dialog, an uncertain scan and the last folder it could not
  read, a non-zero exit, a time-out, a stall, a report that grew too large. Its text comes from the
  screen and the scan report, so it names things in the tree.
- A recording is kept only with `--record`, and its screens show real names too.

### What it does not defend against

The soak is run by the person who owns the machine, on a tree that person chose, and it is not a
sandbox. Its checks look at a place when they look, and it writes with ordinary calls by path: it
does not hold a directory open between a look and a write. So it does not defend against another
process that changes the file system under it while it runs: one that renames the output, scratch,
or target directory and puts a link where it was after the soak has looked, one that makes or
replaces `latest` between the look at it and the replacing, or one that rewrites the file that
cargo reported in place between cargo's exit and the copy. Nor does it look at access control
lists, recognize a file system that decides for itself about who may change what (the macOS
volume that ignores ownership is the one it knows), or confine the code that the build runs. An
attack that needs one of these is made by a process that already has the person's own rights,
except where other users can change a directory, which is why the scratch directory must be
private. What the soak promises is what it says where it says it: the root is not written but in
the places the prompt names, no key that deletes is sent, and everything it starts is ended with
its process group.

### Tests

`cargo test -p excise-harness --lib -- soak safety report run_support headless::process pty` (the
allowlist and the driver; sessions against scripts that stand in for `excise`; the source tripwire;
whole runs against scripts: a program that hangs is killed with its process group and the log says
so, a binary or a copy that is replaced or written in place, an interrupt, a bound, a report that
grows past its cap; the private-directory check, with a stand-in for a volume that ignores
ownership, and on macOS, with `EXCISE_HARNESS_PRIVILEGED=1`, a real image attached without owners
(`fixture::volume`); the supervisor that gives up on a process that the kill does not end; the
schema), `cargo test --test harness_soak` (generated fixtures with distinctive names, soaked
with the real binary: it finishes, validates, leaves the tree byte for byte as it was, and puts no
name or path in `summary.json`; it works in `/tmp`, or `EXCISE_E2E_TMPDIR`, else in cargo's test
directory, else in the system's, whichever the soak accepts as a scratch directory, and says
`skipped:` where none is, as in a Nix build sandbox, where `/tmp` belongs to `nobody`, and fails
there instead when `CI` is set), and `cargo test -p xtask` (the build supervisor, which lets its
program go as soon as it is ended; the handlers, run in a process of their own whose main thread
calls nothing: a second Ctrl+C or Ctrl+\ ends the command once the exit is armed, which a thread
of its own does when no program is held, and not while one is, and two hang-ups, terminations, or
stops do not; and `xtask/tests/soak.rs`, which runs the built `xtask` binary: it refuses without a
terminal, at a mistyped path, and at a scratch directory that others can change, before it builds
or starts anything, and Ctrl+Z ends a build as the other signals do).

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
   The [interactive driver](#interactive-driver) holds itself to the same rule: its `delete` runs
   the same protocol, and its `keys` never sends a key that could confirm a deletion dialog. The
   [version sweep](#version-sweep) runs published builds, which predate these guards, and goes
   further: it has no `delete` step and sends no key that could ask for a deletion. A
   scenario's own `key` that asks for a deletion (Backspace, or bytes that finish an escape
   sequence that earlier keys began, as `alt+[` and `121u` do) engages its run: from then on a `y`,
   Enter, filter text, or quit confirmation is sent only on an exact screen that shows no deletion
   dialog (see `key` under [Steps](#steps)). Where the pseudo-terminal cannot tie its screen to a
   frame exactly (`SCREEN_IS_EXACT` is false: Windows, see [`settle`](#pty-runner)), the harness
   confirms no deletion from the screen: the `delete` step, in both modes, and the driver's
   `delete` and confirmation guard refuse before any key, not even Backspace, and `cargo xtask e2e`
   skips every scenario that has a `delete` step, presses Backspace itself, or composes a
   sequence from its keys. Those scenarios run in-process under `cargo test`.
5. **Fixtures are owned.** The runner refuses any root without the harness ownership marker, no
   `delete` step may name the marker or anything inside it (validation rejects it, and the
   interactive driver refuses it), and the runners check the marker again right before each key
   that confirms or starts a deletion.
6. **Runs are isolated.** The runner clears the environment and rebuilds it from `TERM`,
   `COLORTERM`, `LANG`, and the profile's own settings. Each scenario gets a scratch `HOME`,
   `EXCISE_CONFIG`, working directory, and `EXCISE_SCAN_STORE_DIR`, so a theme commit, an export,
   or a killed run can never touch real state and residue checks are exact.

This crate enforces rules 1 and 2 and the sentinel requirement of rule 4: a scenario has no way to
name a root, and `Scenario::validate` rejects unsafe paths and unguarded deletions. Rule 3, the
run-time dialog and sentinel checks of rule 4, and rules 5 and 6 are obligations of the runner and
the fixture generator.

`excise-shape` is not a runner and has no scenario, so these rules do not apply to it. It has its
own: this repository's tests, scripts, and agents run it only on fixtures the harness generated
and on scratch trees a test made, never on a real path (see [Shape profiles](#shape-profiles)).

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
| `hostile` | Hostile | `features`, at least one and without repeats, of `control_names`, `bidi_names` (U+202E and friends), `escape_names` (ESC sequences), `newline_names`, `invalid_utf8_names`, `long_names` (255 bytes), `unreadable_dirs` (modes 000 and 100, with contents), `unreadable_files` (modes 000 and 200), and `unwritable_dirs` (`unwritable/`, a mode 555 directory that holds `stuck.txt`: it can be listed and entered, and nothing in it can be removed). Each name feature creates one directory per name, holding a file of the same name. Like `unreadable_dirs`, `unwritable_dirs` keeps a fixture out of the cache, because a path-based removal cannot remove it; and it needs a user that is not root, because root ignores the mode, so a run copy refuses to generate it for one (`FixtureError::NeedsUnprivilegedUser`). |
| `volume` | Volumes | A mount point: an empty directory in the master. `size_mib` (required, 8 to 1024), `files` (0) of `file_bytes` (1024) written once a volume is attached (see [Volumes](#volumes)). |
| `shaped` | Scale, and Identity when it has links | A tree with the shape of another tree, which `excise-shape profile` measured (see [Shape profiles](#shape-profiles)); `excise-shape spec` writes it. `root` (required), `levels` (required, 1 to 32 of them: one table of `directories`, `files`, and `symlinks` (each 0) per depth from 1, none empty, and a level below the first needs a directory above it), `max_file_bytes` (16,384, at most 1 GiB), `dangling_symlinks` (0) of the symbolic links, and the histograms the levels need, each a table of `bucket = count`: `subdirectories_per_directory`, `files_per_directory` (which also spreads the symbolic links), and `file_sizes` in classes (`0` or a power of two), and `directory_name_lengths`, `file_name_lengths`, and `symlink_name_lengths`, one bucket per length from 1 to 255. `hard_links` (none) lists the groups of regular files that are names of one file, one table for each class of file size that has any, in ascending order of class: `class` (the smallest value of the class), `names` (how many names its groups have in all), and `group_sizes`, a table of `bucket = count` in classes from 2 (how many names a group has, and how many groups have that many, so the counts add up to the groups of the class). The names of a group are one file, so they share a size and lie in one class of `file_sizes`: the groups of a class take their names from the files of that class and no other, the histogram of file sizes is kept exactly, and there is nothing to pack. Each group is given an exact number of names within its class, so that they add up to `names`. A class with fewer files than `names` (the cut at `max_file_bytes` counted: every class above it is the class of the cut), a `names` that groups of those sizes cannot have (below what their smallest sizes add up to, or above their largest), a group of fewer than two names, and classes out of order are refused, naming `hard_links`. The groups of two shaped parts are never one group. The entries at each depth are exact, so the part plans `1 +` the sum of its levels, and how a depth's entries are spread over the folders above follows the histograms, with the seed deciding which folder gets which share and which files are the names of a group. A link that resolves points at one file of the part by the shortest relative path, and a plan in which a target would be longer than 1,023 bytes (macOS's limit, the lowest of the three) is refused. |

A symbolic link of a `shaped` part that resolves points at a file that exists wherever hard links
cannot be made: at a file that is no group's, and when every file has more than one name, at the
file of a group, which is its first name in canonical order, the one the plan keeps, and never at a
name that needs a hard link. Every kind of part has a bound on its longest planned path, worked
out from its fields, and a spec whose longest planned path, below the cache directory, can pass the
1,023 bytes that `PATH_MAX` allows is never cached there, as a `deep` part never is (see
[Cache and integrity](#cache-and-integrity)); where the cache is decides it.

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
| `delete-file-while-scanning` | 249,255 | `victim.bin`, a 48 KiB file that is the largest entry as soon as the root is listed, so the cursor lands on it while the scan runs; `keep-a.bin` and `docs/keep-0.txt` as sentinels, with `docs/keep-1.txt` beside the second; and `bulk/`, 249,000 empty files in 249 folders, so that the scan is still running when the scenario deletes the victim. |
| `nested` | 8 | A folder with a folder inside it, and files beside it: `victim/` holds two files and `inner/`, which holds two more (32 KiB in all, the largest entry, so a fresh map selects it), and `keep-a.bin` and `keep-b.bin` (16 KiB each) are the sentinels. The id is short on purpose: a terminal 60 columns wide cuts a longer path in the deletion dialog. |
| `refused` | 4 | A file that cannot be deleted: `hostile/unwritable/stuck.txt`, in a mode 555 directory, and `keep-a.bin` beside it as a sentinel. It needs permission modes and a user that is not root (Linux and macOS), and it is never cached: take a run copy. The id is short on purpose: the deletion dialog cuts a long path, and the `delete` step refuses to confirm a path it cannot read whole. |
| `twins` | 4 | Two files called `twin.bin`, one at the root (the largest entry, so a fresh map selects it) and one in `docs/`, and `keep-a.bin` beside them as a sentinel. The selected-item panel shows a name and a kind and nothing else, so it cannot tell the twins apart: the controls for a `delete` step with no `path`, or with no dialog, use it. |
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
stale entry is never mistaken for a current one. The generator version (`GENERATOR_VERSION`) is
raised whenever the same spec and seed would plan another tree or manifest. Version 2 numbers the
hard-link groups of a plan once for the whole plan, which changes the manifest of a spec with more
than one identity part, and anchors the links of a `shaped` part at a file that exists where hard
links cannot be made. A new version costs every cached master one regeneration, in a new entry:
the entries of the old version are never reused, and stay in the cache directory until `cargo
clean` removes them. The manifest hashes that the tests pin do not include the version (it is in
the manifest document, the marker, and the name of the entry, and the hash covers the entries
alone), and the pinned hashes of the bundled specs did not change with it. Generation happens in a
`.partial-…` sibling that is renamed into place once the marker, written last, has sealed it. Tests
pass a temporary directory as the cache root and never touch the shared cache.

The shared cache holds only trees that a path-based removal can remove, so that `cargo clean` and
`git worktree remove` can always remove the target directory. Such a removal fails on a directory
that cannot be listed (the hostile `unreadable_dirs` feature), on one that nothing can be removed
from (`unwritable_dirs`), and on a path longer than `PATH_MAX`. A `deep` part always has one. Every
other kind of part can: a `tree` or a `shaped` part has paths as long as its levels and names allow,
up to 8,447 bytes (32 levels of names of 255 bytes, which a spec of one's own can ask for, and
`--fixture-dir` lets one reach the cache), and a `file`, `volume`, `identity`, or `hostile` part has
a root of up to 255 bytes with names below it. The marker is a path as well: it is written at the
fixture root under a name of 25 bytes (`.excise-harness-owned.tmp`) and renamed to its own, of 21.
A path-based removal is handed the whole path from the root of the file system, so `PATH_MAX` is
the limit: 1,024 bytes on macOS, the lowest of the platforms the harness runs on (it is 4,096 on
Linux), and it counts the terminating NUL, so a path is at most 1,023 bytes
(`MAX_PORTABLE_PATH_BYTES`). The cache directory above the fixture root takes part of that, and
where it is the harness does not say: `CARGO_TARGET_DIR` puts it anywhere, through a link too. So
whether a fixture is cached is decided where the cache is known, by one rule
(`Fixtures::is_cacheable`, which `Fixtures::master`, the headless suite, and the `bench-e2e`
fixture cases all go by): the path of the cache directory, a separator, the longest name the cache
gives a directory directly below it, a separator, and the longest path the spec can plan, of any
part or of the marker, must fit in 1,023 bytes together (`FixtureCache::longest_entry_path_bytes`
is the first three, `FixtureSpec::longest_path_bytes` the last, and `FixtureSpec::removable_by_path`
takes them). The path of the cache directory is measured three ways, and the longest counts: as it
is spelled; as it resolves, with every symbolic link in it expanded; and as the system works on it
while it expands each link, which is the content of the link and what is left of the path after it.
On Unix the spelling counts as it is, because the system is handed the text of the root and counts
every `.` name and repeated separator in it, which `std::path::absolute` drops: `<dir>//cache` is
a byte longer than `<dir>/cache`, `<dir>/./cache` two, and a relative root is the current directory
and the text written after it (a trailing separator counts too, though `join` adds none after it: a
byte on the safe side). Off Unix `absolute` is kept: on Windows, in Rust 1.98, it returns a
verbatim path (`\\?\...`) as it is and hands any other to `GetFullPathNameW`, the normalization
(`.`, `..`, repeated separators) that std applies to a path of 248 UTF-16 units or more, and Win32
to the rest, before the path is used, so the normalized spelling is the one the system counts there
(read from the source of the standard library; nothing was measured on Windows). The system counts
the expansion of a link against `PATH_MAX`: on macOS a path fails with `ENAMETOOLONG` when the
content of a link and the rest of the path are together longer than 1,023 bytes, and that pathname
can be longer than the path as it was written and longer than the one it resolves to. A short
`CARGO_TARGET_DIR` that is a link to a deep directory, or to a long path that ends in a link back
to a short one, or whose content goes down a long way and comes back up with `..`, is such a case;
`/tmp` and `/var` there are links that make a path 8 bytes longer as it resolves. The resolver
(`src/fixture/resolve.rs`) takes the path one name at a time, as the system does, and keeps the
length of every pathname that makes. The path it gives is that of the longest ancestor of the cache
directory that exists, with every link expanded, and then the names that do not exist yet as they
were written, so the cache directory need not exist. Off Unix `fs::canonicalize` gives it (on
Windows the verbatim form, `\\?\C:\...`, four bytes longer than the drive form), and only it and the
spelled path are measured. A cache whose path cannot be resolved holds no fixture at all, and a run
copy is made: it is empty, a name on the way cannot be searched, a name that exists is not a folder,
the last one included (a root that is a file, a link to one, a link whose content goes on after one
with `/`, `/.`, or `/..`, and a root written so, which the system refuses with `ENOTDIR` and
`create_dir_all` cannot use), a link in it leads to a name that is not there (the link exists, so
the cache cannot make it) or loops, or more than 32 links are on the way (a name that is missing,
with no link leading to it, is no error). The longest name is that of a fixture while it is
generated, in a `.partial-…` directory that is renamed into place afterwards: `.partial-`, 16 digits
of the spec hash, the number of the process (at most 10 digits), and a count of the calls (at most
20), 57 bytes in all, which is more than the name of the entry (`<16 digits>-<generator version>`).
Under a target directory of 100 bytes as it resolves, the cache directory is 124 bytes
(`<target>/excise-fixtures.noindex`), and a spec may plan paths of at most 840 bytes. The longest
path is an upper bound worked out from the fields of each part alone (`Part::longest_path_bytes`),
without planning it, because the answer is wanted before a fixture of up to 10 million entries is
planned, and no plan has a longer path, whatever the seed. It is exact for a `tree` (the root, the
longest folder name of its style at every level, and the longest file name), a `file` and a `volume`
(the root: the files written onto a volume are in a run copy, never in the cache), a `deep` part,
and a `hostile` part (the root, the folder of a feature, and two names of its catalog, or a fixed
path), and for an `identity` part (the root and the longest path of each feature, in its fixed
subfolders) unless the longest names of `links/` are spread to folders whose names are shorter than
the longest (`d10` past `d9`); for a `shaped` part it is the root, then the longest folder names the
histograms can give the folders of a path, and the longest name of an entry at the bottom, none
shorter than the hexadecimal digits that tell the entries of a folder apart
(`ShapedPart::longest_path_bytes`). So a spec that passes is safe to cache there, and one that fails
may still plan only short paths; and the same spec can be cached under one target directory and
refused under another that is longer, or that is a link to one. `Fixtures::master` refuses a fixture
that is not removable by path (`FixtureError::NotCacheable`, and nothing is generated in the cache),
and a runner takes a run copy of it instead.

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

On macOS the same command also runs the test behind the read-only soak's scratch-directory check
([Read-only soak](#read-only-soak)): an image attached with `hdiutil attach -owners off` is a volume
that ignores ownership, which the check refuses whatever the mode of its directories, and one
attached with `-owners on` is not refused for that. `mount`, which decodes the flags of the same
`statfs`, says which of the two a volume is, and the test compares the check with it.

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
| Version sweep | `harness-sweep` | 1 | `cargo xtask sweep` | [`harness-sweep.schema.json`](schemas/harness-sweep.schema.json) | What the sweep found of every published version and the candidate: the versions (ref, commit, toolchain, binary digest, how the build went, what its `--help` lists), the paired and interleaved timings (every sample with the round it was taken in and whether it finished, how many rounds a version was skipped in, each version's median, and its ratios, with bootstrap intervals over the rounds in which both sides finished and the bounds the other rounds give), the observation of every other check with its evidence file, the conditions it ran under, and the table: one row per defect, one cell per version (`affected`, `not-affected`, or `not-measurable`, with the reason, the measured value, and the evidence). See [Version sweep](#version-sweep). |
| Tui command | `harness-tui` | 1 | `cargo xtask tui` | [`harness-tui.schema.json`](schemas/harness-tui.schema.json) | What one interactive-driver command prints: its result, whose shape depends on `command` (the first screen, the screen and the events after keys, a deletion's verified dialog and sentinels, the screen, the event records, how the session ended, or the open sessions), or why it failed. |
| Shape profile | `harness-shape-profile` | 1 | `excise-shape profile` | [`harness-shape-profile.schema.json`](schemas/harness-shape-profile.schema.json) | The shape of one tree as aggregates only: counts by kind and by depth, histograms of what a folder holds, of file sizes, and of name lengths, hard links, symbolic links, and unreadable entries; never a name, a path, a link target, an owner, or a timestamp (see [Shape profiles](#shape-profiles)). |
| Soak | `harness-soak` | 1 | `cargo xtask soak` | [`harness-soak.schema.json`](schemas/harness-soak.schema.json) | What one read-only soak of a real tree measured: the commit and the SHA-256 of the binary, how the run ended, for each round the headless scan's wall time, CPU time, and peak memory with the `state`, `accounting`, and `summary` counts of its report, and for each session in a pseudo-terminal how it ended, whether the terminal was restored, what the program left behind, and a map of named metrics whose names the schema lists, with the quirks counted by kind. Metrics and counts only: no path, no name from the tree, no host name, and no free text, so that it can be shared (see [Read-only soak](#read-only-soak)). |

`cargo xtask headless` writes a `harness-summary`, not a separate document kind: a headless run is
one more kind of scenario result, named `headless-<fixture>`, whose open `metrics` map carries the
oracle-diff and `du -sk` figures instead of pseudo-terminal ones (see
[headless's own **Output**](#headless-runner) for the names). `cargo xtask compare` writes no
document at all: it only prints the verdict table, because a ratio-budget comparison's evidence
(the paired samples and the ratio) is exactly a `harness-ab` row running against this crate's own
binary, and a future slice may fold it into one if that evidence needs to be kept.

Every `cargo xtask tui` command prints one `harness-tui` document, and so does a command that fails
(`ok: false`, with an `error`). `keys`, `delete`, `close`, and `open` carry their events as a
digest: the frame records are summarized, and `events` is the one command that lists them. The unit
tests in `src/report/tests.rs` cover every shape of the document, and `xtask/tests/tui.rs`
validates the documents the real commands print.

`cargo xtask sweep` writes one `harness-sweep` document, `target/excise-sweep/<run-id>/sweep.json`,
and its table is the document's `rows`. The schema cannot say that the candidate is the last of the
versions, that a ref names one version, that every row has one cell per version in their order, that
every series and check names a version, that a series' samples, completion flags, and rounds line
up, that its rounds increase and stay below the rounds the measurement ran, that its samples and
skipped rounds are no more than those rounds, that a ratio has a median and an interval exactly when
some round had a ratio and counts no more rounds than the measurement ran, that the `du` samples of
a measurement are one for each round, or that a `not-measurable` cell, a check that did not run, and
a failed build each say why; `HarnessSweep::check` holds a document to them, and the sweep checks
its own document with it before it writes it.

A `metrics` map is deliberately open: the harness does not constrain which names appear, and the
schemas say so rather than enumerating them (a scenario's own `measure` names, a fixture's class,
or a future metric all pass through unchanged). The one exception is the soak's, which is made to
be shared: the names of a session's `metrics` are listed in `harness-soak.schema.json`
(`$defs/metric_name`), so that a name from the tree cannot be a key. Fixed-shape fields
(identities, verdicts, the profile and tier enums, the session diagnostics object) are fully
enumerated and reject an unknown member or an undeclared field.

Each schema's `$id` is
`https://github.com/findyourexit/excise/harness/schemas/<document_kind>-v1.json`. The Rust types
are `HarnessSummary`, `HarnessFailure`, `HarnessAb`, `HarnessCounts`, `HarnessSweep`,
`HarnessTui`, `HarnessShapeProfile`, and `HarnessSoak` in `excise_harness::report`.
They implement `Document`,
which carries the kind, the schema id, and the schema text, and renders the canonical form:
pretty-printed JSON in field order with a final newline.

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
same configuration layers, without reading the environment or a configuration file. The runner
gives the program a configuration file of its own, empty, which a theme commit writes and
`expect_config` reads, and a scenario's `disable_delete_confirmation` adds the same flag the
process runners pass:

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
| `delete` | Presses Backspace, parses the dialog, asserts its title and path name exactly this entry and kind under the fixture root (a path the dialog cut short cannot be checked) and, with `path`, exactly that path, asserts every sentinel exists, and only then sends `y`, or Enter with `confirm_with = "enter"`. A mismatch fails the step and the confirmation key is never sent. With `disable_delete_confirmation` it instead checks the selected-item panel (name and kind), the entry on disk at `path`, and the sentinels, sends Backspace alone, and fails if a dialog opens. |
| `wait_refresh` | Delivers one barrier, as `settle` does: the barrier returns once the owner loop has nothing outstanding, which includes the rebuild or publication that replaces the map after a deletion. |
| `wait_fs_absent`, `wait_fs_present`, `expect_fs` | Resolve paths one component at a time, never through a symbolic link. |
| `expect_config` | Reads the configuration file the runner gave the program and compares one string setting of it, on a fresh screen: a barrier comes first when input was delivered since the last one. |
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
skipped. On Windows, where the pseudo-terminal runner skips the scenarios that have a `delete` step
(see [Tiers and platforms](#tiers-and-platforms)), it still runs them: the same steps against the
real executor, with no terminal (`bundled_in_process_scenarios_pass_under_every_declared_profile`).

### `settle` and waits

`settle` is the runtime's barrier: it renders, then drains worker events, background deletion
work, the rebuild or publication that replaces the map after a deletion, timers, and animation
until the owner loop is quiescent, and so is `wait_refresh`. In-process time is virtual, and the
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
   a user would end it; a scenario that stops earlier is stopped by the runner. After a `delete`,
   put a `wait_refresh` before the `quit`: a quit while the program is still rebuilding its map
   exits 130, not 0, and one before the rebuild has started exits 2.
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
