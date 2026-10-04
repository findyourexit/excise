//! The reference model.
//!
//! The model does not predict what the runtime does. It watches the live tree between points at
//! which only the runtime acts, and holds what changed in each stretch against what the runtime
//! reviewed and reported.
//!
//! A *window* is the stretch between two snapshots of the world. A snapshot is taken only while
//! nothing else touches the tree: when the runtime is idle (the script's mutations run then, at
//! the barrier that precedes them), or on the executor's own thread, as it takes a plan (an armed
//! mutation runs there, after the planner's review). So whatever differs between the snapshots at
//! the two ends of a window is the runtime's doing.
//!
//! What the runtime had confirmed and reviewed comes from the runtime itself, through
//! [`DeletionProbe`], not from counting keys:
//!
//! - The owner loop says when the interface accepted a request to delete and when it accepted a
//!   confirmation of it, each with the work item's id (the work queue made the id when it took the
//!   request). The model ties each to the key the script was handing the runtime, and keeps one
//!   record per request, so that consent is spent by the one work item it was given to, matched
//!   by id, and by no other. In reduced confirmation mode the accepted request is the consent,
//!   and carries its id like any other.
//! - The executor says when it takes a work item, with the plan: every entry the planner
//!   reviewed. The model keeps those entries, as the tree held them (identity and generation), for
//!   that deletion alone, whatever its report holds. The executor says again when it is done, with
//!   its report, or with none when its final check refused the plan.
//!
//! The checks of `check::Window`, for each window:
//!
//! 1. **Only reviewed, confirmed targets lose anything.** An entry that disappears was reviewed
//!    by the plan of a deletion that finished in the window, as that entry: an entry that carries
//!    a reviewed identity but was put there after the review (the harness numbers entries with
//!    generations for this, because a file system may reuse an identity at once) is not covered.
//!    A name inside a confirmed target that goes was reviewed under the entry it held when the
//!    window opened, and its report says it was deleted (or failed after the removal; a report
//!    that is not complete says nothing of what it leaves out). An entry reported deleted was
//!    there, as the entry reviewed, and is gone. And the executor takes no work item that an
//!    accepted request and confirmation have not paid for.
//! 2. **Nothing outside a confirmed target changes.** Every name outside every confirmed target
//!    holds the same entry when the window closes as when it opened, and no name appears there.
//!    The sentinels beside the scan root are such names.
//!
//! The other two invariants, that the interface is navigable and the terminal is restored, are
//! checked by the caller from how the run ended.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use excise::fuzz::deletion::{DeletionProbe, DeletionReport, PlannedKind, ReviewedPlan};

use crate::check::{
    Finished, Outcome, RULE_CONFIRMED, RULE_REVIEWED, Report, Reviewed, Violation, Window,
};
use crate::script::{Key, Op};
use crate::world::{self, Generations, Kind, Snapshot, World};

/// What one input reached.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Times a confirmation dialog appeared on screen.
    pub dialogs_shown: u64,
    /// Plans that reached the executor.
    pub executions: u64,
    /// Plans the executor's final check refused.
    pub refused: u64,
    /// Entries the runtime deleted.
    pub deleted_entries: u64,
    /// Deletions the user stopped part-way.
    pub soft_cancelled: u64,
    /// Cancelling keys pressed while a confirmation dialog was on screen.
    pub cancels_on_dialog: u64,
    /// Mutations the script applied while the runtime was idle.
    pub mutations: u64,
    /// Of those, mutations applied while a confirmation dialog was open: after the user saw the
    /// target, before they confirmed it.
    pub mutations_with_dialog: u64,
    /// Of those, ones that a confirming key followed while the dialog was still open.
    pub confirms_after_mutation: u64,
    /// Mutations applied on the executor's thread, after the planner's review and before the
    /// final check.
    pub armed_mutations: u64,
    /// Windows checked.
    pub windows: u64,
}

/// Where a request to delete stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Consent {
    /// The interface accepted the request and has not accepted a confirmation of it.
    Asked,
    /// Consent is given: a confirming key, or, in reduced confirmation mode, the request itself.
    Given,
    /// The executor took the work item that the consent paid for.
    Spent,
}

/// A request to delete that the interface accepted.
#[derive(Debug)]
struct Request {
    target: PathBuf,
    /// The work item the work queue gave the request.
    work_id: u64,
    consent: Consent,
}

/// The plan the executor is working on.
struct Running {
    work_id: u64,
    target: PathBuf,
    /// Every entry the planner reviewed for it.
    reviewed: BTreeMap<PathBuf, Reviewed>,
}

