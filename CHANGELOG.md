# Changelog

All notable Excise changes are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

Excise preserves the historical Diskonaut changelog below. Diskonaut versions and tags are not Excise releases.

## [Unreleased]

### Added

* Added a Zensical documentation site with searchable project guides, source links, and GitHub Pages deployment.

### Fixed

* Confirmed deletion targets remain visibly staged through the final all-entry safety check; progress counters begin only after mutation starts, and removed map entries use an organic randomized dissolve animation.
* A deletion that requires a full map rebuild now retains the prior verified map for navigation while refreshing and disables further deletion until the replacement snapshot publishes.
* Cancelling a retained-map refresh now labels the map as stale and blocks deletion and exact scan export until a verified replacement publishes.
* On Windows, the interactive map no longer stalls after a key release. It previously waited for the next input before redrawing, applying scan results, or showing the quit dialog.
* On macOS, deleting a folder no longer occasionally leaves it behind. When the file system briefly refused the temporary name a deleted file had just vacated, Excise kept an empty file under the deleted file's name, so the folder could not be removed and the deletion reported skipped entries. The temporary entry now moves to a fresh private name and is removed there.
* On Windows, a scan no longer occasionally ends with "Excise could not build a complete folder map" instead of completing. When another program briefly held a scan-store file Excise had just written, the rename that replaces it failed and the scan was abandoned; admission no longer writes that file, so there is nothing left to rename.
* After a scan completes, the interactive map no longer animates the selected entry indefinitely. Its highlight plays one cycle after the last input or change and then settles; previously an idle session kept redrawing the terminal, writing almost 1 MB of output per second.
* On macOS, the scan-store scratch-space budget used the wrong block size and could compute an effective quota far larger than the volume's real free space, so the documented 25 percent reserve was not enforced. It now uses the correct, POSIX-defined unit.
* An out-of-space error while writing scan data now reaches the user the same way the session's own scratch-space limit does, in both the interactive map and `--format json`, instead of a generic "could not build a complete folder map" message.
* The selection cursor could land on a different entry than the one chosen: a background refresh could reassign tile identities once a folder still being measured grew past another entry's size, including right as a scan completed. Once the cursor has been moved, it now stays on that exact entry through every later map refresh, or clears if the entry is genuinely gone, instead of silently landing on whichever entry is currently largest.
* Scanning on a terminal that reads output slower than Excise writes it (an SSH session over a slow link, or a slow terminal emulator) no longer stalls, and no longer stalls far worse under the default animated map than under reduced motion. Frame output now goes through a dedicated writer thread and is paced to what the terminal can absorb, so a slow terminal can no longer block scan ingestion or input handling; terminal restoration still follows every frame byte but now gives up waiting, bounded, rather than hanging if the terminal never reads anything.
* Scanning a large folder is now much faster. Admitting each scanned batch no longer performs a durable filesystem sync (`fsync`/`F_FULLFSYNC` on macOS, `FlushFileBuffers` on Windows) before continuing; that sync, repeated for every admitted batch, dominated scan time on large trees. The private bookkeeping file it protected had no remaining reader, so it is no longer written either.
* Writing a JSON report (`--format json`, with or without `--output`) and the two interactive exports are no longer slow on large trees. Each wrote one `write` system call per pretty-printed token straight to the destination file or the terminal; the writer is now buffered and explicitly flushed, so a write error on the final flush (a full disk, most plausibly) still fails visibly instead of silently truncating the report.
* Closing the terminal, or sending SIGTERM, SIGHUP, or SIGQUIT (on Windows, a console close, break, logoff, or shutdown event), no longer leaves the terminal raw on the alternate screen, ends a headless run without a report, or leaks a scan-store session directory. Excise had no signal handling at all, so the operating system ended the process immediately with none of its cleanup run; any of those is now a confirmed quit that cancels pending plans, stops an active deletion at its next entry boundary, restores the terminal or writes a cancelled report, removes session storage, and exits 130. A second one does not wait for what the first started to settle: it gives the threads that are stopping (the scanner, the deletion planner, the scan-store thread, a history export) one second to end, so that a call stuck in a hung file system cannot hold the exit, and then restores the terminal and exits. It still waits for an active deletion to stop at its next entry boundary, because ending the process inside an entry could leave a target that is neither intact nor gone.
* A folder Excise could not list, and every folder above it up to the root, was reported `complete` with an exact byte range in both the interactive map and `--format json`; only the overall document state, exit code, and summary count carried the uncertainty. The folder's own identity was recorded as fully read by its parent's listing before Excise learned it could not open or list it, and that first record was never corrected afterward. An unreadable folder and every folder above it now report `uncertain` with an open upper bound, keeping the known lower bound, in both places, so the map's header no longer reads `COMPLETE` for them.
* Large scans no longer fail with "Too many open files" at a low descriptor limit, such as the 256 a process started from a macOS terminal gets by default. Every sealed batch of scan data kept its file open until it was read, so open files grew with the size of the tree; a sealed batch now holds no file until it is read, and on Unix Excise also raises its soft descriptor limit toward the hard limit at startup.
* A run that was killed or crashed (`SIGKILL` or a crash) no longer leaves its private scan-store directory (`.excise-scan-*`, in the scratch parent) behind for good. A session can only remove its own directory when it ends cleanly, and nothing removed the others, although the documentation described the scan data as automatically cleaned. Every start now sweeps the scratch parent before it creates its own session. Each session holds a lock on a file in its own directory for as long as it runs, and that file records that Excise made the directory. A start removes a directory only when it is a real, private directory of the current user, holds that record, and its lock is free, so a running session (including another Excise that starts at the same moment) is never touched. It never follows a link or leaves the directory, and it leaves everything else alone, including other users' directories and the directories that earlier versions left behind, which have no such record and can be removed by hand.
* On Unix, a scan-store session directory could be read by other users. It was created with `0755` under the usual umask, so in a shared temporary directory such as `/tmp` any local user could list and read the scan data it held (file and folder names and sizes) while a session ran. It is now created accessible only to the current user (`0700`).
* Folders nested deeper than the system's path length limit (1,024 bytes on macOS, 4,096 on Linux) are now scanned and measured. Everything below that depth was missing from the map and the report, and the scan ended `uncertain` with one unreadable folder although the whole tree could be read, because Excise measured each entry by its full path, which the operating system rejects past that limit. On Unix it now reaches each folder, and measures each entry, through the handle of the folder above it, so the length of a path no longer matters; Windows still measures entries by their paths. A very deep tree takes longer to scan than a flat one of the same size, because every folder is reached from the scan root again.
* On Unix, folders nested deeper than the system's path length limit (1,024 bytes on macOS, 4,096 on Linux) can now be deleted. Excise refused them before changing anything, with "Deletion did not start: the selected item could not be checked", whether the folder itself or only something inside it was that deep, because planning asked whether each folder was a mount point by its full path, which the operating system rejects past that limit. It now asks from the handle of the folder above it and the folder's own name, so the rule that a folder containing a mount root cannot be deleted is unchanged, bind mounts on Linux included, and no longer depends on how long a path is. A refusal before deletion starts now says why when Excise can tell: a path too long or invalid for the system, a mount point or filesystem root in the folder, or a value the file system reports that Excise cannot keep. Excise moves each entry aside before it removes it, by exchanging the entry's name with a placeholder's, and macOS 14 refuses, with "No space left on device", any rename whose source is a folder whose path is longer than the limit, an exchange included. An exchange swaps both names whichever is named first, so Excise now names the placeholder, which is always a file, as the source, and macOS 14 accepts that at any depth. A system that still refuses the rename stops the deletion at the first such entry, which is recorded as failed with the system's reason, every later entry is recorded as not run, and no entry that could not be moved aside is ever removed. A very deep tree takes longer to delete than a flat one of the same size, because every entry is reached from the scan root again.
* On macOS, planning the deletion of a folder could panic when it held a device node with a major number of 128 or more (the system keeps device numbers in a signed 32-bit value), or an entry whose size the file system reported as negative: the library that read each entry's metadata unwrapped the conversion of both. Excise now reads every entry through the checked conversion the scanner uses, so such an entry refuses the deletion with a message naming the value, and nothing is changed.
* Applying a filter no longer ends Excise with a panic (exit code 101) when the name matches something more than one level below the folder being filtered, such as `node_modules` inside each project of a projects folder, whether the filter is applied at the root or inside an opened folder. Reading the filtered page looked up the folder holding a deeper match while the read of the whole subtree was still open, and both reads moved one shared file offset, so the subtree read lost its place, failed with "scan run keys must be strictly sorted", and left the map with no page to draw. Each scan-store read now keeps a position of its own. A filter that still cannot be applied, or a finished map that cannot be reopened, no longer leaves the map without a page: the filter keeps the map and the filter that were in force and reports the error, and the map shows the unavailable-results screen.
* The interactive map stays responsive while a large scan's data is merged and published, and reaches `COMPLETE` about as soon as the headless command would finish. Admitting each scanned batch, merging runs, and publishing the finished scan ran on the loop that reads keys and draws frames, so a key could wait about a quarter of a second as a large scan opened on a slow terminal, the map could stop drawing for more than a third of a second as the scan finished, and a 50,000-entry tree reached `COMPLETE` about twice as late as `--format json` took to scan it. A dedicated scan-store thread now does that work while the loop goes on taking input and drawing: in the same measurements a key waits at most 15 ms, and `COMPLETE` arrives within 13 percent of the headless time. While a large scan's map is being finished, the header shows how far that has got and how long it has taken.
* Exporting the deletion history (`Shift+E`) no longer freezes the map while a long history is written. After a 65,111-entry deletion the export was serialized on the loop that reads keys and draws frames, so the next key waited more than 350 ms. The history is now written on a thread of its own: a notice confirms that the export began, and another names the file once it is written, or says why it was not.
* Deleting an entry from a tree that contains folders no longer scans the whole tree again. Excise builds the map that follows a deletion from the map before it, but that failed whenever the tree had a folder in it, so each deletion that removed its whole target fell back to a full rescan, during which the map was marked as rebuilding and deletion was locked. The map is now updated in place, with the folder that held the entry recorded as the deletion left it, so that folder can be deleted next, provided it holds only the entries the map lists for it: a folder that another process changed since the scan is scanned again, so that it cannot be deleted with entries in it that the map never showed. A deletion that changes what the map says of another entry still scans again: removing one of several hard links to a file, including a link the file gained after the scan, leaves each of the others with one link fewer, and a file that was already gone when Excise reached it may live on under another name. On macOS, where Excise opens nothing it removes and so cannot show that a file or symbolic link left no other link, removing one still scans again; a folder with no file in it is updated in place. On Linux and macOS Excise also no longer makes a temporary hard link beside each file it deletes in order to count the file's links; on Linux it counts them through a reference to the file once the file is gone.
* A deletion that finished while the deletion history was nearly full could lose its report: queued deletions were each allowed to start while the history had room, though together they could not all be kept, so the last report to finish was dropped, and a history export did not contain it. The report of a large folder could also outgrow its place: a plan that spilled its entries to temporary storage was charged for the entries on disk as well as for what it held in memory, so the folder's report did not fit once the folder was gone. The history now counts the deletions still running with the reports it holds, budgets each plan from what the plans already running leave, charges a report for what its plan held in memory (the report of a large folder is kept in temporary storage until it is exported), and says it is full instead of starting a deletion whose report it could not keep.
* The cursor no longer stays on a small entry the scan happened to list first. Until you move or click, it now goes to the largest entry of the folder you are in when the scan completes, and it still does not move while the scan runs; a cursor you moved or clicked stays on your entry. The map kept the entry it selected on the scan's first page selected by name for as long as that entry was listed, so a small file found early kept the cursor after much larger entries appeared, and Enter on it right after `COMPLETE` did nothing.
* On Windows, closing the console occasionally ended Excise with a status of Windows' own, an `NTSTATUS` such as `0xC000013A` (`STATUS_CONTROL_C_EXIT`), instead of 130, in the interactive map and in a headless run alike. The quit itself had finished, so the terminal was restored and the session storage removed, but a parent process or script that reads the exit status saw the wrong value. The console handler returned to Windows as soon as the quit finished, and Windows ends the process the moment a close handler returns, so that termination raced the program's own exit with 130. The handler now stays blocked once the quit has finished, each one if several events overlap or arrive late, so the program's own exit ends the process with 130. A quit that has not finished after four seconds is still given up on: for a close, Windows then ends the process with its own status.

