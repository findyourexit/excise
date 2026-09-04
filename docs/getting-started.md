# Getting Started

Excise permanently deletes selected files and folders without trash or undo. Begin with a disposable directory.

## The 1.0.0 Stable Release

The `1.0.0` release defines the stable command-line tool, configuration, and JSON report formats. Read the [permanent deletion contract](safety/deletion.md) before making a scan or deletion plan.

The project is independent from Diskonaut. Tags `0.1.0` through `0.11.0` are preserved Diskonaut releases, not Excise releases. Do not move, reuse, or treat those tags as an Excise installation.

## Requirements

- Rust 1.98 or newer
- A terminal with color and separate-screen support for the interactive interface
- Linux, macOS, or Windows

The repository pins Rust 1.98 in `rust-toolchain.toml`.

For the `1.0.0` support policy, x86_64 Linux, AArch64 macOS, and x86_64 Windows have been tested on the target systems and are fully supported. x86_64 macOS, AArch64 Linux, and AArch64 Windows have release artifacts but remain build-only and best effort until they have been tested on the target systems. File system limitations are documented in [Support](../SUPPORT.md).

## Build & Run From Source

```console
git clone https://github.com/findyourexit/excise.git
cd excise
cargo build --release --locked
./target/release/excise /path/to/inspect
```

On Windows, run `target\release\excise.exe`.

To install the checked-out source into Cargo's binary directory:

```console
cargo install --path . --locked
```

Nix users can build the locked release:

```console
nix build
./result/bin/excise /path/to/inspect
```

## Install 1.2.3 From A Release Channel

The `1.2.3` package is published on crates.io and can also be built locally. It is not one of the pre-built GitHub archives:

```console
cargo install excise --version 1.2.3 --locked
excise --version
```

