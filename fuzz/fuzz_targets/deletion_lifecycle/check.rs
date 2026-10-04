//! The checks the model makes on one window: the stretch between two snapshots of the world in
//! which only the runtime acted, with the deletions that finished in it, each with the entries the
//! planner reviewed for it and the executor's report. See the documentation of the `model` module
//! for what a window is and why the checks are sound.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use excise::fuzz::deletion::{DeletionEntryOutcome, DeletionReport};
use file_id::FileId;

use crate::world::{Snapshot, World};

pub const RULE_CONFIRMED: &str = "only confirmed deletions run";
pub const RULE_REVIEWED: &str = "only reviewed identities of confirmed targets disappear";
pub const RULE_OUTSIDE: &str = "nothing outside a confirmed target changes";
pub const RULE_SENTINEL: &str = "nothing outside a confirmed target changes (a sentinel)";
pub const RULE_RESIDUE: &str = "the runtime leaves no entry of its own inside a confirmed target";

/// A check that failed.
pub struct Violation {
    pub rule: &'static str,
    pub detail: String,
}

/// What a deletion report says about one entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    Deleted,
    Changed,
    Missing,
    Failed,
    Unattempted,
}

#[derive(Clone, Debug)]
pub struct ReportedEntry {
    pub path: PathBuf,
    pub id: FileId,
    pub outcome: Outcome,
}

/// What the executor reported of one deletion: its target, and the entries it lists with what
/// became of each.
#[derive(Clone, Debug)]
pub struct Report {
    pub target: PathBuf,
    pub entries: Vec<ReportedEntry>,
    /// Whether the report lists every entry the plan held. A report that does not (the runtime
    /// could not store all its results) says nothing of the entries it leaves out.
    pub complete: bool,
    pub soft_cancelled: bool,
}

impl Report {
    pub fn of(report: &DeletionReport) -> Self {
        let mut complete = report.reporting_complete();
        let mut entries = Vec::new();
        for result in &report.entries {
            let Ok(result) = result else {
                complete = false;
                continue;
            };
            entries.push(ReportedEntry {
                path: report.scan_root.join(&result.entry.relative_path),
                id: result.entry.snapshot.identity.file_id,
                outcome: match result.outcome {
                    DeletionEntryOutcome::Deleted => Outcome::Deleted,
                    DeletionEntryOutcome::Changed(_) => Outcome::Changed,
                    DeletionEntryOutcome::Missing => Outcome::Missing,
                    DeletionEntryOutcome::Failed(_) => Outcome::Failed,
                    DeletionEntryOutcome::Unattempted => Outcome::Unattempted,
                },
            });
        }
        Self {
            target: report.scan_root.join(&report.root_relative_path),
            entries,
            complete,
            soft_cancelled: report.soft_cancelled,
        }
    }

    pub fn count(&self, outcome: &Outcome) -> usize {
        self.entries
            .iter()
            .filter(|entry| &entry.outcome == outcome)
            .count()
    }
}

/// What the planner reviewed of one entry, as the model saw the tree when the executor took the
/// plan: the identity the planner recorded, and the generation (see
/// [`crate::world::Generations`]) the name held, so that an entry put there afterwards is not
/// the one that was reviewed even when it carries the same identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reviewed {
    pub id: FileId,
    pub generation: u64,
}

/// One deletion that ran to its report.
#[derive(Clone, Debug)]
pub struct Finished {
    pub target: PathBuf,
    /// Every entry the planner reviewed for the target, kept from the plan itself, whatever the
    /// report holds.
    pub reviewed: BTreeMap<PathBuf, Reviewed>,
    pub report: Report,
}

impl Finished {
    fn outcome_of(&self, path: &Path) -> Option<&Outcome> {
        self.report
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| &entry.outcome)
    }
}

/// One window.
pub struct Window<'a> {
    pub world: &'a World,
    /// The world when the window opened.
    pub before: &'a Snapshot,
    /// The world when it closed.
    pub after: &'a Snapshot,
    /// The deletions that finished in between.
    pub finished: &'a [Finished],
}

