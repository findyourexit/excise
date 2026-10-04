//! The headless suite: every selected fixture, scanned, held to its oracle, and timed against
//! `du -sk`.
//!
//! For each fixture the suite
//!
//! 1. gets the fixture: the cached master, which headless runs only read, or a fresh run copy,
//!    removed when the fixture is done, for a fixture that is never cached because `cargo clean`
//!    could not remove it (see
//!    [`FixtureSpec::removable_by_path`](crate::fixture::FixtureSpec::removable_by_path)) and
//!    for a fixture with a volume part whose volume is attached (privileged, behind
//!    `EXCISE_HARNESS_PRIVILEGED=1`);
//! 2. walks it for the oracle;
//! 3. runs the scan once, and `du -sk` once, to warm the binary and the page cache, and drops both
//!    timings;
//! 4. runs the scan and `du -sk` alternately, scan first, `repeat` times each: the measured pairs;
//! 5. checks that the fixture is still the tree the oracle saw;
//! 6. diffs every scan's report against the oracle (see [`diff`](super::diff)), after the timed
//!    runs so that parsing a report never sits between two timings;
//! 7. resolves a verdict against the expected failures (see
//!    [`expectations`](super::expectations)).
//!
//! The result is a [`SuiteReport`]: one [`FixtureReport`] per fixture, a verdict table, and a
//! `harness-summary` document, written to `<out_root>/<run-id>/summary.json`.

mod render;

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use thiserror::Error;

use crate::{
    fixture::{
        FixtureError, Fixtures, Oracle, OracleError, PrivilegedOptIn, SpecError,
        spec::{FixtureSpec, Part},
    },
    metrics::CpuTimes,
    report::{
        BinaryIdentity, Document, HarnessSummary, SchemaVersion, SummaryKind, Tier, TimingWarning,
        Verdict,
    },
    run_support::{compact_utc, host_name, point_latest_at, rfc3339, sha256_file},
    runner::work_base,
    safety::{FixtureRoot, SafetyError, ScratchError},
    scenario::{Budget, Profile},
    string_enum::string_enum,
};

use super::{
    diff::{Diff, Discrepancy, DiscrepancyKind, Scan, diff},
    document::{DocumentError, ScanDocument, path_bytes},
    du::{Du, DuError, DuFlavor},
    expectations::{Expectations, ExpectedFailure, ExpectedMemoryFailure, RatioExpectedFailure},
    pairs::{Spread, millis, ratios},
    process::Ended,
    scan::{ScanError, ScanRequest, ScanRun, run_scan},
};

/// The measured pairs per fixture that the validation program asks for (section 4: the median of
/// five interleaved runs).
pub const DEFAULT_REPEAT: u32 = 5;

/// The largest fixture, in planned entries, that the quick tier selects by default.
pub const QUICK_MAX_ENTRIES: u64 = 10_000;

/// The largest fixture, in planned entries, that the full tier selects by default: the pull-request
/// tier's limit.
pub const FULL_MAX_ENTRIES: u64 = 250_000;

/// The scan-time multiple of `du -sk` a headless scan is budgeted to. Gated per platform and
/// fixture through `expectations/headless.toml` (see [`Expectations::expected_ratio_failure`])
/// on a fixture whose ratio is gated; see [`MIN_GATED_ENTRIES`]. Set from measurements on macOS
/// and Linux once the per-run durable writes were gone and the report writer was buffered: the
/// gated fixtures' medians then fell between 4.6x and 11.5x on both platforms. The remaining cost
/// is CPU that `du`'s stat-only walk does not pay: publishing the scan store's page index, then
/// building and serializing the report. Revisited only at a maintainer recalibration.
pub const RATIO_BUDGET: f64 = 15.0;

/// The smallest oracle entry count, fixed by a fixture's spec and seed, gated against
/// [`RATIO_BUDGET`]. Below it the ratio is reported but never gated. This is a count, not a
/// measured time, so which fixtures are gated never depends on how loaded the machine was: an
/// earlier, measured-`du`-time threshold let one fixture's ratio verdict flip between runs,
/// because its median `du` time landed right at the boundary under load. The harness README,
/// "Expected failures", names the fixtures this currently excludes.
pub const MIN_GATED_ENTRIES: u64 = 2_000;

