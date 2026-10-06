//! The table: what the checks observed, decided into `affected`, `not-affected`, or
//! `not-measurable` for every defect on every version.
//!
//! Each rule is stated in the cell it produces, so a maintainer reading the table sees what was
//! measured, what it was held to, and where the evidence is. Three principles hold throughout:
//!
//! * **A claim needs a measurement.** `affected` and `not-affected` come from a check that ran. A
//!   check that could not run, a timing whose interval does not settle the question, and a defect
//!   that only a deletion shows are `not-measurable`, with the reason.
//! * **Timings are paired, by round.** A version is slower than the candidate when the median of
//!   the per-round ratios of the rounds in which both finished is more than 20% above one and its
//!   bootstrap interval excludes one (the policy of `cargo xtask bench-e2e`). A run that did not
//!   finish within the bound is censored: it is only known to have taken at least the bound, so it
//!   decides what that can decide and no more. Two rounds in which a version's run did not finish
//!   and the candidate's did make it affected, two in which the candidate's did not and the
//!   version's did make it not slower, and a round in which neither finished says nothing.
//! * **The candidate is the reference.** Its cell in a timing row is `not-affected` by definition.
//!   In every other row it is held to the same rule as the rest, and a row where it shows the
//!   defect says so in its note: the run then does not show the defect fixed.

use std::fmt::Write as _;

use crate::{
    bench::verdict::{MetricKind, classify},
    headless::DiscrepancyKind,
    pty::ui::Inspector,
    report::{
        AbVerdict, CellState, SweepCell, SweepMeasurement, SweepRatio, SweepRow, SweepSeries,
    },
    run_support::format_ms,
    runner::default_limit,
    scenario::{Budget, Profile, Signal},
};

use super::{
    checks::{
        DescriptorObservation, DriftObservation, FilterObservation, IdleObservation,
        KillRestartObservation,
    },
    defects::{DEFECTS, DELETION_REASON, Defect},
    headless::{OracleObservation, ReportRead},
    model::{MeasuredSet, Placed, Plan, VersionFacts, roles},
    signals::{signals_for, skipped_note},
};

/// How many more descriptors a program may hold on the large tree than on the small one before it
/// counts as growing with the tree: the fixed build holds the same number on both, within the
/// noise of a sampler that looks every 50 ms.
pub(crate) const DESCRIPTOR_GROWTH: u32 = 24;

/// Everything the rules read.
pub(crate) struct Classifier<'a> {
    /// The columns, oldest first and the candidate last.
    pub versions: &'a [VersionFacts],
    /// The paired timings.
    pub measurements: &'a [MeasuredSet],
    /// What ran.
    pub plan: &'a Plan,
    /// The platform, as `std::env::consts::OS` spells it.
    pub os: &'a str,
    /// The cap of the slow terminal, in bytes per second.
    pub drain: u64,
}

/// What a timing lookup found.
enum Timing<'a> {
    Series {
        set: &'a MeasuredSet,
        series: &'a SweepSeries,
    },
    Missing(String),
}

/// How a version's time compares with the candidate's, as the rounds in which both finished say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slowness {
    /// Slower by more than the threshold, with an interval above one.
    Much,
    /// No slower.
    Same,
    /// Somewhere between: not slower than the threshold, or not confidently, or too few rounds.
    Unsure,
}

/// The fewest rounds in which both sides finished that a ratio may rest on to settle a cell: with
/// one the bootstrap interval is a point and with two it is barely wider, and a single run is never
/// compared.
const MIN_ROUNDS: u32 = 3;

/// The fewest rounds in which a run did not finish that settle a cell without a ratio: one such
/// round is a slow moment, and two are not.
const MIN_CENSORED: usize = 2;

/// How much slower than the candidate a version may be before it counts as slower: the regression
/// budget of `cargo xtask bench-e2e`.
fn regression() -> f64 {
    default_limit(Budget::TimingAbRegression).unwrap_or(0.20)
}

/// What the rounds in which both finished say of a ratio to the candidate: more than 20% slower
/// with an interval above one, or no slower. Fewer than [`MIN_ROUNDS`] of them say nothing.
fn slowness(ratio: &SweepRatio) -> Slowness {
    let (Some(median), Some(interval)) = (ratio.median, ratio.bootstrap_ci) else {
        return Slowness::Unsure;
    };
    if ratio.pairs < MIN_ROUNDS {
        return Slowness::Unsure;
    }
    match classify(
        MetricKind::Timing,
        median,
        interval,
        regression(),
        default_limit(Budget::MemoryAbTolerance).unwrap_or(0.05),
    ) {
        AbVerdict::Block => Slowness::Much,
        AbVerdict::Pass => Slowness::Same,
        AbVerdict::Warn => Slowness::Unsure,
    }
}

/// What the rounds in which both finished say of a ratio held to a budget on the ratio itself: over
/// it with an interval above it, or at most it. Fewer than [`MIN_ROUNDS`] of them say nothing.
fn within_budget(ratio: &SweepRatio, limit: f64) -> Slowness {
    let (Some(median), Some(interval)) = (ratio.median, ratio.bootstrap_ci) else {
        return Slowness::Unsure;
    };
    if ratio.pairs < MIN_ROUNDS {
        Slowness::Unsure
    } else if median <= limit {
        Slowness::Same
    } else if interval.lower > limit {
        Slowness::Much
    } else {
        Slowness::Unsure
    }
}

/// What the rounds of a ratio add up to, those in which both sides finished and those in which a
/// run did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    /// Slower, as the median of the rounds in which both finished says or, with `by_bounds`, as the
    /// rounds in which the numerator did not finish prove, whatever the median.
    Slower { by_bounds: bool },
    /// Not slower, as the median says or, with `by_bounds`, as the rounds in which the denominator
    /// did not finish prove.
    NotSlower { by_bounds: bool },
    /// Neither, for this reason.
    Unsure(Doubt),
}

/// Why the rounds of a ratio settle nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Doubt {
    /// Too few rounds finished on both sides, and the rounds in which a run did not finish settle
    /// nothing by themselves either.
    Thin,
    /// Enough rounds finished, and their ratio is neither one thing nor the other.
    Undecided,
    /// The rounds that finished say slower, but rounds in which the denominator did not finish are
    /// left out of their median, and could lower it.
    Lowered,
    /// The rounds that finished, or those in which the denominator did not, say not slower, but
    /// rounds in which the numerator did not finish are left out of the median, and could only
    /// raise it.
    Raised,
    /// Some rounds say slower and others say not slower.
    Conflict,
}

/// Decides a ratio from every round it has. `finished` is what the rounds in which both sides
/// finished say; a lower bound above `slower_than` proves its round slower, and an upper bound at
/// or below `not_slower_than` proves its round not slower.
///
/// A round in which a run did not finish says one thing only: the least a numerator took can show
/// that a ratio is high and never that it is low, and the least a denominator took can show that it
/// is low and never that it is high. Two such rounds settle a cell without a ratio (one is a slow
/// moment). The median of the rounds that finished leaves the others out, so it stands only when
/// none of them argues against it, and rounds that argue both ways settle nothing.
fn judge(ratio: &SweepRatio, finished: Slowness, slower_than: f64, not_slower_than: f64) -> Call {
    let proven_slow = ratio
        .lower_bounds
        .iter()
        .filter(|bound| **bound > slower_than)
        .count();
    let proven_not_slow = ratio
        .upper_bounds
        .iter()
        .filter(|bound| **bound <= not_slower_than)
        .count();
    let says_slower = finished == Slowness::Much || proven_slow >= MIN_CENSORED;
    let says_not_slower = finished == Slowness::Same || proven_not_slow >= MIN_CENSORED;
    match (says_slower, says_not_slower) {
        (true, true) => Call::Unsure(Doubt::Conflict),
        (true, false) => {
            if proven_slow >= MIN_CENSORED {
                Call::Slower {
                    by_bounds: finished != Slowness::Much,
                }
            } else if ratio.upper_bounds.is_empty() {
                Call::Slower { by_bounds: false }
            } else {
                Call::Unsure(Doubt::Lowered)
            }
        }
        (false, true) => {
            if ratio.lower_bounds.is_empty() {
                Call::NotSlower {
                    by_bounds: finished != Slowness::Same,
                }
            } else {
                Call::Unsure(Doubt::Raised)
            }
        }
        (false, false) => Call::Unsure(if ratio.pairs >= MIN_ROUNDS {
            Doubt::Undecided
        } else {
            Doubt::Thin
        }),
    }
}

fn times(value: f64) -> String {
    if value >= 10.0 {
        format!("{value:.0}x")
    } else {
        format!("{value:.1}x")
    }
}

/// `1 round`, `3 rounds`.
fn rounds_of(count: usize) -> String {
    format!("{count} round{}", if count == 1 { "" } else { "s" })
}

/// What the rounds of `ratio` in which a run did not finish add to it: the numerator's run is the
/// version's own, and `against` names the denominator's. `None` when there are none.
fn censored_note(ratio: &SweepRatio, against: &str) -> Option<String> {
    let mut parts = Vec::new();
    if !ratio.lower_bounds.is_empty() {
        let least = ratio
            .lower_bounds
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min);
        parts.push(format!(
            "in {} its run did not finish while {against} did (at least {} there)",
            rounds_of(ratio.lower_bounds.len()),
            times(least)
        ));
    }
    if !ratio.upper_bounds.is_empty() {
        let most = ratio.upper_bounds.iter().copied().fold(0.0, f64::max);
        parts.push(format!(
            "in {} {against} did not finish while its run did (at most {} there)",
            rounds_of(ratio.upper_bounds.len()),
            times(most)
        ));
    }
    if ratio.both_censored > 0 {
        parts.push(format!(
            "in {} neither finished",
            rounds_of(usize::try_from(ratio.both_censored).unwrap_or(usize::MAX))
        ));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn describe_ratio(ratio: &SweepRatio, against: &str) -> String {
    let mut text = match (ratio.median, ratio.bootstrap_ci) {
        (Some(median), Some(interval)) => format!(
            "{} {against} (95% CI {:.1}-{:.1}, {} rounds)",
            times(median),
            interval.lower,
            interval.upper,
            ratio.pairs
        ),
        _ => format!("no round finished on both sides, so there is no ratio to {against}"),
    };
    if let Some(note) = censored_note(ratio, against) {
        let _ = write!(text, "; {note}");
    }
    text
}

/// How many runs a series measured, how many of them did not finish within the bound, and how many
/// rounds it skipped.
struct Census {
    ran: usize,
    unfinished: usize,
    skipped: usize,
}

impl Census {
    fn of(series: &SweepSeries) -> Self {
        Self {
            ran: series.samples.len(),
            unfinished: series.completed.iter().filter(|done| !**done).count(),
            skipped: usize::try_from(series.skipped).unwrap_or(usize::MAX),
        }
    }

    /// How many of the runs finished.
    fn finished(&self) -> usize {
        self.ran.saturating_sub(self.unfinished)
    }
}

/// Why a version is slower when the rounds in which its run did not finish show it: how many of its
/// measured runs did not finish within the bound, in how many rounds `who`'s run did, how many runs
/// of `who` finished, and how many rounds were skipped.
fn slower_by_bounds(
    own: &SweepSeries,
    reference: Option<&SweepSeries>,
    who: &str,
    ratio: &SweepRatio,
) -> String {
    let own = Census::of(own);
    let mut text = format!(
        "{} of its {} measured runs did not finish within the bound, {} of them in rounds where \
         {who}'s run did",
        own.unfinished,
        own.ran,
        ratio.lower_bounds.len()
    );
    if let Some(reference) = reference {
        let census = Census::of(reference);
        let _ = write!(
            text,
            " ({who} finished {} of its {} runs)",
            census.finished(),
            census.ran
        );
    }
    let _ = write!(text, "; {} skipped", rounds_of(own.skipped));
    text
}

/// Why a version is not slower when the rounds in which `who`'s run did not finish show it: in how
/// many rounds that was, and how many of the runs of `who` did not finish, and how many rounds it
/// skipped.
fn not_slower_by_bounds(reference: Option<&SweepSeries>, who: &str, ratio: &SweepRatio) -> String {
    let mut text = format!(
        "its run finished in {} where {who}'s run did not finish within the bound, in less time \
         than that bound",
        rounds_of(ratio.upper_bounds.len())
    );
    if let Some(reference) = reference {
        let census = Census::of(reference);
        let _ = write!(
            text,
            "; {who} did not finish {} of its {} runs ({} skipped)",
            census.unfinished,
            census.ran,
            rounds_of(census.skipped)
        );
    }
    text
}

/// Why the rounds of `ratio` settle nothing, in the words of a rule that holds a version's run to
/// `who`'s. `None` for [`Doubt::Undecided`], which each rule words for itself.
fn doubt_text(doubt: Doubt, ratio: &SweepRatio, who: &str) -> Option<String> {
    match doubt {
        Doubt::Undecided => None,
        Doubt::Thin => {
            let mut text = format!(
                "{} finished on both sides, which is too few to settle it: a ratio needs at least \
                 {MIN_ROUNDS} rounds, and runs that did not finish settle it only when at least \
                 {MIN_CENSORED} rounds show it",
                rounds_of(usize::try_from(ratio.pairs).unwrap_or(usize::MAX))
            );
            if ratio.both_censored > 0 {
                let _ = write!(
                    text,
                    "; in {} neither side finished, which says nothing",
                    rounds_of(usize::try_from(ratio.both_censored).unwrap_or(usize::MAX))
                );
            }
            Some(text)
        }
        Doubt::Lowered => Some(format!(
            "{who}'s run did not finish in {} in which its run did, and those rounds are left out \
             of the median, which they could lower",
            rounds_of(ratio.upper_bounds.len())
        )),
        Doubt::Raised => Some(format!(
            "its run did not finish in {} in which {who}'s run did, which says it is slower there \
             and keeps the other rounds from showing it is not",
            rounds_of(ratio.lower_bounds.len())
        )),
        Doubt::Conflict => Some(
            "the rounds disagree: some show it slower and others show it is not, so none settles \
             it"
            .to_owned(),
        ),
    }
}

fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    #[allow(clippy::cast_precision_loss)]
    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn cell(
    version: &VersionFacts,
    state: CellState,
    reason: Option<String>,
    value: Option<String>,
    evidence: Vec<String>,
) -> SweepCell {
    SweepCell {
        reference: version.reference.clone(),
        state,
        reason,
        value,
        evidence,
    }
}

fn affected(
    version: &VersionFacts,
    reason: impl Into<String>,
    value: impl Into<String>,
    evidence: Vec<String>,
) -> SweepCell {
    cell(
        version,
        CellState::Affected,
        Some(reason.into()),
        Some(value.into()),
        evidence,
    )
}

fn not_affected(
    version: &VersionFacts,
    reason: impl Into<String>,
    value: impl Into<String>,
    evidence: Vec<String>,
) -> SweepCell {
    cell(
        version,
        CellState::NotAffected,
        Some(reason.into()),
        Some(value.into()),
        evidence,
    )
}

fn not_measurable(
    version: &VersionFacts,
    reason: impl Into<String>,
    evidence: Vec<String>,
) -> SweepCell {
    cell(
        version,
        CellState::NotMeasurable,
        Some(reason.into()),
        None,
        evidence,
    )
}

fn signal_label(signal: Signal) -> &'static str {
    match signal {
        Signal::Term => "SIGTERM",
        Signal::Hup => "SIGHUP",
        Signal::Quit => "SIGQUIT",
        Signal::Int => "SIGINT",
        Signal::Close => "console close",
        Signal::Break => "console break",
    }
}

