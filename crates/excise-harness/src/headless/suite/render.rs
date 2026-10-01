//! What a suite shows: the `harness-summary` rows, the failure directories, and the table.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    headless::diff::Diff,
    report::{ScenarioResult, Verdict},
    run_support::{format_ms, median, render_rows, worst},
};

use super::{FixtureReport, RATIO_BUDGET, Round, ScanRun, SuiteReport, Volumes, millis};

/// How many discrepancies of a failing fixture the table lists.
const TABLE_LISTED: usize = 8;
/// How many characters of a discrepancy the table shows; the evidence file has all of it.
const TABLE_WIDTH: usize = 200;

/// The name a fixture's result has in the summary: `headless-<fixture id>`, within the 64
/// characters a summary name may have.
fn scenario_name(fixture: &str) -> String {
    let mut name = format!("headless-{fixture}");
    name.truncate(64);
    name
}

fn put(metrics: &mut BTreeMap<String, f64>, name: &str, value: Option<f64>) {
    if let Some(value) = value.filter(|value| value.is_finite()) {
        metrics.insert(name.to_owned(), value);
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "counts and byte sizes far below 2^52"
)]
fn number(value: u64) -> f64 {
    value as f64
}

/// The `harness-summary` row of a fixture.
pub(super) fn result_of(report: &FixtureReport) -> ScenarioResult {
    let mut metrics = BTreeMap::new();
    put(&mut metrics, "entries", Some(number(report.entries)));
    put(
        &mut metrics,
        "runs",
        Some(number(report.measured().count() as u64)),
    );
    put(&mut metrics, "oracle_ms", Some(millis(report.oracle_time)));
    put(&mut metrics, "generation_ms", report.generation.map(millis));
    let scans = report.scan_millis();
    put(&mut metrics, "headless_ms", median(&scans));
    put(
        &mut metrics,
        "headless_ms_min",
        scans.iter().copied().reduce(f64::min),
    );
    put(&mut metrics, "headless_ms_max", worst(&scans));
    put(&mut metrics, "du_ms", median(&report.du_millis()));
    if let Some(spread) = report.ratio_spread() {
        put(&mut metrics, "headless_scan_ratio", Some(spread.median));
        put(&mut metrics, "headless_scan_ratio_min", Some(spread.min));
        put(&mut metrics, "headless_scan_ratio_q1", Some(spread.q1));
        put(&mut metrics, "headless_scan_ratio_q3", Some(spread.q3));
        put(&mut metrics, "headless_scan_ratio_max", Some(spread.max));
    }
    let last_du = report
        .rounds
        .iter()
        .rev()
        .find_map(|round| round.du.as_ref().and_then(|du| du.kib));
    put(&mut metrics, "du_kib", last_du.map(number));
    put(
        &mut metrics,
        "du_expected_kib",
        report.du_expected_kib.map(number),
    );
    put(
        &mut metrics,
        "du_matches_oracle",
        report
            .du_matches_oracle()
            .map(|matches| f64::from(u8::from(matches))),
    );
    let measured: Vec<&Round> = report.measured().collect();
    let cpu = |pick: fn(&crate::metrics::CpuTimes) -> std::time::Duration| -> Vec<f64> {
        measured
            .iter()
            .filter_map(|round| round.scan.cpu.as_ref())
            .map(|cpu| millis(pick(cpu)))
            .collect()
    };
    put(&mut metrics, "user_ms", median(&cpu(|cpu| cpu.user)));
    put(&mut metrics, "sys_ms", median(&cpu(|cpu| cpu.system)));
    put(
        &mut metrics,
        "peak_rss_bytes",
        report
            .rounds
            .iter()
            .filter_map(|round| round.scan.peak_memory_bytes)
            .max()
            .map(number),
    );
    if let Some(round) = report.rounds.first()
        && let Some(code) = round.scan.ended.code()
    {
        put(&mut metrics, "exit_code", Some(f64::from(code)));
    }
    if let Some((_, diff)) = report.first_failure() {
        put(&mut metrics, "discrepancies", Some(number(diff.total())));
        for (kind, count) in &diff.counts {
            put(
                &mut metrics,
                &format!("discrepancies_{kind}"),
                Some(number(*count)),
            );
        }
    } else if !report.diffs.is_empty() {
        put(&mut metrics, "discrepancies", Some(0.0));
    }
    ScenarioResult {
        name: scenario_name(&report.fixture),
        profile: report.profile,
        verdict: report.verdict,
        duration_ms: u64::try_from(report.duration.as_millis()).unwrap_or(u64::MAX),
        metrics,
        failure_bundle: report
            .failure_dir
            .as_ref()
            .map(|directory| directory.display().to_string()),
    }
}

