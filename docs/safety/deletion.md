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

1. Copy every examined descendant identity into the review plan.
2. List the current contents of the live folder separately. Record each relative path, identity, type, size, allocation, modification state, and required deletion order.
3. Require the live list and the reviewed list to match exactly. Check them again immediately before confirmation.
4. If anything changed, discard the plan, scan again, and ask for confirmation again.

Consent covers only the identities in the plan. It never covers a new entry that appears later at the same path.

## Confirmation

- Files require an explicit confirmation dialog.
- Safe printable directories require their exact leaf name by default.
- Hostile or untypeable names show an escaped full path and identity. They require a generated challenge such as `DELETE K7M4`.
- A session-only reduced mode uses a dialog and a separate confirmation action.
- Reduced mode is visible and is never saved.
- Root and Administrator accounts receive a visible warning. The identity checks remain unchanged.

A short dialog animation may run without blocking cancel or back input. The irreversible action stays disabled until the dialog reaches its final stable content. The path, challenge, warnings, and choices then remain unchanged while waiting for consent.

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

Normal completion and soft cancellation report every planned identity as deleted, changed, missing, failed, or unattempted. Session history has a fixed memory limit. It writes directly to the versioned `deletion-history` format and releases retained reports after a successful export.

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
