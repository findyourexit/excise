# Changelog

All notable Excise changes are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

Excise preserves the historical Diskonaut changelog below. Diskonaut versions and tags are not Excise releases.

## [Unreleased]

_The planned 0.1.1 release remains unpublished pending verification._

## [0.1.1] - Unreleased

### Added

* Prepared the `excise` crate for early testing; publication is gated on release verification, and its public library surface is provisional and may change before 1.0.

## [0.1.0] - 2026-08-21

### Added

* Added bounded, iterative scanning with explicit exclusions, filesystem boundaries, uncertainty, cancellation, and secure identity spill.
* Added identity-unique allocated-byte accounting, hard-link deduplication, reclaimable bounds, and explicit `Shared` and `Other` nodes.
* Added identity-planned, no-follow permanent deletion with independent enumeration, per-entry revalidation, hostile-name challenges, partial reports, and soft or hard cancellation.
* Added versioned TOML configuration, environment and CLI layering, fifteen built-in themes, reduced motion, ASCII output, mouse support, and Vim, Emacs, or custom movement.
* Added noninteractive table and JSON reports, lossless native-path encoding, published JSON Schemas, generated man pages, and shell completions.
* Added Linux, macOS, Windows, Nix, packaging, PTY, snapshot, fuzz, benchmark, dependency-policy, SBOM, checksum, and provenance verification.

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
