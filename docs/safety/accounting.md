# Space Accounting Contract

## Main Measure

Excise reports allocated space counted once for each file identity.

- A regular file is counted once even when it has more than one name.
- The space used by a symbolic link or reparse object is counted when the platform provides it.
- Directory metadata space is excluded for consistent behavior across platforms.
- File length remains a separate measure.
- Unknown allocated space is never replaced with file length.

## Files With More Than One Name

Track each observed identity, its allocated space, its declared link count when available, and the paths that point to it.

- Count each identity once within the scan scope.
- Keep every path visible, but give later paths no additional unique space.
- Show space referenced by several child folders under a synthetic `Shared` entry at the lowest observed common parent.
- Treat `Shared` as information only. It is never a deletion target.

## Space That Deletion Can Reclaim

For each real folder, report the following values:

- Known space counted once per identity
- A conservative lower bound for space that deletion can reclaim
- An upper bound when link observations make one possible
- Unknown space and the number of unknown files or folders

A link that may exist outside the scan scope prevents an unjustified exact total.

## Unknown Data

A failed space or metadata query contributes unknown space. An unreadable folder contributes an unknown number of descendants. Parent folders keep a lower bound and show the uncertainty.

User exclusions and one-file-system boundaries define the scan scope. Excise reports those boundaries separately instead of calling them read failures.

Configured exclusions and foreign file-system boundaries remain visible as zero-byte records with a reason. Excise-owned session and scanner paths are different. Excise omits them before they enter the working model or reports, and it matches only their exact active paths. User files with similar names are scanned normally.

## Shared Physical Storage

Version 1.0 counts file identities rather than physical storage blocks. Copy-on-write files, clones, transparent file-system deduplication, compression, and shared physical storage can therefore cause an overcount. The reports and support documentation disclose this limit.

## Memory & Temporary Storage

Each session has one fixed temporary-storage budget, 512 MiB by default, shared by queued scanner directory tasks, private identity spill files, and overflow directory deletion-plan and outcome records. Every owned file reserves capacity before it grows and releases it only after shrinking or cleanup. Unix deletion records use anonymous files; Windows atomically creates them in the selected target's parent with a current-user-only DACL, exclusive sharing, and delete-on-close, so the target cannot contain them. Plan and outcome records also carry a process-private authentication tag so a named file cannot silently change consent or reporting.

If queued scanner work cannot reserve capacity, the scanner reports an actionable failure and does not complete that partial scan as exact. If identity persistence cannot reserve capacity, it releases the private database and continues with unknown physical-allocation and reclaimability bounds; the resulting uncertain model cannot authorize permanent deletion. If a directory plan cannot reserve enough capacity for every reviewed identity and outcome, it stops before confirmation and deletes no entry. A post-consent result-storage failure stops new mutations and returns an explicit incomplete result for focused rescan. Capacity failures neither drop queued work silently nor leave owned temporary files outside the session accounting.

## Map Invariants

- Child areas add up to the parent's represented known space.
- `Shared` and `Other` preserve additive totals.
- Unknown space is shown outside any falsely exact area.
- Map geometry is finite, repeatable, inside its bounds, and non-overlapping.
- Animation moves between valid old and new geometry without changing the underlying numbers.

## Required Fixtures

- Several names for one file in a directory
- Names for one file split across sibling and deep folders
- Names for one file outside the selected folder and outside the scan scope
- Sparse and compressed files
- Missing allocated-space metadata
- Inaccessible directories
- Zero and maximum-size values
- Aggregation and pressure on temporary identity storage
- Platform-specific identity sources
