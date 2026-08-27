<p align="center">
  <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/demo-main.gif" alt="Excise scanning a disposable fixture in the terminal" width="900" />
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

The hero above, dense half-block map, per-folder heat ramp, overflow summary, and directed map-layout transitions described below are unreleased behavior on the current `main` branch. They are not in the published `0.1.2` packages; build current `main` from source to use them. `assets/demo-main.gif` is the current-main recording, while `assets/demo.gif` remains the `0.1.2` recording so the published `v0.1.2` README cannot display unreleased behavior.

## Why Excise

- **Honest storage accounting:** apparent and allocated bytes remain distinct; hard-linked identities are counted once; unknown data stays unknown.
- **Bounded operation:** scanner queues, worker counts, model memory, identity spill, reports, and UI history have explicit limits.
- **Race-aware deletion:** plans bind to filesystem identities, never follow links, revalidate before mutation, and never sweep newly created entries.
- **Terminal discipline:** terminal setup is RAII-owned and restored on normal exit, errors, panics, cancellation, and hard interruption.
- **Accessible terminal UI:** keyboard-first controls, narrow-terminal layouts, ASCII mode, monochrome mode, and reduced motion preserve the same information.
- **Headless reports:** deterministic table and versioned JSON output work without a TTY.
- **Size you can see:** on a colour-capable map, ordinary entries are coloured on a heat ramp fitted to the comparable entries in the folder on screen. When comparable sizes produce a distinguishable log-space range, its largest comparable entry is red and its smallest is blue before you read a single label; equal or near-equal sizes that collapse at the ramp's rendering precision rest mid-ramp. Uncertain, shared, and aggregated entries retain their semantic colours. The cursor lifts out of that band while every other entry sinks, so focus never competes with size.
- **Directed map-layout navigation:** opening an entry grows its contents out of the rectangle you chose. On drill-out, departing contents contract into the pivot while the incoming parent layout grows out of it, so a drill reads as one movement rather than a swapped screen; pane chrome and other UI do not participate in that transition.

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
| `Enter` | Open or focused rescan |
| `Esc` | Go back or cancel the current interaction |
| `/` | Filter the current view |
| `+`, `-`, `0` | Zoom in, zoom out, reset zoom |
| `e` | Export the current scan or deletion history |
| `t` | Cycle themes |
| `?` | Open in-application help |
| `Backspace` | Begin a permanent-deletion plan |
| `q`, `Ctrl-C` | Exit or interrupt safely |

On narrower adaptive footer command tiers, whenever an Enter action is shown, it retains the exact `Enter open/rescan` hint; a tier that cannot fit it omits that command rather than abbreviating it.

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

## Demo recording (current main, unreleased)

On current `main`, the unreleased `cargo demo` alias delegates to `xtask demo`; it is not part of published `0.1.2`. The hero recording is generated by [VHS](https://github.com/charmbracelet/vhs) from [`tapes/demo.tape`](tapes/demo.tape). The tape creates a fresh temporary fixture, scans only that fixture, and exits without entering deletion mode or sending a deletion confirmation. `xtask demo` requires VHS, `ttyd`, `ffmpeg`, `ffprobe`, and `gifsicle` on `PATH`, plus a Unix-like `bash` and core utilities: the tape explicitly selects `bash`, creates its fixture under `/tmp`, and invokes utilities including `head`, `mkdir`, and `rm`.

```console
(
  set -euo pipefail
  cargo +1.88.0 build --release --locked
  cargo demo  # current-main alias for xtask demo; writes assets/demo-main.gif
)
```

`xtask demo` validates the tape and renders it at the tape's 24 fps, then resamples it to 20 fps while rebuilding a 64-colour palette without dithering before lossy GIF quantisation. It owns the staging paths and only atomically promotes the size-gated result to `assets/demo-main.gif`, so a failed render leaves the current-main recording untouched and never changes `assets/demo.gif`, the published `0.1.2` recording. Dithering, not palette size, dominates the weight of a recorded terminal: it converts flat cells into per-pixel noise that no frame differ can compress. Skipping it and capping the palette at 64 colours keeps the flat interface compact without visible loss. Running `vhs tapes/demo.tape` directly writes an unoptimised 24 fps sequence to `assets/demo-main.gif`; it skips the 20 fps resampling, palette rebuild, quantisation, and size gate, so it must not be used to refresh the committed hero.

The [demo workflow](.github/workflows/demo.yml) runs the same pipeline and uploads the GIF as a build artifact on pull requests and pushes to `main`; it never commits generated media or pushes to `main`. `assets/` and `tapes/` are demo-only and excluded from release package metadata.

## Community

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md), use [GitHub Discussions](https://github.com/findyourexit/excise/discussions) for questions, and follow [SECURITY.md](SECURITY.md) for private vulnerability or data-loss reports.

## License

MIT. See [LICENSE](LICENSE). Diskonaut's original copyright and contributor history are preserved.
