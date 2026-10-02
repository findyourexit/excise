//! Owner-loop probe for the internal benchmark harness.
//!
//! This module exists only with the `internal` feature. A probe records how the
//! production owner loop spends wall time, per phase, in fixed-size histograms,
//! so a run of any length keeps constant memory. The loop finds its probe through
//! [`InputSource::owner_loop_probe`](crate::input::InputSource::owner_loop_probe):
//! an input source that returns one asks the loop to record itself, and every
//! other source, along with every build without the feature, leaves the loop
//! unchanged. Timings use real [`Instant`]s, never the loop's logical clock.
//!
//! Phases nest. Admission runs inside the `scan_batch` and `scan_unscanned`
//! worker events, so each phase reports its own inclusive duration. Admission
//! is the owner loop's handoff of a sealed batch to the store thread, which
//! does the admitting; publication is the owner loop's part of a scan's
//! publication, swapping in the generation the store thread built. The store
//! thread's own work is not an owner-loop phase.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ratatui::backend::Backend;

use super::OwnerLoop;
use super::worker::WorkerEvent;

/// Buckets per histogram: bucket `i` holds `[2^i, 2^(i+1))` nanoseconds, and
/// bucket 0 also holds zero. Sixty-four buckets span every `u64` nanosecond count.
pub const HISTOGRAM_BUCKETS: usize = 64;

/// Fixed-size log-scale distribution of wall-clock durations.
///
/// A histogram never allocates, so its memory is the same after one sample as
/// after a billion. Counts and sums saturate instead of wrapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhaseHistogram {
    count: u64,
    sum_nanos: u64,
    max_nanos: u64,
    buckets: [u64; HISTOGRAM_BUCKETS],
}

impl Default for PhaseHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl PhaseHistogram {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            count: 0,
            sum_nanos: 0,
            max_nanos: 0,
            buckets: [0; HISTOGRAM_BUCKETS],
        }
    }

    /// Adds one sample. A duration beyond `u64::MAX` nanoseconds saturates.
    pub fn record(&mut self, elapsed: Duration) {
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        self.count = self.count.saturating_add(1);
        self.sum_nanos = self.sum_nanos.saturating_add(nanos);
        self.max_nanos = self.max_nanos.max(nanos);
        let bucket = &mut self.buckets[Self::bucket_index(nanos)];
        *bucket = bucket.saturating_add(1);
    }

    /// Returns the bucket that holds `nanos`.
    #[must_use]
    pub fn bucket_index(nanos: u64) -> usize {
        // `ilog2` floors the base-two logarithm; `max(1)` folds zero into bucket 0.
        usize::try_from(nanos.max(1).ilog2()).unwrap_or(HISTOGRAM_BUCKETS - 1)
    }

    /// Returns the largest duration `bucket` can hold. An index past the last
    /// bucket reports the last bucket's bound.
    #[must_use]
    pub fn bucket_upper_bound(bucket: usize) -> Duration {
        let nanos = u32::try_from(bucket.saturating_add(1))
            .ok()
            .and_then(|bits| 1_u64.checked_shl(bits))
            .map_or(u64::MAX, |exclusive| exclusive - 1);
        Duration::from_nanos(nanos)
    }

    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// Returns the total of all samples, saturating at `u64::MAX` nanoseconds.
    #[must_use]
    pub const fn sum(&self) -> Duration {
        Duration::from_nanos(self.sum_nanos)
    }

    /// Returns the exact longest sample, or zero when empty.
    #[must_use]
    pub const fn max(&self) -> Duration {
        Duration::from_nanos(self.max_nanos)
    }

    #[must_use]
    pub fn mean(&self) -> Option<Duration> {
        self.sum_nanos
            .checked_div(self.count)
            .map(Duration::from_nanos)
    }

    /// Returns how many samples fell into each bucket.
    #[must_use]
    pub const fn buckets(&self) -> &[u64; HISTOGRAM_BUCKETS] {
        &self.buckets
    }

    /// Returns the bucket holding the nearest-rank sample at `per_mille` of the
    /// distribution (990 is the 99th percentile, 1000 the maximum), or `None`
    /// when no sample was recorded.
    #[must_use]
    pub fn quantile_bucket(&self, per_mille: u16) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let rank = (u128::from(self.count) * u128::from(per_mille.min(1_000)))
            .div_ceil(1_000)
            .max(1);
        let mut seen = 0_u128;
        for (index, in_bucket) in self.buckets.iter().enumerate() {
            seen += u128::from(*in_bucket);
            if seen >= rank {
                return Some(index);
            }
        }
        Some(HISTOGRAM_BUCKETS - 1)
    }

    /// Returns a bound the nearest-rank sample at `per_mille` never exceeds: its
    /// bucket's upper edge, tightened to the exact maximum.
    #[must_use]
    pub fn quantile_upper_bound(&self, per_mille: u16) -> Option<Duration> {
        self.quantile_bucket(per_mille)
            .map(|bucket| Self::bucket_upper_bound(bucket).min(self.max()))
    }

    /// Returns the bucket that holds the 99th-percentile sample.
    #[must_use]
    pub fn p99_bucket(&self) -> Option<usize> {
        self.quantile_bucket(990)
    }

    /// Returns a bound that the 99th-percentile sample never exceeds.
    #[must_use]
    pub fn p99_upper_bound(&self) -> Option<Duration> {
        self.quantile_upper_bound(990)
    }
}

