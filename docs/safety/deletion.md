# Permanent deletion contract

## Meaning

Excise permanently removes the confirmed filesystem identities. It does not use trash, a retained quarantine, or undo in 1.0. Linux and Apple adapters use a transient unpredictable same-parent name only while one identity is verified and mutated. Recursive deletion is not globally atomic.

## Eligibility

Deletion can be armed only when:

- the selected node is real, not `Other` or `Shared`;
- the selected subtree is materialized and complete;
- the entry is not the scan root, filesystem root, or drive root;
- the platform has an audited handle-relative adapter;
- no unresolved scan/identity state prevents a trustworthy plan.

## Plan construction

1. Copy every materialized scan-model descendant identity into the review contract.
2. Independently enumerate the live subtree with bounded memory and directory-handle use, recording native relative path, identity, type, allocated/logical size, modification state, and required ordering.
3. Require an exact path-and-snapshot set match, then revalidate again immediately before confirmation.
4. If anything changed, discard the plan, rescan, and re-prompt.

Consent covers only planned identities. It never covers future path occupants or new children.

## Confirmation

- Files: explicit modal confirmation.
- Safe printable directories: type exact leaf name by default.
- Hostile/untypeable names: show escaped full path plus identity and require a generated challenge such as `DELETE K7M4`.
- Session-only reduced mode: modal plus a distinct confirm action.
- Reduced mode is visibly active and never persisted.
- Root/Administrator is visibly warned; identity controls remain unchanged.

A brief modal-entry effect may run without blocking cancel/back input. The irreversible confirmation action remains disabled until the content reaches its stable final buffer; path, challenge, warnings, and choices then remain static while awaiting consent.

## Execution

The deletion worker uses no-follow, capability-relative APIs. Linux and Apple atomically exchange one entry with an unpredictable same-parent placeholder before validating and removing the isolated name; replacement occupants at the original path are never removed. Windows opens the entry no-follow with delete access and applies disposition to that verified handle. For every planned entry:

1. Resolve relative to its confirmed parent capability without following links.
2. Bind the mutation to one namespace-isolated Unix identity or one Windows file handle.
3. Revalidate identity, type, size, allocation, and modification state relevant to the confirmed plan.
4. Delete only if every relevant field still matches; otherwise restore the namespace, record the exact outcome, and skip it.
5. Record success, changed identity, permission/sharing error, disappearance, namespace-recovery error, or other failure.
6. Complete recovery for that entry, then continue independently safe planned siblings.

Model updates are derived only from confirmed results.

## Interruption

Quit during mutation offers:

- **Soft cancel:** stop between entries and return a precise partial report (exit class 3 in headless contexts).
- **Hard cancel:** from either the interruption prompt or a pending soft stop, detach blocked worker calls, restore the terminal immediately, and return an imprecise exit-130 outcome. The worker starts no further entry after the active call returns.
- **Back:** continue deletion before a soft stop is committed.

A first supported interrupt opens the precise soft-cancel choice. `h` or a second `Ctrl-C` remains an explicit hard escape hatch while the active filesystem call is pending.

## Result

Normal completion and soft cancellation report every planned identity as deleted, changed, missing, failed, or unattempted. The in-memory session history has a fixed process-memory budget, streams directly to the versioned `deletion-history` schema, and releases retained reports after a successful export.

## Required evidence

- file↔directory replacement races;
- new-child races;
- symlink and Windows junction behavior;
- permission and sharing failures;
- partial best-effort continuation;
- elevated and reduced-guard modes;
- hostile name confirmation;
- soft/hard cancellation and terminal restoration;
- native Tier-1 adapter tests;
- deletion-plan fuzzing.