/// The process memory contract: the peak-memory budget both the headless gate (`measure`, below)
/// and the Linux cgroup cap (`safety::cgroup`) check a scan against, 512 MiB. The same figure as
/// `Budget::PeakRssBytes`'s default (`runner::budget`) and `excise`'s own `EXCISE_MEMORY_MIB`
/// default (`src/config.rs`); kept here, not in `safety::cgroup`, because the cap is a mechanism
/// for enforcing this budget on Linux, not a budget of its own, and the PTY runner's cgroup wrap
/// (`runner::run`) imports it from here for the same reason.
pub const MEMORY_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// How large a report may be, in bytes, for every run of a fixture to keep its report until the
/// diffs are made. Above it, only the first run's report is kept, to bound the disk a large
/// fixture uses.
const KEEP_EVERY_REPORT_UP_TO: u64 = 64 * 1024 * 1024;

string_enum! {
    /// A class of fixture (decision D4.8), by what its specification generates.
    pub enum Class {
        /// Scale shapes: wide, deep past `PATH_MAX`, `node_modules`-shaped, many tiny files.
        Scale => "scale",
        /// Identity edge cases: hard links, links that dangle or loop, sparse files, clones.
        Identity => "identity",
        /// Hostile inputs: unreadable entries, and control, bidi, escape, newline, and long names.
        Hostile => "hostile",
        /// Volumes: a mount boundary. Needs privileges.
        Volumes => "volumes",
    }
}

impl Class {
    /// The classes a specification generates.
    #[must_use]
    pub fn of(spec: &FixtureSpec) -> BTreeSet<Self> {
        spec.parts
            .iter()
            .filter_map(|part| match part {
                Part::Tree(_) | Part::Deep(_) => Some(Self::Scale),
                Part::Identity(_) => Some(Self::Identity),
                Part::Hostile(_) => Some(Self::Hostile),
                Part::Volume(_) => Some(Self::Volumes),
                Part::File(_) => None,
            })
            .collect()
    }
}

/// What to run and where to put the results.
#[derive(Debug, Clone)]
pub struct SuiteOptions {
    /// The `excise` binary under test.
    pub binary: PathBuf,
    /// Where the fixtures come from.
    pub fixtures: Fixtures,
    /// The tier: it limits the size of the fixtures that are selected when none is named.
    pub tier: Tier,
    /// The fixtures to run, by id. They run whatever their size.
    pub fixture_ids: Vec<String>,
    /// The classes to run: every fixture that generates one of them, up to the tier's size.
    pub classes: Vec<Class>,
    /// The profile to scan under: `default` or `deterministic`.
    pub profile: Profile,
    /// The measured pairs of scan and `du` per fixture. Zero measures nothing and only checks.
    pub repeat: u32,
    /// How long one scan or one `du` may take before it is killed.
    pub timeout: Duration,
    /// Keeps each fixture's scratch areas, and with them the reports, instead of deleting them.
    pub keep: bool,
    /// The output root, normally `target/excise-headless`.
    pub out_root: PathBuf,
    /// The directory fixtures' scratch areas are made in. `None` uses
    /// [`work_base`](crate::runner::work_base).
    pub work_dir: Option<PathBuf>,
    /// The 40-character commit the binary was built from.
    pub git_sha: String,
    /// The operator's decision to attach volumes, which a volume fixture needs.
    pub privileged: Option<PrivilegedOptIn>,
    /// The fixtures that are expected to fail, and why.
    pub expectations: Expectations,
    /// Whether a ratio over [`RATIO_BUDGET`] is reported as a warning and does not fail the
    /// fixture: for a hosted machine whose speed the budget was not set on. The oracle diff, the
    /// memory budget, and a scan or `du` that does not end in time still fail it. A ratio that an
    /// `[[expect_ratio_fail]]` entry documents keeps its strict verdict (`xfail`, or `xpass` when
    /// it is within the budget).
    pub timing_informational: bool,
}

/// The suite could not be run.
#[derive(Debug, Error)]
pub enum SuiteError {
    /// Nothing matched the selection.
    #[error("no fixture matches the selection ({0})")]
    NothingToRun(String),
    /// A fixture was named that does not exist.
    #[error("unknown fixture `{id}`; the fixtures are {}", known.join(", "))]
    UnknownFixture {
        /// The id that was asked for.
        id: String,
        /// The ids there are.
        known: Vec<String>,
    },
    /// A fixture's specification cannot be loaded.
    #[error(transparent)]
    Spec(#[from] SpecError),
    /// The profile only changes how the terminal looks.
    #[error(
        "the `{0}` profile only changes how the terminal looks and does not apply to a headless scan"
    )]
    Profile(Profile),
    /// A file or directory could not be read or written.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
    /// The summary could not be rendered.
    #[error("cannot render the run summary: {0}")]
    Summary(#[from] serde_json::Error),
}

fn io_error(context: impl Into<String>) -> impl FnOnce(io::Error) -> SuiteError {
    let context = context.into();
    move |source| SuiteError::Io { context, source }
}

