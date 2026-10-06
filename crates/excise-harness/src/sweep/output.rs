//! What a sweep writes as it goes: the evidence file of every check, and the list of checks the
//! document keeps.
//!
//! Every file is named by the [`label`] of the version it belongs to and never by the ref as it was
//! typed: a ref with a slash would make a directory, and one with `..` could leave the run's
//! directory. The label of each version is worked out once, when the output is made.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::report::SweepCheck;

use super::{
    checks::{Checked, Recorded},
    label::label,
    model::Placed,
    run::{SweepError, VersionInput},
};

/// What is written for the run: the evidence files below the run's directory, and the checks the
/// document lists.
#[derive(Debug)]
pub(crate) struct Output {
    run_dir: PathBuf,
    /// The label of every version, by its ref.
    labels: BTreeMap<String, String>,
    /// The checks recorded so far, in the order of the document's `checks`.
    pub(crate) checks: Vec<SweepCheck>,
}

impl Output {
    /// An output for `versions`, whose files go below `run_dir`.
    ///
    /// # Errors
    ///
    /// Returns [`SweepError::Options`] when two versions would keep their evidence under one name:
    /// refs whose commits begin alike and that differ only in the characters a label replaces, or
    /// in case, which a file system may not tell apart.
    pub(crate) fn new(run_dir: &Path, versions: &[VersionInput]) -> Result<Self, SweepError> {
        let mut labels = BTreeMap::new();
        let mut taken: BTreeMap<String, &str> = BTreeMap::new();
        for version in versions {
            let name = label(&version.reference, &version.sha);
            if let Some(first) = taken.insert(name.to_ascii_lowercase(), &version.reference) {
                return Err(SweepError::Options(format!(
                    "the versions `{first}` and `{}` would keep their files under one name, \
                     `{name}`: the characters of a ref that a file name cannot hold become `_`, \
                     and not every file system tells letters of different case apart; name one \
                     of them by its commit instead",
                    version.reference
                )));
            }
            labels.insert(version.reference.clone(), name);
        }
        Ok(Self {
            run_dir: run_dir.to_path_buf(),
            labels,
            checks: Vec::new(),
        })
    }

