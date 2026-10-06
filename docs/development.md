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
| `hello` | `version`, `pid`, `frame_marks`, `input_barrier` | First line. `version` is the Excise package version. `frame_marks` is `true`: an interactive run of this binary follows every `frame` event with a mark in the terminal output (below). `input_barrier` is `true`: this binary answers input barrier requests (see "Input barrier" below). Binaries from before the marks omit `frame_marks`, and binaries from before the barrier omit `input_barrier`. |
| `frame` | `seq`, `inputs`, `barriers` | After every render that drew, once the frame is queued for the thread that writes to the terminal, which can still hold its bytes back. `seq` counts drawn frames from 1. `inputs` counts the terminal input events the main loop has consumed so far. `barriers` counts the input barrier requests the main loop has consumed so far, which are not inputs: it is 0 until the first request. Binaries from before the barrier omit `barriers`. |
| `scan_complete` | `entries` | The initial scan finished and the map switched to its completed state. `entries` is the scanned-entry count. |
| `quit_prompt` | none | The quit dialog was built. It can be built again while background work finishes. |
| `deletion_finished` | `removed`, `failed` | A deletion worker reported. The counts come from its report. |
| `refresh_finished` | `outcome` | The map on screen has caught up with the deletions that removed entries: the map without them was published (`published`), or no map could be shown (`failed`), and no rebuild or publication is owed. It follows the `deletion_finished` it answers, and a refresh that covered several deletions is reported once. A deletion that removed nothing owes none. |
| `exit` | `code` | An interactive run is about to return its exit code. The terminal, if it was entered, has already been restored, unless it never absorbed the restoration output within a bounded wait. A process that panics or is killed emits none. |

An event marks a state change, not the screen that shows it. A `frame` is reported once the frame is queued for the thread that writes to the terminal, and the terminal can still hold the bytes back for as long as it likes, so a reader that needs the screen to show a frame goes by that frame's mark instead of the event. The mark is ESC `]` `9471` `;` `excise-frame=`, the `seq` of the frame in decimal, and BEL: `"\x1b]9471;excise-frame=<seq>\x07"`. Excise writes it only while `EXCISE_TEST_EVENTS` is set, and queues it right behind the frame's own bytes through the same writer, which never reorders or drops what it is handed. On a Unix pseudo-terminal, which relays the output in order, the mark therefore follows its frame's bytes: a reader that has read up to the mark has read every byte of that frame and none of the next, so one that feeds a screen model stops at the mark and reads the screen there, and reading up to the mark is exact. Windows `ConPTY` re-renders the output instead of relaying it, and there the mark arrives before the paint of its frame (below). A frame whose event could not be written, because the channel had ended, gets no mark either, and only an interactive run, which has a terminal writer, writes marks at all.

`hello.frame_marks` says that a binary writes marks. Binaries from before it do not, and a reader of one has to fall back to reading until the output has been quiet for a while: a Unix pseudo-terminal delivers a frame's bytes within a thread switch, but Windows `ConPTY` paints on its own schedule and delivered frames up to 22 ms after their event in the recordings that the harness README (`crates/excise-harness/README.md`) cites under `settle`, so the harness reads for 100 ms there.

A terminal that rewrites the stream instead of relaying it, as Windows `ConPTY` does, passes the mark on, but ahead of the paint of its frame: `ConPTY` parses the program's output into a buffer of its own, passes the mark through as soon as it has parsed it, and paints the screen later, on its own timer. Observed in CI run 37376817720 (the Windows pseudo-terminal tier): the marks do reach the harness, and they arrive before the paint of their frame (in one recording, mark 10 at 0.133 s and the 9,756-byte paint that shows frame 10 at 0.142 s). So on Windows the mark says that the console host has parsed every byte of that frame into its buffer, not that the screen model shows the frame, and no quiet-time rule can prove a `ConPTY` paint complete: a repaint can be split after a cursor or control prefix, and a paint begun before the mark can be flushed after it, leaving a stale dialog on the screen.

Whether the pseudo-terminal can tie its screen to a frame exactly is a capability of the validation harness, `SCREEN_IS_EXACT` in `crates/excise-harness/src/runner/live.rs`: true on Unix, where a frame's mark follows its bytes, so the screen is exact once the mark is read, and false on Windows. Where it is false, the harness never confirms a deletion from the screen. The `delete` step, in both modes (with the confirmation dialog and with `disable_delete_confirmation`), and the interactive driver's `delete` and its confirmation guard refuse before any key, not even Backspace: the step fails with `DeleteRefused` (the driver with the error kind `refused`) and says that the terminal repaints the screen on its own timer (the console host of Windows), so the harness cannot tie the screen to a frame and confirms no deletion from it, and that the scenario runs in-process under `cargo test`. The reads that follow the Backspace in the protocol stay as a second line of defence; nothing can satisfy them there.

Reads that decide nothing destructive wait a bounded time on Windows instead: `settle`, `select`, `resize`, `wait_refresh`, the first frame, and the `expect_*` steps after a `settle`. After the mark they wait for the first output that is not a mark (the mark of another frame arriving first does not count), and then for the bounded quiet read, output quiet for 3 ms for at most 20 ms; or for the frame window (`CONPTY_FRAME_WINDOW`, 100 ms) to pass after the mark with nothing painted, which is taken to mean that nothing needed painting. Every part is bounded by the step's deadline (`timeout_ms`), so a mark that never comes still makes the step time out. Where the capability is true, as on Unix, the mark alone decides, as above. A program whose `hello` lacks `frame_marks` (a build from before the marks) is read as `hello.frame_marks` describes above, and everything that could confirm a deletion refuses it.

Where the capability is false, `cargo xtask e2e` also skips every scenario that has a `delete` step, in either mode, and every scenario that presses Backspace itself with a `key` or `type` step, or composes an escape sequence from its keys (`alt+[` and then the text `121u` are `ESC [ 121 u`, which the program reads as `y`, though no write holds a `y`), and prints the reason (`SKIP name: ...`), even when the scenario is named with `--scenario`; the in-process runner is unchanged. On an exact screen a scenario's own Backspace, or a write that continues an escape sequence that an earlier write began, engages its run: a `y`, Enter, filter text, or quit confirmation after it is sent only behind an input barrier, on a screen that shows no deletion dialog (the harness README says how, under `key`). The runner reads every byte it writes with one scan and decodes none of it: a sequence that one write begins and a later write continues counts as both a request for a deletion and a confirmation, whatever its bytes, and a write that would continue `ESC [` is refused, since no barrier can be written behind it. The harness README (`crates/excise-harness/README.md`) has the same rule under `settle`, and "Continuous integration tiers" below says what the Windows tiers run.

`inputs` counts every input event the main loop consumed, including ones the terminal sends by itself, so it can be above zero before any key is sent: in recordings from Windows it was already 1 in the first frames, and on macOS it was 0. A reader that matches a frame to a key by counting takes the `inputs` of the latest frame when it sends its first key as a baseline, and looks for a frame whose `inputs` is at least that baseline plus the keys sent. `tests/pty_smoke.rs` does, for the single-byte keys that it writes one at a time. Counting is sound only while it is known how many input events each write makes, and a terminal write is not one decoded input event in general (see "Input barrier" below), so a reader that has to be exact about what the program has read uses the input barrier instead.

A headless run (`--format json` or `--format table`) opens the channel and writes only its `hello` line.

`tests/pty_smoke.rs` and the validation harness consume the channel. `tests/pty_smoke.rs` also checks that the marks in the terminal output are exactly the `frame` events (except on Windows, where `ConPTY` repaints the screen) and that nothing is marked without the variable. On Unix it also writes input barrier requests in a pseudo-terminal and checks that the next frame answers each, that none of them is counted as an input, that the answering frame shows what was written right before the request, and that without the variable the byte changes nothing.

### Input barrier

A reader that writes keys to the terminal has to know when the program has read them and drawn what they did. Counting its writes against `inputs` cannot tell it, because a terminal write is not one decoded input event: crossterm decodes `ESC DEL` as Alt+Backspace when both bytes arrive in one read and as Esc and then Backspace when they do not, and `ESC ESC [ A` as Esc, `[`, and `A`. The program does read its input in order, and a barrier request uses that.

