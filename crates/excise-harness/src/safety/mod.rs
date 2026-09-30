//! Safety machinery shared by every runner.
//!
//! The harness only ever runs `excise` against fixtures it owns, in an environment it built, and it
//! never leaves a process or a file behind. This module holds the pieces that enforce that:
//!
//! * [`FixtureRoot`] refuses any directory the fixture generator's `verify_owned` refuses (no
//!   ownership marker, or a link where a directory or a regular file belongs) and resolves
//!   fixture-relative paths without ever following a symbolic link.
//! * [`Scratch`] is the per-run scratch area (`HOME`, configuration, working directory, scan-store
//!   directory, temporary directory, and the event file) and the exact residue check over it.
//! * [`isolated_env`] builds the complete environment of a spawned `excise`: nothing is inherited.
//! * `kill_process_group` and [`send_signal`] deliver signals on Unix. See the notes on
//!   [`process`] for what Windows can and cannot do.
//! * [`FixtureSnapshot`] fingerprints a fixture so that a run can prove it changed nothing but its
//!   intended deletions and mutations.

mod fixture;
mod isolation;
pub mod process;
mod scratch;
mod snapshot;

pub use fixture::{FixtureRoot, SafetyError};
pub use isolation::{NARROW_COLS, ProfileSettings, isolated_env};
pub use process::{KillOutcome, SignalError, send_signal};
#[cfg(unix)]
pub use process::{kill_process_group, process_group_exists};
pub use scratch::{Scratch, ScratchError};
pub use snapshot::{FixtureDiff, FixtureSnapshot, SnapshotError};
