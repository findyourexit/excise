# Development

## Toolchain

The workspace uses Rust 1.88 and edition 2024. Install the pinned toolchain and supported compilation targets:

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

The published target set has separate native-behavior and release-artifact evidence. The `1.0.0` support policy is runtime evidence first: only native behavioral targets are fully supported.

## Target evidence

| Target | Support classification | Published evidence |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` (x86_64 Linux) | Supported in `1.0.0`; native behavioral target | Native verification plus hosted release build/archive |
| `aarch64-apple-darwin` (AArch64 macOS) | Supported in `1.0.0`; native behavioral target | Native verification plus hosted release build/archive |
| `x86_64-pc-windows-msvc` (x86_64 Windows) | Supported in `1.0.0`; native behavioral target | Native verification plus hosted release build/archive |
| `x86_64-apple-darwin` (x86_64 macOS) | Build-only/best-effort; compile/archive only | Hosted release build/archive job |
| `aarch64-unknown-linux-gnu` (AArch64 Linux) | Build-only/best-effort; compile/archive only | Hosted release build/archive job |
| `aarch64-pc-windows-msvc` (AArch64 Windows) | Build-only/best-effort; compile/archive only | Hosted release build/archive job |

The native behavioral rows are the complete `1.0.0` runtime support set. The release pipeline continues to publish all six archives, but the three compile-only targets carry no native runtime guarantee and remain best-effort until promoted by native evidence. A successful hosted build or archive demonstrates release compilation and packaging, not native runtime compatibility.

The target rows and workflow matrices are checked by `cargo run --locked --package xtask -- check-support-matrix` and are included in `cargo verify`.

## Filesystem and terminal scope

The supported runtime target policy applies to local filesystem paths accessed through the documented operating-system APIs. Filesystem-provider-specific ACL, sharing, allocation, network or remote filesystem, reflink, clone, compression, and shared-extent behavior is best-effort unless separately evidenced; unknown allocation remains explicit rather than guessed.

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

## 1.0.0 release candidate checks

The `1.0.0` release is the stable CLI, configuration, and report contract. From a clean checkout at the release commit, run the focused checks before requesting the hosted candidate:

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

The candidate aliases used above in `.cargo/config.toml` map `cargo verify`, `cargo check-generated`, and `cargo dist-local` to locked `xtask` commands. `cargo package --locked --list` exposes the exact crates.io file set; `cargo publish --locked --dry-run` validates packaging without uploading. `xtask dist-local` owns the local `dist/` staging path and writes the host archive, `dist/checksums.sha256`, and `dist/homebrew/excise.rb`; it neither publishes them nor authorizes a release.

For the hosted candidate, dispatch the workflow only from the exact protected `main` commit and pass the manifest version, reviewed commit SHA, and a unique dispatch ID explicitly:

```console
set -euo pipefail
source_sha="$(git rev-parse HEAD)"
candidate_dir="$(mktemp -d "${TMPDIR:-/tmp}/excise-candidate.XXXXXX")"
trap "$(printf 'rm -rf -- %q' "$candidate_dir")" EXIT
dispatch_seed="$(date -u +%s)-$$-$RANDOM"
if command -v sha256sum >/dev/null 2>&1; then
  dispatch_id="$(printf '%s' "$dispatch_seed" | sha256sum | cut -c1-32)"
else
  dispatch_id="$(printf '%s' "$dispatch_seed" | shasum -a 256 | cut -c1-32)"
fi
run_url="$(gh workflow run release.yml --repo findyourexit/excise --ref main --field version=1.0.0 --field source_sha="$source_sha" --field dispatch_id="$dispatch_id")"
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
  jq -e --arg version 1.0.0 '([.packages[] | select(.name == "excise" and .versionInfo == $version)] | length == 1)' excise.spdx.json
  archives=(
    excise-x86_64-unknown-linux-gnu-v1.0.0.tar.gz
    excise-aarch64-unknown-linux-gnu-v1.0.0.tar.gz
    excise-x86_64-apple-darwin-v1.0.0.tar.gz
    excise-aarch64-apple-darwin-v1.0.0.tar.gz
    excise-x86_64-pc-windows-msvc-v1.0.0.zip
    excise-aarch64-pc-windows-msvc-v1.0.0.zip
  )
  for archive in "${archives[@]}"; do
    test -s "$archive"
  done
  for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
    archive="excise-${target}-v1.0.0.tar.gz"
    root="excise-${target}-v1.0.0"
    tar -tzf "$archive" | grep -Fqx "$root/excise"
    tar -tzf "$archive" | grep -Fqx "$root/LICENSE"
    tar -tzf "$archive" | grep -Fqx "$root/generated/man/excise.1"
    tar -tzf "$archive" | grep -Fqx "$root/schemas/scan-report.schema.json"
  done
  for target in x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
    archive="excise-${target}-v1.0.0.zip"
    root="excise-${target}-v1.0.0"
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
git tag -a v1.0.0 "$source_sha" -m "candidate-run-id: $run_id"
git push origin v1.0.0
```

The push-triggered workflow requires that exact annotated-tag candidate ID; never substitute a different candidate run or a lightweight tag.

## Full verification

`cargo verify` runs the complete local suite. It expects:

- Cargo Deny 0.20.2;
- actionlint 1.7.12;
- lychee 0.24.2;
- Node.js/npm for Renovate 44.34.0 validation;
- cargo-fuzz 0.13.2 with `nightly-2026-08-18`; and
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

The `cargo demo` alias is current `main` development behavior rather than a release-package command. It delegates to `xtask demo`; refresh the VHS demonstration after user-visible CLI or TUI changes and review the output before a release:

```console
(
  set -euo pipefail
  cargo +1.88.0 build --release --locked --package excise
  cargo demo
)
```

Run the tape from the repository root. `xtask demo` validates `tapes/demo.tape`, renders it at the tape's 24 fps, then resamples it to 20 fps while rebuilding a 64-colour palette without dithering and applying lossy GIF quantisation. It owns the `assets/demo-main.rendered.gif`, `assets/demo-main.palette.gif`, and `assets/demo-main.quantised.gif` staging paths and atomically promotes the last to `assets/demo-main.gif` only after it passes the published GIF's weight ceiling; a failure leaves the committed current-main asset untouched and never changes `assets/demo.gif`, the historical `0.1.2` recording. It needs `vhs`, `ttyd`, `ffmpeg`, `ffprobe`, and `gifsicle` on `PATH`, plus a Unix-like `bash` and core utilities: the tape explicitly selects `bash`, creates its fixture under `/tmp`, and invokes utilities including `head`, `mkdir`, and `rm`.

Invoking `vhs tapes/demo.tape` directly writes an unoptimised 24 fps sequence to `assets/demo-main.gif` and skips the 20 fps resampling, palette rebuild, quantisation, and size gate, so it must not be used to refresh the committed current-main hero.

## Fuzzing

The `fuzz` package is intentionally outside the main workspace. List and run targets with cargo-fuzz:

```console
cargo +nightly-2026-08-18 fuzz list
cargo +nightly-2026-08-18 fuzz run native_path -- -max_total_time=60 -max_len=4096
```

Crash artifacts and evolving corpora are ignored. Curated seeds under `fuzz/seeds` are reviewed source fixtures.

## Benchmarks

```console
cargo bench --bench core --features internal --locked -- --noplot
cargo bench --bench tachyonfx --features internal --locked -- --noplot
```

Treat small host-local changes as noise unless supported by repeated statistical evidence on comparable hardware.

## Pull requests

See [CONTRIBUTING.md](../CONTRIBUTING.md) for DCO, review, safety, accessibility, and documentation requirements.
