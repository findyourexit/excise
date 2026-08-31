# Release Process

This runbook records the historical early-testing releases through `0.3.0` and the procedure for the first stable `1.0.0` release and future releases. It is a procedure, not authorization.

## The 0.1.1 Contract (Historical)

The published `0.1.1` release was for early testing. Its public library API and destructive behavior remained provisional until the project declared a stable line. Users were instructed to test only with disposable data and not to use it with irreplaceable files.

The release commit and candidate must agree on all of the following:

- `Cargo.toml`, `Cargo.lock`, the command-line version, and the changelog identify `0.1.1`.
- The crate is publishable. The release metadata must not set `publish = false`.
- The annotated `v0.1.1` tag points to the exact protected `main` commit that passed verification.
- Six target archives, their SHA-256 manifest, the SPDX JSON software bill of materials, and GitHub build attestations describe that same commit and version.
- The first-party Homebrew tap formula refers only to those immutable GitHub release assets.
- The tagged Nix flake and cargo-binstall metadata resolve the same immutable `0.1.1` release.

The release did not enable Scoop, WinGet, Homebrew Core, or any other package channel beyond the first-party Homebrew tap, tagged Nix flake, crates.io, and cargo-binstall metadata. Templates under `packaging/` are validation inputs unless a separately approved channel promotion says otherwise. The source formula at `packaging/homebrew-core/excise.rb.in` is for a possible future Homebrew Core submission. It is not the first-party tap formula.

## The 0.1.2 Corrective Release (Historical)

The corrective `0.1.2` release contains the post-`0.1.1` accounting hardening and fuzz-oracle fix. It is published but remains an early-testing release: its public library API and destructive behavior are provisional. The `0.1.2` publication record remains historical and immutable.

## The 0.2.0 Early-Testing Release

The `0.2.0` release packaged the dense storage map, accessible terminal presentation, animation, overflow reporting, and retained accounting work described in the changelog. It was a minor early-testing release because the public library API changed. Its publication record is historical and immutable.

The approved `0.2.0` publication used:

- Source commit: `f8329ce3ec5d338ee15459ec96a1f8897321b4ef`.
- Candidate workflow run: https://github.com/findyourexit/excise/actions/runs/33045125756
- Immutable publication workflow run: https://github.com/findyourexit/excise/actions/runs/33045511141
- Annotated tag: `v0.2.0`, pointing to the exact source commit.
- Successful native verification: https://github.com/findyourexit/excise/actions/runs/33044958150
- published GitHub Release assets, crates.io package, first-party Homebrew Tap, cargo-binstall metadata, and tagged Nix flake.

## The 0.3.0 Early-Testing Release

The `0.3.0` release packaged the private Rust API boundary and compatibility-policy work described in the changelog. It was a breaking minor release because provisional Rust module paths were removed from the default public surface. Its destructive behavior was provisional, and its publication record is historical and immutable. The active candidate, verification, and promotion procedure below targets `1.0.0`.

## The 1.0.0 Stable Release

The `1.0.0` release freezes the command-line tool, configuration, versioned JSON reports, deletion and accounting semantics, platform support policy, and exact-commit distribution procedure described by the v1 contract decision record below. It is the first stable release. Later incompatible changes require a major version. Additive report and configuration changes must preserve the documented compatibility rules.

## Approved `1.0.0` Publication

The approved stable publication used:

- Source commit: `b384a9ca6ac8d4853574083945d4a10d22b16817`.
- Protected-main native verification: https://github.com/findyourexit/excise/actions/runs/33117864079
- Exact-SHA candidate workflow: https://github.com/findyourexit/excise/actions/runs/33118939870
- Annotated tag: `v1.0.0`, carrying `candidate-run-id: 33118939870` and pointing to the exact source commit.
- Immutable publication workflow: https://github.com/findyourexit/excise/actions/runs/33119398710
- GitHub Release: https://github.com/findyourexit/excise/releases/tag/v1.0.0
- first-party Homebrew Tap formula commit: https://github.com/findyourexit/homebrew-tap/commit/9157017d736c23037e100ca6f13317f52c9c8683.

