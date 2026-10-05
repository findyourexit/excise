//! The shape of a tree, as aggregates only, and fixtures built from it.
//!
//! The maintainer wants benchmarks and scenarios that behave like his own home directory without
//! anyone running Excise on it. This module is the two halves of that, and the binary
//! `excise-shape` is its command line:
//!
//! * [`walk`] (`excise-shape profile`) walks a tree, read-only: it writes nothing, and
//!   `--output` makes its one new file only after the walk has ended. It returns a
//!   [`HarnessShapeProfile`](crate::report::HarnessShapeProfile): counts, histograms, and the share
//!   of links, and never a name, a path, a link target, an owner, or a timestamp. The profile of a
//!   real home is private to its owner, and a profile attached to a performance report describes
//!   a slow tree without showing it.
//! * [`derive`] (`excise-shape spec`) turns a profile into a [`FixtureSpec`] whose `shaped` part
//!   the fixture generator builds into a tree with the same shape at any size. The spec goes in a
//!   directory of specs of its own, which `cargo xtask headless` and `cargo xtask bench-e2e` take
//!   with `--fixture-dir`.
//! * [`cli`] is the command line, as a function of its arguments, so that it is tested without
//!   starting a process.
//!
//! The tool is safe to point at a real tree and is never pointed at one by this repository's own
//! tests, scripts, or agents: they profile the fixtures the harness generates and scratch trees
//! they make.
//!
//! [`FixtureSpec`]: crate::fixture::spec::FixtureSpec

pub mod cli;
pub mod derive;
mod dir;
pub mod walk;

#[cfg(test)]
mod cli_tests;
#[cfg(test)]
mod docs_tests;
#[cfg(test)]
mod tests;

pub use derive::{DeriveError, SHAPED_ROOT, SpecRequest, render_spec, spec_from_profile};
pub use walk::{HANDLE_BUDGET, WalkError, WalkOptions, profile};
