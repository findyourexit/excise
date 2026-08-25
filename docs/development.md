# Development

## Toolchain

The workspace uses Rust 1.88 and edition 2024. Install the pinned toolchain and supported compilation targets:

```console
rustup show
rustup target add \
  aarch64-apple-darwin \
  x86_64-apple-darwin \
  x86_64-unknown-linux-gnu \
  x86_64-pc-windows-msvc
```

A host may not be able to install every target. The published target set has separate native-behavior and release-artifact evidence.

## Target evidence

| Target | Support classification | Published evidence |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` (x86_64 Linux) | Native behavioral target | Native verification plus hosted release build/archive |
| `aarch64-apple-darwin` (AArch64 macOS) | Native behavioral target | Native verification plus hosted release build/archive |
| `x86_64-pc-windows-msvc` (x86_64 Windows) | Native behavioral target | Native verification plus hosted release build/archive |
| `x86_64-apple-darwin` (x86_64 macOS) | Release target; compile/archive only | Hosted release build/archive job |
| `aarch64-unknown-linux-gnu` (AArch64 Linux) | Release target; compile/archive only | Hosted release build/archive job |
| `aarch64-pc-windows-msvc` (AArch64 Windows) | Release target; compile/archive only | Hosted release build/archive job |

Only the native behavioral rows carry published runtime-behavior evidence. Compile-only targets must not be treated as fully behaviorally validated: a successful hosted build or archive demonstrates release compilation and packaging, not native runtime compatibility.

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

## 0.1.2 candidate checks

The `0.1.2` corrective release remains early testing, not a stable API or a promise that destructive behavior is safe for irreplaceable data. From a clean checkout at the release commit, run the focused checks before requesting the hosted candidate:

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

The aliases in `.cargo/config.toml` map `cargo verify`, `cargo check-generated`, and `cargo dist-local` to the locked `xtask` commands. `cargo package --locked --list` exposes the exact crates.io file set; `cargo publish --locked --dry-run` validates packaging without uploading. `cargo dist-local` writes the host archive, `dist/checksums.sha256`, and `dist/homebrew/excise.rb`; it does not publish or authorize a release.

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
run_url="$(gh workflow run release.yml --repo findyourexit/excise --ref main --field version=0.1.2 --field source_sha="$source_sha" --field dispatch_id="$dispatch_id")"
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
  jq -e --arg version 0.1.2 '([.packages[] | select(.name == "excise" and .versionInfo == $version)] | length == 1)' excise.spdx.json
  archives=(
    excise-x86_64-unknown-linux-gnu-v0.1.2.tar.gz
    excise-aarch64-unknown-linux-gnu-v0.1.2.tar.gz
    excise-x86_64-apple-darwin-v0.1.2.tar.gz
    excise-aarch64-apple-darwin-v0.1.2.tar.gz
    excise-x86_64-pc-windows-msvc-v0.1.2.zip
    excise-aarch64-pc-windows-msvc-v0.1.2.zip
  )
  for archive in "${archives[@]}"; do
    test -s "$archive"
  done
  for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
    archive="excise-${target}-v0.1.2.tar.gz"
    root="excise-${target}-v0.1.2"
    tar -tzf "$archive" | grep -Fqx "$root/excise"
    tar -tzf "$archive" | grep -Fqx "$root/LICENSE"
    tar -tzf "$archive" | grep -Fqx "$root/generated/man/excise.1"
    tar -tzf "$archive" | grep -Fqx "$root/schemas/scan-report.schema.json"
  done
  for target in x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
    archive="excise-${target}-v0.1.2.zip"
    root="excise-${target}-v0.1.2"
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
git tag -a v0.1.2 "$source_sha" -m "candidate-run-id: $run_id"
git push origin v0.1.2
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

Commit generated changes with the source contract that produced them. Regenerate VHS demonstrations after user-visible CLI or TUI changes and review the output before a release:

```console
(
  set -euo pipefail
  cargo +1.88.0 build --release --locked --package excise
  for tape in tapes/*.tape; do
    vhs "$tape"
  done
)
```

Run the tapes from the repository root. Each tape owns its output path; do not move recordings into a different path to make a check pass. The repository intentionally ignores generated media under `docs/**/assets/`, `docs/**/recordings/`, and `docs/**/*.cast`; preserve the checked-in tape source and obtain a reviewer approval for any externally published recording.

## Fuzzing

The `fuzz` package is intentionally outside the main workspace. List and run targets with cargo-fuzz:

```console
cargo +nightly-2026-08-18 fuzz list
cargo +nightly-2026-08-18 fuzz run native_path -- -max_total_time=60 -max_len=4096
```

Crash artifacts and evolving corpora are ignored. Curated seeds under `fuzz/seeds` are reviewed source fixtures.

## Benchmarks

```console
cargo bench --bench core --locked -- --noplot
cargo bench --bench tachyonfx --locked -- --noplot
```

Treat small host-local changes as noise unless supported by repeated statistical evidence on comparable hardware.

## Pull requests

See [CONTRIBUTING.md](../CONTRIBUTING.md) for DCO, review, safety, accessibility, and documentation requirements.