## [1.3.0] - 2026-09-24

### Changed

* Replaced the old in-memory scan state and focused staging paths with a session-local `ScanStore`. Completed maps and reports read immutable pages that list direct children. No file system child is collapsed into an undeletable summary.
* Unified scanning, result reduction, refreshes, and deletion work under one session coordinator. The interactive header now reports combined active and queued background work without exposing queue internals.
* Retained only the shared-allocation summary, which cannot be selected. Removed old aggregate and compatibility paths from the map, reports, schemas, and user guidance.
* Replaced the cyclic theme toggle with a keyboard preview picker. `Enter` persists the selected theme for later TUI sessions, while `Esc` restores the prior theme.
* Made deletion confirmation foreground-only and moved planning and execution into the background. Accepted confirmation returns immediately to map navigation. Explicit exit choices cover pending plans and active mutations.
* Reworked terminal presentation with padded pane titles, a raised outline for the selected entry, full-color modal borders when available, an honest scan field, and a fixed absolute color scale for the storage map.
* Added hosted benchmark evidence for a one-million-file scan and bounded merge behavior, including logical I/O, CPU, retention, and temporary-overlap metrics.

### Fixed

* When scan storage reaches capacity, Excise now preserves an explicit incomplete or deterministic `summary-only` outcome instead of claiming a complete detailed map or deletion-ready inventory.
* Scan and deletion refreshes no longer accept stale work from retired scan versions. Gaps between scan versions are handled safely. The independent deletion recheck remains the final safety check.
* Terminal rendering keeps truecolour foreground and background commands separate, preventing parsers that only read foreground sequences from displaying colour-control tails as text.

