# Support

## 0.1.1 early testing

The planned `0.1.1` release is for early testing. Support is best effort, the public library API may change before 1.0, and no release build should be trusted with irreplaceable data. Start with [Getting started](docs/getting-started.md) and a disposable fixture.

## Usage questions

Search the [documentation](docs/README.md) and existing discussions first. Use [GitHub Discussions](https://github.com/findyourexit/excise/discussions) for installation, configuration, and usage questions.

## Troubleshooting

### Permission or sharing failures

A scan permission error means Excise could not read metadata or enumerate part of the tree; the report is uncertain rather than a complete inventory. Check that the account has the needed access to the path and its parent directories, correct the ACL or mount access, and rescan.

A deletion `failed` result means Excise did not confirm that entry as deleted. On Unix, check write and execute permission on the parent directory. On Windows, close applications, sync clients, indexers, or antivirus software that may hold an incompatible sharing handle, then start a fresh scan. Do not infer the filesystem state from the error alone.

If a deletion was hard-cancelled, its result is imprecise; rescan before acting again. Preserve the exit code and deletion-history report when asking for help. Never continue an old deletion plan after a failed, cancelled, changed, or uncertain run.

### Boundaries and exclusions

If paths are missing, check the default one-filesystem boundary and configured exclusions before treating them as read failures. `--cross-filesystems` permits traversal across mounts and should be used only when that scope is intended. `--exclude PATTERN` uses ordered gitignore-style patterns rooted at the scan path.

Excluded entries and foreign-filesystem boundaries remain visible as zero-byte scoped records with their reason, but their descendants are not traversed. Symlink targets are not followed; on Windows this also includes junction and reparse targets.

### Terminal and non-TTY output

The TUI requires stdin and stdout TTYs, ANSI rendering, alternate-screen support, and a window at least `32 x 8`. Resize a smaller terminal or use a headless mode. Table and JSON modes run without a TTY, raw mode, or alternate screen and are suitable for redirection, shell pipelines, and CI. `--output FILE` is valid only with table or JSON mode.

### Native paths and Windows limitations

Text output uses escaped display paths: terminal and bidirectional controls, newlines, tabs, backslashes, invalid Unix path bytes, and ill-formed Windows UTF-16 are escaped. In JSON, `path` is lossless native data (`unix-bytes` on Unix or `windows-utf16-le` on Windows, base64); `display_path` is presentation text. See [Reports and schemas](docs/reports.md).

Windows does not populate allocated-byte snapshots, so allocated and reclaimable upper bounds can be unknown; use `--apparent-size` for logical file lengths. Windows deletion uses no-follow handles, but ACLs and sharing modes can still reject an entry. See [Getting started](docs/getting-started.md) for first-use and platform guidance.

## Defects and bug reports

Use the [issue chooser](https://github.com/findyourexit/excise/issues/new/choose) for a reproducible, non-sensitive defect. Include the Excise version or commit, operating system, terminal and shell, exact command with real paths replaced, filesystem and mount context, permissions, links or junctions, configuration, terminal dimensions, expected behavior, actual behavior, and whether deletion occurred. Preserve the exit code and relevant table/JSON or deletion-history report excerpts after removing paths, usernames, and other private data. Reproduce only on a synthetic disposable fixture when it is safe to do so; a read-only table or JSON scan is preferable to rerunning a destructive action.

Do not use a public issue for a vulnerability, path or identity confusion, an unintended or over-broad deletion, a race that could invalidate consent, or any report that could teach someone to trigger data loss. Stop using the affected build and use the private [security report](https://github.com/findyourexit/excise/security/advisories/new), following [SECURITY.md](SECURITY.md). Do not include sensitive real filesystem paths when a synthetic reproduction is possible, and do not ask a reporter to rerun a potentially destructive command merely to collect logs.

## Security and deletion safety

For normal first use, follow [Getting started](docs/getting-started.md) and the [permanent-deletion contract](docs/safety/deletion.md). Deletion is permanent; there is no trash or undo. If a run may have deleted the wrong entry, stop immediately, preserve the command, exit status, and reports, and report privately rather than attempting a second deletion or cleanup command.

## Scope

Support is best effort. The project does not provide private consulting, emergency recovery, or guarantees for pre-release builds. The repository preserves Diskonaut history and historical tags for archival reference; they are not Excise releases, and historical Diskonaut releases are unsupported. See [Project lineage](docs/lineage.md).