/// What a fixture's volume parts came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Volumes {
    /// The fixture has no volume part.
    None,
    /// The fixture has a volume part and no volume was attached, so the mount point is an empty
    /// directory and the scan crosses no boundary. Attaching a volume needs
    /// `EXCISE_HARNESS_PRIVILEGED=1`.
    Detached,
    /// This many volumes were attached, each a mount boundary the scan must not cross.
    Attached(usize),
}

/// What one scan took and used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanMeasure {
    /// Wall time, from just before the spawn to the exit.
    pub wall: Duration,
    /// CPU time, where the platform says.
    pub cpu: Option<CpuTimes>,
    /// Peak memory, where the platform says.
    pub peak_memory_bytes: Option<u64>,
    /// The Linux cgroup v2 `memory.peak` of the scope the scan ran in, when the
    /// `EXCISE_HARNESS_CGROUP=1` opt-in wrapped it (see `safety::cgroup`). `None` off Linux,
    /// without the opt-in, or when the counter could not be read.
    pub cgroup_memory_peak_bytes: Option<u64>,
    /// How the process ended.
    pub ended: Ended,
    /// Whether the deadline killed it.
    pub timed_out: bool,
    /// The size of the report it wrote, when it wrote one.
    pub report_bytes: Option<u64>,
}

/// What one `du -sk` run took and printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuMeasure {
    /// Wall time, from just before the spawn to the exit.
    pub wall: Duration,
    /// The total it printed, in KiB.
    pub kib: Option<u64>,
    /// How the process ended.
    pub ended: Ended,
}

/// One round: a scan, and the `du` that followed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Round {
    /// Whether this is the warm-up round, whose timings are not used.
    pub warm_up: bool,
    /// The scan.
    pub scan: ScanMeasure,
    /// The `du` that followed it, when there is a `du`.
    pub du: Option<DuMeasure>,
    /// Whether the scan's report was held to the oracle.
    pub diffed: bool,
}

/// The result of one fixture.
#[derive(Debug, Clone)]
pub struct FixtureReport {
    /// The fixture's id.
    pub fixture: String,
    /// The classes it belongs to.
    pub classes: Vec<Class>,
    /// The entries of its tree, the root included, as the oracle walked it.
    pub entries: u64,
    /// The profile it was scanned under.
    pub profile: Profile,
    /// What came of its volume parts.
    pub volumes: Volumes,
    /// The verdict, with expected failures applied.
    pub verdict: Verdict,
    /// The expected failure that applies to it, if any.
    pub expectation: Option<ExpectedFailure>,
    /// Whether this fixture's ratio is gated against [`RATIO_BUDGET`]: see
    /// [`FixtureReport::ratio_is_gated`].
    pub ratio_gated: bool,
    /// The ratio-budget expectation that applies to this fixture on this platform, if any.
    pub ratio_expectation: Option<RatioExpectedFailure>,
    /// The ratio check the fixture missed without failing for it, because the suite held timing
    /// informational: its median ratio over [`RATIO_BUDGET`], at most once.
    pub timing_warnings: Vec<TimingWarning>,
    /// The highest `peak_memory_bytes` seen across its rounds, where the platform could measure
    /// it.
    pub memory_peak_bytes: Option<u64>,
    /// The budget `memory_peak_bytes` was checked against, when it was checked.
    pub memory_budget_bytes: Option<u64>,
    /// The memory-budget verdict: `pass` or `fail` against the plain budget, or `xfail`/`xpass`
    /// against `memory_expectation`. Stays `pass` when nothing was measured (for example on
    /// Windows, which this crate has no safe way to sample).
    pub memory_verdict: Verdict,
    /// The expected memory failure that applies to it, if any.
    pub memory_expectation: Option<ExpectedMemoryFailure>,
    /// The warm-up round, then the measured rounds, in the order they ran.
    pub rounds: Vec<Round>,
    /// The diff of every report that was held to the oracle, with the index of its round.
    pub diffs: Vec<(usize, Diff)>,
    /// Which `du` it was timed against.
    pub du_flavor: Option<DuFlavor>,
    /// What `du -sk` must print for this tree.
    pub du_expected_kib: Option<u64>,
    /// How long the fixture took to generate, when it was generated now.
    pub generation: Option<Duration>,
    /// How long the oracle walk took.
    pub oracle_time: Duration,
    /// The directory with the evidence of a failure.
    pub failure_dir: Option<PathBuf>,
    /// Where the scratch areas are, when they were kept.
    pub kept: Option<PathBuf>,
    /// Why the harness could not run the fixture, when it could not.
    pub error: Option<String>,
    /// How long the fixture took in all.
    pub duration: Duration,
}

