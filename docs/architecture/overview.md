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

The main loop polls terminal input with a bounded timeout. It renders each folder drill before it resumes queued scanner work, applies staged scan batches one entry per input poll, and uses bounded-channel backpressure while treemap geometry is moving. It redraws only when state has changed, except for a live atomic deletion progress counter and frame-only chrome. It limits active effects to 30 frames per second and drops overdue frames. A new transition replaces an older transition with the same purpose.

### Scanner

The scanner uses a fixed number of workers and walks directories without recursion. One session coordinator actor owns typed work keys, leases, focus, generation transitions, and terminal outcomes for scanner, reduction, refresh, and deletion work. Its bounded disk-backed task journal stores scanner payloads only; workers never mutate queue or lease state directly. The coordinator publishes one coalesced, bounded status snapshot rather than a progress-event backlog, so observing pending, active, and terminal work never delays scan output. The scanner journal, ScanStore runs, and the bounded build index used to publish direct-child pages use an independent scan-store budget; directory deletion-plan and outcome records use a separate temporary-storage reservation.

A single scanner service executes the initial walk and invalidation-triggered refreshes serially. A refresh receives its own coordinator lease and cancellation fence, preserves the current published generation until a strictly newer canonical generation completes, and leaves independent deletion revalidation authoritative. Focus requests reprioritize existing work lazily at lease time.

The default worker count leaves one available processor for the owner loop when possible and is clamped from one through eight. The configured value must be between one and 32. Exclusions and file system boundaries remain visible in the working model. Link targets are never traversed.

### ScanStore

Scanner workers sort each bounded fact batch into sealed path and identity runs using a generation-bound factory. The scanner coordinator forwards a run only while its exact directory-work lease remains active; the owner is the sole admission point and folds each run family at a fixed fan-in before publication. Final reduction derives one physical allocation contribution per file identity and post-order directory summaries, then folds those facts plus the original overlay facts into a sparse `ChildQuery` run indexed by its written blocks. The builder consumes and releases each raw run before the next derived phase, so a published generation retains only that compact query run. Every generation is checksummed by its manifest. A path that cannot be represented keeps available pages concrete, reports its omitted-path count, and presents affected space totals as lower bounds.

The interactive map materializes one 512-entry direct-child page plus its ancestor chain, then retains only a fixed small LRU cache of recently visited immutable pages. `PageDown` and `PageUp` move between concrete pages; no child is replaced by an undeletable summary. An unfiltered cache miss seeks directly into the publication-time `ChildQuery` run and decodes only the requested slice, so completed-folder navigation has no loading view or live-model fallback. Filters walk only the scoped canonical page.

### Working Model

Scanner workers seal canonical path and identity facts before the owner observes their UI events. During scanning the UI keeps only bounded progress and interaction state; it does not retain a second filesystem-wide path model whose capacity could change the scan result. Once a ScanStore generation publishes, the interactive map and scan reports read its immutable canonical pages and query records: names, metrics, and child lists are bounded to the visible page rather than retained for the complete filesystem. Walking and removal use loops rather than the call stack.

The map keeps a `MapOverflow` summary for regions that do not fit in the terminal viewport. This is display geometry only; direct-child pagination keeps filesystem entries concrete and selectable. The renderer checks the available drawing area before it paints the summary and shows count or weight labels only when there is enough room.

The default process memory limit is 512 MiB. Working data may use 75 percent of that limit, leaving 25 percent for the rest of the process. ScanStore run fan-in, batch size, page size, and the sparse query index are fixed or block-bounded; no scanner result makes a UI-owned collection grow with the scanned filesystem.

### Space Accounting

Published ScanStore generations reduce canonical identity observations, place multi-name allocations at their lowest common ancestor as noninteractive shared totals, and retain concrete directory paths. A real metadata failure remains uncertain; UI memory pressure never fabricates a partial canonical total.

### Deletion

The main loop retains at most four non-overlapping interactive deletion requests. Every accepted planning or execution command holds an exact coordinator lease: work can return to the ready ledger only when its worker did not accept it, and its terminal success, failure, cancellation, or invalidation is recorded before the UI rail releases it. A separate planner can build the next identity plan while the single executor performs a confirmed target's final full-plan revalidation and serial mutation. Large directory plans retain a bounded resident prefix and use authenticated temporary storage outside the selected target for later plan and outcome records before consent. Platform code works relative to the confirmed parent and does not follow links. It validates each decoded plan path as a componentwise descendant of the selected target, checks the file identity, type, size, allocation, and modification state before each deletion, and skips changed entries. Newly observed entries are never added to the consented plan.

The [background task system decision](background-tasks.md) defines the single session coordinator, bounded deletion rail, and review required before adding new task kinds.

### Reports

Versioned `scan-report` and `deletion-history` documents use the same stable encoding for file paths. Scan reports stream from immutable published ScanStore generations and identify uncertainty directly from canonical coverage.

## Dependency Direction

Platform code supplies file information to the scanner and deletion code. The scanner seals facts for ScanStore, which publishes immutable pages to application state and reports; domain code does not depend on terminal widgets, and interface code does not change the file system.

The storage map uses dense half-block cells inside its pane. Each cell can carry foreground and background shading without inserting gaps between entries. Map movement belongs to application state. The board keeps the current position, the next position, and one transition clock. A new scan can redirect the transition from its current position without restarting it.

Opening an entry grows its contents from the selected rectangle. Moving back contracts the departing contents into that rectangle while the parent view grows from it. Entries that are no longer in the parent view remain visible until they finish moving away. If the selected rectangle cannot be resolved, the board settles without a directed transition.

## Architectural Constraints

- No multiple threads may change application or terminal state.
- No queue or model owner may grow without a limit.
- No shell command may perform scanning or deletion.
- No network client or telemetry may run as part of the program.
- Unsafe code is confined to the reviewed Windows system interface. Domain, model, runtime, and interface code remain safe Rust.
- A new background task system requires public design review.
