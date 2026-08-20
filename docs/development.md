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

A host may not be able to install every target. Native behavior is exercised by GitHub Actions on Linux, macOS, and Windows.

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