impl FixtureReport {
    fn new(fixture: &str, classes: &BTreeSet<Class>, profile: Profile, entries: u64) -> Self {
        Self {
            fixture: fixture.to_owned(),
            classes: classes.iter().copied().collect(),
            entries,
            profile,
            volumes: Volumes::None,
            verdict: Verdict::Error,
            expectation: None,
            ratio_gated: false,
            ratio_expectation: None,
            timing_warnings: Vec::new(),
            memory_peak_bytes: None,
            memory_budget_bytes: None,
            memory_verdict: Verdict::Pass,
            memory_expectation: None,
            rounds: Vec::new(),
            diffs: Vec::new(),
            du_flavor: None,
            du_expected_kib: None,
            generation: None,
            oracle_time: Duration::ZERO,
            failure_dir: None,
            kept: None,
            error: None,
            duration: Duration::ZERO,
        }
    }

    /// The rounds whose timings count: every round but the warm-up.
    pub fn measured(&self) -> impl Iterator<Item = &Round> {
        self.rounds.iter().filter(|round| !round.warm_up)
    }

    /// The kinds of discrepancy any report showed.
    #[must_use]
    pub fn kinds(&self) -> BTreeSet<DiscrepancyKind> {
        self.diffs
            .iter()
            .flat_map(|(_, diff)| diff.counts.keys().copied())
            .collect()
    }

    /// The wall times of the measured scans, in milliseconds.
    #[must_use]
    pub fn scan_millis(&self) -> Vec<f64> {
        self.measured()
            .map(|round| millis(round.scan.wall))
            .collect()
    }

    /// The wall times of the measured `du` runs, in milliseconds.
    #[must_use]
    pub fn du_millis(&self) -> Vec<f64> {
        self.measured()
            .filter_map(|round| round.du.as_ref())
            .map(|du| millis(du.wall))
            .collect()
    }

    /// The spread of the scan time over the `du` time of each measured pair.
    #[must_use]
    pub fn ratio_spread(&self) -> Option<Spread> {
        let (scans, references): (Vec<Duration>, Vec<Duration>) = self
            .measured()
            .filter_map(|round| Some((round.scan.wall, round.du.as_ref()?.wall)))
            .unzip();
        Spread::of(&ratios(&scans, &references))
    }

    /// Whether this fixture's ratio is gated against [`RATIO_BUDGET`]: its oracle entry count
    /// (fixed by its spec and seed) is at least [`MIN_GATED_ENTRIES`]. Below it the ratio is
    /// reported but never gated.
    #[must_use]
    pub const fn ratio_is_gated(&self) -> bool {
        self.entries >= MIN_GATED_ENTRIES
    }

    /// Whether the measured ratio exceeds [`RATIO_BUDGET`], when it was measured at all.
    #[must_use]
    pub fn ratio_over_budget(&self) -> Option<bool> {
        self.ratio_spread()
            .map(|spread| spread.median > RATIO_BUDGET)
    }

    /// Whether every `du` printed what the oracle says it must, which is to say that it walked the
    /// whole tree. `None` when there is no `du` or no expected total.
    #[must_use]
    pub fn du_matches_oracle(&self) -> Option<bool> {
        let expected = self.du_expected_kib?;
        let mut totals = self
            .rounds
            .iter()
            .filter_map(|round| round.du.as_ref())
            .map(|du| du.kib)
            .peekable();
        totals.peek()?;
        Some(totals.all(|kib| kib == Some(expected)))
    }

    /// Whether the `du` runs the ratio is measured against are a valid reference: every measured
    /// round has a `du` that exited with code 0, and no `du` printed a total the oracle
    /// contradicts. A `du` that failed, was killed, or walked only part of the tree times
    /// something other than the scan's work, so its ratio says nothing about the scan.
    #[must_use]
    pub fn du_reference_is_valid(&self) -> bool {
        self.measured().all(|round| {
            round
                .du
                .as_ref()
                .is_some_and(|du| du.ended.code() == Some(0))
        }) && self.du_matches_oracle() != Some(false)
    }

    /// The diff of the first report that was not clean.
    #[must_use]
    pub fn first_failure(&self) -> Option<&(usize, Diff)> {
        self.diffs.iter().find(|(_, diff)| !diff.is_clean())
    }
}

/// The result of a whole suite.
#[derive(Debug)]
pub struct SuiteReport {
    /// The `harness-summary` document that was written.
    pub summary: HarnessSummary,
    /// Where it was written.
    pub summary_path: PathBuf,
    /// The output directory of this run.
    pub run_dir: PathBuf,
    /// Every fixture that ran, in the order it ran.
    pub fixtures: Vec<FixtureReport>,
    /// Whether the fixtures that have volume parts had volumes attached.
    pub volumes_attached: bool,
    /// The flavor of the `du` the scans were timed against, when there is one.
    pub du: Option<DuFlavor>,
}

