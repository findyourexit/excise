# Permanent Deletion Contract

## Meaning

Excise permanently removes the confirmed file identities. It does not use a trash folder, a quarantine area, or an undo feature. Linux and Apple systems use a temporary unpredictable name in the same parent directory while one identity is checked and removed. Recursive deletion is not one atomic operation.

## Eligibility

Deletion can be prepared only when all of these conditions hold:

- The selected entry is real, retained in the map, and is not the virtual `Shared` allocation summary.
- Its canonical scan snapshot has a verified concrete backing identity. The displayed direct-child page may still be incomplete.
- The entry is not the scan root, a filesystem, drive, or mount root.
- The platform has a reviewed method for deleting the entry without following links.

The display model only selects the target. The planner creates the authoritative no-follow live review, so approximate hard-link accounting or a partial scan neither authorizes an unsafe deletion nor blocks a valid one.

## Plan Construction

1. Inspect the selected target from the live filesystem and bind its identity, type, size, allocation, and modification state to the displayed target snapshot.
2. For a directory, list its current contents without following links. Record every relative path, identity, type, size, allocation, modification state, and required deletion order.
3. Keep directory-plan records in bounded resident memory, spilling overflow plan and outcome records under the configured temporary-storage limit. Unix uses anonymous files; Windows atomically creates a current-user-only, exclusive, delete-on-close file in the selected target's parent, outside the target.
4. Authenticate every spilled record with a process-private key. Every decoded path must have safe components whose prefix is the selected target before revalidation or execution.
5. Reserve resident and temporary capacity for every planned identity and outcome before confirmation. If either complete plan or report cannot be retained, discard it before confirmation and delete nothing.
6. Check every planned entry again immediately before deletion; reject a directory plan that targets or contains a filesystem or mount root.
7. If pre-consent planning or final whole-plan revalidation finds a change, discard that plan and require a fresh user request. Prior consent is never reused.

## Confirmation

- Files and safe printable directories confirm with `Enter` or `y` in the confirmation dialog.
- Hostile or untypeable names show an escaped full path and identity. They require a generated challenge such as `DELETE K7M4`.
- A session-only reduced mode accepts `Enter` or `y` for all entries except hostile names.
- Reduced mode is visible and is never saved.
- Root and Administrator accounts receive a visible warning. The identity checks remain unchanged.

The planner runs in the background and retains at most four non-overlapping targets. When a plan is ready, its confirmation remains foreground; accepted confirmation returns immediately to map navigation. A single executor serializes final full-plan revalidation and filesystem mutation; every Unix entry is checked again after isolation and immediately before its removal.

## Execution

The deletion worker uses platform file operations that do not follow links. Linux and Apple systems temporarily exchange one directory entry with an unpredictable name in the same parent, verify the isolated entry again immediately before removal, and restore it when its identity changed. A replacement at the original path is never removed. Windows opens the confirmed entry without following a reparse point and applies deletion to that verified handle.

For every planned entry, Excise does the following:

1. Resolve it from the confirmed parent without following links.
2. Bind the operation to the confirmed file identity or Windows file handle.
3. Check the identity, type, size, allocation, and modification state against the review plan.
4. Delete the entry only when every relevant value still matches. Otherwise restore the temporary name, record the exact result, and skip the entry.
5. Record success, a changed identity, a permission or sharing error, a missing entry, a recovery error, or another failure.
6. Finish recovery for that entry before continuing with other planned entries that remain safe.

The working model changes only from confirmed deletion results.

## Interruption

Quitting distinguishes the work that can be discarded from an active filesystem mutation:

- **No work:** Exit through the ordinary preference prompt.
- **Pending plans:** Cancel the bounded pending set and quit, or return to the map and wait.
- **Active deletion:** Stop only after the current entry finishes and record the bounded result, or return to the map and wait.

No key silently detaches a mutation worker or claims that a blocked filesystem operation has stopped.

## Result

Normal completion and soft cancellation report every planned identity as deleted, changed, missing, failed, or unattempted through bounded resident or authenticated outcome-spill storage. Session history has a fixed memory limit and writes directly to the versioned `deletion-history` format. If outcome storage fails after consent, Excise starts no further entries, returns an explicit incomplete result, and schedules a root generation rebuild rather than materializing an unbounded report.

## Required Evidence

- File and directory replacement races
- New-child races
- Symbolic link and Windows junction behavior
- Permission and sharing failures
- Deterministic Windows sharing-violation behavior
- Partial continuation when some entries cannot be deleted
- Elevated and reduced-safeguard modes
- Safe-stop and wait choices with terminal restoration
- Tests of each supported platform deletion method
- Randomized tests for deletion-plan construction
