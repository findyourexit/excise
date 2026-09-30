//! Session recording in the asciicast v2 format.
//!
//! The first line is a JSON header; every later line is `[seconds, kind, data]`:
//!
//! * `"o"`: bytes `excise` wrote to the terminal, as text;
//! * `"i"`: bytes the harness wrote to the terminal, as text;
//! * `"r"`: a resize, `"<columns>x<rows>"`. Version 2 of the format defines only `o` and `i`;
//!   `r` is the resize code of version 3, and players that do not know it skip the line.
//!
//! `seconds` counts from the moment the recording started. Terminal output is a byte stream and a
//! read can end in the middle of a multi-byte character, so output is re-chunked at character
//! boundaries: every line is valid UTF-8, and a byte sequence that is not valid UTF-8 is written as
//! U+FFFD.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
    time::Instant,
};

use serde_json::{Value, json};

/// The first line of a recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastHeader {
    /// The terminal width in columns.
    pub width: u16,
    /// The terminal height in rows.
    pub height: u16,
    /// When the recording started, in seconds since the Unix epoch.
    pub timestamp: u64,
    /// The environment variables worth recording with the session.
    pub env: BTreeMap<String, String>,
    /// A human-readable title.
    pub title: Option<String>,
}

/// Writes a recording to any [`Write`].
#[derive(Debug)]
pub struct CastWriter<W: Write> {
    out: W,
    started: Instant,
    /// The tail of the output that ended inside a multi-byte character.
    partial: Vec<u8>,
}

impl CastWriter<BufWriter<File>> {
    /// Creates the recording file and writes the header.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be created or written.
    pub fn create(path: &Path, header: &CastHeader, started: Instant) -> io::Result<Self> {
        Self::new(BufWriter::new(File::create(path)?), header, started)
    }
}

impl<W: Write> CastWriter<W> {
    /// Writes the header to `out`. Times in later lines count from `started`.
    ///
    /// # Errors
    ///
    /// Returns an error if the header cannot be written.
    pub fn new(mut out: W, header: &CastHeader, started: Instant) -> io::Result<Self> {
        let mut document = json!({
            "version": 2,
            "width": header.width,
            "height": header.height,
            "timestamp": header.timestamp,
            "env": header.env,
        });
        if let (Some(title), Some(object)) = (&header.title, document.as_object_mut()) {
            object.insert("title".to_owned(), Value::String(title.clone()));
        }
        writeln!(out, "{document}")?;
        Ok(Self {
            out,
            started,
            partial: Vec::new(),
        })
    }

    /// Records terminal output that was read at `at`.
    ///
    /// # Errors
    ///
    /// Returns an error if the recording cannot be written.
    pub fn output(&mut self, at: Instant, bytes: &[u8]) -> io::Result<()> {
        self.partial.extend_from_slice(bytes);
        let mut text = String::new();
        let mut rest = std::mem::take(&mut self.partial);
        loop {
            match std::str::from_utf8(&rest) {
                Ok(valid) => {
                    text.push_str(valid);
                    rest.clear();
                    break;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    text.push_str(&String::from_utf8_lossy(&rest[..valid_up_to]));
                    if let Some(invalid) = error.error_len() {
                        text.push('\u{fffd}');
                        rest.drain(..valid_up_to + invalid);
                    } else {
                        // The bytes end inside a character; wait for the rest of it.
                        rest.drain(..valid_up_to);
                        break;
                    }
                }
            }
        }
        self.partial = rest;
        if text.is_empty() {
            Ok(())
        } else {
            self.event(at, "o", &text)
        }
    }

    /// Records input the harness wrote at `at`.
    ///
    /// # Errors
    ///
    /// Returns an error if the recording cannot be written.
    pub fn input(&mut self, at: Instant, bytes: &[u8]) -> io::Result<()> {
        self.event(at, "i", &String::from_utf8_lossy(bytes))
    }

    /// Records a resize to `cols` by `rows` at `at`.
    ///
    /// # Errors
    ///
    /// Returns an error if the recording cannot be written.
    pub fn resize(&mut self, at: Instant, cols: u16, rows: u16) -> io::Result<()> {
        self.event(at, "r", &format!("{cols}x{rows}"))
    }

