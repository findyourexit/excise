# Development

!!! abstract "Maintain the bounded contracts"

    Changes to Excise must preserve its command-line, configuration, report, deletion, accounting, support, and release guarantees. Run the smallest relevant proof while developing, then use the complete verification gate before release work.

## Toolchain

The workspace uses Rust 1.98 and edition 2024. Install the pinned toolchain and supported compilation targets:

```console
rustup show
rustup target add \
  aarch64-apple-darwin \
  x86_64-apple-darwin \
  aarch64-unknown-linux-gnu \
  x86_64-unknown-linux-gnu \
  aarch64-pc-windows-msvc \
  x86_64-pc-windows-msvc
```

## Documentation Site

The published documentation site uses Zensical. Use Python `3.14.7`, matching the deployment workflow:

```console
python -m pip install --require-hashes -r .github/requirements-docs.txt # (1)!
zensical serve # (2)!
zensical build --clean # (3)!
```

1. Install only the reviewed, hash-locked documentation dependencies.
2. Serve the documentation locally while editing.
3. Build the deployable `site/` output before delivery.

Do not edit `site/`; it is generated and ignored. Keep `zensical.toml`, source under `docs/`, and `.github/requirements-docs.txt` in sync. When changing the direct Zensical version, regenerate the hash-locked file with the command in `.github/requirements-docs.in`.

## Target Evidence

The published targets have separate evidence for native behavior and release archives. Only targets with native runtime evidence are fully supported in stable v1.

| Target | Support classification | Published evidence |
|---|---|---|
| `x86_64-unknown-linux-gnu` (x86_64 Linux) | Supported in stable v1 and tested on the target system | Native checks and hosted release archive |
| `aarch64-apple-darwin` (AArch64 macOS) | Supported in stable v1 and tested on the target system | Native checks and hosted release archive |
| `x86_64-pc-windows-msvc` (x86_64 Windows) | Supported in stable v1 and tested on the target system | Native checks and hosted release archive |
| `x86_64-apple-darwin` (x86_64 macOS) | Build-only and best effort | Hosted release archive |
| `aarch64-unknown-linux-gnu` (AArch64 Linux) | Build-only and best effort | Hosted release archive |
| `aarch64-pc-windows-msvc` (AArch64 Windows) | Build-only and best effort | Hosted release archive |

Only rows marked **Supported** have native runtime evidence. The release pipeline publishes all six archives, but build-only targets have no native runtime guarantee until evidence supports them. A hosted build or archive proves release compilation and packaging, not runtime compatibility.

`cargo run --locked --package xtask -- check-support-matrix` checks these rows and workflow matrices; `cargo verify` includes it.

## Scope of Support

!!! info "Filesystem and terminal limits"

    The supported runtime policy applies to local filesystem paths accessed through documented operating-system APIs. Behavior can vary with filesystem types, access rules, network filesystems, copy-on-write files, clones, compression, and shared physical storage. These cases remain best effort unless they have separate evidence. Unknown allocated space remains explicit rather than guessed.

Interactive support requires stdin and stdout TTYs, ANSI rendering, alternate-screen support, and a window at least `32 x 8`. Table and JSON modes are the supported non-TTY path for redirection, pipelines, CI, and terminals without those capabilities.

## Fast Feedback

=== "Rust changes"

    ```console
    cargo fmt --all -- --check
    cargo check --workspace --all-targets --locked
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo test --workspace --locked
    ```

    Run the terminal lifecycle tests separately when terminal behavior changes:

    ```console
    cargo test --test pty_smoke --locked
    ```

=== "Documentation changes"

    ```console
    zensical build --clean
    ```

    Build the site and inspect the changed page, navigation, rendered diagrams, tabs, admonitions, and links.

## Test Event Channel

!!! warning "Internal testing interface"

    The test event channel exists for Excise's own tests and validation tooling. It is not part of the v1 command-line, configuration, or report contract, and it can change in any release. It is not a user setting, so the configuration reference deliberately omits it.

Set `EXCISE_TEST_EVENTS` to the path of a file that does not exist yet, outside the tree Excise scans, because a file inside that tree is scanned like any other. Excise creates that file exclusively, with mode `0600` on Unix, and appends one JSON object per line. It never opens or truncates an existing path, and on Unix a symbolic link at the path is rejected rather than followed. An empty value, an existing path, a missing directory, or any other creation failure is a configuration error: Excise reports it and exits with class 78 before it touches the terminal. Without the variable, Excise creates no file and starts no thread, and each emission site costs one branch.

Each event is written as one complete line and flushed, never synced to disk. If a write fails, the channel disables itself for the rest of the run, so a reader that went away cannot crash or stall the interface.

Events carry counts and timings only. They never contain names, paths, or other scan data.

