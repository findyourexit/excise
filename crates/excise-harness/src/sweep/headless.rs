//! The headless checks of the sweep: one scan of a fixture by one version, held to the oracle, and
//! the timed scans that the paired rounds are made of.
//!
//! Both run `excise --format json --output <report> <root>` under the isolation of the other
//! runners (an empty environment rebuilt from `TERM`, `COLORTERM`, and `LANG`, a scratch `HOME`,
//! configuration, working directory, temporary directory, and scan-store directory), against a
//! fixture root that carries the ownership marker, bounded by a deadline. Every published version
//! takes that command line; what differs is the report: version 1 until v1.2.4, version 3 from
//! v1.3.0, and [`ScanDocument::read`] reads both.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    path::Path,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::{
    fixture::Oracle,
    headless::{
        DiscrepancyKind, DocumentError, ScanDocument, ScanRequest, ScanRun,
        diff::{Scan, diff},
        document::{ScanState, path_bytes},
        du::Du,
        process::{self, Ended},
        run_scan,
    },
    runner::resolve_binary,
    safety::{FixtureRoot, Scratch, isolated_env},
    scenario::Profile,
};

use super::{
    checks::{Checked, Recorded},
    timing::{Reading, RunValue},
};

/// How many discrepancies of the diff the evidence file lists.
const LISTED: usize = 40;

/// The series a headless scan feeds: its wall time, the time its report took to write, and its
/// time without the report.
pub(crate) const HEADLESS_WALL: &str = "headless_wall_ms";
pub(crate) const REPORT_WRITE: &str = "report_write_ms";
pub(crate) const HEADLESS_SCAN: &str = "headless_scan_ms";
/// The least a scan can have taken without its report for that time to be believed: a poll that
/// sees the report grow for as long as the process ran has not told the scan from the write.
const MIN_SCAN: Duration = Duration::from_millis(1);
/// How often the length of the report file is read while the process runs.
const POLL: Duration = Duration::from_micros(500);

/// What the report of a scan came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReportRead {
    /// The report was read and held to the oracle.
    Read {
        /// The `schema_version` the build wrote.
        version: u32,
        /// The state of the whole report.
        state: ScanState,
        /// How many discrepancies of each kind the oracle diff found.
        kinds: BTreeMap<DiscrepancyKind, u64>,
        /// The scan-store limit the report's summary states (the model's limit in a version-1
        /// report).
        scan_store_limit_bytes: u64,
    },
    /// The scan wrote no report.
    Missing(String),
    /// The report could not be read: it breaks its schema, or is not JSON.
    Invalid(String),
}

/// One scan of one fixture by one version, held to the oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OracleObservation {
    /// How the process ended.
    pub ended: Ended,
    /// Whether the deadline passed and the process was killed.
    pub timed_out: bool,
    /// From the spawn to the exit.
    pub wall: Duration,
    /// What the scan left in its scratch area besides the report.
    pub residue: Vec<String>,
    /// What came of the report.
    pub report: ReportRead,
    /// The free space of the volume the scan's scratch area is on, before the scan, in bytes of
    /// available blocks. `None` where the platform cannot say.
    pub available_bytes: Option<u64>,
}

/// A count or a size as a metric. Both stay far below 2^52, where an `f64` stops holding every
/// integer.
#[allow(clippy::cast_precision_loss)]
fn as_metric(count: u64) -> f64 {
    count as f64
}

/// The name of the oracle check of `fixture`.
pub(crate) fn oracle_check_name() -> &'static str {
    "headless-oracle"
}

/// The free space, in bytes, of the volume that holds `dir`: available blocks times the fragment
/// size, which is what POSIX defines `f_bavail` in.
#[cfg(unix)]
pub(crate) fn available_bytes(dir: &Path) -> Option<u64> {
    let statistics = rustix::fs::statvfs(dir).ok()?;
    statistics.f_bavail.checked_mul(statistics.f_frsize)
}

/// The free space of the volume that holds `dir`. Windows has no `statvfs`, and the sweep has no
/// safe wrapper for what it has.
#[cfg(not(unix))]
pub(crate) fn available_bytes(_dir: &Path) -> Option<u64> {
    None
}

