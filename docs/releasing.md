# Release process

Excise has not published a stable release. This runbook defines the evidence and artifact shape expected before publication; it does not authorize a release by itself.

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

The manually dispatched `Release candidate artifacts` workflow builds immutable archives for:

- x86_64 and AArch64 Linux;
- x86_64 and Apple Silicon macOS; and
- x86_64 and AArch64 Windows.

It then produces SHA-256 checksums, an SPDX JSON dependency SBOM from the exact source checkout, and optional GitHub build provenance. The SBOM describes the locked Cargo package inventory. The checksum manifest and archive contents describe the released files. Artifacts are short-lived validation inputs until an explicitly reviewed publishing workflow is introduced.

## Publication requirements

Before a stable release, maintainers must verify:

- the exact protected commit passed every required check;
- the tag, manifest, changelog, archive names, checksums, SBOM, and provenance agree;
- release notes call out deletion, accounting, schema, configuration, platform, and compatibility changes;
- security advisories and yanked dependencies are clear;
- install metadata points only to immutable release assets; and
- rollback consists of stopping distribution and publishing a corrective version, never moving an existing tag.

## Historical tags

Tags `0.1.0` through `0.11.0` are preserved Diskonaut releases. They are not Excise releases and must not be moved or reused.
