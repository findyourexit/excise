# Support

## The 1.0.0 Stable Release

The `1.0.0` release defines the stable command-line tool, configuration, and versioned JSON report formats. Deletion remains permanent with no trash or undo. Start with [Getting Started](docs/getting-started.md), use a disposable fixture first, and report safety or release-integrity defects privately under [SECURITY.md](SECURITY.md).

The `0.3.x` releases were early testing and are superseded. They remain in the changelog and release history as historical records.

## The 1.0.0 Support Matrix

The stable platform support set is:

| Target | `1.0.0` status | Evidence |
|---|---|---|
| x86_64 Linux (`x86_64-unknown-linux-gnu`) | Supported | Testing on Linux, terminal testing, and release archive |
| AArch64 macOS (`aarch64-apple-darwin`) | Supported | Testing on macOS, terminal testing, and release archive |
| x86_64 Windows (`x86_64-pc-windows-msvc`) | Supported | Testing on Windows, terminal testing, and release archive |
| x86_64 macOS (`x86_64-apple-darwin`) | Build-only and best effort | Release compilation and archive only |
| AArch64 Linux (`aarch64-unknown-linux-gnu`) | Build-only and best effort | Release compilation and archive only |
| AArch64 Windows (`aarch64-pc-windows-msvc`) | Build-only and best effort | Release compilation and archive only |

The three build-only archives remain available for experimentation. A successful download or compilation does not prove that the program runs correctly on that target.

Behavior can vary with file system types, access rules, network file systems, files that share storage with copies, compression, and shared physical storage. These cases remain best effort unless they have separate evidence. Unknown allocated space remains explicit. Interactive use requires a terminal with color and separate-screen support. Use table or JSON mode without a terminal.

## Usage Questions

Search the [Documentation](docs/README.md) and existing discussions first. Use [GitHub Discussions](https://github.com/findyourexit/excise/discussions) for installation, configuration, and usage questions.

## Troubleshooting

### Permission Or Sharing Failures

A scan permission error means Excise could not read file information or list part of the tree. The report is uncertain rather than a complete inventory. Check that the account has the needed access to the path and its parent directories. Correct the access rules or mount access, then scan again.

A deletion `failed` result means Excise did not confirm that the entry was deleted. On Unix, check write and execute permission on the parent directory. On Windows, close applications, sync clients, indexers, or antivirus software that may hold an incompatible sharing handle. Start a new scan. Do not infer the file system state from the error alone.

If deletion was forced to stop, its result is imprecise. Scan again before acting. Preserve the exit code and deletion-history report when asking for help. Never continue an old deletion plan after a failed, cancelled, changed, or uncertain run.

### Boundaries & Exclusions

If paths are missing, check the default one-file-system boundary and configured exclusions before treating them as read failures. `--cross-filesystems` permits traversal across mounts and should be used only when that scope is intended. `--exclude PATTERN` uses ordered gitignore-style patterns rooted at the scan path.

Excluded entries and foreign-file-system boundaries remain visible as zero-byte records with their reason. Their descendants are not traversed. Link targets are not followed. On Windows this also includes junction and reparse targets.

### Terminal & Noninteractive Output

The interactive interface needs standard input and output connected to a terminal, terminal color and control support, a separate screen, and a window at least `32 x 8`. Resize a smaller terminal or use table or JSON mode instead. Table and JSON modes work without a terminal, raw input mode, or a separate screen. They are suitable for redirection, shell pipelines, and continuous integration. `--output FILE` works only with table or JSON mode.

### File Names & Windows Limitations

Text output uses escaped file names. Terminal and bidirectional controls, newlines, tabs, backslashes, invalid Unix path bytes, and invalid Windows UTF-16 are escaped. In JSON, `path` keeps the original platform data in base64 and `display_path` remains presentation text. See [Reports & JSON Formats](docs/reports.md).

Windows does not provide allocated-space snapshots, so allocated and reclaimable upper bounds can be unknown. Pass `--apparent-size` when logical file lengths are the intended comparison. Windows deletion uses verified handles, but access rules and sharing modes can still reject an entry. See [Getting Started](docs/getting-started.md) for first-use and platform guidance.

## Defects & Bug Reports

Use the [issue chooser](https://github.com/findyourexit/excise/issues/new/choose) for a reproducible, non-sensitive defect. Include the Excise version or commit, operating system, terminal and shell, command with real paths replaced, file system and mount context, permissions, links or junctions, configuration, terminal dimensions, expected behavior, actual behavior, and whether deletion occurred. Preserve the exit code and relevant table, JSON, or deletion-history excerpts after removing paths, usernames, and other private data. Reproduce only on a synthetic disposable fixture when it is safe to do so. A read-only table or JSON scan is preferable to rerunning a destructive action.

Do not use a public issue for a vulnerability, path or identity confusion, unintended or over-broad deletion, a race that could invalidate confirmation, or any report that could teach someone to trigger data loss. Stop using the affected build and use the private [security report](https://github.com/findyourexit/excise/security/advisories/new), following [SECURITY.md](SECURITY.md). Do not include sensitive real file system paths in a public report, and do not ask a reporter to rerun a potentially destructive action merely to collect logs.

## Security & Deletion Safety

For normal first use, follow [Getting Started](docs/getting-started.md) and the [permanent deletion contract](docs/safety/deletion.md). Deletion is permanent. If a run may have deleted the wrong entry, stop immediately. Preserve the command, exit status, and reports, then report it privately instead of attempting another deletion or cleanup command.

## Scope

Support commitments apply only to the targets marked Supported in the matrix. Build-only targets and file-system-specific behavior remain best effort. The project does not provide private consulting, emergency recovery, or guarantees for unreleased builds. The repository preserves Diskonaut history and historical tags for archival reference. They are not Excise releases. See [Project Lineage](docs/lineage.md).
