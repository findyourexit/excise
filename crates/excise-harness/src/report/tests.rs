//! Report documents: the schemas compile, match the Rust constants and types, and reject drift.

use std::collections::{BTreeMap, BTreeSet};

use jsonschema::Validator;
use regex::Regex;
use serde_json::Value;

use super::{
    AbContext, AbFixture, AbKind, AbVerdict, AccountingHeadline, BinaryIdentity, BuildIdentity,
    BuildStatus, CellState, CheckStatus, ConfidenceInterval, CountsCase, CountsContext,
    CountsFixture, CountsInvalid, CountsKind, CountsRunner, Document, FailedStep, FailureKind,
    FixtureIdentity, HarnessAb, HarnessCounts, HarnessFailure, HarnessSoak, HarnessSummary,
    HarnessSweep, HarnessTui, MAX_CASES, MAX_COUNT, MetricComparison, PullRequestOrigin, QuirkKind,
    Rusage, SCHEMA_VERSION, Samples, ScenarioResult, SchemaVersion, ScreenComparison,
    SessionDiagnostics, Side, SoakAccounting, SoakExit, SoakHeadless, SoakKind, SoakLimits,
    SoakOutcome, SoakPhase, SoakQuirkCount, SoakReportFacts, SoakRounds, SoakScanSummary, SoakTui,
    SummaryKind, SweepBuild, SweepCell, SweepCheck, SweepContext, SweepInvalid, SweepKind,
    SweepMeasurement, SweepRatio, SweepRow, SweepSeries, SweepTier, SweepToolchain, SweepTraits,
    SweepVersion, TerminalModes, Tier, TimingWarning, Verdict,
    tui::{
        BoxInfo, Cleanup, CloseResult, ConfirmationKind, Cursor, DeleteDialogInfo,
        DeleteDialogKind, DeleteResult, DialogInfo, EventRecord, EventRecordKind, EventsPage,
        EventsResult, ExitInfo, ExitVia, FilterInfo, FixtureChanges, FrameInfo, InputCounts,
        KeysResult, ListResult, Modes, OpenResult, Rect, RefreshOutcomeKind, ScreenInfo,
        ScreenResult, SelectedInfo, SentKey, SessionInfo, SessionState, Size, StaleSession,
        TuiCommand, TuiError, TuiErrorKind, TuiResult,
    },
};
use super::{
    HarnessShapeProfile, MAX_PROFILE_DEPTH, ShapeDepth, ShapeEntries, ShapeHardLinks,
    ShapeHistogram, ShapeNameLengths, ShapePlatform, ShapeProfileKind, ShapeSymbolicLinks,
    ShapeUnreadable, ShapeWalk,
};
use crate::{
    headless::document::ScanState,
    runner::is_latency,
    scenario::{Budget, EntryKind, Expect, Profile},
};

const SHA256: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
const GIT_SHA: &str = "37d1c18a5f0c2b9e4d1a7c3b8e6f2a90d4c5b1e7";
const FIXTURE_HASH: &str = "3a7bd3e2360a3d29eea436fcfb7e44c735d117c42d1c1835420b6b9942dd4f1b";
const SCHEMA_ID_PREFIX: &str = "https://github.com/findyourexit/excise/harness/schemas/";

/// A summary with every optional field present.
fn summary() -> HarnessSummary {
    HarnessSummary {
        document_kind: SummaryKind::HarnessSummary,
        schema_version: SchemaVersion,
        run_id: "20260930T163000Z-3f9a".to_owned(),
        tier: Tier::Quick,
        started_at: "2026-09-30T16:30:00Z".to_owned(),
        finished_at: "2026-09-30T16:31:41.250+10:00".to_owned(),
        host: "ci-runner-7".to_owned(),
        os: "macos".to_owned(),
        arch: "aarch64".to_owned(),
        excise_binary: BinaryIdentity {
            path: "target/release/excise".to_owned(),
            sha256: SHA256.to_owned(),
        },
        git_sha: GIT_SHA.to_owned(),
        latency_budget_scale: Some(2.0),
        timing_informational: true,
        quick_tier_ms: Some(98_765),
        scenarios: vec![
            ScenarioResult {
                name: "delete-folder-lifecycle".to_owned(),
                profile: Profile::Default,
                verdict: Verdict::Pass,
                duration_ms: 4_180,
                metrics: BTreeMap::from([
                    ("max_rss_bytes".to_owned(), 20_971_520.0),
                    ("time_to_complete_ms".to_owned(), 3_720.5),
                ]),
                failure_bundle: None,
                timing_warnings: vec![
                    TimingWarning {
                        budget: Budget::MaxStallMs,
                        metric: "max_stall_ms".to_owned(),
                        value: 588.25,
                        limit: 500.0,
                    },
                    TimingWarning {
                        budget: Budget::InputToFrameP99Ms,
                        metric: "input_to_frame_p99_ms".to_owned(),
                        value: 61.5,
                        limit: 50.0,
                    },
                ],
            },
            ScenarioResult {
                name: "signal-term-mid-scan".to_owned(),
                profile: Profile::Deterministic,
                verdict: Verdict::Xfail,
                duration_ms: 2_100,
                metrics: BTreeMap::new(),
                failure_bundle: Some(
                    "target/excise-e2e/20260930T163000Z-3f9a/signal-term-mid-scan-deterministic"
                        .to_owned(),
                ),
                timing_warnings: Vec::new(),
            },
        ],
    }
}

/// A summary with every optional field absent.
fn minimal_summary() -> HarnessSummary {
    HarnessSummary {
        latency_budget_scale: None,
        timing_informational: false,
        quick_tier_ms: None,
        scenarios: Vec::new(),
        ..summary()
    }
}

fn failure() -> HarnessFailure {
    HarnessFailure {
        document_kind: FailureKind::HarnessFailure,
        schema_version: SchemaVersion,
        scenario: "delete-folder-lifecycle".to_owned(),
        profile: Profile::MonochromeAscii,
        failed_step: FailedStep {
            index: 2,
            description: "delete victim (folder)".to_owned(),
        },
        screen: ScreenComparison {
            expected: "dialog names victim/".to_owned(),
            actual: "Delete keep-a.bin?\n[y] confirm".to_owned(),
        },
        terminal_modes: TerminalModes {
            alternate_screen: true,
            cursor_visible: false,
            echo: false,
            icanon: false,
        },
        session_diagnostics: None,
        cast_path: "target/excise-e2e/run/delete-folder-lifecycle.cast".to_owned(),
        rusage: Rusage {
            max_rss_bytes: 20_971_520,
            user_ms: 1_340,
            sys_ms: 210,
        },
        fixture: FixtureIdentity {
            hash: FIXTURE_HASH.to_owned(),
            seed: 18_446_744_073_709_551_615,
        },
        repro_command:
            "cargo xtask e2e --scenario delete-folder-lifecycle --profile monochrome-ascii"
                .to_owned(),
    }
}

/// A failure whose cause was a step timing out, with the session's diagnostics recorded: the
/// counterpart to [`failure`], whose cause leaves `session_diagnostics` absent.
fn timeout_failure() -> HarnessFailure {
    HarnessFailure {
        failed_step: FailedStep {
            index: 0,
            description: "wait_header expects complete [Timeout]: not within 5000 ms; the \
                           header shows nothing yet"
                .to_owned(),
        },
        session_diagnostics: Some(SessionDiagnostics {
            output_bytes: 412,
            first_byte_after_ms: Some(18),
            child_running: true,
            cursor_reports_answered: 1,
            head: "booting up".to_owned(),
            tail: "still scanning".to_owned(),
        }),
        ..failure()
    }
}

fn ab() -> HarnessAb {
    HarnessAb {
        document_kind: AbKind::HarnessAb,
        schema_version: SchemaVersion,
        baseline: BuildIdentity {
            git_ref: "v1.3.0".to_owned(),
            binary_sha256: SHA256.to_owned(),
        },
        candidate: BuildIdentity {
            git_ref: "findyourexit/x1-drop-flushes".to_owned(),
            binary_sha256: SHA256.replace('9', "a"),
        },
        trials: 2,
        interleaving: vec![
            Side::Baseline,
            Side::Candidate,
            Side::Candidate,
            Side::Baseline,
        ],
        metrics: vec![MetricComparison {
            name: "delete-folder-lifecycle-default__time_to_complete_ms".to_owned(),
            samples: Samples {
                baseline: vec![3_900.0, 3_850.5],
                candidate: vec![1_100.0, 1_050.25],
            },
            median_ratio: 0.2789,
            bootstrap_ci: ConfidenceInterval {
                lower: 0.27,
                upper: 0.29,
                confidence: 0.95,
            },
            verdict: AbVerdict::Pass,
        }],
        context: AbContext {
            host: "macbook".to_owned(),
            cpu: "Apple M1 Pro".to_owned(),
            os: "macOS 26.0".to_owned(),
            arch: "aarch64".to_owned(),
            logical_cpus: 10,
            toolchain: "rustc 1.98.0".to_owned(),
            power: "ac".to_owned(),
            load_average_start: 1.2,
            load_average_end: 1.5,
            concurrent_excise_processes: 0,
            fixtures: vec![AbFixture {
                id: "delete-folder".to_owned(),
                hash: FIXTURE_HASH.to_owned(),
                seed: 7,
            }],
        },
    }
}

/// A counts document of a pull request, with every optional field present.
fn counts() -> HarnessCounts {
    HarnessCounts {
        document_kind: CountsKind::HarnessCounts,
        schema_version: SchemaVersion,
        context: CountsContext {
            git_sha: GIT_SHA.to_owned(),
            committed_at: "2026-10-05T10:35:18+11:00".to_owned(),
            runner: CountsRunner {
                os: "linux".to_owned(),
                os_version: "Ubuntu 24.04.3 LTS".to_owned(),
                arch: "x86_64".to_owned(),
            },
            toolchain: "rustc 1.98.0 (88d9e12ae 2026-08-18)".to_owned(),
            pull_request: Some(PullRequestOrigin {
                number: 123,
                base_sha: "c0ffee0123456789c0ffee0123456789c0ffee01".to_owned(),
                head_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            }),
        },
        cases: vec![
            CountsCase {
                fixture: CountsFixture {
                    id: "wide-1k".to_owned(),
                    hash: FIXTURE_HASH.to_owned(),
                    seed: 7,
                },
                profile: Profile::Deterministic,
                metrics: BTreeMap::from([
                    ("entries".to_owned(), 1_002),
                    ("residue_files".to_owned(), 0),
                    ("scan_store_bytes".to_owned(), 353_597),
                ]),
            },
            CountsCase {
                fixture: CountsFixture {
                    id: "tiny-files-50k".to_owned(),
                    hash: FIXTURE_HASH.replace('3', "4"),
                    seed: 50_000,
                },
                profile: Profile::Default,
                metrics: BTreeMap::from([("entries".to_owned(), 49_051)]),
            },
        ],
    }
}

/// A counts document of a commit on `main`: no pull request, one case.
fn minimal_counts() -> HarnessCounts {
    let mut document = counts();
    document.context.pull_request = None;
    document.cases.truncate(1);
    document
}

/// A sweep with every optional field present: an old version, one whose build failed, and the
/// candidate; a headless timing and an interface timing; a check of each status; and two rows
/// between them holding a cell of each state.
fn sweep() -> HarnessSweep {
    HarnessSweep {
        document_kind: SweepKind::HarnessSweep,
        schema_version: SchemaVersion,
        run_id: "20261006T091500Z-5c1e".to_owned(),
        tier: SweepTier::Full,
        started_at: "2026-10-06T09:15:00Z".to_owned(),
        finished_at: "2026-10-06T20:47:41.250+11:00".to_owned(),
        candidate: "HEAD".to_owned(),
        context: sweep_context(),
        versions: sweep_versions(),
        measurements: vec![headless_measurement(), tui_measurement()],
        checks: sweep_checks(),
        rows: sweep_rows(),
    }
}

fn sweep_context() -> SweepContext {
    SweepContext {
        host: "macbook".to_owned(),
        cpu: "Apple M1 Pro".to_owned(),
        os: "macOS 26.0".to_owned(),
        arch: "aarch64".to_owned(),
        logical_cpus: 10,
        power: "ac".to_owned(),
        load_average_start: 1.2,
        load_average_end: 1.5,
        concurrent_excise_processes: 0,
        checkout_sha: GIT_SHA.to_owned(),
        rounds: 3,
        seed: 24_301,
        drain_bytes_per_sec: 150_000,
        timeout_ms: 30_000,
        du: Some("/usr/bin/du".to_owned()),
        fixtures: vec![
            AbFixture {
                id: "node-modules-2k".to_owned(),
                hash: FIXTURE_HASH.to_owned(),
                seed: 7,
            },
            AbFixture {
                id: "tiny-files-50k".to_owned(),
                hash: FIXTURE_HASH.replace('3', "4"),
                seed: 50_000,
            },
        ],
    }
}

/// An old version that built under an older toolchain, a version whose build failed (so that it has
/// no binary and nothing was learned of one), and the candidate.
fn sweep_versions() -> Vec<SweepVersion> {
    vec![
        SweepVersion {
            reference: "v1.0.0".to_owned(),
            sha: "c0ffee0123456789c0ffee0123456789c0ffee01".to_owned(),
            toolchain: Some(SweepToolchain {
                channel: "1.88.0".to_owned(),
                rustc: "rustc 1.88.0 (6b00bc388 2025-06-23)".to_owned(),
            }),
            binary_sha256: Some(SHA256.replace('9', "a")),
            build: SweepBuild {
                status: BuildStatus::Built,
                cached: true,
                reason: None,
                log: None,
            },
            traits: Some(SweepTraits {
                report_version: Some(1),
                scan_store_dir: false,
            }),
        },
        SweepVersion {
            reference: "v1.1.0".to_owned(),
            sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            toolchain: None,
            binary_sha256: None,
            build: SweepBuild {
                status: BuildStatus::Failed,
                cached: false,
                reason: Some("cargo build exited with status 101".to_owned()),
                log: Some("builds/v1.1.0.log".to_owned()),
            },
            traits: None,
        },
        SweepVersion {
            reference: "HEAD".to_owned(),
            sha: GIT_SHA.to_owned(),
            toolchain: Some(SweepToolchain {
                channel: "1.98.0".to_owned(),
                rustc: "rustc 1.98.0 (88d9e12ae 2026-08-18)".to_owned(),
            }),
            binary_sha256: Some(SHA256.to_owned()),
            build: SweepBuild {
                status: BuildStatus::Built,
                cached: false,
                reason: None,
                log: None,
            },
            traits: Some(SweepTraits {
                report_version: Some(1),
                scan_store_dir: true,
            }),
        },
    ]
}

/// A ratio over three rounds in which both sides finished, with a 95% interval, and no round in
/// which a run did not finish.
fn sweep_ratio(median: f64, lower: f64, upper: f64) -> SweepRatio {
    SweepRatio {
        median: Some(median),
        bootstrap_ci: Some(ConfidenceInterval {
            lower,
            upper,
            confidence: 0.95,
        }),
        pairs: 3,
        lower_bounds: Vec::new(),
        upper_bounds: Vec::new(),
        both_censored: 0,
    }
}

/// The order of three rounds that ran `v1.0.0` and `HEAD` in turn, the second round reversed.
fn sweep_rounds() -> Vec<Vec<String>> {
    vec![
        vec!["v1.0.0".to_owned(), "HEAD".to_owned()],
        vec!["HEAD".to_owned(), "v1.0.0".to_owned()],
        vec!["v1.0.0".to_owned(), "HEAD".to_owned()],
    ]
}

/// A scan timed against `du`, whose samples are present except for the round it could not be
/// timed in, which the `du` ratios leave out instead of shifting the rounds after it.
fn headless_measurement() -> SweepMeasurement {
    SweepMeasurement {
        metric: "headless_wall_ms".to_owned(),
        fixture: "node-modules-2k".to_owned(),
        profile: None,
        drain_bytes_per_sec: None,
        rounds: 3,
        order: sweep_rounds(),
        du_samples: vec![Some(40.0), None, Some(50.0)],
        series: vec![
            SweepSeries {
                reference: "v1.0.0".to_owned(),
                rounds: vec![0, 1, 2],
                samples: vec![120.0, 100.0, 110.0],
                completed: vec![true, true, true],
                skipped: 0,
                median: Some(110.0),
                ratios: BTreeMap::from([
                    ("candidate".to_owned(), sweep_ratio(1.22, 1.0, 1.5)),
                    (
                        "du".to_owned(),
                        SweepRatio {
                            pairs: 2,
                            ..sweep_ratio(2.6, 2.2, 3.0)
                        },
                    ),
                ]),
            },
            SweepSeries {
                reference: "HEAD".to_owned(),
                rounds: vec![0, 1, 2],
                samples: vec![80.0, 100.0, 90.0],
                completed: vec![true, true, true],
                skipped: 0,
                median: Some(90.0),
                ratios: BTreeMap::from([
                    ("candidate".to_owned(), sweep_ratio(1.0, 1.0, 1.0)),
                    (
                        "du".to_owned(),
                        SweepRatio {
                            pairs: 2,
                            ..sweep_ratio(1.9, 1.8, 2.0)
                        },
                    ),
                ]),
            },
        ],
    }
}

