//! The `harness-sweep` document: what `cargo xtask sweep` found when it ran the checks a published
//! release can take against every version, and the version-by-defect table built from them.
//!
//! The document has four parts that refer to one another by version name (`ref`):
//!
//! * `versions` are the columns of the table: one build per git ref, with the toolchain that built
//!   it, the digest of the binary, and what the build is known to do (see [`SweepTraits`]).
//! * `measurements` are the paired, interleaved timings: for each fixture, metric, and profile,
//!   rounds that ran every version one after another in one session on the same warm fixture, the
//!   samples of each version in the round each was taken in, and the median of each version's
//!   per-round ratios to the candidate (and to the other references the metric names) with a
//!   bootstrap confidence interval. A run that did not finish within the bound is a censored
//!   sample: it is flagged, and a ratio keeps what such runs say (a bound on the ratio of their
//!   round) apart from what the rounds in which both sides finished say (the median and the
//!   interval).
//! * `checks` are the observations of every other check, per version, as numbers and notes, with
//!   the path of the evidence file the run kept.
//! * `rows` are the table: one row per defect fixed in the release, one cell per version, each cell
//!   `affected`, `not-affected`, or `not-measurable` with the reason, the measured value, and where
//!   the evidence is.
//!
//! A reader that holds `rows` to `versions` (one cell per version, in the same order), every
//! `not-measurable` cell to a reason, and every series and ratio to the rounds of its measurement
//! is [`HarnessSweep::check`]; the JSON Schema cannot say any of them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{AbFixture, ConfidenceInterval, Document, SchemaVersion};
use crate::{scenario::Profile, string_enum::string_enum};

string_enum! {
    /// The value of a sweep document's `document_kind` field.
    #[derive(Default)]
    pub enum SweepKind {
        /// The only kind a sweep document can have.
        #[default]
        HarnessSweep => "harness-sweep",
    }
}

string_enum! {
    /// How much of the sweep a run covered.
    pub enum SweepTier {
        /// Small fixtures only: the checks that fit in minutes.
        Quick => "quick",
        /// Everything the sweep can measure, including the checks that need large fixtures.
        Full => "full",
    }
}

string_enum! {
    /// Whether a version's binary could be built.
    pub enum BuildStatus {
        /// The binary was built, or found in the cache of a build of the same commit.
        Built => "built",
        /// The build failed. Every cell of the version says so, and the sweep went on.
        Failed => "failed",
    }
}

string_enum! {
    /// What came of one check on one version.
    pub enum CheckStatus {
        /// The check ran and its observation is valid.
        Ran => "ran",
        /// The check could not run on this version, tier, or platform, and `reason` says why.
        NotRun => "not-run",
        /// The harness could not carry the check out (a spawn, a fixture, or an isolation failure):
        /// the observation says nothing about the version.
        Errored => "errored",
    }
}

string_enum! {
    /// What the sweep found of one defect on one version.
    pub enum CellState {
        /// The version shows the defect.
        Affected => "affected",
        /// The check ran and the version does not show the defect.
        NotAffected => "not-affected",
        /// No check the sweep can run decides it for this version, and `reason` says why.
        NotMeasurable => "not-measurable",
    }
}

/// The toolchain a version was built with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepToolchain {
    /// The channel the version's own `rust-toolchain.toml` names, for example `1.88.0`.
    pub channel: String,
    /// The first line of `rustc --version` as that toolchain printed it.
    pub rustc: String,
}

/// How a version's build went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepBuild {
    /// Whether there is a binary.
    pub status: BuildStatus,
    /// Whether the binary came from the cache of an earlier build of the same commit and
    /// toolchain.
    pub cached: bool,
    /// Why the build failed, with the end of the build's output: present exactly when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The build's complete output, as a path below the run's directory, when it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
}

