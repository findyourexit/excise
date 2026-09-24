# Getting Started

!!! danger "Begin with disposable data"

    Excise permanently deletes selected files and folders without trash or undo. Use a temporary fixture for first use, keep the default confirmation enabled, and never begin in a home directory, a filesystem root, a mounted volume, or another path containing data you cannot lose.

## Stable v1 Contract

The stable v1 line defines Excise’s command-line tool, configuration, and versioned JSON report formats. Read the [Permanent Deletion Contract](safety/deletion.md) before making a scan or deletion plan.

!!! note "Historical Diskonaut tags"

    Excise is independent from Diskonaut. Tags `0.1.0` through `0.11.0` are preserved Diskonaut releases, not Excise releases. Do not move, reuse, or treat those tags as an Excise installation.

## Requirements and Support

- Rust 1.98 or newer when building from source.
- A terminal with color and separate-screen support for the interactive interface.
- Linux, macOS, or Windows.

The repository pins Rust 1.98 in `rust-toolchain.toml`. Release binaries do not require a local Rust installation.

| Target class | Platforms |
|---|---|
| **Supported** | x86_64 Linux, AArch64 macOS, and x86_64 Windows; tested on target systems |
| **Build-only, best effort** | x86_64 macOS, AArch64 Linux, and AArch64 Windows; release artifacts exist without native runtime evidence |