/// Owner-loop work measured once per invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnerPhase {
    /// One input event, from `read` through its command's effects.
    Input,
    /// One frame drawn. A `render` call that found nothing dirty records nothing.
    Render,
    /// One handoff of sealed scanner runs to the store thread, which admits them.
    Admission,
    /// One application of the primary scan's published generation: swapping it in and reading
    /// its first page.
    Publication,
}

impl OwnerPhase {
    pub const ALL: [Self; 4] = [
        Self::Input,
        Self::Render,
        Self::Admission,
        Self::Publication,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Render => "render",
            Self::Admission => "admission",
            Self::Publication => "publication",
        }
    }
}

/// The worker-event variants the owner loop handles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerEventKind {
    ScanBatch,
    ScanUnscanned,
    ScanFailed,
    ScanFinished,
    DeletionPlanned,
    DeletionExecutionRejected,
    DeletionFinished,
}

impl WorkerEventKind {
    pub const ALL: [Self; 7] = [
        Self::ScanBatch,
        Self::ScanUnscanned,
        Self::ScanFailed,
        Self::ScanFinished,
        Self::DeletionPlanned,
        Self::DeletionExecutionRejected,
        Self::DeletionFinished,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ScanBatch => "scan_batch",
            Self::ScanUnscanned => "scan_unscanned",
            Self::ScanFailed => "scan_failed",
            Self::ScanFinished => "scan_finished",
            Self::DeletionPlanned => "deletion_planned",
            Self::DeletionExecutionRejected => "deletion_execution_rejected",
            Self::DeletionFinished => "deletion_finished",
        }
    }

    pub(super) const fn of(event: &WorkerEvent) -> Self {
        match event {
            WorkerEvent::ScanBatch { .. } => Self::ScanBatch,
            WorkerEvent::ScanUnscanned { .. } => Self::ScanUnscanned,
            WorkerEvent::ScanFailed { .. } => Self::ScanFailed,
            WorkerEvent::ScanFinished { .. } => Self::ScanFinished,
            WorkerEvent::DeletionPlanned { .. } => Self::DeletionPlanned,
            WorkerEvent::DeletionExecutionRejected { .. } => Self::DeletionExecutionRejected,
            WorkerEvent::DeletionFinished { .. } => Self::DeletionFinished,
        }
    }
}

/// Everything one probed owner-loop run measured. It is plain fixed-size data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnerLoopReport {
    phases: [PhaseHistogram; OwnerPhase::ALL.len()],
    worker_events: [PhaseHistogram; WorkerEventKind::ALL.len()],
    scan_runs_admitted: u64,
    scan_entries_handled: u64,
    time_to_scan_complete: Option<Duration>,
}

/// A report is a few kilobytes however long the run lasted.
const _: () = assert!(size_of::<OwnerLoopReport>() <= 16 * 1024);

