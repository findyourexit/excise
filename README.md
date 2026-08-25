<p align="center">
  <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/demo.gif" alt="Excise scanning a disposable fixture in the terminal" width="900" />
</p>

# Excise

[![Native verification](https://github.com/findyourexit/excise/actions/workflows/ci.yml/badge.svg)](https://github.com/findyourexit/excise/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/findyourexit/excise)](https://github.com/findyourexit/excise/releases)
[![crates.io](https://img.shields.io/crates/v/excise.svg)](https://crates.io/crates/excise)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-2f74c0)](rust-toolchain.toml)
[![License](https://img.shields.io/badge/license-MIT-2f855a)](LICENSE)

Excise is a surgical terminal storage navigator. It combines a responsive treemap with identity-aware storage accounting, bounded scanning, lossless native paths, and guarded permanent deletion.

Excise is an independent successor to [Diskonaut](https://github.com/imsnif/diskonaut). The repository preserves Diskonaut's history and release tags while giving the project a new product, architecture, and maintenance life.

> [!WARNING]
> Excise permanently deletes selected filesystem entries. There is no trash or undo. Use it only on data you can safely remove.

> [!IMPORTANT]
The published Excise **0.1.2** release is a corrective early-testing release. Start with a disposable directory, keep the default confirmation enabled, and read the [deletion contract](docs/safety/deletion.md) before selecting anything you might need.

## Why Excise

- **Honest storage accounting:** apparent and allocated bytes remain distinct; hard-linked identities are counted once; unknown data stays unknown.
- **Bounded operation:** scanner queues, worker counts, model memory, identity spill, reports, and UI history have explicit limits.
- **Race-aware deletion:** plans bind to filesystem identities, never follow links, revalidate before mutation, and never sweep newly created entries.
- **Terminal discipline:** terminal setup is RAII-owned and restored on normal exit, errors, panics, cancellation, and hard interruption.
- **Accessible terminal UI:** keyboard-first controls, narrow-terminal layouts, ASCII mode, monochrome mode, and reduced motion preserve the same information.
- **Headless reports:** deterministic table and versioned JSON output work without a TTY.

## Quick Start

All release-channel commands below target the published **0.1.2** corrective early-testing release. Before using them, read the [release process](docs/releasing.md) and follow the disposable-fixture guidance.

### Install it

<details>
<summary><strong>Homebrew (macOS)</strong></summary>

The first-party Homebrew tap carries the formula:

```console
brew tap findyourexit/tap
brew install findyourexit/tap/excise
excise --version  # excise 0.1.2
```

</details>

<details>
<summary><strong>crates.io</strong></summary>

The `0.1.2` package is published on crates.io; install it with the locked dependency graph:

```console
cargo install excise --version 0.1.2 --locked
excise --version  # excise 0.1.2
```

</details>

<details>
<summary><strong>GitHub Release</strong></summary>

Download the archive for your platform from the [published 0.1.2 GitHub Release](https://github.com/findyourexit/excise/releases/tag/v0.1.2). For example, Apple silicon macOS:
The provenance check requires the GitHub CLI (`gh`) and a GitHub API-authenticated session.

```console
(
  set -euo pipefail
  download_dir="$(mktemp -d "${TMPDIR:-/tmp}/excise-download.XXXXXX")"
  readonly download_dir
  trap 'rm -rf -- "$download_dir"' EXIT
  cd "$download_dir"
  curl --fail --location --remote-name https://github.com/findyourexit/excise/releases/download/v0.1.2/excise-aarch64-apple-darwin-v0.1.2.tar.gz
  curl --fail --location --remote-name https://github.com/findyourexit/excise/releases/download/v0.1.2/checksums.sha256
  shasum -a 256 --ignore-missing --check checksums.sha256
  source_sha="$(git ls-remote --exit-code https://github.com/findyourexit/excise.git 'refs/tags/v0.1.2^{}' | cut -f1)"
  if [[ ! "$source_sha" =~ ^[0-9a-f]{40}$ ]]; then
    echo "could not resolve the v0.1.2 tag commit" >&2
    exit 1
  fi
  gh attestation verify excise-aarch64-apple-darwin-v0.1.2.tar.gz \
    --repo findyourexit/excise \
    --signer-workflow findyourexit/excise/.github/workflows/release.yml \
    --source-digest "$source_sha" \
    --source-ref refs/heads/main
  tar --extract --gzip --file excise-aarch64-apple-darwin-v0.1.2.tar.gz
  ./excise-aarch64-apple-darwin-v0.1.2/excise --version  # excise 0.1.2
)
```

The release provides archives for AArch64 and x86_64 macOS, AArch64 and x86_64 Linux, and x86_64 and AArch64 Windows. The example verifies the selected archive before extraction or execution; verify the remaining release assets before placing a binary on your `PATH`.

</details>

<details>
<summary><strong>Build from source</strong></summary>

To build the current protected `main` source:

```console
git clone --branch main --depth 1 https://github.com/findyourexit/excise.git
cd excise
cargo install --path . --locked
excise --version
```

To reproduce the exact published release commit, replace `main` with `v0.1.2` in the clone command.

</details>

<details>
<summary><strong>Nix</strong></summary>

Run or install the published `v0.1.2` flake without updating its lock file:

```console
nix run github:findyourexit/excise/v0.1.2 -- --format table /path/to/inspect
nix profile install github:findyourexit/excise/v0.1.2
excise --version  # excise 0.1.2
```

</details>

### Run a safe first scan

This fixture is created under a platform temporary directory and is the only path passed to Excise. The subshell keeps the cleanup trap local and the readonly variable bound to the directory created by `mktemp`:

```console
(
  set -euo pipefail
  fixture="$(mktemp -d "${TMPDIR:-/tmp}/excise-quick-start.XXXXXX")"
  readonly fixture
  trap 'rm -rf -- "$fixture"' EXIT
  printf 'sample\n' > "$fixture/file.txt"
  printf 'nested sample\n' > "$fixture/nested.txt"
  excise --format table "$fixture"
)
```

For an interactive scan, replace `--format table` with the default TUI invocation. On Windows, use a disposable directory created with the platform's temporary-directory tools. Never substitute a home directory, filesystem root, mounted volume, or another path containing data you cannot lose.

See [Getting started](docs/getting-started.md) for platform notes and safer first-use guidance.

## Usage

```console
# Interactive terminal UI
excise /path/to/inspect

# Noninteractive summary
excise --format table /path/to/inspect

# Versioned machine-readable report
excise --format json --output scan.json /path/to/inspect

# Stay on the starting filesystem and exclude build output
excise --exclude target/ --exclude .git/ /path/to/inspect
```

Configuration precedence is command line, environment, versioned TOML, then defaults. Run `excise --help` for the complete option list and see [Configuration](docs/configuration.md) for examples.

### Core controls

| Control | Action |
|---|---|
| Arrow keys | Move selection |
| `h j k l` | Vim movement preset |
| `Enter` | Open or materialize selection |
| `Esc` | Go back or cancel the current interaction |
| `/` | Filter the current view |
| `+`, `-`, `0` | Zoom in, zoom out, reset zoom |
| `e` | Export the current scan or deletion history |
| `t` | Cycle themes |
| `?` | Open in-application help |
| `Backspace` | Begin a permanent-deletion plan |
| `q`, `Ctrl-C` | Exit or interrupt safely |

## Safety model

Deletion is available only for complete, materialized, real entries. Excise rejects roots and synthetic aggregate nodes, independently enumerates the reviewed subtree, compares the live identity set with the scan model, and revalidates each entry immediately before mutation. Changed, replaced, missing, or newly created entries are not silently deleted.

Read the [deletion contract](docs/safety/deletion.md), [storage accounting contract](docs/safety/accounting.md), and [threat model](docs/architecture/threat-model.md) before relying on destructive behavior.

## Documentation

- [Getting started](docs/getting-started.md)
- [Configuration](docs/configuration.md)
- [Reports and schemas](docs/reports.md)
- [Architecture](docs/architecture/overview.md)
- [Development](docs/development.md)
- [Release process](docs/releasing.md)
- [Project lineage](docs/lineage.md)

## Demo recording

The hero recording is generated by [VHS](https://github.com/charmbracelet/vhs) from [`tapes/demo.tape`](tapes/demo.tape). The tape creates a fresh temporary fixture, scans only that fixture, and exits without entering deletion mode or sending a deletion confirmation. It requires VHS, `ttyd`, and `ffmpeg` on `PATH`:

```console
(
  set -euo pipefail
  cargo +1.88.0 build --release --locked
  vhs validate tapes/demo.tape
  vhs tapes/demo.tape  # writes assets/demo.gif
)
```

The [demo workflow](.github/workflows/demo.yml) renders and uploads the GIF as a build artifact on pull requests and pushes to `main`; it never commits generated media or pushes to `main`. `assets/` and `tapes/` are demo-only and excluded from release package metadata.

## Community

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md), use [GitHub Discussions](https://github.com/findyourexit/excise/discussions) for questions, and follow [SECURITY.md](SECURITY.md) for private vulnerability or data-loss reports.

## License

MIT. See [LICENSE](LICENSE). Diskonaut's original copyright and contributor history are preserved.
