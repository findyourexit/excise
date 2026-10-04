//! The channel between a command and its session's supervisor: request and reply files.
//!
//! A command writes its request into the session's `req/` directory, under a name that sorts by
//! the time it was made, and waits for the reply file of the same name in `rep/`. The supervisor
//! takes the oldest request, serves it, and writes the reply. Both files are written under a
//! temporary name and renamed into place, so a reader sees all of a file or none of it. The
//! command removes the reply it has read; a supervisor that is about to end waits for that before
//! it removes the session directory, which is how the last reply of a session (the one `close`
//! gets) survives the end of the session.
//!
//! The supervisor takes a request by renaming it, and a command that gives up on a request
//! removes it, so exactly one of the two gets it: a request a command withdrew is never served,
//! and one that was served is never withdrawn. A command that reports a timeout therefore knows
//! whether its request may still run.
//!
//! There is no socket and no port: the channel is files in a directory only its owner can enter,
//! which works under any sandbox that lets a command write into `target/`, and is reachable from
//! nowhere but this machine's file system.

use std::{
    fs, io,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use super::layout::{SessionDir, write_atomic};
use crate::{report::HarnessTui, scenario::EntryKind};

/// What a command asks of a session's supervisor. `open` and `list` are not requests: `open`
/// starts a supervisor, and `list` reads the session directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub(super) enum Request {
    /// `keys`.
    Keys {
        /// The keys, as written on the command line.
        keys: Vec<String>,
        /// The bound on the wait for a frame that counts them.
        timeout_ms: u64,
    },
    /// `delete`.
    Delete {
        /// The fixture-relative path of the entry.
        name: String,
        /// Whether it is a file or a folder.
        kind: EntryKind,
        /// The bound of the whole command.
        timeout_ms: u64,
    },
    /// `screen`.
    Screen,
    /// `events`.
    Events {
        /// The index of the first record.
        since: Option<u64>,
    },
    /// `close`.
    Close,
}

/// A request the supervisor took, and the name its reply goes under.
#[derive(Debug)]
pub(super) struct Taken {
    /// The name shared by the request and its reply.
    pub(super) name: String,
    /// The request, or why it could not be read.
    pub(super) request: Result<Request, String>,
}

impl SessionDir {
    /// Leaves `request` for the supervisor and returns the name its reply will have.
    pub(super) fn submit(&self, request: &Request) -> io::Result<String> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let name = format!(
            "{nanos:022}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let bytes = serde_json::to_vec(request).map_err(io::Error::other)?;
        write_atomic(&self.requests().join(format!("{name}.json")), &bytes)?;
        Ok(name)
    }

    /// Takes the oldest request, claiming its file. `None` when there is none.
    ///
    /// The file is claimed by renaming it, which a command that withdraws the request beats or
    /// loses atomically: a request that is gone by then is skipped.
    pub(super) fn take_request(&self) -> io::Result<Option<Taken>> {
        loop {
            let mut names = Vec::new();
            for entry in fs::read_dir(self.requests())? {
                let name = entry?.file_name();
                if let Some(name) = name.to_str().and_then(|name| name.strip_suffix(".json")) {
                    names.push(name.to_owned());
                }
            }
            let Some(name) = names.into_iter().min() else {
                return Ok(None);
            };
            let path = self.requests().join(format!("{name}.json"));
            let claimed = self.requests().join(format!("{name}.taken"));
            match fs::rename(&path, &claimed) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
            let bytes = fs::read(&claimed)?;
            fs::remove_file(&claimed)?;
            let request = serde_json::from_slice(&bytes)
                .map_err(|error| format!("a request that is not valid: {error}"));
            return Ok(Some(Taken { name, request }));
        }
    }

    /// Withdraws the request called `name`, if the supervisor has not taken it. `true` when it
    /// was withdrawn: it will never be served. `false` when it was taken, or never there.
    pub(super) fn withdraw(&self, name: &str) -> bool {
        fs::remove_file(self.requests().join(format!("{name}.json"))).is_ok()
    }

    /// Writes the reply to the request called `name`.
    pub(super) fn reply(&self, name: &str, document: &HarnessTui) -> io::Result<()> {
        let text = serde_json::to_vec_pretty(document).map_err(io::Error::other)?;
        write_atomic(&self.replies().join(format!("{name}.json")), &text)
    }

