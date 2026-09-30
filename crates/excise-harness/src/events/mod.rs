//! Reader for the release binary's test event channel.
//!
//! Setting `EXCISE_TEST_EVENTS` to the path of a file that does not exist makes `excise` create
//! the file and append one JSON object per line: protocol version 1, documented in
//! `docs/development.md` under "Test Event Channel". [`EventLog`] follows that file, and
//! [`Event`] is one parsed line.
//!
//! Parsing is strict. A line must be a JSON object with `v` equal to 1, a known `kind`, a numeric
//! `t_us`, and exactly the fields its kind carries. Anything else is an [`EventError`]: a harness
//! that reads a channel it does not understand would otherwise report nonsense.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    time::Instant,
};

use serde_json::{Map, Value};
use thiserror::Error;

use crate::scenario::{Comparison, EventField, EventKind};

/// The only protocol version this reader accepts.
pub const PROTOCOL_VERSION: u64 = 1;

/// One line of the event channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Microseconds since the channel opened, as `excise` measured them.
    pub t_us: u64,
    /// When the harness read the line. It bounds when `excise` wrote it from above.
    pub observed: Instant,
    /// What the line reports.
    pub payload: Payload,
}

/// What an event reports, with the fields of its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    /// The first line: the package version and the process id.
    Hello {
        /// The `excise` package version.
        version: String,
        /// The process id of `excise`.
        pid: u64,
    },
    /// A render drew a frame.
    Frame {
        /// The drawn-frame counter, from 1.
        seq: u64,
        /// The terminal input events the owner loop had consumed when the frame was drawn.
        inputs: u64,
    },
    /// The initial scan finished.
    ScanComplete {
        /// The number of scanned entries.
        entries: u64,
    },
    /// The quit dialog was built.
    QuitPrompt,
    /// A deletion worker reported.
    DeletionFinished {
        /// Entries the deletion removed.
        removed: u64,
        /// Entries the deletion failed to remove.
        failed: u64,
    },
    /// The interactive run is about to return its exit code.
    Exit {
        /// The exit code.
        code: u64,
    },
}

impl Payload {
    /// The `kind` name on the wire.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Frame { .. } => "frame",
            Self::ScanComplete { .. } => "scan_complete",
            Self::QuitPrompt => "quit_prompt",
            Self::DeletionFinished { .. } => "deletion_finished",
            Self::Exit { .. } => "exit",
        }
    }

    /// The scenario-vocabulary kind, or `None` for the `hello` handshake.
    #[must_use]
    pub const fn event_kind(&self) -> Option<EventKind> {
        match self {
            Self::Hello { .. } => None,
            Self::Frame { .. } => Some(EventKind::Frame),
            Self::ScanComplete { .. } => Some(EventKind::ScanComplete),
            Self::QuitPrompt => Some(EventKind::QuitPrompt),
            Self::DeletionFinished { .. } => Some(EventKind::DeletionFinished),
            Self::Exit { .. } => Some(EventKind::Exit),
        }
    }
}

impl Event {
    /// The numeric field `field`, if this kind of event carries it.
    #[must_use]
    pub const fn field(&self, field: EventField) -> Option<u64> {
        match (&self.payload, field) {
            (_, EventField::TimeMicros) => Some(self.t_us),
            (Payload::Frame { seq, .. }, EventField::Seq) => Some(*seq),
            (Payload::Frame { inputs, .. }, EventField::Inputs) => Some(*inputs),
            (Payload::ScanComplete { entries }, EventField::Entries) => Some(*entries),
            (Payload::DeletionFinished { removed, .. }, EventField::Removed) => Some(*removed),
            (Payload::DeletionFinished { failed, .. }, EventField::Failed) => Some(*failed),
            (Payload::Exit { code }, EventField::Code) => Some(*code),
            _ => None,
        }
    }