/// An interface run against a slow terminal under a profile, in which `v1.0.0` ran out of time in
/// its second round and was then given up on, so that it has no sample for the third.
fn tui_measurement() -> SweepMeasurement {
    SweepMeasurement {
        metric: "tui_complete_ms".to_owned(),
        fixture: "node-modules-2k".to_owned(),
        profile: Some(Profile::Default),
        drain_bytes_per_sec: Some(150_000),
        rounds: 3,
        order: vec![
            vec!["v1.0.0".to_owned(), "HEAD".to_owned()],
            vec!["HEAD".to_owned(), "v1.0.0".to_owned()],
            vec!["HEAD".to_owned()],
        ],
        du_samples: Vec::new(),
        series: vec![
            SweepSeries {
                reference: "v1.0.0".to_owned(),
                rounds: vec![0, 1],
                samples: vec![1_800.0, 30_000.0],
                completed: vec![true, false],
                skipped: 1,
                median: Some(1_800.0),
                ratios: BTreeMap::from([
                    (
                        "candidate".to_owned(),
                        SweepRatio {
                            pairs: 1,
                            lower_bounds: vec![30.0],
                            ..sweep_ratio(2.0, 2.0, 2.0)
                        },
                    ),
                    (
                        "reduced-motion".to_owned(),
                        SweepRatio {
                            pairs: 1,
                            lower_bounds: vec![54.5],
                            ..sweep_ratio(3.0, 3.0, 3.0)
                        },
                    ),
                ]),
            },
            SweepSeries {
                reference: "HEAD".to_owned(),
                rounds: vec![0, 1, 2],
                samples: vec![900.0, 1_000.0, 950.0],
                completed: vec![true, true, true],
                skipped: 0,
                median: Some(950.0),
                ratios: BTreeMap::from([
                    ("candidate".to_owned(), sweep_ratio(1.0, 1.0, 1.0)),
                    (
                        "reduced-motion".to_owned(),
                        sweep_ratio(1.118, 1.111, 1.125),
                    ),
                ]),
            },
        ],
    }
}

/// A check of each status: two that ran, one that could not run on a version with no binary, and
/// one that the harness could not carry out.
fn sweep_checks() -> Vec<SweepCheck> {
    vec![
        SweepCheck {
            reference: "v1.0.0".to_owned(),
            check: "signal-term".to_owned(),
            fixture: Some("tiny-files-50k".to_owned()),
            profile: Some(Profile::Deterministic),
            status: CheckStatus::Ran,
            reason: None,
            metrics: BTreeMap::from([
                ("exit_code".to_owned(), 143.0),
                ("residue_files".to_owned(), 3.0),
            ]),
            notes: vec![
                "ended on SIGTERM with no report".to_owned(),
                "left the scan store in the temporary directory".to_owned(),
            ],
            evidence: Some("evidence/v1.0.0/signal-term.json".to_owned()),
        },
        SweepCheck {
            reference: "v1.1.0".to_owned(),
            check: "headless-oracle".to_owned(),
            fixture: None,
            profile: None,
            status: CheckStatus::NotRun,
            reason: Some("the build failed, so there is no binary".to_owned()),
            metrics: BTreeMap::new(),
            notes: Vec::new(),
            evidence: None,
        },
        SweepCheck {
            reference: "HEAD".to_owned(),
            check: "signal-term".to_owned(),
            fixture: Some("tiny-files-50k".to_owned()),
            profile: Some(Profile::Deterministic),
            status: CheckStatus::Ran,
            reason: None,
            metrics: BTreeMap::from([
                ("exit_code".to_owned(), 143.0),
                ("residue_files".to_owned(), 0.0),
            ]),
            notes: vec!["ended on SIGTERM after a partial report".to_owned()],
            evidence: Some("evidence/HEAD/signal-term.json".to_owned()),
        },
        SweepCheck {
            reference: "HEAD".to_owned(),
            check: "descriptors".to_owned(),
            fixture: Some("tiny-files-50k".to_owned()),
            profile: Some(Profile::Default),
            status: CheckStatus::Errored,
            reason: Some("lsof is not installed".to_owned()),
            metrics: BTreeMap::new(),
            notes: Vec::new(),
            evidence: Some("evidence/HEAD/descriptors.txt".to_owned()),
        },
    ]
}

/// A row decided by the timings and one decided by a check, each with a cell of every state.
fn sweep_rows() -> Vec<SweepRow> {
    vec![
        SweepRow {
            defect: "F1".to_owned(),
            title: "Scanning a large folder is slow".to_owned(),
            changelog: "Scanning a large folder is now much faster".to_owned(),
            measured_by: "headless_wall_ms".to_owned(),
            cells: vec![
                SweepCell {
                    reference: "v1.0.0".to_owned(),
                    state: CellState::Affected,
                    reason: Some("the median is above 1.2x the candidate".to_owned()),
                    value: Some("median 1.22x the candidate (95% CI 1.0-1.5)".to_owned()),
                    evidence: vec!["#/measurements/0".to_owned()],
                },
                SweepCell {
                    reference: "v1.1.0".to_owned(),
                    state: CellState::NotMeasurable,
                    reason: Some("the build failed, so there is no binary".to_owned()),
                    value: None,
                    evidence: vec!["builds/v1.1.0.log".to_owned()],
                },
                SweepCell {
                    reference: "HEAD".to_owned(),
                    state: CellState::NotAffected,
                    reason: Some("the candidate is the reference".to_owned()),
                    value: Some("median 1.0x the candidate".to_owned()),
                    evidence: vec!["#/measurements/0".to_owned()],
                },
            ],
            note: Some("the candidate is not-affected by construction".to_owned()),
        },
        SweepRow {
            defect: "F10".to_owned(),
            title: "SIGTERM leaves the scan store behind".to_owned(),
            changelog: "Closing the terminal, or sending SIGTERM".to_owned(),
            measured_by: "signal-term".to_owned(),
            cells: vec![
                SweepCell {
                    reference: "v1.0.0".to_owned(),
                    state: CellState::Affected,
                    reason: None,
                    value: None,
                    evidence: vec!["#/checks/0".to_owned()],
                },
                SweepCell {
                    reference: "v1.1.0".to_owned(),
                    state: CellState::NotMeasurable,
                    reason: Some("the build failed, so there is no binary".to_owned()),
                    value: None,
                    evidence: vec!["#/checks/1".to_owned()],
                },
                SweepCell {
                    reference: "HEAD".to_owned(),
                    state: CellState::NotAffected,
                    reason: None,
                    value: None,
                    evidence: vec!["#/checks/2".to_owned()],
                },
            ],
            note: None,
        },
    ]
}

/// A sweep with every optional field absent: one version that has no toolchain, digest, or traits,
/// and a row with one cell and no note, from a quick run that timed and checked nothing.
fn minimal_sweep() -> HarnessSweep {
    let mut document = sweep();
    document.tier = SweepTier::Quick;
    document.context.du = None;
    document.context.fixtures.clear();
    document.versions = vec![SweepVersion {
        reference: "HEAD".to_owned(),
        sha: GIT_SHA.to_owned(),
        toolchain: None,
        binary_sha256: None,
        build: SweepBuild {
            status: BuildStatus::Built,
            cached: false,
            reason: None,
            log: None,
        },
        traits: None,
    }];
    document.measurements.clear();
    document.checks.clear();
    document.rows.truncate(1);
    document.rows[0].note = None;
    document.rows[0].cells = vec![SweepCell {
        reference: "HEAD".to_owned(),
        state: CellState::NotAffected,
        reason: None,
        value: None,
        evidence: Vec::new(),
    }];
    document
}

/// What the report of a headless scan said, with every flag set that can be.
fn soak_facts() -> SoakReportFacts {
    SoakReportFacts {
        state: ScanState::Uncertain,
        accounting: SoakAccounting {
            headline: AccountingHeadline::IdentityUniqueAllocatedBytes,
            hard_links_deduplicated: true,
            shared_extents_deduplicated: false,
            directory_metadata_included: false,
        },
        summary: SoakScanSummary {
            scanned_entries: 1_204_331,
            identified_entries: 1_204_100,
            unreadable_entries: 3,
            unscanned_entries: 2,
            excluded_entries: 0,
            filesystem_boundaries: 1,
            link_entries: 5_212,
            deleted_entries: 0,
            deletion_changed_entries: 0,
            deletion_missing_entries: 0,
            deletion_failed_entries: 0,
            deletion_unattempted_entries: 0,
            scan_store_bytes: 402_653_184,
            scan_store_limit_bytes: 3_221_225_472,
            last_unreadable_path_present: true,
            last_unscanned_path_present: true,
            last_unscanned_reason_present: true,
            last_worker_error_present: false,
        },
    }
}

/// A soak with every optional field present, across its entries.
fn soak() -> HarnessSoak {
    let facts = soak_facts();
    HarnessSoak {
        document_kind: SoakKind::HarnessSoak,
        schema_version: SchemaVersion,
        run_id: "20261006T120000Z-4242".to_owned(),
        started_at: "2026-10-06T12:00:00.000Z".to_owned(),
        finished_at: "2026-10-06T12:07:31.250+11:00".to_owned(),
        os: "macos".to_owned(),
        arch: "aarch64".to_owned(),
        git_sha: GIT_SHA.to_owned(),
        excise_sha256: SHA256.to_owned(),
        outcome: SoakOutcome::Finished,
        rounds: SoakRounds {
            requested: 2,
            completed: 2,
        },
        limits: SoakLimits { run_ms: 1_800_000 },
        headless: vec![
            SoakHeadless {
                round: 1,
                profile: Profile::Default,
                exit_code: Some(2),
                signal: None,
                timed_out: false,
                wall_ms: 61_250.5,
                user_ms: Some(180_000.25),
                sys_ms: Some(42_500.0),
                peak_rss_bytes: Some(301_989_888),
                residue_files: 0,
                report: Some(facts),
            },
            SoakHeadless {
                round: 2,
                profile: Profile::Default,
                exit_code: None,
                signal: Some(9),
                timed_out: true,
                wall_ms: 900_000.0,
                user_ms: None,
                sys_ms: None,
                peak_rss_bytes: None,
                residue_files: 1,
                report: None,
            },
        ],
        tui: vec![
            SoakTui {
                round: 1,
                profile: Profile::Default,
                exit: SoakExit {
                    code: Some(0),
                    signal: None,
                    via: ExitVia::Quit,
                },
                timed_out_phase: None,
                terminal_restored: true,
                residue_files: 0,
                drilled: true,
                metrics: BTreeMap::from([
                    ("first_frame_ms".to_owned(), 41.5),
                    ("scan_complete_ms".to_owned(), 70_100.0),
                    ("complete_ms".to_owned(), 74_300.0),
                    ("input_samples".to_owned(), 60.0),
                    ("input_to_frame_p99_ms".to_owned(), 38.0),
                    ("max_stall_ms".to_owned(), 212.0),
                    ("peak_rss_bytes".to_owned(), 335_544_320.0),
                    ("drill_ms".to_owned(), 480.0),
                    ("up_ms".to_owned(), 120.0),
                    ("quit_ms".to_owned(), 90.0),
                ]),
            },
            SoakTui {
                round: 1,
                profile: Profile::Deterministic,
                exit: SoakExit {
                    code: None,
                    signal: Some(9),
                    via: ExitVia::Killed,
                },
                timed_out_phase: Some(SoakPhase::Complete),
                terminal_restored: false,
                residue_files: 3,
                drilled: false,
                metrics: BTreeMap::from([("first_frame_ms".to_owned(), 44.0)]),
            },
        ],
        quirks: vec![
            SoakQuirkCount {
                kind: QuirkKind::UncertainScan,
                count: 2,
            },
            SoakQuirkCount {
                kind: QuirkKind::Timeout,
                count: 1,
            },
        ],
    }
}

/// A soak that was interrupted before anything ran: no scans, no sessions, no quirks.
fn minimal_soak() -> HarnessSoak {
    let mut document = soak();
    document.outcome = SoakOutcome::Interrupted;
    document.rounds.completed = 0;
    document.headless.clear();
    document.tui.clear();
    document.quirks.clear();
    document
}

fn schema<D: Document>() -> Value {
    serde_json::from_str(D::SCHEMA_JSON).expect("the schema should be valid JSON")
}

/// A validator for `D`'s schema, optionally asserting `format` keywords.
fn validator<D: Document>(validate_formats: bool) -> Validator {
    jsonschema::draft202012::options()
        .should_validate_formats(validate_formats)
        .build(&schema::<D>())
        .expect("the schema should compile")
}

fn violations(validator: &Validator, instance: &Value) -> Vec<String> {
    validator
        .iter_errors(instance)
        .map(|error| error.to_string())
        .collect()
}

fn to_value<D: Document>(document: &D) -> Value {
    serde_json::to_value(document).expect("a document should serialize")
}

fn assert_schema_compiles<D: Document>() {
    let schema = schema::<D>();

    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema",
        "{} must declare draft 2020-12",
        D::KIND
    );
    jsonschema::draft202012::meta::validate(&schema)
        .unwrap_or_else(|error| panic!("{} is not a draft 2020-12 schema: {error}", D::KIND));
    validator::<D>(true);
}

fn assert_schema_identity<D: Document>() {
    let schema = schema::<D>();

    assert_eq!(schema["$id"], D::SCHEMA_ID);
    assert_eq!(
        D::SCHEMA_ID,
        format!("{SCHEMA_ID_PREFIX}{}-v{SCHEMA_VERSION}.json", D::KIND),
        "the $id follows <document_kind>-v<schema_version>"
    );
    assert_eq!(schema["properties"]["document_kind"]["const"], D::KIND);
    assert_eq!(
        schema["properties"]["schema_version"]["const"],
        SCHEMA_VERSION
    );
    for field in ["document_kind", "schema_version"] {
        assert!(
            schema["required"]
                .as_array()
                .expect("required is a list")
                .iter()
                .any(|name| name == field),
            "{} must require {field}",
            D::KIND
        );
    }
}

/// Every object schema that declares properties must reject undeclared ones.
fn assert_objects_are_closed(schema: &Value, at: &str) {
    if schema.get("properties").is_some() {
        assert_eq!(
            schema["additionalProperties"], false,
            "{at} declares properties and must set additionalProperties to false"
        );
    }
    let nested = schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .chain(
            schema
                .get("$defs")
                .and_then(Value::as_object)
                .into_iter()
                .flatten(),
        );
    for (name, child) in nested {
        assert_objects_are_closed(child, &format!("{at}/{name}"));
    }
}

/// Which declared properties an instance serialized, so a schema cannot drift from the types.
#[derive(Default)]
struct Coverage {
    declared: BTreeSet<String>,
    seen: BTreeSet<String>,
    definitions: BTreeSet<String>,
}

fn cover(root: &Value, schema: &Value, at: &str, instance: &Value, coverage: &mut Coverage) {
    let (schema, at) = match schema.get("$ref").and_then(Value::as_str) {
        Some(reference) => {
            let target = reference
                .strip_prefix('#')
                .expect("only local references are used");
            if let Some(definition) = target.strip_prefix("/$defs/") {
                coverage.definitions.insert(definition.to_owned());
            }
            (
                root.pointer(target).expect("a reference should resolve"),
                target.to_owned(),
            )
        }
        None => (schema, at.to_owned()),
    };
    if let (Some(properties), Some(object)) = (
        schema.get("properties").and_then(Value::as_object),
        instance.as_object(),
    ) {
        for (name, property) in properties {
            coverage.declared.insert(format!("{at}/{name}"));
            if let Some(value) = object.get(name) {
                coverage.seen.insert(format!("{at}/{name}"));
                cover(
                    root,
                    property,
                    &format!("{at}/properties/{name}"),
                    value,
                    coverage,
                );
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), instance.as_array()) {
        for value in array {
            cover(root, items, &format!("{at}/items"), value, coverage);
        }
    }
    if let (Some(values), Some(object)) = (
        schema
            .get("additionalProperties")
            .filter(|value| value.is_object()),
        instance.as_object(),
    ) {
        for value in object.values() {
            cover(
                root,
                values,
                &format!("{at}/additionalProperties"),
                value,
                coverage,
            );
        }
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        // Only the branch the instance takes: a null takes the `null` branch and exercises no
        // definition.
        for branch in branches {
            let is_null = branch.get("type").is_some_and(|kind| kind == "null");
            if is_null == instance.is_null() {
                cover(root, branch, &at, instance, coverage);
            }
        }
    }
    // The definition that names the keys of a map is used by every map that refers to it.
    if let Some(definition) = schema
        .get("propertyNames")
        .and_then(|names| names.get("$ref"))
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix("#/$defs/"))
    {
        coverage.definitions.insert(definition.to_owned());
    }
}

fn assert_schema_and_types_declare_the_same_fields<D: Document>(sample: &D) {
    let root = schema::<D>();
    let mut coverage = Coverage::default();

    cover(&root, &root, "", &to_value(sample), &mut coverage);

    let never_serialized: Vec<_> = coverage.declared.difference(&coverage.seen).collect();
    assert!(
        never_serialized.is_empty(),
        "{} declares fields the types never serialize: {never_serialized:?}",
        D::KIND
    );
    let defined: BTreeSet<String> = root["$defs"]
        .as_object()
        .expect("the schema has definitions")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        defined,
        coverage.definitions,
        "{} defines a shape that no serialized field uses",
        D::KIND
    );
}

fn assert_valid<D: Document>(document: &D) {
    let found = violations(&validator::<D>(true), &to_value(document));

    assert!(
        found.is_empty(),
        "a serialized {} must validate: {found:#?}",
        D::KIND
    );
}

fn assert_round_trips<D: Document + PartialEq + std::fmt::Debug>(document: &D) {
    let text = document.to_json_pretty().expect("a document should render");

    assert!(
        text.ends_with("}\n"),
        "the canonical form ends with one newline"
    );
    assert!(
        text.starts_with(&format!("{{\n  \"document_kind\": \"{}\",", D::KIND)),
        "the canonical form starts with the document kind:\n{text}"
    );
    assert_eq!(
        &D::from_json_str(&text).expect("the output should parse"),
        document
    );
    let rendered: Value = serde_json::from_str(&text).expect("the output is JSON");
    assert!(violations(&validator::<D>(true), &rendered).is_empty());
}