struct Inner {
    world: World,
    generations: Generations,
    /// The world as it was when the current window opened.
    last: Snapshot,
    /// What opened the current window.
    since: String,
    /// The deletions that finished in the current window.
    finished: Vec<Finished>,
    /// The plan the executor has taken and not yet ended.
    running: Option<Running>,
    armed: VecDeque<Op>,
    /// The requests to delete that the interface accepted, in order.
    requests: Vec<Request>,
    /// The key the runtime is handling, until it asks for the next input. A request or a
    /// confirmation the interface accepts happens while the runtime handles a key.
    key: Option<Key>,
    /// The step the script is at.
    step: String,
    dialog_open: bool,
    mutated_with_dialog: bool,
    stats: Stats,
    trace: Vec<String>,
}

pub struct Model {
    inner: Mutex<Inner>,
    /// The screen as of the last frame, for messages.
    screen: Arc<Mutex<String>>,
}

fn kind_matches(kind: Kind, planned: PlannedKind) -> bool {
    matches!(
        (kind, planned),
        (Kind::Folder, PlannedKind::Directory)
            | (Kind::File, PlannedKind::File)
            | (Kind::Symlink, PlannedKind::Link)
    )
}

fn describe_key(key: Option<Key>) -> String {
    key.map_or_else(|| "no key".to_owned(), |key| format!("the key {key}"))
}

impl Model {
    /// Starts watching `world`, as it is now.
    pub fn new(world: World, screen: Arc<Mutex<String>>) -> std::io::Result<Arc<Self>> {
        let mut generations = Generations::default();
        let last = world::snapshot(&world, &mut generations)?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner {
                world,
                generations,
                last,
                since: "the start of the run".to_owned(),
                finished: Vec::new(),
                running: None,
                armed: VecDeque::new(),
                requests: Vec::new(),
                key: None,
                step: "the start of the run".to_owned(),
                dialog_open: false,
                mutated_with_dialog: false,
                stats: Stats::default(),
                trace: Vec::new(),
            }),
            screen,
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The script reached step `number`, described by `text`. One step may be several actions.
    pub fn step(&self, number: usize, text: &str) {
        let mut inner = self.lock();
        let step = format!("step {number}: {text}");
        if inner.step != step {
            inner.trace.push(step.clone());
            inner.step = step;
        }
    }

    /// The script hands the runtime `key`, which it handles before it asks for the next input.
    pub fn key(&self, key: Key) {
        let mut inner = self.lock();
        inner.key = Some(key);
        if key.confirms() && inner.dialog_open && inner.mutated_with_dialog {
            inner.stats.confirms_after_mutation += 1;
            inner.mutated_with_dialog = false;
        }
        if key.cancels() && inner.dialog_open {
            inner.stats.cancels_on_dialog += 1;
            inner.mutated_with_dialog = false;
        }
    }

    /// The runtime asked for the next input: it is done with the key it had.
    pub fn key_handled(&self) {
        self.lock().key = None;
    }

    /// A frame was drawn. `dialog` is what a deletion dialog on it says, or `None` when none is.
    pub fn frame(&self, dialog: Option<&str>) {
        let mut inner = self.lock();
        if let Some(dialog) = dialog
            && !inner.dialog_open
        {
            inner.stats.dialogs_shown += 1;
            inner.trace.push(format!("  dialog shown: {dialog}"));
        }
        inner.dialog_open = dialog.is_some();
    }

    /// Adds a line to the trace.
    pub fn note(&self, line: &str) {
        self.lock().trace.push(format!("  {line}"));
    }

    /// Arms `op` to run when the executor next takes a plan.
    pub fn arm(&self, op: Op) {
        let mut inner = self.lock();
        inner.trace.push(format!("  armed {op}"));
        inner.armed.push_back(op);
    }

    /// Applies `op` to the live tree, which the runtime has left idle.
    pub fn mutate_when_idle(&self, op: Op) {
        let mut inner = self.lock();
        assert!(
            inner.running.is_none(),
            "the model applied a mutation while the executor was busy, at {}: a settle step \
             no longer means that the runtime is idle",
            inner.step
        );
        let at = format!("the mutation {op} at {}", inner.step);
        self.close_window(&mut inner, &format!("just before {at}"));
        let effect = Self::apply(&mut inner, op);
        inner.stats.mutations += 1;
        if inner.dialog_open {
            inner.stats.mutations_with_dialog += 1;
            inner.mutated_with_dialog = true;
        }
        inner.trace.push(format!("  mutation: {effect}"));
        inner.since = format!("just after {at}");
    }

