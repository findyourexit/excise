//! The steps of the scenario vocabulary, implemented on the executor.

use std::time::Instant;

use crate::{
    events::Payload,
    fixture::mutate,
    pty::ui::{
        DialogView, FilterPrompt, Inspector, dialog_view, filter_prompt, header_state, inspector,
        offers_plain_quit,
    },
    safety::{FixtureSnapshot, send_signal},
    scenario::{
        DEFAULT_TIMEOUT_MS, Delete, ExpectBudget, ExpectExit, ExpectFs, ExpectScreen, FsMutate,
        Measure, Quit, Residue, Resize, Select, SendSignal, Settle, Step, WaitEvent, WaitFs,
        WaitHeader, WaitText,
    },
};

use super::{
    budget::limit_for,
    delete::verify,
    exec::{Executor, FS_POLL_INTERVAL, Waited},
    outcome::{FailureCause, RunError, Stop},
    plan::{Prepared, describe_region},
};

/// The bytes of a Backspace key press.
const BACKSPACE: [u8; 1] = [0x7f];
/// The bytes of an Enter key press.
const ENTER: [u8; 1] = *b"\r";
/// After a `settle`, output already in flight is read until it has been quiet this long...
const SETTLE_QUIET: std::time::Duration = std::time::Duration::from_millis(3);
/// ...or this long has passed.
const SETTLE_LIMIT: std::time::Duration = std::time::Duration::from_millis(20);
/// How long `expect_exit` reads the rest of the output after the program has exited.
const EXIT_OUTPUT_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

