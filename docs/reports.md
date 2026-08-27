# Reports and schemas

Excise produces bounded reports rather than claiming a complete inventory when the scan is uncertain or the model has aggregated cold entries.

## Table output

```console
excise --format table /path/to/inspect
```

Table mode is intended for people and shell pipelines. Its headings, column order, and layout are human-facing and not a machine-compatibility contract; use JSON for machine consumers. It does not initialize a TTY. Paths are escaped for safe display.

## JSON output

```console
excise --format json /path/to/inspect
excise --format json --output scan.json /path/to/inspect
```

JSON uses versioned document kinds and lossless native-path encoding. The published Draft 2020-12 schemas are:

- [`scan-report` v1](schemas/scan-report.schema.json)
- [`deletion-history` v1](schemas/deletion-history.schema.json)
- [`native-path` v1](schemas/native-path.schema.json)

Unknown upper bounds are `null`; they are never replaced with apparent bytes. Synthetic `Shared`, `Other`, and aggregate records are explicitly typed and are not deletion targets.

## TUI exports

Press `e` in the normal view to export the current scan. Press `e` from deletion results to export deletion history. Excise writes the first available filename in the current directory:

- `excise-scan-report.json`, then `excise-scan-report-1.json`, and so on;
- `excise-deletion-history.json`, then `excise-deletion-history-1.json`, and so on.

Existing files are never overwritten by automatic TUI export naming.

## Exit codes

| Code | Meaning |
|---:|---|
| `0` | exact result |
| `2` | usable but uncertain result |
| `3` | partial operation result |
| `64` | command-line usage error |
| `70` | runtime failure |
| `74` | I/O failure |
| `78` | configuration failure |
| `130` | interrupted or hard-cancelled operation |

An uncertain or partial exit may still include a valid report. Consumers should inspect both the exit code and document state.

## Accounting semantics

The headline metric is identity-unique allocated bytes. Hard-linked identities are deduplicated; physical shared extents from reflinks, cloning, compression, or filesystem deduplication are not. See [Storage accounting](safety/accounting.md).