/// What the sweep learned of a build by asking it, which is what the checks adapt to: the sweep
/// never assumes that an old build takes today's flags or writes today's report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepTraits {
    /// The `schema_version` of the `scan-report` documents the build writes, read from one it wrote
    /// to a headless scan. Absent when it wrote none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_version: Option<u32>,
    /// Whether the build's `--help` lists `--scan-store-dir`, so that it keeps its scan data in
    /// the directory the harness names. A build that does not keeps it in the temporary directory,
    /// which is where the checks then look for leftovers.
    pub scan_store_dir: bool,
}

/// One column of the table: a git ref and its build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepVersion {
    /// The ref as it was given, for example `v1.3.0` or `HEAD`.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The 40-character commit the ref resolved to.
    pub sha: String,
    /// The toolchain the build used. Absent when the build did not get as far as running one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<SweepToolchain>,
    /// The lowercase hexadecimal SHA-256 of the binary. Absent when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_sha256: Option<String>,
    /// How the build went.
    pub build: SweepBuild,
    /// What the build is known to do. Absent when it was not asked, because there is no binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traits: Option<SweepTraits>,
}

/// A ratio of two paired timings: the median of the per-round ratios of the rounds in which both
/// sides finished, a bootstrap confidence interval for it, and what the other rounds say.
///
/// A run that did not finish within the bound is recorded at how long it ran (the bound, when it
/// ran out of time), which is only the least it took. A round in which both sides finished gives a
/// ratio. A round in which only the numerator did not finish gives a lower bound on its ratio, one
/// in which only the denominator did not finish gives an upper bound, and one in which neither
/// finished says nothing. The median and the interval rest on the rounds in which both sides
/// finished and on nothing else. A round in which a side has no sample at all (it was skipped, or
/// its runs could not be carried out) is none of these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepRatio {
    /// The median of the per-round numerator/denominator ratios of the rounds in which both sides
    /// finished. Absent when there are none (`pairs` is 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub median: Option<f64>,
    /// A deterministic bootstrap interval for `median`. Present exactly when `median` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_ci: Option<ConfidenceInterval>,
    /// How many rounds had a ratio: both sides finished, and the denominator was not zero.
    pub pairs: u32,
    /// For each round in which only the numerator did not finish, in round order: the least its
    /// ratio can have been, the numerator's bound over the denominator's time.
    pub lower_bounds: Vec<f64>,
    /// For each round in which only the denominator did not finish, in round order: the most its
    /// ratio can have been, the numerator's time over the denominator's bound.
    pub upper_bounds: Vec<f64>,
    /// How many rounds neither side finished in: they say nothing about the ratio.
    pub both_censored: u32,
}

/// One version's timings of one metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepSeries {
    /// The version.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The round (counting from 0, after the warm-up round that is not recorded) each sample was
    /// measured in, increasing. A round the version did not run in has no sample.
    pub rounds: Vec<u32>,
    /// The measurement of each of those rounds in milliseconds. A run that did not reach the end
    /// within the bound is recorded at how long it ran (the bound, when it ran out of time), which
    /// is only the least it took, and `completed` says so.
    pub samples: Vec<f64>,
    /// Whether each run reached the end within the bound, index-aligned with `samples`.
    pub completed: Vec<bool>,
    /// How many rounds the version was not run in, because its runs had run out of time several
    /// times in a row. A skipped round is neither a sample nor a pair.
    pub skipped: u32,
    /// The median of the samples whose run completed, absent when none did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub median: Option<f64>,
    /// The ratios the metric defines, by the name of what they are taken against: `candidate`
    /// (this version over the candidate, round by round), `du` (a scan over `du -sk`),
    /// `headless-scan` (an interface run over the headless scan of the same version in the same
    /// round, without the writing of its report), and `reduced-motion` (default motion over
    /// reduced motion, for the same version in the same round).
    pub ratios: BTreeMap<String, SweepRatio>,
}