Protocol version 1 objects start with `v` (always `1`) and `kind`, and end with `t_us`, the monotonic microseconds since the channel opened:

| `kind` | Fields | Emitted |
|---|---|---|
| `hello` | `version`, `pid` | First line. `version` is the Excise package version. |
| `frame` | `seq`, `inputs` | After every render that drew. `seq` counts drawn frames from 1. `inputs` counts the terminal input events the main loop has consumed so far. |
| `scan_complete` | `entries` | The initial scan finished and the map switched to its completed state. `entries` is the scanned-entry count. |
| `quit_prompt` | none | The quit dialog was built. It can be built again while background work finishes. |
| `deletion_finished` | `removed`, `failed` | A deletion worker reported. The counts come from its report. |
| `exit` | `code` | An interactive run is about to return its exit code. The terminal, if it was entered, has already been restored, unless it never absorbed the restoration output within a bounded wait. A process that panics or is killed emits none. |

An event marks a state change, not the screen that shows it. Wait for the next `frame` before reading the terminal.

A headless run (`--format json` or `--format table`) opens the channel and writes only its `hello` line.

`tests/pty_smoke.rs` consumes the channel on every platform today, and the validation harness will consume it later.

## Release Candidate Checks

!!! warning "Candidate input must be exact"

    A stable release preserves the v1 CLI, configuration, and report contract. Run every candidate command from the clean reviewed release commit. `cargo verify` uses `--allow-dirty` only for its local package-content listing; `cargo package --locked --list` and `cargo publish --locked --dry-run` remain strict and require a clean reviewed checkout.

### Local Candidate

```console
(
  set -euo pipefail
  cargo verify
  cargo run --locked --package xtask -- check-generated
  cargo run --locked --package xtask -- check-distribution
  cargo package --locked --list
  cargo publish --locked --dry-run
  cargo dist-local
)
```

The aliases in `.cargo/config.toml` map `cargo verify`, `cargo check-generated`, and `cargo dist-local` to locked `xtask` commands. `cargo package --locked --list` shows the exact crates.io file set. `cargo publish --locked --dry-run` validates packaging without uploading. `xtask dist-local` owns the local `dist/` staging path and writes the host archive, `dist/checksums.sha256`, and `dist/homebrew/excise.rb`; it does not publish or authorize a release.

??? info "Dispatch and collect the hosted candidate"

    Dispatch only from the exact protected `main` commit. Pass the manifest version, reviewed commit SHA, and a unique dispatch ID explicitly:

    ```console
    set -euo pipefail
    source_sha="$(git rev-parse HEAD)"
    version="$(sed -n 's/^version = "\([^\"]*\)"/\1/p' Cargo.toml | sed -n '1p')"
    test -n "$version"
    candidate_dir="$(mktemp -d "${TMPDIR:-/tmp}/excise-candidate.XXXXXX")"
    trap "$(printf 'rm -rf -- %q' "$candidate_dir")" EXIT
    dispatch_seed="$(date -u +%s)-$$-$RANDOM"
    if command -v sha256sum >/dev/null 2>&1; then
      dispatch_id="$(printf '%s' "$dispatch_seed" | sha256sum | cut -c1-32)"
    else
      dispatch_id="$(printf '%s' "$dispatch_seed" | shasum -a 256 | cut -c1-32)"
    fi
    run_url="$(gh workflow run release.yml --repo findyourexit/excise --ref main --field version="$version" --field source_sha="$source_sha" --field dispatch_id="$dispatch_id")"
    run_id="${run_url##*/}"
    if [[ ! "$run_id" =~ ^[0-9]+$ ]]; then
      run_id="$(
        candidate=""
        for attempt in 1 2 3 4 5; do
          if candidate="$(
            gh run list \
              --repo findyourexit/excise \
              --workflow release.yml \
              --event workflow_dispatch \
              --branch main \
              --commit "$source_sha" \
              --limit 20 \
              --json databaseId,headSha,headBranch,event,createdAt,displayTitle |
            jq -r --arg expected "$source_sha" --arg dispatch_id "$dispatch_id" '
              map(select(
                .headSha == $expected and
                .headBranch == "main" and
                .event == "workflow_dispatch" and
                .displayTitle == ("Excise release candidate " + $dispatch_id)
              ))
              | sort_by(.createdAt)
              | .[].databaseId
            '
          )"; then
            candidate_count="$(printf '%s\n' "$candidate" | sed '/^$/d' | wc -l | tr -d '[:space:]')"
            if [[ "$candidate_count" == "1" && "$candidate" =~ ^[0-9]+$ ]]; then
              printf '%s' "$candidate"
              break
            fi
            if (( candidate_count > 1 )); then
              echo "multiple workflow runs matched dispatch ID $dispatch_id" >&2
              exit 1
            fi
          fi
          sleep 2
        done
      )"
    fi
    if [[ ! "$run_id" =~ ^[0-9]+$ ]]; then
      echo "could not resolve the dispatched workflow run ID $dispatch_id: $run_url" >&2
      exit 1
    fi
    gh run watch "$run_id" --repo findyourexit/excise --exit-status
    gh run download "$run_id" --repo findyourexit/excise --name excise-release-candidate --dir "$candidate_dir"
    ```

