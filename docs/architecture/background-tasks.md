# Background Task System Decision

**Status:** accepted for deletion work; future task kinds require their own review.

## Problem

Primary scanning, focused exploration, deletion planning, final revalidation, and mutation can block on the file system. The owner loop must remain the only writer of application and terminal state while the reader can continue navigating the map.

## Decision

The application owns a bounded `DeletionWork` rail separately from `UiMode`. Each retained item has a monotonic work identifier, a concrete componentwise path, an escaped display label, and exactly one state:

1. queued planning or planning;
2. awaiting or foreground confirmation;
3. queued execution or serial execution;
4. refreshing or focused rescanning after a stale plan; or
5. cancellation acknowledgement, rejection, or completion.

The planner and executor use separate bounded command lanes. At most one planner builds an identity-bound plan, while at most one executor performs final revalidation and filesystem mutation. Final revalidation and execution occur in the same executor operation, so no queue turn can open a gap between a successful revalidation and the first mutation. A planner may prepare a non-overlapping target while an executor is active; it never receives mutation authority.

A compacted aggregate directory keeps a verified concrete backing path and identity, so it can enter the same live deletion planner immediately. The planner's no-follow walk, not the bounded display model, reviews a directory's descendants. `Other` and `Shared` summaries remain virtual, noninteractive totals.

Planning, refresh, queueing, execution, and completion remain in the work rail. A ready plan becomes the normal confirmation dialog only when the foreground mode can present it. Accepted consent returns immediately to normal map navigation; the serial worker continues its final checks and mutation in the background.

The primary scanner is breadth-first. While it remains active, entering a visible directory starts a separate focused scan rather than reordering primary tasks. That scan reserves a fixed one-third share of the model budget and traverses serially; after it commits, later primary events below its target are ignored. If the live model cannot free this optional slice, the owner returns to the live map with a notice rather than terminating. An on-demand focused scan can therefore populate the selected directory without making its completed map depend on when it was opened.

## Bounds and Target Conflicts

- `MAX_DELETION_WORK_ITEMS` caps every retained operation, including active work, confirmations, refreshes, and planner cancellation reservations.
- Planner, executor, focused-rescan, and event channels have fixed capacities. Owner-loop submission is nonblocking; a rejected submission restores its item rather than dropping it.
- A new item is rejected when its componentwise target path equals, contains, or is contained by a retained target. This prevents ancestor, descendant, and duplicate operations from racing.
- A cancelled in-flight planner retains its reservation until its acknowledgement arrives. A stale-plan refresh retains its operation until scan completion or cancellation.
- Deletion history has both a byte limit and a fixed report-count cap. Summaries retain only fixed-size counters and an atomic progress value; reports stream directly from bounded resident or authenticated spill storage.

## Exit and Reconciliation

The exit dialog distinguishes no work, cancellable pending work, and active mutation. Pending work can be cancelled before quitting or left running while the reader returns to the map. Active mutation can only be stopped at an entry boundary or awaited; the worker is never detached silently.

A completed report is reconciled through `FileTree::try_apply_deletion_report`. Retained matching nodes are removed, while successfully deleted descendants absent from the bounded model need no fabricated placeholder. During a still-active primary scan, results below a confirmed removed target are ignored so stale scanner events cannot revive it. Board replacement then selects a surviving actionable entry, clears an empty view, or restores the nearest valid folder.

## Consequences

The visible work rail makes bounded background activity and progress explicit without copying plans or reports into frame state. The foreground dialog remains limited to an irreversible consent decision, and report formats retain their existing precise or uncertain semantics. Any change to target overlap rules, queue capacities, serial mutation, no-follow validation, report retention, or exit behavior requires public architecture review.