## [1.2.4] - 2026-09-12

### Changed

* Documented X-CMD's `x eget use findyourexit/excise` command for installing the pre-built GitHub Release binaries.
* Isolated private scanner queue/spill mechanics and the Windows FFI boundary to narrow the safety-audit surface without changing user-visible behavior.
* Updated runtime, development, fuzzing, and CI dependencies, notably `crossbeam-channel` 0.5.17, `jsonschema` 0.53.0, `redb` 4.2.0, `sha2` 0.11.0, `tachyonfx` 0.25.2, and `toml` 1.1.5.
* CI now retains reproducible benchmark evidence and its execution context for 90 days.

### Fixed

* Corrected the supported Rust compiler contract to Rust 1.98 or later, matching the locked dependency set and CI.
* Local and hosted fuzz verification now share a pinned nightly toolchain. Pull requests exercise bounded directory-deletion plans across hostile names, hard links, replacements, late entries, and temporary-storage spills.

## [1.2.3] - 2026-09-04

### Fixed

* Directory-deletion-plan fuzz coverage now accepts the intended bounded temporary-storage spill path and replays its regression case.

## [1.2.2] - 2026-09-04

### Fixed

* Scanner task, identity, and directory-deletion plan and result spill files now share a fixed per-session temporary-storage limit. Deletion plan and outcome records use a process-private key. On Windows, they are held in atomically created exclusive files outside the selected target. Capacity or storage-read failures produce a safe incomplete outcome instead of an unbounded report. An unretainable directory plan stops before confirmation without deleting an entry.
* Scanner directory-completion events that arrive after model compaction now safely no-op instead of terminating a long root scan with an invalid-model-path error.
* Full-system scans now release an exhausted private identity spill database and continue with explicit unknown physical-allocation and reclaimability bounds, instead of terminating after the bounded temporary-storage limit is reached.
* Full-system scans no longer fail when compaction aggregates an unreadable entry at the configured model-memory limit. The aggregate reuses an already billed summary slot instead of requiring a new allocation.
* Repeated hard-link observations are coalesced through compaction and remapping, keeping participant storage bounded while preserving exact link accounting.
* Model-memory compaction preserves collapsed subtree and ancestor metrics incrementally instead of rebuilding global metrics for every cap-limited insertion retry.