    /// Applies `op` and makes what the tree is now the start of the next window.
    fn apply(inner: &mut Inner, op: Op) -> String {
        let applied = world::apply(&inner.world, &inner.last, op);
        for path in &applied.touched {
            inner.generations.touch(path);
        }
        match world::snapshot(&inner.world, &mut inner.generations) {
            Ok(after) => inner.last = after,
            Err(error) => {
                panic!("the model could not snapshot the world after a mutation: {error}")
            }
        }
        applied.effect
    }

    /// Ends the current window at the tree as it is now and checks it. `until` says what ends it.
    fn close_window(&self, inner: &mut Inner, until: &str) {
        let now = match world::snapshot(&inner.world, &mut inner.generations) {
            Ok(now) => now,
            Err(error) => panic!("the model could not snapshot the world {until}: {error}"),
        };
        inner.stats.windows += 1;
        let window = Window {
            world: &inner.world,
            before: &inner.last,
            after: &now,
            finished: &inner.finished,
        };
        if let Err(violation) = window.check() {
            self.fail(inner, &violation, until);
        }
        inner.finished.clear();
        inner.last = now;
    }

    /// Ends the run's last window.
    pub fn finish(&self) {
        let mut inner = self.lock();
        if let Some(running) = &inner.running {
            let violation = Violation {
                rule: "the interface returns to a navigable state",
                detail: format!(
                    "the executor took work item {} for {} and never ended it: a deletion \
                     never finished",
                    running.work_id,
                    running.target.display()
                ),
            };
            self.fail(&inner, &violation, "the end of the run");
        }
        self.close_window(&mut inner, "the end of the run");
    }

    pub fn stats(&self) -> Stats {
        self.lock().stats
    }

    /// A number that stands for everything the run did that the trace records, with the fixture's
    /// own location left out, so that two replays of an input that went the same way have the
    /// same digest, whichever directory each built its fixture in.
    pub fn digest(&self) -> u64 {
        let inner = self.lock();
        let base = inner.world.base.display().to_string();
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        let mut feed = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
        };
        for line in &inner.trace {
            feed(line.replace(&base, "<base>").as_bytes());
            feed(b"\n");
        }
        feed(format!("{:?}", inner.stats).as_bytes());
        hash
    }

    /// The step the script is at, for messages.
    pub fn current_step(&self) -> String {
        self.lock().step.clone()
    }

    /// [`Model::current_step`] for the watchdog, which must not wait on a lock that a hung thread
    /// may hold.
    pub fn current_step_unlocked(&self) -> String {
        self.inner.try_lock().map_or_else(
            |_| "(a step, but the model is busy)".to_owned(),
            |inner| inner.step.clone(),
        )
    }

    /// [`Model::describe`] for the watchdog.
    pub fn describe_unlocked(&self) -> String {
        self.inner.try_lock().map_or_else(
            |_| "(the model is busy)".to_owned(),
            |inner| self.describe_locked(&inner),
        )
    }

    /// The trace of the run, and the screen as of its last frame.
    pub fn describe(&self) -> String {
        let inner = self.lock();
        self.describe_locked(&inner)
    }

    fn describe_locked(&self, inner: &Inner) -> String {
        let mut text = String::from("trace of the run:\n");
        let skipped = inner.trace.len().saturating_sub(80);
        if skipped > 0 {
            let _ = writeln!(text, "  ({skipped} earlier lines left out)");
        }
        for line in &inner.trace[skipped..] {
            let _ = writeln!(text, "{line}");
        }
        let screen = self.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = write!(text, "last screen:\n{}", *screen);
        text
    }

    fn fail(&self, inner: &Inner, violation: &Violation, until: &str) -> ! {
        panic!(
            "deletion safety violated: {}\n{}\nwindow: from {} to {until}\nat {}\n{}",
            violation.rule,
            violation.detail,
            inner.since,
            inner.step,
            self.describe_locked(inner)
        );
    }

    /// What the planner reviewed for the plan the executor has just taken, as the tree holds it.
    fn reviewed(&self, inner: &Inner, plan: &ReviewedPlan) -> BTreeMap<PathBuf, Reviewed> {
        let Some(entries) = &plan.entries else {
            let violation = Violation {
                rule: RULE_REVIEWED,
                detail: format!(
                    "the plan for {} keeps its entries in a file, which a fixture this small \
                     never needs: the model cannot tell what the planner reviewed",
                    plan.target.display()
                ),
            };
            self.fail(inner, &violation, "the executor taking the plan");
        };
        let mut reviewed = BTreeMap::new();
        for entry in entries {
            let path = inner.world.root.join(&entry.relative_path);
            let id = entry.snapshot.identity.file_id;
            match inner.last.get(&path) {
                Some(held) if held.id == id && kind_matches(held.kind, entry.snapshot.kind) => {
                    reviewed.insert(
                        path,
                        Reviewed {
                            id,
                            generation: held.generation,
                        },
                    );
                }
                held => {
                    let violation = Violation {
                        rule: RULE_REVIEWED,
                        detail: format!(
                            "the planner reviewed {} as {id:?} ({:?}), which the tree held as \
                             {held:?} when the executor took the plan for {}",
                            path.display(),
                            entry.snapshot.kind,
                            plan.target.display()
                        ),
                    };
                    self.fail(inner, &violation, "the executor taking the plan");
                }
            }
        }
        reviewed
    }

    /// Fails for a deletion the executor took that no accepted request and confirmation paid for.
    fn unpaid(&self, inner: &Inner, work_id: u64, target: &Path, why: &str) -> ! {
        let violation = Violation {
            rule: RULE_CONFIRMED,
            detail: format!(
                "the executor took work item {work_id} for {}, {why}\nthe requests the \
                 interface accepted: {:#?}",
                target.display(),
                inner.requests
            ),
        };
        self.fail(inner, &violation, "the executor taking the plan");
    }
}