/// Paired, interleaved timings of one metric on one fixture: rounds that ran every version in turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepMeasurement {
    /// The metric: `headless_wall_ms` (a scan with `--format json --output`), `headless_scan_ms`
    /// (the same scan's wall time less the time its report took to write, from the same run),
    /// `report_write_ms` (how long the report file took to grow, from its first byte to the last
    /// change in its length), or `tui_complete_ms` (spawn to the header reading COMPLETE). The
    /// vocabulary is open: a sweep that times one more thing adds a name and keeps version 1.
    pub metric: String,
    /// The fixture's id.
    pub fixture: String,
    /// The profile an interface metric ran under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    /// The cap on how fast the terminal's output was read, in bytes per second, for an interface
    /// metric that ran against a slow terminal. Absent when it read as fast as it came.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_bytes_per_sec: Option<u64>,
    /// How many measured rounds were planned, after the warm-up round that is not recorded. A
    /// version may have fewer samples: see `skipped` of its series.
    pub rounds: u32,
    /// The refs that ran in each round, in the order they ran, so that the interleaving can be
    /// audited. A version that was skipped in a round, or whose runs could not be carried out, is
    /// not in it.
    pub order: Vec<Vec<String>>,
    /// The milliseconds `du -sk` took in each round, `null` for a round in which it could not be
    /// timed, for the metric that is timed against it. Absent when there are none, and otherwise
    /// one for every round.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub du_samples: Vec<Option<f64>>,
    /// One series per version that ran, in the order of `versions`.
    pub series: Vec<SweepSeries>,
}

/// The observation of one check on one version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepCheck {
    /// The version.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The check: `headless-oracle`, `signal-term`, `signal-hup`, `signal-quit`, `signal-close`,
    /// `kill-restart`, `idle`, `filter`, `descriptors`, or `selection-drift`. The vocabulary is
    /// open.
    pub check: String,
    /// The fixture it ran on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixture: Option<String>,
    /// The profile it ran under, for an interface check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    /// What came of it.
    pub status: CheckStatus,
    /// Why it did not run, or why the harness could not carry it out. Present exactly when
    /// `status` is not `ran`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What it measured, by name: exit codes, byte counts, milliseconds. The names are an open
    /// vocabulary.
    pub metrics: BTreeMap<String, f64>,
    /// What it observed that is not a number, one fact per line: how the program ended, what was
    /// left behind.
    pub notes: Vec<String>,
    /// The evidence file the run kept, as a path below the run's directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// One cell of the table: one defect on one version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepCell {
    /// The version.
    #[serde(rename = "ref")]
    pub reference: String,
    /// What the sweep found.
    pub state: CellState,
    /// Why: required when the cell is `not-measurable`, and the rule that decided it otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The measured value, as text: `median 14.2x the candidate (95% CI 13.1-15.0)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Where the evidence is: paths below the run's directory, and JSON pointers into this document
    /// (`#/measurements/0`, `#/checks/12`).
    pub evidence: Vec<String>,
}

/// One row of the table: a defect fixed in the release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepRow {
    /// The finding: `F1`, `F10`, `F23b`.
    pub defect: String,
    /// What the defect is, in a line.
    pub title: String,
    /// The opening words of the `CHANGELOG.md` `[Unreleased]` entry that records the fix.
    pub changelog: String,
    /// How the row is measured.
    pub measured_by: String,
    /// One cell per version, in the order of `versions`.
    pub cells: Vec<SweepCell>,
    /// A caveat about the whole row, for example that the candidate shows the defect too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The conditions the sweep ran under, recorded because timings do not transfer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepContext {
    /// The host name.
    pub host: String,
    /// The CPU model.
    pub cpu: String,
    /// The operating system and version, for example `macOS 14.8.9`.
    pub os: String,
    /// The CPU architecture, for example `aarch64`.
    pub arch: String,
    /// The number of logical CPUs.
    pub logical_cpus: u32,
    /// The power state: `ac`, `battery`, or `unknown`.
    pub power: String,
    /// The 1-minute load average when the sweep started.
    pub load_average_start: f64,
    /// The 1-minute load average when the sweep finished.
    pub load_average_end: f64,
    /// How many other `excise` processes were running when the sweep started.
    pub concurrent_excise_processes: u32,
    /// The commit of the checkout that ran the sweep.
    pub checkout_sha: String,
    /// How many measured rounds each paired timing ran.
    pub rounds: u32,
    /// The seed of the bootstrap.
    pub seed: u64,
    /// The cap, in bytes per second, of the slow terminal the interface timings ran against.
    pub drain_bytes_per_sec: u64,
    /// How long one run of a timed check could take before it counted as not completed.
    pub timeout_ms: u64,
    /// The `du` the scans were timed against, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub du: Option<String>,
    /// The fixtures the sweep ran on, one entry each.
    pub fixtures: Vec<AbFixture>,
}