## [1.2.1] - 2026-09-02

### Fixed

* The tagged Nix flake now derives its package version from `Cargo.toml`. The immutable `v1.2.0` flake built the correct executable but exposed `1.0.0`. This corrective release exposes `1.2.1`.

## [1.2.0] - 2026-09-02

### Changed

* Simplified the ordinary directory-deletion review flow to use a single-key confirmation. Directories with deceptive names still require their generated safety challenge.
* Directory deletion plans can exceed the retained file and link history budget. The planning dialog estimates entries, and deletion displays live per-identity progress while execution runs.
* The default scanner worker count reserves one available processor for user interaction and never exceeds eight workers.

### Fixed

* Treemap navigation stays responsive during active scans. Folder drills render before queued scanner work, and scanner model updates wait while treemap geometry is moving instead of skipping navigation frames.
* Tree compaction continues at the model memory limit after deep navigation returns to the root by reusing an untracked summary slot from the subtree being removed.

## [1.1.1] - 2026-09-01

### Fixed

* Directory deletion now works regardless of whether the first scan is still running or directory contents were evicted from the bounded in-memory model because of memory pressure. The previous release required a complete in-memory snapshot of every file in the subtree before allowing a deletion plan. This blocked deletion when descendants were still scanning or had been aggregated, even when the directory itself was complete. The planner now builds the deletion plan from a fresh live file-system walk, matching Diskonaut's approach. File and link deletion retains per-entry identity snapshots for targeted safety.

