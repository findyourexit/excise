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
//!   `harness-failure`, `harness-ab`); their JSON Schemas live in `schemas/`.
//! * [`fixture`] generates the fixtures scenarios run against, and computes the independent
//!   oracle of what a generated tree contains.
//! * [`headless`] runs `excise --format json` against a fixture, checks the scan report against
//!   the oracle under the accounting contract, and times the scan against `du -sk`.
//! * [`bench`] builds or accepts two `excise` binaries and compares them with paired, interleaved
//!   A/B runs (`cargo xtask bench-e2e`), recording the result as a `harness-ab` document.
//! * [`comparison`] checks a ratio budget (`motion_complete_ratio`, `tui_complete_ratio`) between
//!   two runs of one binary: another profile of the same interactive scenario, or a headless
//!   scan of the same fixture (`cargo xtask compare`).

pub mod bench;
pub mod comparison;
pub mod events;
pub mod fixture;
pub mod headless;
pub mod metrics;
mod platform;
pub mod pty;
pub mod report;
mod run_support;
pub mod runner;
pub mod safety;
pub mod scenario;
mod string_enum;
