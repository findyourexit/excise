# Contributing to Excise

Thank you for improving Excise. Contributions are welcome across Rust code, tests, documentation, packaging, accessibility, and platform support.

## Before opening work

- Search existing issues and pull requests first.
- Use the issue chooser for reproducible defects and feature proposals.
- Use [GitHub Discussions](https://github.com/findyourexit/excise/discussions) for usage questions and early ideas.
- Report security vulnerabilities and unintended-deletion risks privately according to [SECURITY.md](SECURITY.md).

For a substantial change, open an issue before implementation so scope and public behavior can be agreed without wasting work.

## Development setup

Excise uses Rust 1.88 and the 2024 edition. The pinned toolchain is declared in `rust-toolchain.toml`.

```console
cargo check --workspace --all-targets --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

`cargo verify` runs the complete local verification suite, including formatting, workflow and dependency-policy validation, link checks, cross-target checks, tests, package checks, Cargo Deny, bounded fuzzing, benchmarks, and generated-file verification. See [docs/development.md](docs/development.md) for prerequisites and focused commands.

## Engineering expectations

- Correctness and data safety come before features.
- Workspace lints deny `unsafe` by default. The audited Windows FFI boundary in `src/os/windows.rs` is the sole explicit exception; keep unsafe code confined there and document every block's safety conditions.
- Runtime behavior remains local: do not add telemetry or network access.
- Destructive-path changes must preserve identity checks, no-follow behavior, consent, revalidation, and partial-failure reporting.
- Accounting changes must distinguish apparent, allocated, shared, and reclaimable bytes.
- UI changes must remain usable with keyboard-only input, reduced motion, monochrome output, ASCII output, and narrow terminals.
- Add tests for new observable behavior and plausible regressions; avoid tests coupled only to implementation details.
- Update user-facing documentation and generated artifacts when their contracts change.

## Pull requests

Keep pull requests focused and reviewable. Include:

1. the user-visible problem;
2. the chosen behavior and important tradeoffs;
3. safety, compatibility, and accessibility impact;
4. exact verification commands and results; and
5. screenshots only when textual or snapshot evidence cannot describe a terminal-layout change.

Snapshot changes are product changes. Review them deliberately and explain meaningful differences.

## Commit and Attribution

Excise does not require a contributor license agreement or copyright assignment. Historical commits retain their original authorship. Do not rewrite another contributor's identity.

To enable the repository's Conventional Commit template in this clone:

```console
git config --local commit.template .gitmessage
```

## Conduct

Participation is governed by [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
