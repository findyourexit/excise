# Background Task System Decision

**Status:** accepted for deletion work; future task kinds require their own review.

## Problem

Deletion planning, revalidation, and execution can block on the file system. The owner loop must remain the only writer of application and terminal state, while a confirmed operation must be able to continue independently from whichever modal or map view the reader later chooses.

## Decision

The application owns a bounded `DeletionWork` queue separately from `UiMode`. The initial implementation accepts the existing `FileToDelete` contract only; it does not treat aggregate or synthetic model nodes as eligible targets. `UiMode` continues to carry current confirmation and result views until a later interface change consumes the operation summary, but the queue owns the operation's identity, plan, cancellation reservation, and retained shared progress counter.

Each item carries a monotonic work identifier and moves through these states:

1. queued planning
2. planning in the deletion worker
3. awaiting explicit confirmation
4. queued and in-flight identity revalidation
5. queued and in-flight execution
6. completed, cancelled, or rejected

The owner loop submits at most one tagged deletion command to a single-slot worker channel. Submission is nonblocking. If the channel is unexpectedly occupied, the work item is restored and the owner reports an invariant failure instead of blocking or dropping it. The deletion worker processes commands serially, so no two filesystem mutations can overlap.

A successful revalidation transitions its item directly to execution at the head of the queue. No later planning or revalidation command may be dispatched between that result and its execution command. The existing `revalidate_plan_cancellable` and `execute_plan_counted` APIs remain responsible for the live identity check immediately before the mutation and the per-entry checks during it.

## Bounds and Target Conflicts

- `MAX_DELETION_WORK_ITEMS` caps all retained operations, including an active item and one awaiting confirmation.
- The worker command channel has capacity one; the owner queue is the only retained sequence of work.
- Deletion history keeps both its existing byte budget and a fixed report-count cap. A zero-byte or undersized report cannot make history grow without bound.
- A new item is rejected when its componentwise target path equals, contains, or is contained by any retained target. This rejects duplicate, ancestor, and descendant operations before they can race.
- A cancelled planner or revalidator keeps its worker reservation until that worker reports completion. The scheduler cannot issue a replacement command into that interval.

## View Seam

`DeletionWorkSummary` contains only aggregate pending/mutating state and the live count for the serial mutation. It deliberately contains no path text or plan data, so a UI can render a background indicator without owning untrusted paths or copying plans. A later UI worker may move the reader back to normal navigation after confirmation while the owner loop continues to advance the state machine from these summaries.

Exit behavior remains unchanged; deletion work is not yet an exit-policy input.

## Consequences

The current input bindings and modal rendering remain unchanged during this foundation change. Future task types, concurrent planners, different queue capacities, report retention policies, or any change to serial mutation and exit semantics require public architecture review before implementation.