A barrier request is the single byte `0x1D` (Ctrl+]) written to the program's terminal input. crossterm's Unix input parser decodes it as the key `5` with the Control modifier, and, when an unread lone `ESC` byte immediately precedes it, as the same key with Alt added, because the parser merges `ESC` and the byte after it into one Alt key. Both forms are requests. No other modifier combination is one, and neither is an event that is not a key press. This is the decoding of a terminal that crossterm's Unix parser reads; the Windows console builds its key events from console input records instead, and the byte was not checked there. A request is recognized only after whole keys, because a byte inside an unfinished escape sequence belongs to that sequence.

The byte is a request only while `EXCISE_TEST_EVENTS` is set. Without the variable it is Ctrl+5, an ordinary key that nothing is bound to, and nothing is counted or drawn for it. While the channel is open, the main loop handles a request as soon as it has read it. The request is not an input: it is not counted in `inputs`, no key binding sees it, and an `ESC` that merged into it is neither counted nor handled as the Esc key, so a reader that wants the Esc key read as one waits for a frame that counts it before it writes a request. The request is counted in `barriers` instead, and the loop draws a frame for certain, even behind one that the terminal is still draining. The next `frame` event answers it: its `barriers` is the number of requests consumed before that frame was drawn. Requests count from 1 in the order the program read them, and the first `frame` whose `barriers` is at least a request's number answers it. Every `frame` carries `barriers`, which is 0 until the first request.

The frame is drawn after the request was consumed, and the program reads its input in order, so an answered request proves that everything written before it was read and drawn, whatever the number of input events those writes made. The answering frame shows all of it, and its mark in the terminal output (above) says when the screen does. A request does not wait for background work that an input started, such as a deletion or a rescan. The barrier is as internal and unversioned as the rest of the channel. Both fields are additive, so the protocol version stays 1: a binary from before the barrier omits `input_barrier` and `barriers` and answers no request.

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

### Deletion Lifecycle

`deletion_lifecycle` fuzzes the one irreversible thing Excise does against a reference model. Each input builds a small fresh fixture in a private temporary directory (folders, files, an empty folder, symbolic links, a hard-linked pair, a hard link to a file outside the scan root, and names that sort in different orders), runs the real runtime in-process on a script of keys, filters, resizes, and deletions, and changes the live tree while it runs: create, remove, rename, replace a file with a folder, a link, or a hard link, add or remove a hard link, grow a file. One variant of the fixture also holds a folder and a file that carry the names the mutations give new entries, so that a rename or a replacement can land on an entry the planner reviewed. The script's mutations run whenever the runtime is idle; an armed mutation runs on the deletion executor's own thread, after the planner reviewed a target and before the executor's final check.

The model does not predict the runtime. It looks at the tree only when nothing but the runtime can change it (while the runtime is idle, and as the executor takes a plan), and holds each stretch against what the runtime itself says it did. It hears that through a small probe (`excise::fuzz::deletion::DeletionProbe`) that exists only with the `fuzzing` feature, so the release binary is unchanged. The owner loop says when the interface accepts a request to delete and when it accepts the confirmation, each with the work item's id, while it handles the key. The executor says when it takes a work item, with the plan and every entry the planner reviewed, and when it is done, with its report, or with none when its final check refused the plan. The model keeps each plan's reviewed entries for that deletion alone, whatever its report holds. It also numbers every entry of the tree itself, a generation, because a file system may hand a replacement the identity of the entry it replaced, and a replacement is not the entry that was reviewed. A name gets a new number only when a mutation really changed what it holds: a rename that fails, a rename of an entry onto its own name or onto another name of the same file, and a hard link replaced by another name of the same file leave the entry, and its number, as they were.

It fails with a message that names the step and the paths when:

1. an entry disappears that the plan of a confirmed deletion did not review, as that entry (one created or replaced after the review is not it, even when it carries a reviewed identity); a name inside a confirmed target goes that its plan did not review, or that its complete report does not list as deleted; an entry reported deleted is not the one reviewed, or is still there; or the executor takes a work item that no accepted request and confirmation paid for (a request carries the id its work item was given, the consent is a confirming key or, in reduced confirmation mode, the request itself, and it belongs to that id: the executor spends it by taking that work item, once, and no other);
2. anything outside every confirmed target changes, including the sentinels beside the scan root and what a symbolic link or a hard link points at;
3. the interface does not return to a navigable state: the fixed closing keys (settle, four Esc, then `q`, `c`, `s`, `y`) must end the run, and one input takes at most 45 seconds in all, counted from when it starts;
4. the terminal is not restored: its session must begin by clearing the screen and hiding the cursor, draw whole frames, and end by clearing the screen and showing the cursor.

A report that is not complete, because the runtime could not store all its results, does not switch the checks off: the plan still says what was reviewed, and only the outcomes the report leaves out are unknown.

Run it long, in one process, with (the nightly workflow runs exactly this on Linux):

```console
mkdir -p fuzz/corpus/deletion_lifecycle
cargo "+$fuzz_toolchain" fuzz run deletion_lifecycle fuzz/corpus/deletion_lifecycle fuzz/seeds/deletion_lifecycle -- -max_total_time=1800 -max_len=128 -len_control=0
```

A run takes a few hundred milliseconds, most of it the runtime's own start and stop, so expect five to eight inputs per second. The target prints what it has reached every hundred inputs: dialogs shown, confirmed deletions, entries deleted, cancelled deletions, mutations (and how many came while a dialog was open, then confirmed), mutations between a plan's review and the final check, and plans the final check refused. A crash leaves at most one directory named `excise-fuzz-deletion-lifecycle-*` in the temporary directory.

Every key, filter, and resize step waits for the runtime to settle first, so what a step does depends on the input alone, and a saved input replays the way it ran. `EXCISE_FUZZ_TRACE=1 cargo "+$fuzz_toolchain" fuzz run deletion_lifecycle fuzz/artifacts/deletion_lifecycle/crash-…` prints the trace and the last screen, and a line `replay input <fingerprint> trace <digest>`; the digest, which leaves out the fixture's directory, is the same in every replay of an input. The one exception is explicit: a `^` before a step takes the settle away, so that the step races the runtime's workers (a quit right behind a confirmation), and an input that holds one may go differently each time, so a crash that needs one may not replay. The input language, which the curated seeds in `fuzz/seeds/deletion_lifecycle` are written in, is documented in `fuzz/fuzz_targets/deletion_lifecycle/script.rs`.

Every mutation runs wherever the file system lets it, except two. Where the platform does not create symbolic links (anything but Unix), the fixture has none, and the mutation that replaces an entry with one leaves the tree as it is, without removing the entry first. Where the number of names an entry has is not read (also anything but Unix), the mutation that removes a hard link finds no entry to act on. Only the Unix paths have run.

libFuzzer replays everything in `fuzz/corpus/deletion_lifecycle` before it mutates anything, and `cargo xtask verify` runs the target from the same directory. At about a fifth of a second per saved input, the roughly 1,800 inputs a 30-minute run keeps take five and a half minutes to replay, so delete the directory after a long run, before you verify.

The target does not generate hostile names or type a confirmation challenge, and it models the reduced confirmation mode as the runtime behaves, where the accepted Backspace is the whole request: it carries its work item's id like any other, and the executor must take that work item.

## Validation Harness