#[test]
fn every_schema_is_draft_2020_12_and_compiles() {
    assert_schema_compiles::<HarnessSummary>();
    assert_schema_compiles::<HarnessFailure>();
    assert_schema_compiles::<HarnessAb>();
    assert_schema_compiles::<HarnessCounts>();
    assert_schema_compiles::<HarnessSweep>();
    assert_schema_compiles::<HarnessTui>();
    assert_schema_compiles::<HarnessShapeProfile>();
    assert_schema_compiles::<HarnessSoak>();
}

#[test]
fn every_schema_identity_matches_the_rust_constants() {
    assert_schema_identity::<HarnessSummary>();
    assert_schema_identity::<HarnessFailure>();
    assert_schema_identity::<HarnessAb>();
    assert_schema_identity::<HarnessCounts>();
    assert_schema_identity::<HarnessSweep>();
    assert_schema_identity::<HarnessTui>();
    assert_schema_identity::<HarnessShapeProfile>();
    assert_schema_identity::<HarnessSoak>();
    assert_eq!(SCHEMA_VERSION, 1);
}

#[test]
fn the_rust_marker_fields_serialize_the_document_constants() {
    for (document, kind) in [
        (to_value(&summary()), HarnessSummary::KIND),
        (to_value(&failure()), HarnessFailure::KIND),
        (to_value(&ab()), HarnessAb::KIND),
        (to_value(&counts()), HarnessCounts::KIND),
        (to_value(&sweep()), HarnessSweep::KIND),
        (to_value(&tui_failure()), HarnessTui::KIND),
        (to_value(&shape_profile()), HarnessShapeProfile::KIND),
        (to_value(&soak()), HarnessSoak::KIND),
    ] {
        assert_eq!(document["document_kind"], kind);
        assert_eq!(document["schema_version"], SCHEMA_VERSION);
    }
}

#[test]
fn every_object_in_every_schema_rejects_undeclared_fields() {
    for schema in [
        schema::<HarnessSummary>(),
        schema::<HarnessFailure>(),
        schema::<HarnessAb>(),
        schema::<HarnessCounts>(),
        schema::<HarnessSweep>(),
        schema::<HarnessTui>(),
        schema::<HarnessShapeProfile>(),
        schema::<HarnessSoak>(),
    ] {
        assert_objects_are_closed(&schema, "#");
    }
}

#[test]
fn serialized_documents_validate_against_their_schemas() {
    assert_valid(&summary());
    assert_valid(&minimal_summary());
    assert_valid(&failure());
    assert_valid(&timeout_failure());
    assert_valid(&ab());
    assert_valid(&counts());
    assert_valid(&minimal_counts());
    assert_valid(&shape_profile());
    assert_valid(&minimal_shape_profile());
    assert_valid(&sweep());
    assert_valid(&minimal_sweep());
    assert_valid(&soak());
    assert_valid(&minimal_soak());
}

#[test]
fn schemas_and_types_declare_exactly_the_same_fields() {
    assert_schema_and_types_declare_the_same_fields(&summary());
    assert_schema_and_types_declare_the_same_fields(&timeout_failure());
    assert_schema_and_types_declare_the_same_fields(&ab());
    assert_schema_and_types_declare_the_same_fields(&counts());
    assert_schema_and_types_declare_the_same_fields(&shape_profile());
    assert_schema_and_types_declare_the_same_fields(&sweep());
    assert_schema_and_types_declare_the_same_fields(&soak());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one table of every closed vocabulary that a schema declares, a line at a time"
)]
fn enumerations_match_between_schemas_and_types() {
    fn names<T: Copy>(all: &[T], name: impl Fn(T) -> &'static str) -> BTreeSet<String> {
        all.iter().map(|&value| name(value).to_owned()).collect()
    }
    fn declared(schema: &Value, pointer: &str) -> BTreeSet<String> {
        schema
            .pointer(pointer)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{pointer} should be an enum"))
            .iter()
            .map(|value| value.as_str().expect("enum names are strings").to_owned())
            .collect()
    }
    let profiles = names(Profile::ALL, Profile::as_str);
    let summary_schema = schema::<HarnessSummary>();
    let ab_schema = schema::<HarnessAb>();
    let sweep_schema = schema::<HarnessSweep>();

    assert_eq!(declared(&summary_schema, "/$defs/profile/enum"), profiles);
    assert_eq!(
        declared(&schema::<HarnessFailure>(), "/$defs/profile/enum"),
        profiles
    );
    assert_eq!(
        declared(&schema::<HarnessCounts>(), "/$defs/profile/enum"),
        profiles
    );
    assert_eq!(declared(&sweep_schema, "/$defs/profile/enum"), profiles);
    assert_eq!(
        declared(&summary_schema, "/properties/tier/enum"),
        names(Tier::ALL, Tier::as_str)
    );
    assert_eq!(
        declared(&summary_schema, "/$defs/verdict/enum"),
        names(Verdict::ALL, Verdict::as_str)
    );
    assert_eq!(
        declared(&ab_schema, "/$defs/side/enum"),
        names(Side::ALL, Side::as_str)
    );
    assert_eq!(
        declared(&ab_schema, "/$defs/metric/properties/verdict/enum"),
        names(AbVerdict::ALL, AbVerdict::as_str)
    );
    assert_eq!(
        declared(&sweep_schema, "/properties/tier/enum"),
        names(SweepTier::ALL, SweepTier::as_str)
    );
    assert_eq!(
        declared(&sweep_schema, "/$defs/build/properties/status/enum"),
        names(BuildStatus::ALL, BuildStatus::as_str)
    );
    assert_eq!(
        declared(&sweep_schema, "/$defs/check/properties/status/enum"),
        names(CheckStatus::ALL, CheckStatus::as_str)
    );
    assert_eq!(
        declared(&sweep_schema, "/$defs/cell/properties/state/enum"),
        names(CellState::ALL, CellState::as_str)
    );
    // A warning is only ever for a budget that the speed of the machine decides: the four latency
    // budgets and the three ratio budgets.
    let timing_budgets: Vec<Budget> = Budget::ALL
        .iter()
        .copied()
        .filter(|budget| is_latency(*budget))
        .chain([
            Budget::HeadlessScanRatio,
            Budget::TuiCompleteRatio,
            Budget::MotionCompleteRatio,
        ])
        .collect();
    assert_eq!(
        declared(
            &summary_schema,
            "/$defs/timing_warning/properties/budget/enum"
        ),
        names(&timing_budgets, Budget::as_str)
    );
    let tui = schema::<HarnessTui>();
    let declared_commands: BTreeSet<String> = tui
        .pointer("/properties/command/enum")
        .and_then(Value::as_array)
        .expect("the command is an enum")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    assert_eq!(
        declared_commands,
        names(TuiCommand::ALL, TuiCommand::as_str)
    );
    assert_eq!(declared(&tui, "/$defs/profile/enum"), profiles);
    assert_eq!(
        declared(&tui, "/$defs/error/properties/kind/enum"),
        names(TuiErrorKind::ALL, TuiErrorKind::as_str)
    );
    assert_eq!(
        declared(&tui, "/$defs/event_record/properties/kind/enum"),
        names(EventRecordKind::ALL, EventRecordKind::as_str)
    );
    assert_eq!(
        declared(&tui, "/$defs/exit/properties/via/enum"),
        names(ExitVia::ALL, ExitVia::as_str)
    );
    assert_eq!(
        declared(&tui, "/$defs/session_info/properties/state/enum"),
        names(SessionState::ALL, SessionState::as_str)
    );
    assert_eq!(
        declared(&tui, "/$defs/delete_dialog/properties/kind/enum"),
        names(DeleteDialogKind::ALL, DeleteDialogKind::as_str)
    );
    assert_eq!(
        declared(&tui, "/$defs/delete_dialog/properties/confirmation/enum"),
        names(ConfirmationKind::ALL, ConfirmationKind::as_str)
    );
    assert_eq!(
        declared(&tui, "/$defs/delete_result/properties/kind/enum"),
        names(EntryKind::ALL, EntryKind::as_str)
    );
}

/// Sets the value at `pointer`, which must already exist.
fn set(document: &mut Value, pointer: &str, value: Value) {
    *document
        .pointer_mut(pointer)
        .unwrap_or_else(|| panic!("{pointer} should exist")) = value;
}

/// Removes the object member at `pointer`.
fn remove(document: &mut Value, pointer: &str) {
    let (parent, name) = pointer.rsplit_once('/').expect("a member pointer");
    document
        .pointer_mut(parent)
        .and_then(Value::as_object_mut)
        .unwrap_or_else(|| panic!("{parent} should be an object"))
        .remove(name);
}

/// Adds the object member at `pointer`, whose parent must be an object.
fn add(document: &mut Value, pointer: &str, value: Value) {
    let (parent, name) = pointer.rsplit_once('/').expect("a member pointer");
    document
        .pointer_mut(parent)
        .and_then(Value::as_object_mut)
        .unwrap_or_else(|| panic!("{parent} should be an object"))
        .insert(name.to_owned(), value);
}

/// One way to break a document, with a description for the failure message.
type Edit<'a> = (&'a str, &'a dyn Fn(&mut Value));