/// What `cargo xtask sweep` found of every version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessSweep {
    /// Always `harness-sweep`.
    pub document_kind: SweepKind,
    /// Always the current schema version.
    pub schema_version: SchemaVersion,
    /// The run's id, which names its directory below `target/excise-sweep`.
    pub run_id: String,
    /// How much of the sweep ran.
    pub tier: SweepTier,
    /// When the sweep started, as an RFC 3339 timestamp.
    pub started_at: String,
    /// When it finished.
    pub finished_at: String,
    /// The version every ratio is taken against: the last one.
    pub candidate: String,
    /// The conditions it ran under.
    pub context: SweepContext,
    /// The columns of the table, oldest first and the candidate last.
    pub versions: Vec<SweepVersion>,
    /// The paired, interleaved timings.
    pub measurements: Vec<SweepMeasurement>,
    /// The observations of every other check.
    pub checks: Vec<SweepCheck>,
    /// The table.
    pub rows: Vec<SweepRow>,
}

impl Document for HarnessSweep {
    const KIND: &'static str = SweepKind::HarnessSweep.as_str();
    const SCHEMA_ID: &'static str =
        "https://github.com/findyourexit/excise/harness/schemas/harness-sweep-v1.json";
    const SCHEMA_JSON: &'static str = include_str!("../../schemas/harness-sweep.schema.json");
}

