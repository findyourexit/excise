# Release process

This runbook records the published `0.1.1` early-testing release contract, the corrective `0.1.2` release contract, the published `0.2.0` release evidence, and the evidence required for the `0.3.0` release and future releases. It is a procedure, not authorization.

## The 0.1.1 contract (historical)

The published `0.1.1` release is for early testing. Its public library API and destructive behavior remain provisional until the project declares a stable line. Test only with disposable data; do not treat this release as suitable for irreplaceable files.

The release commit and candidate must agree on all of the following:

- `Cargo.toml`, `Cargo.lock`, the CLI version, and the changelog identify `0.1.1`;
- the crate is publishable (the release metadata must not set `publish = false`);
- the annotated `v0.1.1` tag points to the exact protected `main` commit that passed verification;
- six target archives, their SHA-256 manifest, the SPDX JSON SBOM, and GitHub build attestations describe that same commit and version;
- the first-party Homebrew tap formula refers only to those immutable GitHub release assets; and
- the tagged Nix flake and cargo-binstall metadata resolve the same immutable `0.1.1` release.

The release does not enable Scoop, WinGet, Homebrew Core, or any other package channel beyond the first-party Homebrew tap, tagged Nix flake, crates.io, and cargo-binstall metadata. Templates under `packaging/` are validation inputs unless a separately approved channel promotion says otherwise. The source formula at `packaging/homebrew-core/excise.rb.in` is for a possible future Homebrew Core submission; it is not the first-party tap formula.

## The 0.1.2 corrective release (historical)

The corrective `0.1.2` release contains the post-`0.1.1` accounting hardening and fuzz-oracle fix. It is published but remains an early-testing release: its public library API and destructive behavior are provisional. The `0.1.2` publication record remains historical and immutable.

## The 0.2.0 early-testing release

The `0.2.0` release packages the dense storage map, accessible terminal presentation, animation, overflow reporting, and retained-accounting work described in the changelog. It was a minor early-testing release because the public library API changed; its publication record is historical and immutable.

The approved `0.2.0` publication used:

- source commit: `f8329ce3ec5d338ee15459ec96a1f8897321b4ef`;
- candidate workflow run: https://github.com/findyourexit/excise/actions/runs/33045125756;
- immutable publication workflow run: https://github.com/findyourexit/excise/actions/runs/33045511141;
- annotated tag: `v0.2.0`, pointing to the exact source commit;
- successful native verification: https://github.com/findyourexit/excise/actions/runs/33044958150;
- published GitHub Release assets, crates.io package, first-party Homebrew Tap, cargo-binstall metadata, and tagged Nix flake.

## The 0.3.0 early-testing release

The `0.3.0` release packages the private Rust API boundary and compatibility-policy work described in the changelog. It is a breaking minor release because provisional Rust module paths are removed from the default public surface; it remains early testing, and its destructive behavior is provisional. Use `0.3.0` and `v0.3.0` in the active candidate, verification, and promotion procedure below.

## Preconditions and clean tree

Only a maintainer may start publication. Before creating a tag, dispatching a candidate, or using a publication credential:

1. Merge the focused release change to protected `main`. It must update the version and lockfile, move user-visible `Unreleased` entries into the dated changelog section, regenerate the man page and shell completions, and contain no unrelated source changes.
2. Review the deletion, accounting, schema, configuration, platform, compatibility, and early-testing notes in the release PR.
3. Check out the exact protected commit and require a clean working tree. This check must report no tracked, staged, or untracked release input:

   ```console
   test -z "$(git status --porcelain=v1 --untracked-files=all)"
   git diff --exit-code
   git diff --cached --exit-code
   ```

   Do not use `--allow-dirty`, copy generated files from another checkout, or mix outputs from different commits. Ignored build output does not make a dirty tracked tree safe; inspect any unexpected ignored release input before proceeding.