/// Writes the evidence of a failing fixture to `<run_dir>/<name>/`: every discrepancy of the first
/// failing run in `discrepancies.txt`, the command that reruns the fixture in `repro.txt`, and
/// that run's report. Returns the directory, or `None` when it cannot be written.
pub(super) fn write_failure_dir(
    run_dir: &Path,
    report: &FixtureReport,
    runs: &[(usize, ScanRun)],
) -> Option<PathBuf> {
    let (round, diff) = report.first_failure()?;
    let directory = run_dir.join(scenario_name(&report.fixture));
    fs::create_dir_all(&directory).ok()?;
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{} [{}] {}: {} discrepancies, {}\n",
        report.fixture,
        report.profile,
        run_label(*round),
        diff.total(),
        report.verdict
    );
    if let Some(expected) = &report.expectation {
        let _ = writeln!(text, "expected failure {expected}\n");
    }
    write_discrepancies(&mut text, diff, usize::MAX, None);
    fs::write(directory.join("discrepancies.txt"), text).ok()?;
    fs::write(
        directory.join("repro.txt"),
        format!(
            "cargo xtask headless --fixture {} --profile {} --repeat 1 --keep-scratch\n",
            report.fixture, report.profile
        ),
    )
    .ok()?;
    if let Some((_, run)) = runs.iter().find(|(index, _)| index == round) {
        let _ = fs::copy(run.report_path(), directory.join("scan-report.json"));
    }
    Some(directory)
}

/// Lists the first `limit` discrepancies of `diff`, each line cut to `width` characters when a
/// width is given, and counts the rest.
fn write_discrepancies(text: &mut String, diff: &Diff, limit: usize, width: Option<usize>) {
    for discrepancy in diff.discrepancies.iter().take(limit) {
        let line = discrepancy.to_string();
        match width {
            Some(width) if line.chars().count() > width => {
                let cut: String = line.chars().take(width).collect();
                let _ = writeln!(text, "  {cut}...");
            }
            _ => {
                let _ = writeln!(text, "  {line}");
            }
        }
    }
    let listed = diff.discrepancies.len().min(limit);
    let unlisted = usize::try_from(diff.total())
        .unwrap_or(usize::MAX)
        .saturating_sub(listed);
    if unlisted > 0 {
        let _ = writeln!(text, "  ... and {unlisted} more");
    }
}

/// What a round is called: the first is the warm-up.
fn run_label(round: usize) -> String {
    if round == 0 {
        "warm-up run".to_owned()
    } else {
        format!("run {round}")
    }
}

impl SuiteReport {
    /// The verdict table: one line per fixture, then one block per fixture that is not clean, then
    /// the overall verdict.
    #[must_use]
    pub fn table(&self) -> String {
        let mut table = render_rows(&self.rows());
        for fixture in &self.fixtures {
            write_fixture_block(&mut table, fixture);
        }
        self.write_notes(&mut table);
        let blocking = self
            .fixtures
            .iter()
            .filter(|fixture| fixture.verdict.blocks_run())
            .count();
        let expected = self
            .fixtures
            .iter()
            .filter(|fixture| fixture.verdict == Verdict::Xfail)
            .count();
        let _ = writeln!(
            table,
            "\nheadless {}: {} fixture(s), {blocking} blocking, {expected} expected failure(s); summary: {}",
            if blocking == 0 { "ok" } else { "FAILED" },
            self.fixtures.len(),
            self.summary_path.display()
        );
        table
    }

    fn rows(&self) -> Vec<[String; 9]> {
        let mut rows = vec![
            [
                "fixture",
                "classes",
                "entries",
                "verdict",
                "headless (median)",
                "du -sk (median)",
                "ratio (median)",
                "ratio (min-max)",
                "oracle diff",
            ]
            .map(str::to_owned),
        ];
        for fixture in &self.fixtures {
            rows.push(row_of(fixture));
        }
        rows
    }

    fn write_notes(&self, table: &mut String) {
        let detached: Vec<&str> = self
            .fixtures
            .iter()
            .filter(|fixture| fixture.volumes == Volumes::Detached)
            .map(|fixture| fixture.fixture.as_str())
            .collect();
        if !detached.is_empty() {
            let _ = writeln!(
                table,
                "\nvolumes were not attached, so their mount points are empty directories and no boundary was crossed, on: {} ({}=1 attaches them)",
                detached.join(", "),
                crate::fixture::PRIVILEGED_ENV
            );
        }
        self.write_ratio_notes(table);
    }

