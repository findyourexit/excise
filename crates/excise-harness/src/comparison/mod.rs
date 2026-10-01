//! Typed model of TOML comparison files: a paired, interleaved ratio-budget check between two
//! runs of the same `excise` binary.
//!
//! A [`crate::scenario::Scenario`] and its `expect_budget` step judge one metric from one run; a
//! ratio such as `motion_complete_ratio` or `tui_complete_ratio` needs two. A [`Comparison`] names
//! a fixture, the budget it checks, and (for `tui_complete_ratio`) which interactive `profile` is
//! the candidate; [`crate::comparison::run::run_comparison`] drives the two sides interleaved and
//! checks the median ratio against the budget's limit. By that budget's own definition (see the
//! harness README's budget table), `motion_complete_ratio` always compares `default` against
//! `reduced-motion`, so a file that checks it never names a profile.
//!
//! [`Comparison::from_toml_str`] and [`Comparison::from_path`] parse a document and reject unknown
//! fields at every level. [`Comparison::validate`] then applies the semantic rules TOML typing
//! cannot express. [`load_comparisons`] loads every file in a directory, in name order, and
//! applies both.

pub mod run;

pub use run::{
    CompareOptions, CompareReport, CompareRunOptions, ComparisonError, ComparisonReport, Skipped,
    run_compare, run_comparison,
};

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::platform::PLATFORMS;
use crate::scenario::{Budget, Expect, Field, MAX_IDENTIFIER_LEN, MAX_TIMEOUT_MS, Profile, Tier};

/// The only `schema_version` this module reads.
pub const SCHEMA_VERSION: u32 = 1;
/// The longest slice id: an uppercase letter followed by up to seven uppercase letters or digits.
const MAX_SLICE_LEN: usize = 8;

/// `pairs` when a comparison file does not set it.
const fn default_pairs() -> u32 {
    5
}

/// One comparison file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comparison {
    /// The format version; must be [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The comparison's identifier.
    pub name: String,
    /// What the comparison demonstrates.
    pub description: String,
    /// The identifier of the fixture specification both sides run against.
    pub fixture: String,
    /// The ratio budget this comparison checks: `motion_complete_ratio` or `tui_complete_ratio`.
    pub budget: Budget,
    /// Which interactive profile is the candidate (numerator). Required when `budget` is
    /// `tui_complete_ratio`: the profile compared against a headless scan. Must be absent when
    /// `budget` is `motion_complete_ratio`, whose two sides (`default` and `reduced-motion`) are
    /// fixed by the budget's own definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    /// How many interleaved baseline/candidate pairs to run. Defaults to 5.
    #[serde(default = "default_pairs")]
    pub pairs: u32,
    /// The bound each side's run gets to reach `COMPLETE` (or finish its scan). A run that has
    /// not within this bound counts as a failed ratio, not an error: F2's own symptom. Choose it
    /// with headroom over the slower side's healthy time, not just over the faster side's.
    pub timeout_ms: u64,
    /// How often the comparison runs. `quick` (the default) runs under every tier; `full` needs
    /// `--full` or `--nightly`; `nightly` needs `--nightly`.
    #[serde(default)]
    pub tier: Tier,
    /// The platforms the comparison runs on, as `std::env::consts::OS` spells them. Absent means
    /// every platform the harness knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
    /// Whether the comparison must pass or must fail.
    #[serde(default)]
    pub expect: Expect,
    /// The platforms `expect = "fail"` applies to. Absent means every platform in `platforms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fails_on: Option<Vec<String>>,
    /// The id of the work slice that will fix the defect; required when `expect` is `fail`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<String>,
    /// Overrides the budget's default limit (1.25).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// Caps how fast the reader drains each side's output, in bytes per second, so the
    /// comparison can simulate a slow terminal. `None` (the default) drains as fast as the
    /// operating system delivers bytes. Copied onto both sides' synthesized scenario (see
    /// `crate::scenario::Terminal::drain_bytes_per_sec`); a headless baseline ignores it, since it
    /// has no pseudo-terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_bytes_per_sec: Option<u64>,
}

