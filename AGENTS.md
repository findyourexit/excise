# AGENTS.md

Instructions for coding agents working on Excise, a terminal disk-usage navigator that can permanently delete what it shows. A wrong deletion is the worst bug there is, so safety outranks speed.

This file is canonical: `CLAUDE.md` and `.github/copilot-instructions.md` only point here. It summarizes [CONTRIBUTING.md](CONTRIBUTING.md), [docs/development.md](docs/development.md), and the [harness README](crates/excise-harness/README.md), which hold the detail. A change that alters a command, tier, or rule stated here updates this file in the same commit.

## Layout

- `src/`, `tests/`, `benches/`: the `excise` binary, its tests, and its benchmarks. `docs/` is the published documentation, with the `docs/safety/` contracts. `generated/` holds the man page and completions: `cargo generate` rewrites them and `cargo check-generated` verifies them.
- `crates/excise-harness/`: the internal, unpublished validation harness: `scenarios/`, `fixtures/`, `comparisons/`, the runners, and `schemas/` (never `docs/schemas/`, which release archives ship).
- `xtask/`: the `cargo xtask` commands: the harness runners (`e2e`, `headless`, `compare`), repository checks, and release tooling.

## Safety rules

- Run `excise` only through the harness, against the fixtures it generates. Never run it against a real path (`~`, a project, a mounted volume).
- Trigger deletions only through a scenario's `delete` step. It presses `y` only after the dialog names exactly the expected entry and every declared sentinel still exists, and a scenario with a `delete` step must declare sentinels.
- Every fixture root carries the ownership marker, a regular file named `.excise-harness-owned`, and every runner refuses a root without it. Never add the marker to a directory the harness did not generate.
- Leave no residue. Runs use scratch directories that are removed afterwards. If you pass `--keep-fixture` or `--keep-scratch`, remove what it kept when you are done, and check that no `xh-*` entry is left in `/tmp` or `$TMPDIR`.
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
cargo xtask bench-e2e --baseline main --fixture ID   # paired A/B evidence against another build
cargo xtask compare --full         # ratio budgets (motion_complete_ratio, tui_complete_ratio) between two runs of one binary
```

`e2e` builds the release binary first, or uses the one named by `EXCISE_E2E_BINARY`. `cargo verify` is the complete local suite and needs more tools than the gate; see [docs/development.md](docs/development.md).

## Tiers

A scenario's `tier` is `quick` (the default), `full`, or `nightly`, and says which `cargo xtask e2e` run includes it:

- `--quick` runs `quick` scenarios under the `default` and `deterministic` profiles only, and must stay within 2 minutes.
- `--full` (the default without a flag) runs `quick` and `full` scenarios under every profile each one declares.
- `--nightly` runs all three tiers under every profile each one declares.
- `--scenario NAME` runs the named scenario whatever its tier.

`cargo xtask e2e --quick` must pass before you report a change as done. A change to scheduling, animation, or scan-store code also needs the relevant `full` scenarios: name them, or run `--full`. `cargo test` runs the `quick` scenarios in-process and never runs `full` or `nightly` ones. `cargo xtask headless --quick` scans the fixtures of at most 10,000 planned entries and `--full` (the default) those of at most 250,000; `--fixture ID` runs one fixture of any size.

## Scenario authoring

A scenario is a strict TOML file (unknown fields are errors), `crates/excise-harness/scenarios/<name>.toml`, and it names its fixture by id: `fixture = "<id>"` means the spec `crates/excise-harness/fixtures/<id>.toml`, and you add a spec there when none fits. A scenario never names a root, and its paths are relative to the fixture root. Start from `scenarios/delete-folder-lifecycle.toml`; the harness README lists every field.

- Lifecycle scenarios declare the `default` and `deterministic` profiles and end with `quit` and `expect_exit` with `residue = "none"`.
- Wait with `wait_header` before you act, never for the screen to go idle. Every wait takes a `timeout_ms` (10 s by default).
- A scenario whose verdict depends on timing includes a step only the pseudo-terminal runner performs (`wait_event`, `measure`, `expect_budget`, `signal`), so that the in-process runner, which never judges timing, skips it.

The steps:

- `wait_text`: wait for text or a regex on the screen or in a region of it.
- `wait_header`: wait for the header badge to read `scanning` or `complete`, the only completion signal.
- `wait_event`: wait for an event-channel event such as `scan_complete` (pseudo-terminal runner only).
- `key`: press one key, with optional `ctrl` and `alt`.
- `type`: type literal text.
- `select`: select an entry by name through the filter, and check the inspector shows exactly it.
- `delete`: press Backspace, check the dialog and every sentinel, and only then press `y`. A mismatch fails the step and sends no `y`.
- `wait_fs_absent`: wait until a fixture-relative path no longer exists.
- `wait_fs_present`: wait until a fixture-relative path exists.
- `fs_mutate`: change the fixture while excise runs (`appear`, `change`, `vanish`, `replace`).
- `resize`: resize the terminal.
- `signal`: deliver a Unix signal or a Windows console event (pseudo-terminal runner only).
- `expect_screen`: assert what the screen shows now.
- `expect_fs`: assert which fixture-relative paths exist now.
- `expect_exit`: wait for the exit, and assert the exit code, the terminal restored, and no residue.
- `expect_budget`: assert that a recorded metric is within a named budget (pseudo-terminal runner only).
- `measure`: record the time between a `start` and a `stop` marker as a metric (pseudo-terminal runner only).
- `settle`: wait until the program has processed everything sent so far.
- `quit`: the confirmed quit, `q` and then `y`.

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

The in-process run fails on a scenario file that does not parse or validate, but it skips, without any output, a scenario that has a pseudo-terminal-only step or sits outside its tier or platforms. Run a new scenario with `cargo xtask e2e --scenario NAME` to see it run.

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
