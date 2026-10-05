# AGENTS.md

Instructions for coding agents working on Excise, a terminal disk-usage navigator that can permanently delete what it shows. A wrong deletion is the worst bug there is, so safety outranks speed.

This file is canonical: `CLAUDE.md` and `.github/copilot-instructions.md` only point here. It summarizes [CONTRIBUTING.md](CONTRIBUTING.md), [docs/development.md](docs/development.md), and the [harness README](crates/excise-harness/README.md), which hold the detail. A change that alters a command, tier, or rule stated here updates this file in the same commit.

## Layout

- `src/`, `tests/`, `benches/`: the `excise` binary, its tests, and its benchmarks. `docs/` is the published documentation, with the `docs/safety/` contracts. `generated/` holds the man page and completions: `cargo generate` rewrites them and `cargo check-generated` verifies them.
- `crates/excise-harness/`: the internal, unpublished validation harness: `scenarios/`, `fixtures/`, `comparisons/`, the runners, `schemas/` (never `docs/schemas/`, which release archives ship), and `excise-shape` (`src/bin/`), the binary that profiles a tree's shape as aggregates only and builds fixture specs from a profile.
- `xtask/`: the `cargo xtask` commands: the harness runners (`e2e`, `headless`, `compare`), the interactive session driver (`tui`), repository checks, and release tooling.

## Safety rules