    /// Whether this is an event of `kind` and every test in `tests` holds for its fields.
    #[must_use]
    pub fn matches(&self, kind: EventKind, tests: &BTreeMap<EventField, Comparison>) -> bool {
        self.payload.event_kind() == Some(kind)
            && tests.iter().all(|(field, test)| {
                self.field(*field).is_some_and(|value| match *test {
                    Comparison::Eq(expected) => value == expected,
                    Comparison::Min(minimum) => value >= minimum,
                    Comparison::Max(maximum) => value <= maximum,
                })
            })
    }
}

/// The event channel could not be read or is not protocol version 1.
#[derive(Debug, Error)]
pub enum EventError {
    /// The event file could not be read.
    #[error("cannot read the event file `{}`: {source}", path.display())]
    Read {
        /// The event file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// A line is not UTF-8.
    #[error("event line {line} is not valid UTF-8")]
    NotUtf8 {
        /// The one-based line number.
        line: usize,
    },
    /// A line is not a JSON object.
    #[error("event line {line} is not a JSON object: {detail}")]
    NotAnObject {
        /// The one-based line number.
        line: usize,
        /// What was wrong.
        detail: String,
    },
    /// A line carries a protocol version other than 1.
    #[error(
        "event line {line} carries protocol version {found}, but this harness reads only \
         version {PROTOCOL_VERSION}"
    )]
    UnsupportedVersion {
        /// The one-based line number.
        line: usize,
        /// The `v` value as written.
        found: String,
    },
    /// A required field is absent.
    #[error("event line {line} has no `{field}` field")]
    MissingField {
        /// The one-based line number.
        line: usize,
        /// The absent field.
        field: &'static str,
    },
    /// A field has the wrong type.
    #[error("event line {line}: `{field}` must be {expected}")]
    WrongType {
        /// The one-based line number.
        line: usize,
        /// The field.
        field: &'static str,
        /// What it must be.
        expected: &'static str,
    },
    /// The `kind` is not one protocol version 1 defines.
    #[error("event line {line} has the unknown kind `{kind}`")]
    UnknownKind {
        /// The one-based line number.
        line: usize,
        /// The kind as written.
        kind: String,
    },
    /// A field that the kind does not carry.
    #[error("event line {line} (`{kind}`) has the unknown field `{field}`")]
    UnknownField {
        /// The one-based line number.
        line: usize,
        /// The kind of the line.
        kind: String,
        /// The unexpected field.
        field: String,
    },
}

/// Parses one complete line of the event channel.
///
/// `line` is the one-based line number, used in errors; `observed` is when the line was read.
///
/// # Errors
///
/// Returns the [`EventError`] that describes why the line is not a protocol-v1 event.
pub fn parse_line(line: usize, bytes: &[u8], observed: Instant) -> Result<Event, EventError> {
    let text = std::str::from_utf8(bytes).map_err(|_| EventError::NotUtf8 { line })?;
    let value: Value = serde_json::from_str(text).map_err(|error| EventError::NotAnObject {
        line,
        detail: error.to_string(),
    })?;
    let Value::Object(mut fields) = value else {
        return Err(EventError::NotAnObject {
            line,
            detail: "it is another kind of JSON value".to_owned(),
        });
    };

    let version = fields
        .remove("v")
        .ok_or(EventError::MissingField { line, field: "v" })?;
    if version.as_u64() != Some(PROTOCOL_VERSION) {
        return Err(EventError::UnsupportedVersion {
            line,
            found: version.to_string(),
        });
    }
    let kind = match fields.remove("kind") {
        Some(Value::String(kind)) => kind,
        Some(_) => {
            return Err(EventError::WrongType {
                line,
                field: "kind",
                expected: "a string",
            });
        }
        None => {
            return Err(EventError::MissingField {
                line,
                field: "kind",
            });
        }
    };
    let t_us = take_number(&mut fields, line, "t_us")?;
    let payload = match kind.as_str() {
        "hello" => Payload::Hello {
            version: take_string(&mut fields, line, "version")?,
            pid: take_number(&mut fields, line, "pid")?,
        },
        "frame" => Payload::Frame {
            seq: take_number(&mut fields, line, "seq")?,
            inputs: take_number(&mut fields, line, "inputs")?,
        },
        "scan_complete" => Payload::ScanComplete {
            entries: take_number(&mut fields, line, "entries")?,
        },
        "quit_prompt" => Payload::QuitPrompt,
        "deletion_finished" => Payload::DeletionFinished {
            removed: take_number(&mut fields, line, "removed")?,
            failed: take_number(&mut fields, line, "failed")?,
        },
        "exit" => Payload::Exit {
            code: take_number(&mut fields, line, "code")?,
        },
        _ => return Err(EventError::UnknownKind { line, kind }),
    };
    if let Some(field) = fields.keys().next() {
        return Err(EventError::UnknownField {
            line,
            kind,
            field: field.clone(),
        });
    }
    Ok(Event {
        t_us,
        observed,
        payload,
    })
}

