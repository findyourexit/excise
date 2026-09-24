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

Each session has a 4 GiB temporary-storage budget and an adaptive scan-store budget. The scan store holds private on-disk session data. By default, it uses 75 percent of the free space on the volume that hosts its session directory and leaves the other 25 percent available. Users can set an upper limit with `--scan-store-mib` or choose a different reserve with `--scan-store-reserve-mib`. The scan store holds the scanner journal, ScanStore runs, and page-index data used to publish pages that list a folder's immediate contents. Temporary storage holds overflow directory deletion plans and results. Each budget grows only as its files are written. Page publication consumes raw fact runs one phase at a time and keeps only its compact child-query run, avoiding a second full scan-store reservation after publication. Every owned file reserves capacity before it grows and releases it only after shrinking or cleanup.

If scanner journal work cannot reserve scan-store capacity, the scanner reports an actionable failure and does not call the partial scan exact. If page creation cannot reserve capacity after directory reduction, ScanStore keeps the deterministic directory result as `summary-only`. It never labels the detailed map exact or deletion-ready. If a directory plan cannot reserve enough temporary storage for every reviewed identity and result, it stops before confirmation and deletes nothing. If result storage fails after consent, Excise starts no further changes and reports the work already completed.

## Map Invariants

- Child areas add up to the parent's represented known space.
- `Shared` preserves the allocation total that is not attributable to one direct entry.
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
- Scan-store capacity pressure and recovery
- Platform-specific identity sources