/// Scans `fixture` once with the build at `binary` and holds the report to `oracle`.
pub(crate) fn oracle_run(
    binary: &Path,
    fixture_id: &str,
    fixture: &FixtureRoot,
    oracle: &Oracle,
    work_dir: &Path,
    timeout: Duration,
) -> Checked<OracleObservation> {
    let check = oracle_check_name();
    let available = available_bytes(work_dir);
    let run = match run_scan(&ScanRequest {
        binary,
        fixture,
        work_dir,
        profile: Profile::Default,
        timeout,
    }) {
        Ok(run) => run,
        Err(error) => {
            return Checked {
                observation: None,
                record: Recorded::errored(
                    check,
                    Some(fixture_id),
                    Some(Profile::Default),
                    error.to_string(),
                ),
            };
        }
    };
    let finished = &run.finished;
    let ReportReading {
        report,
        evidence,
        mut metrics,
    } = read_report(&run, fixture, oracle, timeout);
    metrics.insert("wall_ms".to_owned(), finished.wall.as_secs_f64() * 1000.0);
    if let Ended::Exited(code) = finished.ended {
        metrics.insert("exit_code".to_owned(), f64::from(code));
    }
    metrics.insert(
        "residue_files".to_owned(),
        as_metric(run.residue.len() as u64),
    );
    if let ReportRead::Read {
        version,
        scan_store_limit_bytes,
        ..
    } = &report
    {
        metrics.insert("report_version".to_owned(), f64::from(*version));
        metrics.insert(
            "scan_store_limit_bytes".to_owned(),
            as_metric(*scan_store_limit_bytes),
        );
    }
    if let Some(bytes) = available {
        metrics.insert("available_bytes".to_owned(), as_metric(bytes));
    }
    let mut notes = vec![describe_report(&report)];
    notes.extend(
        run.residue
            .iter()
            .map(|entry| format!("left behind: {entry}")),
    );
    let observation = OracleObservation {
        ended: finished.ended,
        timed_out: finished.timed_out,
        wall: finished.wall,
        residue: run.residue.clone(),
        report,
        available_bytes: available,
    };
    let mut record = Recorded::new(check, Some(fixture_id), Some(Profile::Default));
    record.metrics = metrics;
    record.notes = notes;
    record.evidence = format!(
        "{}, {:.2} s; standard output {} bytes\n{evidence}",
        finished.ended.describe(),
        finished.wall.as_secs_f64(),
        finished.stdout.bytes
    );
    Checked {
        observation: Some(observation),
        record,
    }
}

/// What reading the report of one scan came to, and what to show of it.
struct ReportReading {
    /// The report, read and held to the oracle, or why it could not be.
    report: ReportRead,
    /// The first discrepancies of the diff, one to a line.
    evidence: String,
    /// How many discrepancies of each kind, named as metrics.
    metrics: BTreeMap<String, f64>,
}

/// Reads the report that `run` wrote and holds it to `oracle`.
fn read_report(
    run: &ScanRun,
    fixture: &FixtureRoot,
    oracle: &Oracle,
    timeout: Duration,
) -> ReportReading {
    let finished = &run.finished;
    let mut evidence = String::new();
    let mut metrics = BTreeMap::new();
    let report = if finished.timed_out {
        ReportRead::Missing(format!(
            "the scan did not end within {:.0} s and was killed",
            timeout.as_secs_f64()
        ))
    } else {
        let path = run.report_path();
        match ScanDocument::read(&path) {
            Ok(document) => {
                let found = diff(
                    oracle,
                    &Scan {
                        root: &path_bytes(fixture.path()),
                        document: &document,
                        exit_code: finished.ended.code(),
                    },
                );
                for (index, discrepancy) in found.discrepancies.iter().take(LISTED).enumerate() {
                    if index == 0 {
                        evidence.push_str("discrepancies (first of each kind):\n");
                    }
                    let _ = writeln!(evidence, "  {discrepancy}");
                }
                for (kind, count) in &found.counts {
                    metrics.insert(format!("discrepancies_{kind}"), as_metric(*count));
                }
                ReportRead::Read {
                    version: ScanDocument::version_of(&path).unwrap_or(0),
                    state: document.state,
                    kinds: found.counts,
                    scan_store_limit_bytes: document.summary.scan_store_limit_bytes,
                }
            }
            Err(DocumentError::Io { .. }) => ReportRead::Missing(format!(
                "the process {} and wrote no report: {}",
                finished.ended.describe(),
                finished.stderr.first_line()
            )),
            Err(error) => ReportRead::Invalid(error.to_string()),
        }
    };
    ReportReading {
        report,
        evidence,
        metrics,
    }
}