    /// Whether the reply to `name` is there to be read.
    pub(super) fn reply_waiting(&self, name: &str) -> bool {
        self.replies().join(format!("{name}.json")).exists()
    }

    /// Reads and removes the reply to `name`, if it is there.
    pub(super) fn take_reply(&self, name: &str) -> io::Result<Option<HarnessTui>> {
        let path = self.replies().join(format!("{name}.json"));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let _ = fs::remove_file(&path);
        serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the supervisor's reply is not a harness-tui document: {error}"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::tui::{TuiCommand, TuiError, TuiErrorKind};

    fn session() -> (tempfile::TempDir, SessionDir) {
        let state = tempfile::tempdir().expect("a state directory");
        let session = SessionDir::create(state.path()).expect("a session directory");
        (state, session)
    }

    #[test]
    fn requests_are_taken_oldest_first_and_only_once() {
        let (_state, session) = session();
        let first = session.submit(&Request::Screen).expect("a request");
        let second = session
            .submit(&Request::Events { since: Some(3) })
            .expect("a request");
        let third = session.submit(&Request::Close).expect("a request");

        let taken = [
            session.take_request().expect("a request").expect("one"),
            session.take_request().expect("a request").expect("one"),
            session.take_request().expect("a request").expect("one"),
        ];

        assert_eq!(
            taken
                .iter()
                .map(|taken| taken.name.as_str())
                .collect::<Vec<_>>(),
            [first.as_str(), second.as_str(), third.as_str()]
        );
        assert_eq!(taken[0].request, Ok(Request::Screen));
        assert_eq!(taken[1].request, Ok(Request::Events { since: Some(3) }));
        assert_eq!(taken[2].request, Ok(Request::Close));
        assert!(session.take_request().expect("no request").is_none());
        assert_eq!(fs::read_dir(session.requests()).expect("req").count(), 0);
    }

    #[test]
    fn a_request_that_cannot_be_read_is_taken_and_reported_not_retried() {
        let (_state, session) = session();
        fs::write(session.requests().join("00-garbage.json"), b"{not json").expect("garbage");

        let taken = session.take_request().expect("a request").expect("one");

        assert_eq!(taken.name, "00-garbage");
        assert!(taken.request.expect_err("garbage").contains("not valid"));
        assert!(session.take_request().expect("no request").is_none());
    }

    #[test]
    fn a_request_is_served_or_withdrawn_never_both() {
        let (_state, session) = session();
        let withdrawn = session.submit(&Request::Close).expect("a request");
        let served = session.submit(&Request::Screen).expect("a request");

        assert!(
            session.withdraw(&withdrawn),
            "a queued request is withdrawn"
        );
        assert!(!session.withdraw(&withdrawn), "and only once");
        let taken = session.take_request().expect("a request").expect("one");

        assert_eq!(taken.name, served);
        assert_eq!(taken.request, Ok(Request::Screen));
        assert!(
            !session.withdraw(&served),
            "a request the supervisor took cannot be withdrawn"
        );
        assert!(
            session.take_request().expect("none").is_none(),
            "the withdrawn request was never served"
        );
        assert!(!session.withdraw("never-submitted"));
    }

    #[test]
    fn a_request_being_written_is_not_taken_until_it_is_whole() {
        let (_state, session) = session();
        fs::write(session.requests().join("a.json.tmp-1-0"), b"{").expect("a partial write");

        assert!(session.take_request().expect("none").is_none());
    }

    #[test]
    fn a_reply_is_read_once_and_a_missing_one_is_not_an_error() {
        let (_state, session) = session();
        let document = HarnessTui::failure(
            Some(TuiCommand::Screen),
            Some(session.id().to_string()),
            TuiError::new(TuiErrorKind::Failed, "no screen"),
        );
        assert!(session.take_reply("x").expect("nothing").is_none());

        session.reply("x", &document).expect("a reply");
        assert!(session.reply_waiting("x"));

        assert_eq!(session.take_reply("x").expect("a reply"), Some(document));
        assert!(!session.reply_waiting("x"));
        assert!(session.take_reply("x").expect("nothing").is_none());
    }

    #[test]
    fn a_reply_that_is_not_a_document_is_an_error() {
        let (_state, session) = session();
        fs::write(session.replies().join("x.json"), br#"{"ok":true}"#).expect("a reply");

        let error = session.take_reply("x").expect_err("not a document");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
