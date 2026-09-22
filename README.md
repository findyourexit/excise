<p align="center">
  <img src="assets/excise-header.png" alt="Excise, a surgical terminal storage navigator" />
</p>

# Excise

[![Native verification](https://github.com/findyourexit/excise/actions/workflows/ci.yml/badge.svg)](https://github.com/findyourexit/excise/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/findyourexit/excise)](https://github.com/findyourexit/excise/releases)
[![crates.io](https://img.shields.io/crates/v/excise.svg)](https://crates.io/crates/excise)
[![Rust 1.98+](https://img.shields.io/badge/Rust-1.98%2B-2f74c0)](rust-toolchain.toml)
[![License](https://img.shields.io/badge/license-MIT-2f855a)](LICENSE)

A terminal tool for understanding and removing exactly the files and folders you choose.

Excise combines an interactive storage map with careful space accounting, clear resource limits, safe handling of unusual file names, and a deliberate review before permanent deletion.

It is an independent fork and spiritual successor to [Diskonaut](https://github.com/imsnif/diskonaut). Diskonaut history and release tags remain preserved, while Excise has its own product and release line.

> [!WARNING]
> Excise permanently deletes selected files and folders. There is no trash or undo. Use it only with data you can safely remove.

<p align="center">
  <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/demo-main.gif" alt="Excise scanning a disposable fixture in the terminal" width="900" />
</p>

## Quick Start

### Install

<details>
<summary><strong>Homebrew for macOS</strong></summary>

```console
brew tap findyourexit/tap
brew install findyourexit/tap/excise
excise --version  # excise 1.2.4
```

</details>

<details>
<summary><strong>crates.io</strong></summary>

```console
cargo install excise --version 1.2.4 --locked
excise --version  # excise 1.2.4
```

</details>

<details>
<summary><strong>X-CMD</strong></summary>

Alternatively, install it with [x-cmd](https://www.x-cmd.com/mod/eget), which downloads the pre-built binary from GitHub Releases:

```bash
x eget use findyourexit/excise
```

</details>

<details>
<summary><strong>Pre-built Binaries</strong></summary>

Download the [v1.2.4 release](https://github.com/findyourexit/excise/releases/tag/v1.2.4) for macOS, Linux, and Windows on Apple silicon, Intel, or Arm systems.

Only x86_64 Linux, AArch64 macOS, and x86_64 Windows have full platform support because they are tested on those platforms. The other archives are build-only and best effort. See the [Support Policy](SUPPORT.md).

</details>

<details>
<summary><strong>Build From Source</strong></summary>

```console
git clone --branch v1.2.4 --depth 1 https://github.com/findyourexit/excise.git
cd excise
cargo install --path . --locked
excise --version
```

Nix users can run the tagged release without changing its lock file:

```console
nix run github:findyourexit/excise/v1.2.4 -- --format table /path/to/inspect
```

</details>

### Start Excise

Open the terminal interface with the simplest command:

```console
excise
```

The default interface starts in the current folder. Keep the default deletion confirmation enabled until you understand the review flow. Never start in a home directory, a filesystem root, a mounted volume, or another path containing data you cannot lose.

## Usage

### Start the Terminal Interface

Run Excise without arguments to open the interface in the current folder:

```console
excise
```

### Other Ways to Use Excise

```console
# Readable output for people
excise --format table /path/to/inspect

# Machine-readable JSON report
excise --format json --output scan.json /path/to/inspect

# Keep the scan on one filesystem and skip build output
excise --exclude target/ --exclude .git/ /path/to/inspect
```

Configuration takes values in this order: command line, environment, versioned TOML file, and defaults. Configuration `version` must be `1`. Unknown fields and unsupported versions are rejected. See [Configuration](docs/configuration.md) for the file format and examples.

## What Excise Does

- **Careful space accounting:** Excise keeps the disk space assigned to files separate from their file length. It counts files with more than one name once and keeps unknown values unknown.
- **Clear limits:** Scan queues, worker counts, memory use, per-session temporary storage, reports, interface history, and the four-slot interactive deletion rail have explicit limits.
- **Safe review before deletion:** Deletion plans record the files and folders that were reviewed. Excise does not follow links, checks for changes before deletion, and never includes new entries silently.
- **Reliable terminal behavior:** The terminal is restored after normal exit, errors, panics, and boundary-safe cancellation; active filesystem work is never detached silently.
- **Accessible interaction:** Keyboard controls, narrow layouts, plain ASCII output, monochrome output, and reduced motion preserve the important safety information.
- **Useful reports:** Table output is intended for people to read. JSON output uses stable, versioned formats for scan results, deletion history, and file paths.
- **Readable maps:** The interface uses allocated space by default. Ordinary entries use a fixed absolute size scale, not their rank in the visible folder: 4 KiB and below are blue, 16 MiB is midpoint green, 1 GiB is yellow, and 64 GiB and above are red. `--apparent-size` applies the same scale to logical file length. Uncertain, shared, and summary entries keep their own meaning. Entries that do not fit remain visible as an overflow summary instead of making a folder look empty.

## Terminal Controls

| Key | Action |
|---|---|
| Arrow keys | Move the selection |
| `h j k l` | Use the Vim movement preset |
| `Enter` | Open the selected folder from its current canonical page; while scanning, prioritize its existing work |
| `Esc` | Go back or cancel the current action |
| `/` | Filter the current view |
| `+`, `-`, `0` | Zoom in, zoom out, or reset zoom |
| `e` | Export the current scan report |
| `E` | Export bounded deletion history |
| `t` | Preview and choose a theme |
| `?` | Open the built-in help |
| `Backspace` | Begin a permanent deletion plan |
| `q`, `Ctrl-C` | Exit safely; pending and active work have explicit choices |

The interactive interface needs standard input and output connected to a terminal, terminal color and control support, a separate screen for the interface, and a window at least `32 x 8`. Use table or JSON mode for redirection, pipelines, continuous integration, and terminals without those capabilities. `--output FILE` works only with table or JSON mode.

## Safety Model

Excise offers deletion as soon as a real item appears in the map, including while the initial scan continues, on a platform with tested deletion support. A backed summarized folder can be opened with an on-demand scan or planned for deletion immediately; incomplete real entries use the same flow. Virtual summaries and filesystem roots remain noninteractive. The background planner independently makes the authoritative no-follow live review, binds it to the selected identity, and checks every planned entry again immediately before deletion.

Changed, replaced, missing, newly created, permission-blocked, and uncertain entries are never silently deleted. Accepted plans return to the map while a bounded named work rail shows planning, queueing, and deletion progress; one executor mutates entries serially. Quitting can cancel pending plans or wait, and an active mutation can only stop at an entry boundary or be awaited. There is no recovery or undo mechanism.

Read the [permanent deletion contract](docs/safety/deletion.md), [space accounting contract](docs/safety/accounting.md), and [threat model](docs/architecture/threat-model.md) before relying on destructive behavior.

## Support Policy

| Target | v1 stable status | Evidence |
|---|---|---|
| x86_64 Linux (`x86_64-unknown-linux-gnu`) | Supported | Testing on Linux, terminal testing, and release archive |
| AArch64 macOS (`aarch64-apple-darwin`) | Supported | Testing on macOS, terminal testing, and release archive |
| x86_64 Windows (`x86_64-pc-windows-msvc`) | Supported | Testing on Windows, terminal testing, and release archive |
| x86_64 macOS (`x86_64-apple-darwin`) | Build-only and best effort | Release compilation and archive only |
| AArch64 Linux (`aarch64-unknown-linux-gnu`) | Build-only and best effort | Release compilation and archive only |
| AArch64 Windows (`aarch64-pc-windows-msvc`) | Build-only and best effort | Release compilation and archive only |

Only the first three targets have full platform support. The remaining archives are published for people who want to experiment, but a successful download or build does not prove that the program runs correctly on that target.

Behavior can vary with file system types, access rules, network file systems, files that share storage with copies, compression, and shared physical storage. These cases remain best effort unless they have separate evidence. Unknown allocated space remains explicit. See [SUPPORT.md](SUPPORT.md) for limitations and troubleshooting.

## Documentation

- [Getting Started](docs/getting-started.md)
- [Configuration](docs/configuration.md)
- [Reports and JSON Formats](docs/reports.md)
- [Permanent Deletion Contract](docs/safety/deletion.md)
- [Space Accounting Contract](docs/safety/accounting.md)
- [Architecture and Threat Model](docs/architecture/overview.md)
- [Development](docs/development.md)
- [Release Process](docs/releasing.md)
- [Support Policy](SUPPORT.md)
- [Security Policy](SECURITY.md)
- [Governance](GOVERNANCE.md)

## Development

Excise requires Rust 1.98 or later and uses the 2024 edition. Rust 1.98.0 is the pinned toolchain and the lowest compiler version tested in CI. Run the complete local verification gate with:

```console
cargo verify
```

This checks formatting, workflows, dependency rules, documentation links, compilation, supported builds, Rust lint checks, unit and snapshot tests, terminal behavior, package contents, limited fuzz testing, benchmarks, generated files, JSON formats, distribution templates, and release binary size.

The current main demonstration is generated with `cargo demo`. See [Development](docs/development.md) before refreshing the VHS recording. The committed `assets/demo-main.gif` is the current demonstration, while `assets/demo.gif` remains the historical `0.1.2` recording.

## Community & License

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md), use [GitHub Discussions](https://github.com/findyourexit/excise/discussions) for questions, and follow [SECURITY.md](SECURITY.md) for private vulnerability or data-loss reports.

MIT. See [LICENSE](LICENSE).