impl DeletionProbe for Model {
    fn requested(&self, work_id: Option<u64>, target: &Path, reduced_guardrails: bool) {
        let mut inner = self.lock();
        let key = inner.key.take();
        if key != Some(Key::Backspace) {
            let violation = Violation {
                rule: RULE_CONFIRMED,
                detail: format!(
                    "the interface accepted a request to delete {}, but what the runtime was \
                     handling was {}, not a Backspace",
                    target.display(),
                    describe_key(key)
                ),
            };
            self.fail(&inner, &violation, "the interface accepting the request");
        }
        let Some(work_id) = work_id else {
            let violation = Violation {
                rule: RULE_CONFIRMED,
                detail: format!(
                    "the interface accepted a request to delete {}, and the work queue holds \
                     no work item for it",
                    target.display()
                ),
            };
            self.fail(&inner, &violation, "the interface accepting the request");
        };
        if inner
            .requests
            .iter()
            .any(|request| request.work_id == work_id)
        {
            let violation = Violation {
                rule: RULE_CONFIRMED,
                detail: format!(
                    "the interface gave work item {work_id} to a second request, for {}\nthe \
                     requests the interface accepted: {:#?}",
                    target.display(),
                    inner.requests
                ),
            };
            self.fail(&inner, &violation, "the interface accepting the request");
        }
        inner.trace.push(format!(
            "  request accepted: work item {work_id} for {}{}",
            target.display(),
            if reduced_guardrails {
                " (reduced confirmation: the request is the consent)"
            } else {
                ""
            }
        ));
        inner.requests.push(Request {
            target: target.to_path_buf(),
            work_id,
            consent: if reduced_guardrails {
                Consent::Given
            } else {
                Consent::Asked
            },
        });
    }

    fn confirmed(&self, work_id: u64, target: &Path) {
        let mut inner = self.lock();
        let key = inner.key.take();
        if !key.is_some_and(Key::confirms) {
            let violation = Violation {
                rule: RULE_CONFIRMED,
                detail: format!(
                    "the interface accepted the confirmation of work item {work_id} for {}, \
                     but what the runtime was handling was {}, not a confirming key",
                    target.display(),
                    describe_key(key)
                ),
            };
            self.fail(
                &inner,
                &violation,
                "the interface accepting the confirmation",
            );
        }
        let waiting = inner
            .requests
            .iter()
            .position(|request| request.consent == Consent::Asked && request.work_id == work_id);
        let Some(index) = waiting else {
            let violation = Violation {
                rule: RULE_CONFIRMED,
                detail: format!(
                    "the interface accepted the confirmation of work item {work_id} for {}, \
                     and no request under that work item is waiting for one\nthe requests the \
                     interface accepted: {:#?}",
                    target.display(),
                    inner.requests
                ),
            };
            self.fail(
                &inner,
                &violation,
                "the interface accepting the confirmation",
            );
        };
        if inner.requests[index].target != target {
            let asked = inner.requests[index].target.display().to_string();
            let violation = Violation {
                rule: RULE_CONFIRMED,
                detail: format!(
                    "the interface accepted the confirmation of work item {work_id} for {}, and \
                     the request under that work item was for {asked}",
                    target.display()
                ),
            };
            self.fail(
                &inner,
                &violation,
                "the interface accepting the confirmation",
            );
        }
        inner.requests[index].consent = Consent::Given;
        inner.trace.push(format!(
            "  confirmation accepted: work item {work_id} for {}",
            target.display()
        ));
    }