### Changed

* Added `cargo create-release-tag <version> <sha> <candidate-run-id>` as a `cargo xtask` subcommand. It creates an annotated release tag in the format required by the release workflow, embedding `candidate-run-id: <id>` in the tag message. The `docs/releasing.md` runbook now documents the annotated-tag creation step between bundle verification and the push that triggers the release workflow.

## [1.1.0] - 2026-09-01

### Changed

* Deletion is no longer locked until the first scan completes. Entries that have been fully examined are now deletable while scanning continues. Previously, pressing `Backspace` during a scan showed a blocking warning. It now proceeds immediately when the selected entry is complete and gives a clear error when it is not.

* `Enter` is now the primary confirmation key for file deletions and directory deletions under `--disable-delete-confirmation`. Previously these required typing `y`. Both keys work. `Enter` is now displayed first in the confirmation dialog.

* Pressing `Enter` during the identity plan build phase pre-arms confirmation for single-key challenges. When the plan completes, deletion begins immediately without a separate confirm step. This reduces the file deletion flow to `Backspace` → `Enter`, matching the speed users expect from interactive storage navigators.

* Updated the warning shown when deletion is attempted during a rescan. The message previously implied that the first scan was also a barrier. It now correctly states that deletion is locked only during rescanning.

## [1.0.2] - 2026-09-01

### Changed

* Reduced the number of filesystem stat calls during directory traversal. The root-path validity check was removed from the inner per-entry loop, a redundant cap-primitives stat that preceded the standard-library stat in each entry's processing was removed, and three sequential directory task validations at the close of each directory scan were consolidated to one.

* Eliminated JSON serialization overhead in the identity store. File identity records are now keyed directly by `FileId` in the in-memory store rather than by their JSON-encoded bytes, removing one allocation per scanned file during identity lookup.

* Replaced the blocking disk enumeration in the mount-root check with two direct filesystem stat calls. The previous implementation called `sysinfo::Disks::new_with_refreshed_list`, which on macOS queries every mounted volume for its available capacity via Apple's storage framework. That call can block indefinitely when a volume is in certain APFS snapshot states. The new implementation compares the device identifiers of a path and its parent using `lstat`, which is a direct kernel call with no involvement from CoreFoundation or StorageKit.

* Arena nodes are now stored directly inside the arena vector rather than behind individual heap allocations. Each `Box<Node>` was a separate small allocation. Inlining the node data reduces allocator pressure and improves cache locality when traversing large trees.

## [1.0.1] - 2026-08-28

### Changed

* Clarified the first-run path in the README and supporting documentation. The simplest `excise` command now leads the usage guidance, while detailed scan examples remain in the getting-started guide.

* Reworded the safety, support, configuration, architecture, and release documentation in plain English without changing product behavior.

* Removed the Developer Certificate of Origin sign-off requirement for contributors. No contributor license agreement or copyright assignment has ever been required. Historical authorship records are unchanged.

## [1.0.0] - 2026-08-27

### Changed

* Promoted the command-line tool, configuration, and versioned JSON report contracts to the first stable `1.0.0` release, with permanent deletion and explicit accounting and support limitations.

* Defined the stable support matrix. x86_64 Linux, AArch64 macOS, and x86_64 Windows are fully supported by native evidence. The other three published archives remain build-only and best effort with documented file-system limitations.

* Tightened release verification so Cargo package checks reject dirty release input, binary-level contract smoke tests run in `cargo verify`, and the v1 contract decision record names the current lead-maintainer release authority.

## [0.3.0] - 2026-08-27

### Changed

