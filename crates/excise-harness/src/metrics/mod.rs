//! Measurements of a run: latency, stalls, output volume, and resource use.
//!
//! [`Recorder`] collects what only the runner knows (when each input was written, which windows
//! count as active, which measurements were opened) and [`Recorder::finish`] combines it with the
//! event log, the output byte count, and the process sampler into named metrics: a map from
//! metric name to a finite number. The names are the ones scenarios use in `expect_budget`.
//!
//! | Metric | Meaning |
//! |---|---|
//! | `first_frame_ms` | Spawn to the first `frame` event. |
//! | `scan_complete_ms` | Spawn to the `scan_complete` event. |
//! | `input_to_frame_p50_ms`, `input_to_frame_p99_ms`, `input_to_frame_max_ms` | Isolated input to the first frame that reflects it. |
//! | `input_samples` | How many isolated inputs were timed. |
//! | `max_stall_ms` | The longest gap between frames inside an active window. |
//! | `quit_ms` | The quit confirmation to the exit of the process. |
//! | `delete_ms` | The deletion confirmation to the `deletion_finished` event. |
//! | `output_bytes`, `output_bytes_per_s` | Terminal output, in total and per second of the run. |
//! | `frames`, `inputs_sent` | Frames drawn and input events sent. |
//! | `peak_rss_bytes` | Peak memory: the peak physical footprint on macOS, the peak resident set size on Linux. |
//! | `threads`, `fds` | The most threads and descriptors seen in a sample. |
//! | `scan_store_peak_bytes` | Peak total apparent size of the run's scan-store directory, seen in a sample. |
//! | `scan_store_bytes_per_entry` | `scan_store_peak_bytes` divided by `scan_complete`'s `entries`. |
//! | `user_ms`, `sys_ms` | CPU time of the child, from the change in `RUSAGE_CHILDREN`. |
//! | `residue_files` | Entries left in the scratch area after the exit. |
//! | any `measure` name | Elapsed milliseconds between its `start` and `stop`. |
//!
//! # Definitions
//!
//! * **Isolated input.** An input written when every earlier input already had a frame that
//!   reflects it. Typing a word fast queues its letters behind each other, and the wait of a
//!   queued letter measures the burst, not the interface, so only isolated inputs are timed.
//!   A frame reflects an input when the frame's `inputs` counter is at least the number of inputs
//!   sent up to and including it and the frame was observed after the input was written.
//! * **Active window.** From the first frame to `scan_complete`, and from each deletion
//!   confirmation to its `deletion_finished`. Outside a window the program is allowed to be quiet;
//!   idle output is its own budget.
//! * **Max stall.** Within each window, the largest gap between consecutive frames, counting the
//!   window's start and end as frames so a stall at either edge shows. Times are when the harness
//!   observed the events, so they are accurate to the polling interval (about a millisecond).
//! * **Percentiles** use the nearest-rank method: the value at rank `ceil(p * n)` of the sorted
//!   samples, so the 99th percentile of fewer than a hundred samples is the largest.
//! * **CPU time** is the change in the CPU time of all reaped children around this child's life.
//!   It is exact when the harness reaps one child at a time, as `cargo xtask e2e` does; a process
//!   that reaps other children concurrently would fold their time in.

mod sample;
mod store;

use std::{collections::BTreeMap, time::Duration, time::Instant};

pub use sample::ProcessSampler;
pub use store::StoreSampler;

use crate::events::{Event, Payload};

/// The CPU time of a process, or the change in it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    /// Time spent in user mode.
    pub user: Duration,
    /// Time spent in the kernel.
    pub system: Duration,
}

impl CpuTimes {
    /// The time used since `earlier`.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            user: self.user.saturating_sub(earlier.user),
            system: self.system.saturating_sub(earlier.system),
        }
    }
}

/// The CPU time of every child this process has reaped so far, or `None` where the platform has no
/// safe way to ask.
#[cfg(unix)]
#[must_use]
pub fn reaped_children_cpu() -> Option<CpuTimes> {
    use nix::sys::{
        resource::{UsageWho, getrusage},
        time::TimeValLike,
    };

    let usage = getrusage(UsageWho::RUSAGE_CHILDREN).ok()?;
    let micros = |time: nix::sys::time::TimeVal| {
        Duration::from_micros(u64::try_from(time.num_microseconds()).unwrap_or(0))
    };
    Some(CpuTimes {
        user: micros(usage.user_time()),
        system: micros(usage.system_time()),
    })
}

