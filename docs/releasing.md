# Release process

This runbook defines the evidence and artifact shape expected for publication. It does not authorize a release by itself.

## Current distribution policy

Distribution is archive-only: the package manifest remains `publish = false`, and no package-manager or other publication channel is enabled. The `cargo-binstall` metadata and Scoop/Winget templates are validation inputs for the distribution contract, not enabled channels; passing their checks does not publish or authorize package-manager artifacts.

Any future distribution channel requires reviewed immutable metadata and release evidence tying that metadata to the exact protected commit and immutable release assets. Until that review is complete, keep `publish = false` and use only the archive candidate described below.

## Release commit

A release pull request must:

1. set one consistent semantic version in `Cargo.toml` and `Cargo.lock`;
2. move user-visible entries from `Unreleased` into a dated changelog section;
3. regenerate the man page and shell completions;
4. validate packaging templates;
5. pass the protected Linux, macOS, Windows, policy, benchmark, fuzz, and supply-chain checks; and
6. contain no unrelated source changes.

## Local candidate

```console
cargo verify
cargo dist-local
```

`cargo dist-local` builds the host release archive and supporting metadata without publishing anything.

## Hosted candidate

The manually dispatched `Release candidate artifacts` workflow builds six immutable target archives for:

- x86_64 and AArch64 Linux;
- x86_64 and Apple Silicon macOS; and
- x86_64 and AArch64 Windows.

It then produces SHA-256 checksums, an SPDX JSON dependency SBOM from the exact source checkout, and optional GitHub build provenance when `attest` is enabled. The candidate set is six immutable target archives plus checksums, SBOM, and provenance evidence when requested. The SBOM describes the locked Cargo package inventory. The checksum manifest and archive contents describe the released files. Candidate artifacts are retained for one day as short-lived validation inputs; they are not published or durable distribution assets.

## Publication requirements

Before a stable release, maintainers must verify:

- the exact protected commit passed every required check;
- the tag, manifest, changelog, archive names, checksums, SBOM, and provenance agree;
- release notes call out deletion, accounting, schema, configuration, platform, and compatibility changes;
- security advisories and yanked dependencies are clear;
- install metadata points only to immutable release assets;
- any future distribution channel has reviewed immutable metadata and release evidence before enablement; and
- rollback consists of stopping distribution and publishing a corrective version, never moving an existing tag.

## Historical tags

Tags `0.1.0` through `0.11.0` are preserved Diskonaut releases. They are not Excise releases and must not be moved or reused.