* **Breaking library interface:** removed the provisional public Rust module exports from the supported surface. The command-line tool and documented configuration and report contracts are now the compatibility boundary. Repository randomized-input testing and benchmark access use explicit feature gates.
* Added the v1.0.0 release-readiness gate covering public contracts, compatibility, support classification, safety evidence, and exact-commit publication.
* Established the conservative v1 baseline. Configuration versions other than `1` are rejected. Table output is human-facing rather than machine-stable. Only targets tested on the actual system qualify for full support.

## [0.2.0] - 2026-08-27

### Added

* Added the `cargo demo` alias and its `xtask demo` command. `xtask demo` validates `tapes/demo.tape`, stages its rendering under `assets/demo-main.*.gif`, resamples the tape's 24 fps GIF to 20 fps while rebuilding a non-dithered 64-colour palette and applying lossy GIF quantisation, and atomically promotes `assets/demo-main.gif` only after the size gate passes. The hosted demo workflow uses the same pipeline. The tape and current-main README hero asset were refreshed.

* Added the public `geometry::MapOverflow` and `TreeMap::overflow` interfaces. They summarize entries omitted from the final map viewport, retaining their count, byte total, and lower-bound uncertainty even when the layout cannot draw an overflow region.

### Changed

* Reworked the terminal interface presentation with Catppuccin-inspired pane chrome, dense half-block treemap surfaces, animated focus borders, and contextual command help.
* Layered dialogs over a scrim so a modal always separates from the map behind it, including on terminals without colour.
* Rendered the treemap with shading density instead of colour whenever colour is unavailable (`NO_COLOR` or a monochrome theme), keeping entries distinguishable in a two-colour terminal.
* Labelled the empty folder state in the narrow list layout, which previously drew nothing at all.
* Centred each map entry's label on both axes. It previously hung from the left edge of the entry.
* Marked the map cursor with brightness instead of an animated outline: the selected entry lifts out of the colour band while every other entry sinks toward the canvas, keeping its own hue. In map layout, pane borders remain the only animated focus signal.
* Made directed map-layout navigation read as one zoom. Opening an entry grows its contents out of the chosen rectangle. On drill-out, departing contents contract into the pivot while the incoming parent layout grows out of it. Replaced entries stay on screen while they recede instead of blinking away. Pane chrome and other interface elements do not participate in the transition.
* Kept a cursor on the map whenever it has entries to hold one: it arms on the largest entry, survives a folder swap or a streaming scan refresh, and holds at the edges rather than clearing, so the inspector always describes something.
* Selected the folder just left when stepping out, instead of restoring whichever index the previous layout happened to use.
* Seated the inspector beside the map as soon as the terminal can afford both panes, and stacked it below when there is enough vertical room for both. On shorter supported terminals the map keeps the full body. It previously vanished entirely below 120 columns.
* Ran the pane title chip through the same colour cycle as the border it sits in, so the label reads as part of the frame rather than a plaque bolted onto it.
* Coloured ordinary map entries by size on a blue-to-red heat ramp fitted to comparable entries in the folder on screen. The relative weight of what is in front of the reader is legible before a single label is read. When comparable sizes produce a distinguishable log-space range, the largest is red and the smallest blue. Equal or near-equal sizes that collapse at the ramp's rendering precision rest mid-ramp. Semantic colours for uncertain, shared, and aggregated entries never participate in the ramp. The monochrome shading fallback is untouched.
* Showed entries omitted from the final map viewport as a `MapOverflow` stipple anchored in its own region rather than scattering dots over a drawn entry. Where the region has enough width, it names how many entries it stands for. With enough width and a second drawable label row, it also reports their weight.
* Labelled a map entry too narrow to carry both figures with its size rather than its share of the folder. A 4 KiB file beside a megabyte of neighbours rounded to `0%`, which reads as nothing worth looking at. A size always carries a unit and cannot round away.
* Fitted the command line under the map to the terminal by dropping whole commands, longest-tail first. At every adaptive footer tier that can advertise an Enter action, it retains `Enter open/rescan`. Tiers too narrow to fit that hint omit it rather than shortening it. A narrow terminal previously advertised a bare `delete` with no key attached and dropped `/ filter` before commands it had room for. Every advertised command now names the key it means and keeps the widest tier's order.
* Retired the whole-screen colour washes that fired on navigation, focus, state changes, scan progress, and aggregation. Effects are now one-shot acknowledgements painted over the header band alone. Completion, errors, and deletion results are still visible without fading across the map while it is being read.
* Kept retained-entry accounting incremental in the scanner. A directory sitting at the retained-child cap previously swept its children twice for every entry delivered to it, once to count identities and once to find an eviction candidate. Wide directories no longer slow down as they fill. The retained set is unchanged.
* **Breaking library interface:** `animation::EXCEPTIONAL_MOTION`, `animation::EffectKey::{Navigation, Focus, StateChange, ScanProgress, Aggregation}`, and `AnimationScheduler::{schedule_navigation, schedule_focus, schedule_state_change, schedule_scan_progress, schedule_aggregation}` have been removed. Those retired effects have no scheduler replacement. Use `ROUTINE_MOTION` for resize or streaming layout motion, `NAVIGATION_MOTION` for drills, and the retained `schedule_completion`, `schedule_error`, and `schedule_deletion_result` only for header acknowledgements. `AnimationScheduler::process` is now `process(now, buffer, area, surface)`. `area` remains the header-band paint target, while the full-terminal `surface: Rect` alone selects the cadence tier. In `geometry`, `Tile::{y, height}` changed from `u16` terminal rows to `u32` half-rows. `Tile::get_horizontal_overlap_with` now returns `u32` half-rows. Use `Tile::{top_row, bottom_row, rows}` for `u32` terminal-row values, pass a `u32` terminal row to `covers_row`, and use `HALF_ROWS_PER_CELL` to convert. `TreeMap::unrenderable_tile_coordinates: Option<(u16, u32)>` now uses `(terminal_column, half_row)` rather than `(terminal_column, terminal_row)`.

