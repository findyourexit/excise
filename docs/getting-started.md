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

The default view uses identity-unique allocated bytes. Pass `--apparent-size` when logical file length is the intended comparison.

## Noninteractive use

Table and JSON modes do not acquire raw mode or an alternate screen:

```console
excise --format table /path/to/inspect
excise --format json --output scan.json /path/to/inspect
```

A nonzero exit can still produce a useful bounded report. See [Reports and schemas](reports.md) for outcome classes.

## Filesystem boundaries and exclusions

Excise stays on the starting filesystem by default. Use `--cross-filesystems` only when traversal across mounts is intended.

Exclusions use ordered gitignore-style patterns rooted at the scan path:

```console
excise --exclude .git/ --exclude target/ /path/to/inspect
```

## Next steps

- Configure persistent defaults in [Configuration](configuration.md).
- Read the [permanent-deletion contract](safety/deletion.md) before destructive use.
- Open `?` in the TUI for context-sensitive controls.
