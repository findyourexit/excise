# Reports & JSON Formats

Excise produces **bounded** reports. When a scan is uncertain, incomplete, or unable to retain a navigable map, the report says so instead of claiming a complete inventory.

!!! info "Choose the consumer first"

    Use table output for people and shell pipelines. Use JSON for software. A nonzero exit can still carry a useful bounded report, so consumers must inspect both the exit code and the document state.

## Choose an Output Format

=== "Human-readable table"

    ```console
    excise --format table /path/to/inspect
    ```

    Table output never initializes a terminal and escapes paths for safe display. Its headings, column order, and layout are presentation details that may change between releases.

=== "Versioned JSON"

    ```console
    excise --format json /path/to/inspect
    excise --format json --output scan.json /path/to/inspect
    ```

    JSON uses named document types and stable version numbers. `--output FILE` writes the document instead of standard output and is available only in table or JSON mode.

!!! warning "Do not parse table layout"

    Table output is intentionally human-facing. Programs must consume the versioned JSON documents and validate them against their published schemas.

## Published JSON Contracts

| Document | Stable version | Purpose |
|---|---:|---|
| [`scan-report`](schemas/scan-report.schema.json) | 3 | Bounded scan result and terminal scan-store reservation |
| [`deletion-history`](schemas/deletion-history.schema.json) | 1 | Bounded result of reviewed deletion work |
| [`native-path`](schemas/native-path.schema.json) | 1 | Lossless platform-specific path encoding |

`scan-report` version 3 includes `scan_store_bytes` and `scan_store_limit_bytes` in its summary. They describe the private scan-storage reservation at terminal state; they are neither file-system space totals nor process-memory measurements.

???+ info "Read document state before interpreting totals"

    An unknown upper bound is `null`. Excise never substitutes an apparent file length for it. The `Shared` allocation summary has an explicit type and cannot be a deletion target.

    A `summary-only` scan report means scan-storage capacity was reached after directory reduction. The report retains terminal summary and root metrics, but deliberately has no navigable entry inventory. Run again with a larger `--scan-store-mib` value when a detailed retained map is required.

## Interactive Exports

In the normal view, press ++e++ to export the current scan and ++shift+e++ to export bounded deletion history. No result modal is required. Excise selects the first available filename in the current directory and never overwrites an existing file.

The scan export describes the current map, so right after a deletion it waits until the map no longer lists what was deleted: pressing ++e++ in that moment reports that the export is unavailable, and pressing it again a moment later writes the report.

The deletion history is written in the background, so a long history never delays the map. A notice confirms that the export began and another names the file once it is complete; the history is cleared only then, and only of the results the file holds. Asking again while an export is still being written reports that one is already running.

| Export | First filename | Later filenames |
|---|---|---|
| Scan report | `excise-scan-report.json` | `excise-scan-report-1.json`, then increasing suffixes |
| Deletion history | `excise-deletion-history.json` | `excise-deletion-history-1.json`, then increasing suffixes |

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
| `130` | Interrupted operation, including a signal-driven quit (Unix SIGTERM, SIGHUP, or SIGQUIT; a Windows console close, break, logoff, or shutdown event) |

A headless run (`--format table` or `--format json`) that receives one of these signals or events quits like an interactive one: it writes a `scan-report` document with `"state": "cancelled"`, covering what the scan had reduced by then, to `--output FILE` or standard output, and exits `130`. A second signal exits `130` at once, without waiting for publication or the report, so it can cut a report off mid-write: discard the output of a run that received two signals instead of reading it as a smaller valid document.

On Windows, a console close, logoff, or shutdown event gives the quit about four seconds to finish. A quit that finishes in that time exits `130`, as above. One that does not is ended by Windows, and a parent process then reads a status of Windows' own instead of `130`. A break has no such limit: Windows does not end the process when Excise stops waiting, so a slow quit still exits `130` when it finishes.

!!! tip "Safe consumer pattern"

    - [ ] Check the process exit code.
    - [ ] Validate JSON against the named schema and version.
    - [ ] Read the document state and uncertainty fields.
    - [ ] Treat `null`, `summary-only`, and partial outcomes as explicit limits, not missing defaults.

## Space Accounting

The main measure reports allocated space counted once for each file identity, even when it has more than one name. Physical storage shared by copy-on-write files, clones, compression, or file-system deduplication is not measured exactly. See [Space Accounting](safety/accounting.md) for the full contract.