??? info "Validate the candidate bundle"

    The workflow rejects a moving or unprotected source ref, checks the exact SHA and manifest version, and attests the six target archives, checksum manifest, and SBOM. In the temporary candidate directory, verify the checksum manifest, SBOM, archive contents, and every attestation before promotion:

    ```console
    (
      set -euo pipefail
      cd "$candidate_dir"
      if command -v sha256sum >/dev/null 2>&1; then
        sha256sum --check checksums.sha256
      else
        shasum -a 256 --check checksums.sha256
      fi
      jq -e '.packages | length > 1' excise.spdx.json
      jq -e '.packages[] | select(.name == "serde")' excise.spdx.json
      jq -e --arg version "$version" '([.packages[] | select(.name == "excise" and .versionInfo == $version)] | length == 1)' excise.spdx.json
      archives=(
        excise-x86_64-unknown-linux-gnu-v${version}.tar.gz
        excise-aarch64-unknown-linux-gnu-v${version}.tar.gz
        excise-x86_64-apple-darwin-v${version}.tar.gz
        excise-aarch64-apple-darwin-v${version}.tar.gz
        excise-x86_64-pc-windows-msvc-v${version}.zip
        excise-aarch64-pc-windows-msvc-v${version}.zip
      )
      for archive in "${archives[@]}"; do
        test -s "$archive"
      done
      for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
        archive="excise-${target}-v${version}.tar.gz"
        root="excise-${target}-v${version}"
        tar -tzf "$archive" | grep -Fqx "$root/excise"
        tar -tzf "$archive" | grep -Fqx "$root/LICENSE"
        tar -tzf "$archive" | grep -Fqx "$root/generated/man/excise.1"
        tar -tzf "$archive" | grep -Fqx "$root/schemas/scan-report.schema.json"
      done
      for target in x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
        archive="excise-${target}-v${version}.zip"
        root="excise-${target}-v${version}"
        unzip -t "$archive" >/dev/null
        unzip -Z1 "$archive" | grep -Fqx "$root/excise.exe"
        unzip -Z1 "$archive" | grep -Fqx "$root/LICENSE"
        unzip -Z1 "$archive" | grep -Fqx "$root/generated/man/excise.1"
        unzip -Z1 "$archive" | grep -Fqx "$root/schemas/scan-report.schema.json"
      done
      for subject in "${archives[@]}" checksums.sha256 excise.spdx.json; do
        gh attestation verify "$subject" \
          --repo findyourexit/excise \
          --signer-workflow findyourexit/excise/.github/workflows/release.yml \
          --source-digest "$source_sha" \
          --source-ref refs/heads/main
      done
    )
    ```

After review, create the annotated release tag with the candidate run ID in its message, then push it. The push-triggered workflow requires that exact annotated-tag candidate ID; never substitute a different candidate run or a lightweight tag.

```console
cargo create-release-tag "$version" "$source_sha" "$run_id"
git push origin "v$version"
```

## Full Verification

`cargo verify` runs the complete local suite. It expects:

- Cargo Deny 0.20.2;
- actionlint 1.7.12;
- lychee 0.24.2;
- Node.js/npm for Renovate 44.34.0 validation;
- cargo-fuzz 0.13.2 with the pinned fuzz toolchain; and
- every host-installable target listed above.

```console
cargo verify
```

The command checks formatting, workflow syntax, Renovate configuration, documentation links, compilation, cross-target compilation, strict Clippy, unit and snapshot tests, release-profile PTY budgets, package contents, dependency policy, bounded fuzz targets, benchmarks, generated files, published schemas, distribution templates, and release-binary size.

## Generated Files

```console
cargo generate
cargo check-generated
```

The man page and shell completions come from the Clap command definition. Commit generated changes with the source contract that produced them.

## Demo Assets