4. Confirm the commit, branch protection, and manifest version before dispatching the hosted candidate. Capture `source_sha="$(git rev-parse HEAD)"` from that exact protected commit and pass it to the workflow; the workflow rejects a moving ref, an unprotected ref, an unmerged commit, or a mismatched SHA.
5. Obtain the release approval and environment approval before enabling any write credential. Candidate generation is read-only; publication is a separate, reviewed action.

## Local candidate

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

`cargo dist-local` builds the host release archive and supporting metadata without publishing anything. It writes the host archive under `dist/`, a `dist/checksums.sha256` file, and a local formula at `dist/homebrew/excise.rb`. Inspect the archive before using any hosted artifact; the archive contains the release binary, `LICENSE`, `README.md`, generated man/completion files, schemas, `excise.cdx.json`, and `provenance.local.json`.

## Hosted candidate

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
run_url="$(gh workflow run release.yml --repo findyourexit/excise --ref main --field version=0.3.0 --field source_sha="$source_sha" --field dispatch_id="$dispatch_id")"
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
  jq -e --arg version 0.3.0 '([.packages[] | select(.name == "excise" and .versionInfo == $version)] | length == 1)' excise.spdx.json
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

Confirm that every archive contains its target binary, `LICENSE`, `generated/man/excise.1`, and `schemas/scan-report.schema.json`; the SBOM and provenance files are candidate-bundle evidence and are not silently substituted for an archive. The workflow retains candidate artifacts for one day. Retention is a validation convenience, not publication or durable distribution.
## Promotion order and publication semantics

The approved `0.1.1` publication used:

- source commit: `59eb0d17295eaef99305521651107c28dce27613`;
- candidate workflow run: [32733774029](https://github.com/findyourexit/excise/actions/runs/32733774029);
- annotated tag: `v0.1.1`, carrying `candidate-run-id: 32733774029`;
- publication recovery run: [32742153533](https://github.com/findyourexit/excise/actions/runs/32742153533).

The tag was never moved. The recovery workflow verified and promoted the exact candidate bytes without rebuilding them.

The approved `0.1.2` publication used:

- source commit: `94987c5f48b7814b6c035cb61931cf7aeb11eab0`;
- candidate workflow run: [32798065116](https://github.com/findyourexit/excise/actions/runs/32798065116);
- annotated tag: `v0.1.2`, carrying `candidate-run-id: 32798065116`;
- publication workflow run: [32798482623](https://github.com/findyourexit/excise/actions/runs/32798482623);
- post-publication native verification: [32800896471](https://github.com/findyourexit/excise/actions/runs/32800896471).

The publication semantics are:

1. The `release` job creates the GitHub release from the promoted candidate bundle. It reuses an existing published release only after the exact tag object, complete asset set, and every asset checksum match; published mismatches and unexpected drafts are refused, while a matching non-prerelease draft may be repaired with the reverified candidate assets.
2. The `publish-crate` job publishes the crate once after the release job succeeds. Do not run `cargo publish` manually; the job accepts an existing version only after matching its registry checksum and non-yanked state, and otherwise fails before retrying.
3. After the `homebrew-tap` environment approval, the `publish-homebrew` job renders and pushes only `Formula/excise.rb` from the verified source SHA. Review the resulting tap commit and formula after the job; do not edit that external repository from this checkout.

The crates.io package follows the release commit's Cargo exclusions (`.cargo`, `.github`, `.gitmessage`, `assets`, `tapes`, `handoff`, and `packaging`); `cargo package --locked --list` is the source of truth. It does not turn the GitHub archive or tap into crate contents. The `0.3.0` API boundary is CLI-only; publishing it is not a promise of a supported Rust library.

## Nix and cargo-binstall verification

The tagged Nix flake is a source-build channel, while cargo-binstall downloads target-specific release archives. Verify the channels independently:

```console
nix flake check github:findyourexit/excise/v0.3.0
nix eval --raw "github:findyourexit/excise/v0.3.0#packages.$(nix eval --raw --impure --expr builtins.currentSystem).default.version"
nix run github:findyourexit/excise/v0.3.0 -- --version
nix run github:findyourexit/excise/v0.3.0 -- --format table /path/to/inspect
(
  set -euo pipefail
  binstall_dir="$(mktemp -d "${TMPDIR:-/tmp}/excise-binstall.XXXXXX")"
  readonly binstall_dir
  trap 'rm -rf -- "$binstall_dir"' EXIT
  cargo binstall --no-confirm --force --install-path "$binstall_dir" --version 0.3.0 excise
  "$binstall_dir/excise" --version
)
```

The `nix eval` and `nix run -- --version` commands verify the tagged Nix package independently. The isolated `cargo binstall` block verifies a fresh target-specific archive and invokes that exact binary; do not treat one channel's successful command as evidence for the other.


## Homebrew tap verification

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

`brew fetch` checks the formula's archive URL and SHA-256, `brew audit` checks formula policy, `brew test` runs the formula's version and JSON-scan smoke checks, and `brew info` confirms the selected version and tap. Also inspect the rendered formula with `brew cat findyourexit/tap/excise`; every URL must be a `releases/download/v0.3.0/` asset and every checksum must match `checksums.sha256`. The source formula in `packaging/homebrew-core/excise.rb.in` has different build semantics and must not be used as evidence that Homebrew Core has accepted the package.

## Credentials and approvals

Keep candidate and publication credentials separate. The candidate workflow needs read access to the source and artifact services plus the permissions required for its attestation step; it must not receive a crates.io or tap write token. A publication environment requires explicit maintainer approval and, where configured, a second reviewer:

- `CARGO_REGISTRY_TOKEN` or Cargo's credential file authorizes `cargo publish`; use it only for the approved command and never print it, commit it, or put it in a tape or report.
- `GH_TOKEN` authorizes local `gh` commands. An Actions `GITHUB_TOKEN` needs explicit `contents: write` only in the approved promotion job; the read-only candidate job must not be broadened casually.
- The external tap requires a separately approved GitHub credential with write access to `findyourexit/homebrew-tap`; repository access is not implied by access to `findyourexit/excise`.

Do not run with shell tracing (`set -x`) around secrets. Review `env`, repository selection, ref, SHA, version, and destination before each write. If a required credential or approval is absent, stop before the write step; do not substitute a personal token or a different repository.

## Reruns and rollback

Candidate generation is safe to rerun for a transient workflow failure, but rerun the same version and exact source SHA and revalidate the complete bundle. If source changes after a failed candidate, land the fix on protected `main`, dispatch a new candidate, and discard the old bundle; never mix archives, checksums, SBOMs, or attestations from different SHAs. A missing one-day artifact is regenerated only through the same gated workflow.

Publication can be retried by rerunning the failed workflow after inspecting the GitHub tag/release/assets, the crates.io `excise` version, and the external tap commit. The release job safely reuses only a published release whose exact candidate asset set and checksums match; continue only with the missing, reviewed step, never rebuild an already published asset, and never republish an existing crate version.

If a workflow defect is fixed on protected `main` after the release tag already exists, do not move the tag. Dispatch the fixed workflow in immutable publication-recovery mode, passing the original candidate source SHA, tag, and candidate run ID:

```console
gh workflow run release.yml \
  --repo findyourexit/excise \
  --ref main \
  --field mode=publish-existing \
  --field version="$version" \
  --field source_sha="$source_sha" \
  --field dispatch_id="$recovery_id" \
  --field tag=v0.3.0 \
  --field candidate_run_id="$run_id"
```

The recovery gate verifies that protected `main` has not moved during dispatch, that the immutable tag still targets `source_sha`, and that `run_id` is the successful candidate for that exact source before reusing its artifacts.

If a destructive-safety or release-integrity defect is found, stop promotion and mark the affected channel unavailable while preserving the candidate evidence. Do not move, delete, or overwrite an existing tag or GitHub asset. A rollback cannot undo filesystem deletion and must not ask users to rerun a destructive command; after the fix is reviewed, publish a new corrective version (for example `0.2.1`), then update each channel to that immutable version. A crates.io yank only prevents new dependency resolution; it does not erase an already downloaded crate.

## v1.0.0 readiness gate

`1.0.0` is authorized only after the public behavior, supported-platform policy, safety evidence, and release procedure below are explicit and reviewed. The `0.3.x` line remains early testing until this gate is complete.

### Contract decisions to freeze

| Area | Required v1 decision | Current position |
| --- | --- | --- |
| CLI | Freeze command names, flags, defaults, help text, and non-TTY behavior. Additive changes are permitted; incompatible changes require a major version. | Existing CLI and generated artifacts are the candidate baseline. |
| Environment and configuration | Preserve command line > environment > versioned TOML > defaults. Reject unknown or invalid values. Reject every file version other than `1`; do not silently migrate or reinterpret configuration. | `version = 1`, precedence, and rejection are implemented and tested. |
| Table output | Classify table output as human-facing and non-stable. Preserve safety semantics and escaping, but direct machine consumers to JSON rather than parsing headings or columns. | JSON is the machine-readable compatibility surface; table layout is explicitly non-stable. |
| JSON reports | Keep `scan-report`, `deletion-history`, and `native-path` schema v1 meanings stable. Add fields only when consumers can ignore them; bump the schema version for incompatible changes. | Published v1 schemas use strict unknown-field rejection and have regression validation. |
| Exit classes | Preserve the documented numeric classes and the rule that uncertain, partial, and interrupted results remain distinguishable from exact results. | Codes are implemented and tested. |
| Deletion | Preserve no-follow identity binding, independent enumeration, revalidation, root and synthetic-node rejection, explicit partial results, and the permanent/no-undo contract. | The deletion contract and focused safety suite are the baseline. |
| Accounting | Preserve identity-unique allocated bytes, separate apparent bytes, conservative reclaimable bounds, and explicit unknowns. Do not claim physical shared-extent exactness. | The accounting contract and fixtures are the baseline. |
| Library API | Treat the CLI, configuration, and versioned reports as the supported product surface. Rust implementation modules are private; the crate-root bridges used by the binary and tooling are hidden and carry no semver guarantee. | The private implementation boundary is implemented; the crate exposes no supported Rust API. |
| Platforms | Make only native behavioral targets fully supported in `1.0.0`; classify compile/archive-only targets as build-only until native behavior evidence promotes them. | Decision recorded: three native targets are supported; three compile-only archives remain published and explicitly best-effort. |
| Distribution and governance | Require exact protected-commit artifacts, checksums, SBOM, provenance, rollback, and an explicit release-approval authority. | Artifact identity and rollback are operational; the second-maintainer approval path is not yet established. |

### Required exit evidence

Before the `v1.0.0` release PR can be approved, attach:

1. a reviewed public-contract decision record covering every row above;
2. upgrade and compatibility tests for CLI/configuration (including rejection of unsupported versions), table safety and escaping, JSON schemas, native paths, and exit classes;
3. native behavioral evidence for every target classified as supported, plus an explicit disposition for build-only targets;
4. focused security reviews of deletion, identity, spill, terminal restoration, and release supply chain;
5. dependency, unsafe-boundary, fuzz, benchmark, packaging, SBOM, checksum, and provenance evidence from the exact release commit;
6. a clean-protected-commit release rehearsal and a documented corrective-release procedure.

An empty consumer-feedback queue is not evidence that a behavior is safe. Keep the early-testing warning until the gate has evidence, not merely until the version number changes.

## Historical tags

Tags `0.1.0` through `0.11.0` are preserved Diskonaut releases. They are not Excise releases and must not be moved, deleted, or reused. The `v0.1.1` tag is a new Excise tag; do not infer that the preserved `0.1.0` tag identifies Excise merely because the changelog contains an Excise `0.1.0` section.
