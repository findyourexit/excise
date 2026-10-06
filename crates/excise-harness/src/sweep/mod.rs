//! The sweep: every published version, run through the checks a published release can take, and the
//! table of which defects each one shows (`cargo xtask sweep`).
//!
//! The release notes of a version name the versions each fixed defect affects, and a claim like that
//! needs a measurement of every published release. This module measures them. It takes the builds
//! (the command line builds each ref in a worktree of its own with its own toolchain; see
//! `xtask/src/refs.rs`) and does the rest:
//!
//! * [`traits`] asks each build what it takes (`--help`), so the checks adapt to it instead of
//!   assuming today's flags.
//! * [`headless`] scans fixtures with each build, holds the reports to the oracle (version-1 and
//!   version-3 reports alike), and times scans for the paired rounds.
//! * [`checks`] drives each build in a pseudo-terminal through [`probe`]: signals, a kill and a
//!   restart, idleness, a filter, the descriptors it holds. Everything is read from the screen, the
//!   exit status, the file system, and the process: no event channel, no frame marks, no input
//!   barrier, and no key that could ask for a deletion.
//! * [`timing`] runs paired, interleaved rounds across all the versions in one session on the same
//!   warm fixture. In its turn of a round a version runs every run of its phase back to back (a
//!   headless scan and then an interface run), so that the two are measured in the same round.
//!   Each measurement is kept in the slot of its round, with whether it finished within the bound:
//!   a skipped round is no sample and no pair, and a run that did not finish only bounds the ratio
//!   of its round. It reports the median of each version's ratio to the candidate over the rounds
//!   in which both finished, with a bootstrap confidence interval ([`crate::bench::bootstrap`]).
//! * [`defects`] is the list of rows, each tied to its `CHANGELOG.md` entry; [`classify`] decides
//!   every cell; [`table`] renders them; the result is a [`HarnessSweep`] document
//!   (`schemas/harness-sweep.schema.json`).
//! * [`intact`] holds the fixtures that builds only scan to the plans they were generated from,
//!   [`signals`] says which signals each platform is sent, and [`output`] writes the evidence files,
//!   which a version's [`label`](fn@label) names and its ref as typed never does.
//!
//! # Safety
//!
//! A published release can be told to delete what it shows, so the sweep never lets one. It runs
//! each build only through the harness, on generated fixtures that carry the ownership marker, in
//! the isolated environment, in a pseudo-terminal of its own process group that is killed when the
//! check ends. The keys it sends are a closed vocabulary (see [`probe`]) that cannot ask for a
//! deletion, so no dialog ever opens for a confirmation to meet.
//!
//! Every run of a build on a fixture is followed by a comparison of the fixture, and a difference
//! stops the whole sweep at once:
//!
//! * the headless scans of a version ([`run_sweep`]) are followed by a check of every fixture they
//!   ran on against the plan it was generated from ([`intact`]), and the same check runs once
//!   before the first build, so that a difference is never put down to a version it predates;
//! * each interface check ([`checks`]) compares its fixture with a snapshot it took before the
//!   check;
//! * the paired timings compare the timing fixture with a snapshot taken before the first scan and
//!   another taken after the last scan of every version.
//!
//! A comparison sees which entries there are and each one's kind, size, and link target (the check
//! against the plan also each one's permission bits). It does not read the contents of a file, and
//! it cannot see below a folder it cannot list: `hostile-small` makes some on purpose, and nothing
//! below them is held to anything.
//!
//! # What it leaves behind
//!
//! * On every platform the scratch areas and fixture copies it makes below the work base are
//!   removed by the time the sweep returns, whether it finished, failed, or stopped on a changed
//!   fixture, and the worktree each build is made in is removed when the build ends: the directory
//!   that holds them, `target/excise-sweep-worktrees/`, is left empty. What stays is what it is
//!   for: the run's directory (`target/excise-sweep/<run id>/`), the cached builds
//!   (`target/excise-sweep-builds/`), and the cached fixtures (`target/excise-fixtures.noindex/`).
//! * A sweep that is killed (`SIGKILL`, power loss) runs no cleanup. It leaves the scratch areas
//!   (`xh-*`) and fixture copies (`<fixture>-<process id>-<number>`) it was using in the work base,
//!   and at most one build worktree below `target/excise-sweep-worktrees/`.
//! * It sends signals to the builds it runs ([`signals`]). On macOS it sends no SIGQUIT, so no
//!   signal it sends makes the system write a crash report. On Linux it sends SIGQUIT with the soft
//!   limit on the size of a core file at 0, so the kernel writes no core file; a machine that pipes
//!   core dumps to a handler does not enforce that limit, and what the handler keeps is its own.
//! * A build that crashes by itself can make the operating system write a crash report, on macOS
//!   into the person's own `~/Library/Logs/DiagnosticReports`. The sweep neither prevents it nor
//!   deletes it. It deletes nothing in the person's home folder.

mod checks;
mod classify;
pub mod defects;
mod headless;
mod intact;
mod label;
mod model;
mod output;
mod probe;
mod run;
mod signals;
pub mod table;
mod timing;
mod traits;

pub use checks::IdleWindow;
pub use label::label;
pub use run::{BuildInput, RunDir, SweepError, SweepOptions, SweepReport, VersionInput, run_sweep};

#[cfg(doc)]
use crate::report::HarnessSweep;
