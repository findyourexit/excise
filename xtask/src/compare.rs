//! `cargo xtask compare`: checks a ratio budget (`motion_complete_ratio`, `tui_complete_ratio`)
//! with paired, interleaved runs of this crate's own `excise` binary.
//!
//! This is a thin wrapper, like `e2e`, `headless`, and `bench-e2e`: the comparison files, the
//! paired-run engine, the bootstrap statistics, and the verdict table all live in
//! `excise_harness::comparison`; this file parses the command line, finds or builds the binary,
//! and turns the result into an exit status.

use std::{
    env,
    error::Error,
    io,
    path::{Path, PathBuf},
    time::Instant,
};

use excise_harness::{
    comparison::{CompareOptions, CompareRunOptions, Comparison, load_comparisons, run_compare},
    fixture::Fixtures,
    report::Tier,
};

use crate::e2e::{BINARY_ENV, build_release_binary};

const USAGE: &str = "usage: cargo xtask compare [--quick|--full|--nightly] [--comparison NAME]... [--pairs N] \
     [--seed S]";

/// `--seed` when it is not given: fixed, so an unqualified run is still reproducible.
const DEFAULT_SEED: u64 = 0;

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
struct Selection {
    tier: Tier,
    comparisons: Vec<String>,
    pairs: Option<u32>,
    seed: u64,
}

/// Runs the command.
pub fn compare(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let selection =
        parse(args).map_err(|message| io::Error::other(format!("{message}\n{USAGE}")))?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| io::Error::other("the xtask manifest has no parent directory"))?
        .to_path_buf();
    let target = env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);

    let comparisons = select_comparisons(
        load_comparisons(&root.join("crates/excise-harness/comparisons"))?,
        &selection.comparisons,
    )?;
    let binary = match env::var_os(BINARY_ENV).filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => build_release_binary(&root, &target)?,
    };
    let fixtures = Fixtures::bundled();
    let work_dir = target.join("excise-compare").join("work");
    std::fs::create_dir_all(&work_dir).map_err(|source| {
        io::Error::other(format!(
            "cannot create the work directory `{}`: {source}",
            work_dir.display()
        ))
    })?;
    let options = CompareRunOptions {
        compare: CompareOptions {
            binary: &binary,
            fixtures: &fixtures,
            work_dir: &work_dir,
            seed: selection.seed,
        },
        tier: selection.tier,
        named: !selection.comparisons.is_empty(),
        pairs: selection.pairs,
    };

    let started = Instant::now();
    let report = run_compare(&options, &comparisons);
    for record in &report.records {
        eprintln!(
            "  {} [{}]: {} ({:.2}s){}",
            record.name,
            record.budget,
            record.verdict,
            record.duration.as_secs_f64(),
            record
                .error
                .as_ref()
                .map_or_else(String::new, |reason| format!(" - {reason}")),
        );
    }
    println!("{}", report.table());
    println!("wall time: {:.1}s", started.elapsed().as_secs_f64());
    if report.is_success() {
        Ok(())
    } else {
        Err(io::Error::other("the compare run has blocking verdicts").into())
    }
}

/// Keeps the comparisons named on the command line, or all of them when none was named.
fn select_comparisons(all: Vec<Comparison>, names: &[String]) -> Result<Vec<Comparison>, String> {
    if names.is_empty() {
        return Ok(all);
    }
    if let Some(unknown) = names
        .iter()
        .find(|name| !all.iter().any(|c| &c.name == *name))
    {
        let known: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        return Err(format!(
            "unknown comparison `{unknown}`; the comparisons are {}",
            known.join(", ")
        ));
    }
    Ok(all
        .into_iter()
        .filter(|c| names.contains(&c.name))
        .collect())
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Selection, String> {
    let mut selection = Selection {
        tier: Tier::Full,
        comparisons: Vec::new(),
        pairs: None,
        seed: DEFAULT_SEED,
    };
    let mut tier: Option<Tier> = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--quick" | "--full" | "--nightly" => {
                let requested = match argument.as_str() {
                    "--quick" => Tier::Quick,
                    "--full" => Tier::Full,
                    _ => Tier::Nightly,
                };
                if tier
                    .replace(requested)
                    .is_some_and(|earlier| earlier != requested)
                {
                    return Err(
                        "`--quick`, `--full`, and `--nightly` exclude each other".to_owned()
                    );
                }
            }
            "--comparison" => selection
                .comparisons
                .push(value(&mut args, "--comparison")?),
            "--pairs" => {
                let text = value(&mut args, "--pairs")?;
                selection.pairs = Some(
                    text.parse()
                        .ok()
                        .filter(|count| *count > 0)
                        .ok_or_else(|| {
                            format!("`--pairs` takes a positive integer, not `{text}`")
                        })?,
                );
            }
            "--seed" => {
                let text = value(&mut args, "--seed")?;
                selection.seed = text
                    .parse()
                    .map_err(|_| format!("`--seed` takes a whole number, not `{text}`"))?;
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    selection.tier = tier.unwrap_or(Tier::Full);
    Ok(selection)
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("`{flag}` needs a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<Selection, String> {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn no_arguments_run_the_full_tier_with_the_default_seed() {
        let selection = parsed(&[]).expect("valid");
        assert_eq!(selection.tier, Tier::Full);
        assert_eq!(selection.seed, DEFAULT_SEED);
        assert!(selection.comparisons.is_empty());
        assert_eq!(selection.pairs, None);
    }

    #[test]
    fn selections_accumulate_the_flags_that_take_lists() {
        let selection = parsed(&[
            "--nightly",
            "--comparison",
            "a",
            "--comparison",
            "b",
            "--pairs",
            "3",
            "--seed",
            "7",
        ])
        .expect("valid");
        assert_eq!(selection.tier, Tier::Nightly);
        assert_eq!(selection.comparisons, ["a", "b"]);
        assert_eq!(selection.pairs, Some(3));
        assert_eq!(selection.seed, 7);
    }

    #[test]
    fn contradictory_or_malformed_arguments_are_refused() {
        for args in [
            &["--quick", "--full"][..],
            &["--quick", "--nightly"],
            &["--pairs", "0"],
            &["--pairs", "many"],
            &["--pairs"],
            &["--seed", "-1"],
            &["--comparison"],
            &["--bogus"],
        ] {
            assert!(parsed(args).is_err(), "{args:?} must be refused");
        }
        assert_eq!(
            parsed(&["--quick", "--quick"]).expect("same twice").tier,
            Tier::Quick
        );
    }

    #[test]
    fn selecting_an_unknown_comparison_by_name_is_refused() {
        let known = vec![comparison_fixture("known")];
        let error = select_comparisons(known, &["missing".to_owned()]).expect_err("unknown");
        assert!(error.contains("unknown comparison `missing`"));
        assert!(error.contains("known"));
    }

    #[test]
    fn selecting_by_name_narrows_to_exactly_those_comparisons() {
        let all = vec![comparison_fixture("a"), comparison_fixture("b")];
        let selected = select_comparisons(all, &["b".to_owned()]).expect("known name");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "b");
    }

    fn comparison_fixture(name: &str) -> Comparison {
        Comparison::from_toml_str(&format!(
            "schema_version = 1\nname = \"{name}\"\ndescription = \"d\"\nfixture = \"f\"\n\
             budget = \"motion_complete_ratio\"\ntimeout_ms = 1000\n"
        ))
        .expect("a comparison")
    }
}