impl Window<'_> {
    /// Holds what the runtime did in the window against what it reviewed and reported.
    pub fn check(&self) -> Result<(), Violation> {
        self.sentinels()?;
        self.reports_match_plans()?;
        self.identities()?;
        self.outside()?;
        self.names_inside()?;
        self.deleted_entries()
    }

    fn inside(&self, path: &Path) -> bool {
        self.finished
            .iter()
            .any(|finished| path.starts_with(&finished.target))
    }

    fn describe_targets(&self) -> String {
        if self.finished.is_empty() {
            "none: no deletion finished in this window".to_owned()
        } else {
            self.finished
                .iter()
                .map(|finished| finished.target.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    /// The sentinels beside the scan root are untouched. Checked first, for the plainest message.
    fn sentinels(&self) -> Result<(), Violation> {
        for (path, entry) in self
            .before
            .iter()
            .filter(|(path, _)| self.world.is_sentinel(path))
        {
            if self.after.get(path) != Some(entry) {
                return Err(Violation {
                    rule: RULE_SENTINEL,
                    detail: format!(
                        "the sentinel {} was {entry:?} and is now {:?}; confirmed targets: {}",
                        path.display(),
                        self.after.get(path),
                        self.describe_targets()
                    ),
                });
            }
        }
        Ok(())
    }

    /// A report lists only entries its own plan reviewed, under the identity the plan recorded.
    fn reports_match_plans(&self) -> Result<(), Violation> {
        for finished in self.finished {
            for reported in &finished.report.entries {
                let reviewed = finished.reviewed.get(&reported.path);
                if reviewed.map(|reviewed| reviewed.id) != Some(reported.id) {
                    return Err(Violation {
                        rule: RULE_REVIEWED,
                        detail: format!(
                            "the report for {} lists {} as {:?} ({:?}), and its plan reviewed \
                             {:?} there",
                            finished.target.display(),
                            reported.path.display(),
                            reported.id,
                            reported.outcome,
                            reviewed.map(|reviewed| reviewed.id)
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Invariant 1: an entry that disappears was reviewed, as the entry it is, for a confirmed
    /// deletion that finished in the window. An entry is its identity and its generation: one put
    /// in after the review is not the one the planner reviewed, whatever identity the file system
    /// gave it.
    fn identities(&self) -> Result<(), Violation> {
        let alive: HashSet<FileId> = self.after.values().map(|entry| entry.id).collect();
        let reviewed: HashSet<(FileId, u64)> = self
            .finished
            .iter()
            .flat_map(|finished| {
                finished
                    .reviewed
                    .values()
                    .map(|reviewed| (reviewed.id, reviewed.generation))
            })
            .collect();
        // Each entry that is gone, with the first name it had.
        let mut gone: HashMap<(FileId, u64), &Path> = HashMap::new();
        for (path, entry) in self.before {
            if !alive.contains(&entry.id) {
                gone.entry((entry.id, entry.generation)).or_insert(path);
            }
        }
        let mut gone: Vec<_> = gone.into_iter().collect();
        gone.sort_by_key(|(_, path)| *path);
        match gone
            .into_iter()
            .find(|(entry, _)| !reviewed.contains(entry))
        {
            None => Ok(()),
            Some(((id, generation), path)) => Err(Violation {
                rule: RULE_REVIEWED,
                detail: format!(
                    "the entry {id:?} (generation {generation}), last seen at {}, is gone, and no \
                     deletion that finished in this window reviewed that entry; one created or \
                     replaced after a review is not covered by it, even when it carries a \
                     reviewed identity; confirmed targets: {}",
                    path.display(),
                    self.describe_targets()
                ),
            }),
        }
    }

    /// Invariant 2: nothing outside a confirmed target changes, and nothing appears there.
    fn outside(&self) -> Result<(), Violation> {
        let names: BTreeSet<&PathBuf> = self.before.keys().chain(self.after.keys()).collect();
        for path in names.into_iter().filter(|path| !self.inside(path)) {
            let (was, is) = (self.before.get(path), self.after.get(path));
            if was != is {
                return Err(Violation {
                    rule: RULE_OUTSIDE,
                    detail: format!(
                        "{} is outside every confirmed target and changed from {was:?} to \
                         {is:?}; confirmed targets: {}",
                        path.display(),
                        self.describe_targets()
                    ),
                });
            }
        }
        Ok(())
    }

    /// Invariant 1, by name: a name inside a confirmed target that goes, or turns into another
    /// entry, held an entry its own plan reviewed, as that entry, and its report says the removal
    /// happened or may have (a report that is not complete says nothing of the entries it leaves
    /// out). And the runtime adds no name inside a target.
    fn names_inside(&self) -> Result<(), Violation> {
        for (path, entry) in self.before.iter().filter(|(path, _)| self.inside(path)) {
            if self.after.get(path) == Some(entry) {
                continue;
            }
            let mut why = String::from("no deletion that finished in this window covers it");
            let mut removed = false;
            for finished in self
                .finished
                .iter()
                .filter(|finished| path.starts_with(&finished.target))
            {
                match finished.reviewed.get(path) {
                    None => {
                        why = format!(
                            "the plan for {} did not review that name",
                            finished.target.display()
                        );
                    }
                    Some(reviewed)
                        if reviewed.id != entry.id || reviewed.generation != entry.generation =>
                    {
                        why = format!(
                            "the plan for {} reviewed that name as {:?} (generation {}), and it \
                             held another entry when the window opened",
                            finished.target.display(),
                            reviewed.id,
                            reviewed.generation
                        );
                    }
                    Some(_) => match finished.outcome_of(path) {
                        Some(Outcome::Deleted | Outcome::Failed) => removed = true,
                        None if !finished.report.complete => removed = true,
                        Some(other) => {
                            why = format!(
                                "the report for {} lists the entry as {other:?}",
                                finished.target.display()
                            );
                        }
                        None => {
                            why = format!(
                                "the complete report for {} does not list it",
                                finished.target.display()
                            );
                        }
                    },
                }
                if removed {
                    break;
                }
            }
            if !removed {
                return Err(Violation {
                    rule: RULE_REVIEWED,
                    detail: format!(
                        "{} held {entry:?} when the window opened and now holds {:?}, and {why}; \
                         confirmed targets: {}",
                        path.display(),
                        self.after.get(path),
                        self.describe_targets()
                    ),
                });
            }
        }
        if let Some(path) = self
            .after
            .keys()
            .find(|path| self.inside(path) && !self.before.contains_key(*path))
        {
            return Err(Violation {
                rule: RULE_RESIDUE,
                detail: format!(
                    "{} appeared inside a confirmed target during the deletion; confirmed \
                     targets: {}",
                    path.display(),
                    self.describe_targets()
                ),
            });
        }
        Ok(())
    }

    /// An entry a report lists as deleted is the entry its plan reviewed, was there as that entry
    /// when the window opened, and is gone.
    fn deleted_entries(&self) -> Result<(), Violation> {
        let held = |snapshot: &Snapshot, path: &Path| {
            snapshot.get(path).map(|entry| (entry.id, entry.generation))
        };
        for finished in self.finished {
            for reported in finished
                .report
                .entries
                .iter()
                .filter(|reported| reported.outcome == Outcome::Deleted)
            {
                let Some(reviewed) = finished.reviewed.get(&reported.path) else {
                    continue;
                };
                let reviewed = (reviewed.id, reviewed.generation);
                let at_open = held(self.before, &reported.path);
                if at_open != Some(reviewed) {
                    return Err(Violation {
                        rule: RULE_REVIEWED,
                        detail: format!(
                            "the report for {} says it deleted {} that the plan reviewed as \
                             {reviewed:?} (identity, generation), which held {at_open:?} when \
                             the window opened: the entry was put there after the review",
                            finished.target.display(),
                            reported.path.display(),
                        ),
                    });
                }
                if held(self.after, &reported.path) == Some(reviewed) {
                    return Err(Violation {
                        rule: RULE_REVIEWED,
                        detail: format!(
                            "the report for {} says it deleted {} ({reviewed:?}), which still \
                             holds it",
                            finished.target.display(),
                            reported.path.display()
                        ),
                    });
                }
            }
        }
        Ok(())
    }
}