=== "Current-main hero"

    `cargo demo` supports current `main` development and delegates to `xtask demo`. Refresh the VHS demonstration after a user-visible CLI or TUI change, then review the output before release.

    ```console
    (
      set -euo pipefail
      cargo +1.98.0 build --release --locked --package excise
      cargo demo
    )
    ```

    Run the tape from the repository root. `xtask demo` validates `tapes/demo.tape`, captures at 24 fps, then resamples to 20 fps with a non-dithered 64-color palette and lossy GIF compression. It owns `assets/demo-main.rendered.gif`, `assets/demo-main.palette.gif`, and `assets/demo-main.quantised.gif`, and promotes the last file to `assets/demo-main.gif` only after its published weight limit passes. If any stage fails, the committed current-main asset remains untouched.

    The command needs `vhs`, `ttyd`, `ffmpeg`, `ffprobe`, and `gifsicle` on `PATH`, plus a Unix-like `bash` and core utilities. The tape explicitly selects `bash`, creates its fixture under `/tmp`, and uses `head`, `mkdir`, and `rm`. Running `vhs tapes/demo.tape` directly writes an unoptimized 24 fps recording to `assets/demo-main.rendered.gif`; it skips resampling, palette rebuild, compression, and the weight check. Do not use it to refresh the committed hero.

=== "README feature demos"

    Feature tapes in `tapes/features/` correspond to the eight README feature entries. They share a guarded fixture builder and keep generated files inside a disposable fixture. After building the release binary, run `cargo demo-features` or select demos, for example `cargo demo-features storage-map reports`. The command writes reviewed GIFs to `assets/features/` and promotes each only after its own duration, frame-count, and size checks pass.

    The `Demo recording` workflow keeps pull requests and `main` on the lightweight `hero` mode. Use its manual `features` mode to render the full feature suite on the pinned Linux toolchain, or `all` to render the hero and every feature together. The workflow uploads a review artifact but never writes or commits source files; explicitly review and commit a validated asset refresh.

## Fuzzing

The `fuzz` package is intentionally outside the main workspace. `cargo verify` and hosted fuzzing use the same toolchain selector from `xtask`. Update only `FUZZ_TOOLCHAIN` when changing the pinned nightly.

```console
fuzz_toolchain="$(cargo run --quiet --locked --package xtask -- fuzz-toolchain)"
cargo "+$fuzz_toolchain" fuzz list
cargo "+$fuzz_toolchain" fuzz run native_path -- -max_total_time=60 -max_len=4096
```

Crash artifacts and evolving corpora are ignored. Curated seeds under `fuzz/seeds` are reviewed source fixtures.

## Validation Harness

The `excise-harness` workspace crate under `crates/` holds the shared vocabulary of the black-box validation harness: the strict TOML scenario format and the versioned JSON Schemas for its machine output (run summaries, failure bundles, and A/B evidence). The harness observes `excise` only from the outside and never depends on the `excise` crate. It is internal test tooling rather than a supported product interface, and it is not published. Its schemas stay beside the crate instead of in `docs/schemas`, which ships in release archives.

```console
cargo test -p excise-harness --locked
```

