//! The rows of the table: the product defects fixed in the release, each tied to its entry in
//! `CHANGELOG.md`.
//!
//! The ids are the numbers of the validation program's findings. A finding that is not a product
//! defect fixed in the release has no row: F17 was withdrawn, F20 is accepted and not fixed, F25
//! was introduced after v1.3.0 by unreleased work, F26 is deferred past the release, and F27 to F30
//! and F23a concern the harness only. The changelog does not number its entries, so a row names
//! its entry by the words it opens with, and a test holds every row to a line that is there. The
//! release notes record some fixes together, and a pair of rows can then name one entry (F11 and
//! F19: folders nested deeper than the path length limit are scanned and can be deleted); the test
//! lists those pairs.

/// A defect fixed in the release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Defect {
    /// The finding: `F1`, `F10`, `F23b`.
    pub id: &'static str,
    /// What the defect is, in a line.
    pub title: &'static str,
    /// The words the `CHANGELOG.md` entry that records the fix opens with, exactly as written
    /// there.
    pub changelog: &'static str,
    /// How the sweep measures it, or why it cannot.
    pub measured_by: &'static str,
}

/// Why no sweep can decide a defect that only a deletion shows. Every build the sweep runs is
/// started without frame marks and without the input barrier, so the harness's own guard sends it
/// no key that could confirm a deletion, and no check of the sweep sends one either.
pub const DELETION_REASON: &str = "no confirmation is sent to a build without frame marks";

/// The defects, in the order of the table.
pub const DEFECTS: &[Defect] = &[
    Defect {
        id: "F1",
        title: "per-run durable writes dominate scanning",
        changelog: "Large folders scan much faster.",
        measured_by: "paired headless scan time against the candidate (and against `du -sk`)",
    },
    Defect {
        id: "F2",
        title: "scan ingestion stalls when the terminal reads slower than excise writes (default motion far worse)",
        changelog: "Scanning no longer stalls on a slow terminal, such as SSH over a slow link",
        measured_by: "paired time to COMPLETE on the header against a terminal that drains 150 KB/s, in both motion modes, against the candidate",
    },
    Defect {
        id: "F3",
        title: "idle animation loop",
        changelog: "An idle map no longer redraws constantly, which wrote almost 1 MB of output per second.",
        measured_by: "terminal output bytes and CPU over a 5 s window that starts 3.2 s after COMPLETE with no input",
    },
    Defect {
        id: "F4",
        title: "leftover scan-store directories",
        changelog: "A killed or crashed run no longer leaves its scan directory",
        measured_by: "what a SIGKILL leaves in the scratch area, and whether a second start that ends cleanly sweeps it",
    },
    Defect {
        id: "F5",
        title: "selection drift after COMPLETE (observed once)",
        changelog: "The selection no longer jumps to another entry when the map refreshes during a scan.",
        measured_by: "reproduction on the selection-drift fixture: the entry the selected-item pane shows before and after COMPLETE (macOS, full tier)",
    },
    Defect {
        id: "F6",
        title: "signals skip all cleanup",
        changelog: "Closing the terminal, SIGTERM, SIGHUP, and SIGQUIT no longer leave the terminal raw",
        measured_by: "exit status, terminal restoration, and residue after SIGTERM, SIGHUP, and SIGQUIT delivered once the scan is idle (not SIGQUIT on macOS, where its default action makes the system write a crash report into the person's home folder; on Windows the console is closed instead)",
    },
    Defect {
        id: "F7",
        title: "macOS scan-store quota about 256x too large",
        changelog: "On macOS, the scan data's space budget now uses the volume's real free space",
        measured_by: "the scan-store limit the headless report states, against the free space statvfs reports for the volume of the scan store",
    },
    Defect {
        id: "F8",
        title: "Windows owner loop stalls after every key release",
        changelog: "On Windows, the map no longer freezes after a key release until the next input.",
        measured_by: "Windows only: needs the Windows console's key-release events",
    },
    Defect {
        id: "F9",
        title: "a deletion can leave an empty file under a deleted file's name, and the folder survives",
        changelog: "On macOS, deleting a folder no longer occasionally leaves it behind",
        measured_by: "needs a confirmed deletion",
    },
    Defect {
        id: "F10",
        title: "unreadable folders, and every folder above them, reported complete with exact bounds",
        changelog: "A folder that cannot be read, and every folder above it, now reports `uncertain`",
        measured_by: "the headless oracle diff of the hostile-small fixture: an `entry-state` discrepancy",
    },
    Defect {
        id: "F11",
        title: "nothing deeper than PATH_MAX is scanned",
        changelog: "On macOS and Linux, folders nested deeper than the path length limit are now scanned and can be deleted",
        measured_by: "the headless oracle diff of the deep-past-path-max fixture: a `missing` discrepancy",
    },
    Defect {
        id: "F12",
        title: "a full scratch volume reported as a generic failure",
        changelog: "Running out of disk space during a scan is now reported as such",
        measured_by: "needs a scratch volume that fills during the scan",
    },
    Defect {
        id: "F13",
        title: "open descriptors grow with the tree, and a large scan fails at the stock macOS descriptor limit",
        changelog: "Large scans no longer fail with \"Too many open files\" at a low limit",
        measured_by: "the most descriptors the program holds while it scans a 49,050-entry tree, against a 1,002-entry tree (full tier)",
    },
    Defect {
        id: "F14",
        title: "the deletion-history export (Shift+E) blocks the interface for seconds after a large deletion",
        changelog: "Exporting the deletion history (`Shift+E`) runs in the background",
        measured_by: "needs a confirmed deletion",
    },
    Defect {
        id: "F15",
        title: "on Windows a scan can end on \"scan results unavailable\" instead of COMPLETE",
        changelog: "On Windows, a scan no longer occasionally fails with \"Excise could not build a complete folder map\"",
        measured_by: "Windows only: needs another program to hold a scan-store file",
    },
    Defect {
        id: "F16",
        title: "the headless report is written unbuffered (one write per JSON token)",
        changelog: "JSON reports (`--format json`) and the interactive exports are written much faster",
        measured_by: "paired time the report file takes to grow, from its first byte to the last change in its length, against the candidate",
    },
    Defect {
        id: "F18",
        title: "the interface reaches COMPLETE 1.2-2.4x later than headless",
        changelog: "The map stays responsive while a large scan is finalized",
        measured_by: "paired time to COMPLETE on the header against the headless scan of the same version without the writing of its report, in the same round, against the 1.25 budget",
    },
    Defect {
        id: "F19",
        title: "a folder with a path longer than PATH_MAX cannot be deleted",
        changelog: "On macOS and Linux, folders nested deeper than the path length limit are now scanned and can be deleted",
        measured_by: "needs a confirmed deletion",
    },
    Defect {
        id: "F21",
        title: "on macOS deletion planning panics on a device node with major 128 or more, or a negative size",
        changelog: "On macOS, planning a deletion no longer panics on a device node with a high major number",
        measured_by: "needs a confirmed deletion",
    },
    Defect {
        id: "F22",
        title: "a filter whose matches lie more than one level below the current folder crashes the program",
        changelog: "Applying a filter that matches entries more than one level deep no longer crashes Excise",
        measured_by: "the exit status after a filter whose matches lie two and three levels down, applied at the root and inside an opened folder",
    },
    Defect {
        id: "F23b",
        title: "the untouched cursor does not land on the largest entry at COMPLETE",
        changelog: "Until you move the cursor or click, it lands on the current folder's largest entry",
        measured_by: "the entry the selected-item pane shows at COMPLETE, with no key pressed, against the largest entry of the fixture; a cursor on the largest entry is read as not affected on Windows only, where a small entry is listed first",
    },
    Defect {
        id: "F24",
        title: "on Windows closing the console can end with an NTSTATUS instead of exit code 130",
        changelog: "On Windows, closing the console no longer occasionally exits with a Windows status",
        measured_by: "Windows only: the exit status after the console closes",
    },
];

