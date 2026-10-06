//! The table as text: a grid for the terminal, and the detail behind every cell for the file.

use std::fmt::Write as _;

use crate::report::{BuildStatus, CellState, HarnessSweep};

/// The three letters of a cell in the grid.
const fn symbol(state: CellState) -> &'static str {
    match state {
        CellState::Affected => "AFF",
        CellState::NotAffected => "ok",
        CellState::NotMeasurable => "n/m",
    }
}

/// The version-by-defect grid: one row per defect, one column per version, then what each defect
/// is and the legend. This is what the command prints.
#[must_use]
pub fn grid(document: &HarnessSweep) -> String {
    let id_width = document
        .rows
        .iter()
        .map(|row| row.defect.chars().count())
        .max()
        .unwrap_or(2);
    let widths: Vec<usize> = document
        .versions
        .iter()
        .map(|version| version.reference.chars().count().max(3))
        .collect();
    let mut out = String::new();
    let _ = write!(out, "{:id_width$}", "");
    for (version, width) in document.versions.iter().zip(&widths) {
        let _ = write!(out, "  {:<width$}", version.reference);
    }
    out.push('\n');
    for row in &document.rows {
        let _ = write!(out, "{:<id_width$}", row.defect);
        for (cell, width) in row.cells.iter().zip(&widths) {
            let _ = write!(out, "  {:<width$}", symbol(cell.state));
        }
        out.push('\n');
    }
    out.push('\n');
    for row in &document.rows {
        let _ = writeln!(out, "{:<id_width$}  {}", row.defect, row.title);
    }
    let _ = writeln!(
        out,
        "\nAFF affected, ok not affected, n/m not measurable. Ratios are to the candidate, {}.",
        document.candidate
    );
    out
}

/// The detail of the table: the builds, then for every defect its changelog entry, how it was
/// measured, and every cell with its reason, value, and evidence.
#[must_use]
pub fn detail(document: &HarnessSweep) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Sweep {} ({} tier), candidate {}\n",
        document.run_id, document.tier, document.candidate
    );
    let _ = writeln!(out, "Builds");
    for version in &document.versions {
        let toolchain = version.toolchain.as_ref().map_or_else(
            || "no toolchain".to_owned(),
            |toolchain| format!("{} ({})", toolchain.channel, toolchain.rustc),
        );
        let status = match version.build.status {
            BuildStatus::Built if version.build.cached => "built (cached)".to_owned(),
            BuildStatus::Built => "built".to_owned(),
            BuildStatus::Failed => format!(
                "build failed: {}",
                version.build.reason.as_deref().unwrap_or("no reason given")
            ),
        };
        let _ = writeln!(
            out,
            "  {:<8} {}  {toolchain}; {status}",
            version.reference,
            &version.sha[..version.sha.len().min(12)]
        );
    }
    for row in &document.rows {
        let _ = writeln!(out, "\n{}  {}", row.defect, row.title);
        let _ = writeln!(out, "  changelog: {}", row.changelog);
        let _ = writeln!(out, "  measured by: {}", row.measured_by);
        if let Some(note) = &row.note {
            let _ = writeln!(out, "  note: {note}");
        }
        for cell in &row.cells {
            let _ = writeln!(out, "  {:<8} {}", cell.reference, cell.state);
            if let Some(value) = &cell.value {
                let _ = writeln!(out, "           value: {value}");
            }
            if let Some(reason) = &cell.reason {
                let _ = writeln!(out, "           why: {reason}");
            }
            if !cell.evidence.is_empty() {
                let _ = writeln!(out, "           evidence: {}", cell.evidence.join(", "));
            }
        }
    }
    out
}