`cargo verify` and hosted CI run these tests with the rest of the workspace. See the [harness README](https://github.com/findyourexit/excise/blob/main/crates/excise-harness/README.md) for the scenario reference, runner semantics, safety rules, and output documents.

The harness crate also holds the fixture generator: TOML fixture specs under `crates/excise-harness/fixtures/`, a seeded and deterministic generator with a cache in `target/excise-fixtures.noindex`, an independent `lstat` oracle of what a generated tree contains, and the live mutators behind `fs_mutate`. Its tests generate only small trees into temporary directories (the 50,000- and 1,000,000-entry specs are for runners and never generated by tests). Scratch volumes and mount boundaries need privileges on Linux and Windows, so they stay behind an explicit opt-in (`EXCISE_HARNESS_PRIVILEGED=1`), and the macOS `hdiutil` test runs only with it. The harness README documents the spec format, the oracle's JSON shape, and which file system capabilities each platform provides.

The `excise` test suite also runs the scenarios that need no separate process through the in-process runner in `src/tests/scenario_runner.rs`, so `cargo test --workspace --locked` exercises them; the harness README lists the steps it supports.

### Pseudo-terminal scenarios

`cargo xtask e2e` runs the scenarios in `crates/excise-harness/scenarios` against a real `excise` process in a pseudo-terminal. Each run gets a disposable copy of its scenario's fixture from the fixture generator, which carries the ownership marker, and a scratch `HOME`, configuration, working directory, and scan-store directory. The program is never run against a real path, and its whole process group is killed on a timeout or failure.

```console
cargo xtask e2e --quick
cargo xtask e2e --nightly
cargo xtask e2e --scenario delete-folder-lifecycle --repeat 20
```

Each scenario declares a `tier` (`quick`, the default; `full`; or `nightly`) and the platforms it runs on. `--quick` runs `quick`-tier scenarios under the `default` and `deterministic` profiles only, and must stay within two minutes; `--full` (the default) adds the `full` tier and every profile a scenario declares; `--nightly` adds `nightly` too. `--scenario` names a scenario and runs it whatever its tier, though a scenario outside its `platforms` is still skipped, with the reason, even when it is named; otherwise a scenario outside the selected tier or its `platforms` is skipped, with the reason, and the skip is printed. `--scenario` and `--profile` narrow the run and may repeat, and `--keep-fixture` keeps each run's fixture and scratch area. The command builds the release binary unless `EXCISE_E2E_BINARY` names one, prints a verdict table, and exits non-zero on any `fail`, `xpass`, or `error`. Before its first run it launches the binary once with `--version`, so that no measured session pays the one-time first-launch cost of a new binary; a failed warm-up stops the command with an error. It writes `target/excise-e2e/<run-id>/summary.json` (with `target/excise-e2e/latest` pointing at it) and one failure bundle per failed run: the failure document, the asciicast recording, a screen dump, the event file, and the exact command that reruns it. The `xtask` alias in `.cargo/config.toml` makes `cargo xtask <command>` the same as `cargo run --locked --package xtask -- <command>`.

`cargo test` also runs each scenario once per profile against the `excise` crate's own binary (`tests/harness_scenarios.rs`). It also runs a negative control from `crates/excise-harness/tests/controls`, which asks `delete` for the wrong entry and asserts that the step fails without a `y` being sent. Fixtures and scratch areas are created under `/tmp` on Unix, or under `EXCISE_E2E_TMPDIR`, and removed afterwards. On Windows, set `EXCISE_E2E_TMPDIR` to a short directory such as `C:\xh`. The deletion dialog is at most 78 columns wide, and the `delete` step refuses a path that the dialog cuts short.

`memory-budget-interactive-250k` (`full` tier) and `memory-budget-interactive-1m` (`nightly`)
check that an interactive scan's peak memory (`peak_rss_bytes`) stays within its budget (512 MiB
by default) on a 250,000- and a 1,000,000-entry fixture. `memory-budget-interactive-1m-cgroup`
(`nightly`, Linux only) runs the same 1,000,000-entry scan under the Linux cgroup memory cap,
checking that it still completes, exits normally, and keeps `peak_rss_bytes` within budget:
`EXCISE_HARNESS_CGROUP=1` and a scenario's own `cgroup_memory_cap = true` together spawn `excise`
under `systemd-run --scope` with `MemoryMax` set to the budget, so the kernel kills the scan
outright if it ever needs more than that, instead of this crate finding out after the fact. The
cgroup's own `memory.peak` (`cgroup_memory_peak_bytes`) is reported alongside `peak_rss_bytes`, not
gated: the cap bounds it by construction, so a check against the same budget could never fail.
Without the opt-in, or on a host that cannot do it, every runner skips such a scenario, with the
reason. The harness README documents the mechanism
(`crates/excise-harness/README.md#linux-cgroup-memory-cap`).

### Headless scans

`cargo xtask headless` scans the fixtures without a terminal (`excise --format json --output <report> <fixture>`), under the same isolation as the scenarios, holds every scan report to the oracle of its fixture under the accounting contract (directory metadata excluded, allocation once per identity, links not followed, unreadable entries uncertain, exit code against the report state), and times the scan against `du -sk` on the same warm fixture.

```console
cargo xtask headless --quick
cargo xtask headless --class hostile --class identity
cargo xtask headless --fixture node-modules-2k --repeat 5
EXCISE_HARNESS_PRIVILEGED=1 cargo xtask headless --class volumes
```

`--quick` runs the fixtures of at most 10,000 planned entries and `--full` (the default) those of at most 250,000. `--fixture` names fixtures and runs them whatever their size, and `--class` (`scale`, `identity`, `hostile`, `volumes`) selects by class. `--repeat N` is the number of timed pairs (five by default, interleaved as scan, `du`, scan, `du`, after one untimed warm-up pair), `--profile` is `default` or `deterministic`, `--timeout` bounds one scan or one `du` in seconds, and `--keep-scratch` keeps the scratch areas and reports. A fixture is scanned in its cached master below the target directory, generated once and then reused, except a fixture that `cargo clean` could not remove because it holds directories that cannot be listed or paths longer than `PATH_MAX`: that one is never cached and is scanned in a fresh copy, removed when the fixture is done. The command builds the release binary unless `EXCISE_E2E_BINARY` names one, prints one row per fixture with the headless and `du -sk` medians, the median and range of their ratio, and the oracle diff, and exits non-zero on any `fail`, `xpass`, or `error`. The ratio is checked against a 3x budget on a fixture whose oracle entry count (fixed by its spec and seed, never a measured time) is large enough to judge; below that it is reported but never gated. A count keeps the gate deterministic: which fixtures are judged never depends on how loaded the machine was. `expectations/headless.toml` names the fixtures and platforms expected to miss the budget today, with the same strict xfail semantics as the oracle diff, and the command exits non-zero on any other miss; both the budget and the entry threshold are expected to be revisited once the durable-write fix lands and ratios approach the budget. Volume fixtures run without privileges too, but then their mount points are empty directories and no boundary is crossed; the table says so. The summary is `target/excise-headless/<run-id>/summary.json`, and a failing fixture gets a directory beside it with its discrepancies, the command that reruns it, and the report. Fixtures that fail the diff for a known defect that is not yet fixed are listed, with the finding, in `crates/excise-harness/expectations/headless.toml` and show as `xfail`; the entry is removed by the change that fixes the defect.