The first-party binary formula is published through the external [findyourexit/homebrew-tap](https://github.com/findyourexit/homebrew-tap), not Homebrew Core:

```console
brew tap findyourexit/tap https://github.com/findyourexit/homebrew-tap.git
brew install findyourexit/tap/excise
brew fetch --force --retry findyourexit/tap/excise
brew test findyourexit/tap/excise
excise --version
```

`brew fetch` checks the formula's archive URL and SHA-256. `brew test` runs the formula's smoke checks. If either check fails, stop and report the version, host platform, and formula revision. Do not bypass the checksum or substitute an unverified archive.

## Start Excise

Run `excise` without arguments to open the interactive interface in the current directory:

```console
excise
```

Keep the default deletion confirmation enabled until you understand the review flow. Never start in a home directory, a file system root, a mounted volume, or another path containing data you cannot lose.

## Start Safely

Create a disposable fixture before exploring deletion:

```console
mkdir -p /tmp/excise-demo/subdirectory
printf 'sample\n' > /tmp/excise-demo/file.txt
printf 'nested\n' > /tmp/excise-demo/subdirectory/nested.txt
excise /tmp/excise-demo
```

Use a temporary directory appropriate to your platform on Windows.

Keep the default confirmation enabled for first use. Select a real file or directory and press `Backspace` to begin a permanent deletion plan. You do not need to wait for the full scan to finish; entries that have been fully examined are already deletable while scanning continues. Review the escaped path, file identity, and number of planned entries. The scan root, a file system or drive root, synthetic `Shared` and `Other` entries, aggregate entries, and incomplete or uncertain subtrees are not deletion targets. Press `Esc` to cancel.

Files and safe printable directories confirm with `Enter` or `y`. While the identity plan is still being built, pressing `Enter` pre-arms confirmation for those entries and reduced-guardrail entries; the deletion starts as soon as the plan is ready without a separate confirm step. Hostile or untypeable names require a generated challenge. `--disable-delete-confirmation` enables a visible, session-only reduced confirmation mode that accepts `Enter` or `y` for all entries except hostile names. It does not remove the other safeguards and is not saved. Prefer an ordinary user for first use. Root and Administrator accounts receive a warning and do not change the identity checks.

Every planned entry is listed independently and checked again immediately before deletion. Changed, replaced, missing, or newly created entries are never silently deleted. A run can therefore be partial. There is no trash or undo.

The default view uses allocated space. Pass `--apparent-size` when logical file length is the intended comparison.

## Interactive Terminal

The interactive interface requires standard input and output connected to a terminal, terminal color and control support, a separate screen, and a window at least `32 x 8`. Smaller windows show a resize message. If a terminal cannot provide these capabilities, use table or JSON mode instead. `--ascii` changes symbols and borders but does not remove the terminal requirement.

### Current Map Behavior

The dense half-block map, animated focus border, heat ramp, overflow summary, and directed map transitions are included in the stable release. Build the current source or install `1.2.3` to use them.

The interactive view uses a dense map and animated focus border on capable terminals. `--ascii`, monochrome mode, and reduced motion preserve the same selection, scope, and deletion information when visual effects are unavailable or undesirable.

In a color-capable map, ordinary entry color carries size. The map compares ordinary entries in the folder currently on screen and uses a blue-to-red scale. Uncertain, shared, and aggregate entries keep their own colors and do not affect that scale.

Entries that do not fit in the final map view are collected into one `MapOverflow` summary. When there is enough room, the renderer shows that summary as a textured region with count and weight labels. When there is not enough room, the summary remains available in the report without drawing a misleading region.

Opening a folder grows its contents from the selected rectangle. Moving back contracts the departing contents into that rectangle while the parent view grows from it. The surrounding interface stays in place.

## Noninteractive Use

Table and JSON modes run without a terminal, raw mode, or separate screen. Use them for redirected output, shell pipelines, continuous integration, or terminals without interactive support:

```console
excise --format table /path/to/inspect
excise --format json --output scan.json /path/to/inspect
```

`--output FILE` writes the report instead of standard output and works only with table or JSON mode. A nonzero exit can still produce a useful bounded report. See [Reports & JSON Formats](reports.md) for outcome classes and document state.

## File System Boundaries & Exclusions

Excise stays on the starting file system by default. A directory on another file system is shown as a boundary and is not traversed. Use `--cross-filesystems` only when traversal across mounts is intended. This broadens the scan scope.

Exclusions use ordered gitignore-style patterns rooted at the scan path:

```console
excise --exclude .git/ --exclude target/ /path/to/inspect
```

An excluded entry is shown as a zero-byte scoped record with its reason. Its descendants are not traversed. Link targets are not traversed. On Windows this also applies to junction and reparse targets. Check these scope rules before treating a missing path as a read failure.

## Native Paths & Display

The interactive interface and text reports use a reversible escaped display form. Newlines, tabs, terminal controls, bidirectional controls, backslashes, invalid UTF-8 on Unix, and ill-formed UTF-16 on Windows are escaped so names cannot inject terminal controls. `display_path` is for people, not a byte-exact shell argument.

JSON keeps `path` lossless in a platform-specific encoding while `display_path` remains escaped. Unix paths use `unix-bytes` with base64 data. Windows paths use `windows-utf16-le` with base64 data. See [Reports & JSON Formats](reports.md) and the [native path format](schemas/native-path.schema.json) when consuming reports.

## Windows Notes

Run the Windows executable with Windows path syntax:

```console
target\release\excise.exe C:\path\to\inspect
```

Windows does not provide allocated-space snapshots, so allocated and reclaimable upper bounds can be unknown. Pass `--apparent-size` when logical file lengths are the intended comparison. Deletion uses verified handles, but access rules and another process's sharing mode can still make an entry fail. Close applications, sync clients, indexers, or antivirus software before a fresh scan and retry. Junction and reparse targets are not followed.

## Next Steps

- Configure persistent defaults in [Configuration](configuration.md).
- Read the [permanent deletion contract](safety/deletion.md) before destructive use.
- Open `?` in the interactive interface for context-sensitive controls.
- See [Support](../SUPPORT.md) for safe troubleshooting and bug reports. Never publish an unintended-deletion path in a public issue.
