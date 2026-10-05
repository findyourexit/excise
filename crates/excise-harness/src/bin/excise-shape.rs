//! `excise-shape`: measures the shape of a tree as aggregates only, and builds a fixture
//! specification shaped like it. See `excise-shape --help`, and the harness README.
//!
//! Install it with
//! `cargo install --git https://github.com/findyourexit/excise excise-harness --bin excise-shape`.

use std::{
    env,
    io::{stderr, stdout},
    process::ExitCode,
};

fn main() -> ExitCode {
    excise_harness::shape::cli::run(
        env::args_os().skip(1),
        &mut stdout().lock(),
        &mut stderr().lock(),
    )
}