`cargo test` also scans the cheap fixtures that need no privileges against the `excise` crate's own binary (`tests/harness_headless.rs`) and runs a negative control: a binary that writes a report that breaks the published schema must fail the run.

Every scan is also held to the memory contract: its peak memory (`peak_rss_bytes`, where the
platform can measure it) must stay at most 512 MiB, with the same strict-xfail semantics as the
oracle diff (`[[expect_memory_fail]]` in `expectations/headless.toml`). `EXCISE_HARNESS_CGROUP=1`
wraps every scan under the Linux cgroup memory cap (`systemd-run --scope`) while this host can do
it; `EXCISE_HARNESS_CGROUP=1 cargo xtask headless --fixture tiny-files-1m --repeat 0` is the
nightly-tier check at the 1,000,000-entry size (`--repeat 0` scans it once; the default five pairs
would cost that many scans of the slowest fixture this program has).

### Paired A/B benchmark

`cargo xtask bench-e2e --baseline <ref>` builds (or accepts, via `--baseline-binary`/`--candidate-binary`) a baseline and a candidate `excise` binary and compares them with paired, interleaved A/B runs on the same warm fixture: headless fixture scans (wall time, user and system CPU, peak memory) and PTY scenario runs (`scan_complete_ms`, `first_frame_ms`, `input_to_frame_p99_ms`, `max_stall_ms`, `peak_rss_bytes`, and any `measure` names), after one untimed warm-up pair.

```console
cargo xtask bench-e2e --baseline main --fixture wide-1k --pairs 5
cargo xtask bench-e2e --baseline v1.3.0 --scenario delete-folder-lifecycle --profile deterministic
```

For every metric it reports the median candidate/baseline ratio and a deterministic bootstrap 95% confidence interval (`--seed`), and applies a verdict: a timing metric blocks past a 20% regression (`--timing-threshold`) with a confident interval, a memory metric blocks past a 5% move either direction (`--memory-tolerance`) with a confident interval, and the command exits non-zero on any block. The document is `target/excise-bench-e2e/<run-id>/ab.json`, a `harness-ab` document that records both builds' identities and the session's context (host, toolchain, power state, load average, and concurrent `excise` processes) because timings never transfer across sessions. `cargo test` runs the same comparison logic with the crate's own binary as both sides, on a small fixture, as a schema and verdict regression check (`tests/harness_bench.rs`).

See the [harness README](https://github.com/findyourexit/excise/blob/main/crates/excise-harness/README.md) for the full reference.

### Session residue

`tests/harness_sessions.rs` covers the startup sweep of dead scan-store sessions (`src/scan_store/sweep.rs`; the behavior is described under "Scratch directory cleanup" in the configuration guide). Its first test builds one scan-store parent shared by several `excise` processes: a directory with the `.excise-scan-*` name shape that no `excise` process ever made, a session kept running in its own pseudo-terminal until the test quits it, and a session killed with `SIGKILL` (process termination on Windows) once its lock file carries the owner marker. A further `excise` finishes a headless scan against the same parent. Its start must remove the dead session's directory and leave the other two alone. The second test starts four `excise` processes at once, in three rounds, against a parent that also holds a dead session, and requires every scan to finish with exit 0: a sweep that removed a session being set up or in use would fail that session's scan. The unit tests beside `src/scan_store/sweep.rs`, `session_lock.rs`, and `storage.rs` cover the rest. They sweep at every step of a session's setup, to pin the order in which it locks its file and writes its marker; check that the lock is released before the directory is removed and the marker outlives the working files; and check what a sweep must leave alone (links, other users' directories and directories open to others, a marker that is not the whole record, a lock someone holds).

### Error exits

`tests/harness_error_exits.rs` runs `excise` against a fixture with `EXCISE_TEST_EVENTS` pointed at
a path that already exists. The channel is created exclusively and never overwrites an existing
file, so this is a configuration error, caught and reported before the terminal is entered. The
process must exit 78 and leave nothing behind in its scratch area.

The PTY scenario `error-exit-unreadable-root.toml` covers the other side: a root containing files
and directories `excise` cannot read still reaches an exit code that reflects the uncertainty
(2), restores the terminal, and leaves no residue, after an ordinary quit.

## Benchmarks

