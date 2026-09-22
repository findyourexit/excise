# Architecture

## Ownership Model

One main loop owns application state, terminal state, layout, rendering, and visual effects. Scanner, focused-rescan, deletion-planner, and serial deletion-executor workers perform blocking file system work in the background. They send typed events through queues with fixed limits.

Visual effects run after the interface has prepared its content. They never own product state and never decide whether an operation is safe.

## Components

### Command Line & Configuration

Excise reads and validates command-line options, environment variables, and the TOML configuration file before it starts the terminal interface. It applies values in this order: command line, environment, versioned file, and defaults. It rejects unknown keys and invalid limits. Table and JSON reports work without a terminal.

### Terminal Session

A terminal session guard owns raw input mode, the separate screen, cursor visibility, colors, and optional mouse capture. It restores the terminal after normal return, a typed error, a panic, or a boundary-safe cancellation.

### Main Loop

The main loop polls terminal input with a bounded timeout. It renders each folder drill before it resumes queued scanner work, applies staged scan batches one entry per input poll, and uses bounded-channel backpressure while treemap geometry is moving. It redraws only when state has changed, except for a live atomic deletion progress counter and frame-only chrome. It limits active effects to 30 frames per second and drops overdue frames. A new transition replaces an older transition with the same purpose.

### Scanner

The scanner uses a fixed number of workers and walks directories without recursion. Directory tasks and worker events use queues with fixed limits. Queued directory tasks, ScanStore runs, identity spill files, and overflow directory deletion-plan and outcome records share one per-session temporary-storage limit and reserve capacity before their files grow. A queued-task capacity breach reports an actionable incomplete scan; a ScanStore write or merge failure is never published as an exact generation; identity-store capacity exhaustion releases its private database and continues traversal with unknown legacy-model bounds; a deletion plan that cannot retain every reviewed identity and outcome is rejected before consent, and a post-consent result-storage failure stops new mutations with an explicit incomplete result. Backpressure never silently drops work.

The default worker count leaves one available processor for the owner loop when possible and is clamped from one through eight. The configured value must be between one and 32. Exclusions and file system boundaries remain visible in the working model. Link targets are never traversed.

### ScanStore

The owner buffers at most 128 scanner facts, sorts each bounded batch into canonical path and identity runs, and folds each run family at a fixed fan-in. Final reduction produces immutable path facts, one physical allocation contribution per file identity, and post-order directory summaries. Every published generation is checksummed by its manifest and retains its run reservations until replacement.

The interactive map materializes only one 512-entry direct-child page plus its ancestor chain. `PageDown` and `PageUp` move between concrete pages; no child is collapsed into an undeletable storage aggregate. Page entries retain the scan-time identity, allocation, and modification snapshot used by deletion planning. Focused rescans and completed full-target deletions publish a new overlay generation while retaining the previous immutable generation until replacement succeeds.

### Working Model

The live table indexed by stable node numbers remains the bounded compatibility and report model while scanning. Once a ScanStore generation publishes, the interactive map reads its page snapshot instead: names, metrics, and child lists are bounded to the visible page rather than retained for the complete filesystem. Walking, compacting, and removing entries use loops rather than the call stack.

The map keeps a `MapOverflow` summary for regions that do not fit in the terminal viewport. This is display geometry only; direct-child pagination keeps filesystem entries concrete and selectable. The renderer checks the available drawing area before it paints the summary and shows count or weight labels only when there is enough room.

The default process memory limit is 512 MiB. Working data may use 75 percent of that limit, leaving 25 percent for the rest of the process. ScanStore run fan-in, batch size, and page size are all fixed; no scanner result makes a UI-owned collection grow with the scanned filesystem.

### Space Accounting

The identity table counts files with more than one name once within the scan scope. When exact identity data exceeds the memory limit, a permission-restricted store for the current session keeps the minimum records needed for legacy-model accounting within the shared temporary-storage limit. Published ScanStore generations independently reduce canonical identity observations, place multi-name allocations at their lowest common ancestor as noninteractive shared totals, and retain concrete directory paths. A real metadata failure remains uncertain; arena capacity pressure never fabricates a partial ScanStore total.

### Deletion

The main loop retains at most four non-overlapping interactive deletion requests. A separate planner can build the next identity plan while the single executor performs a confirmed target's final full-plan revalidation and serial mutation. Large directory plans retain a bounded resident prefix and use authenticated temporary storage outside the selected target for later plan and outcome records before consent. Platform code works relative to the confirmed parent and does not follow links. It validates each decoded plan path as a componentwise descendant of the selected target, checks the file identity, type, size, allocation, and modification state before each deletion, and skips changed entries. Newly observed entries are never added to the consented plan. A complete removal invalidates its ScanStore prefix and publishes a replacement generation before the map accepts another view of that path.

The [background task system decision](background-tasks.md) defines the bounded deletion-work foundation and the review required before adding other task kinds.

### Reports

Versioned `scan-report` and `deletion-history` documents use the same stable encoding for file paths. Reports describe the bounded working model and identify uncertainty and summary entries explicitly.

## Dependency Direction

Platform code supplies file information to the scanner and deletion code. The scanner supplies facts to the bounded compatibility model and ScanStore. ScanStore publishes immutable pages to application state; reporting reads the compatibility model. Domain code does not depend on terminal widgets, and interface code does not change the file system.

The storage map uses dense half-block cells inside its pane. Each cell can carry foreground and background shading without inserting gaps between entries. Map movement belongs to application state. The board keeps the current position, the next position, and one transition clock. A new scan can redirect the transition from its current position without restarting it.

Opening an entry grows its contents from the selected rectangle. Moving back contracts the departing contents into that rectangle while the parent view grows from it. Entries that are no longer in the parent view remain visible until they finish moving away. If the selected rectangle cannot be resolved, the board settles without a directed transition.

## Architectural Constraints

- No multiple threads may change application or terminal state.
- No queue or model owner may grow without a limit.
- No shell command may perform scanning or deletion.
- No network client or telemetry may run as part of the program.
- Unsafe code is confined to the reviewed Windows system interface. Domain, model, runtime, and interface code remain safe Rust.
- A new background task system requires public design review.