fn take_number(
    fields: &mut Map<String, Value>,
    line: usize,
    field: &'static str,
) -> Result<u64, EventError> {
    match fields.remove(field) {
        Some(value) => value.as_u64().ok_or(EventError::WrongType {
            line,
            field,
            expected: "an unsigned integer",
        }),
        None => Err(EventError::MissingField { line, field }),
    }
}

fn take_string(
    fields: &mut Map<String, Value>,
    line: usize,
    field: &'static str,
) -> Result<String, EventError> {
    match fields.remove(field) {
        Some(Value::String(text)) => Ok(text),
        Some(_) => Err(EventError::WrongType {
            line,
            field,
            expected: "a string",
        }),
        None => Err(EventError::MissingField { line, field }),
    }
}

/// Follows the file `excise` appends events to.
///
/// [`EventLog::poll`] reads what was appended since the last call and parses every complete line.
/// A trailing partial line waits for its newline, so a reader that catches `excise` between the
/// two halves of a write never sees half an event. The log is cumulative: every event stays
/// available through [`EventLog::events`], in the order `excise` wrote them.
#[derive(Debug)]
pub struct EventLog {
    path: PathBuf,
    /// Opened once `excise` has created the file.
    file: Option<File>,
    /// The bytes after the last newline.
    partial: Vec<u8>,
    lines_read: usize,
    events: Vec<Event>,
}

