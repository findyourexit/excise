//! Black-box validation harness for Excise.
//!
//! This crate is internal test tooling. It is not published, it is not shipped in release
//! archives, and nothing in it is part of the supported Excise product interface: it may change
//! without notice. It never depends on the `excise` crate. It drives the `excise` binary from the
//! outside and inspects only what a user or a script could observe.
//!
//! * [`scenario`] is the typed model of TOML scenario files, with strict parsing and semantic
//!   validation.
//! * [`report`] holds the versioned machine-output documents (`harness-summary`,
//!   `harness-failure`, `harness-ab`, `harness-counts`, `harness-tui`, `harness-shape-profile`);
//!   their JSON Schemas live in `schemas/`.
//! * [`fixture`] generates the fixtures scenarios run against, and computes the independent
//!   oracle of what a generated tree contains.
//! * [`headless`] runs `excise --format json` against a fixture, checks the scan report against
//!   the oracle under the accounting contract, and times the scan against `du -sk`.
//! * [`bench`] builds or accepts two `excise` binaries and compares them with paired, interleaved
//!   A/B runs (`cargo xtask bench-e2e`), recording the result as a `harness-ab` document.
//! * [`comparison`] checks a ratio budget (`motion_complete_ratio`, `tui_complete_ratio`) between
//!   two runs of one binary: another profile of the same interactive scenario, or a headless
//!   scan of the same fixture (`cargo xtask compare`).
//! * [`counts`] counts what a build costs without timing anything (`cargo xtask counts`), records
//!   the counts of every commit on `main`, and compares a pull request's counts with them.
//! * [`tui`] drives one live `excise` session step by step on a fixture, for exploration
//!   (`cargo xtask tui`).
//! * [`shape`] measures the shape of a tree as aggregates only and builds a fixture specification
//!   shaped like it (`excise-shape`, the binary of this crate).

pub mod bench;
pub mod comparison;
pub mod counts;
pub mod events;
pub mod fixture;
pub mod headless;
pub mod histogram;
pub mod metrics;
mod platform;
pub mod pty;
pub mod report;
mod run_support;
pub mod runner;
pub mod safety;
pub mod scenario;
pub mod shape;
mod string_enum;
pub mod tui;
