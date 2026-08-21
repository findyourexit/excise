# Support

## Usage questions

Search the [documentation](docs/README.md) and existing discussions first. Use [GitHub Discussions](https://github.com/findyourexit/excise/discussions) for installation, configuration, and usage questions.

## Troubleshooting

### Permission or sharing failures

A scan permission error means Excise could not read metadata or enumerate part of the tree; the report is uncertain rather than a complete inventory. Check that the account has the needed access to the path and its parent directories, correct the ACL or mount access, and rescan.

A deletion `failed` result means Excise did not confirm that entry as deleted. On Unix, check write and execute permission on the parent directory. On Windows, close applications, sync clients, indexers, or antivirus software that may hold an incompatible sharing handle, then start a fresh scan. Do not infer the filesystem state from the error alone.

If a deletion was hard-cancelled, its result is imprecise; rescan before acting again. Preserve the exit code and deletion-history report when asking for help.

### Boundaries and exclusions

If paths are missing, check the default one-filesystem boundary and configured exclusions before treating them as read failures. `--cross-filesystems` permits traversal across mounts and should be used only when that scope is intended. `--exclude PATTERN` uses ordered gitignore-style patterns rooted at the scan path.

Excluded entries and foreign-filesystem boundaries remain visible as zero-byte scoped records with their reason, but their descendants are not traversed. Symlink targets are not followed; on Windows this also includes junction and reparse targets.

### Terminal and non-TTY output

The TUI requires stdin and stdout TTYs, ANSI rendering, alternate-screen support, and a window at least `32 x 8`. Resize a smaller terminal or use a headless mode. Table and JSON modes run without a TTY, raw mode, or alternate screen and are suitable for redirection, shell pipelines, and CI. `--output FILE` is valid only with table or JSON mode.

### Native paths and Windows limitations

Text output uses escaped display paths: terminal and bidirectional controls, newlines, tabs, backslashes, invalid Unix path bytes, and ill-formed Windows UTF-16 are escaped. In JSON, `path` is lossless native data (`unix-bytes` on Unix or `windows-utf16-le` on Windows, base64); `display_path` is presentation text. See [Reports and schemas](docs/reports.md).

Windows does not populate allocated-byte snapshots, so allocated and reclaimable upper bounds can be unknown; use `--apparent-size` for logical file lengths. Windows deletion uses no-follow handles, but ACLs and sharing modes can still reject an entry. See [Getting started](docs/getting-started.md) for first-use and platform guidance.

## Defects

Use the issue chooser for reproducible bugs. Include the Excise version or commit, operating system, terminal, command, filesystem context, expected behavior, and actual behavior. Replace real paths with synthetic fixtures where possible.

## Security and deletion safety

For normal first use, follow [Getting started](docs/getting-started.md) and the [permanent-deletion contract](docs/safety/deletion.md). Deletion is permanent; there is no trash or undo.

Do not use a public issue for a vulnerability or an unintended-deletion path that could put users at risk. Follow [SECURITY.md](SECURITY.md) instead.

## Scope

Support is best effort. The project does not provide private consulting, emergency recovery, or guarantees for pre-release builds. The repository preserves Diskonaut history and historical tags for archival reference; they are not Excise releases, and historical Diskonaut releases are unsupported. See [Project lineage](docs/lineage.md).
