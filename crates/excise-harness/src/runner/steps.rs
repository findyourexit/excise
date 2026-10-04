//! The steps of the scenario vocabulary, implemented on the executor.
//!
//! The steps that talk to the program through a protocol shared with the interactive driver
//! (`select`, `delete`, `settle`, `quit`) call the protocol in [`super::live`] and report its
//! failures as step failures; the rest are specific to a scenario run.

use std::{
    fs,
    time::{Duration, Instant},
};

use crate::{
    events::{Payload, RefreshOutcome},
    fixture::mutate,
    metrics::live_cpu_ms,
    pty::ui::{
        DialogView, Inspector, SelectedItem, dialog_view, header_state, inspector,
        offers_plain_quit,
    },
    report::TimingWarning,
    safety::{FixtureSnapshot, send_signal},
    scenario::{
        DEFAULT_TIMEOUT_MS, Delete, DeleteWait, ExpectBudget, ExpectConfig, ExpectExit, ExpectFs,
        ExpectScreen, FsMutate, Idle, Measure, Quit, Residue, Resize, Select, SendSignal, Settle,
        Signal, Step, WaitEvent, WaitFs, WaitHeader, WaitRefresh, WaitText, config_setting,
    },
};

use super::{
    budget::{is_latency, limit_for},
    delete::verify_selection,
    exec::{Executor, FS_POLL_INTERVAL},
    live::{BACKSPACE, DeletionRequest, Drive, Needs, Waited},
    outcome::{FailureCause, RunError, Stop},
    plan::{Prepared, describe_region},
};

/// How long `expect_exit` reads the rest of the output after the program has exited.
const EXIT_OUTPUT_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Where the map stands after the deletions a `wait_refresh` waits for.
enum Refresh {
    /// The map is current. The step waits for a frame after the event at `after`: the end of the
    /// refresh, or, when no deletion removed anything, the last report of one.
    Settled { after: usize },
    /// The program could not show a map after the deletion.
    Failed,
}