/// A sweep document that breaks a rule its JSON Schema cannot express.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SweepInvalid {
    /// The candidate is not one of the versions, or is not the last.
    #[error("the candidate `{0}` is not the last of the versions")]
    CandidateNotLast(String),
    /// Two versions have one name.
    #[error("the version `{0}` appears more than once")]
    DuplicateVersion(String),
    /// A row has a cell for a version that is not the one in its place, or too few or too many.
    #[error("the row for `{defect}` does not have exactly one cell per version, in their order")]
    CellsDoNotMatchVersions {
        /// The defect of the row.
        defect: String,
    },
    /// A cell that says nothing is measurable does not say why.
    #[error("the cell of `{defect}` on `{reference}` is not measurable and gives no reason")]
    NoReason {
        /// The defect of the row.
        defect: String,
        /// The version of the cell.
        reference: String,
    },
    /// A series, a check, or a cell names a version that has no column.
    #[error("{what} names `{reference}`, which is not one of the versions")]
    UnknownVersion {
        /// Which part of the document.
        what: String,
        /// The name.
        reference: String,
    },
    /// A series whose samples, completion flags, and rounds do not line up.
    #[error(
        "the `{metric}` series of `{reference}` has {samples} samples, {flags} flags, and {rounds} \
         rounds"
    )]
    SeriesLengths {
        /// The metric.
        metric: String,
        /// The version.
        reference: String,
        /// How many samples.
        samples: usize,
        /// How many flags.
        flags: usize,
        /// How many round numbers.
        rounds: usize,
    },
    /// A series whose rounds do not increase, or reach past the rounds of its measurement.
    #[error(
        "the rounds of the `{metric}` series of `{reference}` do not increase below the {rounds} \
         rounds of the measurement"
    )]
    SeriesRounds {
        /// The metric.
        metric: String,
        /// The version.
        reference: String,
        /// How many rounds the measurement ran.
        rounds: u32,
    },
    /// A series with more samples and skipped rounds than its measurement has rounds.
    #[error(
        "the `{metric}` series of `{reference}` has {samples} samples and {skipped} skipped rounds \
         in a measurement of {rounds} rounds"
    )]
    SeriesOverrun {
        /// The metric.
        metric: String,
        /// The version.
        reference: String,
        /// How many samples.
        samples: usize,
        /// How many rounds were skipped.
        skipped: u32,
        /// How many rounds the measurement ran.
        rounds: u32,
    },
    /// A ratio that has a median without a round that had a ratio, or has none with one, or counts
    /// more rounds than its measurement ran.
    #[error("the `{name}` ratio of the `{metric}` series of `{reference}` does not add up")]
    RatioCounts {
        /// The metric.
        metric: String,
        /// The version.
        reference: String,
        /// What the ratio is taken against.
        name: String,
    },
    /// A measurement whose `du` samples are not one for each round.
    #[error("the `{metric}` measurement has {samples} `du` samples in {rounds} rounds")]
    DuSamples {
        /// The metric.
        metric: String,
        /// How many `du` samples.
        samples: usize,
        /// How many rounds the measurement ran.
        rounds: u32,
    },
    /// A check that did not run does not say why, or one that ran gives a reason.
    #[error("the `{check}` check of `{reference}` has status `{status}` and the wrong reason")]
    CheckReason {
        /// The check.
        check: String,
        /// The version.
        reference: String,
        /// Its status.
        status: CheckStatus,
    },
    /// A build that failed gives no reason, or one that did not fail gives one.
    #[error("the build of `{0}` has the wrong reason for its status")]
    BuildReason(String),
}

/// How many items, as a number that cannot overflow the sum of a few of them.
fn count(items: usize) -> u64 {
    u64::try_from(items).unwrap_or(u64::MAX)
}

/// Whether the parts of a ratio hold together: a median and an interval exactly when some round
/// had a ratio, and no more rounds counted than the measurement ran.
fn ratio_adds_up(ratio: &SweepRatio, rounds: u32) -> bool {
    let finished = ratio.pairs > 0;
    let counted = u64::from(ratio.pairs)
        + count(ratio.lower_bounds.len())
        + count(ratio.upper_bounds.len())
        + u64::from(ratio.both_censored);
    ratio.median.is_some() == finished
        && ratio.bootstrap_ci.is_some() == finished
        && counted <= u64::from(rounds)
}

/// Checks the rules of one measurement that the JSON Schema cannot say.
fn check_measurement(measurement: &SweepMeasurement, names: &[&str]) -> Result<(), SweepInvalid> {
    let rounds = measurement.rounds;
    if !measurement.du_samples.is_empty()
        && count(measurement.du_samples.len()) != u64::from(rounds)
    {
        return Err(SweepInvalid::DuSamples {
            metric: measurement.metric.clone(),
            samples: measurement.du_samples.len(),
            rounds,
        });
    }
    for series in &measurement.series {
        if !names.contains(&series.reference.as_str()) {
            return Err(SweepInvalid::UnknownVersion {
                what: format!("a series of `{}`", measurement.metric),
                reference: series.reference.clone(),
            });
        }
        let (metric, reference) = (measurement.metric.clone(), series.reference.clone());
        if series.samples.len() != series.completed.len()
            || series.samples.len() != series.rounds.len()
        {
            return Err(SweepInvalid::SeriesLengths {
                metric,
                reference,
                samples: series.samples.len(),
                flags: series.completed.len(),
                rounds: series.rounds.len(),
            });
        }
        if !series.rounds.windows(2).all(|pair| pair[0] < pair[1])
            || series.rounds.last().is_some_and(|last| *last >= rounds)
        {
            return Err(SweepInvalid::SeriesRounds {
                metric,
                reference,
                rounds,
            });
        }
        if count(series.samples.len()) + u64::from(series.skipped) > u64::from(rounds) {
            return Err(SweepInvalid::SeriesOverrun {
                metric,
                reference,
                samples: series.samples.len(),
                skipped: series.skipped,
                rounds,
            });
        }
        if let Some((name, _)) = series
            .ratios
            .iter()
            .find(|(_, ratio)| !ratio_adds_up(ratio, rounds))
        {
            return Err(SweepInvalid::RatioCounts {
                metric,
                reference,
                name: name.clone(),
            });
        }
    }
    Ok(())
}