/// A comparison file could not be read, parsed, or used.
#[derive(Debug, Error)]
pub enum LoadError {
    /// The file could not be read.
    #[error("cannot read comparison `{}`: {source}", path.display())]
    Read {
        /// The path that was read.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The file is not a valid comparison document.
    #[error("invalid comparison `{}`: {source}", path.display())]
    Parse {
        /// The path that was read.
        path: PathBuf,
        /// The underlying error, with line and column.
        source: toml::de::Error,
    },
    /// The file parses but breaks a semantic rule.
    #[error("comparison `{name}` is invalid: {errors}")]
    Invalid {
        /// The comparison's name.
        name: String,
        /// Every broken rule.
        errors: ValidationErrors,
    },
    /// The file is named differently from the comparison inside it.
    #[error(
        "`{}` holds the comparison `{name}`; a comparison file is named after its comparison",
        path.display()
    )]
    Misnamed {
        /// The file.
        path: PathBuf,
        /// The comparison name inside it.
        name: String,
    },
    /// A directory could not be read.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
}

impl Comparison {
    /// Parses a comparison from TOML text.
    ///
    /// Parsing rejects unknown fields, unknown enum values, and wrongly typed values. It does not
    /// apply the semantic rules of [`Comparison::validate`], which a caller must also apply.
    ///
    /// # Errors
    ///
    /// Returns the TOML error, which carries the line and column of the problem.
    pub fn from_toml_str(source: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(source)
    }

    /// Reads and parses a comparison file.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::Read`] when the file cannot be read and [`LoadError::Parse`] when it
    /// is not a valid comparison document.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, LoadError> {
        let path = path.as_ref();
        let source = fs::read_to_string(path).map_err(|source| LoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&source).map_err(|source| LoadError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// A rule that a comparison as a whole breaks.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// `schema_version` is not the version this module reads.
    #[error("`schema_version` is {found}, but this harness reads only version {SCHEMA_VERSION}")]
    UnsupportedSchemaVersion {
        /// The version found.
        found: u32,
    },
    /// `name` or `fixture` is not a valid identifier.
    #[error(
        "`{field}` {value:?} is not an identifier: use 1 to {MAX_IDENTIFIER_LEN} lowercase ASCII \
         letters, digits, `-` or `_`, starting with a letter or digit"
    )]
    InvalidIdentifier {
        /// The field that holds the value.
        field: &'static str,
        /// The offending value.
        value: String,
    },
    /// `budget` is not one of the two ratio budgets this module computes.
    #[error(
        "budget `{budget}` is not a paired-run ratio this comparison can check; use \
         `motion_complete_ratio` or `tui_complete_ratio`"
    )]
    UnsupportedBudget {
        /// The offending budget.
        budget: Budget,
    },
    /// `profile` is set, but `budget` is `motion_complete_ratio`, whose two sides are fixed.
    #[error(
        "`motion_complete_ratio` compares `default` against `reduced-motion` by its own \
        definition; `profile` must be omitted"
    )]
    ProfileNotApplicable,
    /// `profile` is missing, but `budget` is `tui_complete_ratio`, which needs one.
    #[error(
        "`tui_complete_ratio` needs `profile`: which interactive profile to compare against a \
         headless scan"
    )]
    ProfileRequired,
    /// `pairs` is zero.
    #[error("`pairs` must be at least 1")]
    ZeroPairs,
    /// `timeout_ms` is zero or above the cap.
    #[error("`timeout_ms` is {timeout_ms}, but must be between 1 and {MAX_TIMEOUT_MS}")]
    TimeoutOutOfRange {
        /// The offending value.
        timeout_ms: u64,
    },
    /// `expect` is `fail` but no `slice` says which slice will fix it.
    #[error("`expect = \"fail\"` requires `slice`")]
    ExpectedFailureWithoutSlice,
    /// `slice` is not a slice id.
    #[error(
        "`slice` {value:?} is not a slice id: use an uppercase letter followed by uppercase \
         letters or digits, at most {MAX_SLICE_LEN} in all"
    )]
    InvalidSlice {
        /// The offending value.
        value: String,
    },
    /// `fails_on` is set without `expect = "fail"`.
    #[error("`fails_on` requires `expect = \"fail\"`")]
    FailsOnWithoutExpectedFailure,
    /// `platforms` or `fails_on` is present but lists nothing.
    #[error("`{field}` is present but empty; omit it to use every platform")]
    EmptyPlatformList {
        /// `platforms` or `fails_on`.
        field: &'static str,
    },
    /// A platform name is not one this harness knows.
    #[error("`{field}` names the platform {value:?}, which is not `linux`, `macos`, or `windows`")]
    UnknownPlatform {
        /// The field and index of the offending entry.
        field: Field,
        /// The offending value.
        value: String,
    },
    /// `fails_on` names a platform outside `platforms`.
    #[error("`{field}` names the platform {value:?}, which `platforms` does not include")]
    FailsOnOutsidePlatforms {
        /// The field and index of the offending entry.
        field: Field,
        /// The offending value.
        value: String,
    },
    /// A platform name is listed more than once in `platforms` or in `fails_on`.
    #[error("`{field}` lists the platform {value:?} more than once")]
    DuplicatePlatform {
        /// The field and index of the second occurrence.
        field: Field,
        /// The offending value.
        value: String,
    },
    /// `limit` is negative, zero, infinite, or not a number.
    #[error("`limit` must be a finite, positive number")]
    InvalidLimit,
    /// `drain_bytes_per_sec` is present but zero, which would never drain.
    #[error("`drain_bytes_per_sec` must be positive; zero would never drain")]
    ZeroDrainRate,
}

