use ::std::sync::mpsc::{Receiver, SendError, SyncSender, TrySendError};
use ::std::sync::{Arc, Condvar, Mutex};
use ::std::thread::park_timeout;
use ::std::time;

use crate::messages::Instruction;

pub enum Event {
    PathError,
    FileDeleted,
    AppExit,
}

#[derive(Default)]
pub(crate) struct EventTracker {
    pending: Mutex<usize>,
    idle: Condvar,
}

impl EventTracker {
    pub(crate) fn begin(&self) {
        *self.pending.lock().expect("failed to lock event tracker") += 1;
    }

    pub(crate) fn complete(&self) {
        let mut pending = self.pending.lock().expect("failed to lock event tracker");
        *pending = pending
            .checked_sub(1)
            .expect("completed an event that was not pending");
        if *pending == 0 {
            self.idle.notify_all();
        }
    }

    pub(crate) fn wait_until_idle(&self) {
        let pending = self.pending.lock().expect("failed to lock event tracker");
        drop(
            self.idle
                .wait_while(pending, |pending| *pending != 0)
                .expect("failed to wait for event completion"),
        );
    }
}

#[derive(Clone)]
pub(crate) struct EventSender {
    sender: SyncSender<Event>,
    tracker: Arc<EventTracker>,
}

impl EventSender {
    pub(crate) const fn new(sender: SyncSender<Event>, tracker: Arc<EventTracker>) -> Self {
        Self { sender, tracker }
    }

    pub(crate) fn send(&self, event: Event) -> Result<(), SendError<Event>> {
        self.tracker.begin();
        self.sender
            .send(event)
            .inspect_err(|_| self.tracker.complete())
    }

    pub(crate) fn try_send(&self, event: Event) -> Result<(), TrySendError<Event>> {
        self.tracker.begin();
        self.sender
            .try_send(event)
            .inspect_err(|_| self.tracker.complete())
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn handle_events(
    event_receiver: Receiver<Event>,
    instruction_sender: SyncSender<Instruction>,
    tracker: Arc<EventTracker>,
) {
    loop {
        let event = event_receiver
            .recv()
            .expect("failed to receive event on channel");
        let app_exit = matches!(event, Event::AppExit);
        match event {
            Event::PathError => {
                let _ = instruction_sender.send(Instruction::SetPathToRed);
                let _ = instruction_sender.send(Instruction::Render);
                park_timeout(time::Duration::from_millis(250));
                let _ = instruction_sender.send(Instruction::ResetCurrentPathColor);
                let _ = instruction_sender.send(Instruction::Render);
            }
            Event::FileDeleted => {
                let _ = instruction_sender.send(Instruction::FlashSpaceFreed);
                let _ = instruction_sender.send(Instruction::Render);
                park_timeout(time::Duration::from_millis(250));
                let _ = instruction_sender.send(Instruction::UnflashSpaceFreed);
                let _ = instruction_sender.send(Instruction::Render);
            }
            Event::AppExit => {}
        }
        tracker.complete();
        if app_exit {
            break;
        }
    }
}
