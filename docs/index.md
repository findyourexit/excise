<p align="center">
  <img src="https://raw.githubusercontent.com/findyourexit/excise/main/assets/excise-header.png" alt="Excise, a surgical terminal storage navigator" />
</p>

# Excise

Excise is a terminal storage navigator for understanding and surgically removing exactly the files and folders you choose. It combines an interactive storage map with careful space accounting, bounded resource use, safe handling of unusual file names, and a deliberate review before permanent deletion.

!!! danger "Permanent means permanent"

    Excise deletes confirmed entries without a trash folder or undo. Start with a disposable directory, keep the default confirmation enabled, and read the [Permanent Deletion Contract](safety/deletion.md) before acting on real data.

<div class="grid cards" markdown>

-   :rocket: **Start safely**

    Install Excise, create a disposable fixture, and learn the review flow before scanning anything important.

    [Getting Started](getting-started.md){ .md-button .md-button--primary }

-   :gear: **Set your defaults**

    Configure scan scope, resource budgets, themes, keymaps, and noninteractive report output.

    [Configuration](configuration.md){ .md-button }

-   :bar_chart: **Understand the result**

    Choose human-readable table output or versioned JSON, then interpret exact, uncertain, and partial results correctly.

    [Reports & JSON Formats](reports.md){ .md-button }

-   :shield: **Review the safety model**

    Learn the deletion, accounting, and threat-model contracts that bound what Excise claims and does.

    [Safety Contracts](safety/deletion.md){ .md-button }

</div>

## A Map With Explicit Limits

| If you need to… | Start here |
|---|---|
| Inspect disk usage interactively | [Getting Started](getting-started.md) |
| Set persistent scan or interface preferences | [Configuration](configuration.md) |
| Consume stable output in automation | [Reports & JSON Formats](reports.md) |
| Understand concurrent scanning, deletion, and storage | [Architecture](architecture/overview.md) |
| Audit deletion or accounting guarantees | [Safety Contracts](safety/deletion.md) |

??? info "What Excise does not promise"

    Excise does not claim a physical-storage total for copy-on-write files, clones, compression, or filesystem deduplication. It keeps uncertainty explicit, never converts an unknown allocation into a guessed value, and never turns a partial scan into deletion authority. See [Space Accounting](safety/accounting.md) and the [Threat Model](architecture/threat-model.md).

## Learn the System

<div class="grid cards" markdown>

-   **Architecture**

    The main loop, session coordinator, persistent scanner, immutable ScanStore, and bounded ownership model.

    [Read the architecture](architecture/overview.md)

-   **Background tasks**

    Why scan, refresh, planning, and deletion work have fixed queues and explicit ownership.

    [Read the decision](architecture/background-tasks.md)

-   **Safety contracts**

    The identity checks, interruption behavior, unknown-space rules, and evidence needed for safe change.

    [Read the contracts](safety/deletion.md)

-   **Project lineage**

    Excise’s relationship to Diskonaut and the preserved historical tags.

    [Read the lineage](lineage.md)

</div>

## Contribute and Maintain

- [Development](development.md) covers the toolchain, local verification, generated artifacts, fuzzing, benchmarks, and the documentation site.
- [Release Process](releasing.md) defines the exact-commit candidate and promotion procedure.
- [Project Lineage](lineage.md) records attribution and the boundary between Excise and its predecessors.

## Project Policies

- [Contributing](https://github.com/findyourexit/excise/blob/main/CONTRIBUTING.md)
- [Support](https://github.com/findyourexit/excise/blob/main/SUPPORT.md)
- [Security](https://github.com/findyourexit/excise/blob/main/SECURITY.md)
- [Governance](https://github.com/findyourexit/excise/blob/main/GOVERNANCE.md)
- [Maintainers](https://github.com/findyourexit/excise/blob/main/MAINTAINERS.md)
- [Code of Conduct](https://github.com/findyourexit/excise/blob/main/CODE_OF_CONDUCT.md)

## Published Formats

- [Scan Report schema](schemas/scan-report.schema.json)
- [Deletion History schema](schemas/deletion-history.schema.json)
- [Native Path schema](schemas/native-path.schema.json)

## Source

[GitHub repository](https://github.com/findyourexit/excise){ .md-button .md-button--primary }
[crates.io package](https://crates.io/crates/excise){ .md-button }
