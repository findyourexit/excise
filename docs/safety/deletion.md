# Permanent Deletion Contract

## Meaning

Excise permanently removes the confirmed file identities. It does not use a trash folder, a quarantine area, or an undo feature. Linux and Apple systems use a temporary unpredictable name in the same parent directory while one identity is checked and removed. Recursive deletion is not one atomic operation.

## Eligibility

Deletion can be prepared only when all of these conditions hold:

- The selected entry is real and is not `Other` or `Shared`.
- Excise has fully examined the selected folder and its descendants.
- The entry is not the scan root, a file system root, or a drive root.
- The platform has a reviewed method for deleting the entry without following links.
- No unresolved scan or identity problem makes the plan untrustworthy.

## Plan Construction

1. List the current contents of the live folder. Record every relative path, identity, type, size, allocation, modification state, and required deletion order.
2. Keep directory-plan records in bounded resident memory, spilling overflow plan and outcome records under the configured temporary-storage limit. Unix uses anonymous files; Windows atomically creates a current-user-only, exclusive, delete-on-close file in the selected target's parent, outside the target.
3. Authenticate every spilled record with a process-private key. Every decoded path must have safe components whose prefix is the selected target before revalidation or execution.
4. Reserve resident and temporary capacity for every planned identity and outcome before confirmation. If either complete plan or report cannot be retained, discard it before confirmation and delete nothing.
5. Require the live list and the reviewed list to match exactly. Check them again immediately before confirmation.
6. If anything changed, discard the plan, scan again, and ask for confirmation again.

## Confirmation

- Files and safe printable directories confirm with `Enter` or `y` in the confirmation dialog.
- Hostile or untypeable names show an escaped full path and identity. They require a generated challenge such as `DELETE K7M4`.
- A session-only reduced mode accepts `Enter` or `y` for all entries except hostile names.
- Reduced mode is visible and is never saved.
- Root and Administrator accounts receive a visible warning. The identity checks remain unchanged.

While the identity plan is being built, pressing `Enter` pre-arms confirmation for single-key challenges (files, safe printable directories, and reduced-guardrail entries). When the plan completes with a single-key challenge, execution begins immediately without showing the separate confirm dialog. The irreversible action does not start until the plan is complete and the pre-arming key has been given. For generated challenges, the confirm dialog is always shown and the user must type the required input before execution begins.

## Execution

The deletion worker uses platform file operations that do not follow links. Linux and Apple systems temporarily exchange one directory entry with an unpredictable name in the same parent before checking and removing the isolated entry. A replacement at the original path is never removed. Windows opens the confirmed entry without following a reparse point and applies deletion to that verified handle.

For every planned entry, Excise does the following:

1. Resolve it from the confirmed parent without following links.
2. Bind the operation to the confirmed file identity or Windows file handle.
3. Check the identity, type, size, allocation, and modification state against the review plan.
4. Delete the entry only when every relevant value still matches. Otherwise restore the temporary name, record the exact result, and skip the entry.
5. Record success, a changed identity, a permission or sharing error, a missing entry, a recovery error, or another failure.
6. Finish recovery for that entry before continuing with other planned entries that remain safe.

The working model changes only from confirmed deletion results.

## Interruption

Quitting during deletion offers these choices:

- **Soft cancel:** Stop between entries and return a precise partial report. The noninteractive exit code is 3.
- **Forced cancel:** From the interruption prompt or a pending soft stop, release blocked operations, restore the terminal immediately, and return an imprecise exit code of 130. The worker starts no new entry after the active operation returns.
- **Back:** Continue the deletion before a soft stop is committed.

The first supported interrupt opens the precise soft-cancel choice. Pressing `h` or pressing `Ctrl-C` again remains an explicit forced escape while a file system call is pending.

## Result

Normal completion and soft cancellation report every planned identity as deleted, changed, missing, failed, or unattempted through bounded resident or authenticated outcome-spill storage. Session history has a fixed memory limit and writes directly to the versioned `deletion-history` format. If outcome storage fails after consent, Excise starts no further entries, returns an explicit incomplete result, and requires a focused rescan rather than materializing an unbounded report.

## Required Evidence

- File and directory replacement races
- New-child races
- Symbolic link and Windows junction behavior
- Permission and sharing failures
- Deterministic Windows sharing-violation behavior
- Partial continuation when some entries cannot be deleted
- Elevated and reduced-safeguard modes
- Hostile-name confirmation
- Soft and forced cancellation with terminal restoration
- Tests of each supported platform deletion method
- Randomized tests for deletion-plan construction