/// Asserts that each edit makes the document invalid.
fn assert_rejected<D: Document>(document: &D, edits: &[Edit<'_>]) {
    let validator = validator::<D>(true);
    for (description, edit) in edits {
        let mut instance = to_value(document);
        edit(&mut instance);

        assert!(
            !validator.is_valid(&instance),
            "{}: the schema must reject: {description}",
            D::KIND
        );
    }
}

#[test]
fn the_summary_schema_rejects_contract_drift() {
    assert_rejected(
        &summary(),
        &[
            ("an undeclared top-level field", &|d| {
                d["extra"] = true.into();
            }),
            ("an undeclared nested field", &|d| {
                d["scenarios"][0]["extra"] = 1.into();
            }),
            ("another document kind", &|d| {
                set(d, "/document_kind", "harness-ab".into());
            }),
            ("another schema version", &|d| {
                set(d, "/schema_version", 2.into());
            }),
            ("a missing git sha", &|d| remove(d, "/git_sha")),
            ("a missing scenario verdict", &|d| {
                remove(d, "/scenarios/0/verdict");
            }),
            ("an unknown tier", &|d| set(d, "/tier", "hourly".into())),
            ("an unknown verdict", &|d| {
                set(d, "/scenarios/0/verdict", "skip".into());
            }),
            ("an unknown profile", &|d| {
                set(d, "/scenarios/0/profile", "fancy".into());
            }),
            ("a run id that could leave its directory", &|d| {
                set(d, "/run_id", "../escape".into());
            }),
            ("an uppercase digest", &|d| {
                set(d, "/excise_binary/sha256", SHA256.to_uppercase().into());
            }),
            ("a short git sha", &|d| set(d, "/git_sha", "37d1c18".into())),
            ("a timestamp without a zone", &|d| {
                set(d, "/started_at", "2026-09-30T16:30:00".into());
            }),
            ("a timestamp with a space", &|d| {
                set(d, "/started_at", "2026-09-30 16:30:00Z".into());
            }),
            ("a negative duration", &|d| {
                set(d, "/scenarios/0/duration_ms", (-1).into());
            }),
            ("a metric that is not a number", &|d| {
                set(d, "/scenarios/0/metrics/max_rss_bytes", "big".into());
            }),
            ("a metric name that is not an identifier", &|d| {
                set(
                    d,
                    "/scenarios/0/metrics",
                    serde_json::json!({ "Max RSS": 1.0 }),
                );
            }),
            ("a scenario name that is not an identifier", &|d| {
                set(d, "/scenarios/0/name", "Delete Folder".into());
            }),
            (
                "a latency scale of exactly 1, which a strict run omits",
                &|d| {
                    set(d, "/latency_budget_scale", 1.into());
                },
            ),
            ("a latency scale that would tighten the budgets", &|d| {
                set(d, "/latency_budget_scale", 0.5.into());
            }),
            ("a latency scale that is not a number", &|d| {
                set(d, "/latency_budget_scale", "2x".into());
            }),
            ("an empty failure bundle path", &|d| {
                set(d, "/scenarios/1/failure_bundle", "".into());
            }),
        ],
    );
}

#[test]
fn the_summary_schema_rejects_drifting_timing_fields() {
    assert_rejected(
        &summary(),
        &[
            ("a timing flag of false, which a strict run omits", &|d| {
                set(d, "/timing_informational", false.into());
            }),
            ("a timing flag that is not a boolean", &|d| {
                set(d, "/timing_informational", "yes".into());
            }),
            ("an empty list of timing warnings", &|d| {
                set(d, "/scenarios/0/timing_warnings", serde_json::json!([]));
            }),
            ("a timing warning for a budget that always fails", &|d| {
                set(
                    d,
                    "/scenarios/0/timing_warnings/0/budget",
                    "peak_rss_bytes".into(),
                );
            }),
            ("a timing warning without its limit", &|d| {
                remove(d, "/scenarios/0/timing_warnings/0/limit");
            }),
            ("a timing warning value that is not a number", &|d| {
                set(d, "/scenarios/0/timing_warnings/0/value", "slow".into());
            }),
            ("a timing warning metric that is not an identifier", &|d| {
                set(
                    d,
                    "/scenarios/0/timing_warnings/0/metric",
                    "Max Stall".into(),
                );
            }),
            ("an undeclared field in a timing warning", &|d| {
                d["scenarios"][0]["timing_warnings"][0]["extra"] = 1.into();
            }),
            ("a negative quick-tier time", &|d| {
                set(d, "/quick_tier_ms", (-1).into());
            }),
            ("a quick-tier time that is not a whole number", &|d| {
                set(d, "/quick_tier_ms", 98_765.5.into());
            }),
            ("a quick-tier time that is not a number", &|d| {
                set(d, "/quick_tier_ms", "two minutes".into());
            }),
        ],
    );
}

#[test]
fn the_failure_schema_rejects_contract_drift() {
    assert_rejected(
        &timeout_failure(),
        &[
            ("an undeclared field", &|d| d["extra"] = true.into()),
            ("an undeclared nested field", &|d| {
                d["terminal_modes"]["extra"] = true.into();
            }),
            ("another document kind", &|d| {
                set(d, "/document_kind", "harness-summary".into());
            }),
            ("another schema version", &|d| {
                set(d, "/schema_version", 2.into());
            }),
            ("a missing terminal mode", &|d| {
                remove(d, "/terminal_modes/icanon");
            }),
            ("a terminal mode that is not a boolean", &|d| {
                set(d, "/terminal_modes/echo", "yes".into());
            }),
            ("a negative step index", &|d| {
                set(d, "/failed_step/index", (-1).into());
            }),
            ("a step index beyond 32 bits", &|d| {
                set(d, "/failed_step/index", 4_294_967_296_u64.into());
            }),
            ("an empty step description", &|d| {
                set(d, "/failed_step/description", "".into());
            }),
            ("a missing screen text", &|d| remove(d, "/screen/actual")),
            ("a hash that is not hexadecimal", &|d| {
                set(d, "/fixture/hash", "XYZ".into());
            }),
            ("a negative seed", &|d| set(d, "/fixture/seed", (-1).into())),
            ("a negative resident set", &|d| {
                set(d, "/rusage/max_rss_bytes", (-1).into());
            }),
            ("an empty repro command", &|d| {
                set(d, "/repro_command", "".into());
            }),
            ("an unknown profile", &|d| {
                set(d, "/profile", "fancy".into());
            }),
            ("an undeclared field inside session diagnostics", &|d| {
                d["session_diagnostics"]["extra"] = true.into();
            }),
            ("a non-boolean child_running", &|d| {
                set(d, "/session_diagnostics/child_running", "yes".into());
            }),
            ("a negative cursor_reports_answered", &|d| {
                set(
                    d,
                    "/session_diagnostics/cursor_reports_answered",
                    (-1).into(),
                );
            }),
            ("a missing session diagnostics head", &|d| {
                remove(d, "/session_diagnostics/head");
            }),
            ("a negative first_byte_after_ms", &|d| {
                set(d, "/session_diagnostics/first_byte_after_ms", (-1).into());
            }),
        ],
    );
}

#[test]
fn the_ab_schema_rejects_contract_drift() {
    assert_rejected(
        &ab(),
        &[
            ("an undeclared field", &|d| d["extra"] = true.into()),
            ("an undeclared nested field", &|d| {
                d["metrics"][0]["bootstrap_ci"]["extra"] = true.into();
            }),
            ("another document kind", &|d| {
                set(d, "/document_kind", "harness-failure".into());
            }),
            ("another schema version", &|d| {
                set(d, "/schema_version", 2.into());
            }),
            ("a run order of one", &|d| {
                set(d, "/interleaving", serde_json::json!(["baseline"]));
            }),
            ("an unknown side in the run order", &|d| {
                set(
                    d,
                    "/interleaving",
                    serde_json::json!(["baseline", "control"]),
                );
            }),
            ("a certain confidence interval", &|d| {
                set(d, "/metrics/0/bootstrap_ci/confidence", 1.0.into());
            }),
            ("a zero confidence interval", &|d| {
                set(d, "/metrics/0/bootstrap_ci/confidence", 0.into());
            }),
            ("an unknown verdict", &|d| {
                set(d, "/metrics/0/verdict", "fail".into());
            }),
            ("a negative trial count", &|d| {
                set(d, "/trials", (-1).into());
            }),
            ("no baseline samples", &|d| {
                set(d, "/metrics/0/samples/baseline", serde_json::json!([]));
            }),
            ("a fractional process count", &|d| {
                set(d, "/context/concurrent_excise_processes", 1.5.into());
            }),
            ("a digest of the wrong length", &|d| {
                set(d, "/candidate/binary_sha256", "abc".into());
            }),
            ("a negative median ratio", &|d| {
                set(d, "/metrics/0/median_ratio", (-0.5).into());
            }),
            ("zero logical cpus", &|d| {
                set(d, "/context/logical_cpus", 0.into());
            }),
            ("a negative load average", &|d| {
                set(d, "/context/load_average_start", (-0.1).into());
            }),
            ("an empty fixtures list", &|d| {
                set(d, "/context/fixtures", serde_json::json!([]));
            }),
            ("a fixture hash that is not hexadecimal", &|d| {
                set(d, "/context/fixtures/0/hash", "not-hex".into());
            }),
            ("a missing fixtures field", &|d| {
                remove(d, "/context/fixtures");
            }),
            ("a missing context field", &|d| remove(d, "/context/power")),
        ],
    );
}

#[test]
fn timestamps_are_checked_for_shape_and_for_valid_dates() {
    let strict = validator::<HarnessSummary>(true);
    let lenient = validator::<HarnessSummary>(false);
    let with_start = |text: &str| {
        let mut instance = to_value(&summary());
        set(&mut instance, "/started_at", text.into());
        instance
    };

    for accepted in [
        "2026-09-30T16:30:00Z",
        "2026-09-30T16:30:00.123456Z",
        "2026-09-30T16:30:00+10:00",
        "2026-09-30T06:30:00-10:00",
    ] {
        assert!(strict.is_valid(&with_start(accepted)), "{accepted}");
    }
    for malformed in [
        "yesterday",
        "2026-09-30",
        "2026-09-30T16:30:00",
        "2026-09-30 16:30:00Z",
    ] {
        assert!(
            !lenient.is_valid(&with_start(malformed)),
            "{malformed} fails the pattern even when formats are not asserted"
        );
    }
    assert!(
        !strict.is_valid(&with_start("2026-13-45T25:61:61Z")),
        "an impossible date is caught by the date-time format"
    );
}

#[test]
fn documents_render_a_canonical_form_that_round_trips() {
    assert_round_trips(&summary());
    assert_round_trips(&minimal_summary());
    assert_round_trips(&failure());
    assert_round_trips(&timeout_failure());
    assert_round_trips(&ab());
    assert_round_trips(&counts());
    assert_round_trips(&minimal_counts());
    assert_round_trips(&shape_profile());
    assert_round_trips(&minimal_shape_profile());
    assert_round_trips(&sweep());
    assert_round_trips(&minimal_sweep());
    assert_round_trips(&soak());
    assert_round_trips(&minimal_soak());
}

#[test]
fn documents_that_are_not_this_kind_and_version_are_rejected() {
    let text = summary().to_json_pretty().expect("a summary should render");

    assert!(HarnessFailure::from_json_str(&text).is_err());
    assert!(HarnessAb::from_json_str(&text).is_err());
    assert!(HarnessSweep::from_json_str(&text).is_err());
    assert!(HarnessSummary::from_json_str(&text.replace("harness-summary", "harness-ab")).is_err());
    let future = text.replace("\"schema_version\": 1", "\"schema_version\": 2");
    let error = HarnessSummary::from_json_str(&future).expect_err("version 2 is unsupported");
    assert!(
        error.to_string().contains("unsupported schema_version 2"),
        "{error}"
    );
    for malformed in [
        text.replace("\"schema_version\": 1", "\"schema_version\": 1.0"),
        text.replace("\"schema_version\": 1", "\"schema_version\": \"1\""),
        text.replace("\"host\"", "\"unexpected\": 1,\n  \"host\""),
        text.replace("\"tier\": \"quick\"", "\"tier\": \"hourly\""),
    ] {
        assert!(
            HarnessSummary::from_json_str(&malformed).is_err(),
            "{malformed}"
        );
    }
}

#[test]
fn a_document_that_json_cannot_carry_is_refused_rather_than_written_as_null() {
    let mut not_a_number = summary();
    not_a_number.scenarios[0]
        .metrics
        .insert("ratio".to_owned(), f64::NAN);
    let mut infinite = ab();
    infinite.metrics[0].samples.candidate[0] = f64::INFINITY;
    let mut unbounded = sweep();
    unbounded.measurements[0].series[0].samples[0] = f64::INFINITY;

    assert!(not_a_number.to_json_pretty().is_err());
    assert!(infinite.to_json_pretty().is_err());
    assert!(unbounded.to_json_pretty().is_err());
}

#[test]
fn strict_xfail_is_encoded_in_verdict_resolution() {
    assert_eq!(Verdict::resolve(Expect::Pass, true), Verdict::Pass);
    assert_eq!(Verdict::resolve(Expect::Pass, false), Verdict::Fail);
    assert_eq!(Verdict::resolve(Expect::Fail, false), Verdict::Xfail);
    assert_eq!(
        Verdict::resolve(Expect::Fail, true),
        Verdict::Xpass,
        "an expected failure that passes is never a pass"
    );
}

#[test]
fn only_pass_and_xfail_keep_a_run_green() {
    let green: Vec<Verdict> = Verdict::ALL
        .iter()
        .copied()
        .filter(|verdict| !verdict.blocks_run())
        .collect();

    assert_eq!(green, [Verdict::Pass, Verdict::Xfail]);
}

#[test]
fn the_counts_schema_rejects_contract_drift() {
    let many_cases = |d: &mut Value| {
        let case = d["cases"][0].clone();
        d["cases"] = Value::Array(vec![case; 17]);
    };
    assert_rejected(
        &counts(),
        &[
            ("an undeclared top-level field", &|d| {
                d["extra"] = true.into();
            }),
            ("an undeclared context field", &|d| {
                d["context"]["extra"] = 1.into();
            }),
            ("an undeclared runner field", &|d| {
                d["context"]["runner"]["extra"] = 1.into();
            }),
            ("an undeclared pull request field", &|d| {
                d["context"]["pull_request"]["extra"] = 1.into();
            }),
            ("an undeclared case field", &|d| {
                d["cases"][0]["extra"] = 1.into();
            }),
            ("an undeclared fixture field", &|d| {
                d["cases"][0]["fixture"]["extra"] = 1.into();
            }),
            ("another document kind", &|d| {
                set(d, "/document_kind", "harness-summary".into());
            }),
            ("another schema version", &|d| {
                set(d, "/schema_version", 2.into());
            }),
            ("no context", &|d| remove(d, "/context")),
            ("no cases", &|d| remove(d, "/cases")),
            ("an empty list of cases", &|d| {
                set(d, "/cases", serde_json::json!([]));
            }),
            ("more than sixteen cases", &many_cases),
        ],
    );
}

#[test]
fn the_counts_schema_rejects_a_context_that_misdescribes_the_run() {
    assert_rejected(
        &counts(),
        &[
            ("a commit that is too short", &|d| {
                set(d, "/context/git_sha", "37d1c18".into());
            }),
            ("a commit in capitals", &|d| {
                set(d, "/context/git_sha", GIT_SHA.to_uppercase().into());
            }),
            ("a commit that is not hexadecimal", &|d| {
                set(d, "/context/git_sha", "z".repeat(40).into());
            }),
            ("a commit time without a zone", &|d| {
                set(d, "/context/committed_at", "2026-10-05T10:35:18".into());
            }),
            ("an operating system in capitals", &|d| {
                set(d, "/context/runner/os", "Linux".into());
            }),
            ("an operating system that could leave its directory", &|d| {
                set(d, "/context/runner/os", "../x".into());
            }),
            ("an operating system name that is too long", &|d| {
                set(d, "/context/runner/os", "a".repeat(17).into());
            }),
            ("an architecture with a hyphen", &|d| {
                set(d, "/context/runner/arch", "x86-64".into());
            }),
            ("an operating system version with a line break", &|d| {
                set(d, "/context/runner/os_version", "Ubuntu\n24.04".into());
            }),
            ("an operating system version that is not ASCII", &|d| {
                set(d, "/context/runner/os_version", "Ubuntu \u{e9}".into());
            }),
            (
                "an operating system version with a control character",
                &|d| {
                    set(d, "/context/runner/os_version", "Ubuntu\t24".into());
                },
            ),
            ("an operating system version that is too long", &|d| {
                set(d, "/context/runner/os_version", "u".repeat(121).into());
            }),
            ("an empty toolchain", &|d| {
                set(d, "/context/toolchain", "".into());
            }),
            ("a toolchain with a line break", &|d| {
                set(
                    d,
                    "/context/toolchain",
                    "rustc 1.98.0\nbinary: rustc".into(),
                );
            }),
        ],
    );
}

#[test]
fn the_counts_schema_rejects_a_pull_request_origin_that_names_no_commit() {
    assert_rejected(
        &counts(),
        &[
            ("a pull request number of zero", &|d| {
                set(d, "/context/pull_request/number", 0.into());
            }),
            ("a negative pull request number", &|d| {
                set(d, "/context/pull_request/number", (-1).into());
            }),
            ("a pull request number a double cannot hold", &|d| {
                set(d, "/context/pull_request/number", (MAX_COUNT + 1).into());
            }),
            ("a short base commit", &|d| {
                set(d, "/context/pull_request/base_sha", "c0ffee".into());
            }),
            ("a head commit that is not hexadecimal", &|d| {
                set(d, "/context/pull_request/head_sha", "z".repeat(40).into());
            }),
            ("no head commit", &|d| {
                remove(d, "/context/pull_request/head_sha");
            }),
        ],
    );
}

#[test]
fn the_counts_schema_rejects_cases_and_counts_that_cannot_be_compared() {
    let many_counts = |d: &mut Value| {
        let metrics: serde_json::Map<String, Value> = (0..33)
            .map(|index| (format!("m{index:02}"), Value::from(index)))
            .collect();
        set(d, "/cases/0/metrics", Value::Object(metrics));
    };
    assert_rejected(
        &counts(),
        &[
            ("a fixture id in capitals", &|d| {
                set(d, "/cases/0/fixture/id", "Wide-1k".into());
            }),
            ("a fixture id with a space", &|d| {
                set(d, "/cases/0/fixture/id", "wide 1k".into());
            }),
            ("a fixture id that is too long", &|d| {
                set(d, "/cases/0/fixture/id", "w".repeat(65).into());
            }),
            ("a fixture hash that is not hexadecimal", &|d| {
                set(d, "/cases/0/fixture/hash", "not-hex".into());
            }),
            ("a negative seed", &|d| {
                set(d, "/cases/0/fixture/seed", (-1).into());
            }),
            ("an unknown profile", &|d| {
                set(d, "/cases/0/profile", "fancy".into());
            }),
            ("a case with no counts", &|d| {
                set(d, "/cases/0/metrics", serde_json::json!({}));
            }),
            ("more than thirty-two counts in a case", &many_counts),
            ("a count named in capitals", &|d| {
                set(d, "/cases/0/metrics", serde_json::json!({ "Entries": 1 }));
            }),
            ("a count named with a hyphen", &|d| {
                set(d, "/cases/0/metrics", serde_json::json!({ "idle-fds": 1 }));
            }),
            ("a count named from a digit", &|d| {
                set(d, "/cases/0/metrics", serde_json::json!({ "1st": 1 }));
            }),
            ("a count name that is too long", &|d| {
                let mut metrics = serde_json::Map::new();
                metrics.insert("m".repeat(49), 1.into());
                set(d, "/cases/0/metrics", Value::Object(metrics));
            }),
            ("a negative count", &|d| {
                set(d, "/cases/0/metrics/entries", (-1).into());
            }),
            ("a fractional count", &|d| {
                set(d, "/cases/0/metrics/entries", 1.5.into());
            }),
            ("a count that is a string", &|d| {
                set(d, "/cases/0/metrics/entries", "1002".into());
            }),
            ("a count that is null", &|d| {
                set(d, "/cases/0/metrics/entries", Value::Null);
            }),
            ("a count a double cannot hold", &|d| {
                set(d, "/cases/0/metrics/entries", (MAX_COUNT + 1).into());
            }),
        ],
    );
}

#[test]
fn the_limits_the_writer_checks_are_the_limits_of_the_schema() {
    let schema = schema::<HarnessCounts>();

    assert_eq!(
        schema.pointer("/properties/cases/maxItems"),
        Some(&Value::from(MAX_CASES)),
        "the most cases a document holds"
    );
    assert_eq!(
        schema.pointer("/$defs/count/maximum"),
        Some(&Value::from(MAX_COUNT)),
        "the largest count a document carries"
    );
}

#[test]
fn a_count_at_the_largest_value_a_double_holds_is_accepted() {
    let mut document = counts();
    document.cases[0]
        .metrics
        .insert("scan_store_bytes".to_owned(), MAX_COUNT);

    assert_valid(&document);
    assert_eq!(document.check(), Ok(()));
}

#[test]
fn a_counts_document_that_breaks_a_rule_the_schema_cannot_say_is_refused() {
    let mut twice = counts();
    twice.cases[1] = twice.cases[0].clone();
    let mut huge = counts();
    huge.cases[0]
        .metrics
        .insert("scan_store_bytes".to_owned(), MAX_COUNT + 1);

    assert_eq!(counts().check(), Ok(()));
    assert_eq!(
        twice.check(),
        Err(CountsInvalid::DuplicateCase {
            fixture: "wide-1k".to_owned(),
            profile: Profile::Deterministic,
        })
    );
    assert_eq!(
        huge.check(),
        Err(CountsInvalid::CountTooLarge {
            fixture: "wide-1k".to_owned(),
            metric: "scan_store_bytes".to_owned(),
            value: MAX_COUNT + 1,
        })
    );
}

#[test]
fn one_fixture_may_be_counted_under_two_profiles() {
    let mut document = counts();
    document.cases[1].fixture = document.cases[0].fixture.clone();

    assert_eq!(
        document.check(),
        Ok(()),
        "the case is a fixture and a profile"
    );
    assert_eq!(
        document
            .case("wide-1k", Profile::Default)
            .map(|case| case.profile),
        Some(Profile::Default)
    );
    assert!(document.case("wide-1k", Profile::Narrow).is_none());
    assert!(document.case("absent", Profile::Default).is_none());
}

#[test]
fn counts_documents_that_are_not_this_kind_and_version_or_have_other_numbers_are_rejected() {
    let text = counts().to_json_pretty().expect("counts should render");

    assert!(HarnessSummary::from_json_str(&text).is_err());
    assert!(HarnessCounts::from_json_str(&text.replace("harness-counts", "harness-ab")).is_err());
    let future = text.replace("\"schema_version\": 1", "\"schema_version\": 2");
    let error = HarnessCounts::from_json_str(&future).expect_err("version 2 is unsupported");
    assert!(
        error.to_string().contains("unsupported schema_version 2"),
        "{error}"
    );
    for malformed in [
        text.replace("\"entries\": 1002", "\"entries\": 1002.5"),
        text.replace("\"entries\": 1002", "\"entries\": -1002"),
        text.replace("\"entries\": 1002", "\"entries\": \"1002\""),
        text.replace("\"seed\": 7", "\"seed\": 7.0"),
        text.replace("\"os\": \"linux\"", "\"os\": \"linux\", \"unexpected\": 1"),
    ] {
        assert!(
            HarnessCounts::from_json_str(&malformed).is_err(),
            "{malformed}"
        );
    }
}

/// The pointer to one object of every shape a sweep document holds.
const SWEEP_OBJECTS: [&str; 14] = [
    "",
    "/context",
    "/context/fixtures/0",
    "/versions/0",
    "/versions/0/toolchain",
    "/versions/0/build",
    "/versions/0/traits",
    "/measurements/0",
    "/measurements/0/series/0",
    "/measurements/0/series/0/ratios/candidate",
    "/measurements/0/series/0/ratios/candidate/bootstrap_ci",
    "/checks/0",
    "/rows/0",
    "/rows/0/cells/0",
];

/// The pointer to every kind of text a sweep document holds, each of which has to say something.
const SWEEP_TEXTS: [&str; 32] = [
    "/run_id",
    "/candidate",
    "/context/host",
    "/context/cpu",
    "/context/os",
    "/context/arch",
    "/context/power",
    "/context/du",
    "/versions/0/ref",
    "/versions/0/toolchain/channel",
    "/versions/0/toolchain/rustc",
    "/versions/1/build/reason",
    "/versions/1/build/log",
    "/measurements/0/metric",
    "/measurements/0/fixture",
    "/measurements/0/order/0/0",
    "/measurements/0/series/0/ref",
    "/checks/0/ref",
    "/checks/0/check",
    "/checks/0/fixture",
    "/checks/0/notes/0",
    "/checks/0/evidence",
    "/checks/1/reason",
    "/rows/0/defect",
    "/rows/0/title",
    "/rows/0/changelog",
    "/rows/0/measured_by",
    "/rows/0/note",
    "/rows/0/cells/0/ref",
    "/rows/0/cells/0/reason",
    "/rows/0/cells/0/value",
    "/rows/0/cells/0/evidence/0",
];

#[test]
fn the_sweep_schema_rejects_contract_drift() {
    for pointer in SWEEP_OBJECTS {
        let description = format!("an undeclared member of {pointer:?}");
        let extra = |d: &mut Value| add(d, &format!("{pointer}/extra"), true.into());
        assert_rejected(&sweep(), &[(description.as_str(), &extra)]);
    }
    assert_rejected(
        &sweep(),
        &[
            ("another document kind", &|d| {
                set(d, "/document_kind", "harness-ab".into());
            }),
            ("another schema version", &|d| {
                set(d, "/schema_version", 2.into());
            }),
            ("no rows", &|d| remove(d, "/rows")),
            ("an empty list of rows", &|d| {
                set(d, "/rows", serde_json::json!([]));
            }),
            ("an empty list of versions", &|d| {
                set(d, "/versions", serde_json::json!([]));
            }),
            ("an order that is not a list", &|d| {
                set(d, "/measurements/0/order", "v1.0.0 then HEAD".into());
            }),
            ("an order of refs rather than rounds", &|d| {
                set(
                    d,
                    "/measurements/0/order",
                    serde_json::json!(["v1.0.0", "HEAD"]),
                );
            }),
            ("a negative round count", &|d| {
                set(d, "/measurements/0/rounds", (-1).into());
            }),
            ("an unknown tier", &|d| set(d, "/tier", "nightly".into())),
            ("an unknown build status", &|d| {
                set(d, "/versions/0/build/status", "cached".into());
            }),
            ("an unknown check status", &|d| {
                set(d, "/checks/0/status", "skipped".into());
            }),
            ("an unknown cell state", &|d| {
                set(d, "/rows/0/cells/0/state", "unaffected".into());
            }),
            ("an unknown profile", &|d| {
                set(d, "/measurements/1/profile", "fancy".into());
            }),
            ("an empty ref", &|d| set(d, "/versions/0/ref", "".into())),
            ("a short commit", &|d| {
                set(d, "/versions/0/sha", "37d1c18".into());
            }),
            ("a time without a zone", &|d| {
                set(d, "/started_at", "2026-10-06T09:15:00".into());
            }),
            ("a failed build with no reason", &|d| {
                remove(d, "/versions/1/build/reason");
            }),
            ("a check that ran and gives a reason", &|d| {
                add(d, "/checks/0/reason", "odd".into());
            }),
            ("a cell that is not measurable and gives no reason", &|d| {
                remove(d, "/rows/0/cells/1/reason");
            }),
        ],
    );
}

#[test]
fn the_sweep_schema_requires_every_member_the_types_always_write() {
    assert_rejected(
        &sweep(),
        &[
            ("no run id", &|d| remove(d, "/run_id")),
            ("no tier", &|d| remove(d, "/tier")),
            ("no start time", &|d| remove(d, "/started_at")),
            ("no finish time", &|d| remove(d, "/finished_at")),
            ("no candidate", &|d| remove(d, "/candidate")),
            ("no context", &|d| remove(d, "/context")),
            ("no versions", &|d| remove(d, "/versions")),
            ("no measurements", &|d| remove(d, "/measurements")),
            ("no checks", &|d| remove(d, "/checks")),
            ("a missing context field", &|d| remove(d, "/context/seed")),
            ("a missing checkout commit", &|d| {
                remove(d, "/context/checkout_sha");
            }),
            ("a fixture with no hash", &|d| {
                remove(d, "/context/fixtures/0/hash");
            }),
            ("a version with no commit", &|d| {
                remove(d, "/versions/0/sha");
            }),
            ("a version with no build", &|d| {
                remove(d, "/versions/0/build");
            }),
            ("a toolchain with no rustc line", &|d| {
                remove(d, "/versions/0/toolchain/rustc");
            }),
            ("a build with no cache flag", &|d| {
                remove(d, "/versions/0/build/cached");
            }),
            ("traits with no scan store flag", &|d| {
                remove(d, "/versions/0/traits/scan_store_dir");
            }),
            ("a timing with no series", &|d| {
                remove(d, "/measurements/0/series");
            }),
            ("a timing with no order", &|d| {
                remove(d, "/measurements/0/order");
            }),
            ("a series with no completion flags", &|d| {
                remove(d, "/measurements/0/series/0/completed");
            }),
            ("a series with no ratios", &|d| {
                remove(d, "/measurements/0/series/0/ratios");
            }),
            ("a ratio with no pair count", &|d| {
                remove(d, "/measurements/0/series/0/ratios/du/pairs");
            }),
            ("a series with no rounds", &|d| {
                remove(d, "/measurements/0/series/0/rounds");
            }),
            ("a series with no skipped count", &|d| {
                remove(d, "/measurements/0/series/0/skipped");
            }),
            ("a ratio with no lower bounds", &|d| {
                remove(d, "/measurements/0/series/0/ratios/du/lower_bounds");
            }),
            ("a ratio with no upper bounds", &|d| {
                remove(d, "/measurements/0/series/0/ratios/du/upper_bounds");
            }),
            (
                "a ratio with no count of rounds neither side finished",
                &|d| {
                    remove(d, "/measurements/0/series/0/ratios/du/both_censored");
                },
            ),
            ("an interval with no upper bound", &|d| {
                remove(d, "/measurements/0/series/0/ratios/du/bootstrap_ci/upper");
            }),
            ("a check with no metrics", &|d| {
                remove(d, "/checks/0/metrics");
            }),
            ("a check with no notes", &|d| remove(d, "/checks/0/notes")),
            ("a cell with no state", &|d| {
                remove(d, "/rows/0/cells/0/state");
            }),
            ("a cell with no evidence", &|d| {
                remove(d, "/rows/0/cells/0/evidence");
            }),
            ("a row with no cells", &|d| remove(d, "/rows/0/cells")),
            ("a row with no measure", &|d| {
                remove(d, "/rows/0/measured_by");
            }),
        ],
    );
}

#[test]
fn every_text_a_sweep_holds_must_say_something() {
    for pointer in SWEEP_TEXTS {
        let description = format!("an empty text at {pointer}");
        let empty = |d: &mut Value| set(d, pointer, "".into());
        assert_rejected(&sweep(), &[(description.as_str(), &empty)]);
    }
}

#[test]
fn the_sweep_schema_rejects_identities_and_times_that_name_nothing_exact() {
    assert_rejected(
        &sweep(),
        &[
            ("a commit that is too short", &|d| {
                set(d, "/versions/0/sha", "37d1c18".into());
            }),
            ("a commit in capitals", &|d| {
                set(d, "/versions/2/sha", GIT_SHA.to_uppercase().into());
            }),
            ("a commit that is not hexadecimal", &|d| {
                set(d, "/versions/0/sha", "z".repeat(40).into());
            }),
            ("a checkout commit that is too short", &|d| {
                set(d, "/context/checkout_sha", "37d1c18".into());
            }),
            ("a checkout commit in capitals", &|d| {
                set(d, "/context/checkout_sha", GIT_SHA.to_uppercase().into());
            }),
            ("a digest that is too short", &|d| {
                set(d, "/versions/0/binary_sha256", "abc".into());
            }),
            ("a digest in capitals", &|d| {
                set(d, "/versions/2/binary_sha256", SHA256.to_uppercase().into());
            }),
            ("a start time without a zone", &|d| {
                set(d, "/started_at", "2026-10-06T09:15:00".into());
            }),
            ("a finish time with a space", &|d| {
                set(d, "/finished_at", "2026-10-06 09:47:41Z".into());
            }),
            ("a finish time that is not a time", &|d| {
                set(d, "/finished_at", "yesterday".into());
            }),
            ("a finish time that is not a date", &|d| {
                set(d, "/finished_at", "2026-13-45T25:61:61Z".into());
            }),
            ("a fixture hash that is not hexadecimal", &|d| {
                set(d, "/context/fixtures/0/hash", "not-hex".into());
            }),
            ("a fixture id in capitals", &|d| {
                set(d, "/context/fixtures/0/id", "Node-Modules-2k".into());
            }),
            ("a fixture id that is too long", &|d| {
                set(d, "/context/fixtures/0/id", "n".repeat(65).into());
            }),
            ("an unknown profile in a timing", &|d| {
                set(d, "/measurements/1/profile", "fancy".into());
            }),
            ("an unknown profile in a check", &|d| {
                set(d, "/checks/0/profile", "fancy".into());
            }),
        ],
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one table of the numbers a run cannot produce, one row each"
)]
fn the_sweep_schema_rejects_numbers_a_run_cannot_produce() {
    assert_rejected(
        &sweep(),
        &[
            ("a negative round count", &|d| {
                set(d, "/measurements/0/rounds", (-1).into());
            }),
            ("a round count that is not whole", &|d| {
                set(d, "/measurements/0/rounds", 2.5.into());
            }),
            ("a round count beyond 32 bits", &|d| {
                set(d, "/measurements/0/rounds", 4_294_967_296_u64.into());
            }),
            ("a negative round count in the context", &|d| {
                set(d, "/context/rounds", (-1).into());
            }),
            ("a negative pair count", &|d| {
                set(d, "/measurements/0/series/0/ratios/du/pairs", (-1).into());
            }),
            ("a negative skipped count", &|d| {
                set(d, "/measurements/1/series/0/skipped", (-1).into());
            }),
            ("a skipped count that is not whole", &|d| {
                set(d, "/measurements/1/series/0/skipped", 1.5.into());
            }),
            ("a negative round number", &|d| {
                set(d, "/measurements/0/series/0/rounds/0", (-1).into());
            }),
            ("a round number beyond 32 bits", &|d| {
                set(
                    d,
                    "/measurements/0/series/0/rounds/0",
                    4_294_967_296_u64.into(),
                );
            }),
            ("a negative lower bound", &|d| {
                set(
                    d,
                    "/measurements/1/series/0/ratios/candidate/lower_bounds/0",
                    (-1.0).into(),
                );
            }),
            ("a negative upper bound", &|d| {
                set(
                    d,
                    "/measurements/1/series/0/ratios/candidate/upper_bounds",
                    serde_json::json!([-0.5]),
                );
            }),
            ("a negative count of rounds neither side finished", &|d| {
                set(
                    d,
                    "/measurements/1/series/0/ratios/candidate/both_censored",
                    (-1).into(),
                );
            }),
            ("a negative timeout", &|d| {
                set(d, "/context/timeout_ms", (-1).into());
            }),
            ("a negative drain rate", &|d| {
                set(d, "/measurements/1/drain_bytes_per_sec", (-1).into());
            }),
            ("a drain rate that is not whole", &|d| {
                set(d, "/context/drain_bytes_per_sec", 1.5.into());
            }),
            ("a negative seed", &|d| set(d, "/context/seed", (-1).into())),
            ("zero logical cpus", &|d| {
                set(d, "/context/logical_cpus", 0.into());
            }),
            ("a negative load average", &|d| {
                set(d, "/context/load_average_end", (-0.1).into());
            }),
            ("a negative process count", &|d| {
                set(d, "/context/concurrent_excise_processes", (-1).into());
            }),
            ("a negative sample", &|d| {
                set(d, "/measurements/0/series/0/samples/0", (-1.0).into());
            }),
            ("a sample that is not a number", &|d| {
                set(d, "/measurements/0/series/0/samples/0", "fast".into());
            }),
            ("a negative du sample", &|d| {
                set(d, "/measurements/0/du_samples/0", (-1.0).into());
            }),
            ("a negative median of samples", &|d| {
                set(d, "/measurements/0/series/0/median", (-1.0).into());
            }),
            ("a negative median ratio", &|d| {
                set(
                    d,
                    "/measurements/0/series/0/ratios/du/median",
                    (-0.5).into(),
                );
            }),
            ("a certain interval", &|d| {
                set(
                    d,
                    "/measurements/0/series/0/ratios/du/bootstrap_ci/confidence",
                    1.0.into(),
                );
            }),
            ("an interval of no confidence", &|d| {
                set(
                    d,
                    "/measurements/0/series/0/ratios/du/bootstrap_ci/confidence",
                    0.into(),
                );
            }),
            ("a completion flag that is not a boolean", &|d| {
                set(d, "/measurements/0/series/0/completed/0", "yes".into());
            }),
            ("a report version beyond 32 bits", &|d| {
                set(
                    d,
                    "/versions/0/traits/report_version",
                    4_294_967_296_u64.into(),
                );
            }),
            ("a scan store flag that is not a boolean", &|d| {
                set(d, "/versions/0/traits/scan_store_dir", "no".into());
            }),
            ("a cached flag that is not a boolean", &|d| {
                set(d, "/versions/0/build/cached", 1.into());
            }),
            ("no du samples at all", &|d| {
                set(d, "/measurements/0/du_samples", serde_json::json!([]));
            }),
            ("a du sample that is text", &|d| {
                set(d, "/measurements/0/du_samples/0", "fast".into());
            }),
        ],
    );
}

#[test]
fn the_sweep_schema_rejects_maps_and_lists_of_the_wrong_shape() {
    assert_rejected(
        &sweep(),
        &[
            ("a metric named with nothing", &|d| {
                set(d, "/checks/0/metrics", serde_json::json!({ "": 1.0 }));
            }),
            ("a metric that is not a number", &|d| {
                set(d, "/checks/0/metrics/exit_code", "143".into());
            }),
            ("a metric that is null", &|d| {
                set(d, "/checks/0/metrics/exit_code", Value::Null);
            }),
            ("a ratio named with nothing", &|d| {
                let ratio = d["measurements"][0]["series"][0]["ratios"]["du"].clone();
                d["measurements"][0]["series"][0]["ratios"][""] = ratio;
            }),
            ("a ratio that is a number", &|d| {
                set(d, "/measurements/0/series/0/ratios/du", 2.5.into());
            }),
            ("evidence that is one path", &|d| {
                set(d, "/rows/0/cells/0/evidence", "#/measurements/0".into());
            }),
            ("notes that are one line", &|d| {
                set(d, "/checks/0/notes", "ended on SIGTERM".into());
            }),
            ("samples that are one number", &|d| {
                set(d, "/measurements/0/series/0/samples", 120.0.into());
            }),
            ("completion flags that are one flag", &|d| {
                set(d, "/measurements/0/series/0/completed", true.into());
            }),
            ("lower bounds that are one number", &|d| {
                set(
                    d,
                    "/measurements/1/series/0/ratios/candidate/lower_bounds",
                    30.0.into(),
                );
            }),
            ("rounds that are one number", &|d| {
                set(d, "/measurements/0/series/0/rounds", 0.into());
            }),
            ("du samples that are one number", &|d| {
                set(d, "/measurements/0/du_samples", 40.0.into());
            }),
            ("series that are one object", &|d| {
                set(d, "/measurements/0/series", serde_json::json!({}));
            }),
            ("an order whose rounds hold numbers", &|d| {
                set(d, "/measurements/0/order", serde_json::json!([[1, 2]]));
            }),
        ],
    );
}

#[test]
fn the_sweep_schema_rejects_a_reason_that_does_not_fit_its_status() {
    assert_rejected(
        &sweep(),
        &[
            ("a failed build that gives no reason", &|d| {
                remove(d, "/versions/1/build/reason");
            }),
            ("a build that did not fail and gives a reason", &|d| {
                add(d, "/versions/0/build/reason", "odd".into());
            }),
            (
                "a cached build that did not fail and gives a reason",
                &|d| {
                    add(d, "/versions/2/build/reason", "odd".into());
                },
            ),
            ("a check that ran and gives a reason", &|d| {
                add(d, "/checks/0/reason", "odd".into());
            }),
            ("a check that did not run and gives no reason", &|d| {
                remove(d, "/checks/1/reason");
            }),
            ("a check that errored and gives no reason", &|d| {
                remove(d, "/checks/3/reason");
            }),
            ("a cell that is not measurable and gives no reason", &|d| {
                remove(d, "/rows/0/cells/1/reason");
            }),
            (
                "a second cell that is not measurable and gives no reason",
                &|d| {
                    remove(d, "/rows/1/cells/1/reason");
                },
            ),
        ],
    );
}

/// What `check` says of the rich sweep once `edit` has broken it, and whether the schema still
/// accepts the document.
fn broken_sweep(edit: impl FnOnce(&mut HarnessSweep)) -> (Result<(), SweepInvalid>, bool) {
    let mut document = sweep();
    edit(&mut document);

    (
        document.check(),
        validator::<HarnessSweep>(true).is_valid(&to_value(&document)),
    )
}

/// Asserts that `edit` breaks a rule that only `check` can say: the schema accepts the document.
fn assert_refused_by_check_alone(edit: impl FnOnce(&mut HarnessSweep), rule: SweepInvalid) {
    assert_eq!(broken_sweep(edit), (Err(rule), true));
}

/// Asserts that `edit` breaks a rule that the schema says as well, which `check` repeats for a
/// reader that holds the document to it alone.
fn assert_refused_by_both(edit: impl FnOnce(&mut HarnessSweep), rule: SweepInvalid) {
    assert_eq!(broken_sweep(edit), (Err(rule), false));
}

#[test]
fn sweep_documents_that_break_a_rule_the_schema_cannot_say_are_refused() {
    assert_eq!(sweep().check(), Ok(()));
    assert_eq!(minimal_sweep().check(), Ok(()));

    assert_refused_by_check_alone(
        |d| d.candidate = "v1.0.0".to_owned(),
        SweepInvalid::CandidateNotLast("v1.0.0".to_owned()),
    );
    assert_refused_by_check_alone(
        |d| d.versions[1].reference = "v1.0.0".to_owned(),
        SweepInvalid::DuplicateVersion("v1.0.0".to_owned()),
    );
    assert_refused_by_check_alone(
        |d| {
            d.rows[0].cells.pop();
        },
        SweepInvalid::CellsDoNotMatchVersions {
            defect: "F1".to_owned(),
        },
    );
    assert_refused_by_check_alone(
        |d| d.measurements[0].series[0].reference = "v9.9.9".to_owned(),
        SweepInvalid::UnknownVersion {
            what: "a series of `headless_wall_ms`".to_owned(),
            reference: "v9.9.9".to_owned(),
        },
    );
    assert_refused_by_check_alone(
        |d| {
            d.measurements[0].series[0].completed.pop();
        },
        SweepInvalid::SeriesLengths {
            metric: "headless_wall_ms".to_owned(),
            reference: "v1.0.0".to_owned(),
            samples: 3,
            flags: 2,
            rounds: 3,
        },
    );
    assert_refused_by_both(
        |d| d.rows[0].cells[1].reason = None,
        SweepInvalid::NoReason {
            defect: "F1".to_owned(),
            reference: "v1.1.0".to_owned(),
        },
    );
    assert_refused_by_both(
        |d| d.checks[0].reason = Some("odd".to_owned()),
        SweepInvalid::CheckReason {
            check: "signal-term".to_owned(),
            reference: "v1.0.0".to_owned(),
            status: CheckStatus::Ran,
        },
    );
    assert_refused_by_both(
        |d| d.versions[1].build.reason = None,
        SweepInvalid::BuildReason("v1.1.0".to_owned()),
    );
}

#[test]
fn sweep_documents_that_break_a_rule_in_another_place_are_refused_too() {
    assert_refused_by_check_alone(
        |d| d.candidate = "v9.9.9".to_owned(),
        SweepInvalid::CandidateNotLast("v9.9.9".to_owned()),
    );
    assert_refused_by_check_alone(
        |d| d.versions.reverse(),
        SweepInvalid::CandidateNotLast("HEAD".to_owned()),
    );
    assert_refused_by_check_alone(
        |d| d.versions[0].reference = "HEAD".to_owned(),
        SweepInvalid::DuplicateVersion("HEAD".to_owned()),
    );
    assert_refused_by_check_alone(
        |d| d.rows[1].cells.swap(0, 2),
        SweepInvalid::CellsDoNotMatchVersions {
            defect: "F10".to_owned(),
        },
    );
    assert_refused_by_check_alone(
        |d| d.rows[0].cells[2].reference = "v9.9.9".to_owned(),
        SweepInvalid::CellsDoNotMatchVersions {
            defect: "F1".to_owned(),
        },
    );
    assert_refused_by_both(
        |d| d.rows[1].cells[1].reason = None,
        SweepInvalid::NoReason {
            defect: "F10".to_owned(),
            reference: "v1.1.0".to_owned(),
        },
    );
    assert_refused_by_check_alone(
        |d| d.checks[0].reference = "v9.9.9".to_owned(),
        SweepInvalid::UnknownVersion {
            what: "the `signal-term` check".to_owned(),
            reference: "v9.9.9".to_owned(),
        },
    );
    assert_refused_by_check_alone(
        |d| d.measurements[1].series[1].samples.push(1_000.0),
        SweepInvalid::SeriesLengths {
            metric: "tui_complete_ms".to_owned(),
            reference: "HEAD".to_owned(),
            samples: 4,
            flags: 3,
            rounds: 3,
        },
    );
    assert_refused_by_both(
        |d| d.checks[1].reason = None,
        SweepInvalid::CheckReason {
            check: "headless-oracle".to_owned(),
            reference: "v1.1.0".to_owned(),
            status: CheckStatus::NotRun,
        },
    );
    assert_refused_by_both(
        |d| d.checks[3].reason = None,
        SweepInvalid::CheckReason {
            check: "descriptors".to_owned(),
            reference: "HEAD".to_owned(),
            status: CheckStatus::Errored,
        },
    );
    assert_refused_by_both(
        |d| d.versions[0].build.reason = Some("odd".to_owned()),
        SweepInvalid::BuildReason("v1.0.0".to_owned()),
    );
}

#[test]
fn sweep_documents_whose_rounds_do_not_add_up_are_refused() {
    fn du_ratio(document: &mut HarnessSweep) -> &mut SweepRatio {
        document.measurements[0].series[0]
            .ratios
            .get_mut("du")
            .expect("the first series has a ratio to du")
    }
    let ratio_counts = || SweepInvalid::RatioCounts {
        metric: "headless_wall_ms".to_owned(),
        reference: "v1.0.0".to_owned(),
        name: "du".to_owned(),
    };

    // The `headless_wall_ms` measurement ran three rounds. `v1.0.0` has a sample, a flag, and a
    // round for each of them.
    assert_refused_by_check_alone(
        |d| {
            d.measurements[0].series[0].rounds.pop();
        },
        SweepInvalid::SeriesLengths {
            metric: "headless_wall_ms".to_owned(),
            reference: "v1.0.0".to_owned(),
            samples: 3,
            flags: 3,
            rounds: 2,
        },
    );
    for (what, rounds) in [
        ("out of order", vec![0, 2, 1]),
        ("repeated", vec![0, 1, 1]),
        ("past the last round", vec![0, 1, 3]),
    ] {
        let (checked, schema_accepts) =
            broken_sweep(|d| d.measurements[0].series[0].rounds = rounds);
        assert_eq!(
            checked,
            Err(SweepInvalid::SeriesRounds {
                metric: "headless_wall_ms".to_owned(),
                reference: "v1.0.0".to_owned(),
                rounds: 3,
            }),
            "{what}"
        );
        assert!(schema_accepts, "{what}: only `check` can say it");
    }

    // In the `tui_complete_ms` measurement `v1.0.0` has two samples and one skipped round.
    assert_refused_by_check_alone(
        |d| d.measurements[1].series[0].skipped = 2,
        SweepInvalid::SeriesOverrun {
            metric: "tui_complete_ms".to_owned(),
            reference: "v1.0.0".to_owned(),
            samples: 2,
            skipped: 2,
            rounds: 3,
        },
    );

    // The ratio of `v1.0.0` to `du` has two pairs, and a round of `du` is missing.
    assert_refused_by_check_alone(|d| du_ratio(d).median = None, ratio_counts());
    assert_refused_by_check_alone(|d| du_ratio(d).bootstrap_ci = None, ratio_counts());
    assert_refused_by_check_alone(|d| du_ratio(d).pairs = 0, ratio_counts());
    assert_refused_by_check_alone(
        |d| du_ratio(d).lower_bounds = vec![30.0, 31.0],
        ratio_counts(),
    );
    assert_refused_by_check_alone(|d| du_ratio(d).both_censored = 2, ratio_counts());

    assert_refused_by_check_alone(
        |d| {
            d.measurements[0].du_samples.pop();
        },
        SweepInvalid::DuSamples {
            metric: "headless_wall_ms".to_owned(),
            samples: 2,
            rounds: 3,
        },
    );
}

#[test]
fn a_ratio_in_which_no_round_finished_on_both_sides_has_no_median_and_is_valid() {
    let mut document = sweep();
    document.measurements[1].series[0].ratios.insert(
        "candidate".to_owned(),
        SweepRatio {
            median: None,
            bootstrap_ci: None,
            pairs: 0,
            lower_bounds: vec![30.0, 31.0],
            upper_bounds: Vec::new(),
            both_censored: 1,
        },
    );

    assert_eq!(document.check(), Ok(()));
    assert_valid(&document);
    assert_round_trips(&document);
    let value = to_value(&document);
    let ratio = &value["measurements"][1]["series"][0]["ratios"]["candidate"];
    assert!(
        ratio.get("median").is_none() && ratio.get("bootstrap_ci").is_none(),
        "{ratio}"
    );
    assert_eq!(ratio["pairs"], 0);
}

#[test]
fn sweep_documents_that_are_not_this_kind_and_version_or_have_other_numbers_are_rejected() {
    let text = sweep().to_json_pretty().expect("a sweep should render");

    assert!(HarnessSummary::from_json_str(&text).is_err());
    assert!(HarnessCounts::from_json_str(&text).is_err());
    assert!(HarnessSweep::from_json_str(&text.replace("harness-sweep", "harness-ab")).is_err());
    let future = text.replace("\"schema_version\": 1", "\"schema_version\": 2");
    let error = HarnessSweep::from_json_str(&future).expect_err("version 2 is unsupported");
    assert!(
        error.to_string().contains("unsupported schema_version 2"),
        "{error}"
    );
    for malformed in [
        text.replace("\"rounds\": 3", "\"rounds\": 3.5"),
        text.replace("\"pairs\": 2", "\"pairs\": -2"),
        text.replace("\"seed\": 7", "\"seed\": 7.0"),
        text.replace("\"cached\": true", "\"cached\": 1"),
        text.replace("\"tier\": \"full\"", "\"tier\": \"hourly\""),
        text.replace("\"status\": \"built\"", "\"status\": \"cached\""),
        text.replace("\"state\": \"affected\"", "\"state\": \"unaffected\""),
        text.replace("\"cached\": true", "\"cached\": true, \"unexpected\": 1"),
    ] {
        assert!(
            HarnessSweep::from_json_str(&malformed).is_err(),
            "{malformed}"
        );
    }
}

#[test]
fn a_sweep_finds_its_rows_and_cells_by_defect_and_version() {
    let document = sweep();

    assert_eq!(
        document.row("F10").map(|row| row.title.as_str()),
        Some("SIGTERM leaves the scan store behind")
    );
    assert_eq!(
        document.cell("F1", "v1.1.0").map(|cell| cell.state),
        Some(CellState::NotMeasurable)
    );
    assert!(document.row("F99").is_none());
    assert!(document.cell("F1", "v9.9.9").is_none());
    assert!(document.cell("F99", "HEAD").is_none());
}

const TUI_SESSION: &str = "0123abcd";

fn tui_modes(restored: bool) -> Modes {
    Modes {
        alternate_screen: !restored,
        cursor_visible: restored,
        echo: Some(restored),
        icanon: None,
    }
}

/// A screen with every optional part present.
fn tui_screen() -> ScreenInfo {
    ScreenInfo {
        size: Size {
            cols: 120,
            rows: 40,
        },
        cursor: Cursor { row: 39, col: 84 },
        modes: tui_modes(false),
        header_state: Some("COMPLETE".to_owned()),
        selected: Some(SelectedInfo {
            name: "victim.bin".to_owned(),
            state: "◆ COMPLETE".to_owned(),
            kind: "file".to_owned(),
        }),
        filter: Some(FilterInfo {
            input: "vict".to_owned(),
            error: Some("no match".to_owned()),
        }),
        dialog: Some(DialogInfo {
            title: "! DELETE FILE".to_owned(),
            rect: Rect {
                left: 21,
                top: 14,
                right: 98,
                bottom: 25,
            },
            text: "▟ ! DELETE FILE ▜\n▏ /tmp/x/victim.bin ▕".to_owned(),
            delete: Some(DeleteDialogInfo {
                kind: DeleteDialogKind::File,
                path: "/tmp/x/victim.bin".to_owned(),
                confirmation: ConfirmationKind::SingleKey,
            }),
        }),
        boxes: vec![BoxInfo {
            title: "STORAGE MAP".to_owned(),
            rect: Rect {
                left: 0,
                top: 3,
                right: 119,
                bottom: 28,
            },
        }],
        rows: vec![" EXCISE  /tmp/x/  ◆ COMPLETE".to_owned(), String::new()],
    }
}

fn tui_record(index: u64, kind: EventRecordKind) -> EventRecord {
    EventRecord {
        index,
        t_us: 1_000 * index,
        kind,
        version: None,
        pid: None,
        frame_marks: None,
        input_barrier: None,
        seq: None,
        inputs: None,
        barriers: None,
        entries: None,
        removed: None,
        failed: None,
        outcome: None,
        code: None,
    }
}

/// One record of every kind, so that every field of a record is serialized.
fn tui_page() -> EventsPage {
    EventsPage {
        since: 0,
        next: 8,
        records: vec![
            EventRecord {
                version: Some("1.3.0".to_owned()),
                pid: Some(87_432),
                frame_marks: Some(true),
                input_barrier: Some(true),
                ..tui_record(0, EventRecordKind::Hello)
            },
            EventRecord {
                seq: Some(1),
                inputs: Some(0),
                barriers: Some(0),
                ..tui_record(1, EventRecordKind::Frame)
            },
            EventRecord {
                entries: Some(6),
                ..tui_record(2, EventRecordKind::ScanComplete)
            },
            EventRecord {
                seq: Some(2),
                inputs: Some(3),
                barriers: Some(1),
                ..tui_record(3, EventRecordKind::Frame)
            },
            tui_record(4, EventRecordKind::QuitPrompt),
            EventRecord {
                removed: Some(1),
                failed: Some(0),
                ..tui_record(5, EventRecordKind::DeletionFinished)
            },
            EventRecord {
                outcome: Some(RefreshOutcomeKind::Published),
                ..tui_record(6, EventRecordKind::RefreshFinished)
            },
            EventRecord {
                code: Some(0),
                ..tui_record(7, EventRecordKind::Exit)
            },
        ],
    }
}

fn tui_exit() -> ExitInfo {
    ExitInfo {
        code: Some(0),
        signal: None,
        via: ExitVia::Quit,
        terminal_restored: true,
        modes: tui_modes(true),
    }
}

fn tui_key(key: &str, bytes: &str) -> SentKey {
    SentKey {
        key: key.to_owned(),
        bytes: bytes.to_owned(),
    }
}

fn tui_session_info(supervisor_pid: Option<u64>, recording: Option<&str>) -> SessionInfo {
    SessionInfo {
        session: TUI_SESSION.to_owned(),
        state: SessionState::Running,
        fixture: "delete-file".to_owned(),
        profile: Profile::Narrow,
        size: Size { cols: 90, rows: 30 },
        supervisor_pid,
        started_at: "2026-10-04T09:37:11.248Z".to_owned(),
        idle_timeout_ms: 900_000,
        recording: recording.map(str::to_owned),
    }
}

fn tui_success(session: Option<&str>, result: TuiResult) -> HarnessTui {
    HarnessTui::success(session.map(str::to_owned), result)
}

fn tui_open() -> HarnessTui {
    tui_success(
        Some(TUI_SESSION),
        TuiResult::Open(OpenResult {
            fixture: "delete-file".to_owned(),
            profile: Profile::Default,
            size: Size {
                cols: 120,
                rows: 40,
            },
            root: "/tmp/xh-tui-0123abcd/delete-file-1-0".to_owned(),
            session_dir: "target/excise-tui/0123abcd".to_owned(),
            pid: 87_432,
            supervisor_pid: 87_431,
            idle_timeout_ms: 900_000,
            recording: Some("target/excise-tui/0123abcd.cast".to_owned()),
            screen: tui_screen(),
            events: tui_page().digest(),
        }),
    )
}

fn tui_keys() -> HarnessTui {
    tui_success(
        Some(TUI_SESSION),
        TuiResult::Keys(KeysResult {
            sent: vec![tui_key("/", "2f"), tui_key("enter", "0d")],
            settled: true,
            frame: Some(FrameInfo { seq: 2, inputs: 3 }),
            inputs: InputCounts {
                sent: 3,
                consumed: 3,
            },
            screen: tui_screen(),
            events: tui_page().digest(),
            exit: Some(tui_exit()),
        }),
    )
}

fn tui_delete() -> HarnessTui {
    tui_success(
        Some(TUI_SESSION),
        TuiResult::Delete(DeleteResult {
            name: "victim.bin".to_owned(),
            kind: EntryKind::File,
            dialog: tui_screen().dialog.expect("the sample screen has a dialog"),
            sentinels_checked: 5,
            removed: 1,
            failed: 0,
            fixture: FixtureChanges {
                removed: 1,
                unexpected: vec!["changed: keep-a.bin".to_owned()],
            },
            screen: tui_screen(),
            events: tui_page().digest(),
        }),
    )
}

fn tui_close() -> HarnessTui {
    tui_success(
        Some(TUI_SESSION),
        TuiResult::Close(CloseResult {
            exit: tui_exit(),
            screen: tui_screen(),
            events: tui_page().digest(),
            recording: Some("target/excise-tui/0123abcd.cast".to_owned()),
            fixture: FixtureChanges {
                removed: 1,
                unexpected: Vec::new(),
            },
            residue: vec!["scratch/leftover".to_owned()],
            cleanup: Cleanup {
                removed: false,
                problems: vec!["cannot remove the run copy: busy".to_owned()],
            },
        }),
    )
}

fn tui_list() -> HarnessTui {
    tui_success(
        None,
        TuiResult::List(ListResult {
            sessions: vec![
                tui_session_info(Some(87_431), Some("target/excise-tui/0123abcd.cast")),
                SessionInfo {
                    state: SessionState::Starting,
                    ..tui_session_info(None, None)
                },
            ],
            stale: vec![StaleSession {
                session: "89abcdef".to_owned(),
                reason: "its supervisor is gone".to_owned(),
                fixture: Some("delete-file".to_owned()),
                recording: Some("target/excise-tui/89abcdef.cast".to_owned()),
                cleaned: false,
                problems: vec!["cannot remove the run copy: busy".to_owned()],
            }],
        }),
    )
}

/// A failure with every optional part present.
fn tui_failure() -> HarnessTui {
    HarnessTui::failure(
        Some(TuiCommand::Delete),
        Some(TUI_SESSION.to_owned()),
        TuiError {
            kind: TuiErrorKind::NotInView,
            message: "`victim.bin` is not in the folder the session shows".to_owned(),
            sent: Some(vec![tui_key("y", "79")]),
            confirmed: Some(false),
            screen: Some(Box::new(tui_screen())),
            exit: Some(tui_exit()),
        },
    )
}

/// Every document the commands can print: one successful one for each command, and failures.
fn tui_documents() -> Vec<HarnessTui> {
    vec![
        tui_open(),
        tui_keys(),
        tui_delete(),
        tui_success(
            Some(TUI_SESSION),
            TuiResult::Screen(ScreenResult {
                screen: tui_screen(),
            }),
        ),
        // The command that returns every record, frames included.
        tui_success(
            Some(TUI_SESSION),
            TuiResult::Events(EventsResult { events: tui_page() }),
        ),
        tui_close(),
        tui_list(),
        tui_failure(),
        HarnessTui::failure(
            None,
            None,
            TuiError::new(TuiErrorKind::Usage, "name a command"),
        ),
    ]
}

#[test]
fn every_tui_document_validates_and_round_trips() {
    for document in tui_documents() {
        assert_valid(&document);
        assert_round_trips(&document);
    }
}

#[test]
fn the_tui_schema_and_types_declare_exactly_the_same_fields() {
    let root = schema::<HarnessTui>();
    let mut coverage = Coverage::default();
    for document in tui_documents() {
        let value = to_value(&document);
        cover(&root, &root, "", &value, &mut coverage);
        if let Some(result) = value.get("result") {
            let command = value["command"]
                .as_str()
                .expect("a successful document names its command");
            let shape = serde_json::json!({ "$ref": format!("#/$defs/{command}_result") });
            cover(&root, &shape, "", result, &mut coverage);
        }
    }

    let never_serialized: Vec<_> = coverage.declared.difference(&coverage.seen).collect();
    assert!(
        never_serialized.is_empty(),
        "harness-tui declares fields the types never serialize: {never_serialized:?}"
    );
    let defined: BTreeSet<String> = root["$defs"]
        .as_object()
        .expect("the schema has definitions")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        defined, coverage.definitions,
        "harness-tui defines a shape that no serialized field uses"
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one table of the ways a document can drift from the schema, a row at a time"
)]
fn the_tui_schema_rejects_contract_drift() {
    let frame =
        || serde_json::json!({"index": 1, "t_us": 1, "kind": "frame", "seq": 1, "inputs": 0});
    assert_rejected(
        &tui_keys(),
        &[
            ("an undeclared top-level field", &|d| {
                d["extra"] = true.into();
            }),
            ("an undeclared nested field", &|d| {
                d["result"]["screen"]["extra"] = 1.into();
            }),
            ("a failed document that still has its result", &|d| {
                d["ok"] = false.into();
            }),
            ("a result without its session", &|d| remove(d, "/session")),
            (
                "a session that is not eight lowercase hexadecimal digits",
                &|d| {
                    set(d, "/session", "ABCD1234".into());
                },
            ),
            ("a result that is another command's", &|d| {
                set(d, "/command", "screen".into());
            }),
            ("an unsupported schema version", &|d| {
                set(d, "/schema_version", 2.into());
            }),
            ("a frame record among the digest's records", &|d| {
                d["result"]["events"]["records"][0] = frame();
            }),
            ("a frame summary of no frames", &|d| {
                set(d, "/result/events/frames/count", 0.into());
            }),
            ("a frame summary whose last record is not a frame", &|d| {
                set(d, "/result/events/frames/last/kind", "exit".into());
            }),
            ("a digest without its frame summary", &|d| {
                remove(d, "/result/events/frames");
            }),
            ("keys that are not hexadecimal bytes", &|d| {
                set(d, "/result/sent/0/bytes", "ZZ".into());
            }),
            ("a successful document that names no command", &|d| {
                set(d, "/command", Value::Null);
            }),
            ("a frame numbered zero", &|d| {
                set(d, "/result/frame/seq", 0.into());
            }),
            ("an exit that has neither a code nor a signal", &|d| {
                set(d, "/result/exit/code", Value::Null);
            }),
            ("an exit that has both a code and a signal", &|d| {
                set(d, "/result/exit/signal", 9.into());
            }),
        ],
    );
    let events = tui_documents().remove(4);
    assert_rejected(
        &events,
        &[
            ("a page that carries a frame summary", &|d| {
                d["result"]["events"]["frames"] = Value::Null;
            }),
            ("a frame record without its counters", &|d| {
                remove(d, "/result/events/records/1/seq");
            }),
            ("a hello record that carries an exit code", &|d| {
                d["result"]["events"]["records"][0]["code"] = 0.into();
            }),
            (
                "a hello record that does not say whether the frames are marked",
                &|d| {
                    remove(d, "/result/events/records/0/frame_marks");
                },
            ),
            ("a hello record whose frame marks are not a boolean", &|d| {
                set(d, "/result/events/records/0/frame_marks", "yes".into());
            }),
            ("a frame record that carries frame marks", &|d| {
                d["result"]["events"]["records"][1]["frame_marks"] = true.into();
            }),
            (
                "a hello record that does not say whether the program answers the input barrier",
                &|d| {
                    remove(d, "/result/events/records/0/input_barrier");
                },
            ),
            (
                "a hello record whose input barrier is not a boolean",
                &|d| {
                    set(d, "/result/events/records/0/input_barrier", 1.into());
                },
            ),
            ("a frame record that carries an input barrier", &|d| {
                d["result"]["events"]["records"][1]["input_barrier"] = true.into();
            }),
            ("a frame record without its barrier count", &|d| {
                remove(d, "/result/events/records/1/barriers");
            }),
            ("a hello record that carries a barrier count", &|d| {
                d["result"]["events"]["records"][0]["barriers"] = 0.into();
            }),
            ("a frame record whose barrier count is negative", &|d| {
                set(d, "/result/events/records/1/barriers", (-1).into());
            }),
            ("a frame record numbered zero", &|d| {
                set(d, "/result/events/records/1/seq", 0.into());
            }),
        ],
    );
    assert_rejected(
        &tui_list(),
        &[("a list that names a session", &|d| {
            d["session"] = TUI_SESSION.into();
        })],
    );
    assert_rejected(
        &tui_failure(),
        &[
            ("a failure without its error", &|d| remove(d, "/error")),
            ("an error kind that is not one", &|d| {
                set(d, "/error/kind", "mystery".into());
            }),
            ("a failure that also has a result", &|d| {
                d["result"] = serde_json::json!({});
            }),
        ],
    );
}