impl Default for OwnerLoopReport {
    fn default() -> Self {
        Self::new()
    }
}

impl OwnerLoopReport {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phases: [PhaseHistogram::new(); OwnerPhase::ALL.len()],
            worker_events: [PhaseHistogram::new(); WorkerEventKind::ALL.len()],
            scan_runs_admitted: 0,
            scan_entries_handled: 0,
            time_to_scan_complete: None,
        }
    }

    #[must_use]
    pub const fn phase(&self, phase: OwnerPhase) -> &PhaseHistogram {
        &self.phases[phase as usize]
    }

    /// Returns the histogram of one worker-event kind; its count is how many of
    /// that kind the loop handled.
    #[must_use]
    pub const fn worker_event(&self, kind: WorkerEventKind) -> &PhaseHistogram {
        &self.worker_events[kind as usize]
    }

    /// Returns the frames drawn: `render` calls that returned `true`.
    #[must_use]
    pub const fn frames_rendered(&self) -> u64 {
        self.phase(OwnerPhase::Render).count()
    }

    /// Returns the sealed scanner runs handed to admission.
    #[must_use]
    pub const fn scan_runs_admitted(&self) -> u64 {
        self.scan_runs_admitted
    }

    /// Returns the scanned entries the loop applied to its model.
    #[must_use]
    pub const fn scan_entries_handled(&self) -> u64 {
        self.scan_entries_handled
    }

    /// Returns when the loop finished handling the first successful scan
    /// completion, measured from probe creation, or `None` if none arrived.
    #[must_use]
    pub const fn time_to_scan_complete(&self) -> Option<Duration> {
        self.time_to_scan_complete
    }
}

#[derive(Debug)]
struct ProbeState {
    started: Instant,
    report: OwnerLoopReport,
}

/// A shared handle to one run's recorded timings. Clones observe the same run.
#[derive(Clone, Debug)]
pub struct OwnerLoopProbe {
    state: Rc<RefCell<ProbeState>>,
}

impl Default for OwnerLoopProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl OwnerLoopProbe {
    /// Starts an empty probe; its clock for [`OwnerLoopReport::time_to_scan_complete`]
    /// begins now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(ProbeState {
                started: Instant::now(),
                report: OwnerLoopReport::new(),
            })),
        }
    }

    /// Returns a copy of everything recorded so far.
    #[must_use]
    pub fn report(&self) -> OwnerLoopReport {
        self.state.borrow().report.clone()
    }

    /// Returns whether the owner loop has finished handling a successful scan completion.
    #[must_use]
    pub fn scan_complete(&self) -> bool {
        self.state.borrow().report.time_to_scan_complete.is_some()
    }

    fn record(&self, target: TimerTarget, elapsed: Duration) {
        let mut state = self.state.borrow_mut();
        let histogram = match target {
            TimerTarget::Phase(phase) => &mut state.report.phases[phase as usize],
            TimerTarget::WorkerEvent(kind) => &mut state.report.worker_events[kind as usize],
        };
        histogram.record(elapsed);
    }

    fn mark_scan_complete(&self) {
        let mut state = self.state.borrow_mut();
        if state.report.time_to_scan_complete.is_none() {
            state.report.time_to_scan_complete = Some(state.started.elapsed());
        }
    }

    /// Stamps a completion without an owner loop, for tests of code that reads it.
    #[cfg(test)]
    pub(crate) fn mark_scan_complete_for_test(&self) {
        self.mark_scan_complete();
    }

    fn add_runs_admitted(&self, runs: usize) {
        let runs = u64::try_from(runs).unwrap_or(u64::MAX);
        let report = &mut self.state.borrow_mut().report;
        report.scan_runs_admitted = report.scan_runs_admitted.saturating_add(runs);
    }

    fn add_entry_handled(&self) {
        let report = &mut self.state.borrow_mut().report;
        report.scan_entries_handled = report.scan_entries_handled.saturating_add(1);
    }
}

#[derive(Clone, Copy, Debug)]
enum TimerTarget {
    Phase(OwnerPhase),
    WorkerEvent(WorkerEventKind),
}