    /// Writes any output still held back and flushes the recording.
    ///
    /// # Errors
    ///
    /// Returns an error if the recording cannot be written.
    pub fn finish(&mut self, at: Instant) -> io::Result<()> {
        if !self.partial.is_empty() {
            let text = String::from_utf8_lossy(&std::mem::take(&mut self.partial)).into_owned();
            self.event(at, "o", &text)?;
        }
        self.out.flush()
    }

    fn event(&mut self, at: Instant, kind: &str, data: &str) -> io::Result<()> {
        let seconds = at.saturating_duration_since(self.started).as_secs_f64();
        writeln!(
            self.out,
            "[{seconds:.6}, {}, {}]",
            Value::from(kind),
            Value::from(data)
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn header() -> CastHeader {
        CastHeader {
            width: 120,
            height: 40,
            timestamp: 1_700_000_000,
            env: BTreeMap::from([("TERM".to_owned(), "xterm-256color".to_owned())]),
            title: Some("scenario".to_owned()),
        }
    }

    fn lines(writer: &CastWriter<Vec<u8>>) -> Vec<Value> {
        String::from_utf8(writer.out.clone())
            .expect("a cast is UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line is JSON"))
            .collect()
    }

    #[test]
    fn the_header_is_the_first_line() {
        let start = Instant::now();
        let writer = CastWriter::new(Vec::new(), &header(), start).expect("a writer");

        let lines = lines(&writer);

        assert_eq!(
            lines[0],
            json!({
                "version": 2,
                "width": 120,
                "height": 40,
                "timestamp": 1_700_000_000_u64,
                "env": {"TERM": "xterm-256color"},
                "title": "scenario",
            })
        );
    }

    #[test]
    fn output_input_and_resize_are_timestamped_events() {
        let start = Instant::now();
        let mut writer = CastWriter::new(Vec::new(), &header(), start).expect("a writer");

        writer
            .output(start + Duration::from_millis(250), b"\x1b[?1049hhello")
            .expect("output");
        writer
            .input(start + Duration::from_millis(1500), b"\x7f")
            .expect("input");
        writer
            .resize(start + Duration::from_secs(2), 60, 30)
            .expect("resize");

        let lines = lines(&writer);
        assert_eq!(lines[1], json!([0.25, "o", "\u{1b}[?1049hhello"]));
        assert_eq!(lines[2], json!([1.5, "i", "\u{7f}"]));
        assert_eq!(lines[3], json!([2.0, "r", "60x30"]));
    }

    #[test]
    fn a_character_split_across_reads_is_written_whole() {
        let start = Instant::now();
        let mut writer = CastWriter::new(Vec::new(), &header(), start).expect("a writer");
        let bytes = "▟é".as_bytes();

        // The horizontal bar block is three bytes and the accented letter two.
        writer.output(start, &bytes[..2]).expect("first half");
        writer.output(start, &bytes[2..4]).expect("second half");
        writer.output(start, &bytes[4..]).expect("last byte");

        let text: String = lines(&writer)[1..]
            .iter()
            .map(|line| line[2].as_str().expect("event data").to_owned())
            .collect();
        assert_eq!(text, "▟é");
    }

    #[test]
    fn invalid_bytes_become_replacement_characters() {
        let start = Instant::now();
        let mut writer = CastWriter::new(Vec::new(), &header(), start).expect("a writer");

        writer.output(start, b"a\xffb").expect("output");

        assert_eq!(lines(&writer)[1][2], json!("a\u{fffd}b"));
    }

    #[test]
    fn finishing_flushes_a_truncated_character() {
        let start = Instant::now();
        let mut writer = CastWriter::new(Vec::new(), &header(), start).expect("a writer");
        writer.output(start, &"▟".as_bytes()[..2]).expect("output");
        assert_eq!(
            lines(&writer).len(),
            1,
            "nothing is written for half a character"
        );

        writer.finish(start).expect("finish");

        assert_eq!(lines(&writer)[1][2], json!("\u{fffd}"));
    }

    #[test]
    fn times_never_go_backwards_past_the_start() {
        let start = Instant::now();
        let mut writer = CastWriter::new(Vec::new(), &header(), start + Duration::from_secs(5))
            .expect("a writer");

        writer.input(start, b"y").expect("input");

        assert_eq!(lines(&writer)[1][0], json!(0.0));
    }
}
