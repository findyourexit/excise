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

## Install 1.2.4 From A Release Channel

The `1.2.4` package is published on crates.io and can also be built locally. It is not one of the pre-built GitHub archives:

```console
cargo install excise --version 1.2.4 --locked
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

Keep the default confirmation enabled for first use. As soon as a real file or directory appears in the map, including while the initial scan continues, select it and press `Backspace` to begin a permanent deletion plan. While the initial scan is active, `Enter` opens a real or summarized directory through a focused scan; after it completes, an ordinary retained directory opens immediately and a summarized directory refreshes through the same focused scan. The scan root, a file system or drive root, and virtual `Shared` and `Other` summaries cannot be deletion targets. An incomplete map or approximate space estimate does not weaken or block the planner's live identity checks.

Files and safe printable directories confirm with `Enter` or `y`. Planning happens in a bounded background rail, and confirmation stays in front of the map until you accept or cancel it. After acceptance the map remains available while the serial worker immediately revalidates then deletes each reviewed entry. Hostile or untypeable names require a generated challenge. `--disable-delete-confirmation` enables a visible, session-only reduced confirmation mode that accepts `Enter` or `y` for all entries except hostile names.

Every planned entry is listed independently and checked again immediately before deletion. Changed, replaced, missing, or newly created entries are never silently deleted. The exit prompt lets pending plans be cancelled or awaited; an active deletion can only stop at an entry boundary or be awaited. A run can therefore be partial. There is no trash or undo.

The default view uses allocated space. Pass `--apparent-size` when logical file length is the intended comparison.

## Interactive Terminal

The interactive interface requires standard input and output connected to a terminal, terminal color and control support, a separate screen, and a window at least `32 x 8`. Smaller windows show a resize message. If a terminal cannot provide these capabilities, use table or JSON mode instead. `--ascii` changes symbols and borders but does not remove the terminal requirement.

### Current Map Behavior

The interactive view uses a dense map with static workspace frames and padded title tabs. A selected map entry has a travelling contour with bright top and left faces, dim bottom and right faces, and a slow diagonal midpoint fill wave that preserves the entry's depth. Animated modal borders use the same truecolour-only treatment. Scan, deletion, and model status appear in the header; the bottom row is dedicated to visually distinct control keys and their hints. `--ascii`, monochrome mode, high-contrast themes, and reduced motion preserve the same selection, scope, and deletion information with static output.
Before measured tiles are available, the map shows a full-surface scan field with the actual number of indexed entries rather than a guessed percentage. Its measuring front becomes a directional reveal when the first tile layout is ready.
When motion is available, its cadence is maintained independently of scanner batches so sustained scans do not interrupt the field.
Opening a directory while the initial scan is active, or opening a summarized directory at any time, enters this scan field before its staging model is prepared. The focused scan reserves a fixed one-third share of the model budget and traverses serially, so opening time cannot change its model capacity; its completed result remains authoritative over later primary-scan events for that path. If that optional capacity cannot be freed, Excise returns to the live map with a clear notice instead of ending the session. Capacity preparation yields between frames, and `Esc` can still cancel the focused refresh.

Press `t` in the normal view, while scanning, or during a focused refresh to preview the theme list. Arrow keys or `j`/`k` move the preview; `Enter` immediately saves the selected theme for later TUI sessions, while `Esc` restores the prior theme.

In a color-capable map, ordinary entry color carries the current space measure on one fixed absolute scale: 4 KiB and below are blue, 16 MiB is midpoint green, 1 GiB is yellow, and 64 GiB and above are red. The default measure is allocated space; `--apparent-size` uses logical file length instead. Unreadable entries and summarized directories retain distinct state colors; virtual summaries stay subdued and do not affect the size scale.

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