/// What the report came to, in the line the record keeps.
fn describe_report(report: &ReportRead) -> String {
    match report {
        ReportRead::Read {
            version,
            state,
            kinds,
            ..
        } => format!(
            "report version {version}, state {state}; discrepancies: {}",
            if kinds.is_empty() {
                "none".to_owned()
            } else {
                kinds
                    .iter()
                    .map(|(kind, count)| format!("{kind} x{count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
        ReportRead::Missing(why) | ReportRead::Invalid(why) => why.clone(),
    }
}

/// How the length of the report file was seen to change while the process ran.
///
/// The report is written after the scan (see [`TimedScan`]), so the first change in its length
/// marks the end of the scan, and the last the end of the write. One change says only that the
/// report appeared: it was written whole between two reads of its length, or it was whole at the
/// read made once the process had exited. How long the write took is not known then, and it is
/// not a write of no time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportGrowth {
    /// The length never changed from 0: no byte of the report had been written, so the scan was
    /// still going.
    Unseen,
    /// The length changed once: the scan had ended, and how long writing the report took is not
    /// known.
    Once,
    /// The length changed at least twice: the time from the first change to the last, as the
    /// report grew.
    Span(Duration),
}

impl ReportGrowth {
    /// How long writing the report took: known only when its length was seen to change at least
    /// twice.
    fn span(self) -> Option<Duration> {
        match self {
            Self::Span(span) => Some(span),
            Self::Unseen | Self::Once => None,
        }
    }
}

/// One timed scan.
///
/// The report is written after the scan. A published release creates the `--output` file only
/// once its scan has ended: `src/cli.rs` of every tag from v1.0.0 to v1.3.0, and of the candidate,
/// runs the headless scan and only then calls `write_scan_report`, which creates the file. So the
/// first byte of the report marks the end of the scan, and everything up to its last byte is the
/// writing of the report. The sweep sees that from outside: a thread reads the size of the file
/// every half millisecond while the process runs, and once more after it has exited, and notes
/// when the size changed (see [`ReportGrowth`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimedScan {
    /// From the spawn to the exit.
    pub wall: Duration,
    /// How the report file was seen to grow. How long writing the report took is known, to within
    /// the half millisecond of the poll, only from a growth seen at least twice.
    pub growth: ReportGrowth,
    /// Whether the scan ended within the bound, with an exit code of 0, 2 or 3.
    pub completed: bool,
}

impl TimedScan {
    /// How long the report took to write: known when the scan finished and the file was seen to
    /// grow, which takes two changes in its length. A report that was seen to change once, or
    /// never, says nothing about how long it took, and is not a write of no time.
    pub(crate) fn report_write(&self) -> Option<Duration> {
        self.growth.span().filter(|_| self.completed)
    }

    /// The wall time less the time the report took to write: the scan, with what the process does
    /// before and after it. `None` when the two cannot be told apart in this run: the scan did not
    /// finish, the file was not seen to grow, or the write took the whole run and left nothing
    /// to be the scan.
    pub(crate) fn without_report(&self) -> Option<Duration> {
        self.wall
            .checked_sub(self.report_write()?)
            .filter(|scan| *scan >= MIN_SCAN)
    }

    /// What the run measured of each series a headless scan feeds. A series the run has no usable
    /// reading of is `None`: it is a round without a sample, never a reading of 0 ms.
    ///
    /// A scan that did not finish is flagged on each series, which is then the least it took: how
    /// long the run lasted, and how long its report had taken to write by then. Its time without
    /// the report is known only when no byte of the report was written, so that the scan was still
    /// going; a run that was killed or failed after it began to write says nothing about its scan.
    pub(crate) fn readings(&self) -> RunValue {
        let millis = |duration: Duration| duration.as_secs_f64() * 1000.0;
        let wall = Reading::new(millis(self.wall), self.completed);
        if self.completed {
            RunValue::from([
                (HEADLESS_WALL, Some(wall)),
                (
                    REPORT_WRITE,
                    self.report_write()
                        .map(|span| Reading::new(millis(span), true)),
                ),
                (
                    HEADLESS_SCAN,
                    self.without_report()
                        .map(|scan| Reading::new(millis(scan), true)),
                ),
            ])
        } else {
            RunValue::from([
                (HEADLESS_WALL, Some(wall)),
                (
                    REPORT_WRITE,
                    Some(Reading::new(self.growth.span().map_or(0.0, millis), false)),
                ),
                (
                    HEADLESS_SCAN,
                    (self.growth == ReportGrowth::Unseen).then_some(wall),
                ),
            ])
        }
    }
}

/// Reads the length of the report file until the process has exited, and says how it changed.
///
/// `exited` is asked before each read, not after it, so the last read is made with the process
/// gone: a report that grows after one read and before the exit is read as it ended up, and its
/// last change is not left out. `pause` is what the poll does between two reads.
fn watch_growth(
    mut exited: impl FnMut() -> bool,
    mut length: impl FnMut() -> Option<u64>,
    mut pause: impl FnMut(),
) -> ReportGrowth {
    let mut watch = GrowthWatch::default();
    loop {
        let over = exited();
        if let Some(length) = length() {
            watch.read(length, Instant::now());
        }
        if over {
            return watch.growth();
        }
        pause();
    }
}

/// What the poll has seen of the length of the report file.
#[derive(Debug, Default)]
struct GrowthWatch {
    /// The length at the last read.
    length: u64,
    /// When the length first changed.
    first: Option<Instant>,
    /// When the length last changed, from its second change on.
    last: Option<Instant>,
}

impl GrowthWatch {
    /// Notes that the file was `length` bytes long at `at`.
    fn read(&mut self, length: u64, at: Instant) {
        if length == self.length {
            return;
        }
        self.length = length;
        if self.first.is_none() {
            self.first = Some(at);
        } else {
            self.last = Some(at);
        }
    }

    /// How the length changed over the reads so far.
    fn growth(&self) -> ReportGrowth {
        match (self.first, self.last) {
            (None, _) => ReportGrowth::Unseen,
            (Some(_), None) => ReportGrowth::Once,
            (Some(first), Some(last)) => ReportGrowth::Span(last.saturating_duration_since(first)),
        }
    }
}

/// Scans `fixture` with the build at `binary` and times it, and the writing of its report.
///
/// # Errors
///
/// Returns why the scan could not be run at all.
pub(crate) fn timed_scan(
    binary: &Path,
    fixture: &FixtureRoot,
    work_dir: &Path,
    timeout: Duration,
) -> Result<TimedScan, String> {
    timed_scan_every(POLL, binary, fixture, work_dir, timeout)
}

/// [`timed_scan`] with the length of the report read every `poll`.
fn timed_scan_every(
    poll: Duration,
    binary: &Path,
    fixture: &FixtureRoot,
    work_dir: &Path,
    timeout: Duration,
) -> Result<TimedScan, String> {
    let program = resolve_binary(binary).map_err(|error| error.to_string())?;
    let scratch = Scratch::create(work_dir).map_err(|error| error.to_string())?;
    let report = scratch.report();
    let mut command = Command::new(&program);
    command
        .arg("--format")
        .arg("json")
        .arg("--output")
        .arg(&report)
        .arg(fixture.path())
        .env_clear()
        .envs(isolated_env(&scratch, Profile::Default, false, None))
        .current_dir(scratch.cwd());

    let done = AtomicBool::new(false);
    let (finished, growth) = thread::scope(|scope| {
        let poller = scope.spawn(|| {
            watch_growth(
                || done.load(Ordering::Acquire),
                || fs::metadata(&report).map(|metadata| metadata.len()).ok(),
                || thread::sleep(poll),
            )
        });
        let finished = process::run(&mut command, timeout, false, false);
        done.store(true, Ordering::Release);
        (finished, poller.join().unwrap_or(ReportGrowth::Unseen))
    });
    let finished = finished.map_err(|error| error.to_string())?;
    let completed = !finished.timed_out && matches!(finished.ended.code(), Some(0 | 2 | 3));
    Ok(TimedScan {
        wall: finished.wall,
        growth,
        completed,
    })
}

/// Times `du -sk` of `fixture`, in milliseconds.
pub(crate) fn timed_du(
    du: &Du,
    fixture: &FixtureRoot,
    work_dir: &Path,
    timeout: Duration,
) -> Option<f64> {
    let run = du.run(fixture.path(), work_dir, timeout).ok()?;
    (run.finished.ended.code() == Some(0)).then_some(run.finished.wall.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn scan(wall_ms: u64, write_ms: Option<u64>, completed: bool) -> TimedScan {
        TimedScan {
            wall: Duration::from_millis(wall_ms),
            growth: write_ms.map_or(ReportGrowth::Unseen, |write| {
                ReportGrowth::Span(Duration::from_millis(write))
            }),
            completed,
        }
    }

    /// A scan whose report was seen to change in length once.
    fn scan_seen_once(wall_ms: u64, completed: bool) -> TimedScan {
        TimedScan {
            wall: Duration::from_millis(wall_ms),
            growth: ReportGrowth::Once,
            completed,
        }
    }

    fn reading(value: &RunValue, key: &str) -> Option<(f64, bool)> {
        value
            .get(key)
            .copied()
            .flatten()
            .map(|reading| (reading.ms, reading.completed))
    }

    #[test]
    fn a_scan_without_its_report_is_the_wall_time_less_the_time_the_report_took() {
        let value = scan(4900, Some(1300), true).readings();

        assert_eq!(reading(&value, HEADLESS_WALL), Some((4900.0, true)));
        assert_eq!(reading(&value, REPORT_WRITE), Some((1300.0, true)));
        assert_eq!(reading(&value, HEADLESS_SCAN), Some((3600.0, true)));
    }

    #[test]
    fn a_report_that_writes_in_a_few_milliseconds_hardly_changes_the_time() {
        let value = scan(154, Some(7), true).readings();

        assert_eq!(reading(&value, HEADLESS_WALL), Some((154.0, true)));
        assert_eq!(reading(&value, HEADLESS_SCAN), Some((147.0, true)));
    }

    #[test]
    fn a_report_never_seen_to_grow_leaves_no_separation_and_is_not_a_write_of_nothing() {
        let value = scan(160, None, true).readings();

        assert_eq!(reading(&value, HEADLESS_WALL), Some((160.0, true)));
        assert_eq!(
            value[REPORT_WRITE], None,
            "no write seen is not a write of 0 ms"
        );
        assert_eq!(
            value[HEADLESS_SCAN], None,
            "and the scan cannot be told from it"
        );
    }

    #[test]
    fn a_write_that_took_the_whole_run_leaves_nothing_to_be_the_scan() {
        for write in [100, 101] {
            let value = scan(100, Some(write), true).readings();

            assert_eq!(
                value[HEADLESS_SCAN], None,
                "a write of {write} ms in a run of 100"
            );
            assert!(value[REPORT_WRITE].is_some(), "the write itself was seen");
        }
    }

    #[test]
    fn a_scan_that_did_not_finish_is_censored_and_its_own_time_is_known_only_before_the_report() {
        let never_wrote = scan(120_000, None, false).readings();
        assert_eq!(
            reading(&never_wrote, HEADLESS_WALL),
            Some((120_000.0, false))
        );
        assert_eq!(
            reading(&never_wrote, HEADLESS_SCAN),
            Some((120_000.0, false))
        );

        let killed_while_writing = scan(120_000, Some(5000), false).readings();
        assert_eq!(
            reading(&killed_while_writing, REPORT_WRITE),
            Some((5000.0, false))
        );
        assert_eq!(killed_while_writing[HEADLESS_SCAN], None);
    }

    #[test]
    fn a_report_seen_to_change_once_is_not_a_write_of_nothing_either() {
        // It appeared whole between two reads of its length.
        let value = scan_seen_once(160, true).readings();

        assert_eq!(reading(&value, HEADLESS_WALL), Some((160.0, true)));
        assert_eq!(
            value[REPORT_WRITE], None,
            "one change is not a write of 0 ms"
        );
        assert_eq!(
            value[HEADLESS_SCAN], None,
            "and the scan cannot be told from the write"
        );
    }

    #[test]
    fn a_scan_killed_after_its_whole_report_appeared_says_nothing_of_its_own_time() {
        let value = scan_seen_once(120_000, false).readings();

        assert_eq!(reading(&value, HEADLESS_WALL), Some((120_000.0, false)));
        assert_eq!(reading(&value, REPORT_WRITE), Some((0.0, false)));
        assert_eq!(
            value[HEADLESS_SCAN], None,
            "the scan had ended before the report appeared, so the length of the run is not the \
             least it took"
        );
    }

    /// What the poll makes of a file whose length is read as `lengths`, one read each
    /// millisecond.
    fn read_lengths(lengths: &[u64]) -> ReportGrowth {
        let start = Instant::now();
        let mut watch = GrowthWatch::default();
        for (millis, length) in (0_u64..).zip(lengths) {
            watch.read(*length, start + Duration::from_millis(millis));
        }
        watch.growth()
    }

    #[test]
    fn a_length_that_never_changed_was_never_seen_to_grow() {
        assert_eq!(read_lengths(&[]), ReportGrowth::Unseen);
        assert_eq!(read_lengths(&[0, 0, 0]), ReportGrowth::Unseen);
    }

    #[test]
    fn a_length_that_changed_once_has_no_span_however_long_it_then_stays() {
        assert_eq!(read_lengths(&[0, 0, 300]), ReportGrowth::Once);
        assert_eq!(
            read_lengths(&[0, 300, 300, 300, 300, 300]),
            ReportGrowth::Once
        );
    }

    #[test]
    fn a_length_that_changed_twice_has_the_span_between_the_changes() {
        // It changes at the reads of 2 ms and of 4 ms; a read that finds the length as it was
        // is not a change.
        assert_eq!(
            read_lengths(&[0, 0, 10, 10, 25, 25, 25]),
            ReportGrowth::Span(Duration::from_millis(2))
        );
    }

    #[test]
    fn the_span_runs_from_the_first_change_to_the_last_of_many() {
        assert_eq!(
            read_lengths(&[0, 10, 25, 25, 25, 60, 60]),
            ReportGrowth::Span(Duration::from_millis(4))
        );
    }

    /// The poll against a script: for each pause of the poll, the length of the report file
    /// (`None` before it exists) and whether the process has exited. The last step is one in which
    /// it has, so a poll that pauses past it did not stop when the process was gone.
    struct Script {
        steps: Vec<(Option<u64>, bool)>,
        at: Cell<usize>,
        reads: Cell<usize>,
    }

    impl Script {
        fn new(steps: &[(Option<u64>, bool)]) -> Self {
            assert!(
                steps.last().is_some_and(|(_, exited)| *exited),
                "a script ends with the process gone"
            );
            Self {
                steps: steps.to_vec(),
                at: Cell::new(0),
                reads: Cell::new(0),
            }
        }

        fn watch(&self) -> ReportGrowth {
            watch_growth(
                || self.steps[self.at.get()].1,
                || {
                    self.reads.set(self.reads.get() + 1);
                    self.steps[self.at.get()].0
                },
                || {
                    assert!(
                        self.at.get() + 1 < self.steps.len(),
                        "the poll went on after the process had exited"
                    );
                    self.at.set(self.at.get() + 1);
                },
            )
        }
    }

    #[test]
    fn a_growth_after_the_last_read_and_before_the_exit_is_read_once_the_process_has_exited() {
        // The report is 100 bytes long at the first read and grows to 250 as the process exits,
        // so `exited` is already true when it is next asked.
        let script = Script::new(&[(Some(100), false), (Some(250), true)]);

        assert!(matches!(script.watch(), ReportGrowth::Span(_)));
        assert_eq!(
            script.reads.get(),
            2,
            "the length is read once more after the exit is seen"
        );
    }

    #[test]
    fn a_report_that_appears_as_the_process_exits_is_read_once_the_process_has_exited() {
        let script = Script::new(&[(None, false), (Some(300), true)]);

        assert_eq!(script.watch(), ReportGrowth::Once);
    }

    #[test]
    fn a_process_that_has_exited_by_the_first_look_has_its_report_read_once() {
        let script = Script::new(&[(Some(300), true)]);

        assert_eq!(script.watch(), ReportGrowth::Once);
        assert_eq!(script.reads.get(), 1);
    }

    #[test]
    fn a_report_written_whole_between_two_reads_is_seen_once() {
        let script = Script::new(&[
            (None, false),
            (None, false),
            (Some(300), false),
            (Some(300), false),
            (Some(300), true),
        ]);

        assert_eq!(script.watch(), ReportGrowth::Once);
    }

    #[test]
    fn a_report_that_never_held_a_byte_is_unseen() {
        let script = Script::new(&[(None, false), (Some(0), false), (None, true)]);

        assert_eq!(script.watch(), ReportGrowth::Unseen);
    }

    #[test]
    fn a_report_seen_to_grow_over_several_reads_has_a_span() {
        let script = Script::new(&[
            (None, false),
            (Some(40), false),
            (Some(90), false),
            (Some(90), true),
        ]);

        assert!(matches!(script.watch(), ReportGrowth::Span(_)));
    }

    /// What `timed_scan_every` makes of a stand-in for `excise` that runs `script`, which finds
    /// the path of its report in `$4`, with the length of the report read every `poll`.
    #[cfg(unix)]
    fn timed_every(poll: Duration, script: &str) -> TimedScan {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("a directory");
        let program = dir.path().join("excise");
        fs::write(&program, format!("#!/bin/sh\n{script}\n")).expect("a stand-in program");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).expect("executable");
        let root = dir.path().join("fixture");
        fs::create_dir(&root).expect("a fixture directory");
        fs::write(root.join(crate::fixture::MARKER_FILE_NAME), b"owned").expect("the marker");
        let fixture = FixtureRoot::open(&root).expect("an owned fixture");
        let work = dir.path().join("work");
        fs::create_dir(&work).expect("a work directory");

        timed_scan_every(poll, &program, &fixture, &work, Duration::from_secs(60))
            .expect("the stand-in runs")
    }

    /// What `timed_scan` makes of the same stand-in.
    #[cfg(unix)]
    fn timed(script: &str) -> TimedScan {
        timed_every(POLL, script)
    }

    #[cfg(unix)]
    #[test]
    fn a_process_that_writes_its_report_whole_and_lingers_gives_no_write_time() {
        let scan = timed("printf '{\"schema_version\":1}' > \"$4\"\nsleep 0.3");

        assert!(scan.completed, "{scan:?}");
        assert_eq!(scan.growth, ReportGrowth::Once, "{scan:?}");
        let value = scan.readings();
        assert_eq!(
            value[REPORT_WRITE], None,
            "a report that appeared whole is not a write of 0 ms"
        );
        assert_eq!(value[HEADLESS_SCAN], None);
        assert!(value[HEADLESS_WALL].is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_report_that_is_whole_only_as_the_process_exits_is_read_once_it_has_exited() {
        // The length is read every 300 ms, and the stand-in writes its whole report in one go and
        // exits within moments of its start. When the scheduler is quick, the poll is asleep for
        // the write and for the exit, and the only read that finds the report is the one made
        // after the exit: a poll that stopped on finding the process gone, without a read after
        // that, would find nothing. When the scheduler is slow, the poll finds the report as the
        // process runs or exits. Either way the report is whole at the last read and its length
        // changed once, from nothing: it is never unseen, and, written in one `write`, it never
        // has a span. No bound on a time depends on the scheduler.
        let scan = timed_every(
            Duration::from_millis(300),
            "printf '{\"schema_version\":1}' > \"$4\"",
        );

        assert!(scan.completed, "{scan:?}");
        assert_eq!(scan.growth, ReportGrowth::Once, "{scan:?}");
    }
}