- Run `excise` only through the harness, against the fixtures it generates. Never run it against a real path (`~`, a project, a mounted volume).
- Never run `excise-shape` on a real path either. Profile only the fixtures the harness generates and scratch trees you create. A profile of a real tree is its owner's to make: you may build a spec from a profile you are given (`excise-shape spec`) and run it through `--fixture-dir`, and you never commit a profile or a spec of a real tree.
- Trigger deletions only through a scenario's `delete` step or `cargo xtask tui delete`. Both press `y` (the scenario step presses Enter instead with `confirm_with = "enter"`) only after the dialog names exactly the expected entry, every declared sentinel still exists, and the fixture root still carries its ownership marker. A scenario that sets `disable_delete_confirmation` has no dialog: the step then checks the selected-item panel, the entry on disk, every sentinel, and the marker, and presses Backspace alone. A scenario with a `delete` step must declare sentinels, and no `delete` step may name the ownership marker or anything inside it. `cargo xtask tui keys` never sends a key that could confirm a deletion dialog.
- Where the pseudo-terminal cannot tie its screen to a frame exactly (`SCREEN_IS_EXACT` in `crates/excise-harness/src/runner/live.rs`: true on Unix, false on Windows, where `ConPTY` paints the screen on its own timer and can leave a stale dialog on it), the harness confirms no deletion from the screen. The scenario `delete` step, in both modes, and `cargo xtask tui delete` refuse before any key, not even Backspace, and `cargo xtask e2e` skips every scenario that has a `delete` step, and every scenario that presses Backspace itself or composes an escape sequence from its keys (see `key` below), also when you name it. On Windows deletions are therefore tested in-process under `cargo test`, and `tests/harness_scenarios.rs` asserts that the `delete` step refuses before any key.
- Every fixture root carries the ownership marker, a regular file named `.excise-harness-owned`, and every runner refuses a root without it. Never add the marker to a directory the harness did not generate.
- Leave no residue. Runs use scratch directories that are removed afterwards. If you pass `--keep-fixture` or `--keep-scratch`, remove what it kept when you are done, and check that no `xh-*` entry is left in `/tmp` or `$TMPDIR`. A `cargo xtask tui` session holds its files only while it is open: `close` every session you open (`cargo xtask tui list` shows the ones left), and remove the recording a `--record` session kept.
- Bound every wait, and kill the child's whole process group on a timeout, as the runners do.
- In your own shell, start no background jobs (`&`, `nohup`, `disown`) and no unbounded loops (`while :`, `yes`). Run every command in the foreground with a timeout. An agent's shell often runs inside the agent process, where such jobs cannot be cancelled and can starve the machine.
- Write no `unsafe` code outside `src/os/windows.rs`. Clippy denies `pedantic` and `unwrap_used`. Add only dependencies that pass `cargo deny check`.
- Set `EXCISE_HARNESS_PRIVILEGED=1` only when your task is about volumes or mount boundaries. It makes the harness attach and detach real disk images, loop devices, or virtual disks.
- Before you touch deletion, accounting, or terminal code, read [Engineering expectations](CONTRIBUTING.md#engineering-expectations).

## Commands

The toolchain is Rust 1.98.0, pinned in `rust-toolchain.toml`. Inside the repository `cargo --version` must print `1.98.0`. If it does not, another `cargo` comes before rustup's on `PATH`, and Clippy results differ between patch releases.

The gate, which CI runs on Linux, macOS, and Windows:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The harness, through the `cargo xtask` alias:

```console
cargo xtask e2e --quick            # pseudo-terminal scenarios, quick tier
cargo xtask e2e --scenario NAME    # one scenario, whatever its tier
cargo xtask headless --quick       # headless scans, checked against the fixture oracle and timed against `du`
cargo xtask headless --fixture-dir DIR --fixture ID   # scan a spec from a directory of your own, such as one `excise-shape spec` wrote; `bench-e2e` takes the same option; a spec is opened without following a link, must be a regular file, and is read up to 1 MiB
cargo xtask bench-e2e --baseline main --fixture ID   # paired A/B evidence against another build
cargo xtask compare --full         # ratio budgets (motion_complete_ratio, tui_complete_ratio) between two runs of one binary
cargo xtask counts                 # deterministic counts (entries, scan-store bytes, residue files), each fixture counted twice and required to agree; CI comments their deltas on pull requests that change src/
cargo xtask tui open --fixture ID  # drive one live session on a fixture to explore the interface (see Exploring the interface)
```

`e2e` and `tui open` build the release binary first, or use the one named by `EXCISE_E2E_BINARY`. `cargo verify` is the complete local suite and needs more tools than the gate; see [docs/development.md](docs/development.md).

## Tiers

A scenario's `tier` is `quick` (the default), `full`, or `nightly`, and says which `cargo xtask e2e` run includes it:

- `--quick` runs `quick` scenarios under the `default` and `deterministic` profiles only, and must stay within 2 minutes. A run of the whole tier (no `--scenario`, `--profile`, or `--repeat`) times itself and prints its time against that budget on the verdict table's last line. Over it, the run names the five slowest runs and warns, or, on the reference machine, which sets `EXCISE_HARNESS_REFERENCE=1` (leave that variable as you find it), fails: give a heavy scenario `tier = "full"`, or make it cheaper.
- `--full` (the default without a flag) runs `quick` and `full` scenarios under every profile each one declares.
- `--nightly` runs all three tiers under every profile each one declares.
- `--scenario NAME` runs the named scenario whatever its tier.
- On Windows `cargo xtask e2e` skips every scenario that has a `delete` step, in either mode, and every scenario that presses Backspace itself or composes an escape sequence from its keys (see `key` below), also when you name it with `--scenario`, and prints `SKIP NAME: ...` (see Safety rules). The quick tier there runs 8 of its 26 runs: the six deleting scenarios and the three that press Backspace themselves (`delete-file-cancelled`, `delete-file-terminal-too-small`, `exit-prompt-keeps-pending-deletion`), two profiles each, are skipped; Windows has no full-tier scenario of either kind, and its pull-request list is empty. `cargo test` still runs those scenarios in-process on Windows (the same steps against the real executor, with no terminal), and `tests/harness_scenarios.rs` asserts there that the `delete` step refuses before any key, with the `ConPTY` reason, and that the fixture is unchanged, so Windows CI checks the rule; on Unix nothing changes.
- `--latency-scale FACTOR` multiplies the limits of the four latency budgets (input to frame, stall, first frame, quit) and no other budget. Local runs stay strict; pull-request CI uses 2.
- `--timing-informational` (on `cargo xtask e2e`, `compare`, and `headless`) reports the timing verdicts that would block (the four latency budgets, a comparison's ratio, a headless scan's ratio against `du`) as warnings that the table and `summary.json` record and the exit status ignores; every other check still blocks, and anything expected to fail keeps its strict verdict. Local runs stay strict; hosted macOS CI uses it.

`cargo xtask e2e --quick` must pass before you report a change as done. A change to scheduling, animation, or scan-store code also needs the relevant `full` scenarios: name them, or run `--full`. `cargo test` runs the `quick` scenarios in-process and never runs `full` or `nightly` ones. `cargo xtask headless --quick` scans the fixtures of at most 10,000 planned entries and `--full` (the default) those of at most 250,000; `--fixture ID` runs one fixture of any size.

CI runs the tiers for you: every pull request runs `e2e --quick --latency-scale 2` plus the full-tier scenarios named in `ci.yml`'s `pr_scenarios` (Linux and macOS; macOS also with `--timing-informational`), each native job within 15 minutes, and compiles every fuzz target; `nightly.yml` runs `e2e --nightly`, `compare --nightly`, and `headless --full` at the strict budgets on Linux (with the memory cap and the privileged volume steps) and with timing informational on macOS, plus Windows' lifecycle tier without the scenarios that delete; `weekly.yml` scans `tiny-files-10m`. Strict timing runs on the Linux nightly and on the reference machine before a release. [docs/development.md](docs/development.md#continuous-integration-tiers) lists the commands and how to reproduce a CI failure.

## Exploring the interface

To find out what the program does before you write a scenario, drive one live session on a fixture with `cargo xtask tui`, then capture what you found as a scenario. Every command prints exactly one `harness-tui` JSON document on stdout, and a failure is a document too: status 1, or 2 for a command line that is not valid. Read stdout and nothing else; stderr carries the release build's output and the supervisor's notes.

```console
cargo xtask tui open --fixture delete-file [--profile NAME] [--size 120x40] [--record] [--idle-timeout 15m]
cargo xtask tui keys SESSION / type:victim.bin enter    # keys, then the screen and the events since the last command
cargo xtask tui screen SESSION                          # text, cursor, modes, boxes, dialog, selection
cargo xtask tui events SESSION --since 0                # every event record, frames included
cargo xtask tui delete SESSION --name victim.bin --kind file
cargo xtask tui close SESSION                           # quits the way a user does, removes everything
cargo xtask tui list                                    # the open sessions; a stale one is cleaned
```

- `--fixture` is the id of a spec in `crates/excise-harness/fixtures/`, never a path. The program runs on a disposable copy that carries the ownership marker, in the isolated environment the scenarios use. A spec that needs a volume is refused.
- One supervisor process per session outlives the command that opened it; its files are in `target/excise-tui/<id>/`. The session ends on `close`, when the program exits, and after the idle timeout (15 minutes), and then removes everything it made. `open` and `list` clean a session whose supervisor died.
- A session is cleaned only when the driver can prove it is its own: a stale session's process is killed only if it carries the session's mark in its environment (never because a file or a process id says so), and every command refuses a `target/excise-tui` that is a link. If a session's directory is still there after its last reply, the command says so (`close` in `cleanup`, any other reply as a failure that keeps the exit), and `list` cleans it.
- Keys are the scenario key names (`enter`, `esc`, `backspace`, `tab`, `up`, `down`, `left`, `right`, `page_up`, `page_down`, `ctrl+` and `alt+` combinations, any single character; `ctrl+]` is the harness's input barrier request, not a key) and `type:<text>`. `keys` waits for a frame that counts what it sent, as `settle` does; `settled: false` means none came, as for a key that changes nothing.
- `open` returns at the first frame, while the scan may still run: wait until `screen` reports `header_state` `COMPLETE` before you act on the map.
- `esc` goes up a folder. A folder opens when it is selected (`/`, `type:<name>`, `enter`) and `enter` is pressed again.
- The filter keeps its text: `/` opens it with the text of the filter before (`screen` shows it in `filter.input`), so erase that with `backspace` before you type a name, or the name is appended to it. A filter that is applied again unchanged selects nothing.
- `delete` runs the scenario `delete` protocol on an entry of the folder the session shows, because the filter selects among that folder's entries. It takes the entry you selected, and selects the entry through the filter otherwise. For an entry elsewhere it fails at once with `not_in_view` and sends nothing: navigate to its folder first. `--name` is the path from the fixture root. A later deletion in a session does not count what an earlier one removed among its sentinels. `--timeout` bounds the whole command; one that runs out is a failure document whose `confirmed` says whether the confirmation key was sent, and the session stays open (after a confirmation the program is still deleting: read `screen`, or `close` it).
- `keys` refuses `y`, `enter`, and every other key that could confirm a deletion dialog while one is open, or may be about to open, and in a command that sent `backspace` before it: erasing a filter's text and applying it are two commands. Before such a key the driver asks the program, with an input barrier (the byte `ctrl+]`, which `excise` reads in order with the keys and answers with a frame while the event channel is open), whether it has read every key sent so far, and reads the dialog from the screen that shows that frame, so no clock decides it and however the terminal cut the keys into input events (`alt+backspace` can reach the program as `esc` and `backspace`); a build that does not mark its frames and answer the barrier gets no such key. Use `delete` for a deletion. `esc` cancels a dialog.
- The driver runs on macOS and Linux only (on Windows every command fails with a document that names the platform), where the screen is exact (`SCREEN_IS_EXACT`). On a terminal where it is not, `delete` and the confirmation guard of `keys` refuse before any key, with the error kind `refused`; the tests check that against a terminal that paints on its own timer.
- `keys` also refuses a key that is the escape byte and a confirmation in one input (`alt+y`, `alt+enter`), always: a program may read it as two inputs, so the escape could close a prompt that covers a deletion dialog and the key confirm the dialog it uncovers. Send `esc`, read `screen`, and then the key. `delete` waits for a deletion that an earlier command confirmed and gave up on before it starts another, so the `deletion_finished` it waits for is its own.
- `keys` also reads every byte the session has written, across commands, with one scan, because the program joins the bytes of an escape sequence across writes: `alt+[` and then `type:121u` are `ESC [ 121 u`, which it reads as `y`, with no `y`, Enter, or line feed in any key. A key that begins a sequence (`alt+[`, `alt+O`) is sent; every key that continues one is refused as a key that could confirm, whatever its bytes, because no input barrier can be written behind `ESC [` (the program takes the request for a byte of the sequence and never answers it). After one, `close` the session and open another. After a lone `esc`, a key that could continue a sequence is guarded like any key that could confirm.
- The program can draw a frame after the one that counts your keys, so a prompt or a dialog may be missing from the screen `keys` prints: read `screen` again when it does not show what the keys should have done, and wait for what you need (`header_state`, a dialog, the selection) before you act.
- `open`, `keys`, `delete`, and `close` summarize the frame events since the previous command (`events.frames`: the count, the time of the first and of the last, and the last frame record) and list every other event in full. `events --since N` lists every record, frames included.

When you know what happens, write it as a scenario (see below): the keys you sent become `key`, `type`, and `select` steps, the screen text you relied on becomes `wait_text`, `wait_header`, and `expect_screen` steps, a deletion becomes a `delete` step with its sentinels, and `cargo xtask e2e --scenario NAME` runs it. `--record` keeps an asciicast of the session, `target/excise-tui/<id>.cast`, to attach to the pull request; it is large (7 MB for a session of 12 seconds on the smallest fixture), so record only a session you mean to keep.

## Scenario authoring

A scenario is a strict TOML file (unknown fields are errors), `crates/excise-harness/scenarios/<name>.toml`, and it names its fixture by id: `fixture = "<id>"` means the spec `crates/excise-harness/fixtures/<id>.toml`, and you add a spec there when none fits. A scenario never names a root, and its paths are relative to the fixture root. Start from `scenarios/delete-folder-lifecycle.toml`; the harness README lists every field.

- Lifecycle scenarios declare the `default` and `deterministic` profiles and end with `quit` and `expect_exit` with `residue = "none"`.
- Wait with `wait_header` before you act, never for the screen to go idle. Every wait takes a `timeout_ms` (10 s by default).
- Put a `settle` after `key = "esc"`: a key sent right behind a lone Esc can be read with it as Alt plus that key.
- After a `delete`, put a `wait_refresh` before a `quit` or anything else that ends the run. The map lists the deleted entry until the program replaces it, a quit meanwhile exits 130 instead of 0, and the header reads `COMPLETE` from the map as it was, so `wait_header` cannot say when it is over.
- A scenario whose verdict depends on timing includes a step only the pseudo-terminal runner performs (`wait_event`, `measure`, `expect_budget`, `signal`), so that the in-process runner, which never judges timing, skips it.
- To start `excise` in its reduced-confirmation mode, set the typed `disable_delete_confirmation = true`. A scenario never passes arguments of its own, because they could point the program at a root the harness does not own.
- A scenario that has a `delete` step, presses Backspace itself, or composes an escape sequence from its keys, needs no `platforms` entry to keep it off Windows: the pseudo-terminal runner skips it there on its own (see Tiers), and `cargo test` runs it in-process.

The steps:

- `wait_text`: wait for text or a regex on the screen or in a region of it.
- `wait_header`: wait for the header badge to read `scanning` or `complete`, the only completion signal.
- `wait_event`: wait for an event-channel event such as `scan_complete` (pseudo-terminal runner only).
- `key`: press one key, with optional `ctrl` and `alt`. A key the program can read as Backspace (`backspace`, `alt+backspace`, `ctrl+h`) asks for a deletion dialog that no `delete` step verifies, and so does a write that continues an escape sequence that an earlier write began and nothing has finished: the program joins the bytes of a sequence across writes, so `alt+[` and then the text `121u` are `ESC [ 121 u`, which it reads as `y`, with no `y`, Enter, or line feed in any write. The runner scans every byte it writes (`pty::input::InputScan`) and decodes none of it: a sequence that one write begins and a later write continues counts as both a request and a confirmation, whatever its bytes (an `esc` that the next write's `[` or `O` turns into one begins it too). From then on the run is engaged: every later `key`, `type`, `select`, or `quit` write that can be read as a confirmation is sent only behind an input barrier, on an exact screen that shows no deletion dialog (and the filter prompt, or the plain quit prompt, where the step needs it); otherwise the step fails with `DeleteRefused` and the key is not sent. `alt+y`, one write that can be two inputs, is always refused, and so is a write that would continue `ESC [` or `ESC O`, behind which no barrier can be written. Send each key as one `key` step. Where the screen is not exact the runner skips the scenario (see Tiers).
- `type`: type literal text, one key press per character, under the same rule as `key`.
- `select`: select an entry by name through the filter, and check the inspector shows exactly it.
- `delete`: press Backspace, check the dialog and every sentinel, check the fixture's ownership marker once more, and only then confirm with `y` (or Enter, with `confirm_with`). The dialog is read after an input barrier written behind the Backspace, from a screen that shows the frame that answers it (a program that does not mark its frames and answer the barrier is refused before any key is sent, and so is the step on Windows, in both modes: `ConPTY` repaints the screen on its own timer, so the harness cannot tie the screen to a frame; the step fails with `DeleteRefused`, and the scenario runs in-process under `cargo test`). A mismatch fails the step and confirms nothing. `path` says where the entry is and defaults to `name` directly below the root: the dialog must show exactly that, so an entry below a folder needs its `path`. With `disable_delete_confirmation = true` there is no dialog: the step asks for a barrier, waits for the selected-item panel to name the entry, checks the entry on disk (at `path`, for an entry below the root), every sentinel, and the marker, refuses when another entry of the fixture has the same name and kind (the panel could not say which one Backspace deletes), then presses Backspace alone, asks for a barrier behind it, and fails if a dialog opened.
- `wait_refresh`: after a `delete`, wait until the map has caught up with it (pseudo-terminal runner: the program's `refresh_finished` event and a frame after it; in-process runner: one barrier). It returns at once after a deletion that removed nothing, and it needs a `delete` before it.
- `wait_fs_absent`: wait until a fixture-relative path no longer exists.
- `wait_fs_present`: wait until a fixture-relative path exists.
- `fs_mutate`: change the fixture while excise runs (`appear`, `change`, `vanish`, `replace`).
- `resize`: resize the terminal.
- `signal`: deliver a Unix signal or a Windows console event (pseudo-terminal runner only).
- `expect_screen`: assert what the screen shows now.
- `expect_fs`: assert which fixture-relative paths exist now.
- `expect_config`: assert one string setting of the configuration file the program saved, by dotted `key` (`runtime.theme`). Put a `settle` before it.
- `expect_exit`: wait for the exit, and assert the exit code, the terminal restored, and no residue.
- `expect_budget`: assert that a recorded metric is within a named budget (pseudo-terminal runner only).
- `measure`: record the time between a `start` and a `stop` marker as a metric (pseudo-terminal runner only).
- `settle`: wait until the program has processed everything sent so far.
- `quit`: the confirmed quit, `q` and then `y`. In an engaged run (see `key`) the `y` waits for a barrier and for the plain quit prompt on an exact screen.

### Known defects (strict xfail)

A scenario that documents an unfixed defect sets `expect = "fail"` and `slice = "X2"`, the id of the change that will fix it. It runs and must fail (`xfail`). If it passes (`xpass`), the run fails, so the fixing change flips it to `expect = "pass"` and drops `slice`.

- `platforms = ["linux", "macos", "windows"]`, spelled as `std::env::consts::OS` spells them, limits where a scenario runs. Elsewhere every runner skips it, even when it is named.
- `fails_on = ["macos"]` limits `expect = "fail"` to some of those platforms. Where the scenario runs but `fails_on` omits the platform, it must pass.
- `tier` keeps a slow or heavy scenario out of `--quick`, whose 2-minute bound it would break.
- Headless fixtures follow the same rule in `crates/excise-harness/expectations/headless.toml`: an `[[expect_fail]]` entry names the fixture, the platforms, the findings (`F10`), and the discrepancy kinds.

### Running one scenario

```console
cargo xtask e2e --scenario NAME --profile deterministic    # pseudo-terminal runner; --repeat N shows the verdict is stable
cargo test -p excise --lib scenario_runner                 # in-process runner, the `quick` scenarios it can perform
```

The in-process run fails on a scenario file that does not parse or validate, but it skips, without any output, a scenario that has a pseudo-terminal-only step or sits outside its tier or platforms. Run a new scenario with `cargo xtask e2e --scenario NAME` to see it run. On Windows that command skips a scenario that has a `delete` step (it prints `SKIP NAME: ...`): `cargo test -p excise --lib scenario_runner` runs it there.

## Failure bundles

A failed scenario leaves a bundle at `target/excise-e2e/<run-id>/<scenario>-<profile>-<n>/`. `target/excise-e2e/latest` points at the newest run, whose `summary.json` holds every verdict. The bundle holds:

- `failure.json`: the failed step, the expected and the actual screen text, the terminal modes, resource use, the fixture hash and seed, and, when the step timed out, the session's diagnostics (output bytes, whether the child was still alive, and the raw output's head and tail);
- `session.cast`: an asciicast recording of the session;
- `screen.txt`: the failure and the screen at that moment;
- `events.jsonl`: the events the program emitted;
- `repro.txt`: the rerun command, `cargo xtask e2e --scenario NAME --profile PROFILE --keep-fixture`, and the exact `excise` invocation with its environment.

Read the bundle before you change anything, and quote it in the pull request. A headless fixture that fails leaves `target/excise-headless/<run-id>/headless-<fixture>/` with `discrepancies.txt`, `repro.txt`, and the scan report.

## Pull requests

Follow [CONTRIBUTING.md](CONTRIBUTING.md) and the pull request template. The description states the exact commands you ran and their results (the gate, `cargo xtask e2e --quick`, and any `full` scenarios or `cargo xtask headless` runs the change calls for), the documentation you updated by file and line or why none changed, and, for a defect, the platforms where it reproduces and the measured values. Commit messages follow Conventional Commits; `.gitmessage` lists the types and scopes.

A performance claim needs paired, interleaved A/B evidence from `cargo xtask bench-e2e --baseline <ref>`: it builds the baseline, alternates it with the candidate (baseline, candidate, baseline, candidate, ...) on the same warm fixture in the same session, and writes `target/excise-bench-e2e/<run-id>/ab.json` with the median of the per-pair ratios, a confidence interval, and the conditions: both commit SHAs, the toolchain, the host, the fixtures, the power state, and the load. Attach the printed table and the document. Close other `excise` sessions first: the tool warns about them, and `--strict` refuses to run. Timings from different sessions or machines, or from a single run, are never compared. A slowdown of more than 20%, or a memory change of more than 5%, whose confidence interval excludes no change blocks. Counts (entries, bytes, syncs) are exact and need no pairing.
