//! The headless runner: `excise --format json` against a fixture, held to an independent oracle,
//! and timed against `du -sk`.
//!
//! Where the pseudo-terminal runner (`crate::runner`) drives the interface, this runner drives the
//! scan on its own, which is what the exactness and throughput claims of the validation program
//! rest on.
//!
//! * [`scan`] runs one scan: `excise --format json --output <scratch>/scan-report.json <root>`,
//!   under the isolation of the pseudo-terminal runner (an empty environment rebuilt from
//!   `TERM`, `COLORTERM`, and `LANG`, a scratch `HOME`, configuration, working directory, and
//!   scan-store directory, and a fixture root that carries the ownership marker). The scan is
//!   bounded, its process group is killed when the bound passes, and its scratch area is checked for
//!   residue. [`process`] is the supervised process runner underneath, shared with `du`.
//! * [`document`] reads the report it wrote, after validating it against the published
//!   `scan-report` schema.
//! * [`diff`] holds that report to the oracle of the fixture ([`crate::fixture::Oracle`]) under the
//!   accounting contract, and returns typed discrepancies.
//! * [`du`] runs `du -sk` on the same tree, and says what it must print for it.
//! * [`pairs`] has the statistics of paired, interleaved timings.
//! * [`expectations`] lists the fixtures that are expected to fail the diff, or whose ratio is
//!   expected to miss the budget, strictly.
//! * [`suite`] runs all of it over the selected fixtures, writes a `harness-summary`, and renders
//!   the verdict table behind `cargo xtask headless`.

pub mod diff;
pub mod document;
pub mod du;
pub mod expectations;
pub mod pairs;
pub mod process;
pub mod scan;
pub mod suite;

pub use diff::{Diff, Discrepancy, DiscrepancyKind};
pub use document::{DocumentError, ScanDocument};
pub use expectations::{ExpectationError, Expectations, ExpectedFailure, RatioExpectedFailure};
pub use scan::{ScanError, ScanRequest, ScanRun, run_scan};
pub use suite::{
    Class, DEFAULT_REPEAT, FixtureReport, Progress, SuiteError, SuiteOptions, SuiteReport,
    run_suite,
};