### Fixed

* Animated map transitions across every frame they need instead of one frame per keypress. The layout tween had no clock of its own, so drilling into a folder left the map frozen mid-morph until the next unrelated event.
* Held the fast animation cadence while the map is moving, so a large terminal no longer samples a 160 ms transition two or three times.
* Stopped an idle session from consuming a freshly scheduled effect: the first frame after a pause charged the whole idle wait to the new effect, which retired it before it was ever drawn.
* Re-aimed the map transition at each streaming scan update instead of restarting its clock, which is what made a map judder while entries were still arriving.
* Stopped pressing Enter on a file from recording navigation history and arming a transition, which the next unrelated refresh then played back as a movement nobody asked for.
* Stopped the map calling a directory empty when it holds entries omitted from the final viewport. A folder of several thousand small files laid out to nothing and reported "Folder is empty". It now retains its `MapOverflow` summary instead.
* Stopped two entries writing their names into the same cells while the map is moving. Mid-transition each entry is interpolated toward its own target, so neighbours briefly pass through each other. Both names are centred in their own entry, and the later write landed inside the earlier name and left a word belonging to neither. The entry on top keeps its name and the one underneath goes quiet until the layout settles.

## [0.1.2] - 2026-08-25

### Fixed

* Treat conflicting hard-link observations as unknown and propagate conservative reclaimable bounds.
* Accept invalidated deletion plans in randomized-input validation without classifying safe rejections as crashes.

## [0.1.1] - 2026-08-24

### Added

* Published the first early-testing release through GitHub archives, crates.io, the first-party Homebrew Tap, cargo-binstall metadata, and the tagged Nix flake. Its public library surface and destructive behavior remain provisional until 1.0.

## [0.1.0] - 2026-08-21

### Added

* Added bounded, iterative scanning with explicit exclusions, filesystem boundaries, uncertainty, cancellation, and secure identity spill.
* Added identity-unique allocated-byte accounting, hard-link deduplication, reclaimable bounds, and explicit `Shared` and `Other` nodes.
* Added identity-planned, no-follow permanent deletion with independent enumeration, per-entry revalidation, hostile-name challenges, partial reports, and soft or hard cancellation.
* Added versioned TOML configuration, environment and command-line layering, fifteen built-in themes, reduced motion, ASCII output, mouse support, and Vim, Emacs, or custom movement.
* Added noninteractive table and JSON reports, lossless native-path encoding, published JSON Schemas, generated man pages, and shell completions.
* Added Linux, macOS, Windows, Nix, packaging, terminal, snapshot, randomized-input, benchmark, dependency-rule, software bill of materials, checksum, and provenance verification.

### Changed

* Reintroduced the project as Excise, an independent successor that retains Diskonaut's commit history and contributor attribution.
* Replaced the legacy actor model with one synchronous owner loop and bounded scanner and deletion worker protocols.
* Rebuilt the terminal interface around Ratatui with responsive treemap and list layouts, semantic status states, and guarded destructive interactions.