/// The CPU time of every child this process has reaped so far.
#[cfg(not(unix))]
#[must_use]
pub fn reaped_children_cpu() -> Option<CpuTimes> {
    None
}

/// An input the runner wrote to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRecord {
    /// When it was written.
    pub at: Instant,
    /// The input events written so far, this one included.
    pub cumulative: u64,
    /// Whether every earlier input already had a frame that reflected it.
    pub isolated: bool,
}

/// What the runner knows about a run that the event log does not.
#[derive(Debug)]
pub struct Recorder {
    spawned_at: Instant,
    inputs: Vec<InputRecord>,
    deletion_confirmations: Vec<Instant>,
    quit_confirmed_at: Option<Instant>,
    exited_at: Option<Instant>,
    open_measures: BTreeMap<String, Instant>,
    measures: BTreeMap<String, f64>,
}

impl Recorder {
    /// A recorder for a process spawned at `spawned_at`.
    #[must_use]
    pub const fn new(spawned_at: Instant) -> Self {
        Self {
            spawned_at,
            inputs: Vec::new(),
            deletion_confirmations: Vec::new(),
            quit_confirmed_at: None,
            exited_at: None,
            open_measures: BTreeMap::new(),
            measures: BTreeMap::new(),
        }
    }

    /// When the process was spawned.
    #[must_use]
    pub const fn spawned_at(&self) -> Instant {
        self.spawned_at
    }

    /// Records an input event written at `at`; `cumulative` counts it.
    pub fn record_input(&mut self, at: Instant, cumulative: u64, isolated: bool) {
        self.inputs.push(InputRecord {
            at,
            cumulative,
            isolated,
        });
    }

    /// Records that a deletion was confirmed at `at`.
    pub fn record_deletion_confirmed(&mut self, at: Instant) {
        self.deletion_confirmations.push(at);
    }

    /// Records that the quit was confirmed at `at`.
    pub fn record_quit_confirmed(&mut self, at: Instant) {
        self.quit_confirmed_at = Some(at);
    }

    /// Records that the process was seen to exit at `at`.
    pub fn record_exit(&mut self, at: Instant) {
        self.exited_at.get_or_insert(at);
    }

    /// Opens the measurement `name` at `at`.
    pub fn start_measure(&mut self, name: &str, at: Instant) {
        self.open_measures.insert(name.to_owned(), at);
    }

    /// Closes the measurement `name` at `at` and returns its milliseconds, or `None` if it was
    /// never opened.
    pub fn stop_measure(&mut self, name: &str, at: Instant) -> Option<f64> {
        let started = self.open_measures.remove(name)?;
        let elapsed = milliseconds(at.saturating_duration_since(started));
        self.measures.insert(name.to_owned(), elapsed);
        Some(elapsed)
    }

    /// The isolated input timings so far: each input's latency in milliseconds.
    #[must_use]
    pub fn input_latencies(&self, events: &[Event]) -> Vec<f64> {
        self.inputs
            .iter()
            .filter(|input| input.isolated)
            .filter_map(|input| {
                events
                    .iter()
                    .find(|event| {
                        matches!(
                            event.payload,
                            Payload::Frame { inputs, .. }
                                if inputs >= input.cumulative && event.observed >= input.at
                        )
                    })
                    .map(|frame| milliseconds(frame.observed.saturating_duration_since(input.at)))
            })
            .collect()
    }

