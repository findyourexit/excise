# Background Task System Decision

**Status:** Accepted. One session coordinator owns scanning, reduction, refresh, and deletion work. New task types need their own review.

## Problem

Scanning, page lookup, deletion planning, final checks, and deletion can block on the file system. The main loop must remain the only writer of application and terminal state so the reader can keep navigating the map.

## Decision

The application keeps a fixed-size `DeletionWork` queue separate from `UiMode`. Each retained deletion item has an increasing work ID, a concrete path broken into components, an escaped display label, and one state:

1. awaiting confirmation or queued planning,
2. planning,
3. queued or serial execution, or
4. cancellation acknowledgement, rejection, or completion.

The planner and executor use separate fixed-size queues. Only one planner builds an identity-bound plan at a time. Only one executor performs final checks and file-system changes. It rechecks and changes files in one operation, so no queue handoff opens a gap between a successful check and the first change. A planner can prepare a non-overlapping target while the executor is active, but it cannot change files.

Only concrete files and directories enter deletion planning. The shared-allocation summary is the only virtual entry, and it cannot be selected. The planner's live walk without following links reviews a directory's descendants rather than relying on the displayed page.

Confirmation stays in the foreground before planning begins. After confirmation, the reader returns immediately to normal map navigation while the planner and then the serial executor perform their final checks and file changes in the background.

The primary scanner walks breadth first. One session coordinator owns work keys, task ownership records, focus, and final status. Its on-disk journal contains only scanner data with fixed limits. When readers open or leave folders, the application reads stored pages of direct children. It never starts a separate scan for each folder or reorders recorded facts. A change that invalidates a published scan schedules a versioned refresh through the persistent scanner. Until that refresh completes, the application never combines the earlier exact map with new live data.

## Bounds and Target Conflicts

- `MAX_DELETION_WORK_ITEMS` caps every retained deletion operation, including active work, confirmations, and planner-cancellation reservations.
- Planner, executor, scanner-rebuild, and event channels have fixed capacities. A rejected submission restores its item instead of dropping it.
- A new item is rejected when its path is the same as, contains, or is within a retained target. This prevents ancestor, descendant, and duplicate operations from racing.
- A running planner keeps its reservation until it acknowledges cancellation. A generation rebuild keeps its scan-store session boundary until it publishes or is cancelled.
- Deletion history has a byte limit and a fixed report-count cap. Summaries keep only fixed-size counters and an atomic progress value. Reports stream directly from limited memory or authenticated temporary storage.

## Exit and Reconciliation

The exit dialog distinguishes no work, cancellable pending work, and active deletion. Pending work can be cancelled before quitting or left running while the reader returns to the map. Active deletion can stop only at an entry boundary or be awaited. The worker is never detached silently.

A completed report either invalidates or republishes the current ScanStore result. A complete target removal creates an immutable overlay without that path. A partial or uncertain result discards the active view and schedules a newer root scan. The board then selects a surviving actionable entry, clears an empty view, or restores the nearest valid folder.

## Consequences

The visible work panel shows limited background activity and progress without copying plans or reports into frame state. The foreground dialog remains limited to an irreversible consent decision, and report formats retain their existing precise or uncertain meanings. Any change to target-overlap rules, queue capacities, serial mutation, no-follow validation, report retention, or exit behavior requires public architecture review.