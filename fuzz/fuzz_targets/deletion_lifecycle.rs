//! Model-based fuzzing of the whole deletion lifecycle.
//!
//! Each input builds a fresh fixture tree in a private temporary directory, runs the real
//! runtime in-process on a script of keys, resizes, filters, and deletions decoded from the
//! input, applies file-system mutations to the live tree between steps, and then holds what the
//! runtime did to the tree against a reference model. A failed check panics with a message that
//! names the step and the paths, and libFuzzer saves the input.
//!
//! The four invariants:
//!
//! 1. Only reviewed identities of confirmed targets disappear. An entry created, replaced, or
//!    moved in after the planner's review is never deleted, even when it carries an identity the
//!    planner reviewed (a file system may reuse one at once), and the executor takes no work
//!    that an accepted request and confirmation have not paid for.
//! 2. Nothing outside a confirmed target changes, and the sentinels beside the scan root
//!    survive.
//! 3. The interface returns to a navigable state after every deletion outcome: the fixed closing
//!    keys end the run.
//! 4. The terminal is restored: the backend's session ends by clearing the screen and showing
//!    the cursor, as `assert_terminal_lifecycle` in the unit tests requires.
//!
//! Every key waits for the runtime to settle first, so an input replays the same way every time;
//! a `^` before a step is the one explicit race. `script` documents the input language, `model`
//! and `check` the checks, and `world` the fixture and the platforms each mutation runs on. A
//! crash leaves at most the private directory, named `excise-fuzz-deletion-lifecycle-*` in the
//! temporary directory. The environment variable `EXCISE_FUZZ_TRACE` prints the trace of every
//! run, with a digest of it that two replays of one input share.

#![no_main]

#[path = "deletion_lifecycle/check.rs"]
mod check;
#[path = "deletion_lifecycle/harness.rs"]
mod harness;
#[path = "deletion_lifecycle/model.rs"]
mod model;
#[path = "deletion_lifecycle/script.rs"]
mod script;
#[path = "deletion_lifecycle/world.rs"]
mod world;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    harness::run_input(data);
});