File-system limitations are documented in [Support](https://github.com/findyourexit/excise/blob/main/SUPPORT.md).

## Install Excise

=== "Build from source"

    ```console
    git clone https://github.com/findyourexit/excise.git
    cd excise
    cargo build --release --locked
    ./target/release/excise /path/to/inspect
    ```

    On Windows, run `target\release\excise.exe`. To install the checked-out source into Cargo’s binary directory:

    ```console
    cargo install --path . --locked
    ```

=== "crates.io 1.3.0"

    ```console
    cargo install excise --version 1.3.0 --locked
    excise --version
    ```

=== "Homebrew"

    ```console
    brew tap findyourexit/tap https://github.com/findyourexit/homebrew-tap.git
    brew install findyourexit/tap/excise
    brew fetch --force --retry findyourexit/tap/excise
    brew test findyourexit/tap/excise
    excise --version
    ```

    The first-party binary formula is published through the external [findyourexit/homebrew-tap](https://github.com/findyourexit/homebrew-tap), not Homebrew Core. `brew fetch` checks the formula’s archive URL and SHA-256; `brew test` runs its smoke checks. If either fails, stop and report the version, host platform, and formula revision. Do not bypass the checksum or substitute an unverified archive.

=== "Nix"

    ```console
    nix build
    ./result/bin/excise /path/to/inspect
    ```

## First Safe Session

Create a disposable fixture, then inspect it:

```console
mkdir -p /tmp/excise-demo/subdirectory
printf 'sample\n' > /tmp/excise-demo/file.txt
printf 'nested\n' > /tmp/excise-demo/subdirectory/nested.txt
excise /tmp/excise-demo
```

Use a temporary directory appropriate to your platform on Windows.

- [ ] Confirm that the selected scan root is disposable.
- [ ] Keep deletion confirmation enabled.
- [ ] Open a stored folder page with ++enter++ before attempting deletion.
- [ ] Review the escaped target name and identity before accepting a plan.
- [ ] Treat a partial result as a signal to rescan before acting again.

When a real file or directory appears in the map, select it and press ++backspace++ to begin a permanent deletion plan. During the first scan, ++enter++ opens a directory that already has a partial stored page and prioritizes its existing scan work. Once a complete result publishes, opening a directory reads a fixed page of direct contents immediately.

The scan root, a filesystem or drive root, and the virtual `Shared` allocation summary cannot be deletion targets. An incomplete map or approximate space estimate neither weakens nor blocks the planner’s separate live identity checks.

!!! warning "A plan is not a blanket authorization"

    Every planned entry is listed independently and checked again immediately before deletion. Changed, replaced, missing, or newly created entries are never silently deleted. Pending plans can be cancelled or awaited; an active deletion can stop only at an entry boundary or be awaited. A run can therefore be partial.

Files and directories with safe printable names accept ++enter++ or ++y++. Planning runs in the background with a fixed limit while confirmation stays in front of the map until you accept or cancel it. After acceptance, the map remains available while one worker rechecks and deletes each reviewed entry. Hostile or untypeable names require a generated challenge. `--disable-delete-confirmation` enables a visible, session-only reduced confirmation mode that accepts ++enter++ or ++y++ for all entries except hostile names.

The default view uses allocated space. Pass `--apparent-size` when logical file length is the intended comparison.

## Interactive Terminal

The interactive interface requires standard input and output connected to a terminal, terminal color and control support, a separate screen, and a window at least `32 x 8`. Smaller windows show a resize message. If a terminal cannot provide those capabilities, use table or JSON mode instead. `--ascii` changes symbols and borders but does not remove the terminal requirement.

???+ info "Current map behavior"

    The interactive view uses a dense map inside fixed workspace panes with padded title tabs. A selected entry has a three-dimensional outline with bright top and left edges, darker bottom and right edges, and a slow diagonal fill. Animated modal borders use the same treatment only when full color is available. The header shows scan, deletion, and model status plus active or queued background work. The bottom row pairs each key with its hint. `--ascii`, monochrome mode, high-contrast themes, and reduced motion keep the same selection, scope, and deletion information in a static form.

    Before tiles are ready, the map shows a full-map scan field with the actual number of indexed entries instead of a guessed percentage. The field becomes a directional reveal when the first layout is ready. When animation is available, its timing stays independent of scan batches, so an active scan does not interrupt it.

    Opening or leaving a directory never starts another scanner or creates a second mutable model. Before a scan publishes its result, navigation reads the available partial stored pages and only reprioritizes existing work. A partial deletion can start one root rescan, which appears in the scan field until it publishes or is cancelled. Press ++esc++ to cancel that rescan and return to normal navigation.

    Press ++t++ in the normal view, while scanning, or during a root rescan to preview the theme list. Arrow keys or ++j++ and ++k++ move the preview. Press ++enter++ to save the selected theme for later TUI sessions, or ++esc++ to restore the prior theme.

    In a color-capable map, ordinary entry color carries the current space measure on one fixed absolute scale: 4 KiB and below is blue, 16 MiB is midpoint green, 1 GiB is yellow, and 64 GiB and above is red. The default measure is allocated space; `--apparent-size` uses logical file length instead. Unreadable entries retain distinct state colors. The virtual shared-allocation summary stays subdued and does not affect the size scale.

    Entries that do not fit in the final map view are collected into one `MapOverflow` summary. When there is room, the renderer shows that summary as a textured region with count and weight labels. When there is not, the summary remains available in the report without drawing a misleading region.

    Opening a folder grows its contents from the selected rectangle. Moving back contracts the departing contents into that rectangle while the parent view grows from it. The surrounding interface stays in place.

## Noninteractive Use

=== "Table output"

    ```console
    excise --format table /path/to/inspect
    ```

=== "JSON output"

    ```console
    excise --format json --output scan.json /path/to/inspect
    ```

Table and JSON modes run without a terminal, raw mode, or separate screen. Use them for redirected output, shell pipelines, continuous integration, or terminals without interactive support. `--output FILE` writes the report instead of standard output and works only with table or JSON mode. A nonzero exit can still produce a useful bounded report. See [Reports & JSON Formats](reports.md) for outcome classes and document state.

## Scope: Boundaries and Exclusions

Excise stays on the starting file system by default. A directory on another file system is shown as a boundary and is not traversed. Use `--cross-filesystems` only when traversal across mounts is intended; it broadens the scan scope.

```console
excise --exclude .git/ --exclude target/ /path/to/inspect
```

Exclusions use ordered gitignore-style patterns rooted at the scan path. An excluded entry is shown as a zero-byte scoped record with its reason, and its descendants are not traversed. Link targets are never traversed. On Windows this also applies to junction and reparse targets. Check these scope rules before treating a missing path as a read failure.

## Native Paths and Display

Excise uses a reversible escaped display form for interactive and text output. Newlines, tabs, terminal controls, bidirectional controls, backslashes, invalid UTF-8 on Unix, and ill-formed UTF-16 on Windows are escaped so a name cannot inject terminal control. `display_path` is for people, not a byte-exact shell argument.

JSON keeps `path` lossless in a platform-specific encoding while `display_path` remains escaped:

| Platform | `path` encoding |
|---|---|
| Unix | `unix-bytes` with base64 data |
| Windows | `windows-utf16-le` with base64 data |

See [Reports & JSON Formats](reports.md) and the [native path format](schemas/native-path.schema.json) when consuming reports.

## Windows Notes

```console
# Windows path syntax
target\release\excise.exe C:\path\to\inspect
```

Windows does not provide allocated-space snapshots, so allocated and reclaimable upper bounds can be unknown. Pass `--apparent-size` when logical file lengths are the intended comparison. Deletion uses verified handles, but access rules and another process’s sharing mode can still make an entry fail. Close applications, sync clients, indexers, or antivirus software before a fresh scan and retry. Junction and reparse targets are not followed.

## Next Steps

- [Configure persistent defaults](configuration.md).
- Read the [Permanent Deletion Contract](safety/deletion.md) before destructive use.
- Open `?` in the interactive interface for context-sensitive controls.
- See [Support](https://github.com/findyourexit/excise/blob/main/SUPPORT.md) for safe troubleshooting and bug reports. Never publish an unintended-deletion path in a public issue.
