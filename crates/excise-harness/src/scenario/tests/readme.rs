//! The README's TOML examples are executable: each must parse and validate.

use crate::scenario::Scenario;

const README: &str = include_str!("../../../README.md");

/// The minimal scenario that README fragments are placed into.
const HEAD: &str = r#"schema_version = 1
name = "readme-fragment"
description = "A README example."
fixture = "readme-fragment"
sentinels = ["keep-a.bin"]
profiles = ["default"]
"#;

/// The bodies of the fenced `toml` blocks in `markdown`.
fn toml_blocks(markdown: &str) -> Vec<&str> {
    const OPEN: &str = "```toml\n";
    const CLOSE: &str = "\n```";
    let mut blocks = Vec::new();
    let mut rest = markdown;
    while let Some(start) = rest.find(OPEN) {
        let body = &rest[start + OPEN.len()..];
        let end = body
            .find(CLOSE)
            .expect("every fenced block should be closed");
        blocks.push(&body[..end]);
        rest = &body[end + CLOSE.len()..];
    }
    blocks
}

#[test]
fn every_toml_example_in_the_readme_parses_and_validates() {
    let mut complete_scenarios = 0;
    for block in toml_blocks(README) {
        // A `budget = "..."` scalar belongs to a comparison file (`crate::comparison::Comparison`),
        // not a scenario: a scenario's budget overrides are a `[budgets]` table of named limits.
        // `crate::comparison::tests::readme` validates those blocks against that type instead.
        if block.contains("\nbudget = \"") {
            continue;
        }
        let source = if block.starts_with("schema_version") {
            complete_scenarios += 1;
            block.to_owned()
        } else if block.starts_with("[[steps]]") {
            format!("{HEAD}\n{block}\n")
        } else if block.starts_with("[budgets]") {
            format!("{HEAD}\n{block}\n[[steps]]\nstep = \"settle\"\n")
        } else {
            panic!("a README example has a shape this test cannot place:\n{block}");
        };

        let scenario = Scenario::from_toml_str(&source)
            .unwrap_or_else(|error| panic!("a README example does not parse: {error}\n{block}"));
        if let Err(errors) = scenario.validate() {
            panic!("a README example is not valid: {errors}\n{block}");
        }
    }

    assert!(
        complete_scenarios >= 1,
        "the README should show at least one complete scenario"
    );
}
