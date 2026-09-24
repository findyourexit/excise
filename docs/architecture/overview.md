# Architecture

## Ownership Model

One main loop owns application state, terminal state, layout, rendering, and visual effects. The persistent scanner, deletion-planner, and serial deletion-executor workers perform blocking file system work in the background. They send typed events through queues with fixed limits.

Visual effects run after the interface has prepared its content. They never own product state and never decide whether an operation is safe.

## Components

### Command Line & Configuration

Excise reads and validates command-line options, environment variables, and the TOML configuration file before it starts the terminal interface. It applies values in this order: command line, environment, versioned file, and defaults. It rejects unknown keys and invalid limits. Table and JSON reports work without a terminal.

### Terminal Session

A terminal session guard owns raw input mode, the separate screen, cursor visibility, colors, and optional mouse capture. It restores the terminal after normal return, a typed error, a panic, or a boundary-safe cancellation.

### Main Loop

The main loop polls terminal input with a short maximum wait. It renders each folder opening before it returns to queued scanning. It applies at most one stored scan batch before checking input again. When the map is moving, a fixed-size channel slows scanner updates. The interface redraws when state changes, apart from deletion progress and visual effects. It runs visual effects at most 30 times each second and drops overdue frames. A new transition replaces an earlier transition with the same purpose.

### Scanner

The scanner has a fixed number of workers and walks directories without recursion. One session coordinator owns work keys, task ownership records, focus, scan versions, and final status for scanning, reduction, refresh, and deletion. Its on-disk journal stores scanner data only. Workers cannot change queue or task-ownership state. The coordinator publishes one fixed-size status summary instead of a growing event backlog, so reading pending, active, or finished work does not delay scan output. The scanner journal, ScanStore runs, and page-index build data use a separate scan-store quota. Directory deletion plans and results use separate temporary storage.

One scanner service performs the first walk and later refreshes one at a time. A refresh has its own task ownership and cancellation boundary. It keeps the published result until a newer completed scan is ready. Deletion performs its own recheck. Focus requests change priority only when a worker accepts more work.

The default worker count leaves one processor available for the main loop when possible and stays between one and eight. A configured value must be between one and 32. Exclusions and file-system boundaries remain visible in the working model. Link targets are never traversed.

### ScanStore

`ScanStore` is private session storage for scan data. Workers turn each fixed-size batch into sorted path and identity runs. The coordinator accepts a run only while the directory task that produced it is still valid. It combines a fixed number of input runs at a time and creates a compact, block-indexed `ChildQuery` for navigation. It releases raw runs as it proceeds, so a completed scan retains only that compact query. Each published scan has a checksummed manifest. If a path cannot be represented, available pages remain concrete, the report records the omitted-path count, and affected space totals are lower bounds.

The interactive map reads one 512-entry page of direct children and its ancestor chain. It keeps only a small cache of recent pages. `PageDown` and `PageUp` move between concrete pages. No child is replaced with an undeletable summary. When an unfiltered page is not cached, the map reads only the requested slice from the published `ChildQuery`. Completed-folder navigation has no loading view or live-model fallback. Filters read only the relevant stored page.

### Working Model

Workers seal path and identity facts before the interface receives their events. During scanning, the interface keeps only limited progress and interaction state. It does not keep a second file-system-wide model that could change the scan result. When `ScanStore` publishes a scan, the map and reports read its immutable pages and query records. Names, metrics, and child lists are retained only for the visible page instead of the complete file system. Walking and deletion use loops rather than the call stack.

The map keeps a `MapOverflow` summary for entries that do not fit in the terminal. This affects only display geometry. Direct-child pagination keeps file-system entries concrete and selectable. The renderer checks available space before drawing the summary and shows count or size labels only when they fit.

The default process memory limit is 512 MiB. Working data may use 75 percent of that limit, leaving 25 percent for the rest of the process. The number of ScanStore runs combined at once, batch size, page size, and the sparse query index are fixed or block-bounded. No scan result can make an interface-owned collection grow with the scanned file system.

### Space Accounting

When `ScanStore` publishes a scan, it combines identity observations, places allocations shared by multiple names at their lowest common ancestor as noninteractive shared totals, and keeps concrete directory paths. A metadata failure remains uncertain. Interface memory pressure never fabricates a partial total.

### Deletion

The main loop keeps at most four non-overlapping interactive deletion requests. Every accepted planning or execution command has a coordinator-owned work record. An item returns to the ready queue only when its worker did not accept it, and the interface keeps the item until its success, failure, cancellation, or invalidation is recorded. A separate planner can prepare the next identity plan while one executor performs a confirmed target's final recheck and serial deletion. Large directory plans keep a limited in-memory prefix and place later plan and result records in authenticated temporary storage outside the selected target before consent. Platform code works relative to the confirmed parent and does not follow links. It verifies that every decoded plan path is beneath the selected target, checks identity, type, size, allocation, and modification state before each deletion, and skips changed entries. Newly observed entries are never added to the approved plan.

The [background task system decision](background-tasks.md) defines the session coordinator, the fixed-size deletion panel, and the review required before adding new task types.

### Reports

Versioned `scan-report` and `deletion-history` documents use the same stable encoding for file paths. Scan reports read immutable published ScanStore pages and state uncertainty from their recorded coverage.

## Dependency Direction

Platform code supplies file information to the scanner and deletion code. The scanner seals facts for ScanStore, which publishes immutable pages to application state and reports. Domain code does not depend on terminal widgets, and interface code does not change the file system.

The storage map uses dense half-block cells inside its pane. Each cell can carry foreground and background shading without gaps between entries. Map movement belongs to application state. The board keeps the current position, the next position, and one transition clock. A new scan can redirect the transition from its current position without restarting it.

Opening an entry grows its contents from the selected rectangle. Moving back contracts the departing contents into that rectangle while the parent view grows from it. Entries that are no longer in the parent view remain visible until they finish moving away. If the selected rectangle cannot be resolved, the board settles without a directed transition.

## Architectural Constraints

- No multiple threads may change application or terminal state.
- No queue or model owner may grow without a limit.
- No shell command may perform scanning or deletion.
- No network client or telemetry may run as part of the program.
- Unsafe code is confined to the reviewed Windows system interface. Domain, model, runtime, and interface code remain safe Rust.
- A new background task system requires public design review.