/// The defect with `id`, if the table has a row for it.
#[must_use]
pub fn defect(id: &str) -> Option<&'static Defect> {
    DEFECTS.iter().find(|defect| defect.id == id)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        path::Path,
    };

    use super::*;

    /// The part of `CHANGELOG.md` newer than the latest release the program compares with: from
    /// the top to the heading of v1.3.0. Before the release that is `[Unreleased]`, and after it
    /// `[1.4.0]`, so the test keeps holding the rows to the words they were written from.
    fn newer_than_1_3_0() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../CHANGELOG.md");
        let text = fs::read_to_string(&path).expect("CHANGELOG.md is readable");
        let end = text
            .find("## [1.3.0]")
            .expect("CHANGELOG.md has the v1.3.0 heading");
        text[..end].to_owned()
    }

    #[test]
    fn every_row_has_a_distinct_id_and_the_fields_a_reader_needs() {
        let ids: BTreeSet<&str> = DEFECTS.iter().map(|defect| defect.id).collect();
        assert_eq!(ids.len(), DEFECTS.len(), "every id is distinct");
        for defect in DEFECTS {
            assert!(defect.id.starts_with('F'), "{}", defect.id);
            assert!(!defect.title.is_empty(), "{}", defect.id);
            assert!(!defect.measured_by.is_empty(), "{}", defect.id);
            assert!(
                defect.changelog.chars().count() >= 30,
                "{}: an entry is named by enough of its words to be found",
                defect.id
            );
        }
    }

    #[test]
    fn the_rows_are_the_findings_the_release_fixes() {
        let ids: Vec<&str> = DEFECTS.iter().map(|defect| defect.id).collect();
        assert_eq!(
            ids,
            [
                "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13",
                "F14", "F15", "F16", "F18", "F19", "F21", "F22", "F23b", "F24"
            ]
        );
    }

    /// Rows whose fixes the changelog records in one entry, so that they name the same line: the
    /// notes say once that deep folders are scanned and can be deleted.
    const SHARED_ENTRIES: &[&[&str]] = &[&["F11", "F19"]];

    #[test]
    fn every_row_is_tied_to_a_line_of_the_changelog_and_only_rows_recorded_together_share_one() {
        let changelog = newer_than_1_3_0();
        let mut rows_by_line: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for defect in DEFECTS {
            let found = changelog.matches(defect.changelog).count();
            assert!(
                found >= 1,
                "{}: the changelog has no entry that opens with `{}`",
                defect.id,
                defect.changelog
            );
            let line = changelog
                .lines()
                .find(|line| line.contains(defect.changelog))
                .expect("a line was found above");
            assert!(
                line.trim_start_matches("* ").starts_with(defect.changelog),
                "{}: `{}` is not where its entry opens",
                defect.id,
                defect.changelog
            );
            rows_by_line.entry(line).or_default().push(defect.id);
        }
        for (line, rows) in &rows_by_line {
            assert!(
                rows.len() == 1 || SHARED_ENTRIES.contains(&rows.as_slice()),
                "{rows:?} name the same entry, `{line}`, and the changelog does not record their fixes together"
            );
        }
        for shared in SHARED_ENTRIES {
            assert!(
                rows_by_line.values().any(|rows| rows.as_slice() == *shared),
                "{shared:?} no longer name one entry, so they need no allowance"
            );
        }
    }

    #[test]
    fn a_row_is_found_by_its_id() {
        assert_eq!(defect("F6").map(|defect| defect.id), Some("F6"));
        assert_eq!(defect("F17"), None);
    }
}