    /// The metrics of the run so far.
    ///
    /// `output_bytes` counts terminal output, `sampler` holds the resource peaks, `store_peak_bytes`
    /// is the peak size [`crate::metrics::StoreSampler`] saw of the run's scan-store directory, and
    /// `cpu` is the child's CPU time when known. `now` closes the run for the output rate.
    #[must_use]
    pub fn finish(
        &self,
        events: &[Event],
        output_bytes: u64,
        sampler: &ProcessSampler,
        store_peak_bytes: Option<u64>,
        cpu: Option<CpuTimes>,
        now: Instant,
    ) -> BTreeMap<String, f64> {
        let mut metrics = self.measures.clone();
        let since_spawn = |at: Instant| milliseconds(at.saturating_duration_since(self.spawned_at));

        let frames: Vec<Instant> = events
            .iter()
            .filter(|event| matches!(event.payload, Payload::Frame { .. }))
            .map(|event| event.observed)
            .collect();
        metrics.insert("frames".to_owned(), count(frames.len() as u64));
        if let Some(first) = frames.first() {
            metrics.insert("first_frame_ms".to_owned(), since_spawn(*first));
        }
        let scan_complete = events
            .iter()
            .find(|event| matches!(event.payload, Payload::ScanComplete { .. }))
            .map(|event| event.observed);
        if let Some(at) = scan_complete {
            metrics.insert("scan_complete_ms".to_owned(), since_spawn(at));
        }
        let scan_complete_entries = events.iter().find_map(|event| match event.payload {
            Payload::ScanComplete { entries } => Some(entries),
            _ => None,
        });

        metrics.insert(
            "inputs_sent".to_owned(),
            count(self.inputs.last().map_or(0, |input| input.cumulative)),
        );
        let mut latencies = self.input_latencies(events);
        latencies.sort_by(f64::total_cmp);
        if !latencies.is_empty() {
            metrics.insert("input_samples".to_owned(), count(latencies.len() as u64));
            metrics.insert(
                "input_to_frame_p50_ms".to_owned(),
                nearest_rank(&latencies, 0.50),
            );
            metrics.insert(
                "input_to_frame_p99_ms".to_owned(),
                nearest_rank(&latencies, 0.99),
            );
            metrics.insert(
                "input_to_frame_max_ms".to_owned(),
                nearest_rank(&latencies, 1.0),
            );
        }

        if let Some(stall) = self.max_stall(events, scan_complete) {
            metrics.insert("max_stall_ms".to_owned(), stall);
        }
        if let Some(first) = self.deletion_confirmations.first() {
            let finished = events
                .iter()
                .find(|event| {
                    matches!(event.payload, Payload::DeletionFinished { .. })
                        && event.observed >= *first
                })
                .map(|event| event.observed);
            if let Some(finished) = finished {
                metrics.insert(
                    "delete_ms".to_owned(),
                    milliseconds(finished.saturating_duration_since(*first)),
                );
            }
        }
        if let (Some(confirmed), Some(exited)) = (self.quit_confirmed_at, self.exited_at) {
            metrics.insert(
                "quit_ms".to_owned(),
                milliseconds(exited.saturating_duration_since(confirmed)),
            );
        }

        metrics.insert("output_bytes".to_owned(), count(output_bytes));
        let seconds = now.saturating_duration_since(self.spawned_at).as_secs_f64();
        if seconds > 0.0 {
            metrics.insert(
                "output_bytes_per_s".to_owned(),
                count(output_bytes) / seconds,
            );
        }
        if let Some(bytes) = sampler.peak_memory_bytes() {
            metrics.insert("peak_rss_bytes".to_owned(), count(bytes));
        }
        if let Some(threads) = sampler.max_threads() {
            metrics.insert("threads".to_owned(), f64::from(threads));
        }
        if let Some(fds) = sampler.max_fds() {
            metrics.insert("fds".to_owned(), f64::from(fds));
        }
        if let Some(bytes) = store_peak_bytes {
            metrics.insert("scan_store_peak_bytes".to_owned(), count(bytes));
            if let Some(entries) = scan_complete_entries.filter(|&entries| entries > 0) {
                metrics.insert(
                    "scan_store_bytes_per_entry".to_owned(),
                    count(bytes) / count(entries),
                );
            }
        }
        if let Some(cpu) = cpu {
            metrics.insert("user_ms".to_owned(), milliseconds(cpu.user));
            metrics.insert("sys_ms".to_owned(), milliseconds(cpu.system));
        }
        metrics
    }

