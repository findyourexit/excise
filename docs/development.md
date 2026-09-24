# Development

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

The published targets have separate evidence for native behavior and release archives. Only targets with native runtime evidence are fully supported in stable v1.

## Target evidence

| Target | Support classification | Published evidence |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` (x86_64 Linux) | Supported in stable v1 and tested on the target system | Native checks and hosted release archive |
| `aarch64-apple-darwin` (AArch64 macOS) | Supported in stable v1 and tested on the target system | Native checks and hosted release archive |
| `x86_64-pc-windows-msvc` (x86_64 Windows) | Supported in stable v1 and tested on the target system | Native checks and hosted release archive |
| `x86_64-apple-darwin` (x86_64 macOS) | Build-only and best effort | Hosted release archive |
| `aarch64-unknown-linux-gnu` (AArch64 Linux) | Build-only and best effort | Hosted release archive |
| `aarch64-pc-windows-msvc` (AArch64 Windows) | Build-only and best effort | Hosted release archive |

Only the rows marked Supported have native runtime evidence. The release pipeline still publishes all six archives, but the three build-only targets have no native runtime guarantee until native evidence supports them. A hosted build or archive proves release compilation and packaging, not runtime compatibility.

The target rows and workflow matrices are checked by `cargo run --locked --package xtask -- check-support-matrix` and are included in `cargo verify`.

## Filesystem and terminal scope

The supported runtime target policy applies to local filesystem paths accessed through the documented operating-system APIs. Behavior can vary with file system types, access rules, network file systems, copy-on-write files, clones, compression, and shared physical storage. These cases remain best effort unless they have separate evidence. Unknown allocated space remains explicit rather than guessed.

Interactive support requires stdin and stdout TTYs, ANSI rendering, alternate-screen support, and a window at least `32 x 8`. Table and JSON modes are the supported non-TTY path for redirection, pipelines, CI, and terminals without those capabilities.

## Fast feedback

```console
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Run the actual terminal lifecycle tests with:

```console
cargo test --test pty_smoke --locked
```

## Release candidate checks

Each stable release preserves the CLI, configuration, and report contract established by `1.0.0`. From a clean checkout at the release commit, run the focused checks before requesting the hosted candidate:

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

The candidate aliases above in `.cargo/config.toml` map `cargo verify`, `cargo check-generated`, and `cargo dist-local` to locked `xtask` commands. `cargo package --locked --list` shows the exact crates.io file set. `cargo publish --locked --dry-run` validates packaging without uploading. `xtask dist-local` owns the local `dist/` staging path and writes the host archive, `dist/checksums.sha256`, and `dist/homebrew/excise.rb`. It does not publish them or authorize a release.

`cargo verify` uses `--allow-dirty` only for its local package-content listing. This lets it inspect a worktree with local changes. The explicit release-candidate `cargo package --locked --list` and `cargo publish --locked --dry-run` commands remain strict and require the reviewed checkout to be clean.


For the hosted candidate, dispatch the workflow only from the exact protected `main` commit and pass the manifest version, reviewed commit SHA, and a unique dispatch ID explicitly:

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

After reviewing the candidate, create the annotated release tag with the reviewed candidate run ID in its message, then push it:

```console
cargo create-release-tag "$version" "$source_sha" "$run_id"
git push origin "v$version"
```

The push-triggered workflow requires that exact annotated-tag candidate ID. Never substitute a different candidate run or a lightweight tag.

## Full verification

`cargo verify` runs the complete local suite. It expects:

- Cargo Deny 0.20.2,
- actionlint 1.7.12,
- lychee 0.24.2,
- Node.js/npm for Renovate 44.34.0 validation,
- cargo-fuzz 0.13.2 with the pinned fuzz toolchain, and
- all host-installable targets listed above.

```console
cargo verify
```

The command checks formatting, workflow syntax, Renovate configuration, documentation links, compilation, cross-target compilation, strict Clippy, unit and snapshot tests, release-profile PTY budgets, package contents, dependency policy, bounded fuzz targets, benchmarks, generated files, published schemas, distribution templates, and release-binary size.

## Generated files

The man page and shell completions are derived from the Clap command definition:

```console
cargo generate
cargo check-generated
```

Commit generated changes with the source contract that produced them.

### Current-main demo pipeline

The `cargo demo` alias supports current `main` development rather than release packaging. It delegates to `xtask demo`. Refresh the VHS demonstration after user-visible CLI or TUI changes and review the output before a release.

```console
(
  set -euo pipefail
  cargo +1.98.0 build --release --locked --package excise
  cargo demo
)
```

Run the tape from the repository root. `xtask demo` validates `tapes/demo.tape`, captures it at 24 fps, then resamples it to 20 fps with a non-dithered 64-color palette and lossy GIF compression.

It owns the `assets/demo-main.rendered.gif`, `assets/demo-main.palette.gif`, and `assets/demo-main.quantised.gif` staging paths. It promotes the last file to `assets/demo-main.gif` only after it passes the published weight limit. If any stage fails, the committed current-main asset remains untouched.