    fn before_execution(&self, work_id: u64, plan: &ReviewedPlan) {
        let mut inner = self.lock();
        inner.stats.executions += 1;
        inner.trace.push(format!(
            "  executor takes work item {work_id} for {}",
            plan.target.display()
        ));
        if let Some(running) = &inner.running {
            let violation = Violation {
                rule: RULE_CONFIRMED,
                detail: format!(
                    "the executor took work item {work_id} while it had not ended work item {}",
                    running.work_id
                ),
            };
            self.fail(&inner, &violation, "the executor taking the plan");
        }
        // The work item's own consent: the consent given to the work item with this id, in
        // reduced confirmation mode as in any other. A work item the interface never took a
        // request for is unpaid, whatever else waits for its target.
        let paid = inner
            .requests
            .iter()
            .position(|request| request.consent == Consent::Given && request.work_id == work_id);
        let Some(index) = paid else {
            self.unpaid(
                &inner,
                work_id,
                &plan.target,
                "which no accepted request and confirmation had paid for",
            );
        };
        if inner.requests[index].target != plan.target {
            let asked = inner.requests[index].target.display().to_string();
            self.unpaid(
                &inner,
                work_id,
                &plan.target,
                &format!("but the consent given for that work item was for {asked}"),
            );
        }
        inner.requests[index].consent = Consent::Spent;

        // Whatever happened before now is checked on its own; the tree is as the planner
        // reviewed it, and the entries it reviewed are kept for this deletion alone.
        self.close_window(
            &mut inner,
            &format!(
                "the executor taking work item {work_id} for {}",
                plan.target.display()
            ),
        );
        let reviewed = self.reviewed(&inner, plan);
        inner.running = Some(Running {
            work_id,
            target: plan.target.clone(),
            reviewed,
        });
        inner.since = format!(
            "just after the executor took work item {work_id} for {}",
            plan.target.display()
        );
        if let Some(op) = inner.armed.pop_front() {
            let effect = Self::apply(&mut inner, op);
            inner.stats.armed_mutations += 1;
            inner.trace.push(format!("  armed mutation: {effect}"));
            inner.since = format!(
                "just after the armed mutation {op}, run after the planner reviewed {} and \
                 before the executor's final check",
                plan.target.display()
            );
        }
    }

    fn after_execution(&self, work_id: u64, report: Option<&DeletionReport>) {
        let mut inner = self.lock();
        let Some(running) = inner.running.take() else {
            let violation = Violation {
                rule: RULE_CONFIRMED,
                detail: format!("the executor ended work item {work_id}, which it had not taken"),
            };
            self.fail(&inner, &violation, "the executor's report");
        };
        if running.work_id != work_id {
            let violation = Violation {
                rule: RULE_CONFIRMED,
                detail: format!(
                    "the executor ended work item {work_id}, and had taken work item {}",
                    running.work_id
                ),
            };
            self.fail(&inner, &violation, "the executor's report");
        }
        let Some(report) = report else {
            inner.stats.refused += 1;
            inner
                .trace
                .push("  the executor's final check refused the plan".to_owned());
            return;
        };
        let report = Report::of(report);
        inner.stats.deleted_entries += report.count(&Outcome::Deleted) as u64;
        inner.stats.soft_cancelled += u64::from(report.soft_cancelled);
        inner.trace.push(format!(
            "  report for {}: {} deleted, {} changed, {} missing, {} failed, {} unattempted{}{}",
            report.target.display(),
            report.count(&Outcome::Deleted),
            report.count(&Outcome::Changed),
            report.count(&Outcome::Missing),
            report.count(&Outcome::Failed),
            report.count(&Outcome::Unattempted),
            if report.soft_cancelled {
                ", stopped"
            } else {
                ""
            },
            if report.complete { "" } else { ", incomplete" },
        ));
        if report.target != running.target {
            let violation = Violation {
                rule: RULE_REVIEWED,
                detail: format!(
                    "the report for work item {work_id} names the target {}, and the plan the \
                     executor took was for {}",
                    report.target.display(),
                    running.target.display()
                ),
            };
            self.fail(&inner, &violation, "the executor's report");
        }
        inner.finished.push(Finished {
            target: running.target,
            reviewed: running.reviewed,
            report,
        });
    }
}
