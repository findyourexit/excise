# Excise

[![Native verification](https://github.com/findyourexit/excise/actions/workflows/ci.yml/badge.svg)](https://github.com/findyourexit/excise/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-2f74c0)](rust-toolchain.toml)
[![License](https://img.shields.io/badge/license-MIT-2f855a)](LICENSE)

Excise is a surgical terminal storage navigator. It combines a responsive treemap with identity-aware storage accounting, bounded scanning, lossless native paths, and guarded permanent deletion.

Excise is an independent successor to [Diskonaut](https://github.com/imsnif/diskonaut). The repository preserves Diskonaut's history and release tags while giving the project a new product, architecture, and maintenance life.

> [!WARNING]
> Excise is pre-release software. Permanent deletion has no trash or undo. Use development builds only on disposable data until a stable release is published.

## Why Excise

- **Honest storage accounting:** apparent and allocated bytes remain distinct; hard-linked identities are counted once; unknown data stays unknown.
- **Bounded operation:** scanner queues, worker counts, model memory, identity spill, reports, and UI history have explicit limits.
- **Race-aware deletion:** plans bind to filesystem identities, never follow links, revalidate before mutation, and never sweep newly created entries.
- **Terminal discipline:** terminal setup is RAII-owned and restored on normal exit, errors, panics, cancellation, and hard interruption.
- **Accessible terminal UI:** keyboard-first controls, narrow-terminal layouts, ASCII mode, monochrome mode, and reduced motion preserve the same information.
- **Headless reports:** deterministic table and versioned JSON output work without a TTY.

## Build from source

A stable release is not available yet. Build the current source with Rust 1.88 or newer:

```console
git clone https://github.com/findyourexit/excise.git
cd excise
cargo install --path . --locked
excise /path/to/inspect
```

Nix users can build the locked flake:

```console
nix build
./result/bin/excise /path/to/inspect
```

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

## Community

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md), use [GitHub Discussions](https://github.com/findyourexit/excise/discussions) for questions, and follow [SECURITY.md](SECURITY.md) for private vulnerability or data-loss reports.

## License

MIT. See [LICENSE](LICENSE). Diskonaut's original copyright and contributor history are preserved.