impl HarnessSweep {
    /// Checks the rules the JSON Schema cannot say: the candidate is the last version, versions
    /// have one name each, every row has one cell per version in their order, a cell that is not
    /// measurable says why, every series and check names a version, a series' samples, completion
    /// flags, and rounds line up and fit the rounds of its measurement (they increase and stay
    /// below its round count, and the samples and skipped rounds are no more than its rounds), a
    /// ratio has a median and an interval exactly when some round had a ratio and counts no more
    /// rounds than the measurement ran, the `du` samples of a measurement are one for each round,
    /// a check that did not run says why, and a build that failed says why.
    ///
    /// # Errors
    ///
    /// Returns the first rule the document breaks.
    pub fn check(&self) -> Result<(), SweepInvalid> {
        let names: Vec<&str> = self
            .versions
            .iter()
            .map(|version| version.reference.as_str())
            .collect();
        if names.last().copied() != Some(self.candidate.as_str()) {
            return Err(SweepInvalid::CandidateNotLast(self.candidate.clone()));
        }
        for (index, name) in names.iter().enumerate() {
            if names[..index].contains(name) {
                return Err(SweepInvalid::DuplicateVersion((*name).to_owned()));
            }
        }
        for version in &self.versions {
            if (version.build.status == BuildStatus::Failed) != version.build.reason.is_some() {
                return Err(SweepInvalid::BuildReason(version.reference.clone()));
            }
        }
        for row in &self.rows {
            let cells: Vec<&str> = row
                .cells
                .iter()
                .map(|cell| cell.reference.as_str())
                .collect();
            if cells != names {
                return Err(SweepInvalid::CellsDoNotMatchVersions {
                    defect: row.defect.clone(),
                });
            }
            if let Some(cell) = row
                .cells
                .iter()
                .find(|cell| cell.state == CellState::NotMeasurable && cell.reason.is_none())
            {
                return Err(SweepInvalid::NoReason {
                    defect: row.defect.clone(),
                    reference: cell.reference.clone(),
                });
            }
        }
        for measurement in &self.measurements {
            check_measurement(measurement, &names)?;
        }
        for check in &self.checks {
            if !names.contains(&check.reference.as_str()) {
                return Err(SweepInvalid::UnknownVersion {
                    what: format!("the `{}` check", check.check),
                    reference: check.reference.clone(),
                });
            }
            if (check.status == CheckStatus::Ran) != check.reason.is_none() {
                return Err(SweepInvalid::CheckReason {
                    check: check.check.clone(),
                    reference: check.reference.clone(),
                    status: check.status,
                });
            }
        }
        Ok(())
    }

    /// The row for `defect`, if the table has one.
    #[must_use]
    pub fn row(&self, defect: &str) -> Option<&SweepRow> {
        self.rows.iter().find(|row| row.defect == defect)
    }

    /// The cell of `defect` on the version `reference`, if the table has one.
    #[must_use]
    pub fn cell(&self, defect: &str, reference: &str) -> Option<&SweepCell> {
        self.row(defect)?
            .cells
            .iter()
            .find(|cell| cell.reference == reference)
    }
}