/// Every rule a comparison breaks, in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationErrors(Vec<ValidationError>);

impl ValidationErrors {
    /// The broken rules. Never empty.
    #[must_use]
    pub fn errors(&self) -> &[ValidationError] {
        &self.0
    }

    /// Takes the broken rules.
    #[must_use]
    pub fn into_errors(self) -> Vec<ValidationError> {
        self.0
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("comparison is invalid")?;
        for error in &self.0 {
            write!(formatter, "\n  - {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

impl Comparison {
    /// Applies the semantic rules of the comparison format.
    ///
    /// The rules are: a supported `schema_version`; identifier-shaped `name` and `fixture`;
    /// `budget` one of the two ratio budgets; `profile` present exactly when `budget` needs it;
    /// `pairs` at least 1; `timeout_ms` between 1 and [`MAX_TIMEOUT_MS`]; `slice` present whenever
    /// `expect = "fail"`; every platform list well-formed; and a finite, positive `limit` override
    /// when one is given.
    ///
    /// # Errors
    ///
    /// Returns every broken rule, not only the first.
    pub fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = Vec::new();
        if self.schema_version != SCHEMA_VERSION {
            errors.push(ValidationError::UnsupportedSchemaVersion {
                found: self.schema_version,
            });
        }
        for (field, value) in [("name", &self.name), ("fixture", &self.fixture)] {
            if !is_identifier(value) {
                errors.push(ValidationError::InvalidIdentifier {
                    field,
                    value: value.clone(),
                });
            }
        }
        match self.budget {
            Budget::MotionCompleteRatio if self.profile.is_some() => {
                errors.push(ValidationError::ProfileNotApplicable);
            }
            Budget::TuiCompleteRatio if self.profile.is_none() => {
                errors.push(ValidationError::ProfileRequired);
            }
            Budget::MotionCompleteRatio | Budget::TuiCompleteRatio => {}
            other => errors.push(ValidationError::UnsupportedBudget { budget: other }),
        }
        if self.pairs == 0 {
            errors.push(ValidationError::ZeroPairs);
        }
        if !(1..=MAX_TIMEOUT_MS).contains(&self.timeout_ms) {
            errors.push(ValidationError::TimeoutOutOfRange {
                timeout_ms: self.timeout_ms,
            });
        }
        if self.expect == Expect::Fail && self.slice.is_none() {
            errors.push(ValidationError::ExpectedFailureWithoutSlice);
        }
        if let Some(slice) = &self.slice
            && !is_slice_id(slice)
        {
            errors.push(ValidationError::InvalidSlice {
                value: slice.clone(),
            });
        }
        if let Some(platforms) = &self.platforms {
            check_platforms("platforms", platforms, &mut errors);
        }
        if let Some(fails_on) = &self.fails_on {
            if self.expect != Expect::Fail {
                errors.push(ValidationError::FailsOnWithoutExpectedFailure);
            }
            check_platforms("fails_on", fails_on, &mut errors);
            for (index, platform) in fails_on.iter().enumerate() {
                if PLATFORMS.contains(&platform.as_str()) && !self.runs_on(platform) {
                    errors.push(ValidationError::FailsOnOutsidePlatforms {
                        field: Field::at("fails_on", index),
                        value: platform.clone(),
                    });
                }
            }
        }
        if let Some(limit) = self.limit
            && (!limit.is_finite() || limit <= 0.0)
        {
            errors.push(ValidationError::InvalidLimit);
        }
        if self.drain_bytes_per_sec == Some(0) {
            errors.push(ValidationError::ZeroDrainRate);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors))
        }
    }

    /// The platforms this comparison runs on, as `std::env::consts::OS` spells them: `platforms`
    /// verbatim, or every platform the harness knows when it is absent.
    #[must_use]
    pub fn effective_platforms(&self) -> Vec<&str> {
        self.platforms.as_deref().map_or_else(
            || PLATFORMS.to_vec(),
            |platforms| platforms.iter().map(String::as_str).collect(),
        )
    }

    /// Whether the comparison runs on `os`, as `std::env::consts::OS` spells it.
    #[must_use]
    pub fn runs_on(&self, os: &str) -> bool {
        match &self.platforms {
            Some(platforms) => platforms.iter().any(|platform| platform == os),
            None => PLATFORMS.contains(&os),
        }
    }

    /// Whether `expect = "fail"` applies on `os`, as `std::env::consts::OS` spells it:
    /// `fails_on` verbatim, or every platform in `platforms` when it is absent.
    fn fails_on_platform(&self, os: &str) -> bool {
        match &self.fails_on {
            Some(fails_on) => fails_on.iter().any(|platform| platform == os),
            None => self.runs_on(os),
        }
    }

    /// The effective expectation on `os`, as `std::env::consts::OS` spells it. Mirrors
    /// [`crate::scenario::Scenario::expect_on`].
    #[must_use]
    pub fn expect_on(&self, os: &str) -> Expect {
        if self.expect == Expect::Fail && self.fails_on_platform(os) {
            Expect::Fail
        } else {
            Expect::Pass
        }
    }

    /// The profile the candidate (numerator) side runs under: `default` for
    /// `motion_complete_ratio` (fixed by that budget's definition), or the declared `profile` for
    /// `tui_complete_ratio`.
    ///
    /// # Panics
    ///
    /// Panics if `budget` is `tui_complete_ratio` and `profile` is `None`, or if `budget` is
    /// neither ratio budget: both are rejected by [`Comparison::validate`], which every loader and
    /// runner must call first.
    #[must_use]
    pub fn candidate_profile(&self) -> Profile {
        match self.budget {
            Budget::MotionCompleteRatio => Profile::Default,
            Budget::TuiCompleteRatio => self
                .profile
                .expect("validate() requires `profile` when budget is tui_complete_ratio"),
            other => {
                unreachable!("validate() restricts `budget` to the two ratio budgets, not {other}")
            }
        }
    }

    /// The baseline (denominator) side: another profile of the same interactive run
    /// (`reduced-motion`, for `motion_complete_ratio`), or `None` for a headless scan of the same
    /// fixture (`tui_complete_ratio`).
    #[must_use]
    pub fn baseline_profile(&self) -> Option<Profile> {
        match self.budget {
            Budget::MotionCompleteRatio => Some(Profile::ReducedMotion),
            _ => None,
        }
    }

    /// The limit this comparison's ratio is checked against: its own override, or the budget's
    /// default (1.25 for both ratio budgets, so the `unwrap_or` below never actually applies).
    #[must_use]
    pub fn limit(&self) -> f64 {
        self.limit
            .or_else(|| crate::runner::default_limit(self.budget))
            .unwrap_or(1.25)
    }
}

/// Lowercase ASCII letters, digits, `-` and `_`, starting with a letter or digit. Mirrors
/// `crate::scenario`'s own rule for `name` and `fixture`.
fn is_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'0'..=b'9'))
        && value.len() <= MAX_IDENTIFIER_LEN
        && bytes.all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
}