impl Executor<'_> {
    pub(super) fn dispatch(&mut self, index: usize, step: &Step) -> Result<(), Stop> {
        match step {
            Step::WaitText(step) => self.wait_text(index, step),
            Step::WaitHeader(step) => self.wait_header(index, step),
            Step::WaitEvent(step) => self.wait_event(index, step),
            Step::Key(_) => self.key(index),
            Step::Type(_) => self.type_text(index),
            Step::Select(step) => self.select(index, step),
            Step::Delete(step) => self.delete(index, step),
            Step::WaitRefresh(step) => self.wait_refresh(index, *step),
            Step::WaitFsAbsent(step) => self.wait_fs(index, step, false),
            Step::WaitFsPresent(step) => self.wait_fs(index, step, true),
            Step::FsMutate(step) => self.fs_mutate(index, step),
            Step::Resize(step) => self.resize(index, *step),
            Step::Signal(step) => self.signal(index, *step),
            Step::ExpectScreen(step) => self.expect_screen(index, step),
            Step::ExpectFs(step) => self.expect_fs(index, step),
            Step::ExpectConfig(step) => self.expect_config(index, step),
            Step::ExpectExit(step) => self.expect_exit(index, step),
            Step::ExpectBudget(step) => self.expect_budget(index, step),
            Step::Measure(step) => {
                self.measure(step);
                Ok(())
            }
            Step::Idle(step) => self.idle(index, step),
            Step::Settle(step) => self.settle(index, *step),
            Step::Quit(step) => self.quit(index, *step),
        }
    }

    /// A step whose preparation is missing: a bug in the runner, not in the scenario.
    fn unprepared(index: usize, step: &'static str) -> Stop {
        RunError::InvalidStep {
            index,
            step,
            reason: "the step was not prepared".to_owned(),
        }
        .into()
    }

    fn wait_text(&mut self, index: usize, step: &WaitText) -> Result<(), Stop> {
        let prepared = self.prepared;
        let Prepared::Text(matcher) = &prepared[index] else {
            return Err(Self::unprepared(index, "wait_text"));
        };
        let region = step.region;
        let waited = self.wait_until(Self::deadline(step.timeout_ms), |exec| {
            matcher
                .is_match(&exec.live.session.screen().region_text(region))
                .then_some(())
        })?;
        match waited {
            Waited::Ready(()) => Ok(()),
            other => Err(self.unmet(
                index,
                &other,
                format!(
                    "{} on the screen{}",
                    matcher.describe(),
                    describe_region(region)
                ),
                step.timeout_ms,
                || {
                    format!(
                        "the screen shows:\n{}",
                        self.live.session.screen().region_text(region)
                    )
                },
            )),
        }
    }

    fn wait_header(&mut self, index: usize, step: &WaitHeader) -> Result<(), Stop> {
        let expected_state = step.state;
        let waited = self.wait_until(Self::deadline(step.timeout_ms), |exec| {
            header_state(exec.live.session.screen())
                .is_some_and(|state| state.is(expected_state))
                .then_some(())
        })?;
        match waited {
            Waited::Ready(()) => Ok(()),
            other => Err(self.unmet(
                index,
                &other,
                format!("the header badge to read {expected_state}"),
                step.timeout_ms,
                || {
                    format!(
                        "the header badge reads {}; the header shows:\n{}",
                        header_state(self.live.session.screen()).map_or_else(
                            || "nothing yet".to_owned(),
                            |state| state.label().to_owned()
                        ),
                        self.live.session.screen().rows_text(0, 2)
                    )
                },
            )),
        }
    }

    fn wait_event(&mut self, index: usize, step: &WaitEvent) -> Result<(), Stop> {
        let (kind, fields) = (step.event, &step.fields);
        let waited = self.wait_until(Self::deadline(step.timeout_ms), |exec| {
            exec.live
                .events
                .events()
                .iter()
                .any(|event| event.matches(kind, fields))
                .then_some(())
        })?;
        match waited {
            Waited::Ready(()) => Ok(()),
            other => Err(self.unmet(
                index,
                &other,
                format!("a `{kind}` event satisfying {fields:?}"),
                step.timeout_ms,
                || self.live.events_summary(),
            )),
        }
    }

    /// Presses one key. The bytes are the step's own, written outside every protocol, so they are
    /// noted before they go out ([`Self::send_raw`]).
    fn key(&mut self, index: usize) -> Result<(), Stop> {
        let prepared = self.prepared;
        let Prepared::Key(bytes) = &prepared[index] else {
            return Err(Self::unprepared(index, "key"));
        };
        self.send_raw(index, bytes)
    }

    /// Types text, one key press per character, each noted and sent as a `key` step's are.
    fn type_text(&mut self, index: usize) -> Result<(), Stop> {
        let prepared = self.prepared;
        let Prepared::Typed(keys) = &prepared[index] else {
            return Err(Self::unprepared(index, "type"));
        };
        for key in keys {
            self.send_raw(index, key)?;
        }
        Ok(())
    }

    /// Writes bytes that a `key` or `type` step holds, outside every protocol.
    ///
    /// The program may read them as a request for a deletion, and the dialog that opens is one
    /// that no protocol verified, so they are noted first: from then on the run is engaged, and
    /// every write that could confirm a deletion is checked on an exact screen before it is sent
    /// ([`Drive::send_confirming`], and "Raw deletion requests" in `runner::live`), this one
    /// included. A run that has never made such a request writes as it always did.
    fn send_raw(&mut self, index: usize, bytes: &[u8]) -> Result<(), Stop> {
        self.live.note_raw_write(bytes);
        self.send_confirming(bytes, Self::deadline(DEFAULT_TIMEOUT_MS), Needs::Nothing)
            .map(|_| ())
            .map_err(|error| self.protocol_stop(index, DEFAULT_TIMEOUT_MS, error))
    }

    /// Opens the filter, types the name, applies it, and asserts that the inspector shows exactly
    /// that entry (`Drive::select_entry`).
    fn select(&mut self, index: usize, step: &Select) -> Result<(), Stop> {
        let prepared = self.prepared;
        let Prepared::Typed(name_keys) = &prepared[index] else {
            return Err(Self::unprepared(index, "select"));
        };
        let deadline = Self::deadline(step.timeout_ms);
        self.select_entry(step.name.as_str(), name_keys, deadline)
            .map_err(|error| self.protocol_stop(index, step.timeout_ms, error))
    }

    /// Presses Backspace, reads the confirmation dialog, and presses the `confirm_with` key (`y`
    /// or Enter) only when the dialog names exactly the requested entry, at the step's `path` or
    /// as `name` directly below the root (`Drive::confirm_deletion`; see [`super::delete`] for
    /// what is checked). `wait_for` (default `"finished"`) controls when the step returns: on the
    /// first frame after `deletion_finished`, which shows the result, or (`"started"`) as soon as
    /// the dialog has closed, while the deletion keeps running in the background.
    ///
    /// A scenario that sets `disable_delete_confirmation` has no dialog to read: see
    /// [`Self::delete_without_dialog`].
    fn delete(&mut self, index: usize, step: &Delete) -> Result<(), Stop> {
        if self.scenario.disable_delete_confirmation {
            return self.delete_without_dialog(index, step);
        }
        let deadline = Self::deadline(step.timeout_ms);
        let (fixture, scenario) = (self.fixture, self.scenario);
        let request = DeletionRequest {
            name: &step.name,
            kind: step.kind,
            fixture,
            sentinels: &scenario.sentinels,
            // Where the entry is: the step's `path`, or `name` itself directly below the root.
            relative: step.path.as_deref().unwrap_or(&step.name),
            confirm_with: step.confirm_with,
        };
        let confirmed = self
            .confirm_deletion(&request, deadline)
            .map_err(|error| self.protocol_stop(index, step.timeout_ms, error))?;
        self.recorder.record_deletion_confirmed(confirmed.at);
        self.intended_deletions.push(confirmed.verified.relative);

        match step.wait_for {
            // The confirmation was sent and processed: the dialog closing is the program leaving
            // `DeleteConfirm` for the planning/execution path it enters in the same turn
            // (`queue_confirmed_deletion`), so the deletion has started. Nothing here waits for it
            // to finish; the deletion keeps running after this returns.
            DeleteWait::Started => self
                .wait_dialog_closed(deadline)
                .map_err(|error| self.protocol_stop(index, step.timeout_ms, error)),
            DeleteWait::Finished => self
                .wait_deletion_finished(confirmed.events_before, deadline)
                .map(|_| ())
                .map_err(|error| self.protocol_stop(index, step.timeout_ms, error)),
        }
    }

    /// `disable_delete_confirmation`: Backspace alone starts the deletion, so there is no dialog to
    /// read and no key to hold back. The step waits for the selected-item panel to name the entry,
    /// then checks the disk and the sentinels (`verify_selection`) and the ownership marker of the
    /// fixture; presses Backspace; fails if a dialog opens (the program did not honour the mode,
    /// and no key is sent to confirm it); and then waits like `wait_for = "finished"`.
    ///
    /// The panel is all that says which entry Backspace deletes, so it is read after a barrier
    /// ([`Drive::barrier`]): once the program has answered one and the screen shows the frame that
    /// answers it, the program has read every key sent so far, however the terminal cut them into
    /// events, and the panel shows what they did (see "The input barrier" in `runner::live`). That
    /// is only possible where the screen is exact. A terminal that paints on its own timer
    /// (`ConPTY`, on Windows) cannot tie its screen to a frame, and a program that does not mark
    /// its frames, or does not answer a barrier, cannot say that the screen shows what it has
    /// read: each is refused before any key is sent, a Backspace included
    /// ([`Live::deletion_refusal`]).
    fn delete_without_dialog(&mut self, index: usize, step: &Delete) -> Result<(), Stop> {
        let deadline = Self::deadline(step.timeout_ms);
        let relative = step.path.as_deref().unwrap_or(&step.name);
        let expected = format!(
            "the selected-item panel to show the {} {:?}, which Backspace alone deletes at {relative:?} under {}",
            step.kind,
            step.name,
            self.fixture.path().display()
        );

        self.pump()?;
        if let Some(reason) = self.live.deletion_refusal() {
            return Err(self.fail(index, FailureCause::DeleteRefused, expected, reason));
        }
        // The selection is what the panel shows once the program has read every key sent so far.
        let answered = self.barrier(deadline)?;
        if !matches!(answered, Waited::Ready(())) {
            return Err(self.unmet(
                index,
                &answered,
                "the program to answer an input barrier, and the screen to show the frame that \
                 answers it",
                step.timeout_ms,
                || {
                    format!(
                        "the screen shows:\n{}\n{}",
                        self.live.session.screen().text(),
                        self.live.frames_summary()
                    )
                },
            ));
        }
        let selected = self.wait_for_the_panel(index, step, deadline, relative, &expected)?;
        let verified = verify_selection(
            &selected,
            &step.name,
            step.kind,
            relative,
            self.fixture,
            &self.scenario.sentinels,
        )
        .map_err(|reason| {
            self.fail(index, FailureCause::DeleteRefused, expected.clone(), reason)
        })?;
        // The marker is the last thing looked at, right before the key that deletes.
        self.fixture.verify_owned().map_err(|error| {
            self.fail(
                index,
                FailureCause::DeleteRefused,
                expected.clone(),
                format!(
                    "the fixture root is no longer owned by the harness: {error}; no key was sent"
                ),
            )
        })?;

        // Backspace is the whole confirmation: record it as the moment the deletion was asked for.
        let events_before = self.live.events.events().len();
        let at = self.send_input(&BACKSPACE)?;
        self.recorder.record_deletion_confirmed(at);
        self.intended_deletions.push(verified.relative);

        self.refuse_a_dialog_after_backspace(index, step, deadline, expected)?;
        self.wait_deletion_finished(events_before, deadline)
            .map(|_| ())
            .map_err(|error| self.protocol_stop(index, step.timeout_ms, error))
    }

    /// The part of the dialog-free deletion that waits for the selected-item panel to name the
    /// entry. The panel can trail the header: a fresh map arms its cursor in the frame that ends
    /// the scan, and the terminal may deliver that frame in pieces, so a panel that shows nothing,
    /// or another entry, is not an answer yet. The step waits for the panel to name this entry and
    /// kind, and refuses only when it never does within the step's `timeout_ms`. Nothing is written
    /// meanwhile, so no key can change the selection while it is waited for.
    fn wait_for_the_panel(
        &mut self,
        index: usize,
        step: &Delete,
        deadline: Instant,
        relative: &str,
        expected: &str,
    ) -> Result<SelectedItem, Stop> {
        let kind = step.kind.to_string();
        let shown = self.wait_until(deadline, |exec| {
            match inspector(exec.live.session.screen()) {
                Inspector::Item(item) if item.name == step.name && item.kind == kind => Some(item),
                _ => None,
            }
        })?;
        let other = match shown {
            Waited::Ready(selected) => return Ok(selected),
            other => other,
        };
        let refusal = match inspector(self.live.session.screen()) {
            Inspector::Item(item) if item.name == step.name && item.kind == kind => None,
            Inspector::Item(item) => verify_selection(
                &item,
                &step.name,
                step.kind,
                relative,
                self.fixture,
                &self.scenario.sentinels,
            )
            .err(),
            Inspector::NothingSelected => Some("the panel shows no selection".to_owned()),
            Inspector::NotShown => Some("the selected-item panel is not drawn".to_owned()),
        };
        if let (Some(reason), true) = (refusal, matches!(other, Waited::TimedOut)) {
            return Err(self.fail(index, FailureCause::DeleteRefused, expected, reason));
        }
        Err(self.unmet(
            index,
            &other.discard(),
            "the panel to name the entry, on a screen that shows the frame that answered the \
             barrier",
            step.timeout_ms,
            || {
                format!(
                    "the screen shows:\n{}\n{}",
                    self.live.session.screen().text(),
                    self.live.frames_summary()
                )
            },
        ))
    }

    /// After the Backspace of a deletion that no dialog confirms: waits for the program to answer
    /// a barrier written behind it and for the screen to show the frame that answers, and fails if
    /// any dialog opened. A confirmation dialog means the program did not honour the mode, and no
    /// key is ever sent to confirm it.
    fn refuse_a_dialog_after_backspace(
        &mut self,
        index: usize,
        step: &Delete,
        deadline: Instant,
        expected: String,
    ) -> Result<(), Stop> {
        let answered = self.barrier(deadline)?;
        let opened = match answered {
            Waited::Ready(()) => dialog_view(self.live.session.screen()),
            other => {
                return Err(self.unmet(
                    index,
                    &other,
                    "the program to answer an input barrier written behind the Backspace, and the \
                     screen to show the frame that answers it",
                    step.timeout_ms,
                    || self.live.events_summary(),
                ));
            }
        };
        match opened {
            DialogView::None => Ok(()),
            DialogView::Delete(dialog) => Err(self.fail(
                index,
                FailureCause::DeleteRefused,
                expected,
                format!(
                    "a confirmation dialog opened although the scenario disables confirmation, \
                     so the program did not honour the mode; no key was sent to confirm it:\n{}",
                    dialog.view.text
                ),
            )),
            DialogView::Other(other) => Err(self.fail(
                index,
                FailureCause::DeleteRefused,
                expected,
                format!(
                    "the dialog `{}` opened instead of a deletion starting:\n{}",
                    other.title, other.text
                ),
            )),
        }
    }

    /// `wait_refresh`: waits until the map on screen has caught up with the deletions confirmed so
    /// far, and for the frame that shows it.
    ///
    /// A deletion that removed entries leaves the map listing them until the program replaces it,
    /// and a quit meanwhile cancels the replacement (exit code 130). The step needs every
    /// confirmed deletion to have reported (`deletion_finished`), and then the program's
    /// `refresh_finished` after the last report that removed something: a refresh that ended
    /// before that deletion is no answer, which is why no `wait_event` can name the event. A
    /// deletion that removed nothing owes none. The step ends as `delete` does, on a frame that
    /// follows the event it waited for, once the screen shows it.
    fn wait_refresh(&mut self, index: usize, step: WaitRefresh) -> Result<(), Stop> {
        let confirmed = self.intended_deletions.len();
        let deadline = Self::deadline(step.timeout_ms);
        let waited = self.wait_until(deadline, |exec| exec.refresh_after(confirmed))?;
        let after = match waited {
            Waited::Ready(Refresh::Settled { after }) => after,
            Waited::Ready(Refresh::Failed) => {
                return Err(self.fail(
                    index,
                    FailureCause::Mismatch,
                    "the map to catch up with the deletions confirmed so far",
                    "the program reported `refresh_finished` with the outcome `failed`: no map \
                     could be shown after the deletion",
                ));
            }
            other => {
                return Err(self.unmet(
                    index,
                    &other.discard(),
                    "the map to catch up with the deletions confirmed so far (a \
                     `refresh_finished` event after the last `deletion_finished` that removed \
                     entries)",
                    step.timeout_ms,
                    || self.live.events_summary(),
                ));
            }
        };
        let shown = self.wait_for_frame_from(after + 1, deadline)?;
        match shown {
            Waited::Ready(()) => Ok(()),
            other => Err(self.unmet(
                index,
                &other,
                "a frame after the refresh finished, shown on the screen",
                step.timeout_ms,
                || self.live.events_summary(),
            )),
        }
    }

    /// Where the events say the map stands after the `confirmed` deletions the scenario has
    /// confirmed, or `None` while a deletion has not reported or its refresh has not ended.
    fn refresh_after(&self, confirmed: usize) -> Option<Refresh> {
        let events = self.live.events.events();
        let mut reported = 0;
        let mut last_report = 0;
        let mut last_removing = None;
        for (at, event) in events.iter().enumerate() {
            if let Payload::DeletionFinished { removed, .. } = event.payload {
                reported += 1;
                last_report = at;
                if removed > 0 {
                    last_removing = Some(at);
                }
            }
        }
        if reported < confirmed {
            return None;
        }
        let Some(removing) = last_removing else {
            return Some(Refresh::Settled { after: last_report });
        };
        events[removing + 1..]
            .iter()
            .enumerate()
            .find_map(|(offset, event)| match event.payload {
                Payload::RefreshFinished {
                    outcome: RefreshOutcome::Published,
                } => Some(Refresh::Settled {
                    after: removing + 1 + offset,
                }),
                Payload::RefreshFinished {
                    outcome: RefreshOutcome::Failed,
                } => Some(Refresh::Failed),
                _ => None,
            })
    }

    /// Applies a live change to the fixture as the step runs. The fixture generator's mutator
    /// checks the ownership marker again and refuses links and the marker itself; a refused
    /// mutation fails the step and changes nothing. A mutation that was applied is an intended
    /// change, so the final comparison with the fixture as it was does not report it.
    fn fs_mutate(&mut self, index: usize, step: &FsMutate) -> Result<(), Stop> {
        mutate::apply(self.fixture.path(), step.op, &step.path).map_err(|error| {
            self.fail(
                index,
                FailureCause::Mismatch,
                format!("the fixture to accept `{}` of {:?}", step.op, step.path),
                error.to_string(),
            )
        })?;
        self.intended_mutations.push(step.path.clone());
        Ok(())
    }

    fn wait_fs(&mut self, index: usize, step: &WaitFs, present: bool) -> Result<(), Stop> {
        let fixture = self.fixture;
        let path = step.path.as_str();
        let mut next_check = Instant::now();
        let waited = self.wait_until(Self::deadline(step.timeout_ms), |_| {
            let now = Instant::now();
            if now < next_check {
                return None;
            }
            next_check = now + FS_POLL_INTERVAL;
            match fixture.exists(path) {
                Ok(exists) if exists == present => Some(Ok(())),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            }
        })?;
        match waited {
            Waited::Ready(Ok(())) => Ok(()),
            Waited::Ready(Err(error)) => Err(error.into()),
            other => Err(self.unmet(
                index,
                &other.discard(),
                format!(
                    "{path:?} to be {}",
                    if present { "present" } else { "absent" }
                ),
                step.timeout_ms,
                || format!("it is still {}", if present { "absent" } else { "present" }),
            )),
        }
    }

    /// Resizes the terminal and waits for the frame that answers it, then until the screen shows
    /// it (`Drive::catch_up`). A resize reaches the program as an input event, so the frame's
    /// counter tells when the program has laid itself out again.
    fn resize(&mut self, index: usize, step: Resize) -> Result<(), Stop> {
        self.live.resize(step.cols, step.rows)?;
        let deadline = Self::deadline(DEFAULT_TIMEOUT_MS);
        let waited = self.wait_until(deadline, |exec| exec.live.reflecting_frame_seq())?;
        let shown = match waited {
            Waited::Ready(seq) => {
                // The program counts the events it really received. Two resizes in a row can reach
                // it as one, so believe the frame over our own count.
                self.live.believe_latest_frame();
                self.catch_up(seq, deadline)?
            }
            other => other.discard(),
        };
        match shown {
            Waited::Ready(()) => Ok(()),
            other => Err(self.unmet(
                index,
                &other,
                format!(
                    "a frame after the resize to {}x{}, shown on the screen",
                    step.cols, step.rows
                ),
                DEFAULT_TIMEOUT_MS,
                || self.live.events_summary(),
            )),
        }
    }

    /// Delivers `step.signal`. `close` has no process id to signal: it is delivered by closing
    /// the pseudo-terminal's controlling side (`PtySession::close_console`), not by
    /// `safety::send_signal`.
    fn signal(&mut self, index: usize, step: SendSignal) -> Result<(), Stop> {
        self.pump()?;
        if let Some(exit) = self.live.session.exit() {
            return Err(self.fail(
                index,
                FailureCause::ProcessExited,
                format!("the program to receive `{}`", step.signal),
                format!("the program had already ended ({})", exit.describe()),
            ));
        }
        match step.signal {
            Signal::Close => self.live.session.close_console(),
            other => send_signal(self.live.session.pid(), other)?,
        }
        Ok(())
    }

    fn expect_screen(&mut self, index: usize, step: &ExpectScreen) -> Result<(), Stop> {
        self.pump()?;
        let prepared = self.prepared;
        let Prepared::Screen {
            contains,
            not_contains,
            patterns,
        } = &prepared[index]
        else {
            return Err(Self::unprepared(index, "expect_screen"));
        };
        let text = self.live.session.screen().region_text(step.region);
        let mut problems = Vec::new();
        for matcher in contains.iter().chain(patterns) {
            if !matcher.is_match(&text) {
                problems.push(format!("{} is missing", matcher.describe()));
            }
        }
        for matcher in not_contains {
            if matcher.is_match(&text) {
                problems.push(format!("{} is present", matcher.describe()));
            }
        }
        if problems.is_empty() {
            return Ok(());
        }
        Err(self.fail(
            index,
            FailureCause::Mismatch,
            format!(
                "the screen{} to satisfy every check",
                describe_region(step.region)
            ),
            format!("{}; the screen shows:\n{text}", problems.join("; ")),
        ))
    }

    fn expect_fs(&mut self, index: usize, step: &ExpectFs) -> Result<(), Stop> {
        let mut problems = Vec::new();
        for path in &step.present {
            if !self.fixture.exists(path)? {
                problems.push(format!("{path:?} is missing"));
            }
        }
        for path in &step.absent {
            if self.fixture.exists(path)? {
                problems.push(format!("{path:?} exists"));
            }
        }
        if problems.is_empty() {
            return Ok(());
        }
        Err(self.fail(
            index,
            FailureCause::Mismatch,
            format!("present {:?} and absent {:?}", step.present, step.absent),
            problems.join("; "),
        ))
    }

    /// Reads the scratch configuration file, the one `EXCISE_CONFIG` names, and compares one
    /// setting of it. The step never waits: a `settle` before it makes the program's last key
    /// reach the file.
    fn expect_config(&mut self, index: usize, step: &ExpectConfig) -> Result<(), Stop> {
        self.pump()?;
        let found = fs::read_to_string(self.scratch.config_file())
            .map_err(|error| format!("the configuration file cannot be read: {error}"))
            .and_then(|text| config_setting(&text, &step.key));
        match found {
            Ok(value) if value == step.equals => Ok(()),
            Ok(value) => Err(self.fail(
                index,
                FailureCause::Mismatch,
                format!(
                    "`{}` in the configuration file to be {:?}",
                    step.key, step.equals
                ),
                format!("it is {value:?}"),
            )),
            Err(reason) => Err(self.fail(
                index,
                FailureCause::Mismatch,
                format!(
                    "`{}` in the configuration file to be {:?}",
                    step.key, step.equals
                ),
                reason,
            )),
        }
    }

    /// Waits for the exit, reads the rest of the output, and asserts how the program ended.
    fn expect_exit(&mut self, index: usize, step: &ExpectExit) -> Result<(), Stop> {
        let waited = self.wait_until(Self::deadline(step.timeout_ms), |exec| {
            exec.live.session.exit().map(|_| ())
        })?;
        if matches!(waited, Waited::TimedOut) {
            return Err(self.unmet(
                index,
                &waited,
                "the program to exit",
                step.timeout_ms,
                || format!("it is still running; {}", self.live.events_summary()),
            ));
        }
        let deadline = Instant::now() + EXIT_OUTPUT_LIMIT;
        while !self.live.session.finished() && Instant::now() < deadline {
            self.pump()?;
            self.live
                .session
                .wait_activity(std::time::Duration::from_millis(1))?;
        }
        self.pump()?;
        let Some(exit) = self.live.session.exit() else {
            return Err(Self::unprepared(index, "expect_exit"));
        };

        let mut problems = Vec::new();
        if exit.code != Some(step.code) {
            problems.push(format!(
                "the program ended with {}, not exit code {}",
                exit.describe(),
                step.code
            ));
        }
        // After a `close` event the console is gone, and nothing the program writes reaches the
        // screen model: the terminal cannot be inspected, so it is not checked (`plan::prepare`
        // refuses `terminal_restored = false` there, which would otherwise pass unexamined).
        let terminal_checked = !self.live.session.console_closed();
        let modes = self.live.session.modes();
        if terminal_checked && modes.restored() != step.terminal_restored {
            problems.push(format!(
                "the terminal is {} (alternate screen {}, cursor {}, echo {}, canonical mode {}), \
                 but the step expects it to be {}",
                if modes.restored() {
                    "restored"
                } else {
                    "not restored"
                },
                on_off(Some(modes.alternate_screen)),
                if modes.cursor_visible {
                    "visible"
                } else {
                    "hidden"
                },
                on_off(modes.echo),
                on_off(modes.icanon),
                if step.terminal_restored {
                    "restored"
                } else {
                    "not restored"
                },
            ));
        }
        match step.residue {
            Residue::None => {
                let residue = self.scratch.residue()?;
                self.residue_files = Some(residue.len());
                if !residue.is_empty() {
                    problems.push(format!("the scratch area holds leftovers: {residue:?}"));
                }
                let after = FixtureSnapshot::take(self.fixture.path())?;
                problems.extend(
                    self.baseline
                        .diff(&after)
                        .unexpected(&self.intended_deletions, &self.intended_mutations),
                );
            }
        }
        if problems.is_empty() {
            return Ok(());
        }
        let terminal = match (terminal_checked, step.terminal_restored) {
            (false, _) => "not checked (the console was closed)",
            (true, true) => "restored",
            (true, false) => "not restored",
        };
        Err(self.fail(
            index,
            FailureCause::Mismatch,
            format!("exit code {}, terminal {terminal}, no residue", step.code),
            problems.join("; "),
        ))
    }

    /// Holds a recorded metric to its budget's limit. A missed latency budget fails the step,
    /// unless the run holds timing informational: then the miss is recorded as a warning and the
    /// step passes. Every other budget, and a metric that was not recorded, fails the step either
    /// way.
    fn expect_budget(&mut self, index: usize, step: &ExpectBudget) -> Result<(), Stop> {
        self.pump()?;
        let metrics = self.metrics();
        let Some(limit) = limit_for(self.scenario, step.budget, self.latency_scale) else {
            return Err(Self::unprepared(index, "expect_budget"));
        };
        let basis = if is_latency(step.budget) && !self.latency_scale.is_strict() {
            format!(
                "{}, the strict limit scaled by {}",
                step.budget,
                self.latency_scale.factor()
            )
        } else {
            step.budget.to_string()
        };
        let expected = format!("`{}` to be at most {limit} ({basis})", step.metric);
        let Some(value) = metrics.get(&step.metric) else {
            let recorded: Vec<&str> = metrics.keys().map(String::as_str).collect();
            return Err(self.fail(
                index,
                FailureCause::Mismatch,
                expected,
                format!("the metric was not recorded; recorded metrics: {recorded:?}"),
            ));
        };
        if *value <= limit {
            return Ok(());
        }
        if self.timing_informational && is_latency(step.budget) {
            // The run measures timing and does not gate on it: the miss is a warning, and the
            // scenario goes on to its other steps, whose failures still fail it.
            self.timing_warnings.push(TimingWarning {
                budget: step.budget,
                metric: step.metric.clone(),
                value: *value,
                limit,
            });
            return Ok(());
        }
        Err(self.fail(
            index,
            FailureCause::Mismatch,
            expected,
            format!("`{}` is {value}", step.metric),
        ))
    }

    fn measure(&mut self, step: &Measure) {
        let now = Instant::now();
        match step.marker {
            crate::scenario::Marker::Start => self.recorder.start_measure(&step.name, now),
            crate::scenario::Marker::Stop => {
                self.recorder.stop_measure(&step.name, now);
            }
        }
    }

    /// Sends nothing, waits `step.after_ms`, then measures over `step.window_ms` the terminal
    /// output bytes and the child's live CPU time, recording them as the metrics
    /// `idle_output_bytes` and `idle_cpu_ms`. `idle_cpu_ms` is recorded only when both samples
    /// were readable (Windows has no live CPU sampler yet, see `metrics::live_cpu_ms`);
    /// `idle_output_bytes` always is, because the runner counts output itself.
    fn idle(&mut self, index: usize, step: &Idle) -> Result<(), Stop> {
        if self.wait_quietly(Duration::from_millis(step.after_ms))? {
            return Err(self.idle_exited(index, step.after_ms));
        }

        let start_bytes = self.live.session.output_bytes();
        let start_cpu = live_cpu_ms(self.live.session.pid());

        if self.wait_quietly(Duration::from_millis(step.window_ms))? {
            return Err(self.idle_exited(index, step.after_ms + step.window_ms));
        }

        let output_bytes = self.live.session.output_bytes().saturating_sub(start_bytes);
        #[allow(clippy::cast_precision_loss)]
        self.recorder
            .record_metric("idle_output_bytes", output_bytes as f64);

        if let (Some(start), Some(end)) = (start_cpu, live_cpu_ms(self.live.session.pid())) {
            self.recorder
                .record_metric("idle_cpu_ms", (end - start).max(0.0));
        }
        Ok(())
    }

    /// The failure for the program ending before an idle window of `waited_ms` finished.
    fn idle_exited(&self, index: usize, waited_ms: u64) -> Stop {
        self.fail(
            index,
            FailureCause::ProcessExited,
            format!("the program to still be running {waited_ms} ms into the idle step"),
            format!("it ended first ({})", self.live.exit_summary()),
        )
    }

    /// Waits until the program has drawn a frame that reflects every input sent so far, then
    /// until the screen shows it (`Drive::settle_until`).
    fn settle(&mut self, index: usize, step: Settle) -> Result<(), Stop> {
        let waited = self.settle_until(Self::deadline(step.timeout_ms))?;
        match waited {
            Waited::Ready(()) => Ok(()),
            other => Err(self.unmet(
                index,
                &other,
                format!(
                    "a frame that reflects all {} inputs sent",
                    self.live.inputs_sent()
                ),
                step.timeout_ms,
                || {
                    format!(
                        "the latest frame counted {} inputs, and a frame that reflects all {} \
                         sent counts {}; {}",
                        self.live.latest_frame_inputs(),
                        self.live.inputs_sent(),
                        self.live.reflecting_counter(),
                        self.live.events_summary()
                    )
                },
            )),
        }
    }

    /// Presses `q`, waits for the quit dialog, and confirms it with `y`.
    ///
    /// The `y` waits for nothing but the frame event that counts the `q`, which is what keeps a
    /// quit on a slow terminal from pressing it after the program has begun drawing the next
    /// frame (`quit_ms`). That holds while no key of the scenario's own has asked for a deletion.
    /// After one has (`Live::note_raw_write`), a dialog for it can be unread, covered by a prompt,
    /// or still to be drawn, and the `q` can restore it instead of opening the prompt that the
    /// frame event and a stale screen say is open. Then the `q` goes out as always, and the `y`
    /// goes through [`Drive::send_confirming`]: behind a barrier, on the exact screen that
    /// answers it, and only if that screen shows no deletion dialog and the plain quit prompt.
    fn quit(&mut self, index: usize, step: Quit) -> Result<(), Stop> {
        let deadline = Self::deadline(step.timeout_ms);
        if self.live.raw_deletion_requested() {
            self.send_input(b"q")?;
            let at = self
                .send_confirming(b"y", deadline, Needs::PlainQuitPrompt)
                .map_err(|error| self.protocol_stop(index, step.timeout_ms, error))?;
            self.recorder.record_quit_confirmed(at);
            return Ok(());
        }
        let view = self
            .request_quit(deadline)
            .map_err(|error| self.protocol_stop(index, step.timeout_ms, error))?;
        if !offers_plain_quit(&view) {
            return Err(self.fail(
                index,
                FailureCause::Mismatch,
                "the quit dialog to offer `[y] Quit`",
                format!("the dialog offers something else:\n{}", view.text),
            ));
        }
        let at = self.send_input(b"y")?;
        self.recorder.record_quit_confirmed(at);
        Ok(())
    }

    /// The checks that hold after the last step: every sentinel survives, and once the program is
    /// gone the fixture changed only by confirmed deletions.
    pub(super) fn final_checks(&mut self) -> Result<(), Stop> {
        let index = self.scenario.steps.len();
        self.pump()?;
        for sentinel in &self.scenario.sentinels {
            if !self.fixture.exists(sentinel)? {
                return Err(self.fail(
                    index,
                    FailureCause::Mismatch,
                    "every sentinel to survive the scenario",
                    format!("the sentinel `{sentinel}` is gone"),
                ));
            }
        }
        if self.live.session.exit().is_some() {
            let after = FixtureSnapshot::take(self.fixture.path())?;
            let unexpected = self
                .baseline
                .diff(&after)
                .unexpected(&self.intended_deletions, &self.intended_mutations);
            if !unexpected.is_empty() {
                return Err(self.fail(
                    index,
                    FailureCause::Mismatch,
                    "the fixture to change only by confirmed deletions",
                    unexpected.join("; "),
                ));
            }
        }
        Ok(())
    }
}

fn on_off(mode: Option<bool>) -> &'static str {
    match mode {
        Some(true) => "on",
        Some(false) => "off",
        None => "unknown",
    }
}

#[cfg(all(test, unix))]
mod tests;