The hosted `benchmark.yml` retains the `criterion-benchmark-evidence` artifact for 90 days. It contains Criterion raw samples and reports from `target/criterion`, one-million and bounded-fan-in probe logs, plus `benchmark-context.txt`, which records the checked-out SHA, workflow run, runner image and CPU, commands, Rust toolchain, and `Cargo.lock` digest.

=== "Run locally"

    ```console
    cargo +1.98.0 bench --bench tachyonfx --features internal --locked -- --noplot
    cargo +1.98.0 bench --bench core --features internal --locked -- --noplot
    ```

    The hosted workflow runs one-million-tiny-file and bounded-batch fan-in probes once with `--profile-time 1`. The premerged probe isolates reduction and late-page lookup cost; the bounded probe exercises production fan-in. Both use explicit private scan-store limits and reproduce locally:

    ```console
    EXCISE_BENCH_MILLION=1 cargo +1.98.0 bench --bench core --features internal --locked -- scan-store/million-tiny-files --noplot --profile-time 1
    EXCISE_BENCH_MILLION=1 EXCISE_BENCH_MILLION_FANIN=1 cargo +1.98.0 bench --bench core --features internal --locked -- scan-store/million-tiny-files/bounded-fan-in --noplot --profile-time 1
    ```

    Both probes print deterministic logical read and write bytes, per-observation ratios, merge write amplification, retained and peak temporary bytes, phase wall time, and Unix process CPU time. `--profile-time 1` performs one scale smoke; omit it on provisioned comparable hardware when collecting Criterion samples.

    `core` measures publication and late-page queries across flat, wide, deep, and shared-link workloads (`scan-store/publication/*` and `scan-store/page-query/*`). With `EXCISE_BENCH_MILLION=1`, it adds one-million-file publication and late-page probes. `EXCISE_BENCH_MILLION_FANIN=1` also exercises production bounded fan-in. It measures a fixed 16,512-entry filesystem walk at one, two, and eight workers, delivery of sixteen focus requests during an active scan, and rebuild-cancellation acknowledgement. It also runs the production owner loop over a 597-entry tree (`owner-loop/scan-ingestion/*`, below). `tachyonfx` measures completion-frame processing at `80x24`, `160x50`, and `200x80`.

    `owner-loop/scan-ingestion` runs the production owner loop (`runtime::run`) until its scan completes, then quits it with `Ctrl-C` and `y`. The tree is a deterministic `node_modules` shape: fan-out 4, depth 3, and eight one-byte files per leaf directory, 597 entries in all. The run uses an in-memory `TestBackend`, the system clock, and explicit scanner-thread, reduced-motion, and loading-animation settings, so it measures the loop's own work and not a terminal's write speed. It runs twice, as `reduced-motion` and `default-motion`, because map animation and scan ingestion share the loop's scheduling; comparing the two `time_to_complete_ms` and `entries_per_second` values shows whether animation defers scan work. Each run has a wall-clock cap (30 s for reduced motion, 10 s for default motion). A scan still running at its cap is cancelled and reported with `complete=false`, so the group cannot hang.

    ```console
    cargo +1.98.0 bench --bench core --features internal --locked -- owner-loop --noplot
    ```

    After Criterion's timing for each case, the probe report of that case's last sample is printed to standard error: one summary line, one line for each phase, and one line for each worker-event kind that occurred.

    ```text
    owner-loop/<case>: complete=…, fixture_entries=…, entries_handled=…, entries_per_second=…, wall_ms=…, time_to_complete_ms=…, frames=…, runs_admitted=…, worker_events={scan_batch=…,scan_unscanned=…,…}
    owner-loop/<case>/phase/<phase>: count=…, total=…, max=…, p99_bucket=…, p99_bucket_upper_bound=…
    owner-loop/<case>/worker_event/<kind>: count=…, total=…, max=…, p99_bucket=…, p99_bucket_upper_bound=…
    ```

    In the summary line, `entries_handled` is the number of scanned entries the loop applied to its model, and `entries_per_second` divides it by `time_to_complete_ms`, or by the whole run when the scan did not finish. `wall_ms` covers the whole run: startup, scan, quit, and worker shutdown. `time_to_complete_ms` ends when the loop has swapped in the finished scan's published map, which is when the interface shows the scan complete. `frames` counts frames drawn (`render` calls that drew), and `runs_admitted` counts sealed scanner runs handed to the scan-store thread, and `worker_events` counts events handled by kind.

    Every phase and worker-event line reports the sample `count`, the summed `total`, and the exact `max`. A phase sample is the wall time of one occurrence, measured with real `Instant`s and never the loop's logical clock: `input` is one input event, `render` one frame drawn, `admission` one handoff of sealed runs to the scan-store thread, which admits them, and `publication` the loop's part of publishing the primary scan generation: swapping in the generation the scan-store thread built and reading its first page. A worker-event sample times the whole handler, so `admission` runs inside `scan_batch` and `scan_unscanned`. The scan-store thread's own work (admitting, merging, publishing) is not an owner-loop phase.

    Durations live in fixed-size histograms of 64 power-of-two nanosecond buckets, so memory stays constant however long a run lasts. `p99_bucket` is the index `i` of the bucket `[2^i, 2^(i+1))` ns that holds the 99th-percentile sample. `p99_bucket_upper_bound` is that bucket's upper edge, tightened to `max`: a bound the 99th-percentile sample does not exceed.

