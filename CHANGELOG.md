# Changelog

All notable Excise changes are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

Excise preserves the historical Diskonaut changelog below. Diskonaut versions and tags are not Excise releases.

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

* Directory deletion now works correctly regardless of whether the initial scan is still running or whether directory contents were evicted from the bounded in-memory model due to memory pressure. The previous release required a complete in-memory snapshot of every file in the subtree before allowing a deletion plan to proceed; this blocked deletion of any directory whose descendants were still scanning or had been aggregated, even when the directory's own node was fully scanned. The planning worker now builds the deletion plan from its own fresh live filesystem walk, matching the approach used by Diskonaut. File and link deletion retains per-entry identity snapshots for targeted single-entry safety.

### Changed

* Added `cargo create-release-tag <version> <sha> <candidate-run-id>` as a `cargo xtask` subcommand. It creates an annotated release tag in the format required by the release workflow, embedding `candidate-run-id: <id>` in the tag message. The `docs/releasing.md` runbook now documents the annotated-tag creation step between bundle verification and the push that triggers the release workflow.

## [1.1.0] - 2026-09-01

### Changed

* Deletion is no longer locked until the initial scan completes. Entries that have been fully examined are now deletable while scanning continues. Previously, pressing `Backspace` during a scan showed a blocking warning; now it proceeds immediately when the selected entry is complete and is refused with a clear error when it is not.

* `Enter` is now the primary confirmation key for file deletions and for directory deletions under `--disable-delete-confirmation`. Previously these required typing `y`. Both keys work; `Enter` is now displayed first in the confirm dialog.

* Pressing `Enter` during the identity plan build phase pre-arms confirmation for single-key challenges. When the plan completes, deletion begins immediately without a separate confirm step. This reduces the file deletion flow to `Backspace` → `Enter`, matching the speed users expect from interactive storage navigators.

* Updated the warning shown when deletion is attempted during a rescan. The message previously implied the initial scan was also a barrier; it now correctly states that deletion is locked only during rescanning.

## [1.0.2] - 2026-09-01

### Changed

* Reduced the number of filesystem stat calls during directory traversal. The root-path validity check was removed from the inner per-entry loop, a redundant cap-primitives stat that preceded the standard-library stat in each entry's processing was removed, and three sequential directory task validations at the close of each directory scan were consolidated to one.

* Eliminated JSON serialization overhead in the identity store. File identity records are now keyed directly by `FileId` in the in-memory store rather than by their JSON-encoded bytes, removing one allocation per scanned file during identity lookup.

* Replaced the blocking disk enumeration in the mount-root check with two direct filesystem stat calls. The previous implementation called `sysinfo::Disks::new_with_refreshed_list`, which on macOS queries every mounted volume for its available capacity via Apple's storage framework. That call can block indefinitely when a volume is in certain APFS snapshot states. The new implementation compares the device identifiers of a path and its parent using `lstat`, which is a direct kernel call with no involvement from CoreFoundation or StorageKit.

* Arena nodes are now stored directly inside the arena vector rather than behind individual heap allocations. Each `Box<Node>` was a separate small allocation; inlining the node data reduces allocator pressure and improves cache locality when traversing large trees.

## [1.0.1] - 2026-08-28

### Changed

* Clarified the first-run path in the README and supporting documentation. The simplest `excise` command now leads the usage guidance, while detailed scan examples remain in the getting-started guide.

* Reworded the safety, support, configuration, architecture, and release documentation in plain English without changing product behavior.

* Removed the Developer Certificate of Origin sign-off requirement for contributors. No contributor license agreement or copyright assignment has ever been required; historical authorship records are unchanged.

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

* Added the `cargo demo` alias and its `xtask demo` command. `xtask demo` validates `tapes/demo.tape`, stages its rendering under `assets/demo-main.*.gif`, resamples the tape's 24 fps GIF to 20 fps while rebuilding a non-dithered 64-colour palette and applying lossy GIF quantisation, and atomically promotes `assets/demo-main.gif` only after the size gate passes. `assets/demo.gif` remains the historical `0.1.2` recording for the README. The hosted demo workflow now uses it. The tape and current-main README hero asset were refreshed.

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
