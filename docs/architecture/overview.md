# Architecture

## Ownership model

One synchronous main-thread loop owns application state, terminal state, semantic layout, rendering, and visual-effect lifecycle. Scanner and deletion workers perform bounded blocking filesystem work and send typed events through bounded channels.

```text
Terminal input and timer ──────────┐
Scanner workers ─── typed batches ─┼─> Owner loop ─> Model ─> Semantic layout ─> Ratatui buffer
Deletion worker ─── typed results ─┘                                      └─> visual effects
```

Effects transform the rendered buffer after semantic widgets and layout are complete. They never own product state or determine whether an operation is safe.

## Components

### CLI and configuration

- Parse and validate command-line, environment, and TOML configuration before terminal initialization.
- Apply command line > environment > versioned file > defaults.
- Reject unknown keys and invalid bounds.
- Keep table and JSON report modes independent of a TTY.

### Terminal session

An RAII guard owns raw mode, alternate screen, cursor visibility, colors, and optional mouse capture. Restoration runs on normal return, typed errors, and panic unwind, with an explicit hard-cancellation fallback.

### Owner loop

- Poll Crossterm with a bounded timeout.
- Process input and worker events before optional visual frames.
- Render only when state is dirty.
- Cap active effects at 30 frames per second and drop overdue frames.
- Replace stale transitions by stable effect key.

### Scanner

A bounded scanner pool performs iterative, no-follow traversal. Directory tasks and worker events use bounded queues; overflow task state can spill to a permission-restricted session store rather than grow resident memory without limit. Backpressure never silently drops an entry.

The default worker count is `min(available_parallelism, 8)`, with a validated `1..=32` override. Exclusions and filesystem boundaries are explicit model records; symlink targets are never traversed.

### Model

A flat arena keyed by stable node IDs stores names once, parent/child relationships, native identity, metrics, scan state, and aggregate state. Traversal, compaction, and destruction are iterative.

On current `main`, the public `geometry::MapOverflow` summary returned by `TreeMap::overflow()` records entries omitted from the final map viewport, not merely individually small entries. It retains their count, bytes, and lower-bound uncertainty even when no overflow region can be drawn; its anchor may be a non-drawable sentinel when no free field exists, so consumers must check drawable bounds before painting; count and weight labels appear only when that region has enough usable space.

The default process envelope is 512 MiB. A hard 75% model/index budget leaves 25% process headroom. Cold compaction preserves exact aggregates while active ancestors, visible nodes, and operation targets remain pinned.

### Accounting

The identity table deduplicates hard links globally within scan scope. When exact identity state exceeds its resident budget, a permission-restricted session-only store retains minimum identity and accounting data. Unknown values propagate as bounds rather than guessed numbers.

### Deletion

The owner builds and reviews a complete identity plan. Platform adapters execute no-follow, capability-relative operations and revalidate identity, type, size, allocation, and modification state before each mutation. Changed entries are skipped; newly observed identities are never added to consent.

### Reports

Versioned `scan-report` and `deletion-history` documents share a stable native-path encoding. Reports describe the bounded logical model and explicitly carry uncertainty and aggregation.

## Dependency direction

```text
platform adapters ─┐
scanner ───────────┼─> domain model and accounting ─> application state ─> UI
mutation adapter ──┘                                            └────────> reports
```

Domain code does not depend on terminal widgets. UI code does not mutate the filesystem. Visual effects consume rendered semantic output, never the reverse.

The shell presents independent workspaces with low-ink pane gaps, title chips, and active-focus chrome. The storage map remains densely tessellated inside its pane: half-block cells carry shaded foreground and background halves instead of inserting gaps between entries.

Map movement is application state, not an effect. The board holds the geometry every entry is drawn at, the geometry it is heading for, and one clock; a layout change re-aims that tween from wherever entries currently sit rather than restarting it, which is what lets a streaming scan keep moving without juddering. Directed transitions apply only to map layout. Navigation supplies a pivot — the rectangle a drill radiates from — so opening an entry grows its contents out of it. On a drill-out, departing contents contract into that pivot while the incoming parent layout grows out of it. Entries the incoming layout no longer contains keep being drawn until they have finished receding into the pivot. If a pivot cannot be resolved, the board settles without a directed tween, or waits for list selection when that is the only missing geometry. Effects remain confined to the header band, acknowledging events rather than repainting the map.

## Architectural constraints

- no multiple threads mutating application or terminal state;
- no unbounded channels or recursive model ownership;
- no shell commands for traversal or deletion;
- no runtime network client or telemetry;
- unsafe code is confined to the audited Windows FFI adapter; domain, model, runtime, and UI code remain safe Rust;
- no new runtime executor without public design review.