impl EventLog {
    /// A log that follows `path`, which does not have to exist yet.
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            partial: Vec::new(),
            lines_read: 0,
            events: Vec::new(),
        }
    }

    /// The followed file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every event read so far.
    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The bytes read after the last newline: an event `excise` had not finished writing.
    #[must_use]
    pub fn incomplete_line(&self) -> &[u8] {
        &self.partial
    }

    /// Reads what `excise` appended since the last call.
    ///
    /// Every event parsed by this call is stamped with `observed`. Returns how many there were.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or a complete line is not a protocol-v1 event.
    pub fn poll(&mut self, observed: Instant) -> Result<usize, EventError> {
        if self.file.is_none() {
            match File::open(&self.path) {
                Ok(file) => self.file = Some(file),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
                Err(source) => {
                    return Err(EventError::Read {
                        path: self.path.clone(),
                        source,
                    });
                }
            }
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(0);
        };
        let mut chunk = [0_u8; 4096];
        loop {
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => self.partial.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(source) => {
                    return Err(EventError::Read {
                        path: self.path.clone(),
                        source,
                    });
                }
            }
        }
        let mut parsed = 0;
        while let Some(newline) = self.partial.iter().position(|byte| *byte == b'\n') {
            self.lines_read += 1;
            let event = parse_line(self.lines_read, &self.partial[..newline], observed)?;
            self.partial.drain(..=newline);
            self.events.push(event);
            parsed += 1;
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use super::*;

    fn parse(text: &str) -> Result<Event, EventError> {
        parse_line(1, text.as_bytes(), Instant::now())
    }

    #[test]
    fn every_kind_of_the_protocol_parses_with_its_fields() {
        let hello = parse(r#"{"v":1,"kind":"hello","version":"1.3.0","pid":4242,"t_us":6}"#)
            .expect("hello");
        assert_eq!(
            hello.payload,
            Payload::Hello {
                version: "1.3.0".to_owned(),
                pid: 4242
            }
        );
        assert_eq!(hello.t_us, 6);

        let frame =
            parse(r#"{"v":1,"kind":"frame","seq":3,"inputs":2,"t_us":900}"#).expect("frame");
        assert_eq!(frame.payload, Payload::Frame { seq: 3, inputs: 2 });

        assert_eq!(
            parse(r#"{"v":1,"kind":"scan_complete","entries":14,"t_us":1}"#)
                .expect("scan_complete")
                .payload,
            Payload::ScanComplete { entries: 14 }
        );
        assert_eq!(
            parse(r#"{"v":1,"kind":"quit_prompt","t_us":1}"#)
                .expect("quit_prompt")
                .payload,
            Payload::QuitPrompt
        );
        assert_eq!(
            parse(r#"{"v":1,"kind":"deletion_finished","removed":56,"failed":0,"t_us":1}"#)
                .expect("deletion_finished")
                .payload,
            Payload::DeletionFinished {
                removed: 56,
                failed: 0
            }
        );
        assert_eq!(
            parse(r#"{"v":1,"kind":"exit","code":130,"t_us":1}"#)
                .expect("exit")
                .payload,
            Payload::Exit { code: 130 }
        );
    }

    #[test]
    fn an_unknown_protocol_version_is_rejected() {
        for line in [
            r#"{"v":2,"kind":"quit_prompt","t_us":1}"#,
            r#"{"v":"1","kind":"quit_prompt","t_us":1}"#,
            r#"{"v":0,"kind":"quit_prompt","t_us":1}"#,
        ] {
            let error = parse(line).expect_err("a foreign version must be rejected");
            assert!(
                matches!(error, EventError::UnsupportedVersion { .. }),
                "{line}: {error}"
            );
        }
        assert!(matches!(
            parse(r#"{"kind":"quit_prompt","t_us":1}"#),
            Err(EventError::MissingField { field: "v", .. })
        ));
    }

    #[test]
    fn malformed_lines_are_rejected_with_their_line_number() {
        let observed = Instant::now();
        let cases: [(&[u8], &str); 9] = [
            (b"not json", "NotAnObject"),
            (b"[1]", "NotAnObject"),
            (b"", "NotAnObject"),
            (b"\xff\xfe", "NotUtf8"),
            (br#"{"v":1,"kind":"nope","t_us":1}"#, "UnknownKind"),
            (
                br#"{"v":1,"kind":"frame","seq":1,"t_us":1}"#,
                "MissingField",
            ),
            (
                br#"{"v":1,"kind":"frame","seq":"1","inputs":0,"t_us":1}"#,
                "WrongType",
            ),
            (
                br#"{"v":1,"kind":"frame","seq":-1,"inputs":0,"t_us":1}"#,
                "WrongType",
            ),
            (
                br#"{"v":1,"kind":"quit_prompt","t_us":1,"path":"/x"}"#,
                "UnknownField",
            ),
        ];

        for (bytes, expected) in cases {
            let error = parse_line(7, bytes, observed).expect_err("a malformed line");
            assert!(
                format!("{error:?}").starts_with(expected),
                "{}: {error:?}",
                String::from_utf8_lossy(bytes)
            );
            assert!(error.to_string().contains('7') || matches!(error, EventError::Read { .. }));
        }
    }

    #[test]
    fn a_missing_time_stamp_is_rejected() {
        assert!(matches!(
            parse(r#"{"v":1,"kind":"quit_prompt"}"#),
            Err(EventError::MissingField { field: "t_us", .. })
        ));
    }

    #[test]
    fn field_tests_compare_numbers_and_ignore_the_wrong_kind() {
        let frame =
            parse(r#"{"v":1,"kind":"frame","seq":3,"inputs":5,"t_us":900}"#).expect("frame");
        let tests = |field, test| BTreeMap::from([(field, test)]);

        assert!(frame.matches(EventKind::Frame, &BTreeMap::new()));
        assert!(frame.matches(
            EventKind::Frame,
            &tests(EventField::Inputs, Comparison::Eq(5))
        ));
        assert!(frame.matches(
            EventKind::Frame,
            &tests(EventField::Inputs, Comparison::Min(5))
        ));
        assert!(!frame.matches(
            EventKind::Frame,
            &tests(EventField::Inputs, Comparison::Min(6))
        ));
        assert!(frame.matches(
            EventKind::Frame,
            &tests(EventField::Seq, Comparison::Max(3))
        ));
        assert!(!frame.matches(
            EventKind::Frame,
            &tests(EventField::Seq, Comparison::Max(2))
        ));
        assert!(frame.matches(
            EventKind::Frame,
            &tests(EventField::TimeMicros, Comparison::Min(900))
        ));
        assert!(!frame.matches(EventKind::Exit, &BTreeMap::new()));
        assert!(
            !frame.matches(
                EventKind::Frame,
                &tests(EventField::Code, Comparison::Eq(0))
            ),
            "a field the event does not carry never satisfies a test"
        );
    }

    #[test]
    fn the_hello_handshake_is_not_a_scenario_event() {
        let hello =
            parse(r#"{"v":1,"kind":"hello","version":"1","pid":1,"t_us":0}"#).expect("hello");

        assert_eq!(hello.payload.event_kind(), None);
        assert_eq!(hello.payload.kind_name(), "hello");
    }

    fn append(path: &Path, text: &str) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("the event file");
        file.write_all(text.as_bytes()).expect("append");
    }

    #[test]
    fn the_log_waits_for_a_file_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("events.jsonl");
        let mut log = EventLog::new(path.clone());

        assert_eq!(log.poll(Instant::now()).expect("poll"), 0);
        append(&path, "{\"v\":1,\"kind\":\"quit_prompt\",\"t_us\":1}\n");

        assert_eq!(log.poll(Instant::now()).expect("poll"), 1);
        assert_eq!(log.events().len(), 1);
    }

    #[test]
    fn the_log_never_parses_half_a_line() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("events.jsonl");
        let mut log = EventLog::new(path.clone());
        append(&path, "{\"v\":1,\"kind\":\"frame\",\"seq\":1,");

        assert_eq!(log.poll(Instant::now()).expect("poll"), 0);
        assert!(!log.incomplete_line().is_empty());

        append(
            &path,
            "\"inputs\":0,\"t_us\":5}\n{\"v\":1,\"kind\":\"quit_prompt\",\"t_us\":9}\n",
        );

        assert_eq!(log.poll(Instant::now()).expect("poll"), 2);
        assert!(log.incomplete_line().is_empty());
        assert_eq!(
            log.events()
                .iter()
                .map(|event| event.payload.kind_name())
                .collect::<Vec<_>>(),
            ["frame", "quit_prompt"]
        );
    }

    #[test]
    fn a_bad_line_reports_its_position_in_the_file() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("events.jsonl");
        let mut log = EventLog::new(path.clone());
        append(
            &path,
            "{\"v\":1,\"kind\":\"quit_prompt\",\"t_us\":1}\n{\"v\":9,\"kind\":\"quit_prompt\",\"t_us\":2}\n",
        );

        let error = log
            .poll(Instant::now())
            .expect_err("the second line has a foreign version");

        assert!(
            matches!(error, EventError::UnsupportedVersion { line: 2, .. }),
            "{error}"
        );
    }
}