/// An uppercase letter followed by uppercase letters and digits, such as `X2`.
fn is_slice_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z'))
        && value.len() <= MAX_SLICE_LEN
        && bytes.all(|byte| matches!(byte, b'A'..=b'Z' | b'0'..=b'9'))
}

/// Checks a `platforms`-shaped list: rejects a present-and-empty list, a name this harness does
/// not know, and a name repeated later in the same list.
fn check_platforms(field: &'static str, platforms: &[String], errors: &mut Vec<ValidationError>) {
    if platforms.is_empty() {
        errors.push(ValidationError::EmptyPlatformList { field });
    }
    for (index, platform) in platforms.iter().enumerate() {
        if !PLATFORMS.contains(&platform.as_str()) {
            errors.push(ValidationError::UnknownPlatform {
                field: Field::at(field, index),
                value: platform.clone(),
            });
        } else if platforms[..index].contains(platform) {
            errors.push(ValidationError::DuplicatePlatform {
                field: Field::at(field, index),
                value: platform.clone(),
            });
        }
    }
}

/// Loads every comparison in `dir`, which holds one `<name>.toml` file per comparison.
///
/// Comparisons are returned in name order. Each is parsed strictly, validated, and checked to be
/// named after its file.
///
/// # Errors
///
/// Returns the first file that cannot be read, parsed, or validated, or that is misnamed, and
/// [`LoadError::Io`] when the directory itself cannot be read.
pub fn load_comparisons(dir: &Path) -> Result<Vec<Comparison>, LoadError> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|source| LoadError::Io {
            context: format!("cannot read the comparison directory `{}`", dir.display()),
            source,
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "toml")
        })
        .collect();
    paths.sort();
    let mut comparisons = Vec::with_capacity(paths.len());
    for path in paths {
        let comparison = Comparison::from_path(&path)?;
        if path.file_stem().and_then(|stem| stem.to_str()) != Some(comparison.name.as_str()) {
            return Err(LoadError::Misnamed {
                path,
                name: comparison.name,
            });
        }
        if let Err(errors) = comparison.validate() {
            return Err(LoadError::Invalid {
                name: comparison.name,
                errors,
            });
        }
        comparisons.push(comparison);
    }
    Ok(comparisons)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comparison(fields: &str) -> Comparison {
        Comparison::from_toml_str(&format!(
            r#"
schema_version = 1
name = "c"
description = "d"
fixture = "f"
timeout_ms = 60000
{fields}
"#
        ))
        .expect("a comparison")
    }

    #[test]
    fn a_motion_ratio_comparison_needs_no_profile_and_fixes_both_sides() {
        let comparison = comparison("budget = \"motion_complete_ratio\"\n");
        comparison.validate().expect("valid");
        assert_eq!(comparison.candidate_profile(), Profile::Default);
        assert_eq!(comparison.baseline_profile(), Some(Profile::ReducedMotion));
    }

    #[test]
    fn a_motion_ratio_comparison_rejects_an_explicit_profile() {
        let comparison = comparison("budget = \"motion_complete_ratio\"\nprofile = \"default\"\n");
        let errors = comparison.validate().expect_err("invalid");
        assert_eq!(errors.errors(), [ValidationError::ProfileNotApplicable]);
    }

    #[test]
    fn a_tui_ratio_comparison_needs_a_profile_and_baselines_against_headless() {
        let comparison =
            comparison("budget = \"tui_complete_ratio\"\nprofile = \"reduced-motion\"\n");
        comparison.validate().expect("valid");
        assert_eq!(comparison.candidate_profile(), Profile::ReducedMotion);
        assert_eq!(comparison.baseline_profile(), None);
    }

    #[test]
    fn a_tui_ratio_comparison_without_a_profile_is_invalid() {
        let comparison = comparison("budget = \"tui_complete_ratio\"\n");
        let errors = comparison.validate().expect_err("invalid");
        assert_eq!(errors.errors(), [ValidationError::ProfileRequired]);
    }

    #[test]
    fn a_non_ratio_budget_is_rejected() {
        let comparison = comparison("budget = \"max_stall_ms\"\n");
        let errors = comparison.validate().expect_err("invalid");
        assert_eq!(
            errors.errors(),
            [ValidationError::UnsupportedBudget {
                budget: Budget::MaxStallMs
            }]
        );
    }

    #[test]
    fn zero_pairs_is_rejected() {
        let comparison = comparison("budget = \"motion_complete_ratio\"\npairs = 0\n");
        let errors = comparison.validate().expect_err("invalid");
        assert_eq!(errors.errors(), [ValidationError::ZeroPairs]);
    }

    #[test]
    fn the_default_pairs_is_five() {
        let comparison = comparison("budget = \"motion_complete_ratio\"\n");
        assert_eq!(comparison.pairs, 5);
    }

    #[test]
    fn a_zero_timeout_is_rejected() {
        let comparison = Comparison::from_toml_str(
            "schema_version = 1\nname = \"c\"\ndescription = \"d\"\nfixture = \"f\"\n\
             budget = \"motion_complete_ratio\"\ntimeout_ms = 0\n",
        )
        .expect("a comparison");
        let errors = comparison.validate().expect_err("invalid");
        assert_eq!(
            errors.errors(),
            [ValidationError::TimeoutOutOfRange { timeout_ms: 0 }]
        );
    }

    #[test]
    fn expect_fail_without_slice_is_rejected() {
        let comparison = comparison("budget = \"motion_complete_ratio\"\nexpect = \"fail\"\n");
        let errors = comparison.validate().expect_err("invalid");
        assert_eq!(
            errors.errors(),
            [ValidationError::ExpectedFailureWithoutSlice]
        );
    }

    #[test]
    fn fails_on_restricted_to_one_platform_is_strict_elsewhere() {
        let comparison = comparison(
            "budget = \"motion_complete_ratio\"\nexpect = \"fail\"\nslice = \"X2\"\n\
             platforms = [\"macos\", \"linux\"]\nfails_on = [\"macos\"]\n",
        );
        comparison.validate().expect("valid");
        assert_eq!(comparison.expect_on("macos"), Expect::Fail);
        assert_eq!(comparison.expect_on("linux"), Expect::Pass);
    }

    #[test]
    fn an_invalid_limit_is_rejected() {
        for limit in ["0.0", "-1.0", "\"nan\""] {
            let text = format!("budget = \"motion_complete_ratio\"\nlimit = {limit}\n");
            if limit == "\"nan\"" {
                // Not a number at all: a TOML type error, caught by parsing rather than validate.
                assert!(Comparison::from_toml_str(&text).is_err());
                continue;
            }
            let comparison = comparison(&text);
            let errors = comparison.validate().expect_err("invalid");
            assert_eq!(errors.errors(), [ValidationError::InvalidLimit]);
        }
    }

    #[test]
    fn a_limit_override_wins_over_the_budget_default() {
        let comparison = comparison("budget = \"motion_complete_ratio\"\nlimit = 2.0\n");
        assert!((comparison.limit() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(
            Comparison::from_toml_str(
                "schema_version = 1\nname = \"c\"\ndescription = \"d\"\nfixture = \"f\"\n\
                 budget = \"motion_complete_ratio\"\ntimeout_ms = 1000\nbogus = true\n"
            )
            .is_err()
        );
    }

    /// The harness README's "Comparison files" section shows one complete comparison example;
    /// this keeps it executable, the same way `scenario::tests::readme` keeps scenario examples
    /// executable, without the two tests parsing each other's shape.
    #[test]
    fn the_readme_comparison_example_parses_and_validates() {
        const README: &str = include_str!("../../README.md");
        const OPEN: &str = "```toml\n";
        const CLOSE: &str = "\n```";
        let mut found = 0;
        let mut rest = README;
        while let Some(start) = rest.find(OPEN) {
            let body = &rest[start + OPEN.len()..];
            let end = body.find(CLOSE).expect("every fenced block is closed");
            let block = &body[..end];
            rest = &body[end + CLOSE.len()..];
            if !block.contains("\nbudget = \"") {
                continue;
            }
            found += 1;
            let comparison = Comparison::from_toml_str(block).unwrap_or_else(|error| {
                panic!("the README comparison example does not parse: {error}\n{block}")
            });
            comparison.validate().unwrap_or_else(|errors| {
                panic!("the README comparison example is not valid: {errors}\n{block}")
            });
        }
        assert!(
            found >= 1,
            "the README should show at least one complete comparison example"
        );
    }
}