The `excise-harness` workspace crate under `crates/` holds the shared vocabulary of the black-box validation harness: the strict TOML scenario format and the versioned JSON Schemas for its machine output (run summaries, failure bundles, A/B evidence, the version sweep's table, and the documents of the interactive session driver). The harness observes `excise` only from the outside and never depends on the `excise` crate. It is internal test tooling rather than a supported product interface, and it is not published. Its schemas stay beside the crate instead of in `docs/schemas`, which ships in release archives.

```console
cargo test -p excise-harness --locked
```

`cargo verify` and hosted CI run these tests with the rest of the workspace. See the [harness README](https://github.com/findyourexit/excise/blob/main/crates/excise-harness/README.md) for the scenario reference, runner semantics, safety rules, and output documents.

The harness crate also holds the fixture generator: TOML fixture specs under `crates/excise-harness/fixtures/`, a seeded and deterministic generator with a cache in `target/excise-fixtures.noindex`, an independent `lstat` oracle of what a generated tree contains, and the live mutators behind `fs_mutate`. Its tests generate only small trees into temporary directories (the 50,000- and 1,000,000-entry specs are for runners and never generated by tests). Scratch volumes and mount boundaries need privileges on Linux and Windows, so they stay behind an explicit opt-in (`EXCISE_HARNESS_PRIVILEGED=1`), and the macOS `hdiutil` test runs only with it. The harness README documents the spec format, the oracle's JSON shape, and which file system capabilities each platform provides.

The `excise` test suite also runs the scenarios that need no separate process through the in-process runner in `src/tests/scenario_runner.rs`, so `cargo test --workspace --locked` exercises them; the harness README lists the steps it supports. On Windows, where the pseudo-terminal runner skips the scenarios that have a `delete` step, this run performs them (see "Pseudo-terminal scenarios").

### Pseudo-terminal scenarios

`cargo xtask e2e` runs the scenarios in `crates/excise-harness/scenarios` against a real `excise` process in a pseudo-terminal. Each run gets a disposable copy of its scenario's fixture from the fixture generator, which carries the ownership marker, and a scratch `HOME`, configuration, working directory, and scan-store directory. The program is never run against a real path, and its whole process group is killed on a timeout or failure.

```console
cargo xtask e2e --quick
cargo xtask e2e --nightly
cargo xtask e2e --scenario delete-folder-lifecycle --repeat 20
```

Each scenario declares a `tier` (`quick`, the default; `full`; or `nightly`) and the platforms it runs on. `--quick` runs `quick`-tier scenarios under the `default` and `deterministic` profiles only, and must stay within two minutes, which a run of the whole tier checks (see [Continuous integration tiers](#continuous-integration-tiers)); `--full` (the default) adds the `full` tier and every profile a scenario declares; `--nightly` adds `nightly` too. `--scenario` names a scenario and runs it whatever its tier, though a scenario outside its `platforms` is still skipped, with the reason, even when it is named, and so is a scenario that has a `delete` step, or presses Backspace itself with a `key` or `type` step, wherever the terminal's screen cannot be tied to a frame exactly (Windows; see "Test Event Channel"); otherwise a scenario outside the selected tier or its `platforms` is skipped, with the reason, and the skip is printed. `--scenario` and `--profile` narrow the run and may repeat, and `--keep-fixture` keeps each run's fixture and scratch area. The command builds the release binary unless `EXCISE_E2E_BINARY` names one, prints a verdict table, and exits non-zero on any `fail`, `xpass`, or `error`. Before its first run it launches the binary once with `--version`, so that no measured session pays the one-time first-launch cost of a new binary; a failed warm-up stops the command with an error. It writes `target/excise-e2e/<run-id>/summary.json` (with `target/excise-e2e/latest` pointing at it) and one failure bundle per failed run: the failure document, the asciicast recording, a screen dump, the event file, and the exact command that reruns it. The `xtask` alias in `.cargo/config.toml` makes `cargo xtask <command>` the same as `cargo run --locked --package xtask -- <command>`.

`cargo test` also runs each scenario once per profile against the `excise` crate's own binary (`tests/harness_scenarios.rs`). It also runs four negative controls from `crates/excise-harness/tests/controls`, which ask `delete` for an entry it cannot bind to what the step names (another entry than the one selected, or an entry below a folder when the step gives no `path`) and assert that the step fails without a `y` being sent, and, in scenarios that disable the confirmation (`disable_delete_confirmation`), without a Backspace being sent: with no dialog, the step also refuses a name and kind that two entries of the fixture share, because the selected-item panel could not say which one Backspace deletes. On Windows, `tests/harness_scenarios.rs` asserts that the `delete` step refuses before any key, with the `ConPTY` reason, and that the fixture is unchanged, so a Windows run checks that rule; on Unix nothing changes. Fixtures and scratch areas are created under `/tmp` on Unix, or under `EXCISE_E2E_TMPDIR`, and removed afterwards. On Windows, set `EXCISE_E2E_TMPDIR` to a short directory such as `C:\xh`. The deletion dialog is at most 78 columns wide, and the `delete` step refuses a path that the dialog cuts short.

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
cargo xtask headless --fixture-dir target/excise-specs --fixture home-50k
```

`--quick` runs the fixtures of at most 10,000 planned entries and `--full` (the default) those of at most 250,000. `--fixture` names fixtures and runs them whatever their size, and `--class` (`scale`, `identity`, `hostile`, `volumes`) selects by class. `--repeat N` is the number of timed pairs (five by default, interleaved as scan, `du`, scan, `du`, after one untimed warm-up pair), `--profile` is `default` or `deterministic`, `--timeout` bounds one scan or one `du` in seconds, and `--keep-scratch` keeps the scratch areas and reports. A fixture is scanned in its cached master below the target directory, generated once and then reused, except a fixture that `cargo clean` could not remove because it holds directories that cannot be listed or paths longer than `PATH_MAX`: that one is never cached and is scanned in a fresh copy, removed when the fixture is done. The command builds the release binary unless `EXCISE_E2E_BINARY` names one, prints one row per fixture with the headless and `du -sk` medians, the median and range of their ratio, and the oracle diff, and exits non-zero on any `fail`, `xpass`, or `error`. The ratio is checked against a 3x budget on a fixture whose oracle entry count (fixed by its spec and seed, never a measured time) is large enough to judge; below that it is reported but never gated. A count keeps the gate deterministic: which fixtures are judged never depends on how loaded the machine was. `expectations/headless.toml` names the fixtures and platforms expected to miss the budget today, with the same strict xfail semantics as the oracle diff, and the command exits non-zero on any other miss; both the budget and the entry threshold are expected to be revisited once the durable-write fix lands and ratios approach the budget. Volume fixtures run without privileges too, but then their mount points are empty directories and no boundary is crossed; the table says so. The summary is `target/excise-headless/<run-id>/summary.json`, and a failing fixture gets a directory beside it with its discrepancies, the command that reruns it, and the report. Fixtures that fail the diff for a known defect that is not yet fixed are listed, with the finding, in `crates/excise-harness/expectations/headless.toml` and show as `xfail`; the entry is removed by the change that fixes the defect.

`cargo test` also scans the cheap fixtures that need no privileges against the `excise` crate's own binary (`tests/harness_headless.rs`) and runs a negative control: a binary that writes a report that breaks the published schema must fail the run.

`--fixture-dir DIR` takes the specs from a directory of your own instead of the bundled ones, and then `--fixture` names ids of that directory only; see [Shapes of your own trees](#shapes-of-your-own-trees). The directory is not trusted to hold only specs: a spec in it is opened without following a link, must be a regular file, and is read up to 1 MiB, so a link, a FIFO, a folder, or a larger file is refused with a message (`headless` reads every spec of the directory, so one such file stops the run before anything is scanned). A spec of your own, one built from a profile (a `shaped` part) or any other, is held to the rule every fixture is: it is cached unless its longest planned path, below the cache directory, would be longer than 1,023 bytes, the most that `PATH_MAX` leaves on macOS, and then it is generated fresh for the run, as a `deep` fixture is. Every kind of part has a bound on its longest path, worked out from its fields without planning the spec, so a `tree` whose names are 255 bytes long a few levels down is refused as a `shaped` part is, whatever the root. The cache directory is your target directory, `excise-fixtures.noindex`, and a name of up to 57 bytes; the target directory counts as the longest of the path as it is spelled (on Unix the system is handed its text, `.` names and repeated separators included), of every pathname the system works on while it expands a link in it (the content of the link and what is left of the path after it, which macOS refuses past 1,023 bytes), and of the path it resolves to. The room a spec has depends on where your target directory leads: the same spec can be cached under a short one and generated fresh under a long one, or under a short link to a long one. A target directory that cannot be resolved holds no cache, and then every fixture is generated fresh: a link in it leads nowhere, or it, or a name above it, is not a folder.

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
cargo xtask bench-e2e --baseline v1.3.0 --scenario navigate-and-quit --profile deterministic
```

A scenario that has a `delete` step, or presses Backspace itself with a `key` or `type` step, or composes an escape sequence from its keys, needs a baseline that marks its frames and answers input barrier requests (see "Test Event Channel"): the `delete` step refuses a program that does not, before it sends a key, and so does the first key after that Backspace that could confirm what it opened (the `y` of `quit` among them), and `bench-e2e` stops with that error. Compare a baseline from before them, such as `v1.3.0`, on `--fixture` cases and on scenarios that have neither.

For every metric it reports the median candidate/baseline ratio and a deterministic bootstrap 95% confidence interval (`--seed`), and applies a verdict: a timing metric blocks past a 20% regression (`--timing-threshold`) with a confident interval, a memory metric blocks past a 5% move either direction (`--memory-tolerance`) with a confident interval, and the command exits non-zero on any block. The document is `target/excise-bench-e2e/<run-id>/ab.json`, a `harness-ab` document that records both builds' identities and the session's context (host, toolchain, power state, load average, and concurrent `excise` processes) because timings never transfer across sessions. `cargo test` runs the same comparison logic with the crate's own binary as both sides, on a small fixture, as a schema and verdict regression check (`tests/harness_bench.rs`).

See the [harness README](https://github.com/findyourexit/excise/blob/main/crates/excise-harness/README.md) for the full reference.

### Shapes of your own trees

`excise-shape`, the binary of the `excise-harness` crate, lets a benchmark or a scan behave like a tree you care about, such as a home directory, without anyone running Excise on it. `excise-shape profile <ROOT>` walks a tree read-only (`lstat` only: it asks nothing of what a symbolic link points at, not even whether the target exists, writes nothing, follows no link below the root, and stays on the root's file system unless asked; `--output` makes its one new file only after the walk has ended, so a profile never counts its own output) and writes a profile of its shape: counts by kind and by depth, histograms of what a folder holds, of file sizes, and of name lengths, the share of hard links, and how many symbolic links there are. It holds aggregates only: never a name, a path, a link target, an owner, or a timestamp, and no message of the command names the root or anything below it. Its memory grows with three things of the tree and nothing else: the names of the folder it is listing, the subfolders still to visit in each folder on the path to it (in a comb-shaped tree, a chain of folders that each hold many subfolders, they can approach the number of folders in the tree), and the identities of the files that have more than one name; it spills nothing to disk. `excise-shape spec <PROFILE> --id ID --entries N` turns a profile into a fixture spec that the generator builds at exactly `N` planned entries, deterministically for a given profile, size, and seed, and `--fixture-dir` points `headless` and `bench-e2e` at the directory that holds it.

```console
cargo install --locked --git https://github.com/findyourexit/excise excise-harness --bin excise-shape
excise-shape profile <ROOT> --output target/excise-profiles/home.json
excise-shape spec target/excise-profiles/home.json --id home-50k --entries 50000 --output target/excise-specs/home-50k.toml
cargo xtask headless --fixture-dir target/excise-specs --fixture home-50k
cargo xtask bench-e2e --baseline main --fixture-dir target/excise-specs --fixture home-50k
```

Profiles and specs of a real tree are local only. `target/` is ignored by Git, so keep them there, and do not commit or publish them. Agents never run `excise-shape` on a real path (`~`, a project, a mounted volume): they profile generated fixtures and scratch trees, and they may build specs from a profile they are given. The tests do the same, and prove that a profile of a tree with distinctive names holds none of them. `--fixture-dir` does not affect `e2e` scenarios, which name bundled fixtures, and `bench-e2e` keeps the bundled fixture for its `--scenario` cases too. Because `ab.json` names a fixture by its id alone, `bench-e2e` refuses, before any case runs, a `--fixture` of the directory whose id is also the bundled fixture of a selected `--scenario`, unless the two specs are the same apart from their description: rename the spec in the directory. A user who reports a slow scan attaches a profile to the performance report form instead of the tree. See the [harness README](https://github.com/findyourexit/excise/blob/main/crates/excise-harness/README.md#shape-profiles) for the profile document, the tolerances a built tree is held to, and what the tests prove.

### Version sweep

`cargo xtask sweep` finds out which published releases show each defect that the release under development fixes. It builds every published `v1.*` tag and the candidate (`HEAD`), runs the checks a published release can take against each build, and writes a table with one row per defect and one column per version. A changelog entry that names the versions a defect affects is a claim about every published release, and the sweep is how the claim is measured. It runs only when someone asks for it, on a development machine or through the manual `version-sweep` job (see "Continuous integration tiers"), and never on a pull request.

```console
cargo xtask sweep                                    # every v1.* tag and HEAD, the full tier
cargo xtask sweep --quick                            # the same versions, small fixtures only: the smoke tier
cargo xtask sweep --refs v1.2.4 v1.3.0 HEAD --quick  # chosen refs; the last one is the candidate
cargo xtask sweep --rounds 9                         # more rounds in every paired timing (5 by default)
```

`--refs REF...` takes every ref up to the next flag (a tag, a branch, or a commit) and may be repeated. The last ref is the candidate that every ratio is taken to, so name the oldest first. A ref is built from its commit, so uncommitted changes are not in the candidate, and from the commit it resolved to when the sweep planned its builds, so a branch or `HEAD` that moves while the sweep runs changes nothing it builds. `--quick` and `--full` (the default) exclude each other: the full tier adds three fixtures to the oracle scans and the two checks that need a large tree (F5 and F13). `--fixture ID` (repeatable) replaces the fixtures of the headless scans and the timings, and a row that reads a fixture the run left out is `not-measurable`. `--rounds N` is the number of measured rounds of each paired timing (a ratio needs at least three rounds in which both sides finished), `--seed S` seeds the bootstrap (0 by default), and `--timeout SECONDS` bounds one scan, or one interface run to `COMPLETE` (120 by default): a run that does not finish within it is kept at how long it ran and flagged as not finished (see Timing below).

What it needs:

- rustup, with the toolchain each ref pins installed: `rustup toolchain install 1.88.0` for v1.0.0 and v1.0.1, and `rustup toolchain install 1.98.0` for every later tag and for `HEAD`. The sweep installs nothing: it checks every pin before the first build, and a missing toolchain fails it there, with the command that installs it.
- The full history with the tags (`git fetch --tags`). The sweep lists the `v1.N.N` tags itself and fails with that advice when there are none, and a ref given with `--refs` has to be a commit of this repository.
- `RUSTUP_TOOLCHAIN` not exported in the shell that runs it. The sweep removes it, with `CARGO`, `RUSTC`, and `RUSTDOC`, from the environment of every build it makes, so that nothing overrides a ref's pin, but `cargo xtask` itself is built and run by the shell's own `cargo`, which would honor it (inside the repository `cargo --version` must print the pinned 1.98.0).
- Disk and time: one release build per ref, each in its own target directory, `target/excise-sweep-builds/<sha>/`. The first run builds every release, and a later run builds nothing for a commit it has built. A build that has not finished after 60 minutes is killed and is a failed build. The full tier also generates the 20,053-entry `selection-drift` and the 49,050-entry `tiny-files-50k` fixtures once, into the fixture cache.
- A quiet machine: close other `excise` sessions. The timings are paired and interleaved, but the run warns about the `excise` processes it finds, as `bench-e2e` does.

What it leaves is its output below `target/`, the cached builds (`target/excise-sweep-builds/`), and the cached fixtures. Every ref is built in a detached worktree below `target/excise-sweep-worktrees/`, which the build removes when it ends with `git worktree remove --force`, and every check runs `excise` on a generated fixture that carries the ownership marker, in a scratch area that is removed afterwards. It never runs `excise` against a path of yours, and it sends no key that could ask for a deletion, so the deletion rows of the table are `not-measurable` (see the harness README). On Unix it sends SIGTERM and SIGHUP to the builds, and SIGQUIT as well except on macOS, where an unhandled SIGQUIT makes the system write a crash report into your own `~/Library/Logs/DiagnosticReports`: no signal it sends does that (on Windows it closes the console instead). On Unix it first sets the soft limit on the size of a core file to 0, which every build inherits, so that SIGQUIT leaves no core file on Linux (a machine that pipes core dumps to a handler does not enforce the limit, and what the handler keeps is its own). A build that crashes by itself can still make macOS write a crash report into that folder, which the sweep neither prevents nor deletes; it deletes nothing in your home folder.

Every build is bounded: a `cargo build` is killed after 60 minutes, and each `git` and `rustup` command of the builder after 5, with everything it started (on Unix each runs in a process group of its own, which is killed whole; on Windows only the command is). A build that ends by itself, whether it succeeded or failed, has what it left running in its group killed too, so that nothing it started outlives it. The `git` and `rustup` commands get no such kill: a detached `git gc --auto` puts itself outside their group anyway, and the background work of your hooks is your own configuration of your repository. A build that ran out of time is a failed build, and the sweep goes on to the next ref. A run that is killed (Ctrl-C, a timeout, `SIGKILL`, power loss) cannot clean up after itself: it leaves the `xh-scratch-*` scratch areas and the fixture copies (`<fixture>-<pid>-<n>`) it was using in `/tmp` or `$TMPDIR` (or `EXCISE_E2E_TMPDIR`, when that is set), and at most one worktree. On Unix Ctrl-C reaches the sweep and not a running build, which is in a process group of its own and goes on, with no deadline, until it ends. A cargo build ends by itself, so waiting is enough. To see it, run `ps -axo pid,pgid,command | grep '[c]argo build'`: its working directory is the worktree below `target/excise-sweep-worktrees/` that the sweep left. To stop it, run `kill -TERM -- -PGID` with the pgid column of that line, which ends the whole group. Then remove the worktree the sweep left with `git worktree remove --force DIR`, which removes the directory and git's entry for it together, and check that no `xh-*` entry is left in `/tmp` or `$TMPDIR`. Do not run `git worktree prune` for it: it forgets every worktree of the repository whose directory is missing (an unmounted disk's, a moved directory's), together with its index, its detached `HEAD`, and its reflog.

The output is `target/excise-sweep/<run-id>/`, and `target/excise-sweep/latest` points at the newest run:

```text
sweep.json                   the harness-sweep document: the versions, the timings, the checks, the rows
table.txt                    the grid, then the detail behind every cell
builds/<label>.log           cargo's output of each ref built in this run (none for a cached build)
evidence/<label>/<name>.txt  one file per check; <name> is <check>[-<fixture>][-<profile>]
```

`<label>` names a version's files, and the ref as typed never does: the ref with every character outside `A-Za-z0-9._-` replaced by `_` (a leading `.` or `-` too), cut to 64 characters, then `-` and the first 12 characters of the commit, for example `v1.3.0-0123456789ab`. So no ref (`feature/x`, `a/../b`) can make a subdirectory, leave the run directory, or hide a file. Two versions with one label, letters compared without case (`release/1.0` and `release_1.0` at one commit), are refused before any fixture is prepared, with no table: name one of them by its commit.

The command prints the grid, then the lines `table:` and `document:` with their paths, on stdout, and its progress and its problems on stderr. It exits 0 when every build was made and every check could be carried out. When a build failed (one that ran out of its 60 minutes included) or a check could not be carried out it exits 1 and still writes and prints the table. When a ref does not resolve, a pinned toolchain is missing, a fixture cannot be made, a fixture is not what its plan says or a build or a check changed one, two versions would keep their files under one name, or the command line is not valid, it exits 1 with no table.

**Reading a cell.** The grid has a row per defect (`F1` to `F24`, each tied to its entry in `CHANGELOG.md`) and a column per version, with `AFF` for affected, `ok` for not affected, and `n/m` for not measurable. `table.txt` gives every cell its `value` (what was measured, for example `14x the candidate (95% CI 12.0-15.0, 5 rounds)`, the rounds being those in which both sides finished), its `why` (the rule that decided it, or why nothing can), and its `evidence`: a file below the run directory (`evidence/v1.3.0-0123456789ab/idle-navigate-folders-default.txt`) or a JSON pointer into `sweep.json` (`#/measurements/0`, `#/checks/12`). A claim needs a measurement: `affected` and `not-affected` come only from a check that ran, so quote a cell with its value and its evidence, and never read `not-measurable` as `not-affected`. A row whose candidate cell is `affected` carries a note that the candidate shows the defect too, so the run does not show it fixed. On macOS the F6 cell rests on SIGTERM and SIGHUP, and its reason ends with the sentence that SIGQUIT is not sent there.

**Timing.** Timings are paired and interleaved: every version runs one after another in each round, in one session, on the same warm fixture, after an untimed warm-up round, and a version's cell rests on the median of its per-round ratios to the candidate, over the rounds in which both finished, with a bootstrap 95% confidence interval. In its turn of a round a version scans the fixture headless and then runs the interface to `COMPLETE`, so that F18 holds the interface to the headless scan of the same version in the same round, without the time its report took to write (a release writes its report after its scan, and the write can be most of a headless run: the sweep times it from outside, as the growth of the `--output` file, and F18 uses the wall time less it); a second phase runs the interface under default and reduced motion against a slow terminal (F2). A version is slower only when that median, over at least three rounds, is more than 20% above one and the interval lies above one. At or below one it is not slower, and anything between is inconclusive (`not-measurable`: more rounds settle it). A run that does not finish within `--timeout` is kept at how long it ran and flagged: it is no sample of the median, only a bound on the ratio of its round. Two rounds in which a version's run did not finish and the candidate's did, with a lower bound on the ratio over 20%, make it slower; two in which the candidate's run did not finish and its own did, with an upper bound of at most one, make it not slower; and a round in which neither finished says nothing. A version whose runs do not finish twice in a row (the warm-up round does not count) is not run again for that kind of run: its remaining rounds are recorded as skipped, and are neither samples nor pairs. A round in which a run has no usable reading of a series is neither a sample nor a pair either, and is not counted as skipped: a scan that finished and whose report was seen to change in size fewer than twice gives no write time and no scan time without the report, and never a time of 0 ms. A timing cell quotes how many runs a version made, how many did not finish, and how many rounds were skipped. Timings from different sessions or machines are never compared, and a single run never is.

**What `not-measurable` means here.** Some cells no sweep can decide: the deletion rows (F9, F14, F19, and F21: `no confirmation is sent to a build without frame marks`), F12 (it needs a scratch volume that fills during a scan, which only a privileged attached volume gives: `EXCISE_HARNESS_PRIVILEGED=1`, which AGENTS.md allows only for a task about volumes), F8 and F15 (Windows only), and F24 anywhere but on Windows, where it is measured. F5 and F13 need the full tier, and F5, which was observed once, is `not-measurable` when a run does not reproduce it, never `not-affected`; an attempt whose scan never reaches `COMPLETE` after the cursor moves is counted apart and shows neither the drift nor its absence. F22 is `not-affected` only when both variants of its filter ran and the program survived both: a variant that could not be carried out leaves the cell `not-measurable`, with the variant named, unless the program ended in the other one, which makes it `affected`. F23b is `affected` on any platform when the untouched cursor is not on the largest entry at `COMPLETE`, and `not-affected` on Windows only when it is: the defect needs a small entry to be listed before the largest one, which Windows does (a folder in name order) and the check cannot see on macOS or Linux, where a cursor on the largest entry is `not-measurable`, with the platform named. A report seen to change in size fewer than twice gives no write time to compare (F16) and no scan time to hold the interface to (F18), so a version with too few such rounds is `not-measurable` in those rows, and its F18 cell says in how many of how many rounds the write could not be told from the scan. A timing can be inconclusive, and a version whose build failed (one that ran out of its 60 minutes included) is `not-measurable` in every row that needs its binary, with the reason and `builds/<label>.log`. The harness README lists every reason.

**Reproducing a CI run.** The manual job (see "Continuous integration tiers") runs `cargo run --locked --package xtask -- sweep`, with the `refs` input split at its spaces after `--refs` and the `tier` input as `--full` or `--quick`: that is `cargo xtask sweep` with the same arguments. To reproduce a run, check out the commit it started on (`context.checkout_sha` in its `sweep.json`), fetch the tags, install the two toolchains, and run that command. The job summary shows the table, and the artifact (`version-sweep-ubuntu-24.04` or `version-sweep-macos-14`) holds the run directory, with `table.txt` and `sweep.json` first.

See the [harness README](https://github.com/findyourexit/excise/blob/main/crates/excise-harness/README.md#version-sweep) for the checks, the timing policy, every reason a cell is `not-measurable`, and the document.

### Counts and count history

`cargo xtask counts` counts what a build of `excise` costs without timing anything: numbers that, for a given binary and fixture, do not depend on timing or load, so two commits compare exactly and a difference is always real. Timing is only ever evidence from the paired A/B benchmark above. It counts four fixtures (`wide-1k`, `node-modules-2k`, `identity-small`, and `tiny-files-50k`) under the `deterministic` profile, and for each:

| Count | What it is |
|---|---|
| `entries` | The entries the scan covered, as the program's own report counts them. It moves only when the fixture or the accounting does, so any change in it is flagged as unexpected. |
| `scan_store_bytes` | The bytes the scan store held when the scan ended, as the program's own report states them. |
| `residue_files` | The files the scan and the session left in their scratch areas. |

Each fixture is scanned headless and then, on Linux and macOS, run in a pseudo-terminal until its scan completes: that scan must be the one that was counted (the same entries as the headless scan, and a header that reads `COMPLETE`), and the files it leaves behind are counted. Every fixture is counted twice (`--repeat`), and the command fails, naming the count and every value it took, if any count differs between the runs. What is not counted is left out on purpose: the peak of descriptors, threads, or scan-store bytes during a scan is sampled, and sampling gave 19, 22, and 24 descriptors, 8 or 10 threads, and two different store peaks for one scan on one machine; the size of the JSON report depends on where the fixture lives, because every entry repeats the root path, and not on the build; wall time, CPU, latency, and memory peaks are for A/B; and the descriptors and threads that the program holds once its scan is complete, because nothing outside the program says that it has finished what follows its scan, and no window of silence proves it (a worker that is blocked or not scheduled for longer than the window gives a value that is too early, and two runs agree on it). They can return when the program emits an `idle` event on the test event channel after the work that follows its scan.

```console
cargo xtask counts
cargo xtask counts --fixture wide-1k --repeat 3 --out target/counts.json
```

The document is `target/excise-counts/counts.json`, a `harness-counts` document that records the commit, the runner, the toolchain, and each fixture's hash. Counts of two documents are compared only where a fixture's hash is equal, and records are per operating system: the same fixture does not count the same everywhere (`identity-small` counted 23 entries and 14,783 scan-store bytes on a hosted Linux runner and 27 and 16,441 on macOS), so a pull request is only ever compared with a record taken on the same system.

**What runs where.** `counts-history.yml` runs on every push to `main` and on demand, on `ubuntu-24.04`, and appends one record, `records/<os>/<first two hex digits of the commit>/<commit>.json`, to the orphan branch `bench-data`. Of the three count workflows it is the only one that holds `contents: write`; its runs never overlap (a run in progress is not cancelled, but GitHub keeps only the newest run waiting behind it, so a burst of pushes can leave a commit without a record, which is why a comment may compare with an ancestor), a push that loses a race is retried five times, and a commit that already has a record is not recorded again. A run on any ref but `main` writes to `bench-data-probe` instead. `counts-pr.yml` runs on a pull request that changes `src/` or the files that build it, counts the merge commit with a read-only token, and uploads the counts as an artifact. `counts-comment.yml` runs when that finishes, in the context of this repository, and never checks out or runs the pull request's code: it checks out the default branch and builds its tooling, and then, immediately before it downloads the artifact, lists the run's artifacts and refuses anything but exactly one `pr-counts` of at most 64 KiB. It reads the artifact as untrusted input (a plain file of at most 64 KiB, held to the schema before anything in it is used) in a step that has no token, against a working copy of `origin/bench-data` that its checkout already holds, and then posts or updates one comment, found by a hidden marker. The posting step believes the artifact's pull request number only if the API says that pull request is open, still at the commit the run was for, from the repository and branch the run was for, and still based on the commit that the counts were compared with; and where the run's payload lists its pull requests (it does for a pull request in this repository, and leaves the list empty for one from a fork), the number must be one of them.

**Reading the comment.** It compares the counts of the merge commit with the record of the pull request's base commit, or of the nearest ancestor that has one (the base commit itself or up to 200 commits before it), and says which and how many commits back. A cost (bytes or files) that moves by more than 5% is flagged **worse** or **better**; any change in a fixture's `entries` is flagged **unexpected**, because it means the fixture or the accounting changed, not the cost. A fixture whose hash changed is listed as not compared. The comment informs and gates nothing; a flagged increase wants an explanation in the description, and timing evidence is a separate matter (see "Pull Requests"). It is at most 60,000 bytes, and says how many rows it left out if a document ever held more than fit.

**What the comment cannot vouch for.** A pull request's counts are measured by the pull request's own code, so a fork can write any numbers that pass the schema; that is why the comment gates nothing, and a reviewer who wants the counts of a head runs `cargo xtask counts` on it. The comment workflow finds the counting workflow by its name, so a workflow of that name in a fork can upload an artifact of the same name, which is held to the same checks as the real one.

**Fetching the history.**

```console
git fetch origin bench-data
git worktree add ../bench-data origin/bench-data
jq -s 'map({at: .context.committed_at, commit: .context.git_sha, bytes: (.cases[] | select(.fixture.id == "tiny-files-50k") | .metrics.scan_store_bytes)}) | sort_by(.at)' ../bench-data/records/linux/*/*.json
```

See the [harness README](https://github.com/findyourexit/excise/blob/main/crates/excise-harness/README.md#counts) for the document, the trust rules, and the commands.

### Interactive sessions

`cargo xtask tui` drives one live `excise` session on a fixture, step by step, to explore the interface before you write a scenario or to see what a scenario would do. It is the way to look at the program for yourself: never run `excise` against a real path.

```console
cargo xtask tui open --fixture delete-file --record     # prints the session id and the first screen
cargo xtask tui keys SESSION / type:victim.bin enter    # select an entry through the filter
cargo xtask tui delete SESSION --name victim.bin --kind file
cargo xtask tui screen SESSION
cargo xtask tui events SESSION --since 0
cargo xtask tui close SESSION
cargo xtask tui list
```

Every command prints exactly one `harness-tui` JSON document on stdout, a failure included (status 1, or 2 for a command line that is not valid). `open` takes the id of a fixture spec, never a path. It starts the release `excise` (or the binary `EXCISE_E2E_BINARY` names) on a disposable copy of the fixture that carries the ownership marker, and waits for the first frame. One supervisor process per session then serves the commands, so the session outlives the command that opened it. It ends on `close`, when the program exits, and after 15 minutes without a command (`--idle-timeout`), and then removes everything it made; `open` and `list` clean a session whose supervisor died. Close every session you open.

What to know when you drive it:

- `open` returns at the first frame, while the scan may still run. Wait until `screen` reports `header_state` `COMPLETE` before you act on the map.
- `keys` waits for a frame that counts what it sent, as `settle` does; `settled: false` means none came, as for a key that changes nothing. `esc` goes up a folder. A folder opens when it is selected (`/`, `type:NAME`, `enter`) and `enter` is pressed again. The filter keeps its text: `/` opens it with the text of the filter before (`screen` shows it in `filter.input`), so erase that with `backspace` before you type a name, and a filter that is applied again unchanged selects nothing.
- Deletions go only through `delete`. It runs the protocol of the scenario `delete` step, so the dialog is verified, with every sentinel, before the confirmation key is sent, and `keys` refuses `y`, `enter`, and every other key that could confirm a deletion dialog while one is open or may be about to open, and in a command that sent `backspace` before it (erase a filter's text in one command and apply it in the next). Before such a key the driver writes an input barrier request behind every key sent so far and waits for the program to answer it and for the screen to show the frame that answers it (see "Input barrier" under "Test Event Channel"; `excise` marks every frame in the terminal output while the event channel is open). The program has then read all those keys, however the terminal cut their bytes into input events (`alt+backspace` can reach it as `esc` and `backspace`), and a dialog opens on a key and never by itself, so what the screen shows about dialogs is exact, on the Unix pseudo-terminals the driver runs on, however slowly the terminal delivers it, and the clock decides nothing. `ctrl+]` is the barrier request, not a key: no `keys` token writes it. Where the terminal's screen cannot be tied to a frame exactly (the capability `SCREEN_IS_EXACT` is false: Windows; see "Test Event Channel"), `delete` and this guard refuse before any key, with the error kind `refused`. A program that does not mark its frames or does not answer the barrier (a build from before them) gets no key that could confirm a deletion, and `delete` refuses it before it sends any key. `delete` also refuses the fixture's ownership marker, `.excise-harness-owned`, and anything inside it as a target, and looks at the marker once more right before the confirmation key. `delete` acts on an entry of the open folder, because the filter selects among that folder's entries: for an entry elsewhere it fails at once with `not_in_view`. It takes the entry you selected and selects it through the filter otherwise. Its `--timeout` bounds the whole command; one that runs out is a failure document whose `confirmed` says whether the confirmation key was sent, and the session stays open (after a confirmation the program is still deleting: read `screen`, or `close` it).
- `keys` also refuses a key that is the escape byte and a confirmation in one input (`alt+y`, `alt+enter`), always, because a program may read it as two inputs and the escape could uncover a deletion dialog that the key then confirms, with no request able to come between the two bytes: send `esc`, read `screen`, then the key. `delete` waits for a deletion that an earlier command confirmed and gave up on before it starts another. A stale session's process is killed only if it carries the session's mark in its environment, and every command refuses a `target/excise-tui` that is a link.
- `keys` reads every byte the session has written, across commands, with one scan (`pty::input::InputScan`), because the program joins the bytes of an escape sequence across writes: `alt+[` and then `type:121u` are `ESC [ 121 u`, which it reads as `y`, with no `y`, Enter, or line feed in any key. A key that begins a sequence (`alt+[`, `alt+O`) is sent, as nothing can be confirmed with it yet. Every key that continues one counts as a key that could confirm, whatever its bytes, and is refused with the error kind `refused`: no input barrier can be written behind `ESC [`, because the program takes the request for a byte of the sequence and never answers it, so every later key is refused too, and the way on is to `close` the session and open another. After a lone `esc`, a key that could continue a sequence (`[`, `O`) is guarded like any key that could confirm: it goes out behind a barrier.
- `open`, `keys`, `delete`, and `close` summarize the frame events since the previous command (`events.frames`) and list every other event in full; `events --since N` lists every record, frames included.
- `--record` keeps an asciicast of the session, `target/excise-tui/<id>.cast`. It is large (7 MB for a session of 12 seconds on the smallest fixture), so record only a session you mean to keep.

macOS and Linux only: on Windows every command fails with a document that names the platform. `xtask/tests/tui.rs` runs the real commands against the `excise` binary that `cargo test` built, in parallel and each in its own state directory. See the [harness README](https://github.com/findyourexit/excise/blob/main/crates/excise-harness/README.md#interactive-driver) for the full reference.

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

## Continuous integration tiers

The validation harness runs on a fixed cadence in four places. `ci.yml` (Native verification) holds the required checks, so the pull-request tier runs inside its three native jobs; the other tiers have workflows of their own. Every workflow uploads the run directories of a failed run, with a retention in days, as an artifact; the manual version sweep uploads its run directory whether or not the job failed, because the table is its product.

| Tier | When | Where | What runs |
|---|---|---|---|
| Pull request | Every pull request and every push to `main` (`ci.yml`) | Linux, macOS, and Windows, in each native job after the dependency policy check | `cargo xtask e2e --quick --latency-scale 2`; then, on Linux and macOS, the full-tier scenarios named in the job's `pr_scenarios` matrix entry at the same scale: latency under deletion and scan load, idle, descriptor and scan-store budgets, signals, and the 250,000-entry memory budget (Linux also exports the deletion history after a 65,111-entry deletion; macOS also runs the selection-drift scenario). Fixtures never exceed 250,000 entries. On macOS both commands also take `--timing-informational`: timing is reported there and not gated (below). |
| Fuzz compile | Every pull request (`pr-fuzz.yml`) | Linux | `cargo +nightly-2026-08-18 check --manifest-path fuzz/Cargo.toml --bins --locked`, so an API change cannot break a fuzz target unnoticed, then one bounded fuzz run |
| Nightly | Every night and on demand (`nightly.yml`) | Linux | `e2e --nightly` at the strict budgets with `EXCISE_HARNESS_CGROUP=1` (the one-million-entry scenario runs under the memory cap, and a check fails the job if it was skipped), `compare --nightly`, `headless --full` and `headless --fixture tiny-files-1m --repeat 0` under the cap, the privileged volume steps (`EXCISE_HARNESS_PRIVILEGED=1`: `e2e --scenario scan-store-quota` and `headless --class volumes`), and a 30-minute run of the `deletion_lifecycle` fuzz target on the pinned nightly toolchain (`-max_total_time=1800`; a failed run keeps its crash from `fuzz/artifacts`) |
| Nightly | Same | Windows | `e2e --full`, the lifecycle tier without the scenarios that have a `delete` step or press Backspace themselves, which the pseudo-terminal runner skips there (below) |
| Nightly | Same | macOS | `e2e --nightly --timing-informational`, `compare --nightly --timing-informational`, and `headless --full --timing-informational`: the lifecycle tier and the performance checks, with timing reported and not gated (below) |
| Count history | Every push to `main` and on demand (`counts-history.yml`) | Linux | `cargo xtask counts --repeat 2`, then one record appended to the `bench-data` branch (see "Counts and count history") |
| Pull-request counts | A pull request that changes `src/` or the files that build it (`counts-pr.yml`, then `counts-comment.yml`) | Linux | The same counts of the merge commit, uploaded as an artifact; a second workflow comments the deltas against the base commit's record |
| Weekly | Saturday, and on demand with the `job` input left at `largest-fixture` (`weekly.yml`) | Linux | `headless --fixture tiny-files-10m --repeat 1 --timeout 7200` under the cap, on an ext4 image that the job builds (1 KiB blocks, 12 million inodes, no journal) because a hosted disk has about five million inodes; it measured about 4.9 times `du`'s time against the budget of 15. A manual run can name `tiny-files-1m` instead to prove the mechanism in minutes, but its ratio can exceed the budget on this image: `du` finishes a million tiny files in about 2 seconds, and 15.3 times was measured. |
| Version sweep | On demand only: a manual run of `weekly.yml` with the `job` input set to `version-sweep`; no schedule | Linux (`ubuntu-24.04`) and macOS (`macos-14`) | `cargo xtask sweep` over every published `v1.*` tag and the commit the run started on, or the refs the `refs` input names, at the `full` or the `quick` tier (the `tier` input), each ref built with its own toolchain; the table is in the job summary and the run directory is the artifact (below, and "Version sweep") |

**Version sweep job.** `weekly.yml` holds a second job, `version-sweep`, that only a manual dispatch with the `job` input set to `version-sweep` starts (the Actions tab, or `gh workflow run weekly.yml --ref BRANCH -f job=version-sweep`). The Saturday schedule, and a manual run that leaves `job` at `largest-fixture`, run the scan above and never the sweep, and the two jobs are in separate concurrency groups, so neither cancels the other. The inputs are `job` (`largest-fixture`, the default, or `version-sweep`); `refs`, for the sweep only: the git refs to build, separated by spaces, the last one the candidate (empty means every `v1.*` tag and then `HEAD`, the commit of the ref the run was started on); `tier`, for the sweep only: `full` (the default) or `quick`, the smoke tier; and `fixture`, which only `largest-fixture` reads. A step before the sweep refuses a `refs` that holds anything but letters, digits, `.`, `_`, `/`, `@`, `^`, `~`, `-`, and spaces, or a ref that starts with a dash. The job runs on `ubuntu-24.04` and `macos-14` (`fail-fast: false`, 240 minutes each), checks out the full history with every tag (`fetch-depth: 0`), and installs Rust 1.88.0 and 1.98.0, the two toolchains the tags pin, with 1.98.0 as the default and no override: the sweep builds each ref with its own pin and installs none itself. A build that has not finished after 60 minutes is killed and fails its column, and the sweep goes on. Each runner shows `table.txt` in its job summary and uploads `target/excise-sweep/`, without its `latest` link, as the artifact `version-sweep-ubuntu-24.04` or `version-sweep-macos-14` for 30 days, whether the job passed or failed. The two runners are separate measurements, and their timings are never compared with each other; their F6 cells rest on different signals too, because the sweep sends SIGQUIT on Linux and not on macOS. See "Version sweep" under "Validation Harness" for how to read the table and how to run the sweep yourself.

**Deletions on Windows.** The pseudo-terminal runner cannot tie the screen to a frame on Windows (the capability `SCREEN_IS_EXACT` is false there; see "Test Event Channel"), so it skips every scenario that has a `delete` step, in either mode, and every scenario that presses Backspace itself with a `key` or `type` step or composes an escape sequence from its keys (a later `y`, Enter, filter text, or quit confirmation could meet a dialog that nothing verified), and prints the reason (`SKIP name: ...`), also when the scenario is named with `--scenario`. The Windows quick tier therefore runs 8 of its 26 runs: `delete-file-lifecycle`, `delete-file-reduced-confirmation`, `delete-folder-lifecycle`, `delete-tree-confirmed-with-enter`, `delete-tree-narrow-terminal-reduced-confirmation`, and `delete-tree-reduced-confirmation` (12 runs), and `delete-file-cancelled`, `delete-file-terminal-too-small`, and `exit-prompt-keeps-pending-deletion`, which press Backspace themselves (6 runs), two profiles each, are skipped. No bundled scenario composes an escape sequence. Windows has no full-tier scenario of either kind, and its list of pull-request scenarios (`pr_scenarios`) is empty. The scenarios' `platforms` stay as they are. The Windows native job's `cargo test --workspace --locked` still runs those scenarios in-process (`bundled_in_process_scenarios_pass_under_every_declared_profile`: the same steps against the real executor, with no terminal), and `tests/harness_scenarios.rs`, which runs scenarios on the real binary through the pseudo-terminal, asserts on Windows that the `delete` step refuses before any key, with the `ConPTY` reason, and that the fixture is unchanged, so Windows CI checks that rule; on Unix nothing changes.

**Latency scale.** Hosted runners are busier than the machines the strict budgets are held on, so the pull-request tier multiplies the limits of the four latency budgets (input to frame, stall, first frame, and quit) by 2 with `--latency-scale 2`. Local runs and the nightly tier use the strict limits. No other budget is scaled, and a scenario that documents a known defect keeps the strict limits. A scaled run records `latency_budget_scale` in its `summary.json` and names the factor on the last line of the verdict table. Latency scenarios run on Linux and macOS only: Windows measures latency, in the metrics of every lifecycle scenario, and gates none of it.

**Timing on hosted macOS.** A hosted macOS runner (three virtual CPUs) is slower than the machine the strict budgets were set on. On it, a slow-terminal scenario stalled for 562 to 588 ms against the pull-request limit of 500, four 50,000-entry interface comparisons ran at 1.68 to 2.19 times their headless scan against a limit of 1.25, and two headless scans ran at 20.8 and 15.8 times `du` against a limit of 15, while every correctness check passed. So on hosted macOS timing is measured and not gated. `--timing-informational`, accepted by `cargo xtask e2e`, `compare`, and `headless`, reports a timing verdict that would block (a scenario's four latency budgets after any scale, a comparison's median ratio over its limit, a headless scan's ratio against `du` over its budget) as a warning: the verdict table lists it and counts it on its last line, the scenario or fixture result in `summary.json` carries it in `timing_warnings` (budget, metric, value, and limit), the summary says `timing_informational`, and the exit status ignores it. `compare` writes no summary, so its warnings are in its table only. Everything else still blocks on every system: the quick tier's lifecycle checks, signals, idle output and CPU, descriptor, thread, memory, and scan-store budgets, residue, waits that time out, failed steps, oracle diffs, and a comparison run that never completed; and a scenario, comparison, or fixture that documents an expected failure keeps its strict verdict. Strict timing gates run on the Linux nightly and, before a release, on the reference development machine the budgets were set on, with the same commands and no option. That machine sets `EXCISE_HARNESS_REFERENCE=1`, which decides one thing today: whether a quick tier over its time budget fails the run or only warns (below).

**Time budget.** A native job must stay within 15 minutes end to end on its hosted runner. The scenarios in `pr_scenarios` are what fits next to the job's other steps; add one only after reading the job's step times (`gh run view <id> --json jobs`).

**Quick-tier time.** The quick tier is what agents and developers run on every iteration, so a run of the whole tier (`--quick` with no `--scenario`, `--profile`, or `--repeat`) times itself, from just before its first launch to the end of its last run. It prints the time against the 120-second budget on the verdict table's last line (`quick tier: 22.6 s of 120 s`) and records it in `summary.json` as `quick_tier_ms`. Over the budget, the run fails on the reference machine, the one that sets `EXCISE_HARNESS_REFERENCE=1` (CI runners do not), and the failure names the budget and the five slowest runs; everywhere else the table warns with the same message and the run passes, because a hosted runner or a loaded machine is slower than the one the budget was set on. `--timing-informational` does not change this: the machine decides, not the option. A run that already failed keeps its failure and still reports its time. When a change makes the tier slow, give its heavy scenario `tier = "full"` or make it cheaper.

**Reproducing a failure locally.** Every command above runs on a development machine with the same environment variables, except where a step needs another system (the memory cap needs Linux with `systemd-run`, the weekly image needs root, and Windows scenarios need Windows). Start from the failed step's log, which names the scenario, profile, and fixture, then run it alone:

```console
cargo xtask e2e --scenario NAME --profile PROFILE --latency-scale 2   # as the pull-request tier ran it; drop the scale to hold the strict budgets
cargo xtask e2e --quick --latency-scale 2                              # the whole pull-request quick tier
cargo xtask e2e --quick --latency-scale 2 --timing-informational      # as the macOS pull-request tier ran it
cargo xtask e2e --nightly                                              # the nightly scenario matrix
EXCISE_HARNESS_CGROUP=1 cargo xtask headless --full                    # under the memory cap, on Linux
EXCISE_HARNESS_PRIVILEGED=1 cargo xtask headless --class volumes       # attaches real disk images: only for volume work
cargo xtask sweep --quick                                              # the manual version sweep at tier=quick; add --refs REF... to name the refs
cargo xtask sweep                                                      # at tier=full, the default
```

The artifact of a failed run holds `target/excise-e2e/` (the summaries and one failure bundle per failed run, each with its `repro.txt`) and, for the headless suite, each run's `summary.json`, `discrepancies.txt`, and `repro.txt`. A failure bundle's `repro.txt` is the command that reruns exactly that scenario; add `--keep-fixture` to keep what it ran against.

The artifact of a manual version sweep is its whole `target/excise-sweep/` directory, without the `latest` link: open `table.txt` first (see "Version sweep").

## Pull Requests

See [CONTRIBUTING.md](https://github.com/findyourexit/excise/blob/main/CONTRIBUTING.md) for review, safety, accessibility, authorship, and documentation requirements.

A change that can affect performance attaches `cargo xtask bench-e2e` JSON (the `harness-ab` document) under "Performance evidence" in the description, or says why none applies; the pull request template has the section. A pull request that changes `src/` also gets a comment of count deltas from CI (see "Counts and count history"). Those are counts, not timings.
