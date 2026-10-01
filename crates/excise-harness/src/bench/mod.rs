//! Paired, interleaved A/B evidence between two builds (`cargo xtask bench-e2e`).
//!
//! * [`pairing`] builds the interleaved run order and pairs its measurements back up by trial.
//! * [`bootstrap`] computes the median candidate/baseline ratio and a deterministic bootstrap
//!   confidence interval for it.
//! * [`verdict`] classifies a metric's comparison as a block, a warning, or a pass.
//! * [`context`] gathers the conditions a comparison ran under: the host, the toolchain, the
//!   power state, the load average, and concurrent `excise` processes.
//! * [`cases`] runs one `--fixture` or `--scenario --profile` case once, on either build, and
//!   extracts the metrics it asks for.
//! * [`run`] builds the baseline and candidate binaries (or accepts prebuilt ones), drives the
//!   warm-up and measured pairs of every case, and assembles the `harness-ab` document.

pub mod bootstrap;
pub mod cases;
pub mod context;
pub mod pairing;
pub mod run;
pub mod verdict;

pub use cases::{Case, CaseError};
pub use run::{BenchOptions, BenchReport, run_bench_e2e};
pub use verdict::MetricKind;