    fn write_ratio_notes(&self, table: &mut String) {
        let Some(flavor) = self.du else {
            let _ = writeln!(table, "\nno `du` on this machine: no ratio was measured");
            return;
        };
        let over: Vec<&str> = self
            .fixtures
            .iter()
            .filter(|fixture| {
                fixture
                    .ratio_spread()
                    .is_some_and(|spread| spread.median > RATIO_BUDGET)
            })
            .map(|fixture| fixture.fixture.as_str())
            .collect();
        let measured = self
            .fixtures
            .iter()
            .filter(|fixture| fixture.ratio_spread().is_some())
            .count();
        let _ = writeln!(
            table,
            "\nratio: scan time over {} time, median of the interleaved pairs; the budget is {RATIO_BUDGET}x (reported, not gated here)",
            flavor.as_str()
        );
        if !over.is_empty() {
            let _ = writeln!(
                table,
                "above the {RATIO_BUDGET}x budget on {} of {measured}: {}",
                over.len(),
                over.join(", ")
            );
        }
        let differ: Vec<&str> = self
            .fixtures
            .iter()
            .filter(|fixture| fixture.du_matches_oracle() == Some(false))
            .map(|fixture| fixture.fixture.as_str())
            .collect();
        if !differ.is_empty() {
            let _ = writeln!(
                table,
                "`du -sk` printed another total than the oracle predicts, so it did not walk the same tree, on: {}",
                differ.join(", ")
            );
        }
    }
}

fn row_of(fixture: &FixtureReport) -> [String; 9] {
    let classes: Vec<&str> = fixture.classes.iter().map(|class| class.as_str()).collect();
    let spread = fixture.ratio_spread();
    let ratio = spread.map_or_else(
        || "-".to_owned(),
        |spread| {
            let over = if spread.median > RATIO_BUDGET {
                " >3x"
            } else {
                ""
            };
            format!("{:.1}x{over}", spread.median)
        },
    );
    let range = spread.map_or_else(
        || "-".to_owned(),
        |spread| format!("{:.1}-{:.1}", spread.min, spread.max),
    );
    let diff = match (&fixture.error, fixture.first_failure()) {
        (Some(_), _) => "could not run".to_owned(),
        (None, Some((_, diff))) => {
            let kinds: Vec<&str> = diff.counts.keys().map(|kind| kind.as_str()).collect();
            let findings = fixture
                .expectation
                .as_ref()
                .map_or_else(String::new, |expected| {
                    format!(" [{}]", expected.findings.join(", "))
                });
            format!(
                "{} discrepancies: {}{findings}",
                diff.total(),
                kinds.join(", ")
            )
        }
        (None, None) if fixture.diffs.is_empty() => "-".to_owned(),
        (None, None) => format!("clean ({} run(s))", fixture.diffs.len()),
    };
    [
        fixture.fixture.clone(),
        classes.join("+"),
        fixture.entries.to_string(),
        fixture.verdict.as_str().to_owned(),
        format_ms(median(&fixture.scan_millis())),
        format_ms(median(&fixture.du_millis())),
        ratio,
        range,
        diff,
    ]
}

/// The block under the table for a fixture that is not clean.
fn write_fixture_block(table: &mut String, fixture: &FixtureReport) {
    if let Some(error) = &fixture.error {
        let _ = writeln!(
            table,
            "\nERROR {}: the harness could not run it: {error}",
            fixture.fixture
        );
        return;
    }
    let Some((round, diff)) = fixture.first_failure() else {
        if fixture.verdict == Verdict::Xpass {
            let _ = writeln!(
                table,
                "\nXPASS {}: expected to fail ({}) but its diff is clean; remove its entry from expectations/headless.toml",
                fixture.fixture,
                fixture
                    .expectation
                    .as_ref()
                    .map_or_else(String::new, ToString::to_string)
            );
        }
        return;
    };
    let _ = writeln!(
        table,
        "\n{} {} [{}] {}: {} discrepancies",
        fixture.verdict.as_str().to_uppercase(),
        fixture.fixture,
        fixture.profile,
        run_label(*round),
        diff.total()
    );
    if let Some(expected) = &fixture.expectation {
        let _ = writeln!(table, "  expected failure {expected}");
        if fixture.verdict == Verdict::Fail {
            let want: Vec<&str> = expected.kinds.iter().map(|kind| kind.as_str()).collect();
            let got: Vec<&str> = fixture.kinds().iter().map(|kind| kind.as_str()).collect();
            let _ = writeln!(
                table,
                "  it must fail with exactly the kinds {}; it failed with {}",
                want.join(", "),
                got.join(", ")
            );
        }
    }
    write_discrepancies(table, diff, TABLE_LISTED, Some(TABLE_WIDTH));
    if let Some(directory) = &fixture.failure_dir {
        let _ = writeln!(table, "  evidence: {}", directory.display());
    }
    if let Some(kept) = &fixture.kept {
        let _ = writeln!(table, "  kept fixture scratch: {}", kept.display());
    }
}