    /// Records `record` as a check of the version `reference`: its evidence file, and its place in
    /// the document. Returns the pointer to it and the evidence path, which is
    /// `evidence/<label>/<check>[-<fixture>][-<profile>].txt` below the run's directory.
    ///
    /// # Errors
    ///
    /// Returns [`SweepError::Io`] when the file cannot be written, and [`SweepError::Options`] when
    /// `reference` is not one of the versions this output was made for.
    pub(crate) fn record(
        &mut self,
        reference: &str,
        record: Recorded,
    ) -> Result<(String, Option<String>), SweepError> {
        let Some(folder) = self.labels.get(reference) else {
            return Err(SweepError::Options(format!(
                "`{reference}` is not one of the versions of this sweep"
            )));
        };
        let mut name = record.check.clone();
        if let Some(fixture) = &record.fixture {
            name.push('-');
            name.push_str(fixture);
        }
        if let Some(profile) = record.profile {
            name.push('-');
            name.push_str(profile.as_str());
        }
        let relative = format!("evidence/{folder}/{name}.txt");
        let path = self.run_dir.join(&relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| SweepError::Io {
                context: format!("cannot create `{}`", parent.display()),
                source,
            })?;
        }
        fs::write(&path, &record.evidence).map_err(|source| SweepError::Io {
            context: format!("cannot write `{}`", path.display()),
            source,
        })?;
        let index = self.checks.len();
        self.checks.push(SweepCheck {
            reference: reference.to_owned(),
            check: record.check,
            fixture: record.fixture,
            profile: record.profile,
            status: record.status,
            reason: record.reason,
            metrics: record.metrics,
            notes: record.notes,
            evidence: Some(relative.clone()),
        });
        Ok((format!("#/checks/{index}"), Some(relative)))
    }

    /// Records a check as [`record`](Self::record) does, and keeps what it observed with where its
    /// evidence is.
    pub(crate) fn place<T>(
        &mut self,
        reference: &str,
        checked: Checked<T>,
    ) -> Result<Placed<T>, SweepError> {
        let (pointer, evidence) = self.record(reference, checked.record.clone())?;
        Ok(Placed {
            checked,
            pointer,
            evidence,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::{scenario::Profile, sweep::run::BuildInput};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";
    /// What the check below is called in the evidence file's name.
    const FILE: &str = "signal-term-navigate-folders-deterministic.txt";

    fn version(reference: &str, sha: &str) -> VersionInput {
        VersionInput {
            reference: reference.to_owned(),
            sha: sha.to_owned(),
            toolchain: None,
            build: BuildInput::Failed {
                reason: "not built in this test".to_owned(),
                log: None,
            },
        }
    }

    fn recorded(check: &str) -> Recorded {
        let mut record = Recorded::new(
            check,
            Some("navigate-folders"),
            Some(Profile::Deterministic),
        );
        record.evidence = format!("the evidence of {check}\n");
        record
    }

    /// A run directory below a base directory of its own, so that a file written outside the run
    /// shows.
    fn run_dir(base: &TempDir) -> PathBuf {
        let run = base.path().join("run");
        fs::create_dir(&run).expect("a run directory");
        run
    }

    /// Everything below `root`, as `/`-separated paths relative to it, sorted.
    fn tree(root: &Path) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, found: &mut Vec<String>) {
            for entry in fs::read_dir(dir).expect("a directory") {
                let path = entry.expect("an entry").path();
                let relative = path.strip_prefix(root).expect("below the root");
                found.push(relative.to_string_lossy().replace('\\', "/"));
                if path.is_dir() {
                    walk(&path, root, found);
                }
            }
        }
        let mut found = Vec::new();
        walk(root, root, &mut found);
        found.sort();
        found
    }

    #[test]
    fn the_evidence_of_a_ref_named_a_dot_dot_b_lands_directly_below_its_own_label() {
        let base = tempfile::tempdir().expect("a directory");
        let run = run_dir(&base);
        let versions = [version("a/../b", SHA)];
        let mut output = Output::new(&run, &versions).expect("an output");

        let (pointer, evidence) = output
            .record("a/../b", recorded("signal-term"))
            .expect("recorded");

        let folder = label("a/../b", SHA);
        let relative = format!("evidence/{folder}/{FILE}");
        assert_eq!(evidence.as_deref(), Some(relative.as_str()));
        assert_eq!(pointer, "#/checks/0");
        assert_eq!(
            output.checks[0].evidence.as_deref(),
            Some(relative.as_str()),
            "the document names the same file"
        );
        assert_eq!(
            fs::read_to_string(run.join(&relative)).expect("the evidence file"),
            "the evidence of signal-term\n"
        );
        // Nothing else was made, in the run directory or beside it.
        let mut expected = vec![
            "run".to_owned(),
            "run/evidence".to_owned(),
            format!("run/evidence/{folder}"),
            format!("run/{relative}"),
        ];
        expected.sort();
        assert_eq!(tree(base.path()), expected);
    }

    #[test]
    fn no_ref_can_make_the_evidence_leave_its_directory_or_split_into_more() {
        let hostile = [
            "../../outside",
            "a/../b",
            "/absolute/path",
            "..",
            ".",
            "^{/fix}",
            "with space",
            "-rf",
            ".hidden",
            "naïve/日本語",
            "",
        ];
        for reference in hostile {
            let base = tempfile::tempdir().expect("a directory");
            let run = run_dir(&base);
            let versions = [version(reference, SHA)];
            let mut output = Output::new(&run, &versions).expect("an output");

            let (_, evidence) = output
                .record(reference, recorded("signal-term"))
                .expect("recorded");

            let evidence = evidence.expect("an evidence path");
            let parts: Vec<&str> = evidence.split('/').collect();
            assert_eq!(parts.len(), 3, "{reference:?}: {evidence}");
            assert_eq!(parts[0], "evidence", "{reference:?}");
            assert_eq!(parts[1], label(reference, SHA), "{reference:?}");
            assert_eq!(parts[2], FILE, "{reference:?}");
            assert!(run.join(&evidence).is_file(), "{reference:?}");
            let made = tree(base.path());
            assert_eq!(made.len(), 4, "{reference:?}: {made:?}");
            assert!(
                made.iter()
                    .all(|path| path == "run" || path.starts_with("run/")),
                "{reference:?} wrote outside the run directory: {made:?}"
            );
        }
    }

    #[test]
    fn the_versions_of_a_run_keep_their_evidence_in_directories_of_their_own() {
        let base = tempfile::tempdir().expect("a directory");
        let run = run_dir(&base);
        let versions = [
            version("release/1.0", SHA),
            version("release_1.0", OTHER_SHA),
            version("HEAD", OTHER_SHA),
        ];
        let mut output = Output::new(&run, &versions).expect("an output");

        let mut files = Vec::new();
        for (index, found) in versions.iter().enumerate() {
            let (pointer, evidence) = output
                .record(&found.reference, recorded("signal-term"))
                .expect("recorded");
            assert_eq!(pointer, format!("#/checks/{index}"));
            files.push(evidence.expect("an evidence path"));
        }

        assert_eq!(
            files,
            [
                format!("evidence/release_1.0-0123456789ab/{FILE}"),
                format!("evidence/release_1.0-fedcba987654/{FILE}"),
                format!("evidence/HEAD-fedcba987654/{FILE}"),
            ]
        );
        for file in &files {
            assert!(run.join(file).is_file(), "{file}");
        }
        assert_eq!(output.checks.len(), 3);
    }

    #[test]
    fn refs_that_would_keep_their_evidence_under_one_name_are_refused() {
        let base = tempfile::tempdir().expect("a directory");
        let run = run_dir(&base);
        for (first, second) in [("release/1.0", "release_1.0"), ("Main", "main")] {
            let versions = [version(first, SHA), version(second, SHA)];

            let refused = Output::new(&run, &versions).expect_err("one name for two versions");

            match refused {
                SweepError::Options(message) => {
                    assert!(
                        message.contains(&format!("`{first}` and `{second}`")),
                        "{message}"
                    );
                    assert!(message.contains("under one name"), "{message}");
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(tree(base.path()), ["run"], "nothing was written");
    }

    #[test]
    fn a_ref_that_is_not_a_version_of_the_run_is_refused_and_writes_nothing() {
        let base = tempfile::tempdir().expect("a directory");
        let run = run_dir(&base);
        let versions = [version("v1.3.0", SHA)];
        let mut output = Output::new(&run, &versions).expect("an output");

        let refused = output
            .record("v9.9.9", recorded("signal-term"))
            .expect_err("a ref that is not swept");

        match refused {
            SweepError::Options(message) => assert!(message.contains("v9.9.9"), "{message}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(tree(base.path()), ["run"]);
        assert!(output.checks.is_empty());
    }

    #[test]
    fn a_placed_check_keeps_what_it_observed_with_the_pointer_and_the_path() {
        let base = tempfile::tempdir().expect("a directory");
        let run = run_dir(&base);
        let versions = [version("v1.3.0", SHA)];
        let mut output = Output::new(&run, &versions).expect("an output");
        let checked = Checked {
            observation: Some(7_u32),
            record: recorded("signal-term"),
        };

        let placed = output.place("v1.3.0", checked).expect("placed");

        assert_eq!(placed.checked.observation, Some(7));
        assert_eq!(placed.pointer, "#/checks/0");
        assert_eq!(
            placed.evidence,
            Some(format!("evidence/v1.3.0-0123456789ab/{FILE}"))
        );
    }
}
