# Architecture

## Ownership Model

One main loop owns application state, terminal state, layout, rendering, and visual effects. Scanner and deletion workers perform blocking file system work in the background. They send typed events through queues with fixed limits.

Visual effects run after the interface has prepared its content. They never own product state and never decide whether an operation is safe.

## Components

### Command Line & Configuration

Excise reads and validates command-line options, environment variables, and the TOML configuration file before it starts the terminal interface. It applies values in this order: command line, environment, versioned file, and defaults. It rejects unknown keys and invalid limits. Table and JSON reports work without a terminal.

### Terminal Session

A terminal session guard owns raw input mode, the separate screen, cursor visibility, colors, and optional mouse capture. It restores the terminal after normal return, a typed error, a panic, or a forced cancellation.

### Main Loop

The main loop polls terminal input with a bounded timeout. It renders each folder drill before it resumes queued scanner work, applies staged scan batches one entry per input poll, and uses bounded-channel backpressure while treemap geometry is moving. It redraws only when state has changed. It limits active effects to 30 frames per second and drops overdue frames. A new transition replaces an older transition with the same purpose.

### Scanner

The scanner uses a fixed number of workers and walks directories without recursion. Directory tasks and worker events use queues with fixed limits. Queued directory tasks, identity spill files, and overflow directory deletion-plan and outcome records share one per-session temporary-storage limit, reserve capacity before their files grow, and release it after shrinking or cleanup. A capacity breach reports an actionable failure and never turns incomplete work into an exact result; backpressure never silently drops an entry.

The default worker count leaves one available processor for the owner loop when possible and is clamped from one through eight. The configured value must be between one and 32. Exclusions and file system boundaries remain visible in the working model. Link targets are never traversed.

### Working Model

A table indexed by stable node numbers stores each name once along with its parent, children, file identity, measurements, scan state, and summary state. Walking, compacting, and removing entries use loops rather than the call stack.

The map keeps a `MapOverflow` summary for entries that do not fit in the final view. It retains their count, space, and uncertainty even when there is no room to draw an overflow region. The renderer checks the available drawing area before it paints the summary and shows count or weight labels only when there is enough room.

The default process memory limit is 512 MiB. Working data may use 75 percent of that limit, leaving 25 percent for the rest of the process. Compaction preserves exact summaries while visible entries, active parents, and deletion targets remain available.

### Space Accounting

The identity table counts files with more than one name once within the scan scope. When exact identity data exceeds the memory limit, a permission-restricted store for the current session keeps the minimum records needed for accounting within the shared temporary-storage limit. Unknown values remain bounds rather than becoming guessed numbers.

### Deletion

The main loop builds and reviews a complete deletion plan. Large directory plans retain a bounded resident prefix and use authenticated temporary storage outside the selected target for later plan and outcome records before consent. Platform code works relative to the confirmed parent and does not follow links. It validates each decoded plan path as a componentwise descendant of the selected target, checks the file identity, type, size, allocation, and modification state before each deletion, and skips changed entries. Newly observed entries are never added to the consented plan; a plan that cannot retain every identity and outcome is rejected before confirmation.

### Reports

Versioned `scan-report` and `deletion-history` documents use the same stable encoding for file paths. Reports describe the bounded working model and identify uncertainty and summary entries explicitly.

## Dependency Direction

Platform code supplies file information to the scanner and deletion code. The scanner supplies information to the working model and space accounting. Application state supplies data to the interface. The reporting code reads the resulting state. Domain code does not depend on terminal widgets, and interface code does not change the file system.

The storage map uses dense half-block cells inside its pane. Each cell can carry foreground and background shading without inserting gaps between entries. Map movement belongs to application state. The board keeps the current position, the next position, and one transition clock. A new scan can redirect the transition from its current position without restarting it.

Opening an entry grows its contents from the selected rectangle. Moving back contracts the departing contents into that rectangle while the parent view grows from it. Entries that are no longer in the parent view remain visible until they finish moving away. If the selected rectangle cannot be resolved, the board settles without a directed transition.

## Architectural Constraints

- No multiple threads may change application or terminal state.
- No queue or model owner may grow without a limit.
- No shell command may perform scanning or deletion.
- No network client or telemetry may run as part of the program.
- Unsafe code is confined to the reviewed Windows system interface. Domain, model, runtime, and interface code remain safe Rust.
- A new background task system requires public design review.