/// Records the wall time between its creation and its drop.
#[must_use = "a timer records when dropped; bind it to a named local for the phase's whole scope"]
pub(super) struct PhaseTimer {
    probe: OwnerLoopProbe,
    target: TimerTarget,
    started: Instant,
    cancelled: bool,
}

impl PhaseTimer {
    fn start(probe: OwnerLoopProbe, target: TimerTarget) -> Self {
        Self {
            probe,
            target,
            started: Instant::now(),
            cancelled: false,
        }
    }

    /// Drops the measurement without recording it.
    pub(super) const fn cancel(&mut self) {
        self.cancelled = true;
    }
}

impl Drop for PhaseTimer {
    fn drop(&mut self) {
        if self.cancelled {
            return;
        }
        self.probe.record(self.target, self.started.elapsed());
    }
}

impl<B> OwnerLoop<B>
where
    B: Backend,
{
    /// Starts timing `phase` when this loop's input source carries a probe.
    #[must_use]
    pub(super) fn probe_phase(&self, phase: OwnerPhase) -> Option<PhaseTimer> {
        let probe = self.input.owner_loop_probe()?;
        Some(PhaseTimer::start(probe.clone(), TimerTarget::Phase(phase)))
    }

    /// Starts timing one worker event under its kind.
    #[must_use]
    pub(super) fn probe_worker_event(&self, event: &WorkerEvent) -> Option<PhaseTimer> {
        let probe = self.input.owner_loop_probe()?;
        Some(PhaseTimer::start(
            probe.clone(),
            TimerTarget::WorkerEvent(WorkerEventKind::of(event)),
        ))
    }

    /// Stamps the probe: the loop finished the scan, its map published. This is the moment the
    /// reader sees the scan complete.
    pub(super) fn probe_scan_complete(&self) {
        if let Some(probe) = self.input.owner_loop_probe() {
            probe.mark_scan_complete();
        }
    }

    /// Counts `runs` sealed runs and starts timing their admission.
    #[must_use]
    pub(super) fn probe_admission(&self, runs: usize) -> Option<PhaseTimer> {
        let probe = self.input.owner_loop_probe()?;
        probe.add_runs_admitted(runs);
        Some(PhaseTimer::start(
            probe.clone(),
            TimerTarget::Phase(OwnerPhase::Admission),
        ))
    }

    pub(super) fn probe_scan_entry(&self) {
        if let Some(probe) = self.input.owner_loop_probe() {
            probe.add_entry_handled();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nanos(values: &[u64]) -> PhaseHistogram {
        let mut histogram = PhaseHistogram::new();
        for value in values {
            histogram.record(Duration::from_nanos(*value));
        }
        histogram
    }

    #[test]
    fn buckets_split_at_powers_of_two_and_zero_shares_the_first() {
        assert_eq!(PhaseHistogram::bucket_index(0), 0);
        assert_eq!(PhaseHistogram::bucket_index(1), 0);
        assert_eq!(PhaseHistogram::bucket_index(2), 1);
        assert_eq!(PhaseHistogram::bucket_index(3), 1);
        assert_eq!(PhaseHistogram::bucket_index(4), 2);
        for exponent in 1..u64::BITS {
            let edge = 1_u64 << exponent;
            let below = PhaseHistogram::bucket_index(edge - 1);
            let at = PhaseHistogram::bucket_index(edge);
            assert_eq!(at, usize::try_from(exponent).unwrap_or(usize::MAX));
            assert_eq!(below + 1, at, "2^{exponent} opens the next bucket");
            assert_eq!(
                PhaseHistogram::bucket_upper_bound(below),
                Duration::from_nanos(edge - 1),
                "a bucket's upper bound is the last value it holds"
            );
        }
        assert_eq!(
            PhaseHistogram::bucket_index(u64::MAX),
            HISTOGRAM_BUCKETS - 1
        );
        assert_eq!(
            PhaseHistogram::bucket_upper_bound(HISTOGRAM_BUCKETS - 1),
            Duration::from_nanos(u64::MAX)
        );
        assert_eq!(
            PhaseHistogram::bucket_upper_bound(HISTOGRAM_BUCKETS + 9),
            Duration::from_nanos(u64::MAX),
            "an out-of-range bucket clamps to the last one"
        );
    }

    #[test]
    fn empty_histogram_reports_no_distribution() {
        let histogram = PhaseHistogram::new();
        assert_eq!(histogram.count(), 0);
        assert_eq!(histogram.sum(), Duration::ZERO);
        assert_eq!(histogram.max(), Duration::ZERO);
        assert_eq!(histogram.mean(), None);
        assert_eq!(histogram.p99_bucket(), None);
        assert_eq!(histogram.p99_upper_bound(), None);
    }

    #[test]
    fn samples_keep_an_exact_count_sum_and_max() {
        let histogram = nanos(&[0, 1, 5, 1_000, 7, 1_000_000]);
        assert_eq!(histogram.count(), 6);
        assert_eq!(histogram.sum(), Duration::from_nanos(1_001_013));
        assert_eq!(histogram.max(), Duration::from_nanos(1_000_000));
        assert_eq!(histogram.mean(), Some(Duration::from_nanos(166_835)));
        assert_eq!(histogram.buckets().iter().sum::<u64>(), histogram.count());
        assert_eq!(histogram.buckets()[0], 2, "0 ns and 1 ns share bucket 0");
        assert_eq!(histogram.buckets()[2], 2, "5 ns and 7 ns lie in [4, 8)");
        assert_eq!(histogram.buckets()[9], 1, "1,000 ns lies in [512, 1024)");
        assert_eq!(
            histogram.buckets()[19],
            1,
            "1,000,000 ns lies in [2^19, 2^20)"
        );
    }

    #[test]
    fn extreme_durations_saturate_instead_of_wrapping() {
        let mut histogram = PhaseHistogram::new();
        histogram.record(Duration::MAX);
        histogram.record(Duration::from_nanos(u64::MAX));
        histogram.record(Duration::from_secs(1));
        assert_eq!(histogram.count(), 3);
        assert_eq!(histogram.max(), Duration::from_nanos(u64::MAX));
        assert_eq!(
            histogram.sum(),
            Duration::from_nanos(u64::MAX),
            "the sum saturates"
        );
        assert_eq!(histogram.buckets()[HISTOGRAM_BUCKETS - 1], 2);
        assert_eq!(histogram.buckets()[29], 1, "one second is about 2^30 ns");
    }

    #[test]
    fn quantile_bound_covers_the_exact_nearest_rank_sample() {
        // A deterministic spread across many magnitudes, with repeats and zeros.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut samples = Vec::new();
        for _ in 0..5_000 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            samples.push(state >> (state % 61 + 3));
        }
        samples.push(0);
        let histogram = nanos(&samples);
        samples.sort_unstable();
        let count = samples.len();
        for per_mille in [1_u16, 500, 900, 990, 999, 1_000] {
            let rank = (count * usize::from(per_mille)).div_ceil(1_000).max(1);
            let exact = samples[rank - 1];
            let bound = histogram
                .quantile_upper_bound(per_mille)
                .expect("a populated histogram has every quantile");
            let bound = u64::try_from(bound.as_nanos()).expect("bounds fit in u64");
            assert!(bound >= exact, "p{per_mille}: {bound} must cover {exact}");
            assert!(
                bound <= exact.saturating_mul(2).max(1) - u64::from(exact != 0),
                "p{per_mille}: bound {bound} must stay within its bucket of {exact}"
            );
            assert!(bound <= u64::try_from(histogram.max().as_nanos()).unwrap_or(u64::MAX));
        }
        assert_eq!(
            histogram.quantile_upper_bound(1_000),
            Some(histogram.max()),
            "the top quantile is tightened to the exact maximum"
        );
    }

    #[test]
    fn p99_ignores_one_outlier_in_a_hundred_but_not_two_in_a_hundred_and_one() {
        let mut histogram = PhaseHistogram::new();
        for _ in 0..99 {
            histogram.record(Duration::from_micros(10));
        }
        let fast = PhaseHistogram::bucket_index(10_000);
        assert_eq!(histogram.p99_bucket(), Some(fast));
        histogram.record(Duration::from_millis(400));
        assert_eq!(
            histogram.p99_bucket(),
            Some(fast),
            "the 99th of 100 samples is still a fast one"
        );
        histogram.record(Duration::from_millis(400));
        assert_eq!(
            histogram.p99_bucket(),
            Some(PhaseHistogram::bucket_index(400_000_000)),
            "the 100th of 101 samples is a slow one"
        );
        assert_eq!(histogram.max(), Duration::from_millis(400));
    }

    #[test]
    fn a_long_run_keeps_the_exact_sum_and_max() {
        let mut histogram = PhaseHistogram::new();
        for _ in 0..100_000 {
            histogram.record(Duration::from_millis(3));
        }
        histogram.record(Duration::from_millis(9));
        assert_eq!(histogram.count(), 100_001);
        assert_eq!(histogram.sum(), Duration::from_millis(300_009));
        assert_eq!(histogram.max(), Duration::from_millis(9));
        assert_eq!(histogram.buckets().iter().sum::<u64>(), 100_001);
    }

    #[test]
    fn worker_events_and_phases_record_into_separate_histograms() {
        let probe = OwnerLoopProbe::new();
        probe.record(
            TimerTarget::Phase(OwnerPhase::Publication),
            Duration::from_micros(5),
        );
        probe.record(
            TimerTarget::WorkerEvent(WorkerEventKind::ScanFinished),
            Duration::from_micros(9),
        );
        probe.record(
            TimerTarget::WorkerEvent(WorkerEventKind::ScanFinished),
            Duration::from_micros(3),
        );
        let report = probe.report();
        assert_eq!(report.phase(OwnerPhase::Publication).count(), 1);
        assert_eq!(
            report.worker_event(WorkerEventKind::ScanFinished).count(),
            2
        );
        assert_eq!(
            report.worker_event(WorkerEventKind::ScanFinished).max(),
            Duration::from_micros(9)
        );
        for phase in OwnerPhase::ALL {
            let expected = u64::from(phase == OwnerPhase::Publication);
            assert_eq!(report.phase(phase).count(), expected, "{}", phase.label());
        }
        for kind in WorkerEventKind::ALL {
            let expected = if kind == WorkerEventKind::ScanFinished {
                2
            } else {
                0
            };
            assert_eq!(
                report.worker_event(kind).count(),
                expected,
                "{}",
                kind.label()
            );
        }
    }

    #[test]
    fn a_cancelled_timer_records_nothing() {
        let probe = OwnerLoopProbe::new();
        drop(PhaseTimer::start(
            probe.clone(),
            TimerTarget::Phase(OwnerPhase::Render),
        ));
        let mut cancelled =
            PhaseTimer::start(probe.clone(), TimerTarget::Phase(OwnerPhase::Render));
        cancelled.cancel();
        drop(cancelled);
        assert_eq!(probe.report().frames_rendered(), 1);
    }

    #[test]
    fn scan_completion_is_stamped_once_and_not_by_the_finish_event() {
        let probe = OwnerLoopProbe::new();
        assert!(!probe.scan_complete());
        // The scanner is done when its finish event is handled, but the map is not published yet,
        // so the reader does not see the scan complete.
        drop(PhaseTimer::start(
            probe.clone(),
            TimerTarget::WorkerEvent(WorkerEventKind::ScanFinished),
        ));
        assert!(
            !probe.scan_complete(),
            "handling the scanner's finish event is not the scan's completion"
        );

        probe.mark_scan_complete();
        assert!(probe.scan_complete());
        let first = probe.report().time_to_scan_complete();

        probe.mark_scan_complete();
        assert_eq!(
            probe.report().time_to_scan_complete(),
            first,
            "a later completion never moves the first"
        );
    }

    #[test]
    fn counters_accumulate_across_multiple_calls() {
        let probe = OwnerLoopProbe::new();
        probe.add_runs_admitted(3);
        probe.add_runs_admitted(2);
        probe.add_entry_handled();
        probe.add_entry_handled();
        let report = probe.report();
        assert_eq!(report.scan_runs_admitted(), 5);
        assert_eq!(report.scan_entries_handled(), 2);
    }
}
