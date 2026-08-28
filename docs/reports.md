# Reports & JSON Formats

Excise produces bounded reports. When a scan is uncertain or the interface has grouped entries that no longer fit in memory, the report says so instead of claiming a complete inventory.

## Table Output

```console
excise --format table /path/to/inspect
```

Table output is written for people and shell pipelines. Its headings, column order, and layout can change between releases. Use JSON when another program needs to read the result. Table mode does not initialize a terminal. Paths are escaped for safe display.

## JSON Output

```console
excise --format json /path/to/inspect
excise --format json --output scan.json /path/to/inspect
```

JSON uses named document types and stable version numbers. The published Draft 2020-12 formats are:

- [`scan-report` version 1](schemas/scan-report.schema.json)
- [`deletion-history` version 1](schemas/deletion-history.schema.json)
- [`native-path` version 1](schemas/native-path.schema.json)

An unknown upper bound is `null`. Excise never replaces it with an apparent file length. `Shared`, `Other`, and other summary records have explicit types and cannot be deletion targets.

## Interactive Exports

Press `e` in the normal view to export the current scan. Press `e` from deletion results to export deletion history. Excise writes the first available filename in the current directory:

- `excise-scan-report.json`, then `excise-scan-report-1.json`, and so on
- `excise-deletion-history.json`, then `excise-deletion-history-1.json`, and so on

Automatic export naming never overwrites an existing file.

## Exit Codes

| Code | Meaning |
|---:|---|
| `0` | Exact result |
| `2` | Usable result with uncertainty |
| `3` | Partial operation result |
| `64` | Command-line usage error |
| `70` | Runtime failure |
| `74` | Input or output failure |
| `78` | Configuration failure |
| `130` | Interrupted or forced-cancelled operation |

An uncertain or partial exit can still include a useful report. Consumers should inspect both the exit code and the document state.

## Space Accounting

The main space measure counts each file once, even when it has more than one name, and reports the allocated space. Physical storage shared by copy-on-write files, clones, compression, or file system deduplication is not measured exactly. See [Space Accounting](safety/accounting.md).
