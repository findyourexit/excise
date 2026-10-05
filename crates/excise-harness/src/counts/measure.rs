//! Counting: runs the fixed set of fixtures and writes the `harness-counts` document.
//!
//! Each fixture is counted twice over: a headless scan (`excise --format json`), from whose
//! report come the entries, the bytes the scan store held when the scan ended, and what the scan
//! left behind; and an interactive session, which must be the same scan, and from which come what
//! that session left behind. Every fixture is counted `repeat` times, and the run fails, naming
//! the metric and its values, if any count differs between the runs: a count that depends on
//! timing is not recorded.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;

use crate::{
    bench::{
        CaseError,
        cases::SharedFixture,
        context::{os_description, toolchain},
    },
    events::EventError,
    fixture::Fixtures,
    headless::{
        DocumentError, ScanDocument, ScanError, ScanRequest, document::ScanState, process::Ended,
        run_scan,
    },
    pty::PtyError,
    report::{
        CountsCase, CountsContext, CountsFixture, CountsInvalid, CountsKind, CountsRunner,
        Document, HarnessCounts, MAX_CASES, PullRequestOrigin, SchemaVersion,
    },
    run_support::render_rows,
    safety::{FixtureRoot, SafetyError, ScratchError},
};

use super::{
    PROFILE,
    artifact::{ArtifactError, parse_untrusted},
    interactive, metric,
};

/// The commit that is being counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The 40-character commit.
    pub sha: String,
    /// When it was committed, as an RFC 3339 timestamp.
    pub committed_at: String,
}

/// What to count and where to put the document.
#[derive(Debug, Clone)]
pub struct CountsOptions {
    /// The `excise` binary under test.
    pub binary: PathBuf,
    /// Where fixtures come from.
    pub fixtures: Fixtures,
    /// The fixtures to count, in the order the document lists them. Never empty.
    pub fixture_ids: Vec<String>,
    /// How many times every fixture is counted. The run fails if the counts differ. At least 1.
    pub repeat: u32,
    /// How long one scan or one interactive session may take.
    pub timeout: Duration,
    /// The existing directory the scratch areas are made in.
    pub work_dir: PathBuf,
    /// The file the document is written to.
    pub out: PathBuf,
    /// The commit being counted.
    pub commit: Commit,
    /// The pull request the counts are for, when they are.
    pub pull_request: Option<PullRequestOrigin>,
}

/// A run of one fixture that is about to start.
#[derive(Debug, Clone, Copy)]
pub struct Progress<'a> {
    /// The fixture.
    pub fixture: &'a str,
    /// Its position among the fixtures, from 1.
    pub index: usize,
    /// How many fixtures there are.
    pub total: usize,
    /// Which count of this fixture this is, from 1.
    pub run: u32,
    /// How many counts of it there are.
    pub repeat: u32,
}

