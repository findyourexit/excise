//! The fixture spec examples in the README are executable: each must parse and validate.
//!
//! They are fenced as `toml fixture-spec` so that the scenario README test, which owns the plain
//! `toml` fences, does not mistake them for scenarios.

use crate::fixture::FixtureSpec;

const README: &str = include_str!("../../../README.md");

#[test]
fn every_fixture_spec_example_in_the_readme_parses_and_validates() {
    const OPEN: &str = "```toml fixture-spec\n";
    const CLOSE: &str = "\n```";

    let mut examples = 0;
    let mut rest = README;
    while let Some(start) = rest.find(OPEN) {
        let body = &rest[start + OPEN.len()..];
        let end = body
            .find(CLOSE)
            .expect("every fenced block should be closed");
        let source = &body[..end];
        let spec = FixtureSpec::from_toml_str(source).unwrap_or_else(|error| {
            panic!("a README fixture example does not parse: {error}\n{source}")
        });
        if let Err(problems) = spec.validate() {
            panic!("a README fixture example is not valid: {problems}\n{source}");
        }
        examples += 1;
        rest = &body[end + CLOSE.len()..];
    }

    assert!(
        examples >= 1,
        "the README should show at least one fixture spec"
    );
}
