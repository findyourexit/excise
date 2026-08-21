# Getting started

Excise permanently deletes selected filesystem entries. There is no trash or undo. Begin with a disposable directory.

## Requirements

- Rust 1.88 or newer
- a terminal with ANSI and alternate-screen support for the interactive UI
- Linux, macOS, or Windows

The repository pins Rust 1.88 in `rust-toolchain.toml`.

## Build and run

```console
git clone https://github.com/findyourexit/excise.git
cd excise
cargo build --release --locked
./target/release/excise /path/to/inspect
```

On Windows, run `target\release\excise.exe`.

To install into Cargo's binary directory:

```console
cargo install --path . --locked
```

Nix users can build the locked flake:

```console
nix build
./result/bin/excise /path/to/inspect
```

## Start safely

Create a disposable fixture before exploring deletion:

```console
mkdir -p /tmp/excise-demo/subdirectory
printf 'sample\n' > /tmp/excise-demo/file.txt
printf 'nested\n' > /tmp/excise-demo/subdirectory/nested.txt
excise /tmp/excise-demo
```

Use a temporary directory appropriate to your platform on Windows.

Keep the default confirmation enabled for first use. Wait for a complete scan, select a real file or directory, press `Backspace`, and review the escaped path, identity, and planned-entry count. The scan root, a filesystem or drive root, synthetic `Shared`, `Other`, or aggregate nodes, and incomplete or uncertain subtrees are not deletion targets. Press `Esc` to cancel.

The default confirmation is explicit: files require `y`; safe printable directories require their exact leaf name; hostile or untypeable names require a generated challenge. `--disable-delete-confirmation` only enables a visible, session-only reduced confirmation mode; it does not remove the other guardrails and is not persisted. Prefer an ordinary user for first use; root/Administrator is warned and does not change identity checks.

Every planned entry is independently enumerated and revalidated immediately before mutation. Changed, replaced, missing, or newly created entries are never silently deleted, so a run can be partial. There is no trash or undo.

The default view uses identity-unique allocated bytes. Pass `--apparent-size` when logical file length is the intended comparison.

## Interactive terminal

The TUI requires stdin and stdout attached to TTYs, ANSI rendering, and alternate-screen support. It needs a window at least `32 x 8`; smaller windows show a resize message. If a terminal cannot provide these capabilities, use table or JSON mode instead. `--ascii` changes symbols and borders but does not remove the TTY requirement.

## Noninteractive use

Table and JSON modes run without a TTY, raw mode, or alternate screen, so use them for redirected output, shell pipelines, CI, or terminals without TUI capabilities:

```console
excise --format table /path/to/inspect
excise --format json --output scan.json /path/to/inspect
```

`--output FILE` writes the report instead of stdout and is valid only with table or JSON mode. A nonzero exit can still produce a useful bounded report. See [Reports and schemas](reports.md) for outcome classes and document state.

## Filesystem boundaries and exclusions

Excise stays on the starting filesystem by default. A directory on another filesystem is shown as a boundary and not traversed. Use `--cross-filesystems` only when traversal across mounts is intended; this broadens the scan scope.

Exclusions use ordered gitignore-style patterns rooted at the scan path:

```console
excise --exclude .git/ --exclude target/ /path/to/inspect
```

An excluded entry is shown as a zero-byte scoped record with its reason; its descendants are not traversed. Symlink targets are not traversed; on Windows this also applies to junction and reparse targets. Check these scope rules before treating a missing path as a read failure.

## Native paths and display

TUI and text reports use a reversible escaped display form. Newlines, tabs, terminal controls, bidirectional controls, backslashes, invalid UTF-8 on Unix, and ill-formed UTF-16 on Windows are escaped so names cannot inject terminal controls. `display_path` is for people, not a byte-exact shell argument.

JSON keeps `path` losslessly in a native encoding while `display_path` remains escaped. Unix paths use `unix-bytes` base64; Windows paths use `windows-utf16-le` base64. See [Reports and schemas](reports.md) and the [native-path schema](schemas/native-path.schema.json) when consuming reports.

## Windows notes

Run the Windows executable with Windows path syntax:

```console
target\release\excise.exe C:\path\to\inspect
```

Windows does not populate allocated-byte snapshots, so allocated and reclaimable upper bounds can be unknown; pass `--apparent-size` when logical file lengths are the intended comparison. Deletion uses no-follow handles, but ACLs and another process's sharing mode can still make an entry fail. Close applications, sync clients, indexers, or antivirus software before a fresh scan and retry. Junction and reparse targets are not followed.

## Next steps

- Configure persistent defaults in [Configuration](configuration.md).
- Read the [permanent-deletion contract](safety/deletion.md) before destructive use.
- Open `?` in the TUI for context-sensitive controls.