=== "Compare hosted evidence"

    Obtain reference and candidate run IDs from their checks, download both evidence artifacts, and inspect contexts before comparing measurements:

    ```console
    set -euo pipefail
    repo=findyourexit/excise
    reference_run=<reference-run-id>
    candidate_run=<candidate-run-id>
    evidence_dir="$(mktemp -d)"
    mkdir "$evidence_dir/reference" "$evidence_dir/candidate"
    gh run download "$reference_run" --repo "$repo" \
      --name criterion-benchmark-evidence --dir "$evidence_dir/reference"
    gh run download "$candidate_run" --repo "$repo" \
      --name criterion-benchmark-evidence --dir "$evidence_dir/candidate"
    reference_context="$(find "$evidence_dir/reference" -type f -name benchmark-context.txt -print -quit)"
    candidate_context="$(find "$evidence_dir/candidate" -type f -name benchmark-context.txt -print -quit)"
    test -n "$reference_context" && test -n "$candidate_context"
    diff -u "$reference_context" "$candidate_context" || true
    ```

    Compare matching paths only when runner OS, architecture, image, CPU, and Rust toolchain are comparable. This script reads each saved Criterion median and confidence interval without introducing a pass/fail threshold:

    ```console
    python3 - "$evidence_dir/reference" "$evidence_dir/candidate" <<'PY'
    import json
    import sys
    from pathlib import Path


    def medians(root):
        return {
            path.parent.parent.relative_to(root).as_posix(): json.loads(path.read_text())["median"]
            for path in root.rglob("new/estimates.json")
        }


    reference, candidate = (medians(Path(root)) for root in sys.argv[1:])
    for name in sorted(reference.keys() | candidate.keys()):
        if name not in reference or name not in candidate:
            print(f"{name}: only present in one artifact")
            continue
        old, new = reference[name], candidate[name]
        change = (new["point_estimate"] / old["point_estimate"] - 1) * 100
        old_ci = old["confidence_interval"]
        new_ci = new["confidence_interval"]
        print(
            f"{name}: {old['point_estimate']:.0f} ns "
            f"[{old_ci['lower_bound']:.0f}, {old_ci['upper_bound']:.0f}] -> "
            f"{new['point_estimate']:.0f} ns "
            f"[{new_ci['lower_bound']:.0f}, {new_ci['upper_bound']:.0f}] "
            f"({change:+.2f}%)"
        )
    PY
    ```

=== "Reproduce with Criterion"

    If evidence shows a meaningful difference, repeat it in a clean disposable checkout on comparable hardware. Use the recorded source SHAs and toolchain, retain the reference baseline in `target/criterion`, then let Criterion compare:

    ```console
    toolchain="$(sed -n 's/^requested_toolchain=//p' "$candidate_context")"
    reference_sha="$(sed -n 's/^source_sha=//p' "$reference_context")"
    candidate_sha="$(sed -n 's/^source_sha=//p' "$candidate_context")"
    reference_ref="$(sed -n 's/^ref=//p' "$reference_context")"
    candidate_ref="$(sed -n 's/^ref=//p' "$candidate_context")"
    test -n "$toolchain" && test -n "$reference_sha" && test -n "$candidate_sha" && test -n "$reference_ref" && test -n "$candidate_ref"
    git fetch origin "$reference_ref" "$candidate_ref"
    rustup toolchain install "$toolchain" --profile minimal
    git switch --detach "$reference_sha"
    cargo +"$toolchain" bench --bench tachyonfx --features internal --locked -- --noplot --save-baseline reference
    cargo +"$toolchain" bench --bench core --features internal --locked -- --noplot --save-baseline reference
    git switch --detach "$candidate_sha"
    cargo +"$toolchain" bench --bench tachyonfx --features internal --locked -- --noplot --baseline reference
    cargo +"$toolchain" bench --bench core --features internal --locked -- --noplot --baseline reference
    ```

Treat small host-local changes as noise unless repeated statistical evidence on comparable hardware supports them.

## Pull Requests

See [CONTRIBUTING.md](https://github.com/findyourexit/excise/blob/main/CONTRIBUTING.md) for review, safety, accessibility, authorship, and documentation requirements.