    /// The longest gap between frames inside an active window, in milliseconds.
    fn max_stall(&self, events: &[Event], scan_complete: Option<Instant>) -> Option<f64> {
        let frames: Vec<Instant> = events
            .iter()
            .filter(|event| matches!(event.payload, Payload::Frame { .. }))
            .map(|event| event.observed)
            .collect();
        let mut windows: Vec<(Instant, Instant)> = Vec::new();
        if let (Some(first), Some(end)) = (frames.first(), scan_complete) {
            windows.push((*first, end.max(*first)));
        }
        for confirmed in &self.deletion_confirmations {
            let finished = events
                .iter()
                .find(|event| {
                    matches!(event.payload, Payload::DeletionFinished { .. })
                        && event.observed >= *confirmed
                })
                .map(|event| event.observed);
            if let Some(finished) = finished {
                windows.push((*confirmed, finished));
            }
        }
        windows
            .into_iter()
            .map(|(start, end)| {
                let mut marks: Vec<Instant> = frames
                    .iter()
                    .copied()
                    .filter(|frame| *frame >= start && *frame <= end)
                    .collect();
                marks.push(start);
                marks.push(end);
                marks.sort();
                marks
                    .windows(2)
                    .map(|pair| milliseconds(pair[1].saturating_duration_since(pair[0])))
                    .fold(0.0, f64::max)
            })
            .reduce(f64::max)
    }
}