## Diskonaut history

### Unreleased upstream changes

* Only show "Small Files" legend when there are small files on screen (https://github.com/imsnif/diskonaut/pull/75) - [@pjsier](https://github.com/pjsier)

## [0.11.0] - 2020-09-23

### Added
* Windows support (https://github.com/imsnif/diskonaut/pull/74) - [@pm100](https://github.com/pm100)

## [0.10.0] - 2020-09-11

### Added
* Add `--disable-delete-confirmation` flag to not immediately delete files without a prompt (https://github.com/imsnif/diskonaut/pull/71) - [@markafarrell](https://github.com/markafarrell)

## [0.9.0] - 2020-07-12

### Added
* Add `--apparent-size` flag to show actual file size rather than file size on disk (https://github.com/imsnif/diskonaut/pull/66) - [@imsnif](https://github.com/imsnif)

## [0.8.0] - 2020-07-09

### Fixed
* Change delete key to BACKSPACE for cross platform support (https://github.com/imsnif/diskonaut/pull/64) - [@maxheyer](https://github.com/maxheyer)
* Do not crash with extremely large files/folders (https://github.com/imsnif/diskonaut/pull/63) - [@Freaky](https://github.com/Freaky)

## [0.7.0] - 2020-07-04

### Added
* Show warning when trying to delete while still scanning (https://github.com/imsnif/diskonaut/pull/60) - [@mhdmhsni](https://github.com/mhdmhsni)
* Add ability to zoom in and out (eg. to see small files) (https://github.com/imsnif/diskonaut/pull/61) - [@imsnif](https://github.com/imsnif)

## [0.6.0] - 2020-07-03

### Added
* Add a visual indication when running as root (https://github.com/imsnif/diskonaut/pull/57) - [@c3st7n](https://github.com/c3st7n)
* Change delete key to DELETE (https://github.com/imsnif/diskonaut/pull/59) - [@maxheyer](https://github.com/maxheyer)

## [0.5.0] - 2020-06-27

### Added
* Add an "Are you sure you want to quit?" modal (https://github.com/imsnif/diskonaut/pull/44) - [@mhdmhsni](https://github.com/mhdmhsni)

### Fixed
* Fix some small_files rendering edge-cases (https://github.com/imsnif/diskonaut/pull/55) - [@imsnif](https://github.com/imsnif)

## [0.4.0] - 2020-06-26

### Added
* Support emacs keybindings (https://github.com/imsnif/diskonaut/pull/40) - [@redzic](https://github.com/redzic)
* Make enter select largest folder if nothing is selected (https://github.com/imsnif/diskonaut/pull/45) - [@redzic](https://github.com/redzic)
* Keep track of tile selection in previous folder (https://github.com/imsnif/diskonaut/pull/53) - [@therealprof](https://github.com/therealprof)

### Fixed
* Do not scan in parallel when running tests (https://github.com/imsnif/diskonaut/pull/43) - [@redzic](https://github.com/redzic)
* Prevent crashes for multibyte characters on grid (https://github.com/imsnif/diskonaut/pull/51) - [@goto-bus-stop](https://github.com/goto-bus-stop)
* Show quit shortcut in legend (https://github.com/imsnif/diskonaut/pull/46) - [@olehs0](https://github.com/olehs0)

## [0.3.0] - 2020-06-21

### Fixed
* Remove unneeded dev dependency (https://github.com/imsnif/diskonaut/pull/35) - [@ignatenkobrain](https://github.com/ignatenkobrain)
* Improve scanning speed (https://github.com/imsnif/diskonaut/pull/38) - [@imsnif](https://github.com/imsnif)
* Refactor movement methods (https://github.com/imsnif/diskonaut/pull/31) - [@phimuemue](https://github.com/phimuemue)

## [0.2.0] - 2020-06-18

### Fixed
* Cross platform file size calculation (https://github.com/imsnif/diskonaut/pull/28) - [@Freaky](https://github.com/Freaky)
* Bumped insta dependency to 0.16.0, bumped cargo-insta dependency to 0.16.0 (https://github.com/imsnif/diskonaut/pull/25) - [@tim77](https://github.com/tim77)
* Bumped tui dependency to 0.9 (https://github.com/imsnif/diskonaut/pull/30) - [@silwol](https://github.com/silwol)

## [0.1.0] - 2020-06-17

Initial release with all the things.