The published bundle contains six target archives, `checksums.sha256`, and `excise.spdx.json`. The checksum manifest, software bill of materials, archive contents, and all eight GitHub attestations were independently verified after publication. crates.io, cargo-binstall, Homebrew, and the tagged Nix flake each reported `1.0.0`. The support classifications are recorded in [SUPPORT.md](../SUPPORT.md). Corrective-release handling remains governed by [Reruns and Rollback](#reruns-and-rollback).

## The 1.0.1 Patch Release

The `1.0.1` release contains the housekeeping documentation rewrites, the DCO requirement removal, and the support-matrix documentation fixes described in the changelog. It is the first patch release on the v1 stable line.

## Approved `1.0.1` Publication

The approved patch publication used:

- Source commit: `6dddc26c6f7f2c9cb75b5715d00f315d5ac91d5c`.
- Protected-main native verification: https://github.com/findyourexit/excise/actions/runs/33149660834
- Exact-SHA candidate workflow: https://github.com/findyourexit/excise/actions/runs/33151782132
- Annotated tag: `v1.0.1`, carrying `candidate-run-id: 33151782132` and pointing to the exact source commit.
- Immutable publication workflow: https://github.com/findyourexit/excise/actions/runs/33152146266
- GitHub Release: https://github.com/findyourexit/excise/releases/tag/v1.0.1

The published bundle contains six target archives, `checksums.sha256`, and `excise.spdx.json`. The checksum manifest, software bill of materials, archive contents, and all eight GitHub attestations were independently verified after publication. crates.io and Homebrew each reported `1.0.1`.

## The 1.0.2 Patch Release

Use `1.0.2` for a narrowly scoped correction after `1.0.1`. A patch release must preserve the documented command-line, configuration, report, deletion, accounting, and support behavior. It should include a focused correction, update the `Unreleased` notes, and repeat the exact protected-commit candidate, checksum, software bill of materials, attestation, package-channel, and rollback checks before publication.


## Approved `1.0.2` Publication

The approved patch publication used:

- Source commit: `10e0803f91e2bb2aaa8f8572fc24a0fba4c23ffd`.
- Protected-main candidate workflow run: https://github.com/findyourexit/excise/actions/runs/33408059484
- Annotated tag: `v1.0.2`, carrying `candidate-run-id: 33408059484` and pointing to the exact source commit.
- Publication workflow run: https://github.com/findyourexit/excise/actions/runs/33408563018
- GitHub Release: https://github.com/findyourexit/excise/releases/tag/v1.0.2

The published bundle contains six target archives, `checksums.sha256`, and `excise.spdx.json`. All checksums, archives, and SBOM verified. crates.io and Homebrew tap updated.
## Preconditions & Clean Tree

Only a maintainer may start publication. Before creating a tag, dispatching a candidate, or using a publication credential:

1. Merge the focused release change to protected `main`. It must update the version and lockfile, move user-visible `Unreleased` entries into the dated changelog section, regenerate the man page and shell completions, and contain no unrelated source changes.
2. Review the deletion, accounting, schema, configuration, platform, compatibility, and release notes in the release PR.
3. Check out the exact protected commit and require a clean working tree. This check must report no tracked, staged, or untracked release input:

   ```console
   test -z "$(git status --porcelain=v1 --untracked-files=all)"
   git diff --exit-code
   git diff --cached --exit-code
   ```

   Do not use `--allow-dirty`, copy generated files from another checkout, or mix outputs from different commits. Ignored build output does not make a dirty tracked tree safe. Inspect any unexpected ignored release input before proceeding.
4. Confirm the commit, branch protection, and manifest version before dispatching the hosted candidate. Capture `source_sha="$(git rev-parse HEAD)"` from that exact protected commit and pass it to the workflow. The workflow rejects a moving ref, an unprotected ref, an unmerged commit, or a mismatched SHA.
5. Obtain the release approval and environment approval before enabling any write credential. Candidate generation is read-only. Publication is a separate, reviewed action.

## Local Candidate

Run these commands from the repository root on the clean release commit:

```console
(
  set -euo pipefail
  cargo verify
  cargo package --locked --list
  cargo publish --locked --dry-run
  cargo dist-local
)
```

`cargo verify` includes generated-file, schema, distribution-template, compilation, test, policy, fuzz, benchmark, and release-binary checks. `cargo publish --locked --dry-run` packages the exact crate without uploading it. It is the last safe check for the crates.io package contents and must pass without `--allow-dirty`.

`cargo dist-local` builds the host release archive and supporting metadata without publishing anything. It writes the host archive under `dist/`, a `dist/checksums.sha256` file, and a local formula at `dist/homebrew/excise.rb`. Inspect the archive before using any hosted artifact. The archive contains the release binary, `LICENSE`, `README.md`, generated man and completion files, schemas, `excise.cdx.json`, and `provenance.local.json`.

## Hosted Candidate

The manually dispatched `Release candidate artifacts` workflow in `.github/workflows/release.yml` checks out the explicit reviewed SHA and requires the input version and dispatch ID to match the package contract. Dispatch it only from protected `main`, and abort if `main` moves between capture and dispatch:

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

The candidate contains six immutable target archives, `checksums.sha256`, and `excise.spdx.json`. Verify the complete bundle while remaining outside the source worktree:

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
  for archive in excise-*.tar.gz; do tar -tzf "$archive" >/dev/null; done
  for archive in excise-*.zip; do unzip -t "$archive" >/dev/null; done
  for subject in excise-*.tar.gz excise-*.zip checksums.sha256 excise.spdx.json; do
    gh attestation verify "$subject" \
      --repo findyourexit/excise \
      --signer-workflow findyourexit/excise/.github/workflows/release.yml \
      --source-digest "$source_sha" \
      --source-ref refs/heads/main
  done
)
```

Confirm that every archive contains its target binary, `LICENSE`, `generated/man/excise.1`, and `schemas/scan-report.schema.json`. The software bill of materials and provenance files are candidate-bundle evidence and are not silently substituted for an archive. The workflow retains candidate artifacts for one day. Retention is a validation convenience, not publication or durable distribution.

## Promotion Order & Publication Semantics

The approved `0.1.1` publication used:

- Source commit: `59eb0d17295eaef99305521651107c28dce27613`.
- Candidate workflow run: [32733774029](https://github.com/findyourexit/excise/actions/runs/32733774029)
- Annotated tag: `v0.1.1`, carrying `candidate-run-id: 32733774029`.
- publication recovery run: [32742153533](https://github.com/findyourexit/excise/actions/runs/32742153533).

The tag was never moved. The recovery workflow verified and promoted the exact candidate bytes without rebuilding them.

The approved `0.1.2` publication used:

- Source commit: `94987c5f48b7814b6c035cb61931cf7aeb11eab0`.
- Candidate workflow run: [32798065116](https://github.com/findyourexit/excise/actions/runs/32798065116)
- Annotated tag: `v0.1.2`, carrying `candidate-run-id: 32798065116`.
- Publication workflow run: [32798482623](https://github.com/findyourexit/excise/actions/runs/32798482623)
- post-publication native verification: [32800896471](https://github.com/findyourexit/excise/actions/runs/32800896471).

The publication semantics are:

1. The `release` job creates the GitHub release from the promoted candidate bundle. It reuses an existing published release only after the exact tag object, complete asset set, and every asset checksum match. Published mismatches and unexpected drafts are refused. A matching non-prerelease draft may be repaired with the reverified candidate assets.
2. The `publish-crate` job publishes the crate once after the release job succeeds. Do not run `cargo publish` manually. The job accepts an existing version only after matching its registry checksum and non-yanked state. Otherwise it fails before retrying.
3. After the `homebrew-tap` environment approval, the `publish-homebrew` job renders and pushes only `Formula/excise.rb` from the verified source SHA. Review the resulting tap commit and formula after the job. Do not edit that external repository from this checkout.

The crates.io package follows the release commit's Cargo exclusions (`.cargo`, `.github`, `.gitmessage`, `assets`, `tapes`, `handoff`, and `packaging`). `cargo package --locked --list` is the source of truth. The package does not turn the GitHub archive or tap into crate contents. The `1.0.0` API boundary is command-line only. Publishing it is not a promise of a supported Rust library.

## Nix & cargo-binstall Verification

The tagged Nix flake is a source-build channel, while cargo-binstall downloads target-specific release archives. Verify the channels independently:

```console
nix flake check github:findyourexit/excise/v1.0.0
nix eval --raw "github:findyourexit/excise/v1.0.0#packages.$(nix eval --raw --impure --expr builtins.currentSystem).default.version"
nix run github:findyourexit/excise/v1.0.0 -- --version
nix run github:findyourexit/excise/v1.0.0 -- --format table /path/to/inspect
(
  set -euo pipefail
  binstall_dir="$(mktemp -d "${TMPDIR:-/tmp}/excise-binstall.XXXXXX")"
  readonly binstall_dir
  trap 'rm -rf -- "$binstall_dir"' EXIT
  cargo binstall --no-confirm --force --install-path "$binstall_dir" --version 1.0.0 excise
  "$binstall_dir/excise" --version
)
```

The `nix eval` and `nix run -- --version` commands verify the tagged Nix package independently. The isolated `cargo binstall` block verifies a fresh target-specific archive and invokes that exact binary. Do not treat one channel's successful command as evidence for the other.


## Homebrew Tap Verification

The first-party binary formula is installed from `findyourexit/homebrew-tap`, not from Homebrew Core:

```console
brew tap findyourexit/tap https://github.com/findyourexit/homebrew-tap.git
brew install findyourexit/tap/excise
brew fetch --force --retry findyourexit/tap/excise
brew audit --formula --strict --online findyourexit/tap/excise
brew test findyourexit/tap/excise
brew info findyourexit/tap/excise
excise --version
```

`brew fetch` checks the formula's archive URL and SHA-256. `brew audit` checks formula policy. `brew test` runs the formula's version and JSON scan smoke checks. `brew info` confirms the selected version and tap. Also inspect the rendered formula with `brew cat findyourexit/tap/excise`. Every URL must be a `releases/download/v1.0.0/` asset and every checksum must match `checksums.sha256`. The source formula in `packaging/homebrew-core/excise.rb.in` has different build semantics and must not be used as evidence that Homebrew Core has accepted the package.

## Credentials & Approvals

Keep candidate and publication credentials separate. The candidate workflow needs read access to the source and artifact services plus the permissions required for its attestation step. It must not receive a crates.io or tap write token. A publication environment requires explicit maintainer approval and, where configured, a second reviewer:

- `CARGO_REGISTRY_TOKEN` or Cargo's credential file authorizes `cargo publish`. Use it only for the approved command. Never print it, commit it, or put it in a tape or report.
- `GH_TOKEN` authorizes local `gh` commands. An Actions `GITHUB_TOKEN` needs explicit `contents: write` only in the approved promotion job. The read-only candidate job must not be broadened casually.
- The external tap requires a separately approved GitHub credential with write access to `findyourexit/homebrew-tap`. Repository access is not implied by access to `findyourexit/excise`.

Do not run with shell tracing (`set -x`) around secrets. Review `env`, repository selection, ref, SHA, version, and destination before each write. If a required credential or approval is absent, stop before the write step. Do not substitute a personal token or a different repository.

## Reruns & Rollback

Candidate generation is safe to rerun for a transient workflow failure, but rerun the same version and exact source SHA and revalidate the complete bundle. If source changes after a failed candidate, land the fix on protected `main`, dispatch a new candidate, and discard the old bundle. Never mix archives, checksums, software bills of materials, or attestations from different SHAs. A missing one-day artifact is regenerated only through the same gated workflow.

Publication can be retried by rerunning the failed workflow after inspecting the GitHub tag, release, assets, the crates.io `excise` version, and the external tap commit. The release job safely reuses only a published release whose exact candidate asset set and checksums match. Continue only with the missing, reviewed step. Never rebuild an already published asset and never republish an existing crate version.

If a workflow defect is fixed on protected `main` after the release tag already exists, do not move the tag. Dispatch the fixed workflow in immutable publication-recovery mode, passing the original candidate source SHA, tag, and candidate run ID:

```console
gh workflow run release.yml \
  --repo findyourexit/excise \
  --ref main \
  --field mode=publish-existing \
  --field version="$version" \
  --field source_sha="$source_sha" \
  --field dispatch_id="$recovery_id" \
  --field tag=v1.0.0 \
  --field candidate_run_id="$run_id"