The command needs `vhs`, `ttyd`, `ffmpeg`, `ffprobe`, and `gifsicle` on `PATH`, plus a Unix-like `bash` and core utilities. The tape explicitly selects `bash`, creates its fixture under `/tmp`, and uses `head`, `mkdir`, and `rm`.

Invoking `vhs tapes/demo.tape` directly writes an unoptimized 24 fps recording to `assets/demo-main.rendered.gif`. It skips the 20 fps resampling, palette rebuild, compression, and weight check. Do not use it to refresh the committed current-main hero.

The `Demo recording` workflow renders the tape on pull requests and `main` with its pinned Linux toolchain. It uploads the GIF for review but never writes or commits source files. Review and explicitly commit a validated `assets/demo-main.gif` refresh.

### README feature demos

The feature tapes in `tapes/features/` correspond to the eight README feature entries. They share a guarded fixture builder and keep every generated file inside a disposable fixture. After building the release binary, render every feature GIF with `cargo demo-features` or render selected demos with `cargo demo-features storage-map reports`. The command writes the reviewed GIFs to `assets/features/` and promotes each one only after it passes its own duration, frame-count, and size checks.

The Demo recording workflow keeps pull requests and `main` on the lightweight `hero` mode. Use its manual `features` mode to render the full feature suite on the pinned Linux toolchain, or `all` to render the hero and every feature demo together.

## Fuzzing

The `fuzz` package is intentionally outside the main workspace. `cargo verify` and hosted fuzzing use the same toolchain selector from `xtask`. Update only `FUZZ_TOOLCHAIN` when changing the pinned nightly. List and run targets with cargo-fuzz:

```console
fuzz_toolchain="$(cargo run --quiet --locked --package xtask -- fuzz-toolchain)"
cargo "+$fuzz_toolchain" fuzz list
cargo "+$fuzz_toolchain" fuzz run native_path -- -max_total_time=60 -max_len=4096
```

Crash artifacts and evolving corpora are ignored. Curated seeds under `fuzz/seeds` are reviewed source fixtures.

## Benchmarks

The hosted `benchmark.yml` retains the `criterion-benchmark-evidence` artifact for 90 days. It contains Criterion's raw samples and reports from `target/criterion`, the one-million and bounded-fan-in probe logs, plus `benchmark-context.txt`, which records the checked-out SHA, workflow run, runner image and CPU, commands, Rust toolchain, and `Cargo.lock` digest.

Run the same local measurements with:

```console
cargo +1.98.0 bench --bench tachyonfx --features internal --locked -- --noplot
cargo +1.98.0 bench --bench core --features internal --locked -- --noplot
```

The hosted workflow runs the one-million-tiny-file scale probe and the bounded-batch fan-in probe once with `--profile-time 1`. The premerged probe isolates reduction and late-page lookup cost. The bounded probe exercises production fan-in. Both use explicit private scan-store limits and can be reproduced locally with the commands below.

```console
EXCISE_BENCH_MILLION=1 cargo +1.98.0 bench --bench core --features internal --locked -- scan-store/million-tiny-files --noplot --profile-time 1
```

Measure the real bounded-batch million-file fan-in separately:

```console
EXCISE_BENCH_MILLION=1 EXCISE_BENCH_MILLION_FANIN=1 cargo +1.98.0 bench --bench core --features internal --locked -- scan-store/million-tiny-files/bounded-fan-in --noplot --profile-time 1
```

Both probes print deterministic logical read and write bytes, per-observation ratios, merge write amplification, retained and peak temporary bytes, phase wall time, and Unix process CPU time. `--profile-time 1` performs one scale smoke. Omit it on provisioned comparable hardware when collecting Criterion samples.

`core` measures publication and late-page queries across flat, wide, deep, and shared-link workloads (`scan-store/publication/*` and `scan-store/page-query/*`). With `EXCISE_BENCH_MILLION=1`, it adds one-million-file publication and late-page probes. `EXCISE_BENCH_MILLION_FANIN=1` also exercises the production bounded fan-in path. It measures a fixed 16,512-entry file-system walk at one, two, and eight workers, delivery of sixteen focus requests during an active scan, and rebuild-cancellation acknowledgement. `tachyonfx` measures completion-frame processing at `80x24`, `160x50`, and `200x80`.

To assess a reported regression, obtain the reference and candidate run IDs from their checks, download both evidence artifacts, and inspect their contexts before comparing measurements:

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

Only compare matching benchmark paths when the runner OS, architecture, image, CPU, and Rust toolchain are comparable. The following reads each saved Criterion median and its confidence interval without introducing a pass/fail threshold:

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

If the evidence indicates a meaningful difference, repeat it in a clean disposable checkout on comparable hardware. Use the recorded source SHAs and toolchain, retain the reference baseline in `target/criterion`, then let Criterion calculate the comparison:

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

Treat small host-local changes as noise unless supported by repeated statistical evidence on comparable hardware.

## Pull requests

See [CONTRIBUTING.md](../CONTRIBUTING.md) for review, safety, accessibility, authorship, and documentation requirements.