impl<T> Waited<T> {
    /// The outcome without its value.
    pub(super) const fn discard(&self) -> Waited<()> {
        match self {
            Self::Ready(_) => Waited::Ready(()),
            Self::TimedOut => Waited::TimedOut,
            Self::Exited => Waited::Exited,
        }
    }
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
            Step::WaitFsAbsent(step) => self.wait_fs(index, step, false),
            Step::WaitFsPresent(step) => self.wait_fs(index, step, true),
            Step::FsMutate(step) => self.fs_mutate(index, step),
            Step::Resize(step) => self.resize(index, *step),
            Step::Signal(step) => self.signal(index, *step),
            Step::ExpectScreen(step) => self.expect_screen(index, step),
            Step::ExpectFs(step) => self.expect_fs(index, step),
            Step::ExpectExit(step) => self.expect_exit(index, step),
            Step::ExpectBudget(step) => self.expect_budget(index, step),
            Step::Measure(step) => {
                self.measure(step);
                Ok(())
            }
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
                .is_match(&exec.session.screen().region_text(region))
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
                        self.session.screen().region_text(region)
                    )
                },
            )),
        }
    }

    fn wait_header(&mut self, index: usize, step: &WaitHeader) -> Result<(), Stop> {
        let expected_state = step.state;
        let waited = self.wait_until(Self::deadline(step.timeout_ms), |exec| {
            header_state(exec.session.screen())
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
                        header_state(self.session.screen()).map_or_else(
                            || "nothing yet".to_owned(),
                            |state| state.label().to_owned()
                        ),
                        self.session.screen().rows_text(0, 2)
                    )
                },
            )),
        }
    }

    fn wait_event(&mut self, index: usize, step: &WaitEvent) -> Result<(), Stop> {
        let (kind, fields) = (step.event, &step.fields);
        let waited = self.wait_until(Self::deadline(step.timeout_ms), |exec| {
            exec.events
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
                || self.events_summary(),
            )),
        }
    }

    fn key(&mut self, index: usize) -> Result<(), Stop> {
        let prepared = self.prepared;
        let Prepared::Key(bytes) = &prepared[index] else {
            return Err(Self::unprepared(index, "key"));
        };
        self.send_input(bytes)?;
        Ok(())
    }

    fn type_text(&mut self, index: usize) -> Result<(), Stop> {
        let prepared = self.prepared;
        let Prepared::Typed(keys) = &prepared[index] else {
            return Err(Self::unprepared(index, "type"));
        };
        for key in keys {
            self.send_input(key)?;
        }
        Ok(())
    }

    /// Waits for a frame that reflects every input so far and a filter prompt that `accept`s.
    fn wait_filter_prompt(
        &mut self,
        deadline: Instant,
        accept: impl Fn(&FilterPrompt) -> bool,
    ) -> Result<Waited<FilterPrompt>, Stop> {
        self.wait_until(deadline, |exec| {
            exec.frame_reflecting_inputs()?;
            filter_prompt(exec.session.screen()).filter(|prompt| accept(prompt))
        })
    }

    /// Opens the filter, types the name, applies it, and asserts that the inspector shows exactly
    /// that entry.
    ///
    /// The filter opens with the previous filter's text in place, so the step reads the prompt and
    /// erases what is there. It checks the prompt after each stage instead of assuming the keys
    /// did what they usually do.
    fn select(&mut self, index: usize, step: &Select) -> Result<(), Stop> {
        let prepared = self.prepared;
        let Prepared::Typed(name_keys) = &prepared[index] else {
            return Err(Self::unprepared(index, "select"));
        };
        let deadline = Self::deadline(step.timeout_ms);
        let name = step.name.as_str();
        let screen_rows = |exec: &Self| exec.session.screen().rows_text(0, 2);

        self.send_input(b"/")?;
        let opened = self.wait_filter_prompt(deadline, |_| true)?;
        let Waited::Ready(prompt) = opened else {
            return Err(self.unmet(
                index,
                &opened.discard(),
                "the filter prompt to open",
                step.timeout_ms,
                || format!("the header shows:\n{}", screen_rows(self)),
            ));
        };

        for _ in prompt.input.chars() {
            self.send_input(&BACKSPACE)?;
        }
        if !prompt.input.is_empty() {
            let erased = self.wait_filter_prompt(deadline, |prompt| prompt.input.is_empty())?;
            if !matches!(erased, Waited::Ready(_)) {
                return Err(self.unmet(
                    index,
                    &erased.discard(),
                    "the previous filter to be erased",
                    step.timeout_ms,
                    || format!("the header shows:\n{}", screen_rows(self)),
                ));
            }
        }

        for key in name_keys {
            self.send_input(key)?;
        }
        let typed = self.wait_filter_prompt(deadline, |prompt| {
            prompt.input == name && prompt.error.is_none()
        })?;
        if !matches!(typed, Waited::Ready(_)) {
            return Err(self.unmet(
                index,
                &typed.discard(),
                format!("the filter prompt to read {name:?}"),
                step.timeout_ms,
                || {
                    format!(
                        "the prompt reads {:?}; the header shows:\n{}",
                        filter_prompt(self.session.screen()).map(|prompt| prompt.input),
                        screen_rows(self)
                    )
                },
            ));
        }

        self.send_input(&ENTER)?;
        let selected = self.wait_until(deadline, |exec| {
            exec.frame_reflecting_inputs()?;
            if filter_prompt(exec.session.screen()).is_some() {
                return None;
            }
            match inspector(exec.session.screen()) {
                Inspector::Item(item) if item.name == name => Some(item),
                _ => None,
            }
        })?;
        let Waited::Ready(_) = selected else {
            let shown = match inspector(self.session.screen()) {
                Inspector::Item(item) => {
                    format!("the inspector shows {:?} ({})", item.name, item.kind)
                }
                Inspector::NothingSelected => "the inspector shows no selection".to_owned(),
                Inspector::NotShown => "the inspector is not drawn".to_owned(),
            };
            let expected = format!("the inspector to show the selected item {name:?}");
            // An entry that is selected but is not the requested one is a wrong selection, not
            // merely a slow screen.
            if matches!(inspector(self.session.screen()), Inspector::Item(_))
                && matches!(selected, Waited::TimedOut)
            {
                return Err(self.fail(index, FailureCause::Mismatch, expected, shown));
            }
            return Err(self.unmet(
                index,
                &selected.discard(),
                expected,
                step.timeout_ms,
                || shown,
            ));
        };
        Ok(())
    }

    /// Presses Backspace, reads the confirmation dialog, and presses `y` only when the dialog
    /// names exactly the requested entry. See [`super::delete`] for what is checked. The step ends
    /// on the first frame after `deletion_finished`, which shows the result.
    fn delete(&mut self, index: usize, step: &Delete) -> Result<(), Stop> {
        let deadline = Self::deadline(step.timeout_ms);
        let expected = format!(
            "the deletion dialog to name the {} {:?} under {}",
            step.kind,
            step.name,
            self.fixture.path().display()
        );

        self.send_input(&BACKSPACE)?;
        let opened = self.wait_until(deadline, |exec| {
            exec.frame_reflecting_inputs()?;
            match dialog_view(exec.session.screen()) {
                DialogView::None => None,
                view => Some(view),
            }
        })?;
        let Waited::Ready(view) = opened else {
            return Err(self.unmet(
                index,
                &opened.discard(),
                "a deletion dialog to open",
                step.timeout_ms,
                || format!("the screen shows:\n{}", self.session.screen().text()),
            ));
        };
        let dialog = match view {
            DialogView::Delete(dialog) => dialog,
            DialogView::Other(other) => {
                return Err(self.fail(
                    index,
                    FailureCause::DeleteRefused,
                    expected,
                    format!(
                        "the dialog `{}` is open instead:\n{}",
                        other.title, other.text
                    ),
                ));
            }
            DialogView::None => return Err(Self::unprepared(index, "delete")),
        };
        let verified = verify(
            &dialog,
            &step.name,
            step.kind,
            self.fixture,
            &self.scenario.sentinels,
        )
        .map_err(|reason| {
            self.fail(
                index,
                FailureCause::DeleteRefused,
                expected.clone(),
                format!("{reason}\nthe dialog reads:\n{}", dialog.view.text),
            )
        })?;

        // Every check passed. Only now is the confirmation key sent.
        let events_before = self.events.events().len();
        let at = self.send_input(b"y")?;
        self.recorder.record_deletion_confirmed(at);
        self.intended_deletions.push(verified.relative);
        let finished = self.wait_until(deadline, |exec| {
            exec.events.events()[events_before..]
                .iter()
                .position(|event| matches!(event.payload, Payload::DeletionFinished { .. }))
                .map(|offset| events_before + offset)
        })?;
        let Waited::Ready(finished_at) = finished else {
            return Err(self.unmet(
                index,
                &finished.discard(),
                "the deletion to finish (a `deletion_finished` event)",
                step.timeout_ms,
                || self.events_summary(),
            ));
        };

        // The program starts rebuilding its map in the same turn that reports the deletion, so a
        // frame drawn after that report shows the result and everything the deletion set going.
        // The screen read before it can still show the map as it was.
        let shown = self.wait_until(deadline, |exec| {
            exec.events.events()[finished_at + 1..]
                .iter()
                .any(|event| matches!(event.payload, Payload::Frame { .. }))
                .then_some(())
        })?;
        match shown {
            Waited::Ready(()) => {
                self.session.drain(SETTLE_QUIET, SETTLE_LIMIT)?;
                self.pump()
            }
            other => Err(self.unmet(
                index,
                &other,
                "a frame after the deletion finished",
                step.timeout_ms,
                || self.events_summary(),
            )),
        }
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

    /// Resizes the terminal and waits for the frame that answers it. A resize reaches the program
    /// as an input event, so the frame's counter tells when the program has laid itself out again.
    fn resize(&mut self, index: usize, step: Resize) -> Result<(), Stop> {
        let at = self.session.resize(step.cols, step.rows)?;
        self.inputs_sent += 1;
        self.last_input_at = Some(at);
        let waited = self.wait_until(Self::deadline(DEFAULT_TIMEOUT_MS), |exec| {
            exec.frame_reflecting_inputs().map(|_| ())
        })?;
        match waited {
            Waited::Ready(()) => {
                // The program counts the events it really received. Two resizes in a row can reach
                // it as one, so believe the frame over our own count.
                self.inputs_sent = self.inputs_sent.max(self.latest_frame_inputs());
                Ok(())
            }
            other => Err(self.unmet(
                index,
                &other,
                format!("a frame after the resize to {}x{}", step.cols, step.rows),
                DEFAULT_TIMEOUT_MS,
                || self.events_summary(),
            )),
        }
    }

    fn signal(&mut self, index: usize, step: SendSignal) -> Result<(), Stop> {
        self.pump()?;
        if let Some(exit) = self.session.exit() {
            return Err(self.fail(
                index,
                FailureCause::ProcessExited,
                format!("the program to receive `{}`", step.signal),
                format!("the program had already ended ({})", exit.describe()),
            ));
        }
        send_signal(self.session.pid(), step.signal)?;
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
        let text = self.session.screen().region_text(step.region);
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

    /// Waits for the exit, reads the rest of the output, and asserts how the program ended.
    fn expect_exit(&mut self, index: usize, step: &ExpectExit) -> Result<(), Stop> {
        let waited = self.wait_until(Self::deadline(step.timeout_ms), |exec| {
            exec.session.exit().map(|_| ())
        })?;
        if matches!(waited, Waited::TimedOut) {
            return Err(self.unmet(
                index,
                &waited,
                "the program to exit",
                step.timeout_ms,
                || format!("it is still running; {}", self.events_summary()),
            ));
        }
        let deadline = Instant::now() + EXIT_OUTPUT_LIMIT;
        while !self.session.finished() && Instant::now() < deadline {
            self.pump()?;
            self.session
                .wait_activity(std::time::Duration::from_millis(1))?;
        }
        self.pump()?;
        let Some(exit) = self.session.exit() else {
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
        let modes = self.session.modes();
        if modes.restored() != step.terminal_restored {
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
        Err(self.fail(
            index,
            FailureCause::Mismatch,
            format!(
                "exit code {}, terminal {}, no residue",
                step.code,
                if step.terminal_restored {
                    "restored"
                } else {
                    "not restored"
                }
            ),
            problems.join("; "),
        ))
    }

    fn expect_budget(&mut self, index: usize, step: &ExpectBudget) -> Result<(), Stop> {
        self.pump()?;
        let metrics = self.metrics();
        let Some(limit) = limit_for(self.scenario, step.budget) else {
            return Err(Self::unprepared(index, "expect_budget"));
        };
        let expected = format!("`{}` to be at most {limit} ({})", step.metric, step.budget);
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
            Ok(())
        } else {
            Err(self.fail(
                index,
                FailureCause::Mismatch,
                expected,
                format!("`{}` is {value}", step.metric),
            ))
        }
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

    /// Waits until the program has drawn a frame that reflects every input sent so far, then reads
    /// the output that is still in flight.
    ///
    /// This is the pseudo-terminal meaning of `settle`. It never waits for the screen to go
    /// quiet: a program that keeps animating settles as soon as its frame counter catches up. A
    /// key that changes nothing draws no frame, so a `settle` after one waits in vain.
    fn settle(&mut self, index: usize, step: Settle) -> Result<(), Stop> {
        let waited = self.wait_until(Self::deadline(step.timeout_ms), |exec| {
            exec.frame_reflecting_inputs().map(|_| ())
        })?;
        match waited {
            Waited::Ready(()) => {
                self.session.drain(SETTLE_QUIET, SETTLE_LIMIT)?;
                self.pump()
            }
            other => Err(self.unmet(
                index,
                &other,
                format!("a frame that reflects all {} inputs sent", self.inputs_sent),
                step.timeout_ms,
                || {
                    format!(
                        "the latest frame counted {} inputs; {}",
                        self.latest_frame_inputs(),
                        self.events_summary()
                    )
                },
            )),
        }
    }

    /// Presses `q`, waits for the quit dialog, and confirms it with `y`.
    fn quit(&mut self, index: usize, step: Quit) -> Result<(), Stop> {
        let events_before = self.events.events().len();
        self.send_input(b"q")?;
        let opened = self.wait_until(Self::deadline(step.timeout_ms), |exec| {
            exec.frame_reflecting_inputs()?;
            let prompted = exec.events.events()[events_before..]
                .iter()
                .any(|event| matches!(event.payload, Payload::QuitPrompt));
            if !prompted {
                return None;
            }
            match dialog_view(exec.session.screen()) {
                DialogView::Other(view) => Some(view),
                _ => None,
            }
        })?;
        let Waited::Ready(view) = opened else {
            return Err(self.unmet(
                index,
                &opened.discard(),
                "the quit dialog to open",
                step.timeout_ms,
                || format!("the screen shows:\n{}", self.session.screen().text()),
            ));
        };
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
        if self.session.exit().is_some() {
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