impl Classifier<'_> {
    /// The table: one row per defect, one cell per version.
    pub(crate) fn rows(&self) -> Vec<SweepRow> {
        DEFECTS
            .iter()
            .map(|defect| {
                let cells: Vec<SweepCell> = self
                    .versions
                    .iter()
                    .map(|version| self.cell(defect, version))
                    .collect();
                let candidate = cells.last();
                let note = candidate
                    .filter(|cell| cell.state == CellState::Affected && !is_timing_row(defect.id))
                    .map(|cell| {
                        format!(
                            "the candidate `{}` shows this defect too, so this run does not show \
                             it fixed",
                            cell.reference
                        )
                    });
                SweepRow {
                    defect: defect.id.to_owned(),
                    title: defect.title.to_owned(),
                    changelog: defect.changelog.to_owned(),
                    measured_by: defect.measured_by.to_owned(),
                    cells,
                    note,
                }
            })
            .collect()
    }

    fn is_candidate(&self, version: &VersionFacts) -> bool {
        self.versions
            .last()
            .is_some_and(|last| last.reference == version.reference)
    }

    fn cell(&self, defect: &Defect, version: &VersionFacts) -> SweepCell {
        match defect.id {
            "F1" => self.guarded(version, |this| this.f1(version)),
            "F2" => self.guarded(version, |this| this.f2(version)),
            "F3" => self.guarded(version, |_| Self::f3(version)),
            "F4" => self.guarded(version, |_| Self::f4(version)),
            "F5" => self.guarded(version, |this| this.f5(version)),
            "F6" => self.guarded(version, |this| this.f6(version)),
            "F7" => self.guarded(version, |_| Self::f7(version)),
            "F8" => self.windows_only(version, "the key-release events of the Windows console"),
            "F9" | "F14" | "F19" | "F21" => not_measurable(version, DELETION_REASON, Vec::new()),
            "F10" => self.guarded(version, |_| {
                Self::oracle_kind(
                    version,
                    roles::HOSTILE,
                    DiscrepancyKind::EntryState,
                    "an unreadable folder, and the folders above it, are reported complete with exact bounds",
                )
            }),
            "F11" => self.guarded(version, |_| {
                Self::oracle_kind(
                    version,
                    roles::DEEP,
                    DiscrepancyKind::Missing,
                    "entries below the path length limit are missing from the report",
                )
            }),
            "F12" => not_measurable(
                version,
                "needs a scratch volume that fills during the scan, which only a privileged \
                 attached volume gives (AGENTS.md allows `EXCISE_HARNESS_PRIVILEGED=1` for a task \
                 about volumes); `EXCISE_HARNESS_PRIVILEGED=1 cargo xtask e2e --scenario \
                 scan-store-quota` runs the closest check there is",
                Vec::new(),
            ),
            "F13" => self.guarded(version, |this| this.f13(version)),
            "F15" => self.windows_only(
                version,
                "another program must hold a scan-store file at the moment of the rename",
            ),
            "F16" => self.guarded(version, |this| this.f16(version)),
            "F18" => self.guarded(version, |this| this.f18(version)),
            "F22" => self.guarded(version, |_| Self::f22(version)),
            "F23b" => self.guarded(version, |this| this.f23b(version)),
            "F24" => self.f24(version),
            other => not_measurable(
                version,
                format!("the sweep has no rule for `{other}`"),
                Vec::new(),
            ),
        }
    }

    /// A rule that needs the build: a version that has none says so instead.
    fn guarded(&self, version: &VersionFacts, rule: impl FnOnce(&Self) -> SweepCell) -> SweepCell {
        match &version.unbuilt {
            Some(why) => not_measurable(version, format!("the build failed: {why}"), Vec::new()),
            None => rule(self),
        }
    }

    fn windows_only(&self, version: &VersionFacts, what: &str) -> SweepCell {
        not_measurable(
            version,
            format!(
                "Windows only ({what}); this sweep ran on {}. The sweep in CI runs on Linux and \
                 macOS, so this row is for the Windows job",
                self.os
            ),
            Vec::new(),
        )
    }

    // -----------------------------------------------------------------------------------------
    // Timings.

    fn timing(
        &self,
        metric: &str,
        fixture: &str,
        profile: Option<Profile>,
        drain: Option<u64>,
        version: &VersionFacts,
    ) -> Timing<'_> {
        let Some(set) = self.measurements.iter().find(|set| {
            let measurement: &SweepMeasurement = &set.measurement;
            measurement.metric == metric
                && measurement.fixture == fixture
                && measurement.profile == profile
                && measurement.drain_bytes_per_sec == drain
        }) else {
            return Timing::Missing(format!(
                "`{metric}` of `{fixture}` was not timed in this run (it is timed on the fixtures \
                 the run selected)"
            ));
        };
        match set
            .measurement
            .series
            .iter()
            .find(|series| series.reference == version.reference)
        {
            Some(series) => Timing::Series { set, series },
            None => Timing::Missing(set.errors.get(&version.reference).map_or_else(
                || "no round of it produced a timing".to_owned(),
                |error| format!("its runs could not be carried out: {error}"),
            )),
        }
    }

    fn candidate_series<'s>(&self, set: &'s MeasuredSet) -> Option<&'s SweepSeries> {
        let candidate = self.versions.last()?;
        set.measurement
            .series
            .iter()
            .find(|series| series.reference == candidate.reference)
    }

    /// A timing judged against the candidate's, round by round: F1, F16, and each half of F2.
    ///
    /// The ratio rests on the rounds in which both finished. A run that did not finish within the
    /// bound only bounds the ratio of its round, and [`judge`] says what the bounds can settle.
    fn relative(
        &self,
        version: &VersionFacts,
        set: &MeasuredSet,
        series: &SweepSeries,
        what: &str,
    ) -> SweepCell {
        let evidence = vec![format!("#/measurements/{}", set.index)];
        if self.is_candidate(version) {
            let census = Census::of(series);
            let mut value = format!("{what} median {}", format_ms(series.median));
            if census.unfinished > 0 {
                let _ = write!(
                    value,
                    "; {} of its {} measured runs did not finish within the bound",
                    census.unfinished, census.ran
                );
            }
            return not_affected(
                version,
                "the candidate: every ratio is taken to it",
                value,
                evidence,
            );
        }
        let candidate = self.candidate_series(set);
        let Some(ratio) = series.ratios.get("candidate") else {
            return not_measurable(
                version,
                "no round has a candidate timing to compare with",
                evidence,
            );
        };
        let own = series.median.map_or_else(
            || "no run finished".to_owned(),
            |ms| format!("median {}", format_ms(Some(ms))),
        );
        let against = candidate.map_or_else(
            || "no candidate timing".to_owned(),
            |candidate| {
                candidate.median.map_or_else(
                    || "no candidate run finished".to_owned(),
                    |ms| format!("the candidate's {}", format_ms(Some(ms))),
                )
            },
        );
        let value = format!(
            "{what}: {}; {own} against {against}",
            describe_ratio(ratio, "the candidate")
        );
        match judge(ratio, slowness(ratio), 1.0 + regression(), 1.0) {
            Call::Slower { by_bounds: true } => affected(
                version,
                slower_by_bounds(series, candidate, "the candidate", ratio),
                value,
                evidence,
            ),
            Call::Slower { by_bounds: false } => affected(
                version,
                "more than 20% slower than the candidate, with a 95% interval above 1.0",
                value,
                evidence,
            ),
            Call::NotSlower { by_bounds: true } => not_affected(
                version,
                not_slower_by_bounds(candidate, "the candidate", ratio),
                value,
                evidence,
            ),
            Call::NotSlower { by_bounds: false } => {
                not_affected(version, "no slower than the candidate", value, evidence)
            }
            Call::Unsure(doubt) => not_measurable(
                version,
                doubt_text(doubt, ratio, "the candidate").unwrap_or_else(|| {
                    format!(
                        "inconclusive: {} is not more than 20% slower with a confident interval, \
                         and not faster; more rounds would settle it",
                        describe_ratio(ratio, "the candidate")
                    )
                }),
                evidence,
            ),
        }
    }

    fn f1(&self, version: &VersionFacts) -> SweepCell {
        match self.timing("headless_wall_ms", roles::TIMING, None, None, version) {
            Timing::Missing(why) => not_measurable(version, why, Vec::new()),
            Timing::Series { set, series } => {
                let mut result = self.relative(version, set, series, "headless scan");
                if let (Some(ratio), Some(value)) = (series.ratios.get("du"), &mut result.value) {
                    let _ = write!(value, "; {}", describe_ratio(ratio, "`du -sk`"));
                }
                result
            }
        }
    }

    fn f16(&self, version: &VersionFacts) -> SweepCell {
        match self.timing("report_write_ms", roles::TIMING, None, None, version) {
            Timing::Missing(why) => not_measurable(version, why, Vec::new()),
            Timing::Series { set, series } => {
                self.relative(version, set, series, "writing the report")
            }
        }
    }

    /// F2: either motion mode slower than the candidate against a terminal that reads slowly. Each
    /// mode is judged by [`Self::relative`] from the rounds it has, and the value also gives
    /// default over reduced motion for the same version in the same round.
    fn f2(&self, version: &VersionFacts) -> SweepCell {
        let drain = Some(self.drain);
        let mut parts = Vec::new();
        for (profile, label) in [
            (Profile::Default, "default motion"),
            (Profile::ReducedMotion, "reduced motion"),
        ] {
            match self.timing(
                "tui_complete_ms",
                roles::TIMING,
                Some(profile),
                drain,
                version,
            ) {
                Timing::Missing(why) => parts.push((label, Err(why))),
                Timing::Series { set, series } => {
                    let result = self.relative(
                        version,
                        set,
                        series,
                        &format!("time to COMPLETE at {} KB/s, {label}", self.drain / 1000),
                    );
                    parts.push((label, Ok((result, series))));
                }
            }
        }
        let mut cells = Vec::new();
        let mut reasons = Vec::new();
        for (label, part) in &parts {
            match part {
                Ok((result, _)) => cells.push((*label, result.clone())),
                Err(why) => reasons.push(format!("{label}: {why}")),
            }
        }
        if cells.is_empty() {
            return not_measurable(version, reasons.join("; "), Vec::new());
        }
        let mut evidence: Vec<String> = cells
            .iter()
            .flat_map(|(_, result)| result.evidence.clone())
            .collect();
        evidence.dedup();
        let mut value = cells
            .iter()
            .filter_map(|(_, result)| result.value.clone())
            .collect::<Vec<_>>()
            .join("; ");
        if let Some(ratio) = parts.iter().find_map(|(label, part)| match part {
            Ok((_, series)) if *label == "default motion" => series.ratios.get("reduced-motion"),
            _ => None,
        }) {
            let _ = write!(value, "; {}", describe_ratio(ratio, "reduced motion"));
        }
        if let Some((label, result)) = cells
            .iter()
            .find(|(_, result)| result.state == CellState::Affected)
        {
            return affected(
                version,
                format!("{label}: {}", result.reason.clone().unwrap_or_default()),
                value,
                evidence,
            );
        }
        if cells
            .iter()
            .all(|(_, result)| result.state == CellState::NotAffected)
            && reasons.is_empty()
        {
            return not_affected(
                version,
                "no slower than the candidate against a terminal that reads 150 KB/s, in either motion mode",
                value,
                evidence,
            );
        }
        let why: Vec<String> = cells
            .iter()
            .filter(|(_, result)| result.state == CellState::NotMeasurable)
            .map(|(label, result)| {
                format!("{label}: {}", result.reason.clone().unwrap_or_default())
            })
            .chain(reasons)
            .collect();
        not_measurable(version, why.join("; "), evidence)
    }

    /// F18: the interface reaches COMPLETE within the budget on the headless scan of the same
    /// version, held round by round to the scan of the same round (the two were measured back to
    /// back in it). The scan is its time without the writing of its report: a release whose
    /// report write is slow (F1, F16) takes longer to finish headless than to scan, and holding
    /// the interface to that time would favour exactly the releases with the slow write, which
    /// would be a wrong `not-affected`. A round in which the write cannot be told from the scan
    /// gives no pair. The ratio rests on the rounds in which both finished, and a round in which
    /// one of them did not only bounds it, as [`judge`] says.
    fn f18(&self, version: &VersionFacts) -> SweepCell {
        let limit = default_limit(Budget::TuiCompleteRatio).unwrap_or(1.25);
        let Timing::Series { set, series } = self.timing(
            "tui_complete_ms",
            roles::TIMING,
            Some(Profile::Default),
            None,
            version,
        ) else {
            return not_measurable(
                version,
                "the interface was not timed to COMPLETE without a slow terminal in this run",
                Vec::new(),
            );
        };
        let mut evidence = vec![format!("#/measurements/{}", set.index)];
        let mut taken = |metric: &str| match self.timing(metric, roles::TIMING, None, None, version)
        {
            Timing::Series { set, series } => {
                evidence.push(format!("#/measurements/{}", set.index));
                Some(series)
            }
            Timing::Missing(_) => None,
        };
        let scan = taken("headless_scan_ms");
        let wall = taken("headless_wall_ms");
        let note = separation_note(wall, scan);
        let Some(ratio) = series.ratios.get("headless-scan") else {
            return not_measurable(
                version,
                match &note {
                    Some(note) => {
                        format!(
                            "no round gave a headless scan time to hold the interface to: {note}"
                        )
                    }
                    None => {
                        "no round has both a headless scan time and an interface time".to_owned()
                    }
                },
                evidence,
            );
        };
        let against = "the same version's headless scan without its report write";
        let value = format!(
            "time to COMPLETE {}; budget {limit}",
            describe_ratio(ratio, against)
        );
        let mut cell = match judge(ratio, within_budget(ratio, limit), limit, limit) {
            Call::Slower { by_bounds: true } => affected(
                version,
                slower_by_bounds(series, scan, "the headless scan", ratio),
                value,
                evidence,
            ),
            Call::Slower { by_bounds: false } => affected(
                version,
                format!("median over the {limit} budget, with a 95% interval above it"),
                value,
                evidence,
            ),
            Call::NotSlower { by_bounds: true } => not_affected(
                version,
                not_slower_by_bounds(scan, "the headless scan", ratio),
                value,
                evidence,
            ),
            Call::NotSlower { by_bounds: false } => not_affected(
                version,
                format!("median at most the {limit} budget"),
                value,
                evidence,
            ),
            Call::Unsure(doubt) => not_measurable(
                version,
                doubt_text(doubt, ratio, "the headless scan").unwrap_or_else(|| {
                    format!(
                        "inconclusive: median {} is over the {limit} budget, but the 95% interval \
                         reaches it; more rounds would settle it",
                        times(ratio.median.unwrap_or_default())
                    )
                }),
                evidence,
            ),
        };
        if let Some(note) = note {
            let text = if cell.state == CellState::NotMeasurable {
                &mut cell.reason
            } else {
                &mut cell.value
            };
            match text {
                Some(text) => {
                    let _ = write!(text, "; {note}");
                }
                None => *text = Some(note),
            }
        }
        cell
    }

    // -----------------------------------------------------------------------------------------
    // Interface checks.

    fn observed<'p, T>(
        version: &VersionFacts,
        placed: Option<&'p Placed<T>>,
        missing: &str,
    ) -> Result<(&'p T, &'p Placed<T>), SweepCell> {
        match placed {
            None => Err(not_measurable(version, missing, Vec::new())),
            Some(placed) => match &placed.checked.observation {
                Some(observation) => Ok((observation, placed)),
                None => Err(not_measurable(
                    version,
                    placed
                        .why_not()
                        .unwrap_or("the check did not run")
                        .to_owned(),
                    placed.evidence_paths(),
                )),
            },
        }
    }

    fn f3(version: &VersionFacts) -> SweepCell {
        let (observation, placed) =
            match Self::observed(version, version.idle.as_ref(), "the idle check did not run") {
                Ok(found) => found,
                Err(cell) => return cell,
            };
        let cpu_limit = default_limit(Budget::IdleCpuMs).unwrap_or(50.0);
        let IdleObservation {
            output_bytes,
            cpu_ms,
            after,
            window,
        } = *observation;
        let value = format!(
            "{output_bytes} bytes of output and {} of CPU over a {:.1} s window that starts {:.1} s after COMPLETE with no input",
            cpu_ms.map_or_else(|| "unknown".to_owned(), |cpu| format!("{cpu:.0} ms")),
            window.as_secs_f64(),
            after.as_secs_f64()
        );
        let loud = output_bytes > 0 || cpu_ms.is_some_and(|cpu| cpu > cpu_limit);
        if loud {
            affected(
                version,
                format!("a quiet terminal must see 0 bytes and at most {cpu_limit:.0} ms of CPU"),
                value,
                placed.evidence_paths(),
            )
        } else {
            not_affected(
                version,
                "no output and no more CPU than the budget while idle",
                value,
                placed.evidence_paths(),
            )
        }
    }

    fn f4(version: &VersionFacts) -> SweepCell {
        let (observation, placed) = match Self::observed(
            version,
            version.kill_restart.as_ref(),
            "the kill-and-restart check did not run",
        ) {
            Ok(found) => found,
            Err(cell) => return cell,
        };
        let KillRestartObservation {
            left_by_kill,
            survivors,
        } = observation;
        let value = format!(
            "SIGKILL left {} entries; {} were still there after a second start finished its scan",
            left_by_kill.len(),
            survivors.len()
        );
        if !survivors.is_empty() {
            affected(
                version,
                "what a killed run left was not swept by the next start",
                value,
                placed.evidence_paths(),
            )
        } else if left_by_kill.is_empty() {
            not_affected(
                version,
                "a killed run leaves nothing behind",
                value,
                placed.evidence_paths(),
            )
        } else {
            not_affected(
                version,
                "the next start swept everything a killed run left",
                value,
                placed.evidence_paths(),
            )
        }
    }

    fn f5(&self, version: &VersionFacts) -> SweepCell {
        if version.drift.is_none() && !self.plan.is_full() {
            return not_measurable(
                version,
                "the reproduction needs the 20,053-entry selection-drift fixture, which only the \
                 full tier runs: use `cargo xtask sweep --full`",
                Vec::new(),
            );
        }
        let (observation, placed) = match Self::observed(
            version,
            version.drift.as_ref(),
            "the selection-drift check did not run",
        ) {
            Ok(found) => found,
            Err(cell) => return cell,
        };
        let DriftObservation { attempts } = observation;
        let drifted = observation.drifted();
        let value = observation.summary();
        if drifted > 0 {
            affected(
                version,
                "reproduced: the selection moved off the chosen entry through COMPLETE",
                value,
                placed.evidence_paths(),
            )
        } else {
            not_measurable(
                version,
                format!(
                    "not reproduced in {} attempts (the precondition held in {}; the scan reached \
                     COMPLETE after it in {} and did not in {}, where nothing was read after \
                     COMPLETE); the defect was observed once and does not reproduce on demand, so \
                     this does not show the version unaffected",
                    attempts.len(),
                    observation.preconditions(),
                    observation.completed(),
                    observation.incomplete()
                ),
                placed.evidence_paths(),
            )
        }
    }

    /// `reason`, and on a platform where the sweep leaves a signal out, why it does.
    fn with_quit_note(&self, reason: impl Into<String>) -> String {
        let reason = reason.into();
        match skipped_note(self.os) {
            Some(note) => format!("{reason}; {note}"),
            None => reason,
        }
    }

    fn f6(&self, version: &VersionFacts) -> SweepCell {
        let mut parts = Vec::new();
        let mut evidence = Vec::new();
        let mut unclean = 0;
        let mut missing = Vec::new();
        for signal in signals_for(self.os) {
            let name = signal.as_str();
            match version.signals.get(name) {
                Some(placed) => {
                    evidence.extend(placed.evidence_paths());
                    match &placed.checked.observation {
                        Some(observation) => {
                            if !observation.is_clean() {
                                unclean += 1;
                            }
                            parts.push(format!(
                                "{}: {}",
                                signal_label(*signal),
                                observation.summary()
                            ));
                        }
                        None => missing.push(format!(
                            "{}: {}",
                            signal_label(*signal),
                            placed.why_not().unwrap_or("did not run")
                        )),
                    }
                }
                None => missing.push(format!("{}: the check did not run", signal_label(*signal))),
            }
        }
        if parts.is_empty() {
            return not_measurable(version, self.with_quit_note(missing.join("; ")), evidence);
        }
        let value = parts.join(" | ");
        if unclean > 0 {
            let rule = "a signal must be a confirmed quit: exit 130, the terminal restored, nothing left behind";
            affected(version, self.with_quit_note(rule), value, evidence)
        } else if missing.is_empty() {
            let rule = "every signal ended the program with exit 130, the terminal restored, and nothing left behind";
            not_affected(version, self.with_quit_note(rule), value, evidence)
        } else {
            let rule = format!(
                "the signals that ran were clean, but not every one ran: {}",
                missing.join("; ")
            );
            not_measurable(version, self.with_quit_note(rule), evidence)
        }
    }

    fn f24(&self, version: &VersionFacts) -> SweepCell {
        if self.os != "windows" {
            return self.windows_only(version, "the exit status after the console closes");
        }
        self.guarded(version, |_| {
            let (observation, placed) = match Self::observed(
                version,
                version.signals.get(Signal::Close.as_str()),
                "the console close check did not run",
            ) {
                Ok(found) => found,
                Err(cell) => return cell,
            };
            let value = observation.summary();
            if observation.exit.is_some_and(|exit| exit.code == Some(130)) {
                not_affected(
                    version,
                    "the program exits 130 when the console closes",
                    value,
                    placed.evidence_paths(),
                )
            } else {
                affected(
                    version,
                    "the program must exit 130 when the console closes",
                    value,
                    placed.evidence_paths(),
                )
            }
        })
    }

    fn f22(version: &VersionFacts) -> SweepCell {
        let (observation, placed) = match Self::observed(
            version,
            version.filter.as_ref(),
            "the filter check did not run",
        ) {
            Ok(found) => found,
            Err(cell) => return cell,
        };
        let FilterObservation {
            at_root, in_folder, ..
        } = observation;
        let attempts = [at_root, in_folder];
        let value = attempts
            .iter()
            .map(|attempt| format!("{}: {}", attempt.place, attempt.describe()))
            .collect::<Vec<_>>()
            .join("; ");
        if attempts.iter().any(|attempt| attempt.ended()) {
            return affected(
                version,
                "a filter whose matches lie below the folder it is applied in ended the program",
                value,
                placed.evidence_paths(),
            );
        }
        let missing: Vec<(&str, &str)> = attempts
            .iter()
            .filter_map(|attempt| attempt.why_not().map(|why| (attempt.place.as_str(), why)))
            .collect();
        match missing.as_slice() {
            [] => not_affected(
                version,
                "the program survived the filter, at the root and inside an opened folder",
                value,
                placed.evidence_paths(),
            ),
            [(place, why)] => not_measurable(
                version,
                format!(
                    "only one of the two filters ran: the filter {place} did not run ({why}), so \
                     the program surviving the other does not show the version unaffected"
                ),
                placed.evidence_paths(),
            ),
            _ => not_measurable(
                version,
                format!(
                    "neither filter ran: {}",
                    missing
                        .iter()
                        .map(|(place, why)| format!("{place}: {why}"))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                placed.evidence_paths(),
            ),
        }
    }

    /// F23b: where the selected-item pane puts the untouched cursor at COMPLETE. The defect is a
    /// cursor that stays on a small entry the scan listed on its first page while much larger
    /// ones appeared, so a check that finds the cursor on the largest entry shows the defect
    /// absent only where the largest entry is not the one listed first. Which entry a scan lists
    /// first is the order the file system lists the folder in. The program found the defect on
    /// Windows, which lists a folder in name order, so `keep-a.bin` comes before `victim`; the
    /// file systems of macOS and Linux list in an order of their own, and the check does not see
    /// the order. So a cursor stuck on a small entry is `affected` on any platform, and a cursor
    /// on the largest entry is `not-affected` on Windows only: elsewhere it is `not-measurable`.
    fn f23b(&self, version: &VersionFacts) -> SweepCell {
        let (observation, placed) = match Self::observed(
            version,
            version.filter.as_ref(),
            "the cursor check did not run",
        ) {
            Ok(found) => found,
            Err(cell) => return cell,
        };
        let Some(largest) = &observation.largest else {
            return not_measurable(
                version,
                "the fixture's largest entry could not be worked out from the oracle",
                placed.evidence_paths(),
            );
        };
        match &observation.selected_at_complete {
            Inspector::Item(item) if item.name == *largest && self.os == "windows" => not_affected(
                version,
                "the untouched cursor is on the largest entry at COMPLETE",
                format!(
                    "the selected-item pane shows `{}`, the largest entry",
                    item.name
                ),
                placed.evidence_paths(),
            ),
            Inspector::Item(item) if item.name == *largest => not_measurable(
                version,
                format!(
                    "the selected-item pane shows `{}`, the largest entry, at COMPLETE, which \
                     does not show the defect absent on {}: the defect needs a small entry to be \
                     listed before the largest one, and the order a file system lists a folder in \
                     is its own (the program found the defect on Windows, which lists in name \
                     order, so `keep-a.bin` comes before `victim`). A cursor stuck on a small \
                     entry would be `affected` on any platform; a cursor on the largest entry is \
                     read as `not-affected` on Windows only",
                    item.name, self.os
                ),
                placed.evidence_paths(),
            ),
            Inspector::Item(item) => affected(
                version,
                "the untouched cursor is not on the largest entry at COMPLETE",
                format!(
                    "the selected-item pane shows `{}`; the largest entry is `{largest}`",
                    item.name
                ),
                placed.evidence_paths(),
            ),
            Inspector::NothingSelected => affected(
                version,
                "the untouched cursor is not on the largest entry at COMPLETE",
                format!(
                    "the selected-item pane invites a choice; the largest entry is `{largest}`"
                ),
                placed.evidence_paths(),
            ),
            Inspector::NotShown => not_measurable(
                version,
                "the selected-item pane was not found on the screen, so the cursor cannot be read",
                placed.evidence_paths(),
            ),
        }
    }

    fn f13(&self, version: &VersionFacts) -> SweepCell {
        if version.descriptors.is_none() && !self.plan.is_full() {
            return not_measurable(
                version,
                "needs the 49,050-entry tiny-files-50k fixture, which only the full tier runs: use \
                 `cargo xtask sweep --full`",
                Vec::new(),
            );
        }
        let (observation, placed) = match Self::observed(
            version,
            version.descriptors.as_ref(),
            "the descriptor check did not run",
        ) {
            Ok(found) => found,
            Err(cell) => return cell,
        };
        let DescriptorObservation { small, large } = *observation;
        let Some(large) = large else {
            return not_measurable(
                version,
                "the large tree did not reach COMPLETE within the bound, so its descriptors were not counted",
                placed.evidence_paths(),
            );
        };
        let value = format!(
            "{small} descriptors at most on a {}-entry tree, {large} on a {}-entry tree",
            "1,002", "49,050"
        );
        if large > small + DESCRIPTOR_GROWTH {
            affected(
                version,
                format!(
                    "the descriptors held grew by more than {DESCRIPTOR_GROWTH} with the size of the tree"
                ),
                value,
                placed.evidence_paths(),
            )
        } else {
            not_affected(
                version,
                format!(
                    "the descriptors held grew by at most {DESCRIPTOR_GROWTH} with the size of the tree"
                ),
                value,
                placed.evidence_paths(),
            )
        }
    }

    // -----------------------------------------------------------------------------------------
    // The headless oracle.

    fn oracle_of<'v>(
        version: &'v VersionFacts,
        fixture: &str,
    ) -> Result<(&'v OracleObservation, &'v Placed<OracleObservation>), String> {
        let Some(placed) = version.oracle.get(fixture) else {
            return Err(format!(
                "fixture `{fixture}` was not run in this sweep (the headless fixtures are those \
                 of the tier, or `--fixture`)"
            ));
        };
        match &placed.checked.observation {
            Some(observation) => Ok((observation, placed)),
            None => Err(placed
                .why_not()
                .unwrap_or("the scan could not be carried out")
                .to_owned()),
        }
    }

    fn oracle_kind(
        version: &VersionFacts,
        fixture: &str,
        kind: DiscrepancyKind,
        what: &str,
    ) -> SweepCell {
        let (observation, placed) = match Self::oracle_of(version, fixture) {
            Ok(found) => found,
            Err(why) => return not_measurable(version, why, Vec::new()),
        };
        let evidence = placed.evidence_paths();
        match &observation.report {
            ReportRead::Read {
                kinds,
                version: report,
                ..
            } => {
                let value = if kinds.is_empty() {
                    format!(
                        "report version {report}: no discrepancy from the oracle on `{fixture}`"
                    )
                } else {
                    format!(
                        "report version {report}: {} on `{fixture}`",
                        kinds
                            .iter()
                            .map(|(kind, count)| format!("{kind} x{count}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                if kinds.contains_key(&kind) {
                    affected(
                        version,
                        format!("the oracle diff finds a `{kind}` discrepancy: {what}"),
                        value,
                        evidence,
                    )
                } else {
                    not_affected(
                        version,
                        format!("the oracle diff finds no `{kind}` discrepancy"),
                        value,
                        evidence,
                    )
                }
            }
            ReportRead::Missing(why) => not_measurable(
                version,
                format!("the scan of `{fixture}` wrote no report: {why}"),
                evidence,
            ),
            ReportRead::Invalid(why) => not_measurable(
                version,
                format!("the report of `{fixture}` could not be read: {why}"),
                evidence,
            ),
        }
    }

    fn f7(version: &VersionFacts) -> SweepCell {
        let reads: Vec<(&String, &OracleObservation, &Placed<OracleObservation>)> = version
            .oracle
            .iter()
            .filter_map(|(fixture, placed)| {
                let observation = placed.checked.observation.as_ref()?;
                matches!(observation.report, ReportRead::Read { .. }).then_some((
                    fixture,
                    observation,
                    placed,
                ))
            })
            .collect();
        let Some((fixture, observation, placed)) = reads.first() else {
            return not_measurable(
                version,
                "no headless report was read, so there is no scan-store limit to read",
                Vec::new(),
            );
        };
        let evidence = placed.evidence_paths();
        let ReportRead::Read {
            version: report,
            scan_store_limit_bytes: limit,
            ..
        } = &observation.report
        else {
            return not_measurable(version, "no readable report", evidence);
        };
        if *report < 3 {
            return not_affected(
                version,
                "the build has no scan store: its report states the model's memory limit, not a scratch-space quota",
                format!("report version {report}"),
                evidence,
            );
        }
        let Some(available) = observation.available_bytes else {
            return not_measurable(
                version,
                "the platform cannot say how much space is free on the scratch volume",
                evidence,
            );
        };
        #[allow(clippy::cast_precision_loss)]
        let ratio = *limit as f64 / available.max(1) as f64;
        let value = format!(
            "the report on `{fixture}` states a scan-store limit of {} on a volume with {} free ({} of it)",
            bytes(*limit),
            bytes(available),
            times(ratio)
        );
        if *limit > available {
            affected(
                version,
                "the quota is larger than the free space, so the quarter reserve is not enforced",
                value,
                evidence,
            )
        } else {
            not_affected(
                version,
                "the quota is within the free space, as the quarter reserve requires",
                value,
                evidence,
            )
        }
    }
}

/// Whether the row is one of the paired timings, whose candidate is `not-affected` by definition.
fn is_timing_row(id: &str) -> bool {
    matches!(id, "F1" | "F2" | "F16")
}

/// What to say when the report write could not be told apart from the scan in some of the rounds
/// the headless scan was timed in: `wall` holds the rounds a wall time was taken in, and `scan`
/// those in which a time without the report could be taken as well.
fn separation_note(wall: Option<&SweepSeries>, scan: Option<&SweepSeries>) -> Option<String> {
    let wall = wall?;
    let separated = scan.map_or(0, |scan| {
        scan.rounds
            .iter()
            .filter(|round| wall.rounds.contains(round))
            .count()
    });
    let lost = wall.rounds.len().checked_sub(separated)?;
    (lost > 0).then(|| {
        format!(
            "the report write could not be told apart from the scan in {lost} of the {} rounds \
             the scan was timed in (the report was seen to change in size fewer than twice, or \
             its write left no time for a scan), so those rounds give no scan time without the \
             report",
            wall.rounds.len()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Duration};

    use crate::{
        headless::{DiscrepancyKind, document::ScanState, process::Ended},
        pty::ui::SelectedItem,
        report::{ConfidenceInterval, SweepTier},
    };

    use super::{
        super::checks::{
            Checked, DriftAttempt, Exit, FilterAttempt, FilterOutcome, Recorded, SignalObservation,
        },
        *,
    };

    use super::super::{
        headless::{ReportGrowth, TimedScan},
        timing::{Against, Reading, RunValue, paired_rounds, series_of},
    };

    const CANDIDATE: &str = "HEAD";

    fn version(reference: &str) -> VersionFacts {
        VersionFacts {
            reference: reference.to_owned(),
            ..VersionFacts::default()
        }
    }

    fn placed<T>(observation: Option<T>) -> Placed<T> {
        let record = match &observation {
            Some(_) => Recorded::new("check", None, None),
            None => Recorded::not_run("check", None, None, "the header never read COMPLETE"),
        };
        Placed {
            checked: Checked {
                observation,
                record,
            },
            pointer: "#/checks/0".to_owned(),
            evidence: Some("evidence/check.txt".to_owned()),
        }
    }

    fn plan(tier: SweepTier) -> Plan {
        Plan {
            tier,
            oracle_fixtures: vec![roles::TIMING.to_owned()],
            timing_fixtures: vec![roles::TIMING.to_owned()],
        }
    }

    fn classify_all(
        versions: &[VersionFacts],
        measurements: &[MeasuredSet],
        tier: SweepTier,
        os: &str,
    ) -> Vec<SweepRow> {
        Classifier {
            versions,
            measurements,
            plan: &plan(tier),
            os,
            drain: 150_000,
        }
        .rows()
    }

    fn row<'r>(rows: &'r [SweepRow], defect: &str) -> &'r SweepRow {
        rows.iter()
            .find(|row| row.defect == defect)
            .unwrap_or_else(|| panic!("no row for {defect}"))
    }

    fn state(rows: &[SweepRow], defect: &str, reference: &str) -> CellState {
        row(rows, defect)
            .cells
            .iter()
            .find(|cell| cell.reference == reference)
            .unwrap_or_else(|| panic!("no cell for {defect} on {reference}"))
            .state
    }

    fn cell<'r>(rows: &'r [SweepRow], defect: &str, reference: &str) -> &'r SweepCell {
        row(rows, defect)
            .cells
            .iter()
            .find(|cell| cell.reference == reference)
            .unwrap_or_else(|| panic!("no cell for {defect} on {reference}"))
    }

    /// A ratio over five rounds in which both sides finished, with a 95% interval, and no round in
    /// which a run did not finish.
    fn ratio(median: f64, lower: f64, upper: f64) -> SweepRatio {
        SweepRatio {
            median: Some(median),
            bootstrap_ci: Some(ConfidenceInterval {
                lower,
                upper,
                confidence: 0.95,
            }),
            pairs: 5,
            lower_bounds: Vec::new(),
            upper_bounds: Vec::new(),
            both_censored: 0,
        }
    }

    /// A ratio of rounds in none of which both sides finished: only the bounds that the runs that
    /// did not finish give, and how many rounds neither side finished in.
    fn bounds(lower_bounds: &[f64], upper_bounds: &[f64], both_censored: u32) -> SweepRatio {
        SweepRatio {
            median: None,
            bootstrap_ci: None,
            pairs: 0,
            lower_bounds: lower_bounds.to_vec(),
            upper_bounds: upper_bounds.to_vec(),
            both_censored,
        }
    }

    /// `ratio` with these bounds from rounds in which a run did not finish.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "it reads like the struct update it stands for"
    )]
    fn and_bounds(ratio: SweepRatio, lower_bounds: &[f64], upper_bounds: &[f64]) -> SweepRatio {
        SweepRatio {
            lower_bounds: lower_bounds.to_vec(),
            upper_bounds: upper_bounds.to_vec(),
            ..ratio
        }
    }

    fn series(
        reference: &str,
        median: f64,
        completed: bool,
        to_candidate: Option<SweepRatio>,
    ) -> SweepSeries {
        SweepSeries {
            reference: reference.to_owned(),
            rounds: (0..5).collect(),
            samples: vec![median; 5],
            completed: vec![completed; 5],
            skipped: 0,
            median: completed.then_some(median),
            ratios: to_candidate
                .into_iter()
                .map(|ratio| ("candidate".to_owned(), ratio))
                .collect(),
        }
    }

    /// A series that ran `ran` rounds, the first `unfinished` of which did not finish within the
    /// bound, and skipped `skipped` more, with `ratios` by the name of what each is taken against.
    fn ran_out_of_time(
        reference: &str,
        ran: u32,
        unfinished: u32,
        skipped: u32,
        ratios: Vec<(&str, SweepRatio)>,
    ) -> SweepSeries {
        let completed: Vec<bool> = (0..ran).map(|round| round >= unfinished).collect();
        SweepSeries {
            reference: reference.to_owned(),
            rounds: (0..ran).collect(),
            samples: completed
                .iter()
                .map(|done| if *done { 4_000.0 } else { 120_000.0 })
                .collect(),
            median: (unfinished < ran).then_some(4_000.0),
            completed,
            skipped,
            ratios: ratios
                .into_iter()
                .map(|(name, ratio)| (name.to_owned(), ratio))
                .collect(),
        }
    }

    /// Asserts that `text` has every one of `fragments`.
    fn assert_has(text: &str, fragments: &[&str]) {
        for fragment in fragments {
            assert!(text.contains(fragment), "`{fragment}` is not in: {text}");
        }
    }

    fn measured(
        index: usize,
        metric: &str,
        profile: Option<Profile>,
        drain: Option<u64>,
        series: Vec<SweepSeries>,
    ) -> MeasuredSet {
        MeasuredSet {
            index,
            measurement: SweepMeasurement {
                metric: metric.to_owned(),
                fixture: roles::TIMING.to_owned(),
                profile,
                drain_bytes_per_sec: drain,
                rounds: 5,
                order: Vec::new(),
                du_samples: Vec::new(),
                series,
            },
            errors: BTreeMap::new(),
        }
    }

    fn clean_signal(signal: Signal) -> SignalObservation {
        SignalObservation {
            signal,
            exit: Some(Exit {
                code: Some(130),
                signal: None,
            }),
            restored: Some(true),
            residue: Vec::new(),
        }
    }

    fn killed_by(signal: Signal, number: i32) -> SignalObservation {
        SignalObservation {
            exit: Some(Exit {
                code: None,
                signal: Some(number),
            }),
            restored: Some(false),
            residue: vec!["store/.excise-scan-x".to_owned()],
            ..clean_signal(signal)
        }
    }

    fn oracle(
        report_version: u32,
        kinds: &[DiscrepancyKind],
        limit: u64,
        available: Option<u64>,
    ) -> Placed<OracleObservation> {
        placed(Some(OracleObservation {
            ended: Ended::Exited(0),
            timed_out: false,
            wall: Duration::from_secs(1),
            residue: Vec::new(),
            report: ReportRead::Read {
                version: report_version,
                state: ScanState::Exact,
                kinds: kinds.iter().map(|kind| (*kind, 1)).collect(),
                scan_store_limit_bytes: limit,
            },
            available_bytes: available,
        }))
    }

    #[test]
    fn the_table_has_a_row_for_every_defect_and_a_cell_for_every_version_in_order() {
        let versions = [version("v1.0.0"), version("v1.3.0"), version(CANDIDATE)];

        let rows = classify_all(&versions, &[], SweepTier::Quick, "linux");

        assert_eq!(rows.len(), DEFECTS.len());
        for (row, defect) in rows.iter().zip(DEFECTS) {
            assert_eq!(row.defect, defect.id);
            assert_eq!(row.title, defect.title);
            assert_eq!(row.changelog, defect.changelog);
            let references: Vec<&str> = row
                .cells
                .iter()
                .map(|cell| cell.reference.as_str())
                .collect();
            assert_eq!(
                references,
                ["v1.0.0", "v1.3.0", CANDIDATE],
                "{}",
                row.defect
            );
            for cell in &row.cells {
                if cell.state == CellState::NotMeasurable {
                    assert!(
                        cell.reason
                            .as_deref()
                            .is_some_and(|reason| !reason.is_empty()),
                        "{} on {}: a cell that is not measurable says why",
                        row.defect,
                        cell.reference
                    );
                }
            }
        }
    }

    #[test]
    fn deletion_defects_are_not_measurable_on_any_version_with_the_reason_a_reviewer_can_check() {
        let mut unbuilt = version("v1.1.0");
        unbuilt.unbuilt = Some("the build failed: error[E0463]".to_owned());
        let versions = [version("v1.0.0"), unbuilt, version(CANDIDATE)];

        let rows = classify_all(&versions, &[], SweepTier::Full, "macos");

        for defect in ["F9", "F14", "F19", "F21"] {
            for reference in ["v1.0.0", "v1.1.0", CANDIDATE] {
                let found = cell(&rows, defect, reference);
                assert_eq!(found.state, CellState::NotMeasurable, "{defect}");
                assert_eq!(found.reason.as_deref(), Some(DELETION_REASON), "{defect}");
            }
        }
    }

    #[test]
    fn a_version_that_could_not_be_built_says_so_in_every_cell_that_needs_a_binary() {
        let mut unbuilt = version("v1.1.0");
        unbuilt.unbuilt = Some("the build failed: error[E0463]".to_owned());
        let versions = [unbuilt, version(CANDIDATE)];

        let rows = classify_all(&versions, &[], SweepTier::Full, "linux");

        for defect in [
            "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F10", "F11", "F13", "F16", "F18", "F22",
            "F23b",
        ] {
            let found = cell(&rows, defect, "v1.1.0");
            assert_eq!(found.state, CellState::NotMeasurable, "{defect}");
            assert!(
                found
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("the build failed")),
                "{defect}: {:?}",
                found.reason
            );
        }
    }

    #[test]
    fn windows_defects_are_marked_for_the_windows_job_elsewhere_and_f24_is_measured_there() {
        let mut old = version("v1.3.0");
        old.signals.insert(
            "close".to_owned(),
            placed(Some(SignalObservation {
                signal: Signal::Close,
                exit: Some(Exit {
                    code: Some(0xC000_013A_u32.cast_signed()),
                    signal: None,
                }),
                restored: None,
                residue: Vec::new(),
            })),
        );
        let mut new = version(CANDIDATE);
        new.signals.insert(
            "close".to_owned(),
            placed(Some(clean_signal(Signal::Close))),
        );
        let versions = [old, new];

        let elsewhere = classify_all(&versions, &[], SweepTier::Quick, "macos");
        for defect in ["F8", "F15", "F24"] {
            let found = cell(&elsewhere, defect, "v1.3.0");
            assert_eq!(found.state, CellState::NotMeasurable, "{defect}");
            assert!(
                found
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("Windows only")),
                "{defect}"
            );
        }

        let on_windows = classify_all(&versions, &[], SweepTier::Quick, "windows");
        assert_eq!(state(&on_windows, "F24", "v1.3.0"), CellState::Affected);
        assert_eq!(state(&on_windows, "F24", CANDIDATE), CellState::NotAffected);
        assert_eq!(state(&on_windows, "F8", "v1.3.0"), CellState::NotMeasurable);
    }

    #[test]
    fn signals_are_affected_unless_every_one_is_a_confirmed_quit() {
        let mut old = version("v1.3.0");
        let mut new = version(CANDIDATE);
        for signal in [Signal::Term, Signal::Hup, Signal::Quit] {
            old.signals.insert(
                signal.as_str().to_owned(),
                placed(Some(killed_by(signal, 15))),
            );
            new.signals.insert(
                signal.as_str().to_owned(),
                placed(Some(clean_signal(signal))),
            );
        }
        let mut partial = version("v1.2.4");
        partial
            .signals
            .insert("term".to_owned(), placed(Some(clean_signal(Signal::Term))));
        partial
            .signals
            .insert("hup".to_owned(), placed(Some(clean_signal(Signal::Hup))));
        partial
            .signals
            .insert("quit".to_owned(), placed(None::<SignalObservation>));
        let versions = [partial, old, new];

        let rows = classify_all(&versions, &[], SweepTier::Quick, "linux");

        let broken = cell(&rows, "F6", "v1.3.0");
        assert_eq!(broken.state, CellState::Affected);
        let value = broken.value.as_deref().expect("a measured value");
        assert!(
            value.contains("SIGTERM: killed by signal 15, terminal not restored, 1 left behind")
        );
        assert!(value.contains("SIGHUP") && value.contains("SIGQUIT"));
        assert!(
            broken
                .evidence
                .iter()
                .any(|path| path == "evidence/check.txt")
        );
        assert_eq!(state(&rows, "F6", CANDIDATE), CellState::NotAffected);
        let open = cell(&rows, "F6", "v1.2.4");
        assert_eq!(open.state, CellState::NotMeasurable);
        assert!(
            open.reason
                .as_deref()
                .is_some_and(|reason| reason.contains("SIGQUIT"))
        );
        assert!(row(&rows, "F6").note.is_none(), "the candidate is clean");
    }

    /// A version that was sent `signals`, as the checks record them: `None` is a check that could
    /// not run.
    fn signalled(reference: &str, signals: &[(Signal, Option<SignalObservation>)]) -> VersionFacts {
        let mut found = version(reference);
        for (signal, observation) in signals {
            found
                .signals
                .insert(signal.as_str().to_owned(), placed(observation.clone()));
        }
        found
    }

    #[test]
    fn on_macos_the_cell_rests_on_sigterm_and_sighup_and_says_why_sigquit_is_not_sent() {
        let versions = [
            signalled(
                "v1.3.0",
                &[
                    (Signal::Term, Some(killed_by(Signal::Term, 15))),
                    (Signal::Hup, Some(killed_by(Signal::Hup, 1))),
                ],
            ),
            signalled(
                "v1.2.4",
                &[
                    (Signal::Term, Some(clean_signal(Signal::Term))),
                    (Signal::Hup, Some(killed_by(Signal::Hup, 1))),
                ],
            ),
            signalled(
                "v1.2.0",
                &[
                    (Signal::Term, Some(clean_signal(Signal::Term))),
                    (Signal::Hup, None),
                ],
            ),
            signalled(
                CANDIDATE,
                &[
                    (Signal::Term, Some(clean_signal(Signal::Term))),
                    (Signal::Hup, Some(clean_signal(Signal::Hup))),
                ],
            ),
        ];

        let rows = classify_all(&versions, &[], SweepTier::Quick, "macos");

        // Both signals clean and no SIGQUIT check at all: nothing is missing, so the cell is
        // decided, and it says what it was not decided from.
        let clean = cell(&rows, "F6", CANDIDATE);
        assert_eq!(clean.state, CellState::NotAffected);
        let reason = clean.reason.as_deref().expect("a reason");
        assert!(reason.contains("SIGQUIT is not sent on macOS"), "{reason}");
        assert!(
            reason.contains("~/Library/Logs/DiagnosticReports"),
            "{reason}"
        );
        let value = clean.value.as_deref().expect("a value");
        assert!(
            value.contains("SIGTERM") && value.contains("SIGHUP"),
            "{value}"
        );
        assert!(!value.contains("SIGQUIT"), "{value}");
        assert!(row(&rows, "F6").note.is_none(), "the candidate is clean");
        // One unclean signal of the two decides it, whatever the other did.
        for broken in ["v1.3.0", "v1.2.4"] {
            let found = cell(&rows, "F6", broken);
            assert_eq!(found.state, CellState::Affected, "{broken}");
            let reason = found.reason.as_deref().expect("a reason");
            assert!(
                reason.contains("SIGQUIT is not sent on macOS"),
                "{broken}: {reason}"
            );
        }
        // A signal the sweep does send that did not run leaves the cell open, and names it.
        let open = cell(&rows, "F6", "v1.2.0");
        assert_eq!(open.state, CellState::NotMeasurable);
        let reason = open.reason.as_deref().expect("a reason");
        assert!(reason.contains("SIGHUP: "), "{reason}");
        assert!(!reason.contains("SIGQUIT: "), "{reason}");
        assert!(reason.contains("SIGQUIT is not sent on macOS"), "{reason}");
    }

    #[test]
    fn off_macos_a_sigquit_that_did_not_run_keeps_the_cell_open() {
        let versions = [
            signalled(
                "v1.2.4",
                &[
                    (Signal::Term, Some(clean_signal(Signal::Term))),
                    (Signal::Hup, Some(clean_signal(Signal::Hup))),
                ],
            ),
            signalled(
                CANDIDATE,
                &[
                    (Signal::Term, Some(clean_signal(Signal::Term))),
                    (Signal::Hup, Some(clean_signal(Signal::Hup))),
                    (Signal::Quit, Some(clean_signal(Signal::Quit))),
                ],
            ),
        ];

        let rows = classify_all(&versions, &[], SweepTier::Quick, "linux");

        let open = cell(&rows, "F6", "v1.2.4");
        assert_eq!(open.state, CellState::NotMeasurable);
        let reason = open.reason.as_deref().expect("a reason");
        assert!(
            reason.contains("SIGQUIT: the check did not run"),
            "{reason}"
        );
        assert!(!reason.contains("macOS"), "{reason}");
        let clean = cell(&rows, "F6", CANDIDATE);
        assert_eq!(clean.state, CellState::NotAffected);
        let reason = clean.reason.as_deref().expect("a reason");
        assert!(!reason.contains("macOS"), "{reason}");
        let value = clean.value.as_deref().expect("a value");
        assert!(value.contains("SIGQUIT"), "{value}");
    }

    #[test]
    fn on_windows_the_cell_rests_on_the_console_close_alone() {
        let versions = [
            signalled(
                "v1.3.0",
                &[(Signal::Close, Some(killed_by(Signal::Close, 1)))],
            ),
            signalled(
                CANDIDATE,
                &[(Signal::Close, Some(clean_signal(Signal::Close)))],
            ),
        ];

        let rows = classify_all(&versions, &[], SweepTier::Quick, "windows");

        assert_eq!(state(&rows, "F6", "v1.3.0"), CellState::Affected);
        let clean = cell(&rows, "F6", CANDIDATE);
        assert_eq!(clean.state, CellState::NotAffected);
        let reason = clean.reason.as_deref().expect("a reason");
        assert!(!reason.contains("macOS"), "{reason}");
    }

    #[test]
    fn a_kill_that_leaves_something_the_next_start_does_not_sweep_is_the_defect() {
        let observed = |left: &[&str], survivors: &[&str]| {
            let mut found = version("v");
            found.kill_restart = Some(placed(Some(KillRestartObservation {
                left_by_kill: left.iter().map(|entry| (*entry).to_owned()).collect(),
                survivors: survivors.iter().map(|entry| (*entry).to_owned()).collect(),
            })));
            found
        };
        let mut leaves = observed(&["store/.excise-scan-a"], &["store/.excise-scan-a"]);
        leaves.reference = "v1.3.0".to_owned();
        let mut sweeps = observed(&["store/.excise-scan-a"], &[]);
        sweeps.reference = "v1.2.4".to_owned();
        let mut clean = observed(&[], &[]);
        clean.reference = CANDIDATE.to_owned();
        let versions = [sweeps, leaves, clean];

        let rows = classify_all(&versions, &[], SweepTier::Quick, "linux");

        assert_eq!(state(&rows, "F4", "v1.3.0"), CellState::Affected);
        assert_eq!(state(&rows, "F4", "v1.2.4"), CellState::NotAffected);
        assert_eq!(state(&rows, "F4", CANDIDATE), CellState::NotAffected);
        assert!(
            cell(&rows, "F4", "v1.2.4")
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("swept"))
        );
    }

    #[test]
    fn an_idle_program_that_writes_or_burns_is_the_defect() {
        let idle = |output_bytes, cpu_ms| {
            placed(Some(IdleObservation {
                output_bytes,
                cpu_ms,
                after: Duration::from_millis(3200),
                window: Duration::from_millis(5000),
            }))
        };
        let mut old = version("v1.3.0");
        old.idle = Some(idle(4_900_000, Some(180.0)));
        let mut cpu_only = version("v1.2.0");
        cpu_only.idle = Some(idle(0, Some(300.0)));
        let mut new = version(CANDIDATE);
        new.idle = Some(idle(0, Some(2.0)));
        let mut unknown_cpu = version("v1.1.0");
        unknown_cpu.idle = Some(idle(0, None));
        let versions = [unknown_cpu, cpu_only, old, new];

        let rows = classify_all(&versions, &[], SweepTier::Quick, "linux");

        assert_eq!(state(&rows, "F3", "v1.3.0"), CellState::Affected);
        assert_eq!(state(&rows, "F3", "v1.2.0"), CellState::Affected);
        assert_eq!(state(&rows, "F3", "v1.1.0"), CellState::NotAffected);
        assert_eq!(state(&rows, "F3", CANDIDATE), CellState::NotAffected);
        let value = cell(&rows, "F3", "v1.3.0").value.clone().expect("a value");
        assert!(
            value.contains("4900000 bytes") && value.contains("5.0 s window"),
            "{value}"
        );
    }

    #[test]
    fn the_oracle_diff_decides_the_unreadable_folder_and_the_deep_path_defects() {
        let mut old = version("v1.3.0");
        old.oracle.insert(
            roles::HOSTILE.to_owned(),
            oracle(3, &[DiscrepancyKind::EntryState], 0, None),
        );
        old.oracle.insert(
            roles::DEEP.to_owned(),
            oracle(
                3,
                &[DiscrepancyKind::Missing, DiscrepancyKind::Summary],
                0,
                None,
            ),
        );
        let mut new = version(CANDIDATE);
        new.oracle
            .insert(roles::HOSTILE.to_owned(), oracle(3, &[], 0, None));
        new.oracle
            .insert(roles::DEEP.to_owned(), oracle(3, &[], 0, None));
        let mut missing = version("v1.0.0");
        missing.oracle.insert(
            roles::HOSTILE.to_owned(),
            Placed {
                checked: Checked {
                    observation: Some(OracleObservation {
                        ended: Ended::Exited(1),
                        timed_out: false,
                        wall: Duration::from_secs(1),
                        residue: Vec::new(),
                        report: ReportRead::Missing("it wrote no report".to_owned()),
                        available_bytes: None,
                    }),
                    record: Recorded::new("headless-oracle", None, None),
                },
                pointer: "#/checks/1".to_owned(),
                evidence: None,
            },
        );
        let versions = [missing, old, new];

        let rows = classify_all(&versions, &[], SweepTier::Quick, "linux");

        assert_eq!(state(&rows, "F10", "v1.3.0"), CellState::Affected);
        assert_eq!(state(&rows, "F10", CANDIDATE), CellState::NotAffected);
        assert_eq!(state(&rows, "F10", "v1.0.0"), CellState::NotMeasurable);
        assert_eq!(state(&rows, "F11", "v1.3.0"), CellState::Affected);
        assert_eq!(state(&rows, "F11", CANDIDATE), CellState::NotAffected);
        let not_run = cell(&rows, "F11", "v1.0.0");
        assert_eq!(not_run.state, CellState::NotMeasurable);
        assert!(
            not_run
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("was not run")),
            "{:?}",
            not_run.reason
        );
    }

    #[test]
    fn the_scan_store_quota_is_held_to_the_free_space_of_its_volume() {
        const GIB: u64 = 1 << 30;
        let mut v1 = version("v1.2.4");
        v1.oracle.insert(
            roles::TIMING.to_owned(),
            oracle(1, &[], 512 << 20, Some(100 * GIB)),
        );
        let mut too_large = version("v1.3.0");
        too_large.oracle.insert(
            roles::TIMING.to_owned(),
            oracle(3, &[], 75 * 256 * GIB, Some(100 * GIB)),
        );
        let mut right = version(CANDIDATE);
        right.oracle.insert(
            roles::TIMING.to_owned(),
            oracle(3, &[], 75 * GIB, Some(100 * GIB)),
        );
        let mut blind = version("v1.1.0");
        blind
            .oracle
            .insert(roles::TIMING.to_owned(), oracle(3, &[], 75 * GIB, None));
        let versions = [v1, blind, too_large, right];

        let rows = classify_all(&versions, &[], SweepTier::Quick, "macos");

        assert_eq!(state(&rows, "F7", "v1.2.4"), CellState::NotAffected);
        assert!(
            cell(&rows, "F7", "v1.2.4")
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no scan store"))
        );
        assert_eq!(state(&rows, "F7", "v1.1.0"), CellState::NotMeasurable);
        assert_eq!(state(&rows, "F7", "v1.3.0"), CellState::Affected);
        assert_eq!(state(&rows, "F7", CANDIDATE), CellState::NotAffected);
        let value = cell(&rows, "F7", "v1.3.0").value.clone().expect("a value");
        assert!(
            value.contains("18.8 TiB") && value.contains("100.0 GiB") && value.contains("192x"),
            "{value}"
        );
    }

    #[test]
    fn a_timing_is_affected_only_when_it_is_confidently_more_than_a_fifth_slower_than_the_candidate()
     {
        let version_series = |reference: &str, ratio: SweepRatio, completed: bool| {
            series(reference, 4000.0, completed, Some(ratio))
        };
        let set = measured(
            0,
            "headless_wall_ms",
            None,
            None,
            vec![
                version_series("v1.3.0", ratio(14.0, 12.0, 15.0), true),
                version_series("v1.2.4", ratio(1.05, 0.9, 1.2), true),
                version_series("v1.2.3", ratio(0.9, 0.8, 1.0), true),
                version_series("v1.2.2", ratio(1.5, 0.9, 2.0), true),
                ran_out_of_time(
                    "v1.2.1",
                    5,
                    5,
                    0,
                    vec![("candidate", bounds(&[300.0; 5], &[], 0))],
                ),
                series(CANDIDATE, 400.0, true, Some(ratio(1.0, 1.0, 1.0))),
            ],
        );
        let versions = [
            version("v1.3.0"),
            version("v1.2.4"),
            version("v1.2.3"),
            version("v1.2.2"),
            version("v1.2.1"),
            version("v1.2.0"),
            version(CANDIDATE),
        ];

        let rows = classify_all(&versions, &[set], SweepTier::Quick, "linux");

        assert_eq!(state(&rows, "F1", "v1.3.0"), CellState::Affected);
        let value = cell(&rows, "F1", "v1.3.0").value.clone().expect("a value");
        assert!(value.contains("14x the candidate"), "{value}");
        assert!(value.contains("95% CI 12.0-15.0, 5 rounds"), "{value}");
        assert_eq!(
            state(&rows, "F1", "v1.2.4"),
            CellState::NotMeasurable,
            "above one, under 20%"
        );
        assert_eq!(state(&rows, "F1", "v1.2.3"), CellState::NotAffected);
        assert_eq!(
            state(&rows, "F1", "v1.2.2"),
            CellState::NotMeasurable,
            "an interval that includes one"
        );
        assert_eq!(
            state(&rows, "F1", "v1.2.1"),
            CellState::Affected,
            "it never finished"
        );
        assert!(
            cell(&rows, "F1", "v1.2.1")
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("did not finish"))
        );
        assert_eq!(state(&rows, "F1", "v1.2.0"), CellState::NotMeasurable);
        assert_eq!(state(&rows, "F1", CANDIDATE), CellState::NotAffected);
        assert!(
            row(&rows, "F1").note.is_none(),
            "the candidate is the reference of a timing row, never affected by definition"
        );
    }

    #[test]
    fn a_ratio_of_fewer_than_three_rounds_settles_nothing() {
        let thin = |reference: &str, pairs: u32| SweepSeries {
            ratios: BTreeMap::from([(
                "candidate".to_owned(),
                SweepRatio {
                    pairs,
                    ..ratio(14.0, 12.0, 15.0)
                },
            )]),
            ..series(reference, 4000.0, true, None)
        };
        let set = measured(
            0,
            "headless_wall_ms",
            None,
            None,
            vec![
                thin("v1.3.0", 1),
                thin("v1.2.4", 2),
                thin("v1.2.3", 3),
                series(CANDIDATE, 400.0, true, Some(ratio(1.0, 1.0, 1.0))),
            ],
        );
        let versions = [
            version("v1.3.0"),
            version("v1.2.4"),
            version("v1.2.3"),
            version(CANDIDATE),
        ];

        let rows = classify_all(&versions, &[set], SweepTier::Quick, "linux");

        assert_eq!(
            state(&rows, "F1", "v1.3.0"),
            CellState::NotMeasurable,
            "one round"
        );
        assert_eq!(
            state(&rows, "F1", "v1.2.4"),
            CellState::NotMeasurable,
            "two rounds"
        );
        assert_eq!(
            state(&rows, "F1", "v1.2.3"),
            CellState::Affected,
            "three rounds"
        );
    }

    #[test]
    fn a_timing_row_that_was_not_timed_says_which_timing_is_missing() {
        let versions = [version("v1.3.0"), version(CANDIDATE)];

        let rows = classify_all(&versions, &[], SweepTier::Quick, "linux");

        for defect in ["F1", "F2", "F16", "F18"] {
            let found = cell(&rows, defect, "v1.3.0");
            assert_eq!(found.state, CellState::NotMeasurable, "{defect}");
            assert!(
                found
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("not timed")),
                "{defect}: {:?}",
                found.reason
            );
        }
    }

    #[test]
    fn a_slow_terminal_is_a_defect_in_either_motion_mode_and_the_motion_ratio_is_reported() {
        let drain = Some(150_000);
        let default = measured(
            0,
            "tui_complete_ms",
            Some(Profile::Default),
            drain,
            vec![
                SweepSeries {
                    ratios: BTreeMap::from([
                        ("candidate".to_owned(), ratio(9.0, 8.0, 10.0)),
                        ("reduced-motion".to_owned(), ratio(3.0, 2.5, 3.5)),
                    ]),
                    ..series("v1.3.0", 30_000.0, true, None)
                },
                series(CANDIDATE, 3_000.0, true, Some(ratio(1.0, 1.0, 1.0))),
            ],
        );
        let reduced = measured(
            1,
            "tui_complete_ms",
            Some(Profile::ReducedMotion),
            drain,
            vec![
                series("v1.3.0", 10_000.0, true, Some(ratio(3.0, 2.8, 3.3))),
                series(CANDIDATE, 3_300.0, true, Some(ratio(1.0, 1.0, 1.0))),
            ],
        );
        let versions = [version("v1.3.0"), version(CANDIDATE)];

        let rows = classify_all(&versions, &[default, reduced], SweepTier::Quick, "linux");

        let affected = cell(&rows, "F2", "v1.3.0");
        assert_eq!(affected.state, CellState::Affected);
        let value = affected.value.as_deref().expect("a value");
        assert!(value.contains("default motion"), "{value}");
        assert!(value.contains("3.0x reduced motion"), "{value}");
        assert_eq!(state(&rows, "F2", CANDIDATE), CellState::NotAffected);
        assert_eq!(affected.evidence, ["#/measurements/0", "#/measurements/1"]);
    }

    #[test]
    fn the_interface_is_held_to_the_budget_against_the_headless_scan_of_its_own_version() {
        let with_headless = |reference: &str, median: f64, lower: f64, upper: f64| SweepSeries {
            ratios: BTreeMap::from([("headless-scan".to_owned(), ratio(median, lower, upper))]),
            ..series(reference, 1000.0, true, None)
        };
        let set = measured(
            0,
            "tui_complete_ms",
            Some(Profile::Default),
            None,
            vec![
                with_headless("v1.3.0", 2.0, 1.6, 2.4),
                with_headless("v1.2.4", 1.3, 1.1, 1.6),
                with_headless("v1.2.3", 1.1, 1.0, 1.2),
                with_headless(CANDIDATE, 1.1, 1.0, 1.2),
            ],
        );
        let versions = [
            version("v1.3.0"),
            version("v1.2.4"),
            version("v1.2.3"),
            version(CANDIDATE),
        ];

        let rows = classify_all(&versions, &[set], SweepTier::Quick, "linux");

        assert_eq!(state(&rows, "F18", "v1.3.0"), CellState::Affected);
        assert_eq!(
            state(&rows, "F18", "v1.2.4"),
            CellState::NotMeasurable,
            "its interval reaches the budget"
        );
        assert_eq!(state(&rows, "F18", "v1.2.3"), CellState::NotAffected);
        assert_eq!(state(&rows, "F18", CANDIDATE), CellState::NotAffected);
    }

    /// A timing in which `v1.2.1` and the candidate ran out of time in all five rounds, held to
    /// what `against` names.
    fn neither_finished(
        index: usize,
        metric: &str,
        profile: Option<Profile>,
        drain: Option<u64>,
        against: &str,
    ) -> MeasuredSet {
        let series = |reference: &str| {
            ran_out_of_time(reference, 5, 5, 0, vec![(against, bounds(&[], &[], 5))])
        };
        measured(
            index,
            metric,
            profile,
            drain,
            vec![series("v1.2.1"), series(CANDIDATE)],
        )
    }

    #[test]
    fn a_version_that_ran_out_of_time_where_the_candidate_finished_is_affected_and_says_how_many() {
        let one_slow_round = SweepRatio {
            pairs: 4,
            ..and_bounds(ratio(0.9, 0.8, 1.0), &[300.0], &[])
        };
        let set = measured(
            0,
            "headless_wall_ms",
            None,
            None,
            vec![
                // Two runs in a row ran out of time, and the other three rounds were skipped.
                ran_out_of_time(
                    "v1.2.1",
                    2,
                    2,
                    3,
                    vec![("candidate", bounds(&[300.0, 300.0], &[], 0))],
                ),
                // One run ran out of time among four that finished no slower than the candidate.
                ran_out_of_time("v1.2.2", 5, 1, 0, vec![("candidate", one_slow_round)]),
                series(CANDIDATE, 400.0, true, Some(ratio(1.0, 1.0, 1.0))),
            ],
        );
        let versions = [version("v1.2.1"), version("v1.2.2"), version(CANDIDATE)];

        let rows = classify_all(&versions, &[set], SweepTier::Quick, "linux");

        let gave_up = cell(&rows, "F1", "v1.2.1");
        assert_eq!(gave_up.state, CellState::Affected);
        assert_has(
            gave_up.reason.as_deref().unwrap_or_default(),
            &[
                "2 of its 2 measured runs did not finish within the bound",
                "2 of them in rounds where the candidate's run did",
                "the candidate finished 5 of its 5 runs",
                "3 rounds skipped",
            ],
        );
        assert_has(
            gave_up.value.as_deref().unwrap_or_default(),
            &["no round finished on both sides"],
        );
        let once = cell(&rows, "F1", "v1.2.2");
        assert_eq!(
            once.state,
            CellState::NotMeasurable,
            "one slow round among four that finished"
        );
        assert_has(
            once.reason.as_deref().unwrap_or_default(),
            &["did not finish in 1 round"],
        );
        assert_eq!(state(&rows, "F1", CANDIDATE), CellState::NotAffected);
    }

    #[test]
    fn runs_that_ran_out_of_time_on_both_sides_are_no_ratio_and_say_nothing() {
        // The bound of each side once read as a duration, so a ratio of about 1.0 was not-affected.
        let default = Some(Profile::Default);
        let reduced = Some(Profile::ReducedMotion);
        let drain = Some(150_000);
        let timings = [
            neither_finished(0, "headless_wall_ms", None, None, "candidate"),
            neither_finished(1, "report_write_ms", None, None, "candidate"),
            neither_finished(2, "tui_complete_ms", default, drain, "candidate"),
            neither_finished(3, "tui_complete_ms", reduced, drain, "candidate"),
            neither_finished(4, "tui_complete_ms", default, None, "headless-scan"),
            neither_finished(5, "headless_scan_ms", None, None, "candidate"),
        ];
        let versions = [version("v1.2.1"), version(CANDIDATE)];

        let rows = classify_all(&versions, &timings, SweepTier::Quick, "linux");

        for defect in ["F1", "F16", "F2", "F18"] {
            let found = cell(&rows, defect, "v1.2.1");
            assert_eq!(found.state, CellState::NotMeasurable, "{defect}");
            assert_has(
                found.reason.as_deref().unwrap_or_default(),
                &["neither side finished"],
            );
        }
    }

    /// What the rounds of `ratio` say of a version held to the candidate: slower above 1.2x, and
    /// not slower at 1.0x or less.
    fn judged(ratio: &SweepRatio) -> Call {
        judge(ratio, slowness(ratio), 1.2, 1.0)
    }

    /// Asserts what each of `cases` is judged to be.
    fn assert_judged(cases: Vec<(&str, SweepRatio, Call)>) {
        for (what, rounds, expected) in cases {
            assert_eq!(judged(&rounds), expected, "{what}");
        }
    }

    #[test]
    fn the_rounds_in_which_both_finished_decide_a_ratio_when_nothing_argues_against_them() {
        let slower = Call::Slower { by_bounds: false };
        let not_slower = Call::NotSlower { by_bounds: false };
        assert_judged(vec![
            ("no slower", ratio(0.9, 0.8, 1.0), not_slower),
            ("slower", ratio(14.0, 12.0, 15.0), slower),
            (
                "neither, with enough rounds",
                ratio(1.1, 0.9, 1.4),
                Call::Unsure(Doubt::Undecided),
            ),
            (
                "two rounds",
                SweepRatio {
                    pairs: 2,
                    ..ratio(14.0, 12.0, 15.0)
                },
                Call::Unsure(Doubt::Thin),
            ),
            // A numerator that did not finish only argues for slower, a denominator for not slower.
            (
                "slower, and a lower bound",
                and_bounds(ratio(14.0, 12.0, 15.0), &[300.0], &[]),
                slower,
            ),
            (
                "no slower, and an upper bound",
                and_bounds(ratio(0.9, 0.8, 1.0), &[], &[0.04]),
                not_slower,
            ),
            (
                "neither finished",
                bounds(&[], &[], 5),
                Call::Unsure(Doubt::Thin),
            ),
        ]);
    }

    #[test]
    fn a_numerator_that_did_not_finish_shows_a_ratio_high_and_never_low() {
        assert_judged(vec![
            (
                "two lower bounds over 20%",
                bounds(&[300.0, 300.0], &[], 0),
                Call::Slower { by_bounds: true },
            ),
            (
                "one lower bound",
                bounds(&[300.0], &[], 0),
                Call::Unsure(Doubt::Thin),
            ),
            (
                "two lower bounds under 20%",
                bounds(&[1.1, 1.1], &[], 0),
                Call::Unsure(Doubt::Thin),
            ),
            (
                "no slower, and a lower bound",
                and_bounds(ratio(0.9, 0.8, 1.0), &[300.0], &[]),
                Call::Unsure(Doubt::Raised),
            ),
            (
                "no slower, and a lower bound under 20%",
                and_bounds(ratio(0.9, 0.8, 1.0), &[1.1], &[]),
                Call::Unsure(Doubt::Raised),
            ),
        ]);
    }

    #[test]
    fn a_denominator_that_did_not_finish_shows_a_ratio_low_and_never_high() {
        assert_judged(vec![
            (
                "two upper bounds",
                bounds(&[], &[0.04, 0.05], 0),
                Call::NotSlower { by_bounds: true },
            ),
            (
                "one upper bound",
                bounds(&[], &[0.04], 0),
                Call::Unsure(Doubt::Thin),
            ),
            (
                "slower, and an upper bound",
                and_bounds(ratio(14.0, 12.0, 15.0), &[], &[0.04]),
                Call::Unsure(Doubt::Lowered),
            ),
        ]);
    }

    #[test]
    fn rounds_that_argue_both_ways_settle_nothing() {
        assert_judged(vec![
            (
                "bounds both ways",
                bounds(&[300.0, 300.0], &[0.04, 0.05], 0),
                Call::Unsure(Doubt::Conflict),
            ),
            (
                "no slower, and two lower bounds",
                and_bounds(ratio(0.9, 0.8, 1.0), &[300.0, 300.0], &[]),
                Call::Unsure(Doubt::Conflict),
            ),
            (
                "slower, and two upper bounds",
                and_bounds(ratio(14.0, 12.0, 15.0), &[], &[0.04, 0.05]),
                Call::Unsure(Doubt::Conflict),
            ),
        ]);
    }

    #[test]
    fn a_version_that_finished_where_the_candidate_did_not_is_not_slower_in_two_such_rounds() {
        let set = measured(
            0,
            "headless_wall_ms",
            None,
            None,
            vec![
                series("v1.2.3", 4_000.0, true, Some(bounds(&[], &[0.04, 0.05], 0))),
                series("v1.2.4", 4_000.0, true, Some(bounds(&[], &[0.04], 0))),
                series(
                    "v1.2.5",
                    4_000.0,
                    true,
                    Some(and_bounds(ratio(14.0, 12.0, 15.0), &[], &[0.04])),
                ),
                ran_out_of_time(CANDIDATE, 5, 2, 0, Vec::new()),
            ],
        );
        let versions = [
            version("v1.2.3"),
            version("v1.2.4"),
            version("v1.2.5"),
            version(CANDIDATE),
        ];

        let rows = classify_all(&versions, &[set], SweepTier::Quick, "linux");

        let faster = cell(&rows, "F1", "v1.2.3");
        assert_eq!(faster.state, CellState::NotAffected);
        assert_has(
            faster.reason.as_deref().unwrap_or_default(),
            &[
                "its run finished in 2 rounds where the candidate's run did not finish within the bound",
                "the candidate did not finish 2 of its 5 runs (0 rounds skipped)",
            ],
        );
        assert_eq!(
            state(&rows, "F1", "v1.2.4"),
            CellState::NotMeasurable,
            "one round is a slow moment"
        );
        let mixed = cell(&rows, "F1", "v1.2.5");
        assert_eq!(
            mixed.state,
            CellState::NotMeasurable,
            "a round in which the candidate did not finish cannot support a slower median"
        );
        assert_has(
            mixed.reason.as_deref().unwrap_or_default(),
            &["left out of the median"],
        );
        let reference = cell(&rows, "F1", CANDIDATE);
        assert_eq!(reference.state, CellState::NotAffected);
        assert_has(
            reference.value.as_deref().unwrap_or_default(),
            &["2 of its 5 measured runs did not finish within the bound"],
        );
    }

    #[test]
    fn rounds_in_which_a_run_did_not_finish_are_not_among_the_three_a_ratio_needs() {
        // Five rounds ran, but only two finished on both sides, and the others show nothing over 20%.
        let two_of_five = SweepRatio {
            pairs: 2,
            ..and_bounds(ratio(14.0, 12.0, 15.0), &[1.1, 1.1, 1.1], &[])
        };
        let set = measured(
            0,
            "headless_wall_ms",
            None,
            None,
            vec![
                ran_out_of_time("v1.3.0", 5, 3, 0, vec![("candidate", two_of_five)]),
                series(CANDIDATE, 400.0, true, Some(ratio(1.0, 1.0, 1.0))),
            ],
        );
        let versions = [version("v1.3.0"), version(CANDIDATE)];

        let rows = classify_all(&versions, &[set], SweepTier::Quick, "linux");

        let found = cell(&rows, "F1", "v1.3.0");
        assert_eq!(found.state, CellState::NotMeasurable);
        assert_has(
            found.reason.as_deref().unwrap_or_default(),
            &["2 rounds finished on both sides, which is too few"],
        );
    }

    #[test]
    fn the_interface_budget_is_settled_by_rounds_that_finished_and_by_bounds_that_prove_it() {
        let interface = |reference: &str, ran, unfinished, skipped, to_headless: SweepRatio| {
            ran_out_of_time(
                reference,
                ran,
                unfinished,
                skipped,
                vec![("headless-scan", to_headless)],
            )
        };
        let two_rounds = SweepSeries {
            ratios: BTreeMap::from([(
                "headless-scan".to_owned(),
                SweepRatio {
                    pairs: 2,
                    ..ratio(2.0, 1.6, 2.4)
                },
            )]),
            ..series("v1.2.1", 1000.0, true, None)
        };
        let tui = measured(
            0,
            "tui_complete_ms",
            Some(Profile::Default),
            None,
            vec![
                // Neither the interface nor the headless scan finished in any round.
                interface("v1.0.0", 5, 5, 0, bounds(&[], &[], 5)),
                // The interface ran out of time twice where the headless scan finished in 40 s:
                // at least 3x, over the budget.
                interface("v1.1.0", 2, 2, 3, bounds(&[3.0, 3.5], &[], 0)),
                // The same, but the bound proves no more than 1.2x, which is within the budget.
                interface("v1.2.0", 2, 2, 3, bounds(&[1.2, 1.2], &[], 0)),
                // Two rounds finished on both sides, which is too few.
                two_rounds,
                // The headless scan ran out of time where the interface finished.
                interface("v1.2.2", 5, 0, 0, bounds(&[], &[0.5, 0.6], 0)),
                interface(CANDIDATE, 5, 0, 0, ratio(1.1, 1.0, 1.2)),
            ],
        );
        let headless = measured(
            1,
            "headless_scan_ms",
            None,
            None,
            vec![series("v1.1.0", 40_000.0, true, None)],
        );
        let versions = [
            version("v1.0.0"),
            version("v1.1.0"),
            version("v1.2.0"),
            version("v1.2.1"),
            version("v1.2.2"),
            version(CANDIDATE),
        ];

        let rows = classify_all(&versions, &[tui, headless], SweepTier::Quick, "linux");

        let neither = cell(&rows, "F18", "v1.0.0");
        assert_eq!(neither.state, CellState::NotMeasurable);
        assert_has(
            neither.reason.as_deref().unwrap_or_default(),
            &["neither side finished"],
        );
        let over = cell(&rows, "F18", "v1.1.0");
        assert_eq!(over.state, CellState::Affected);
        assert_has(
            over.reason.as_deref().unwrap_or_default(),
            &[
                "2 of its 2 measured runs did not finish within the bound",
                "the headless scan finished 5 of its 5 runs",
                "3 rounds skipped",
            ],
        );
        assert_eq!(over.evidence, ["#/measurements/0", "#/measurements/1"]);
        assert_eq!(
            state(&rows, "F18", "v1.2.0"),
            CellState::NotMeasurable,
            "a bound that shows no more than 1.2x does not show the budget exceeded"
        );
        let thin = cell(&rows, "F18", "v1.2.1");
        assert_eq!(thin.state, CellState::NotMeasurable);
        assert_has(
            thin.reason.as_deref().unwrap_or_default(),
            &["2 rounds finished on both sides, which is too few"],
        );
        let within = cell(&rows, "F18", "v1.2.2");
        assert_eq!(within.state, CellState::NotAffected);
        assert_has(
            within.reason.as_deref().unwrap_or_default(),
            &["where the headless scan's run did not finish within the bound"],
        );
        assert_eq!(state(&rows, "F18", CANDIDATE), CellState::NotAffected);
    }

    #[test]
    fn a_slow_terminal_that_a_motion_mode_never_finished_on_is_affected_and_says_so() {
        let drain = Some(150_000);
        let default = measured(
            0,
            "tui_complete_ms",
            Some(Profile::Default),
            drain,
            vec![
                ran_out_of_time(
                    "v1.3.0",
                    2,
                    2,
                    3,
                    vec![
                        ("candidate", bounds(&[40.0, 40.0], &[], 0)),
                        ("reduced-motion", bounds(&[12.0, 12.0], &[], 0)),
                    ],
                ),
                series(CANDIDATE, 3_000.0, true, Some(ratio(1.0, 1.0, 1.0))),
            ],
        );
        let reduced = measured(
            1,
            "tui_complete_ms",
            Some(Profile::ReducedMotion),
            drain,
            vec![
                series("v1.3.0", 10_000.0, true, Some(ratio(3.0, 2.8, 3.3))),
                series(CANDIDATE, 3_300.0, true, Some(ratio(1.0, 1.0, 1.0))),
            ],
        );
        let versions = [version("v1.3.0"), version(CANDIDATE)];

        let rows = classify_all(&versions, &[default, reduced], SweepTier::Quick, "linux");

        let found = cell(&rows, "F2", "v1.3.0");
        assert_eq!(found.state, CellState::Affected);
        assert_has(
            found.reason.as_deref().unwrap_or_default(),
            &[
                "default motion: 2 of its 2 measured runs did not finish within the bound",
                "3 rounds skipped",
            ],
        );
        assert_has(
            found.value.as_deref().unwrap_or_default(),
            &[
                "no round finished on both sides, so there is no ratio to reduced motion",
                "in 2 rounds its run did not finish while reduced motion did (at least 12x there)",
            ],
        );
        assert_eq!(state(&rows, "F2", CANDIDATE), CellState::NotAffected);
    }

    #[test]
    fn a_ratio_describes_the_rounds_in_which_a_run_did_not_finish_beside_its_median() {
        assert_eq!(
            describe_ratio(&ratio(14.0, 12.0, 15.0), "the candidate"),
            "14x the candidate (95% CI 12.0-15.0, 5 rounds)"
        );
        let with_bounds = SweepRatio {
            lower_bounds: vec![300.0, 285.0],
            upper_bounds: vec![0.5],
            both_censored: 1,
            ..ratio(14.0, 12.0, 15.0)
        };

        assert_eq!(
            describe_ratio(&with_bounds, "the candidate"),
            "14x the candidate (95% CI 12.0-15.0, 5 rounds); in 2 rounds its run did not finish \
             while the candidate did (at least 285x there); in 1 round the candidate did not \
             finish while its run did (at most 0.5x there); in 1 round neither finished"
        );
        assert_eq!(
            describe_ratio(&bounds(&[], &[], 5), "`du -sk`"),
            "no round finished on both sides, so there is no ratio to `du -sk`; in 5 rounds \
             neither finished"
        );
    }

    /// Asserts that `text` holds every one of `fragments`.
    fn assert_mentions(text: Option<&str>, fragments: &[&str]) {
        let text = text.unwrap_or_default();
        for fragment in fragments {
            assert!(
                text.contains(fragment),
                "{fragment:?} is missing from the text: {text}"
            );
        }
    }

    /// A filter variant after which the program went on.
    fn survived(place: &str) -> FilterAttempt {
        FilterAttempt {
            place: place.to_owned(),
            outcome: FilterOutcome::Survived { error_shown: false },
        }
    }

    /// A filter variant that ended the program with exit code `code`.
    fn crashed(place: &str, code: i32) -> FilterAttempt {
        FilterAttempt {
            place: place.to_owned(),
            outcome: FilterOutcome::Ended(Exit {
                code: Some(code),
                signal: None,
            }),
        }
    }

    /// A filter variant that could not be carried out.
    fn not_run(place: &str, why: &str) -> FilterAttempt {
        FilterAttempt {
            place: place.to_owned(),
            outcome: FilterOutcome::NotRun(why.to_owned()),
        }
    }

    /// The table of a sweep whose filter check made these two variants on `v1.3.0`.
    fn filter_rows(at_root: FilterAttempt, in_folder: FilterAttempt) -> Vec<SweepRow> {
        let mut old = version("v1.3.0");
        old.filter = Some(placed(Some(FilterObservation {
            selected_at_complete: Inspector::NotShown,
            largest: None,
            at_root,
            in_folder,
        })));
        classify_all(&[old, version(CANDIDATE)], &[], SweepTier::Quick, "linux")
    }

    #[test]
    fn a_filter_that_ends_the_program_is_the_defect_and_the_cursor_is_held_to_the_largest_entry() {
        let attempt = |place: &str, exit: Option<i32>| match exit {
            Some(code) => crashed(place, code),
            None => survived(place),
        };
        let item = |name: &str| {
            Inspector::Item(SelectedItem {
                name: name.to_owned(),
                state: "COMPLETE".to_owned(),
                kind: "folder".to_owned(),
            })
        };
        let observation = |selected: Inspector, root: Option<i32>, folder: Option<i32>| {
            placed(Some(FilterObservation {
                selected_at_complete: selected,
                largest: Some("victim".to_owned()),
                at_root: attempt("at the root", root),
                in_folder: attempt("inside victim", folder),
            }))
        };
        let mut old = version("v1.3.0");
        old.filter = Some(observation(item("keep-a.bin"), Some(101), Some(101)));
        let mut new = version(CANDIDATE);
        new.filter = Some(observation(item("victim"), None, None));
        let mut blind = version("v1.0.0");
        blind.filter = Some(observation(Inspector::NotShown, None, Some(101)));
        let mut none_selected = version("v1.1.0");
        none_selected.filter = Some(observation(Inspector::NothingSelected, None, None));
        let versions = [blind, none_selected, old, new];

        let rows = classify_all(&versions, &[], SweepTier::Quick, "linux");

        assert_eq!(state(&rows, "F22", "v1.3.0"), CellState::Affected);
        assert!(
            cell(&rows, "F22", "v1.3.0")
                .value
                .as_deref()
                .is_some_and(|value| value.contains("exit code 101"))
        );
        assert_eq!(state(&rows, "F22", CANDIDATE), CellState::NotAffected);
        assert_eq!(
            state(&rows, "F22", "v1.0.0"),
            CellState::Affected,
            "one crash is enough"
        );
        assert_eq!(state(&rows, "F23b", "v1.3.0"), CellState::Affected);
        assert_eq!(state(&rows, "F23b", "v1.1.0"), CellState::Affected);
        assert_eq!(
            state(&rows, "F23b", CANDIDATE),
            CellState::NotMeasurable,
            "a cursor on the largest entry shows nothing where the listing order is not known"
        );
        assert_eq!(state(&rows, "F23b", "v1.0.0"), CellState::NotMeasurable);
    }

    #[test]
    fn a_crash_inside_the_folder_is_the_defect_although_the_root_filter_did_not_run() {
        let rows = filter_rows(
            not_run("at the root", "the header did not read COMPLETE"),
            crashed("inside victim", 101),
        );

        let found = cell(&rows, "F22", "v1.3.0");

        assert_eq!(found.state, CellState::Affected, "{found:?}");
        assert_mentions(
            found.value.as_deref(),
            &[
                "at the root: did not run: the header did not read COMPLETE",
                "inside victim: the program ended: exit code 101",
            ],
        );
    }

    #[test]
    fn a_crash_at_the_root_is_the_defect_although_the_folder_filter_did_not_run() {
        let rows = filter_rows(
            crashed("at the root", 101),
            not_run("inside victim", "Enter did not open `victim`"),
        );

        let found = cell(&rows, "F22", "v1.3.0");

        assert_eq!(found.state, CellState::Affected, "{found:?}");
        assert_mentions(
            found.value.as_deref(),
            &[
                "at the root: the program ended: exit code 101",
                "inside victim: did not run: Enter did not open `victim`",
            ],
        );
    }

    #[test]
    fn a_program_that_survived_both_filters_is_not_affected() {
        let rows = filter_rows(survived("at the root"), survived("inside victim"));

        let found = cell(&rows, "F22", "v1.3.0");

        assert_eq!(found.state, CellState::NotAffected, "{found:?}");
        assert_mentions(
            found.value.as_deref(),
            &[
                "at the root: the program went on",
                "inside victim: the program went on",
            ],
        );
    }

    #[test]
    fn a_survival_of_only_one_filter_does_not_show_the_version_unaffected() {
        let folder_missing = filter_rows(
            survived("at the root"),
            not_run("inside victim", "Enter did not open `victim`"),
        );
        let root_missing = filter_rows(
            not_run("at the root", "the header did not read COMPLETE"),
            survived("inside victim"),
        );

        let found = cell(&folder_missing, "F22", "v1.3.0");
        assert_eq!(found.state, CellState::NotMeasurable, "{found:?}");
        assert_mentions(
            found.reason.as_deref(),
            &[
                "only one of the two filters ran",
                "the filter inside victim did not run (Enter did not open `victim`)",
                "does not show the version unaffected",
            ],
        );

        let found = cell(&root_missing, "F22", "v1.3.0");
        assert_eq!(found.state, CellState::NotMeasurable, "{found:?}");
        assert_mentions(
            found.reason.as_deref(),
            &[
                "only one of the two filters ran",
                "the filter at the root did not run (the header did not read COMPLETE)",
            ],
        );
    }

    #[test]
    fn neither_filter_having_run_is_not_measurable_and_says_why_for_each() {
        let rows = filter_rows(
            not_run("at the root", "the header did not read COMPLETE"),
            not_run("inside victim", "Enter did not open `victim`"),
        );

        let found = cell(&rows, "F22", "v1.3.0");

        assert_eq!(found.state, CellState::NotMeasurable, "{found:?}");
        assert_mentions(
            found.reason.as_deref(),
            &[
                "neither filter ran",
                "at the root: the header did not read COMPLETE",
                "inside victim: Enter did not open `victim`",
            ],
        );
    }

    /// An attempt that reached COMPLETE and read `after` where the cursor had been moved onto
    /// `before`.
    fn read_after_complete(before: &str, after: &str) -> DriftAttempt {
        DriftAttempt::Complete {
            before: Some(before.to_owned()),
            after: Some(after.to_owned()),
        }
    }

    /// The table of a full-tier sweep whose drift check made these attempts on `v1.3.0`.
    fn drift_rows(attempts: Vec<DriftAttempt>) -> Vec<SweepRow> {
        let mut old = version("v1.3.0");
        old.drift = Some(placed(Some(DriftObservation { attempts })));
        classify_all(&[old, version(CANDIDATE)], &[], SweepTier::Full, "macos")
    }

    #[test]
    fn the_selection_drift_needs_the_full_tier_and_a_reproduction() {
        let attempts = |drifted: bool| {
            let after = if drifted { "victim" } else { "big-file.bin" };
            DriftObservation {
                attempts: vec![
                    read_after_complete("big-file.bin", after),
                    DriftAttempt::NoPrecondition,
                ],
            }
        };
        let mut old = version("v1.3.0");
        old.drift = Some(placed(Some(attempts(true))));
        let mut new = version(CANDIDATE);
        new.drift = Some(placed(Some(attempts(false))));
        let versions = [old, new];

        let quick = classify_all(
            &[version("v1.3.0"), version(CANDIDATE)],
            &[],
            SweepTier::Quick,
            "macos",
        );
        let full = classify_all(&versions, &[], SweepTier::Full, "macos");

        let skipped = cell(&quick, "F5", "v1.3.0");
        assert_eq!(skipped.state, CellState::NotMeasurable);
        assert!(
            skipped
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("--full"))
        );
        assert_eq!(state(&full, "F5", "v1.3.0"), CellState::Affected);
        let not_reproduced = cell(&full, "F5", CANDIDATE);
        assert_eq!(
            not_reproduced.state,
            CellState::NotMeasurable,
            "a defect observed once is not shown absent by one run that did not see it"
        );
        assert!(
            not_reproduced
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("observed once"))
        );
    }

    #[test]
    fn an_attempt_that_never_reached_complete_is_not_a_reproduction() {
        let rows = drift_rows(vec![DriftAttempt::Incomplete, DriftAttempt::Incomplete]);

        let found = cell(&rows, "F5", "v1.3.0");

        assert_eq!(
            found.state,
            CellState::NotMeasurable,
            "nothing was read after COMPLETE, so there is no drift to report"
        );
        assert_mentions(
            found.reason.as_deref(),
            &[
                "not reproduced in 2 attempts",
                "the precondition held in 2",
                "after it in 0 and did not in 2",
                "observed once",
            ],
        );
    }

    #[test]
    fn a_real_drift_counts_although_another_attempt_never_reached_complete() {
        let rows = drift_rows(vec![
            DriftAttempt::Incomplete,
            read_after_complete("big-file.bin", "victim"),
        ]);

        let found = cell(&rows, "F5", "v1.3.0");

        assert_eq!(found.state, CellState::Affected, "{found:?}");
        assert_mentions(
            found.value.as_deref(),
            &[
                "1 of 2 attempts",
                "the precondition held in 2",
                "reached COMPLETE after it in 1 of those (1 did not)",
            ],
        );
    }

    #[test]
    fn a_selection_that_stayed_put_does_not_show_the_version_unaffected() {
        let rows = drift_rows(vec![
            read_after_complete("big-file.bin", "big-file.bin"),
            read_after_complete("big-file.bin", "big-file.bin"),
        ]);

        let found = cell(&rows, "F5", "v1.3.0");

        assert_eq!(found.state, CellState::NotMeasurable, "{found:?}");
        assert_mentions(
            found.reason.as_deref(),
            &[
                "not reproduced in 2 attempts",
                "after it in 2 and did not in 0",
                "does not show the version unaffected",
            ],
        );
    }

    #[test]
    fn descriptors_that_grow_with_the_tree_are_the_defect() {
        let observed = |small, large| placed(Some(DescriptorObservation { small, large }));
        let mut old = version("v1.3.0");
        old.descriptors = Some(observed(20, Some(120)));
        let mut new = version(CANDIDATE);
        new.descriptors = Some(observed(20, Some(36)));
        let mut timed_out = version("v1.2.0");
        timed_out.descriptors = Some(observed(20, None));
        let versions = [timed_out, old, new];

        let quick = classify_all(
            &[version("v1.3.0"), version(CANDIDATE)],
            &[],
            SweepTier::Quick,
            "linux",
        );
        let full = classify_all(&versions, &[], SweepTier::Full, "linux");

        assert_eq!(state(&quick, "F13", "v1.3.0"), CellState::NotMeasurable);
        assert!(
            cell(&quick, "F13", "v1.3.0")
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("full tier"))
        );
        assert_eq!(state(&full, "F13", "v1.3.0"), CellState::Affected);
        assert_eq!(state(&full, "F13", CANDIDATE), CellState::NotAffected);
        assert_eq!(state(&full, "F13", "v1.2.0"), CellState::NotMeasurable);
    }

    #[test]
    fn a_row_says_so_when_the_candidate_shows_the_defect_it_is_meant_to_have_fixed() {
        let mut old = version("v1.3.0");
        let mut new = version(CANDIDATE);
        for found in [&mut old, &mut new] {
            found
                .signals
                .insert("term".to_owned(), placed(Some(killed_by(Signal::Term, 15))));
        }
        let versions = [old, new];

        let rows = classify_all(&versions, &[], SweepTier::Quick, "linux");

        assert_eq!(state(&rows, "F6", CANDIDATE), CellState::Affected);
        assert!(
            row(&rows, "F6")
                .note
                .as_deref()
                .is_some_and(|note| note.contains("shows this defect too"))
        );
    }

    #[test]
    fn a_cursor_on_the_largest_entry_is_not_affected_on_windows_only_and_a_stuck_one_is_affected_anywhere()
     {
        let observed = |name: &str| {
            let mut found = version(CANDIDATE);
            found.filter = Some(placed(Some(FilterObservation {
                selected_at_complete: Inspector::Item(SelectedItem {
                    name: name.to_owned(),
                    state: "COMPLETE".to_owned(),
                    kind: "folder".to_owned(),
                }),
                largest: Some("victim".to_owned()),
                at_root: survived("at the root"),
                in_folder: survived("inside victim"),
            })));
            [found]
        };

        for os in ["macos", "linux"] {
            let rows = classify_all(&observed("victim"), &[], SweepTier::Quick, os);
            let found = cell(&rows, "F23b", CANDIDATE);
            assert_eq!(found.state, CellState::NotMeasurable, "{os}: {found:?}");
            let absent = format!("does not show the defect absent on {os}");
            assert_mentions(
                found.reason.as_deref(),
                &[
                    "shows `victim`, the largest entry",
                    absent.as_str(),
                    "on Windows only",
                ],
            );
        }
        let windows = classify_all(&observed("victim"), &[], SweepTier::Quick, "windows");
        assert_eq!(state(&windows, "F23b", CANDIDATE), CellState::NotAffected);
        for os in ["macos", "linux", "windows"] {
            let stuck = classify_all(&observed("keep-a.bin"), &[], SweepTier::Quick, os);
            assert_eq!(
                state(&stuck, "F23b", CANDIDATE),
                CellState::Affected,
                "{os}"
            );
        }
    }

    fn scan(wall_ms: u64, write_ms: Option<u64>) -> TimedScan {
        TimedScan {
            wall: Duration::from_millis(wall_ms),
            growth: write_ms.map_or(ReportGrowth::Unseen, |write| {
                ReportGrowth::Span(Duration::from_millis(write))
            }),
            completed: true,
        }
    }

    /// What the sweep records of a headless scan and an interface run to COMPLETE in each of five
    /// rounds, for each of `runs` (a version, what its scan did, and how long its interface took),
    /// made by the real round and series code: the scan's wall time, its time without the report,
    /// and the interface, held to the scan without its report (what F18 reads) and, to show what
    /// the rule used to read, to the wall time.
    fn unthrottled_phase(runs: &[(&str, TimedScan, f64)]) -> Vec<MeasuredSet> {
        const WALL: &str = "headless_wall_ms";
        const SCAN: &str = "headless_scan_ms";
        let names: Vec<String> = runs.iter().map(|(name, ..)| (*name).to_owned()).collect();
        let legs: [&[&str]; 2] = [&[WALL, "report_write_ms", SCAN], &["tui"]];
        let paired = paired_rounds(
            &names,
            &legs,
            5,
            Some(2),
            || None,
            |position, leg| {
                let (_, scan, interface) = &runs[position];
                Ok(if leg == 0 {
                    scan.readings()
                } else {
                    RunValue::from([("tui", Some(Reading::new(*interface, true)))])
                })
            },
        );
        let taken = |index: usize,
                     key: &str,
                     metric: &str,
                     profile: Option<Profile>,
                     also: &[(&str, Against)]| {
            let series = series_of(key, &names, &paired, 0, also);
            measured(index, metric, profile, None, series)
        };
        vec![
            taken(0, WALL, "headless_wall_ms", None, &[]),
            taken(1, SCAN, "headless_scan_ms", None, &[]),
            taken(
                2,
                "tui",
                "tui_complete_ms",
                Some(Profile::Default),
                &[
                    ("headless-scan", Against::Own(SCAN)),
                    ("headless-wall", Against::Own(WALL)),
                ],
            ),
            taken(3, "report_write_ms", "report_write_ms", None, &[]),
        ]
    }

    #[test]
    fn a_report_write_that_hides_the_interface_time_does_not_make_the_version_unaffected() {
        // The headless command takes 5.0 s, of which its report takes 1.5 s. The interface at
        // 4.9 s is under the headless wall time, which is what the rule used to be held to, and
        // 1.4 times the scan, which is over the 1.25 budget.
        let sets = unthrottled_phase(&[
            ("v1.3.0", scan(5000, Some(1500)), 4900.0),
            (CANDIDATE, scan(154, Some(7)), 162.0),
        ]);
        let ratios = &sets[2].measurement.series[0].ratios;
        let old = ratios["headless-wall"].median.expect("a median");
        let new = ratios["headless-scan"].median.expect("a median");
        assert!(old < 1.0, "held to the wall time the version passes: {old}");
        assert!(new > 1.25, "held to its scan it is over the budget: {new}");
        let versions = [version("v1.3.0"), version(CANDIDATE)];

        let rows = classify_all(&versions, &sets, SweepTier::Quick, "linux");

        let found = cell(&rows, "F18", "v1.3.0");
        assert_eq!(found.state, CellState::Affected, "{found:?}");
        assert_mentions(found.value.as_deref(), &["without its report write"]);
        assert_eq!(state(&rows, "F18", CANDIDATE), CellState::NotAffected);
    }

    #[test]
    fn a_version_whose_report_write_cannot_be_told_from_its_scan_is_not_measurable_for_f18() {
        // The report was never seen to grow. The interface at 3.0 s is far under the headless wall
        // time of 4.9 s, which the rule used to read as not affected.
        let sets = unthrottled_phase(&[
            ("v1.3.0", scan(4900, None), 3000.0),
            (CANDIDATE, scan(154, Some(7)), 162.0),
        ]);
        let versions = [version("v1.3.0"), version(CANDIDATE)];

        let rows = classify_all(&versions, &sets, SweepTier::Quick, "linux");

        let found = cell(&rows, "F18", "v1.3.0");
        assert_eq!(found.state, CellState::NotMeasurable, "{found:?}");
        assert_mentions(
            found.reason.as_deref(),
            &["could not be told apart from the scan in 5 of the 5 rounds"],
        );
        assert_eq!(
            state(&rows, "F16", "v1.3.0"),
            CellState::NotMeasurable,
            "a report that was not seen to grow is not a write of no time"
        );
    }

    #[test]
    fn a_report_write_of_a_few_milliseconds_leaves_f18_as_it_was() {
        let sets = unthrottled_phase(&[(CANDIDATE, scan(154, Some(7)), 162.0)]);
        let ratios = &sets[2].measurement.series[0].ratios;
        let old = ratios["headless-wall"].median.expect("a median");
        let new = ratios["headless-scan"].median.expect("a median");
        assert!(old <= 1.25 && new <= 1.25, "{old} {new}");
        assert!(
            (new - old).abs() < 0.06,
            "7 ms of 154 moves the ratio a little: {old} {new}"
        );

        let rows = classify_all(&[version(CANDIDATE)], &sets, SweepTier::Quick, "linux");

        assert_eq!(state(&rows, "F18", CANDIDATE), CellState::NotAffected);
    }
}
