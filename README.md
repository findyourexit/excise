<p align="center">
  <img src="assets/excise-header.png" alt="Excise, a surgical terminal storage navigator" />
</p>

# Excise

[![Native verification](https://github.com/findyourexit/excise/actions/workflows/ci.yml/badge.svg)](https://github.com/findyourexit/excise/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/findyourexit/excise)](https://github.com/findyourexit/excise/releases)
[![crates.io](https://img.shields.io/crates/v/excise.svg)](https://crates.io/crates/excise)
[![Rust 1.98+](https://img.shields.io/badge/Rust-1.98%2B-2f74c0)](rust-toolchain.toml)
[![License](https://img.shields.io/badge/license-MIT-2f855a)](LICENSE)

A tool for understanding and _surgically_ removing exactly the files and folders you choose.

Excise combines an interactive storage map with careful space accounting, clear resource limits, safe handling of unusual file names, and a deliberate review before permanent deletion.

<p align="center">
  <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/demo-main.gif" alt="Excise scanning a disposable fixture in the terminal" width="900" />
</p>

## Quick Start

### Install

<details>
<summary><strong>Homebrew (macOS)</strong></summary>

```console
brew tap findyourexit/tap
brew install findyourexit/tap/excise
excise --version  # excise 1.3.0
```

</details>

<details>
<summary><strong>crates.io</strong></summary>

```console
cargo install excise --version 1.3.0 --locked
excise --version  # excise 1.3.0
```

</details>

<details>
<summary><strong>X-CMD</strong></summary>

Install using [X-CMD](https://www.x-cmd.com/mod/eget):

```bash
x eget use findyourexit/excise
```

> [!NOTE]
> _[X-CMD](https://www.x-cmd.com/mod/eget) downloads the pre-built binary from GitHub Releases._

</details>

<details>
<summary><strong>Pre-built Binaries</strong></summary>

Download the [v1.3.0 release](https://github.com/findyourexit/excise/releases/tag/v1.3.0) for macOS, Linux, and Windows, across Apple Silicon, Intel, or ARM systems.

Full support is limited to `x86_64` Linux, `AArch64` macOS, and `x86_64` Windows binaries at present. Each of these are built and tested.

Other binaries are also available, but they're build-only, and so support is considered "best-effort".

> [!TIP]
> See the [Support Policy](SUPPORT.md) for more details.

</details>

<details>
<summary><strong>Build From Source</strong></summary>

```console
git clone --branch v1.3.0 --depth 1 https://github.com/findyourexit/excise.git
cd excise
cargo install --path . --locked
excise --version
```

[Nix](https://github.com/nixos/nix) users can run the tagged release without changing its lock file:

```console
nix run github:findyourexit/excise/v1.3.0 -- --format table /path/to/inspect
```

</details>

## Usage

### Start the Terminal Interface

Run Excise without arguments to open the interface in the current folder:

```console
excise
```

### Other Ways to Use Excise

```console
# Start the TUI from a specific directory
excise /path/to/inspect

# Readable output for humans
excise --format table /path/to/inspect

# Machine-readable JSON report for clankers
excise --format json --output scan.json /path/to/inspect

# Keep the scan on one filesystem and skip build output
excise --exclude target/ --exclude .git/ /path/to/inspect
```

### Configuration

Excise offers a degree of configuration to tailor your persisted tool preferences.

Configurations are honoured in the following order (highest priority, to lowest):

- Command line
- Environment
- Versioned TOML file
- Defaults

> [!TIP]
> See [Configuration](docs/configuration.md) for more details on configuring Excise.

## Features

<table width="100%">
  <tbody>
    <tr>
      <td width="45%" valign="top">
        <h3>Interactive Storage Map</h3>
        <p>Browse folders during an active scan. Completed folders expose concrete direct children, filtering, zoom controls, and an overflow summary when the viewport cannot draw every entry.</p>
      </td>
      <td width="55%" valign="top">
        <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/features/storage-map.gif" alt="Excise opening the media folder in the storage map" width="100%" />
      </td>
    </tr>
    <tr>
      <td width="45%" valign="top">
        <h3>Accurate Space Accounting</h3>
        <p>Show allocated space by default or apparent size with <code>--apparent-size</code>. Hard-linked files are counted once, shared allocations remain explicit, and unknown values stay unknown.</p>
      </td>
      <td width="55%" valign="top">
        <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/features/space-accounting.gif" alt="Excise browsing files with hard-linked and sparse-file accounting" width="100%" />
      </td>
    </tr>
    <tr>
      <td width="45%" valign="top">
        <h3>Scoped, Bounded Scanning</h3>
        <p>Honor exclusions and file-system boundaries, never traverse link targets, and keep worker queues, memory, session storage, reports, history, and deletion work within fixed limits.</p>
      </td>
      <td width="55%" valign="top">
        <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/features/scoped-scanning.gif" alt="Excise completing a scan with a bounded private scan store" width="100%" />
      </td>
    </tr>
    <tr>
      <td width="45%" valign="top">
        <h3>Reviewed Permanent Deletion</h3>
        <p>Build a fresh deletion plan from the live file system without following links. Confirm intent, recheck every planned entry before removal, and skip entries that no longer pass live identity and metadata checks. There is no trash or undo.</p>
      </td>
      <td width="55%" valign="top">
        <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/features/reviewed-deletion.gif" alt="Excise displaying a permanent deletion confirmation for a disposable file" width="100%" />
      </td>
    </tr>
    <tr>
      <td width="45%" valign="top">
        <h3>Responsive Background Work</h3>
        <p>Keep the map available while planning, deletion, and rescans run. Show their state, cancel pending work, or wait for an active operation to reach an entry boundary.</p>
      </td>
      <td width="55%" valign="top">
        <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/features/background-work.gif" alt="Excise returning to the map while a disposable folder deletion continues" width="100%" />
      </td>
    </tr>
    <tr>
      <td width="45%" valign="top">
        <h3>Human &amp; Machine Reports</h3>
        <p>Use readable table output or stable, versioned JSON scan and deletion reports. Table and JSON modes work without a terminal. Display paths are escaped, and JSON retains native path data without loss.</p>
      </td>
      <td width="55%" valign="top">
        <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/features/reports.gif" alt="Excise confirming that a scan report was exported" width="100%" />
      </td>
    </tr>
    <tr>
      <td width="45%" valign="top">
        <h3>Robust Terminal Sessions</h3>
        <p>Restore the terminal after normal exits, errors, panics, and safe cancellation.</p>
      </td>
      <td width="55%" valign="top">
        <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/features/terminal-sessions.gif" alt="Excise presenting an explicit safe-exit choice" width="100%" />
      </td>
    </tr>
    <tr>
      <td width="45%" valign="top">
        <h3>Accessible, Configurable Interaction</h3>
        <p>Use keyboard navigation, optional mouse selection, themes, reduced motion, ASCII and monochrome rendering, custom keymaps, and narrow-terminal layouts.</p>
      </td>
      <td width="55%" valign="top">
        <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/features/accessibility.gif" alt="Excise previewing the accessible theme picker" width="100%" />
      </td>
    </tr>
  </tbody>
</table>

## Terminal Controls

| Key                                                 | Action                                                         |
|-----------------------------------------------------|----------------------------------------------------------------|
| <kbd>←</kbd> <kbd>↓</kbd> <kbd>↑</kbd> <kbd>→</kbd> | Move the selection                                             |
| <kbd>h</kbd> <kbd>j</kbd> <kbd>k</kbd> <kbd>l</kbd> | Move the selection, Vim style                                  |
| <kbd>return</kbd> / <kbd>enter</kbd>                | Move into the selected directory                               |
| <kbd>esc</kbd>                                      | Move out of the current directory or cancel the current action |
| <kbd>/</kbd>                                        | Filter the current view                                        |
| <kbd>+</kbd>, <kbd>-</kbd>, <kbd>0</kbd>            | Zoom in, zoom out, or reset zoom                               |
| <kbd>e</kbd>                                        | Export the current scan report                                 |
| <kbd>E</kbd>                                        | Export bounded deletion history                                |
| <kbd>t</kbd>                                        | Preview and choose a theme                                     |
| <kbd>?</kbd>                                        | Open the built-in help                                         |
| <kbd>backspace</kbd>                                | Begin a permanent deletion plan                                |
| <kbd>q</kbd>, <kbd>ctrl-c</kbd>                     | Exit                                                           |

The interactive interface needs standard input and output connected to a terminal, terminal color and control support, a separate screen for the interface, and a window at least `32 x 8`. Use table or JSON mode for redirection, pipelines, continuous integration, and terminals without those capabilities. `--output FILE` works only with table or JSON mode.

## Support Policy

| Target                                      | Support Status | Details                                                   |
|---------------------------------------------|----------------|-----------------------------------------------------------|
| x86_64 Linux (`x86_64-unknown-linux-gnu`)   | ✅ Supported   | Testing on Linux, terminal testing, and release archive   |
| AArch64 macOS (`aarch64-apple-darwin`)      | ✅ Supported   | Testing on macOS, terminal testing, and release archive   |
| x86_64 Windows (`x86_64-pc-windows-msvc`)   | ✅ Supported   | Testing on Windows, terminal testing, and release archive |
| x86_64 macOS (`x86_64-apple-darwin`)        | 🟡 Best effort | Release compilation and archive only                      |
| AArch64 Linux (`aarch64-unknown-linux-gnu`) | 🟡 Best effort | Release compilation and archive only                      |
| AArch64 Windows (`aarch64-pc-windows-msvc`) | 🟡 Best effort | Release compilation and archive only                      |

Only the first three targets have full platform support. The remaining archives are published for people who want to experiment, but a successful download or build does not prove that the program runs correctly on that target.

Behavior can vary with file system types, access rules, network file systems, files that share storage with copies, compression, and shared physical storage. These cases remain best effort unless they have separate evidence. Unknown allocated space remains explicit. See [SUPPORT.md](SUPPORT.md) for limitations and troubleshooting.

## Development

Excise requires Rust `1.98` or later and uses the `2024` edition. Rust `1.98.0` is the pinned toolchain and the lowest compiler version tested in CI. Run the complete local verification gate with:

```console
cargo verify
```

This checks formatting, workflows, dependency rules, documentation links, compilation, supported builds, Rust lint checks, unit and snapshot tests, terminal behavior, package contents, limited fuzz testing, benchmarks, generated files, JSON formats, distribution templates, and release binary size.

The current main demonstration is generated with `cargo demo`. See [Development](docs/development.md) before refreshing the VHS recording. The committed `assets/demo-main.gif` is the current demonstration. The Demo recording workflow uploads a review artifact but does not change the repository.

## Documentation

- [Documentation website](https://tomlarcher.com/excise/)
- [Getting Started](docs/getting-started.md)
- [Configuration](docs/configuration.md)
- [Reports and JSON Formats](docs/reports.md)
- [Permanent Deletion Contract](docs/safety/deletion.md)
- [Space Accounting Contract](docs/safety/accounting.md)
- [Architecture](docs/architecture/overview.md)
- [Threat Model](docs/architecture/threat-model.md)
- [Development](docs/development.md)
- [Release Process](docs/releasing.md)
- [Support Policy](SUPPORT.md)
- [Security Policy](SECURITY.md)
- [Governance](GOVERNANCE.md)

## Provenance

Excise is an independent fork and spiritual successor to [Diskonaut](https://github.com/imsnif/diskonaut).

While the [Diskonaut](https://github.com/imsnif/diskonaut) history and release tags remain preserved in this repository, Excise has become quite a different beast. That said, both tools are centred around a similar tree-map visualisation of your local storage.

## Community & License

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md), use [GitHub Discussions](https://github.com/findyourexit/excise/discussions) for questions, and follow [SECURITY.md](SECURITY.md) for private vulnerability or data-loss reports.

MIT. See [LICENSE](LICENSE).
