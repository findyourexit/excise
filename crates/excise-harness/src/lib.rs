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

pub mod fixture;
pub mod report;
pub mod scenario;
mod string_enum;