/// The counts could not be taken, or are not a document.
#[derive(Debug, Error)]
pub enum CountsError {
    /// No fixture was named.
    #[error("nothing to count: name at least one fixture")]
    NothingToCount,
    /// A count of zero runs was asked for.
    #[error("every fixture must be counted at least once")]
    NoRuns,
    /// More fixtures were named than one document holds.
    #[error(
        "{requested} fixtures were asked for, but a counts document holds at most {limit}: count \
         fewer in one run"
    )]
    TooManyFixtures {
        /// How many were named.
        requested: usize,
        /// How many a document holds.
        limit: usize,
    },
    /// A fixture could not be acquired.
    #[error(transparent)]
    Fixture(#[from] CaseError),
    /// A fixture root failed the ownership check.
    #[error(transparent)]
    Safety(#[from] SafetyError),
    /// The binary cannot be used.
    #[error("cannot use the binary: {0}")]
    Binary(String),
    /// A scan could not be run.
    #[error(transparent)]
    Scan(#[from] ScanError),
    /// A scan report is not a valid `scan-report`.
    #[error(transparent)]
    Report(#[from] DocumentError),
    /// A scratch area could not be made or read.
    #[error(transparent)]
    Scratch(#[from] ScratchError),
    /// The pseudo-terminal session failed.
    #[error(transparent)]
    Pty(#[from] PtyError),
    /// The event channel could not be read.
    #[error(transparent)]
    Events(#[from] EventError),
    /// A scan or a session did not finish in time.
    #[error("{fixture}: gave up waiting for {what} after {timeout:?}")]
    TimedOut {
        /// The fixture.
        fixture: String,
        /// What was being waited for.
        what: &'static str,
        /// The bound that passed.
        timeout: Duration,
    },
    /// The program did something other than what a count needs.
    #[error("{fixture}: {what}")]
    Failed {
        /// The fixture.
        fixture: String,
        /// What happened.
        what: String,
    },
    /// A count differs between the runs of one fixture.
    #[error(
        "{fixture}: `{metric}` is not deterministic: {} over {} runs; a count that depends on \
         timing must not be recorded",
        join(values),
        values.len()
    )]
    Unstable {
        /// The fixture.
        fixture: String,
        /// The metric.
        metric: String,
        /// Its value on each run, in order.
        values: Vec<Option<u64>>,
    },
    /// The counts are not a valid document.
    #[error("the counts are not a valid document: {0}")]
    Invalid(#[from] CountsInvalid),
    /// The counts are a document that the readers of this history would refuse, so it is not
    /// written.
    #[error("the counts are not a document that the readers of the history accept: {0}")]
    NotAccepted(#[from] ArtifactError),
    /// The document could not be rendered.
    #[error("cannot render the counts: {0}")]
    Json(#[from] serde_json::Error),
    /// A file could not be written.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
}

/// `values` as a comma-separated list, `none` where a run had no value.
fn join(values: &[Option<u64>]) -> String {
    values
        .iter()
        .map(|value| value.map_or_else(|| "none".to_owned(), |value| value.to_string()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What one counting run produced.
#[derive(Debug)]
pub struct CountsReport {
    /// The document.
    pub document: HarnessCounts,
    /// Where it was written.
    pub document_path: PathBuf,
}

impl CountsReport {
    /// One row per fixture and count.
    #[must_use]
    pub fn table(&self) -> String {
        let mut rows = vec![["fixture".to_owned(), "count".to_owned(), "value".to_owned()]];
        for case in &self.document.cases {
            for (name, value) in &case.metrics {
                rows.push([case.fixture.id.clone(), name.clone(), value.to_string()]);
            }
        }
        render_rows(&rows)
    }
}

/// Counts every fixture, requires the counts to agree across the runs, and writes the document.
///
/// # Errors
///
/// Returns why a fixture could not be counted, which count was not deterministic, or why the
/// document could not be written.
pub fn run_counts(
    options: &CountsOptions,
    mut progress: impl FnMut(Progress<'_>),
) -> Result<CountsReport, CountsError> {
    if options.fixture_ids.is_empty() {
        return Err(CountsError::NothingToCount);
    }
    if options.repeat == 0 {
        return Err(CountsError::NoRuns);
    }
    if options.fixture_ids.len() > MAX_CASES {
        return Err(CountsError::TooManyFixtures {
            requested: options.fixture_ids.len(),
            limit: MAX_CASES,
        });
    }
    fs::create_dir_all(&options.work_dir).map_err(io_error(format!(
        "cannot create `{}`",
        options.work_dir.display()
    )))?;

    let total = options.fixture_ids.len();
    let mut cases = Vec::with_capacity(total);
    for (position, id) in options.fixture_ids.iter().enumerate() {
        let shared = SharedFixture::acquire(&options.fixtures, id, &options.work_dir)?;
        let identity = shared.identity(id);
        let root = FixtureRoot::open(shared.root())?;
        let mut runs = Vec::new();
        for run in 1..=options.repeat {
            progress(Progress {
                fixture: id,
                index: position + 1,
                total,
                run,
                repeat: options.repeat,
            });
            runs.push(count_once(options, id, &root)?);
        }
        cases.push(CountsCase {
            fixture: CountsFixture {
                id: identity.id,
                hash: identity.hash,
                seed: identity.seed,
            },
            profile: PROFILE,
            metrics: agree(id, &runs)?,
        });
    }

    let document = HarnessCounts {
        document_kind: CountsKind::HarnessCounts,
        schema_version: SchemaVersion,
        context: CountsContext {
            git_sha: options.commit.sha.clone(),
            committed_at: options.commit.committed_at.clone(),
            runner: CountsRunner {
                os: std::env::consts::OS.to_owned(),
                os_version: printable(&os_description()),
                arch: std::env::consts::ARCH.to_owned(),
            },
            toolchain: printable(toolchain().lines().next().unwrap_or_default()),
            pull_request: options.pull_request.clone(),
        },
        cases,
    };
    let text = render_checked(&document)?;
    write_text(&options.out, &text)?;
    Ok(CountsReport {
        document,
        document_path: options.out.clone(),
    })
}

/// The longest free-text field a document carries.
const PRINTABLE_LIMIT: usize = 120;

/// `text` as one line of printable ASCII of at most 120 characters, which is what the schema
/// allows of a free-text field: every other character becomes `?`.
pub(crate) fn printable(text: &str) -> String {
    let line: String = text
        .trim()
        .chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
        .take(PRINTABLE_LIMIT)
        .collect();
    if line.is_empty() {
        "unknown".to_owned()
    } else {
        line
    }
}

fn io_error(context: String) -> impl FnOnce(io::Error) -> CountsError {
    move |source| CountsError::Io { context, source }
}

/// The counts of one run of one fixture.
fn count_once(
    options: &CountsOptions,
    id: &str,
    root: &FixtureRoot,
) -> Result<BTreeMap<String, u64>, CountsError> {
    let mut metrics = BTreeMap::new();

    let run = run_scan(&ScanRequest {
        binary: &options.binary,
        fixture: root,
        work_dir: &options.work_dir,
        profile: PROFILE,
        timeout: options.timeout,
    })?;
    if run.finished.timed_out {
        return Err(CountsError::TimedOut {
            fixture: id.to_owned(),
            what: "the scan",
            timeout: options.timeout,
        });
    }
    if run.finished.ended != Ended::Exited(0) {
        return Err(CountsError::Failed {
            fixture: id.to_owned(),
            what: format!("the scan {}", run.finished.ended.describe()),
        });
    }
    let report = ScanDocument::read(&run.report_path())?;
    if report.state != ScanState::Exact {
        return Err(CountsError::Failed {
            fixture: id.to_owned(),
            what: format!(
                "the scan ended in the state `{}`, and the counts of a scan that is not exact \
                 are not recorded",
                report.state.as_str()
            ),
        });
    }
    let mut residue = run.residue.len();
    metrics.insert(metric::ENTRIES.to_owned(), report.summary.scanned_entries);
    metrics.insert(
        metric::SCAN_STORE_BYTES.to_owned(),
        report.summary.scan_store_bytes,
    );

    if interactive::SUPPORTED {
        residue += interactive::session_residue(
            id,
            &options.binary,
            root,
            &options.work_dir,
            options.timeout,
            report.summary.scanned_entries,
        )?;
    }
    metrics.insert(
        metric::RESIDUE_FILES.to_owned(),
        u64::try_from(residue).unwrap_or(u64::MAX),
    );
    Ok(metrics)
}

/// The counts every run agrees on, or the first one they do not.
fn agree(id: &str, runs: &[BTreeMap<String, u64>]) -> Result<BTreeMap<String, u64>, CountsError> {
    let mut names: Vec<&String> = runs.iter().flat_map(BTreeMap::keys).collect();
    names.sort();
    names.dedup();
    let mut agreed = BTreeMap::new();
    for name in names {
        let values: Vec<Option<u64>> = runs.iter().map(|run| run.get(name).copied()).collect();
        match values.first().copied().flatten() {
            Some(first) if values.iter().all(|value| *value == Some(first)) => {
                agreed.insert(name.clone(), first);
            }
            _ => {
                return Err(CountsError::Unstable {
                    fixture: id.to_owned(),
                    metric: name.clone(),
                    values,
                });
            }
        }
    }
    Ok(agreed)
}

/// Renders `document`, and holds the text to everything that a reader of it will hold it to: the
/// rules of [`HarnessCounts::check`], the schema (its limits on the cases and counts, the pattern
/// of every string), and the size bound. What this returns is what `read_untrusted` accepts, so
/// a document that no reader would take is never written.
fn render_checked(document: &HarnessCounts) -> Result<String, CountsError> {
    document.check()?;
    let text = document.to_json_pretty()?;
    parse_untrusted(text.as_bytes())?;
    Ok(text)
}

/// Writes `text`, the rendered document, to `path`, so that a reader never sees half of it.
fn write_text(path: &Path, text: &str) -> Result<(), CountsError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(io_error(format!("cannot create `{}`", parent.display())))?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(io_error(format!(
        "cannot create a file in `{}`",
        parent.display()
    )))?;
    file.write_all(text.as_bytes())
        .map_err(io_error(format!("cannot write `{}`", path.display())))?;
    file.persist(path).map_err(|error| CountsError::Io {
        context: format!("cannot write `{}`", path.display()),
        source: error.error,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counts::test_support::{HASH, case, commit, record, suite};

    fn run(metrics: &[(&str, u64)]) -> BTreeMap<String, u64> {
        metrics
            .iter()
            .map(|(name, value)| ((*name).to_owned(), *value))
            .collect()
    }

    #[test]
    fn counts_that_every_run_agrees_on_are_the_counts() {
        let runs = [
            run(&[("entries", 5), ("scan_store_bytes", 99)]),
            run(&[("entries", 5), ("scan_store_bytes", 99)]),
            run(&[("entries", 5), ("scan_store_bytes", 99)]),
        ];

        let agreed = agree("wide-1k", &runs).expect("deterministic");

        assert_eq!(agreed, run(&[("entries", 5), ("scan_store_bytes", 99)]));
    }

    #[test]
    fn a_count_that_differs_between_runs_is_refused_with_every_value_it_took() {
        let runs = [
            run(&[("entries", 5), ("scan_store_bytes", 99)]),
            run(&[("entries", 5), ("scan_store_bytes", 107)]),
            run(&[("entries", 5), ("scan_store_bytes", 99)]),
        ];

        let error = agree("node-modules-2k", &runs).expect_err("not deterministic");

        match &error {
            CountsError::Unstable {
                fixture,
                metric,
                values,
            } => {
                assert_eq!(fixture, "node-modules-2k");
                assert_eq!(metric, "scan_store_bytes");
                assert_eq!(values, &[Some(99), Some(107), Some(99)]);
            }
            other => panic!("expected an unstable count, got {other:?}"),
        }
        assert_eq!(
            error.to_string(),
            "node-modules-2k: `scan_store_bytes` is not deterministic: 99, 107, 99 over 3 runs; a \
             count that depends on timing must not be recorded"
        );
    }

    #[test]
    fn a_count_that_one_run_lacks_is_not_deterministic_either() {
        let runs = [
            run(&[("entries", 5), ("residue_files", 2)]),
            run(&[("entries", 5)]),
        ];

        let error = agree("wide-1k", &runs).expect_err("one run has no residue count");

        assert!(
            matches!(&error, CountsError::Unstable { metric, values, .. }
                if metric == "residue_files" && values == &[Some(2), None]),
            "{error:?}"
        );
        assert!(error.to_string().contains("2, none over 2 runs"), "{error}");
    }

    #[test]
    fn a_single_run_is_its_own_agreement() {
        let runs = [run(&[("entries", 5)])];

        assert_eq!(agree("x", &runs).expect("one run"), run(&[("entries", 5)]));
    }

    #[test]
    fn free_text_is_made_into_one_line_of_printable_ascii_within_the_limit() {
        assert_eq!(printable("  Ubuntu 24.04.3 LTS \n"), "Ubuntu 24.04.3 LTS");
        assert_eq!(printable("Ubuntu\n24.04\t\u{e9}"), "Ubuntu?24.04??");
        assert_eq!(printable(""), "unknown");
        assert_eq!(printable(" \n "), "unknown");
        assert_eq!(printable(&"x".repeat(500)).len(), 120);
    }

    #[test]
    fn what_the_writer_produces_for_free_text_always_passes_the_schema_pattern() {
        let pattern = regex::Regex::new("^[ -~]{1,120}$").expect("the schema's pattern");
        for text in [
            "",
            "Darwin 27.0.1",
            "caf\u{e9}",
            "line\nbreak",
            "\u{1b}[2J",
            &"y".repeat(1_000),
        ] {
            assert!(pattern.is_match(&printable(text)), "{text:?}");
        }
    }

    #[test]
    fn the_table_has_a_row_for_every_count() {
        let document = record(&commit(1), suite(350));
        let report = CountsReport {
            document,
            document_path: PathBuf::from("counts.json"),
        };

        let table = report.table();

        assert_eq!(table.lines().count(), 1 + 6, "{table}");
        assert!(
            table
                .lines()
                .next()
                .expect("a header")
                .starts_with("fixture")
        );
        assert!(table.contains("wide-1k"));
        assert!(table.contains("scan_store_bytes") && table.contains("350"));
    }

    /// Options for a run that has nothing to run: no binary, no work area.
    fn options(fixture_ids: Vec<String>, repeat: u32) -> CountsOptions {
        CountsOptions {
            binary: PathBuf::from("/definitely/not/a/binary"),
            fixtures: Fixtures::bundled(),
            fixture_ids,
            repeat,
            timeout: Duration::from_secs(1),
            work_dir: PathBuf::from("/definitely/not/a/directory"),
            out: PathBuf::from("/definitely/not/counts.json"),
            commit: Commit {
                sha: commit(1),
                committed_at: "2026-10-05T10:35:18+11:00".to_owned(),
            },
            pull_request: None,
        }
    }

    #[test]
    fn nothing_to_count_and_no_runs_are_refused_before_any_binary_is_used() {
        assert!(matches!(
            run_counts(&options(Vec::new(), 1), |_| {}),
            Err(CountsError::NothingToCount)
        ));
        assert!(matches!(
            run_counts(&options(vec!["wide-1k".to_owned()], 0), |_| {}),
            Err(CountsError::NoRuns)
        ));
    }

    /// A document holds at most 16 cases, so a run that would count 17 distinct fixtures is
    /// refused at once, naming the limit, and not after it has counted them all.
    #[test]
    fn more_fixtures_than_a_document_holds_are_refused_before_any_work() {
        let fixtures = |count: usize| -> Vec<String> {
            (0..count)
                .map(|index| format!("fixture-{index:02}"))
                .collect()
        };

        let error = run_counts(&options(fixtures(17), 1), |_| {})
            .expect_err("17 fixtures do not fit one document")
            .to_string();
        assert!(
            error.contains("17 fixtures") && error.contains("at most 16"),
            "{error}"
        );

        let error = run_counts(&options(fixtures(16), 1), |_| {})
            .expect_err("these fixtures do not exist")
            .to_string();
        assert!(
            !error.contains("at most 16"),
            "16 is within the limit and is refused for another reason: {error}"
        );
    }

    /// `check` knows only the rules a schema cannot say, so a document that breaks the schema's
    /// own limits or patterns passes it. What is written is held to the schema as well: whatever
    /// the reader refuses, the writer does not write.
    #[test]
    fn a_document_that_breaks_the_schema_is_not_rendered_even_where_check_accepts_it() {
        let many = |count: usize| -> Vec<CountsCase> {
            (0..count)
                .map(|index| case(&format!("fixture-{index:02}"), HASH, &[("entries", 1)]))
                .collect()
        };
        let mut too_many_cases = record(&commit(1), many(MAX_CASES + 1));
        assert!(
            too_many_cases.check().is_ok(),
            "the rules of `check` do not include the schema's limits"
        );
        let error = render_checked(&too_many_cases)
            .expect_err("the schema allows no more cases than that")
            .to_string();
        assert!(
            error.contains("breaks the harness-counts schema") && error.contains("/cases"),
            "{error}"
        );
        too_many_cases.cases.truncate(MAX_CASES);
        assert!(
            render_checked(&too_many_cases).is_ok(),
            "exactly the limit is a document"
        );

        let mut too_many_counts = record(&commit(1), many(1));
        too_many_counts.cases[0].metrics = (0..33)
            .map(|index| (format!("count_{index:02}"), 1))
            .collect();
        let error = render_checked(&too_many_counts)
            .expect_err("the schema allows 32 counts in a case")
            .to_string();
        assert!(error.contains("/cases/0/metrics"), "{error}");

        let mut unprintable = record(&commit(1), many(1));
        unprintable.context.toolchain = "rustc 1.98.0\nnot a second line".to_owned();
        let error = render_checked(&unprintable)
            .expect_err("free text is one line of printable ASCII")
            .to_string();
        assert!(error.contains("/context/toolchain"), "{error}");
    }
}