impl SuiteReport {
    /// Whether no fixture has a blocking verdict: a `fail`, `xpass`, or `error`, on the oracle
    /// diff, the ratio budget, or the memory budget, fails the run.
    #[must_use]
    pub fn is_success(&self) -> bool {
        !self
            .fixtures
            .iter()
            .any(|fixture| fixture.verdict.blocks_run())
    }
}

/// A fixture the suite will run.
struct Planned {
    id: String,
    classes: BTreeSet<Class>,
    entries: u64,
    needs_volumes: bool,
    cacheable: bool,
}

/// What the suite tells a caller as it goes.
#[derive(Debug, Clone, Copy)]
pub enum Progress<'a> {
    /// A fixture is about to run.
    Started {
        /// Its id.
        fixture: &'a str,
        /// Its position, from one.
        index: usize,
        /// How many fixtures there are.
        total: usize,
    },
    /// A fixture has finished.
    Finished(&'a FixtureReport),
}

/// Runs the suite and writes the summary.
///
/// A fixture that fails, or that the harness cannot run, is a result in the report and not an
/// error of the suite.
///
/// # Errors
///
/// Returns [`SuiteError`] when the selection is empty or names an unknown fixture, when the
/// profile does not apply to a scan, or when the output directory or the summary cannot be
/// written.
pub fn run_suite(
    options: &SuiteOptions,
    mut progress: impl FnMut(Progress<'_>),
) -> Result<SuiteReport, SuiteError> {
    if !matches!(options.profile, Profile::Default | Profile::Deterministic) {
        return Err(SuiteError::Profile(options.profile));
    }
    let planned = select(options)?;
    let work_dir = options.work_dir.clone().unwrap_or_else(work_base);
    fs::create_dir_all(&work_dir).map_err(io_error(format!(
        "cannot create the work directory `{}`",
        work_dir.display()
    )))?;
    let started_at = SystemTime::now();
    let run_id = format!("{}-{}", compact_utc(started_at), std::process::id());
    let run_dir = options.out_root.join(&run_id);
    fs::create_dir_all(&run_dir)
        .map_err(io_error(format!("cannot create `{}`", run_dir.display())))?;

    let du = Du::find();
    let context = Context {
        options,
        work_dir: &work_dir,
        run_dir: &run_dir,
        du: du.as_ref(),
    };
    let total = planned.len();
    let mut fixtures = Vec::with_capacity(total);
    for (index, fixture) in planned.iter().enumerate() {
        progress(Progress::Started {
            fixture: &fixture.id,
            index: index + 1,
            total,
        });
        let report = run_fixture(&context, fixture);
        progress(Progress::Finished(&report));
        fixtures.push(report);
    }

    let tier = if planned
        .iter()
        .any(|fixture| fixture.entries > FULL_MAX_ENTRIES)
    {
        options.tier.max(Tier::Nightly)
    } else {
        options.tier
    };
    let summary = HarnessSummary {
        document_kind: SummaryKind::default(),
        schema_version: SchemaVersion,
        run_id,
        tier,
        started_at: rfc3339(started_at),
        finished_at: rfc3339(SystemTime::now()),
        host: host_name(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        excise_binary: BinaryIdentity {
            path: options.binary.display().to_string(),
            sha256: sha256_file(&options.binary).map_err(io_error(format!(
                "cannot hash `{}`",
                options.binary.display()
            )))?,
        },
        git_sha: options.git_sha.clone(),
        latency_budget_scale: None,
        timing_informational: options.timing_informational,
        scenarios: fixtures.iter().map(render::result_of).collect(),
    };
    let summary_path = run_dir.join("summary.json");
    fs::write(&summary_path, summary.to_json_pretty()?).map_err(io_error(format!(
        "cannot write `{}`",
        summary_path.display()
    )))?;
    point_latest_at(&options.out_root, &summary.run_id)
        .map_err(io_error("cannot update the `latest` pointer"))?;
    Ok(SuiteReport {
        summary,
        summary_path,
        run_dir,
        fixtures,
        volumes_attached: options.privileged.is_some(),
        du: du.as_ref().map(Du::flavor),
    })
}

// ---------------------------------------------------------------------------------------------
// Selection.

/// Which fixtures to run.
fn select(options: &SuiteOptions) -> Result<Vec<Planned>, SuiteError> {
    let known = options
        .fixtures
        .ids()
        .map_err(io_error("cannot read the fixture specifications"))?;
    for id in &options.fixture_ids {
        if !known.contains(id) {
            return Err(SuiteError::UnknownFixture {
                id: id.clone(),
                known,
            });
        }
    }
    let cap = match options.tier {
        Tier::Quick => QUICK_MAX_ENTRIES,
        Tier::Full => FULL_MAX_ENTRIES,
        Tier::Nightly | Tier::Weekly => u64::MAX,
    };
    let narrowed = !options.fixture_ids.is_empty() || !options.classes.is_empty();
    let mut planned = Vec::new();
    for id in known {
        let spec = options.fixtures.spec(&id)?;
        let classes = Class::of(&spec);
        let entries = spec.planned_entry_count();
        let named = options.fixture_ids.contains(&id);
        let in_class = classes.iter().any(|class| options.classes.contains(class));
        let selected = if narrowed {
            named || (in_class && entries <= cap)
        } else {
            entries <= cap
        };
        if !selected {
            continue;
        }
        let needs_volumes = spec.has_volumes();
        let cacheable = spec.removable_by_path();
        planned.push(Planned {
            id,
            classes,
            entries,
            needs_volumes,
            cacheable,
        });
    }
    if planned.is_empty() {
        let what = if narrowed {
            "the fixtures and classes named".to_owned()
        } else {
            format!("the {} tier", options.tier)
        };
        return Err(SuiteError::NothingToRun(what));
    }
    Ok(planned)
}

// ---------------------------------------------------------------------------------------------
// One fixture.

struct Context<'a> {
    options: &'a SuiteOptions,
    work_dir: &'a Path,
    run_dir: &'a Path,
    du: Option<&'a Du>,
}

/// Why the harness could not run a fixture.
#[derive(Debug, Error)]
enum StepError {
    #[error("cannot create a work directory: {0}")]
    Workspace(io::Error),
    #[error(transparent)]
    Fixture(#[from] FixtureError),
    #[error(transparent)]
    Oracle(#[from] OracleError),
    #[error(transparent)]
    Safety(#[from] SafetyError),
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error(transparent)]
    Du(#[from] DuError),
    #[error(transparent)]
    Scratch(#[from] ScratchError),
}

fn run_fixture(context: &Context<'_>, planned: &Planned) -> FixtureReport {
    let started = Instant::now();
    let mut report = FixtureReport::new(
        &planned.id,
        &planned.classes,
        context.options.profile,
        planned.entries,
    );
    report.expectation = context
        .options
        .expectations
        .expected_failure(&planned.id)
        .cloned();
    report.memory_expectation = context
        .options
        .expectations
        .expected_memory_failure(&planned.id)
        .cloned();
    if let Err(error) = measure(context, planned, &mut report) {
        report.verdict = Verdict::Error;
        report.error = Some(error.to_string());
    }
    report.duration = started.elapsed();
    report
}

/// The verdict of a fixture whose runs showed `observed` discrepancy kinds.
fn resolve(expected: Option<&ExpectedFailure>, observed: &BTreeSet<DiscrepancyKind>) -> Verdict {
    match expected {
        None if observed.is_empty() => Verdict::Pass,
        Some(_) if observed.is_empty() => Verdict::Xpass,
        Some(expected) if *observed == expected.kinds => Verdict::Xfail,
        None | Some(_) => Verdict::Fail,
    }
}

/// The verdict of a fixture's single-bit budget check (the ratio budget or the memory budget),
/// when it applies at all (`report.ratio_gated` for the ratio; the memory budget always
/// applies): the same strict-xfail shape as [`resolve`], with one over/under-budget bit standing
/// in for discrepancy kinds, and `expected` collapsed to whether any applicable expectation
/// exists (a caller already resolved which one, if any, through
/// [`crate::headless::expectations::Expectations`]). Shared by both budgets: a fixture with no
/// expectation for this platform must be within budget, exactly like an undocumented oracle-diff
/// discrepancy.
fn resolve_budget_check(expected: bool, over_budget: bool) -> Verdict {
    match (expected, over_budget) {
        (false, false) => Verdict::Pass,
        (true, false) => Verdict::Xpass,
        (true, true) => Verdict::Xfail,
        (false, true) => Verdict::Fail,
    }
}

/// The verdict of a gated fixture's ratio budget.
///
/// A ratio over [`RATIO_BUDGET`] fails the fixture when no `[[expect_ratio_fail]]` entry documents
/// it on this platform, unless `informational` and the `du` runs it was measured against are a
/// valid reference ([`FixtureReport::du_reference_is_valid`]): then the miss is recorded in
/// `report.timing_warnings` and the verdict is `pass`. Against an invalid reference the ratio
/// keeps its strict verdict, so a failed `du` can never turn into a warning. A ratio that an entry
/// documents keeps the strict-xfail verdict whether or not the run is informational (`xfail` over
/// the budget, `xpass` within it), because excusing it would turn a documented defect into an
/// `xpass`.
fn judge_ratio(report: &mut FixtureReport, informational: bool) -> Verdict {
    let expected = report.ratio_expectation.is_some();
    let median = report.ratio_spread().map(|spread| spread.median);
    let over_budget = median.is_some_and(|median| median > RATIO_BUDGET);
    let excusable = informational && !expected && report.du_reference_is_valid();
    match median {
        Some(value) if over_budget && excusable => {
            report.timing_warnings.push(TimingWarning {
                budget: Budget::HeadlessScanRatio,
                metric: Budget::HeadlessScanRatio.to_string(),
                value,
                limit: RATIO_BUDGET,
            });
            Verdict::Pass
        }
        _ => resolve_budget_check(expected, over_budget),
    }
}

/// How much a verdict counts against a run, highest first: an expected outcome (`Pass`, `Xfail`)
/// is least notable, a blocking one (`Xpass`, `Fail`, `Error`) more so.
const fn verdict_severity(verdict: Verdict) -> u8 {
    match verdict {
        Verdict::Pass => 0,
        Verdict::Xfail => 1,
        Verdict::Xpass => 2,
        Verdict::Fail => 3,
        Verdict::Error => 4,
    }
}

/// Combines two independent verdicts of the same fixture (the oracle diff and the ratio budget)
/// into the one the run reports: the more severe of the two.
fn combine_verdicts(first: Verdict, second: Verdict) -> Verdict {
    if verdict_severity(second) > verdict_severity(first) {
        second
    } else {
        first
    }
}

/// Gets the fixture, runs the rounds, and holds every kept report to the oracle. Leaves the
/// verdict in `report`, and the evidence of a failure on disk, before the scratch areas go.
fn measure(
    context: &Context<'_>,
    planned: &Planned,
    report: &mut FixtureReport,
) -> Result<(), StepError> {
    let options = context.options;
    let workspace = tempfile::Builder::new()
        .prefix("xh-")
        .tempdir_in(context.work_dir)
        .map_err(StepError::Workspace)?;

    // A master is read-only and shared, and it stays cached between runs, so a large fixture is
    // generated once. A fixture whose volumes are to be attached needs a copy of its own to attach
    // them to, and a fixture that `cargo clean` could not remove is never cached: each of those is
    // scanned in a copy, generated fresh and removed, permissions restored first, when it drops.
    let attach = options.privileged.filter(|_| planned.needs_volumes);
    let (root, _copy) = if planned.cacheable && attach.is_none() {
        let master = options.fixtures.master(&planned.id)?;
        report.generation = master.generation_time();
        (master.root, None)
    } else {
        let generating = Instant::now();
        let mut copy = options.fixtures.run_copy(&planned.id, workspace.path())?;
        report.generation = Some(generating.elapsed());
        if let Some(opt_in) = attach {
            report.volumes = Volumes::Attached(copy.attach_volumes(opt_in)?);
        }
        (copy.root().to_path_buf(), Some(copy))
    };
    if planned.needs_volumes && attach.is_none() {
        report.volumes = Volumes::Detached;
    }
    let fixture = FixtureRoot::open(&root)?;

    let walked = Instant::now();
    let oracle = Oracle::collect(fixture.path())?;
    report.oracle_time = walked.elapsed();
    report.entries = u64::try_from(oracle.entries.len()).unwrap_or(u64::MAX);
    report.du_flavor = context.du.map(Du::flavor);
    report.du_expected_kib = context.du.and_then(|du| du.expected_kib(&oracle));

    let mut kept_runs = run_rounds(context, &fixture, workspace.path(), report)?;

    // The fixture must be the tree the oracle saw: a scan only reads.
    let changes = describe_changes(&oracle, &Oracle::collect(fixture.path())?);
    let root_bytes = path_bytes(fixture.path());
    for (index, run) in &kept_runs {
        let mut diff = check(&oracle, &root_bytes, run, options.timeout);
        if !changes.is_empty() {
            diff.record(Discrepancy::FixtureChanged {
                changes: changes.clone(),
            });
        }
        report.diffs.push((*index, diff));
    }
    report.verdict = resolve(report.expectation.as_ref(), &report.kinds());
    report.ratio_expectation = options
        .expectations
        .expected_ratio_failure(&planned.id)
        .cloned();
    if report.ratio_is_gated() {
        report.ratio_gated = true;
        let ratio_verdict = judge_ratio(report, options.timing_informational);
        report.verdict = combine_verdicts(report.verdict, ratio_verdict);
    }
    let peak = report
        .rounds
        .iter()
        .filter_map(|round| round.scan.peak_memory_bytes)
        .max();
    if let Some(peak) = peak {
        let budget = MEMORY_BUDGET_BYTES;
        report.memory_peak_bytes = Some(peak);
        report.memory_budget_bytes = Some(budget);
        let memory_verdict =
            resolve_budget_check(report.memory_expectation.is_some(), peak > budget);
        report.memory_verdict = memory_verdict;
        report.verdict = combine_verdicts(report.verdict, memory_verdict);
    }
    if report.first_failure().is_some() {
        report.failure_dir = render::write_failure_dir(context.run_dir, report, &kept_runs);
    }
    if options.keep {
        for (_, run) in &mut kept_runs {
            run.keep();
        }
        report.kept = Some(workspace.keep());
    }
    Ok(())
}

/// Runs the warm-up round and the measured rounds: a scan, then `du -sk`, alternately. Returns the
/// scans whose reports are to be held to the oracle, with the index of their round.
fn run_rounds(
    context: &Context<'_>,
    fixture: &FixtureRoot,
    work_dir: &Path,
    report: &mut FixtureReport,
) -> Result<Vec<(usize, ScanRun)>, StepError> {
    let options = context.options;
    let mut kept_runs = Vec::new();
    for round in 0..=options.repeat {
        let warm_up = round == 0;
        let scan = run_scan(&ScanRequest {
            binary: &options.binary,
            fixture,
            work_dir,
            profile: options.profile,
            timeout: options.timeout,
        })?;
        let report_bytes = fs::metadata(scan.report_path())
            .ok()
            .map(|metadata| metadata.len());
        let finished = &scan.finished;
        let measure = ScanMeasure {
            wall: finished.wall,
            cpu: finished.cpu,
            peak_memory_bytes: finished.peak_memory_bytes,
            cgroup_memory_peak_bytes: finished.cgroup_memory_peak_bytes,
            ended: finished.ended,
            timed_out: finished.timed_out,
            report_bytes,
        };
        let du = match context.du {
            Some(du) => {
                let run = du.run(fixture.path(), work_dir, options.timeout)?;
                Some(DuMeasure {
                    wall: run.finished.wall,
                    kib: run.kib,
                    ended: run.finished.ended,
                })
            }
            None => None,
        };
        // A large report is kept only for the first round, to bound the disk a fixture uses.
        let keep = warm_up || report_bytes.is_none_or(|bytes| bytes <= KEEP_EVERY_REPORT_UP_TO);
        let index = report.rounds.len();
        report.rounds.push(Round {
            warm_up,
            scan: measure,
            du,
            diffed: keep,
        });
        if keep {
            kept_runs.push((index, scan));
        }
    }
    Ok(kept_runs)
}

/// Holds one scan to the oracle: the run's own conduct first, then its report.
fn check(oracle: &Oracle, root: &[u8], run: &ScanRun, timeout: Duration) -> Diff {
    let mut found = Diff::default();
    let finished = &run.finished;
    if finished.timed_out {
        found.record(Discrepancy::Timeout {
            limit_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        });
    }
    if finished.stdout.bytes > 0 {
        found.record(Discrepancy::UnexpectedOutput {
            stdout_bytes: finished.stdout.bytes,
        });
    }
    if !run.residue.is_empty() {
        found.record(Discrepancy::Residue {
            files: run.residue.clone(),
        });
    }
    if finished.timed_out {
        return found;
    }
    match ScanDocument::read(&run.report_path()) {
        Ok(document) => found.absorb(diff(
            oracle,
            &Scan {
                root,
                document: &document,
                exit_code: finished.ended.code(),
            },
        )),
        Err(DocumentError::Io { .. }) => found.record(Discrepancy::NoReport {
            ended: finished.ended.describe(),
            detail: finished.stderr.first_line(),
        }),
        Err(error) => found.record(Discrepancy::InvalidReport {
            reason: error.to_string(),
        }),
    }
    found
}

/// What differs between two walks of the same fixture, the first few differences as text.
fn describe_changes(before: &Oracle, after: &Oracle) -> Vec<String> {
    const SHOWN: usize = 5;
    if before == after {
        return Vec::new();
    }
    let mut changes = Vec::new();
    if before.entries.len() != after.entries.len() {
        changes.push(format!(
            "{} entries became {}",
            before.entries.len(),
            after.entries.len()
        ));
    }
    for (was, now) in before.entries.iter().zip(&after.entries) {
        if was != now {
            changes.push(format!("`{}` is not what it was", was.path));
            if changes.len() >= SHOWN {
                break;
            }
        }
    }
    if changes.is_empty() {
        changes.push("the oracle's own facts differ".to_owned());
    }
    changes
}

#[cfg(test)]
mod tests;