/// The value at rank `ceil(fraction * n)` of `sorted`, which must be sorted and not empty.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "the rank is a small positive number: at least 1 and at most the sample count"
)]
pub fn nearest_rank(sorted: &[f64], fraction: f64) -> f64 {
    let rank = (fraction * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

/// The milliseconds in `duration`.
#[must_use]
pub fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[allow(clippy::cast_precision_loss)]
fn count(value: u64) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Events observed at millisecond offsets from `start`.
    fn frame(start: Instant, at_ms: u64, seq: u64, inputs: u64) -> Event {
        Event {
            t_us: at_ms * 1000,
            observed: start + Duration::from_millis(at_ms),
            payload: Payload::Frame { seq, inputs },
        }
    }

    fn other(start: Instant, at_ms: u64, payload: Payload) -> Event {
        Event {
            t_us: at_ms * 1000,
            observed: start + Duration::from_millis(at_ms),
            payload,
        }
    }

    fn finish(
        recorder: &Recorder,
        events: &[Event],
        start: Instant,
        end_ms: u64,
    ) -> BTreeMap<String, f64> {
        recorder.finish(
            events,
            0,
            &ProcessSampler::default(),
            None,
            None,
            start + Duration::from_millis(end_ms),
        )
    }

    #[test]
    fn nearest_rank_percentiles_follow_the_documented_definition() {
        let samples: Vec<f64> = (1..=10).map(f64::from).collect();

        assert!((nearest_rank(&samples, 0.50) - 5.0).abs() < f64::EPSILON);
        assert!((nearest_rank(&samples, 0.99) - 10.0).abs() < f64::EPSILON);
        assert!((nearest_rank(&samples, 1.0) - 10.0).abs() < f64::EPSILON);
        assert!((nearest_rank(&[7.0], 0.99) - 7.0).abs() < f64::EPSILON);
        let hundred: Vec<f64> = (1..=100).map(f64::from).collect();
        assert!((nearest_rank(&hundred, 0.99) - 99.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_first_frame_and_scan_completion_count_from_the_spawn() {
        let start = Instant::now();
        let events = [
            frame(start, 180, 1, 0),
            other(start, 420, Payload::ScanComplete { entries: 14 }),
        ];

        let metrics = finish(&Recorder::new(start), &events, start, 500);

        assert!((metrics["first_frame_ms"] - 180.0).abs() < 1e-6);
        assert!((metrics["scan_complete_ms"] - 420.0).abs() < 1e-6);
        assert!((metrics["frames"] - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn an_isolated_input_is_timed_to_the_first_frame_that_reflects_it() {
        let start = Instant::now();
        let mut recorder = Recorder::new(start);
        recorder.record_input(start + Duration::from_millis(100), 1, true);
        recorder.record_input(start + Duration::from_millis(300), 2, true);
        let events = [
            frame(start, 90, 1, 0),
            frame(start, 112, 2, 1),
            frame(start, 500, 3, 2),
        ];

        let latencies = recorder.input_latencies(&events);

        assert_eq!(latencies.len(), 2);
        assert!((latencies[0] - 12.0).abs() < 1e-6, "{latencies:?}");
        assert!((latencies[1] - 200.0).abs() < 1e-6, "{latencies:?}");
    }

    #[test]
    fn a_frame_from_before_the_input_never_answers_it() {
        // The counter says the input was consumed, but the frame was observed before it was written:
        // the counter must have included an input the harness did not send.
        let start = Instant::now();
        let mut recorder = Recorder::new(start);
        recorder.record_input(start + Duration::from_millis(100), 1, true);
        let events = [frame(start, 50, 1, 1), frame(start, 130, 2, 1)];

        let latencies = recorder.input_latencies(&events);

        assert_eq!(latencies.len(), 1);
        assert!((latencies[0] - 30.0).abs() < 1e-6);
    }

    #[test]
    fn queued_inputs_are_not_timed() {
        let start = Instant::now();
        let mut recorder = Recorder::new(start);
        recorder.record_input(start + Duration::from_millis(100), 1, true);
        recorder.record_input(start + Duration::from_millis(101), 2, false);
        recorder.record_input(start + Duration::from_millis(102), 3, false);
        let events = [frame(start, 140, 1, 3)];

        let metrics = finish(&recorder, &events, start, 200);

        assert!((metrics["input_samples"] - 1.0).abs() < f64::EPSILON);
        assert!((metrics["input_to_frame_max_ms"] - 40.0).abs() < 1e-6);
        assert!((metrics["inputs_sent"] - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn an_input_that_never_gets_a_frame_has_no_latency() {
        let start = Instant::now();
        let mut recorder = Recorder::new(start);
        recorder.record_input(start + Duration::from_millis(100), 1, true);

        let metrics = finish(&recorder, &[frame(start, 50, 1, 0)], start, 200);

        assert!(!metrics.contains_key("input_to_frame_p99_ms"));
    }

    #[test]
    fn the_stall_is_the_longest_gap_between_frames_inside_a_window() {
        let start = Instant::now();
        let events = [
            frame(start, 100, 1, 0),
            frame(start, 130, 2, 0),
            frame(start, 400, 3, 0),
            frame(start, 420, 4, 0),
            other(start, 470, Payload::ScanComplete { entries: 1 }),
            // Quiet after completion is not a stall.
            frame(start, 5000, 5, 0),
        ];

        let metrics = finish(&Recorder::new(start), &events, start, 6000);

        assert!(
            (metrics["max_stall_ms"] - 270.0).abs() < 1e-6,
            "{metrics:?}"
        );
    }

    #[test]
    fn a_stall_at_the_end_of_a_window_counts() {
        let start = Instant::now();
        let events = [
            frame(start, 100, 1, 0),
            frame(start, 110, 2, 0),
            other(start, 700, Payload::ScanComplete { entries: 1 }),
        ];

        let metrics = finish(&Recorder::new(start), &events, start, 800);

        assert!(
            (metrics["max_stall_ms"] - 590.0).abs() < 1e-6,
            "{metrics:?}"
        );
    }

    #[test]
    fn a_deletion_is_its_own_window_and_is_timed() {
        let start = Instant::now();
        let mut recorder = Recorder::new(start);
        recorder.record_deletion_confirmed(start + Duration::from_millis(1000));
        let events = [
            frame(start, 100, 1, 0),
            other(start, 150, Payload::ScanComplete { entries: 1 }),
            frame(start, 1010, 2, 1),
            frame(start, 1500, 3, 1),
            other(
                start,
                1600,
                Payload::DeletionFinished {
                    removed: 5,
                    failed: 0,
                },
            ),
        ];

        let metrics = finish(&recorder, &events, start, 2000);

        assert!((metrics["delete_ms"] - 600.0).abs() < 1e-6);
        assert!(
            (metrics["max_stall_ms"] - 490.0).abs() < 1e-6,
            "{metrics:?}"
        );
    }

    #[test]
    fn quit_latency_runs_from_the_confirmation_to_the_exit() {
        let start = Instant::now();
        let mut recorder = Recorder::new(start);
        recorder.record_quit_confirmed(start + Duration::from_millis(900));
        recorder.record_exit(start + Duration::from_millis(931));
        recorder.record_exit(start + Duration::from_millis(5000));

        let metrics = finish(&recorder, &[], start, 1000);

        assert!(
            (metrics["quit_ms"] - 31.0).abs() < 1e-6,
            "the first exit observation counts"
        );
    }

    #[test]
    fn output_volume_and_rate_come_from_the_byte_count() {
        let start = Instant::now();

        let metrics = Recorder::new(start).finish(
            &[],
            2_000,
            &ProcessSampler::default(),
            None,
            None,
            start + Duration::from_secs(2),
        );

        assert!((metrics["output_bytes"] - 2000.0).abs() < f64::EPSILON);
        assert!((metrics["output_bytes_per_s"] - 1000.0).abs() < 1e-6);
    }

    #[test]
    fn named_measurements_record_elapsed_milliseconds() {
        let start = Instant::now();
        let mut recorder = Recorder::new(start);

        recorder.start_measure("delete-window", start + Duration::from_millis(10));
        let elapsed = recorder.stop_measure("delete-window", start + Duration::from_millis(60));

        assert!(elapsed.is_some_and(|ms| (ms - 50.0).abs() < 1e-6));
        assert!((finish(&recorder, &[], start, 100)["delete-window"] - 50.0).abs() < 1e-6);
        assert_eq!(recorder.stop_measure("never-started", start), None);
    }

    #[test]
    fn cpu_times_are_reported_as_a_change() {
        let start = Instant::now();
        let before = CpuTimes {
            user: Duration::from_millis(100),
            system: Duration::from_millis(40),
        };
        let after = CpuTimes {
            user: Duration::from_millis(160),
            system: Duration::from_millis(45),
        };

        let metrics = Recorder::new(start).finish(
            &[],
            0,
            &ProcessSampler::default(),
            None,
            Some(after.since(before)),
            start,
        );

        assert!((metrics["user_ms"] - 60.0).abs() < 1e-6);
        assert!((metrics["sys_ms"] - 5.0).abs() < 1e-6);
        assert_eq!(
            before.since(after),
            CpuTimes::default(),
            "time never goes negative"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_children_cpu_counter_is_readable() {
        assert!(reaped_children_cpu().is_some());
    }

    #[test]
    fn every_metric_is_finite() {
        let start = Instant::now();
        let mut recorder = Recorder::new(start);
        recorder.record_input(start, 1, true);
        recorder.record_deletion_confirmed(start);
        let events = [
            frame(start, 0, 1, 1),
            other(
                start,
                5,
                Payload::DeletionFinished {
                    removed: 1,
                    failed: 0,
                },
            ),
        ];

        let metrics = finish(&recorder, &events, start, 0);

        assert!(
            metrics.values().all(|value| value.is_finite()),
            "{metrics:?}"
        );
    }

    #[test]
    fn scan_store_bytes_per_entry_divides_the_peak_by_the_scanned_entries() {
        let start = Instant::now();
        let events = [other(start, 10, Payload::ScanComplete { entries: 4 })];

        let metrics = Recorder::new(start).finish(
            &events,
            0,
            &ProcessSampler::default(),
            Some(2_000),
            None,
            start,
        );

        assert!((metrics["scan_store_peak_bytes"] - 2000.0).abs() < f64::EPSILON);
        assert!((metrics["scan_store_bytes_per_entry"] - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn scan_store_peak_bytes_with_no_scan_complete_event_has_no_ratio() {
        let start = Instant::now();

        let metrics =
            Recorder::new(start).finish(&[], 0, &ProcessSampler::default(), Some(900), None, start);

        assert!((metrics["scan_store_peak_bytes"] - 900.0).abs() < f64::EPSILON);
        assert!(
            !metrics.contains_key("scan_store_bytes_per_entry"),
            "{metrics:?}"
        );
    }

    #[test]
    fn a_scan_of_zero_entries_never_divides_by_zero() {
        let start = Instant::now();
        let events = [other(start, 10, Payload::ScanComplete { entries: 0 })];

        let metrics = Recorder::new(start).finish(
            &events,
            0,
            &ProcessSampler::default(),
            Some(900),
            None,
            start,
        );

        assert!(
            !metrics.contains_key("scan_store_bytes_per_entry"),
            "{metrics:?}"
        );
    }

    #[test]
    fn no_sample_means_neither_scan_store_metric_is_reported() {
        let start = Instant::now();
        let events = [other(start, 10, Payload::ScanComplete { entries: 4 })];

        let metrics =
            Recorder::new(start).finish(&events, 0, &ProcessSampler::default(), None, None, start);

        assert!(
            !metrics.contains_key("scan_store_peak_bytes"),
            "{metrics:?}"
        );
        assert!(
            !metrics.contains_key("scan_store_bytes_per_entry"),
            "{metrics:?}"
        );
    }
}