#[test]
fn the_tui_reader_accepts_only_a_result_that_is_its_commands() {
    let keys = tui_keys().to_json_pretty().expect("a document renders");
    let failed = tui_failure().to_json_pretty().expect("a document renders");

    assert!(HarnessTui::from_json_str(&keys).is_ok());
    for malformed in [
        keys.replace("\"command\": \"keys\"", "\"command\": \"screen\""),
        keys.replace("\"settled\"", "\"unexpected\": 1,\n    \"settled\""),
        keys.replace("\"schema_version\": 1", "\"schema_version\": 2"),
        keys.replace("harness-tui", "harness-ab"),
        failed.replace("\"ok\": false", "\"ok\": true"),
        failed.replace("\"not_in_view\"", "\"mystery\""),
    ] {
        assert!(
            HarnessTui::from_json_str(&malformed).is_err(),
            "{malformed}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The shape profile.

fn histogram(count: u64, total: u64, max: u64, buckets: &[(u64, u64)]) -> ShapeHistogram {
    ShapeHistogram {
        count,
        total,
        max,
        buckets: buckets.iter().copied().collect(),
    }
}

/// The profile of a small tree, with a number in every field: the root holds the folders `a` and
/// `b`, three files, and a link; `a` holds two files and the folder `c`; `c` holds a file. Two of
/// the files are names of one file.
fn shape_profile() -> HarnessShapeProfile {
    HarnessShapeProfile {
        document_kind: ShapeProfileKind::HarnessShapeProfile,
        schema_version: SchemaVersion,
        platform: ShapePlatform {
            os: "linux".to_owned(),
            identity: true,
        },
        walk: ShapeWalk {
            cross_filesystems: false,
            mount_points_skipped: 0,
        },
        entries: ShapeEntries {
            total: 10,
            directories: 3,
            files: 6,
            symlinks: 1,
            others: 0,
        },
        max_depth: 3,
        depths: vec![
            ShapeDepth {
                depth: 1,
                directories: 2,
                files: 3,
                symlinks: 1,
                others: 0,
                bytes: 110,
            },
            ShapeDepth {
                depth: 2,
                directories: 1,
                files: 2,
                symlinks: 0,
                others: 0,
                bytes: 6_000,
            },
            ShapeDepth {
                depth: 3,
                directories: 0,
                files: 1,
                symlinks: 0,
                others: 0,
                bytes: 5_000,
            },
        ],
        children_per_directory: histogram(4, 10, 6, &[(0, 1), (1, 1), (2, 1), (4, 1)]),
        subdirectories_per_directory: histogram(4, 3, 2, &[(0, 2), (1, 1), (2, 1)]),
        files_per_directory: histogram(4, 6, 3, &[(0, 1), (1, 1), (2, 2)]),
        file_sizes: histogram(
            6,
            11_110,
            5_000,
            &[(0, 1), (8, 1), (64, 1), (512, 1), (4_096, 2)],
        ),
        name_lengths: ShapeNameLengths {
            directories: histogram(3, 13, 5, &[(3, 1), (5, 2)]),
            files: histogram(6, 48, 12, &[(4, 2), (7, 1), (9, 1), (12, 2)]),
            symlinks: histogram(1, 6, 6, &[(6, 1)]),
        },
        hard_links: ShapeHardLinks {
            groups: 1,
            names: 2,
            incomplete_groups: 0,
            group_sizes: histogram(1, 2, 2, &[(2, 1)]),
            // The two names are two of the files in the class of 4,096.
            group_sizes_by_file_size: [(4_096, histogram(1, 2, 2, &[(2, 1)]))]
                .into_iter()
                .collect(),
        },
        symbolic_links: ShapeSymbolicLinks { count: 1 },
        unreadable: ShapeUnreadable {
            directories: 0,
            errors: 0,
        },
    }
}

/// The profile of an empty folder: nothing below the root, whose own listing is the one value of
/// each histogram of what a folder holds.
fn minimal_shape_profile() -> HarnessShapeProfile {
    HarnessShapeProfile {
        entries: ShapeEntries {
            total: 0,
            directories: 0,
            files: 0,
            symlinks: 0,
            others: 0,
        },
        max_depth: 0,
        depths: Vec::new(),
        children_per_directory: histogram(1, 0, 0, &[(0, 1)]),
        subdirectories_per_directory: histogram(1, 0, 0, &[(0, 1)]),
        files_per_directory: histogram(1, 0, 0, &[(0, 1)]),
        file_sizes: ShapeHistogram::default(),
        name_lengths: ShapeNameLengths::default(),
        hard_links: ShapeHardLinks::default(),
        symbolic_links: ShapeSymbolicLinks::default(),
        ..shape_profile()
    }
}

#[test]
fn the_shape_profile_samples_obey_the_rules_a_schema_cannot_say() {
    shape_profile().check().expect("a consistent profile");
    minimal_shape_profile()
        .check()
        .expect("a consistent profile");
}

#[test]
#[allow(clippy::too_many_lines)]
fn the_shape_profile_schema_rejects_contract_drift() {
    let many_depths = |d: &mut Value| {
        let level = d["depths"][0].clone();
        d["depths"] = Value::Array(vec![level; 4097]);
    };
    assert_rejected(
        &shape_profile(),
        &[
            ("an undeclared top-level field", &|d| {
                d["extra"] = true.into();
            }),
            ("an undeclared platform field", &|d| {
                d["platform"]["extra"] = 1.into();
            }),
            ("an undeclared walk field", &|d| {
                d["walk"]["extra"] = 1.into();
            }),
            ("an undeclared entries field", &|d| {
                d["entries"]["extra"] = 1.into();
            }),
            ("an undeclared depth field", &|d| {
                d["depths"][0]["name"] = "documents".into();
            }),
            ("an undeclared histogram field", &|d| {
                d["file_sizes"]["extra"] = 1.into();
            }),
            ("an undeclared name length histogram", &|d| {
                d["name_lengths"]["others"] = serde_json::json!({});
            }),
            ("an undeclared hard link field", &|d| {
                d["hard_links"]["extra"] = 1.into();
            }),
            (
                "an undeclared field in the histogram of a class of groups",
                &|d| {
                    d["hard_links"]["group_sizes_by_file_size"]["4096"]["extra"] = 1.into();
                },
            ),
            ("a class of groups that is not a power of two", &|d| {
                let histogram = d["hard_links"]["group_sizes_by_file_size"]["4096"].clone();
                d["hard_links"]["group_sizes_by_file_size"]["3"] = histogram;
            }),
            ("a class of groups written with a leading zero", &|d| {
                let histogram = d["hard_links"]["group_sizes_by_file_size"]["4096"].clone();
                d["hard_links"]["group_sizes_by_file_size"]["04096"] = histogram;
            }),
            (
                "a class of groups that is a count and not a histogram",
                &|d| {
                    d["hard_links"]["group_sizes_by_file_size"]["4096"] = 2.into();
                },
            ),
            ("no groups by file size", &|d| {
                remove(d, "/hard_links/group_sizes_by_file_size");
            }),
            ("an undeclared symbolic link field", &|d| {
                d["symbolic_links"]["target"] = "/etc".into();
            }),
            ("an undeclared unreadable field", &|d| {
                d["unreadable"]["extra"] = 1.into();
            }),
            ("another document kind", &|d| {
                set(d, "/document_kind", "harness-counts".into());
            }),
            ("another schema version", &|d| {
                set(d, "/schema_version", 2.into());
            }),
            ("no platform", &|d| remove(d, "/platform")),
            ("no walk", &|d| remove(d, "/walk")),
            ("no entries", &|d| remove(d, "/entries")),
            ("no depths", &|d| remove(d, "/depths")),
            ("no children histogram", &|d| {
                remove(d, "/children_per_directory");
            }),
            ("no symbolic link name lengths", &|d| {
                remove(d, "/name_lengths/symlinks");
            }),
            ("no unreadable count", &|d| remove(d, "/unreadable")),
            ("a histogram without buckets", &|d| {
                remove(d, "/file_sizes/buckets");
            }),
            ("a depth deeper than the limit", &|d| {
                set(d, "/max_depth", (MAX_PROFILE_DEPTH + 1).into());
            }),
            ("more levels than the limit", &many_depths),
            ("a level at depth 0", &|d| {
                set(d, "/depths/0/depth", 0.into());
            }),
            ("a negative count", &|d| {
                set(d, "/entries/files", (-1).into());
            }),
            ("a count that is not whole", &|d| {
                set(d, "/entries/files", serde_json::json!(6.5));
            }),
            ("a count that is text", &|d| {
                set(d, "/entries/files", "6".into());
            }),
            ("a count above what a double holds", &|d| {
                set(
                    d,
                    "/entries/files",
                    serde_json::json!(9_007_199_254_740_992_u64),
                );
            }),
            ("an operating system in capitals", &|d| {
                set(d, "/platform/os", "Linux".into());
            }),
            ("identity as text", &|d| {
                set(d, "/platform/identity", "yes".into());
            }),
        ],
    );
}

#[test]
fn the_shape_profile_schema_rejects_buckets_that_are_not_what_their_histogram_holds() {
    let class_buckets = ["children_per_directory", "file_sizes"];
    for field in class_buckets {
        let pointer = format!("/{field}/buckets");
        assert_rejected(
            &shape_profile(),
            &[
                ("a class that is not a power of two", &|d| {
                    set(d, &pointer, serde_json::json!({ "3": 1 }));
                }),
                ("a class written with a leading zero", &|d| {
                    set(d, &pointer, serde_json::json!({ "04": 1 }));
                }),
                ("a class past the largest", &|d| {
                    set(
                        d,
                        &pointer,
                        serde_json::json!({ "18446744073709551616": 1 }),
                    );
                }),
                ("a name as a class", &|d| {
                    set(d, &pointer, serde_json::json!({ "documents": 1 }));
                }),
                ("an empty bucket", &|d| {
                    set(d, &pointer, serde_json::json!({ "4": 0 }));
                }),
                ("a negative bucket", &|d| {
                    set(d, &pointer, serde_json::json!({ "4": -1 }));
                }),
                ("a bucket that is not whole", &|d| {
                    set(d, &pointer, serde_json::json!({ "4": 1.5 }));
                }),
                ("a bucket above what a double holds", &|d| {
                    set(
                        d,
                        &pointer,
                        serde_json::json!({ "4": 9_007_199_254_740_992_u64 }),
                    );
                }),
            ],
        );
    }
    for field in ["directories", "files", "symlinks"] {
        let pointer = format!("/name_lengths/{field}/buckets");
        assert_rejected(
            &shape_profile(),
            &[
                ("a length of 0", &|d| {
                    set(d, &pointer, serde_json::json!({ "0": 1 }));
                }),
                ("a length past NAME_MAX", &|d| {
                    set(d, &pointer, serde_json::json!({ "256": 1 }));
                }),
                ("a length with a leading zero", &|d| {
                    set(d, &pointer, serde_json::json!({ "07": 1 }));
                }),
                ("a name as a length", &|d| {
                    set(d, &pointer, serde_json::json!({ "report.pdf": 1 }));
                }),
                ("an empty bucket", &|d| {
                    set(d, &pointer, serde_json::json!({ "7": 0 }));
                }),
            ],
        );
    }
    // The longest length and the largest class are accepted: the limits are the schema's own.
    let validator = validator::<HarnessShapeProfile>(true);
    let mut document = to_value(&shape_profile());
    set(
        &mut document,
        "/name_lengths/files/buckets",
        serde_json::json!({ "255": 1, "1": 1 }),
    );
    set(
        &mut document,
        "/file_sizes/buckets",
        serde_json::json!({ "9223372036854775808": 1 }),
    );
    assert!(validator.is_valid(&document), "the limits are inclusive");
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_shape_profile_that_disagrees_with_itself_is_refused() {
    type Edit = fn(&mut HarnessShapeProfile);
    let cases: [(&str, Edit, &str); 28] = [
        (
            "kinds that do not add up",
            |p| p.entries.total += 1,
            "`entries.total` is 11",
        ),
        (
            "a depth that holds other files than the kinds",
            |p| {
                p.entries.files += 1;
                p.entries.total += 1;
            },
            "the depths hold 6 files and `entries.files` is 7",
        ),
        (
            "a deepest depth that is not the number of levels",
            |p| p.max_depth = 2,
            "`max_depth` is 2 and `depths` has 3 levels",
        ),
        (
            "a level that is out of place",
            |p| p.depths[1].depth = 5,
            "says it is depth 5",
        ),
        (
            "a level that holds nothing",
            |p| {
                p.depths.push(ShapeDepth {
                    depth: 4,
                    directories: 0,
                    files: 0,
                    symlinks: 0,
                    others: 0,
                    bytes: 0,
                });
                p.max_depth = 4;
            },
            "depth 4 holds nothing",
        ),
        (
            "a level below a depth with no folder",
            |p| {
                p.depths[0].directories = 0;
                p.entries.directories -= 2;
                p.entries.total -= 2;
            },
            "holds entries below a depth with no folder",
        ),
        (
            "bytes that do not add up",
            |p| p.file_sizes.total += 1,
            "the depths hold 11110 bytes and `file_sizes.total` is 11111",
        ),
        (
            "a histogram that counts what its buckets do not hold",
            |p| p.children_per_directory.count = 5,
            "`children_per_directory` counts 5 and its buckets hold 4",
        ),
        (
            "a sum its buckets cannot hold",
            |p| p.file_sizes.total = 1,
            "`file_sizes` adds up to 1, which its buckets cannot hold",
        ),
        (
            "a largest value outside the last bucket",
            |p| p.file_sizes.max = 1,
            "the largest value of `file_sizes` is not in its last bucket",
        ),
        (
            "an empty histogram with a largest value",
            |p| {
                p.name_lengths.symlinks = ShapeHistogram {
                    max: 3,
                    ..ShapeHistogram::default()
                };
            },
            "`name_lengths.symlinks` is empty and its largest value is not 0",
        ),
        (
            "a class that is not a power of two",
            |p| p.children_per_directory.buckets.add(3, 1),
            "`children_per_directory` has no bucket 3",
        ),
        (
            "a name length of 0",
            |p| p.name_lengths.files.buckets.add(0, 1),
            "`name_lengths.files` has no bucket 0",
        ),
        (
            "folders listed in two numbers",
            |p| p.subdirectories_per_directory.count = 3,
            "`subdirectories_per_directory` counts 3 folders and `children_per_directory` counts 4",
        ),
        (
            "more folders listed than there are",
            |p| {
                p.children_per_directory = histogram(10, 10, 6, &[(0, 1), (1, 1), (2, 1), (4, 7)]);
                p.subdirectories_per_directory = histogram(10, 3, 2, &[(0, 8), (1, 1), (2, 1)]);
                p.files_per_directory = histogram(10, 6, 3, &[(0, 7), (1, 1), (2, 2)]);
            },
            "more folders were listed than the tree has",
        ),
        (
            "files per folder that do not add up to the files",
            |p| p.files_per_directory.total = 5,
            "`files_per_directory` adds up to 5 and the tree has 6",
        ),
        (
            "names that do not match the folders",
            |p| p.name_lengths.directories = histogram(2, 8, 5, &[(3, 1), (5, 1)]),
            "`name_lengths.directories` counts 2 and the tree has 3",
        ),
        (
            "groups that do not match their sizes",
            |p| p.hard_links.groups = 2,
            "`hard_links.groups` is 2 and `group_sizes` counts 1",
        ),
        (
            "names that do not match their sizes",
            |p| p.hard_links.names = 3,
            "`hard_links.names` is 3 and `group_sizes` adds up to 2",
        ),
        (
            "more incomplete groups than groups",
            |p| p.hard_links.incomplete_groups = 2,
            "more hard-link groups are incomplete than there are groups",
        ),
        (
            "classes of groups that count other groups than the total",
            |p| {
                p.hard_links.group_sizes_by_file_size = [(4_096, histogram(2, 4, 2, &[(2, 2)]))]
                    .into_iter()
                    .collect();
            },
            "`hard_links.groups` is 1 and the classes of `group_sizes_by_file_size` count 2",
        ),
        (
            "a class of groups that holds no group",
            |p| {
                p.hard_links
                    .group_sizes_by_file_size
                    .insert(0, ShapeHistogram::default());
            },
            "`hard_links.group_sizes_by_file_size[0]` holds no group: leave the class out",
        ),
        (
            "a class of groups that is not a power of two",
            |p| {
                p.hard_links
                    .group_sizes_by_file_size
                    .insert(3, histogram(1, 2, 2, &[(2, 1)]));
            },
            "`hard_links.group_sizes_by_file_size` has no class 3",
        ),
        (
            "more linked names in a class than the class has files",
            |p| {
                p.hard_links.group_sizes_by_file_size =
                    [(512, histogram(1, 2, 2, &[(2, 1)]))].into_iter().collect();
            },
            "`hard_links.group_sizes_by_file_size[512]` has 2 names and `file_sizes` has 1 files in the class",
        ),
        (
            "a largest group that the classes do not agree on",
            |p| {
                p.hard_links.group_sizes_by_file_size = [(4_096, histogram(1, 2, 3, &[(2, 1)]))]
                    .into_iter()
                    .collect();
            },
            "the largest group of `group_sizes_by_file_size` is 3 names and `group_sizes` says 2",
        ),
        (
            "links counted in two numbers",
            |p| p.symbolic_links.count = 2,
            "`symbolic_links.count` is 2 and `entries.symlinks` is 1",
        ),
        (
            "mount points skipped by a walk that crosses",
            |p| {
                p.walk.cross_filesystems = true;
                p.walk.mount_points_skipped = 1;
            },
            "a walk that crosses file systems skipped no mount point",
        ),
        (
            "hard links on a platform that cannot find them",
            |p| p.platform.identity = false,
            "a platform without identities finds no mount point and no hard link",
        ),
    ];
    for (what, edit, expected) in cases {
        let mut profile = shape_profile();
        edit(&mut profile);

        let problems = profile
            .check()
            .expect_err(&format!("{what} must be refused"));

        assert!(
            problems.to_string().contains(expected),
            "{what}: `{expected}` is not in `{problems}`"
        );
    }
}

#[test]
fn a_shape_profile_holds_no_string_but_its_kind_and_its_platform() {
    fn strings(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => out.push(text.clone()),
            Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
            Value::Object(members) => members.values().for_each(|member| strings(member, out)),
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
    let mut found = Vec::new();
    strings(&to_value(&shape_profile()), &mut found);
    found.sort();

    assert_eq!(found, ["harness-shape-profile", "linux"]);
}

#[test]
fn the_soak_schema_enumerations_match_the_types() {
    fn names<T: Copy>(all: &[T], name: impl Fn(T) -> &'static str) -> BTreeSet<String> {
        all.iter().map(|&value| name(value).to_owned()).collect()
    }
    fn declared(schema: &Value, pointer: &str) -> BTreeSet<String> {
        schema
            .pointer(pointer)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{pointer} should be an enum"))
            .iter()
            .map(|value| value.as_str().expect("enum names are strings").to_owned())
            .collect()
    }
    let schema = schema::<HarnessSoak>();

    assert_eq!(
        declared(&schema, "/properties/outcome/enum"),
        names(SoakOutcome::ALL, SoakOutcome::as_str)
    );
    assert_eq!(
        declared(&schema, "/$defs/profile/enum"),
        names(Profile::ALL, Profile::as_str)
    );
    assert_eq!(
        declared(&schema, "/$defs/scan_state/enum"),
        names(ScanState::ALL, ScanState::as_str)
    );
    assert_eq!(
        declared(&schema, "/$defs/exit/properties/via/enum"),
        names(ExitVia::ALL, ExitVia::as_str)
    );
    assert_eq!(
        declared(&schema, "/$defs/phase/enum"),
        names(SoakPhase::ALL, SoakPhase::as_str)
    );
    assert_eq!(
        declared(&schema, "/$defs/quirk_count/properties/kind/enum"),
        names(QuirkKind::ALL, QuirkKind::as_str)
    );
    assert_eq!(
        schema["$defs"]["accounting"]["properties"]["headline"]["const"],
        AccountingHeadline::IdentityUniqueAllocatedBytes.as_str()
    );
}

#[test]
fn the_soak_schema_rejects_contract_drift() {
    assert_rejected(
        &soak(),
        &[
            ("an undeclared top-level field", &|d| {
                d["extra"] = true.into();
            }),
            ("an undeclared field of a scan", &|d| {
                d["headless"][0]["extra"] = 1.into();
            }),
            ("an undeclared field of a session", &|d| {
                d["tui"][0]["extra"] = 1.into();
            }),
            ("another document kind", &|d| {
                set(d, "/document_kind", "harness-summary".into());
            }),
            ("another schema version", &|d| {
                set(d, "/schema_version", 2.into());
            }),
            ("a missing outcome", &|d| remove(d, "/outcome")),
            ("an unknown outcome", &|d| {
                set(d, "/outcome", "paused".into());
            }),
            ("a missing digest of the binary", &|d| {
                remove(d, "/excise_sha256");
            }),
            ("a binary path in place of its digest", &|d| {
                set(
                    d,
                    "/excise_sha256",
                    "/Users/someone/excise/target/release/excise".into(),
                );
            }),
            ("a commit that is not 40 hexadecimal digits", &|d| {
                set(d, "/git_sha", "main".into());
            }),
            ("an unknown kind of quirk", &|d| {
                set(d, "/quirks/0/kind", "mystery".into());
            }),
            ("a quirk kind that had no quirk", &|d| {
                set(d, "/quirks/0/count", 0.into());
            }),
            ("a round that is not from 1", &|d| {
                set(d, "/headless/0/round", 0.into());
            }),
            ("a negative wall time", &|d| {
                set(d, "/headless/0/wall_ms", (-1).into());
            }),
            ("a state that is not a scan state", &|d| {
                set(d, "/headless/0/report/state", "mostly".into());
            }),
            ("another accounting headline", &|d| {
                set(
                    d,
                    "/headless/0/report/accounting/headline",
                    "apparent-bytes".into(),
                );
            }),
            ("a path in the counts of a report", &|d| {
                d["headless"][0]["report"]["summary"]["last_unreadable_path"] =
                    "/Users/someone/Private".into();
            }),
            ("an unknown phase", &|d| {
                set(d, "/tui/1/timed_out_phase", "dance".into());
            }),
            ("an unknown way for a program to end", &|d| {
                set(d, "/tui/0/exit/via", "vanished".into());
            }),
            ("a metric that is not a number", &|d| {
                set(d, "/tui/0/metrics/first_frame_ms", "fast".into());
            }),
            ("a metric whose name is a path", &|d| {
                d["tui"][0]["metrics"]["/Users/someone/Private/photos"] = 1.into();
            }),
            ("a metric whose name is a file name", &|d| {
                d["tui"][0]["metrics"]["Holiday Photos.jpg"] = 1.into();
            }),
            ("a metric named like a folder", &|d| {
                d["tui"][0]["metrics"]["private"] = 1.into();
            }),
            ("a metric named like a folder with a year", &|d| {
                d["tui"][0]["metrics"]["payroll-2026"] = 1.into();
            }),
            ("a metric named like a plain word", &|d| {
                d["tui"][0]["metrics"]["photos"] = 1.into();
            }),
            ("a metric named like one that is not quite one", &|d| {
                d["tui"][0]["metrics"]["first_frame"] = 1.into();
            }),
            (
                "the timing of a deletion, which a soak never records",
                &|d| {
                    d["tui"][0]["metrics"]["delete_ms"] = 1.into();
                },
            ),
        ],
    );
}

/// The source of a file of this crate without its test module and without its comment lines, so
/// that what is looked for in it is code.
fn code_of_source(source: &str) -> String {
    source
        .split("\n#[cfg(test)]\n")
        .next()
        .unwrap_or(source)
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The names that the calls matched by `call` (what comes before their arguments) are made with in
/// `source`, when the name is written out as the first argument. A call that is made another way
/// fails the test: a name that is built could be anything.
fn names_in_calls(source: &str, call: &str) -> BTreeSet<String> {
    let any = Regex::new(&format!(r"{call}\s*\(")).expect("a pattern");
    let named = Regex::new(&format!(r#"{call}\s*\(\s*"([a-z0-9_]+)""#)).expect("a pattern");
    assert_eq!(
        any.find_iter(source).count(),
        named.find_iter(source).count(),
        "a call of `{call}` is not made with a name written out: its name could be anything"
    );
    named
        .captures_iter(source)
        .map(|captures| captures[1].to_owned())
        .collect()
}

/// Every metric name that a session of the soak can record, read from the source of the code that
/// records them: `Recorder::finish` (every name it inserts) and the calls of the soak's session
/// driver (`record_metric`, and the measures it starts and stops). Reading the source is what makes
/// the list complete: a name that is added to that code is found here, whoever adds it, and the
/// schema is then made to list it.
fn recorded_metric_names() -> BTreeSet<String> {
    let recorder = code_of_source(include_str!("../metrics/mod.rs"));
    let session = code_of_source(include_str!("../soak/session.rs"));

    let mut names = names_in_calls(&recorder, r"\bmetrics\s*\.\s*insert");
    for call in [r"\brecord_metric", r"\bstart_measure", r"\bstop_measure"] {
        names.extend(names_in_calls(&session, call));
    }
    assert!(
        names.len() >= 24,
        "the code that records was not read: {names:?}"
    );
    names
}

#[test]
fn the_soak_schema_lists_every_metric_a_session_can_record_and_no_other() {
    let mut recorded = recorded_metric_names();
    // A soak confirms no deletion, so a session never records the timing of one, although the
    // recorder that every runner shares can.
    let session = code_of_source(include_str!("../soak/session.rs"));
    assert!(
        !session.contains("record_deletion_confirmed"),
        "a soak confirms no deletion"
    );
    assert!(
        recorded.remove("delete_ms"),
        "the recorder times a deletion"
    );

    let schema = schema::<HarnessSoak>();
    let listed: BTreeSet<String> = schema
        .pointer("/$defs/metric_name/enum")
        .and_then(Value::as_array)
        .expect("the metric names are an enumeration")
        .iter()
        .map(|name| name.as_str().expect("a name is a string").to_owned())
        .collect();

    assert_eq!(listed, recorded);
    assert_eq!(
        schema["$defs"]["tui"]["properties"]["metrics"]["propertyNames"],
        serde_json::json!({ "$ref": "#/$defs/metric_name" }),
        "the keys of a session's metrics are those names, and no pattern"
    );
}

#[test]
fn a_session_that_records_every_metric_of_the_soak_validates() {
    let mut document = soak();
    document.tui[0].metrics = recorded_metric_names()
        .into_iter()
        .filter(|name| name != "delete_ms")
        .map(|name| (name, 1.0))
        .collect();

    assert_valid(&document);
}

#[test]
fn the_soak_schema_has_no_free_text() {
    // Every string the document can hold is an enumerated word, a constant, or has a pattern, so
    // that a path or a name from the tree cannot be written into it and still validate. A string
    // with none of the three would be free text, and this fails for it.
    fn walk(schema: &Value, at: &str) {
        let is_string = schema.get("type").is_some_and(|kind| {
            kind == "string"
                || kind
                    .as_array()
                    .is_some_and(|kinds| kinds.iter().any(|kind| kind == "string"))
        });
        if is_string {
            assert!(
                ["enum", "const", "pattern"]
                    .iter()
                    .any(|keyword| schema.get(keyword).is_some()),
                "{at} is a string with no enum, const, or pattern: free text"
            );
        }
        for key in ["properties", "$defs"] {
            for (name, child) in schema
                .get(key)
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                walk(child, &format!("{at}/{key}/{name}"));
            }
        }
        for key in ["items", "additionalProperties", "propertyNames"] {
            if let Some(child) = schema.get(key).filter(|child| child.is_object()) {
                walk(child, &format!("{at}/{key}"));
            }
        }
    }

    walk(&schema::<HarnessSoak>(), "#");
}

#[test]
fn no_string_of_a_soak_document_accepts_a_path_or_a_name() {
    // Every string the sample holds is replaced, one at a time, by a path and then by a file name,
    // and the schema must reject each: the document cannot carry what names the tree.
    fn strings(value: &Value, at: &str, found: &mut Vec<String>) {
        match value {
            Value::String(_) => found.push(at.to_owned()),
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    strings(item, &format!("{at}/{index}"), found);
                }
            }
            Value::Object(members) => {
                for (name, member) in members {
                    strings(member, &format!("{at}/{name}"), found);
                }
            }
            _ => {}
        }
    }
    let document = to_value(&soak());
    let validator = validator::<HarnessSoak>(true);
    let mut pointers = Vec::new();
    strings(&document, "", &mut pointers);

    assert!(pointers.len() >= 10, "{pointers:?}");
    for pointer in pointers {
        for intruder in ["/Users/someone/Private/photos", "Holiday Photos.jpg"] {
            let mut changed = document.clone();
            set(&mut changed, &pointer, intruder.into());
            assert!(
                !validator.is_valid(&changed),
                "{pointer} accepts {intruder:?}: the document could carry a name"
            );
        }
    }
}