```

The recovery gate verifies that protected `main` has not moved during dispatch, that the immutable tag still targets `source_sha`, and that `run_id` is the successful candidate for that exact source before reusing its artifacts.

If a deletion-safety or release-integrity defect is found, stop promotion and mark the affected channel unavailable while preserving the candidate evidence. Do not move, delete, or overwrite an existing tag or GitHub asset. A rollback cannot undo filesystem deletion and must not ask users to rerun a destructive command. After the fix is reviewed, publish a new corrective version, such as `0.2.1`, then update each channel to that immutable version. A crates.io yank only prevents new dependency resolution. It does not erase an already downloaded crate.

## v1.0.0 Readiness Gate

The `1.0.0` release is the first stable line and is authorized only after the public behavior, supported-platform policy, safety evidence, and release procedure below are explicit and reviewed. The `0.3.x` line remains historical early testing.

### Contract Decision Record

| Area | Required v1 decision | Current position |
| --- | --- | --- |
| Command line | Freeze command names, options, defaults, help text, and noninteractive behavior. Additive changes are permitted. Incompatible changes require a major version. | Existing command definitions and generated files are the candidate baseline. |
| Environment and configuration | Preserve command line, environment, versioned TOML file, and default precedence. Reject unknown or invalid values. Reject every file version other than `1`. Do not silently migrate or reinterpret configuration. | `version = 1`, precedence, and rejection are implemented and tested. |
| Table output | Treat table output as human-facing and not stable for programs. Preserve safety and escaping. Direct programmatic consumers to JSON instead of parsing headings or columns. | JSON is the machine-readable format. Table layout is not stable. |
| JSON reports | Keep the meanings of the `scan-report`, `deletion-history`, and `native-path` version 1 formats stable. Add fields only when consumers can ignore them. Increase the format version for incompatible changes. | Published version 1 formats reject unknown fields and have regression tests. |
| Exit classes | Preserve the documented numeric classes. Keep uncertain, partial, and interrupted results distinct from exact results. | Codes are implemented and tested. |
| Deletion | Preserve file identity checks without following links, independent listing, repeated checks, root and summary rejection, explicit partial results, and permanent deletion. | The deletion contract and focused safety suite are the baseline. |
| Accounting | Preserve space counted once per file identity, separate file length, conservative reclaimable bounds, and explicit unknowns. Do not claim exact physical shared-storage totals. | The accounting contract and fixtures are the baseline. |
| Rust interface | Treat the command-line tool, configuration, and versioned reports as the supported product. Rust implementation modules are private. The crate carries no stable Rust interface promise. | The private implementation boundary is implemented. The crate exposes no supported Rust interface. |
| Platforms | Fully support only targets tested on the actual system. Keep build-only targets clearly marked until they have runtime evidence. | Three targets are supported. Three archives remain published for best-effort use. |
| Distribution and governance | Require exact protected-commit artifacts, checksums, a software bill of materials, origin records, rollback, and an explicit release authority. | Artifact identity and rollback are operational. The lead maintainer authority is explicit in `GOVERNANCE.md`, with an additional maintainer review when one is appointed. |

The table above is the normative `1.0.0` public-contract decision record. Maintain it through reviewed pull requests. Any implementation change that affects a row requires that row to be reviewed again before release authorization. The lead maintainer listed in `MAINTAINERS.md` owns final product, safety, and release decisions and may authorize publication only after this gate, protected-main ruleset checks, and publication-environment approval. When a second maintainer is appointed, the additional requirements in `GOVERNANCE.md` apply.

The release candidate must also record the exact source commit, candidate run, artifact checksums, software bill of materials, attestations, package-channel results, and reviewer decision against this table. A passing automated check is evidence for its scope. It is not approval for a different scope.

### Required Exit Evidence

Before the `v1.0.0` release PR can be approved, attach:

1. A reviewed public-contract decision record covering every row above.
2. Upgrade and compatibility tests for command-line behavior and configuration, including rejection of unsupported versions, table safety and escaping, JSON formats, file paths, and exit classes. `tests/cli_contract_smoke.rs` must exercise the binary-level portion of this contract.
3. Runtime evidence on every target classified as supported, together with an explicit disposition for build-only targets.
4. Focused security reviews of deletion, file identity, temporary storage, terminal restoration, and release systems.
5. Dependency, unsafe-boundary, fuzz, benchmark, packaging, software bill of materials, checksum, and provenance evidence from the exact release commit.
6. A clean protected-commit release rehearsal and a documented corrective-release procedure.

An empty list of user reports is not evidence that a behavior is safe. Keep the early-testing warning until the gate has evidence, not merely until the version number changes.

## Historical Tags

Tags `0.1.0` through `0.11.0` are preserved Diskonaut releases. They are not Excise releases and must not be moved, deleted, or reused. The `v0.1.1` tag is a new Excise tag. Do not infer that the preserved `0.1.0` tag identifies Excise merely because the changelog contains an Excise `0.1.0` section.
