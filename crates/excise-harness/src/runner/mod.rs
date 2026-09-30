//! The pseudo-terminal runner: executes a validated scenario against a real `excise` process.
//!
//! [`run_scenario`] runs one scenario under one profile; [`run_e2e`] runs the scenario × profile
//! matrix and writes the run summary. Both take an already-materialized fixture root: building the
//! fixture is somebody else's job (the fixture generator's `Fixtures::run_copy`).
//!
//! # How a run goes
//!
//! 1. The scenario is validated and *prepared*: every regular expression is compiled and every key
//!    encoded. A scenario that cannot run, or needs something this runner does not do, is an
//!    `error` before any process exists.
//! 2. The fixture root must carry the ownership marker, or the run is refused.
//! 3. `excise` is spawned in a pseudo-terminal, in its own process group, with an isolated
//!    environment (see [`crate::safety::isolated_env`]) and a fresh scratch area.
//! 4. The steps run in order. Every wait is bounded and semantic: it looks at the screen model, the
//!    event channel, or the file system, never at raw output bytes and never at the screen being
//!    idle. See the `exec` module for how waits work.
//! 5. On any failure the whole process group is killed, then the failure bundle is written.
//! 6. Afterwards the run cleans up after itself: no process, no scratch area, no recording.
//!
//! # Semantics worth knowing
//!
//! * **`settle`** waits for a `frame` event whose `inputs` counter is at least the number of input
//!   events the runner has sent and that was observed after the last one was written, then reads
//!   the terminal output still in flight (at most 20 ms, ending after 3 ms of quiet) so that the
//!   screen model has caught up with the frame. A key that changes nothing draws no frame and so
//!   never settles.
//! * **`select`** opens the filter with `/`, erases any text the filter opened with, types the
//!   name, checks the prompt, presses Enter, and waits until the inspector pane shows exactly that
//!   name.
//! * **`delete`** presses Backspace, reads the dialog, and presses `y` only when the dialog names
//!   exactly the requested entry, kind, and path and every sentinel is intact. Otherwise it fails
//!   without ever sending `y`. It then waits for the `deletion_finished` event and for the first
//!   frame after it, so the screen shows the result. The program rebuilds its map after a deletion
//!   and treats a quit during the rebuild as a cancellation (exit 130), so a scenario that quits
//!   next waits for the header to read `COMPLETE`.
//! * **`quit`** presses `q`, waits for the quit dialog, and confirms with `y`.
//! * **`resize`** resizes the terminal and waits for the frame that answers it.
//! * **`wait_event`** matches any event read so far, including events before the step began.
//! * **`expect_exit`** also compares the fixture with its state before the run: only confirmed
//!   deletions may differ.
//! * **`signal`** is delivered to the child process on Unix. Windows console events are
//!   unsupported.
//! * **`fs_mutate`** applies the fixture generator's mutator (`fixture::mutate::apply`) when the
//!   step runs. The mutated path is then an intended change: `expect_exit` accepts differences at
//!   that path, below it, and in the directories the mutation created above it, and nothing else.
//! * A step's `timeout_ms` bounds the whole step, not each wait inside it.
//!
//! # Profiles
//!
//! `default` changes nothing, `deterministic` sets `EXCISE_REDUCED_MOTION=1` and
//! `EXCISE_SCAN_THREADS=1`, `monochrome-ascii` sets `EXCISE_THEME=monochrome` and `EXCISE_ASCII=1`,
//! `narrow` makes the terminal 60 columns wide (its rows come from the scenario), and
//! `mouse-keymaps` sets `EXCISE_MOUSE=1` and `EXCISE_KEYMAP=emacs`.

mod budget;
mod bundle;
mod delete;
mod e2e;
mod exec;
mod outcome;
mod plan;
mod run;
mod steps;
mod verdict;
mod work;

pub use budget::{default_limit, limit_for};
pub use e2e::{E2eError, E2eOptions, E2eReport, RunRecord, load_scenarios, run_e2e};
pub use outcome::{FailureCause, RunError, StepFailure};
pub use run::{RunReport, RunRequest, run_scenario};
pub use verdict::{Outcome, verdict};
pub use work::work_base;
