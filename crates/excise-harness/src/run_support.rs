//! Helpers the runners share: the identity of a run (host, binary digest, timestamps), the
//! `latest` pointer beside a run's output, and plain-text tables.

use std::{
    fmt::Write as _,
    fs::{self, File},
    io::{self, Read},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

/// The longest run id [`is_run_id`] accepts: sixteen characters of timestamp, a dash, and a
/// process id of at most ten digits.
const MAX_RUN_ID_LEN: usize = 16 + 1 + 10;

/// The longest a `latest` file that a run wrote can be: the longest run id and, after it, the
/// longest line ending (`\r\n`).
const MAX_POINTER_LEN: usize = MAX_RUN_ID_LEN + 2;

/// Makes `<out_root>/latest` refer to the run `run_id`: a symbolic link on Unix, and a text file
/// holding the run id elsewhere, where creating a link needs a privilege.
///
/// An existing `latest` is replaced only when it is what an earlier run made
/// ([`latest_can_be_replaced`]); anything else is left alone, and the error names it. This is the
/// one removal a runner makes in its output directory, and the read-only soak, whose output
/// directory can be inside the tree it soaks, relies on it being that narrow.
pub(crate) fn point_latest_at(out_root: &Path, run_id: &str) -> io::Result<()> {
    latest_can_be_replaced(out_root)?;
    let latest = out_root.join("latest");
    match fs::remove_file(&latest) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(run_id, &latest)
    }
    #[cfg(not(unix))]
    {
        fs::write(&latest, format!("{run_id}\n"))
    }
}

/// Checks that `<out_root>/latest` is absent or is what an earlier run made, so that
/// [`point_latest_at`] may replace it: on Unix a symbolic link, whatever it points at (even
/// nothing), because a run makes only links there; elsewhere a regular file that holds nothing but
/// a run id as the harness writes one ([`is_run_id`]). A directory, a regular file on Unix, and a
/// file with any other content, a person's note called `draft-1` included, are not, and are never
/// touched.
///
/// The read-only soak asks before it starts, so that a long run is not wasted on a `latest` that
/// cannot be replaced.
///
/// # Errors
///
/// Returns `AlreadyExists`, with a message that names `latest`, for an entry that is not a run's
/// own, and the system's error when the entry cannot be inspected.
pub(crate) fn latest_can_be_replaced(out_root: &Path) -> io::Result<()> {
    let latest = out_root.join("latest");
    let metadata = match fs::symlink_metadata(&latest) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if made_by_a_run(&latest, &metadata, cfg!(unix)) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "`{}` is not a link that a run made, so it was left alone; move it away and `latest` \
             can point at the newest run",
            safe_path_text(&latest)
        ),
    ))
}

/// Whether the entry `latest`, whose metadata is `metadata`, is what a run made. A run makes a
/// symbolic link where it can (`as_a_link`), and elsewhere a regular file that holds its id.
fn made_by_a_run(latest: &Path, metadata: &fs::Metadata, as_a_link: bool) -> bool {
    if as_a_link {
        metadata.file_type().is_symlink()
    } else {
        metadata.file_type().is_file() && holds_a_run_id(latest)
    }
}

/// Whether the file holds a run id and nothing else, with at most one line ending after it. The
/// file is read to its end, and one byte more than the longest content that can qualify is enough
/// to tell that it goes on: a file that begins with a run id is not a pointer when more follows.
fn holds_a_run_id(path: &Path) -> bool {
    let mut content = Vec::with_capacity(MAX_POINTER_LEN + 1);
    let read = File::open(path).and_then(|file| {
        file.take(MAX_POINTER_LEN as u64 + 1)
            .read_to_end(&mut content)
    });
    read.is_ok() && is_pointer(&content)
}

/// Whether `content` is what a run writes into a `latest` file: a run id, and after it nothing
/// but one line ending, which is the `\n` a run writes or the `\r\n` that an editor on Windows
/// saves in its place.
fn is_pointer(content: &[u8]) -> bool {
    let id = content
        .strip_suffix(b"\r\n")
        .or_else(|| content.strip_suffix(b"\n"))
        .unwrap_or(content);
    std::str::from_utf8(id).is_ok_and(is_run_id)
}

/// Whether `text` is a run id as every runner writes one: the start of the run as
/// [`compact_utc`] spells it, a dash, and the id of the process that ran it, as in
/// `20261006T120000Z-4242`. Only what a runner can write passes: a date and a time of day that
/// exist, and a process id that is a number as `std::process::id` gives it. The schemas of the run
/// documents allow more (`run_id` is any word of letters, digits, dots, dashes, and underscores),
/// but a word a person chose, such as `draft-1`, and a string that only looks like an id, such as
/// `99999999T999999Z-0`, are not a pointer a run made, and this is what decides whether a file
/// called `latest` may be replaced.
fn is_run_id(text: &str) -> bool {
    let Some((stamp, process)) = text.split_once('-') else {
        return false;
    };
    is_compact_utc(stamp) && is_process_id(process)
}

/// Whether `stamp` is a time as [`compact_utc`] spells it, `YYYYMMDDTHHMMSSZ`: a date and a time
/// of day that exist, no earlier than the epoch, which is what a clock set before it is spelled
/// as, and no later than the last second of year 9999, which is what a clock set after it is
/// spelled as.
fn is_compact_utc(stamp: &str) -> bool {
    let bytes = stamp.as_bytes();
    if bytes.len() != 16 || bytes[8] != b'T' || bytes[15] != b'Z' {
        return false;
    }
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        decimal(&bytes[0..4]),
        decimal(&bytes[4..6]),
        decimal(&bytes[6..8]),
        decimal(&bytes[9..11]),
        decimal(&bytes[11..13]),
        decimal(&bytes[13..15]),
    ) else {
        return false;
    };
    year >= 1970
        && (1..=12).contains(&month)
        && (1..=days_in_month(year, month)).contains(&day)
        && hour < 24
        && minute < 60
        && second < 60
}

/// The number that `digits`, at most nine of them, spell in decimal, or `None` when one of them is
/// not a digit.
fn decimal(digits: &[u8]) -> Option<u32> {
    let mut value = 0_u32;
    for &digit in digits {
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u32::from(digit - b'0');
    }
    Some(value)
}

/// How many days `month`, from 1 to 12, has in `year`.
const fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Whether `year` of the Gregorian calendar has a 29th of February.
const fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// Whether `text` is a process id as `std::process::id` gives it, written the one way a number is
/// written in decimal: no sign, no leading zero, not zero, and within the 32 bits an id has.
fn is_process_id(text: &str) -> bool {
    text.bytes().all(|byte| byte.is_ascii_digit())
        && !text.starts_with('0')
        && text.parse::<u32>().is_ok()
}

/// Lays `rows` out as a table: every column as wide as its widest cell, two spaces between
/// columns, and no trailing spaces.
pub(crate) fn render_rows<const N: usize>(rows: &[[String; N]]) -> String {
    let widths: Vec<usize> = (0..N)
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        let _ = writeln!(out, "{}", line.join("  ").trim_end());
    }
    out
}

/// The median of `values`, or `None` for none. The median of an even count is the midpoint of the
/// two middle values.
pub(crate) fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        f64::midpoint(sorted[middle - 1], sorted[middle])
    })
}

/// The largest of `values`, or `None` for none.
pub(crate) fn worst(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::max)
}

/// Milliseconds as `42 ms`, or as `1.50 s` from a second on; `-` for no value.
pub(crate) fn format_ms(value: Option<f64>) -> String {
    value.map_or_else(
        || "-".to_owned(),
        |ms| {
            if ms >= 1000.0 {
                format!("{:.2} s", ms / 1000.0)
            } else {
                format!("{ms:.0} ms")
            }
        },
    )
}

/// The marker `excise` puts before a path it had to escape (`DECEPTIVE_DISPLAY_MARKER` in its
/// `native_path` module). This crate never depends on `excise`, so the rule is repeated here.
const DECEPTIVE_MARKER: &str = "[deceptive]";

/// Whether `character` can rewrite what a terminal shows or the order it shows it in: a control
/// character (a line break, an escape, a backspace), or a bidirectional control. The rule is the
/// one `excise` shows paths by (`escape_valid_text` and `is_bidi_control` in `native_path.rs`),
/// and the one the read-only soak refuses a root by.
pub(crate) fn is_deceptive(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{206f}'
        )
}

/// `path` as text that is safe to print on a terminal, by the rule `excise` shows paths by: a
/// backslash is doubled; a line break, a tab, an escape, any other control character, and a
/// bidirectional control are written as `\n`, `\r`, `\t`, `\x1b`, and `\u{XXXX}`; a byte that is
/// not text is written as `\xNN`. A path that needed any of that but the backslash starts with
/// `[deceptive]`.
///
/// Use it for every path that is shown to a person and that the person did not type back: the
/// directories a command writes in, a run's directory, a path in an error. The root of a
/// read-only soak is shown as it is, because what the person types back must equal what was
/// shown, and a root that holds such a character is refused instead.
#[must_use]
pub fn safe_path_text(path: &Path) -> String {
    let (text, deceptive) = path
        .to_str()
        .map_or_else(|| escape_unreadable(path), escape_text);
    if deceptive {
        format!("{DECEPTIVE_MARKER} {text}")
    } else {
        text
    }
}

/// `text` with what is deceptive in it escaped, and whether anything was. A backslash is doubled,
/// so that an escape that was written is never mistaken for one that was made, but it is not
/// deceptive.
fn escape_text(text: &str) -> (String, bool) {
    let mut escaped = String::with_capacity(text.len());
    let mut deceptive = false;
    for character in text.chars() {
        match character {
            '\\' => {
                escaped.push_str("\\\\");
                continue;
            }
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{1b}' => escaped.push_str("\\x1b"),
            _ if is_deceptive(character) => {
                let _ = write!(escaped, "\\u{{{:04x}}}", u32::from(character));
            }
            _ => {
                escaped.push(character);
                continue;
            }
        }
        deceptive = true;
    }
    (escaped, deceptive)
}

/// A path that is not text, written by its bytes: what is printable ASCII stays, and every other
/// byte is `\xNN`. Always deceptive.
#[cfg(unix)]
fn escape_unreadable(path: &Path) -> (String, bool) {
    use std::os::unix::ffi::OsStrExt as _;

    let mut text = String::new();
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b'\\' => text.push_str("\\\\"),
            byte if byte.is_ascii_graphic() || *byte == b' ' => text.push(char::from(*byte)),
            byte => {
                let _ = write!(text, "\\x{byte:02x}");
            }
        }
    }
    (text, true)
}

/// A path that is not Unicode (an unpaired surrogate): what cannot be shown is replaced, and the
/// replacement is written out. Always deceptive.
#[cfg(not(unix))]
fn escape_unreadable(path: &Path) -> (String, bool) {
    let (text, _) = escape_text(&path.to_string_lossy());
    (text.replace('\u{fffd}', "\\u{fffd}"), true)
}

// ---------------------------------------------------------------------------------------------
// Identity of the run.

pub(crate) fn host_name() -> String {
    #[cfg(unix)]
    {
        let name = rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned();
        if !name.is_empty() {
            return name;
        }
    }
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The lowercase hexadecimal SHA-256 of a file.
pub(crate) fn sha256_file(path: &Path) -> io::Result<String> {
    let mut digest = FileDigest::open(path)?;
    loop {
        if !digest.step()? {
            return Ok(digest.finish());
        }
    }
}

/// The SHA-256 of a file, taken one chunk at a time, so that a caller can look at a clock or at an
/// interrupt between two chunks. [`sha256_file`] takes the whole file at once.
pub(crate) struct FileDigest {
    file: File,
    hasher: Sha256,
    buffer: Vec<u8>,
}

impl FileDigest {
    /// Opens `path`, whose chunks [`FileDigest::step`] hashes.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            hasher: Sha256::new(),
            buffer: vec![0_u8; 64 * 1024],
        })
    }

    /// Hashes the next chunk of the file, and says whether there was one: `false` is the end of
    /// the file, and nothing was hashed.
    pub(crate) fn step(&mut self) -> io::Result<bool> {
        let read = self.file.read(&mut self.buffer)?;
        self.hasher.update(&self.buffer[..read]);
        Ok(read > 0)
    }

    /// The digest of everything hashed so far, in lowercase hexadecimal.
    pub(crate) fn finish(self) -> String {
        let mut hex = String::with_capacity(64);
        for byte in self.hasher.finalize() {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

/// The last second that [`rfc3339`] and [`compact_utc`] spell: `9999-12-31T23:59:59Z`. The schemas
/// of the run documents want a date-time whose year has four digits, and a run id holds a stamp of
/// exactly sixteen characters ([`is_compact_utc`]), so no clock is spelled with more. A clock set
/// past this second is spelled as it, as a clock set before the epoch is spelled as the epoch.
const LAST_SPELLED_SECOND: u64 = 253_402_300_799;

/// `time` as an RFC 3339 UTC timestamp with millisecond precision, such as
/// `2026-09-30T13:37:56.123Z`. A clock before the epoch is spelled as the epoch, and one after
/// `9999-12-31T23:59:59.999Z` as that moment: the year always has four digits, as the schemas of
/// the run documents want.
pub(crate) fn rfc3339(time: SystemTime) -> String {
    let (date, seconds_of_day, millis) = civil(time);
    format!(
        "{date}T{:02}:{:02}:{:02}.{millis:03}Z",
        seconds_of_day / 3600,
        seconds_of_day / 60 % 60,
        seconds_of_day % 60,
    )
}

/// `time` as a compact UTC timestamp for a run id, such as `20260930T133756Z`. A clock before the
/// epoch is spelled as the epoch, and one after `9999-12-31T23:59:59Z` as that second, so that the
/// stamp always has sixteen characters.
pub(crate) fn compact_utc(time: SystemTime) -> String {
    let (date, seconds_of_day, _) = civil(time);
    format!(
        "{}T{:02}{:02}{:02}Z",
        date.replace('-', ""),
        seconds_of_day / 3600,
        seconds_of_day / 60 % 60,
        seconds_of_day % 60,
    )
}

/// The UTC date as `YYYY-MM-DD`, the seconds since midnight, and the milliseconds of the second.
/// A clock before the epoch is read as the epoch, and one after [`LAST_SPELLED_SECOND`] as the
/// last moment of that second, so that the year has four digits.
///
/// The date comes from the days-since-epoch to civil-date algorithm: shift the epoch to 0000-03-01
/// so leap days fall at the end of the year, then work in 400-year eras of 146097 days.
fn civil(time: SystemTime) -> (String, u64, u32) {
    let since_epoch = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .min(Duration::new(LAST_SPELLED_SECOND, 999_999_999));
    let days = i64::try_from(since_epoch.as_secs() / 86_400).unwrap_or(0);
    let seconds_of_day = since_epoch.as_secs() % 86_400;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        format!("{year:04}-{month:02}-{day:02}"),
        seconds_of_day,
        since_epoch.subsec_millis(),
    )
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    use super::*;

    fn at(seconds: u64, millis: u32) -> SystemTime {
        UNIX_EPOCH + Duration::new(seconds, millis * 1_000_000)
    }

    #[test]
    fn timestamps_are_rfc3339_in_utc() {
        assert_eq!(rfc3339(at(0, 0)), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(at(951_782_400, 5)), "2000-02-29T00:00:00.005Z");
        assert_eq!(rfc3339(at(1_790_775_476, 123)), "2026-09-30T13:37:56.123Z");
        assert_eq!(rfc3339(at(4_102_444_799, 999)), "2099-12-31T23:59:59.999Z");
    }

    #[test]
    fn a_run_id_timestamp_is_a_valid_id_component() {
        let id = compact_utc(at(1_790_775_476, 0));

        assert_eq!(id, "20260930T133756Z");
        assert!(id.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    }

    #[test]
    fn leap_days_and_year_ends_are_dated_correctly() {
        for (seconds, date) in [
            (68_256_000, "1972-03-01"),
            (94_608_000, "1972-12-31"),
            (1_709_164_800, "2024-02-29"),
            (1_709_251_200, "2024-03-01"),
            (4_107_456_000, "2100-02-28"),
            (4_107_542_400, "2100-03-01"),
        ] {
            assert_eq!(civil(at(seconds, 0)).0, date, "{seconds}");
        }
    }

    /// The pattern that a schema of the run documents holds for a timestamp (`$defs/timestamp`).
    fn timestamp_pattern(schema: &str) -> Regex {
        let schema: serde_json::Value = serde_json::from_str(schema).expect("a schema");
        let pattern = schema["$defs"]["timestamp"]["pattern"]
            .as_str()
            .expect("the schema holds a pattern for a timestamp");
        Regex::new(pattern).expect("the pattern is a regular expression")
    }

    /// Every schema whose documents carry a timestamp that `rfc3339` writes.
    fn timestamp_patterns() -> Vec<(&'static str, Regex)> {
        vec![
            (
                "harness-soak",
                timestamp_pattern(include_str!("../schemas/harness-soak.schema.json")),
            ),
            (
                "harness-summary",
                timestamp_pattern(include_str!("../schemas/harness-summary.schema.json")),
            ),
            (
                "harness-tui",
                timestamp_pattern(include_str!("../schemas/harness-tui.schema.json")),
            ),
            (
                "harness-counts",
                timestamp_pattern(include_str!("../schemas/harness-counts.schema.json")),
            ),
        ]
    }

    /// The clocks whose spelling is in question: before the epoch, now, around the end of the
    /// last year that has four digits, and as far beyond it as the clock goes.
    fn clocks() -> Vec<SystemTime> {
        let mut clocks = vec![
            UNIX_EPOCH - Duration::from_secs(1),
            UNIX_EPOCH,
            at(1_790_775_476, 123),
            at(LAST_SPELLED_SECOND - 1, 500),
            at(LAST_SPELLED_SECOND, 0),
            at(LAST_SPELLED_SECOND, 999),
            at(LAST_SPELLED_SECOND + 1, 0),
            at(LAST_SPELLED_SECOND + 1, 999),
            at(LAST_SPELLED_SECOND + 86_400 * 366, 0),
        ];
        // Far past it, where the clock can count that far at all.
        for seconds in [1_u64 << 40, 1 << 50, 1 << 62, u64::MAX / 2] {
            clocks.extend(UNIX_EPOCH.checked_add(Duration::new(seconds, 999_999_999)));
        }
        clocks
    }

    #[test]
    fn the_last_second_with_four_digits_is_spelled_and_a_clock_past_it_is_spelled_as_it() {
        assert_eq!(
            rfc3339(at(LAST_SPELLED_SECOND, 0)),
            "9999-12-31T23:59:59.000Z"
        );
        assert_eq!(
            rfc3339(at(LAST_SPELLED_SECOND, 999)),
            "9999-12-31T23:59:59.999Z"
        );
        assert_eq!(compact_utc(at(LAST_SPELLED_SECOND, 0)), "99991231T235959Z");
        // 10000-01-01T00:00:00Z, which has a year of five digits, is not written.
        assert_eq!(
            rfc3339(at(LAST_SPELLED_SECOND + 1, 0)),
            "9999-12-31T23:59:59.999Z"
        );
        assert_eq!(
            compact_utc(at(LAST_SPELLED_SECOND + 1, 0)),
            "99991231T235959Z"
        );
        // As a clock before the epoch is the epoch.
        assert_eq!(
            rfc3339(UNIX_EPOCH - Duration::from_secs(1)),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn no_clock_is_spelled_in_a_form_the_schemas_or_the_pointer_reader_refuse() {
        let patterns = timestamp_patterns();
        for clock in clocks() {
            let text = rfc3339(clock);
            assert_eq!(text.len(), 24, "{text}");
            for (schema, pattern) in &patterns {
                assert!(
                    pattern.is_match(&text),
                    "{text} is not a timestamp by the pattern of {schema}"
                );
            }
            let stamp = compact_utc(clock);
            assert_eq!(stamp.len(), 16, "{stamp}");
            assert!(is_compact_utc(&stamp), "{stamp}");
            let id = format!("{stamp}-4242");
            assert!(is_run_id(&id), "{id}");
            assert!(
                is_pointer(format!("{id}\n").as_bytes()),
                "a run that wrote `{id}` could not replace its own pointer"
            );
        }
    }

    #[test]
    fn a_stamp_with_five_digits_in_its_year_is_not_a_run_s_stamp() {
        for stamp in ["100000101T000000Z", "99991231T235959Z0", "0000101T000000Z"] {
            assert!(!is_compact_utc(stamp), "{stamp}");
        }
        assert!(is_compact_utc("99991231T235959Z"));
        assert!(!is_compact_utc("99991232T235959Z"));
    }

    #[test]
    fn medians_and_worst_values_ignore_missing_metrics() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0]), Some(3.0));
        assert_eq!(median(&[5.0, 1.0, 9.0]), Some(5.0));
        assert_eq!(median(&[1.0, 2.0, 3.0, 10.0]), Some(2.5));
        assert_eq!(worst(&[]), None);
        assert_eq!(worst(&[1.0, 7.5, 3.0]), Some(7.5));
    }

    #[test]
    fn durations_read_in_milliseconds_or_seconds() {
        assert_eq!(format_ms(None), "-");
        assert_eq!(format_ms(Some(42.4)), "42 ms");
        assert_eq!(format_ms(Some(1500.0)), "1.50 s");
    }

    /// Run ids in the form every runner writes.
    const FIRST: &str = "20261006T120000Z-1";
    const SECOND: &str = "20261006T120001Z-2";
    #[cfg(unix)]
    const THIRD: &str = "20261006T120002Z-3";

    /// Strings that have the form of a run id and that no runner can write: a date or a time of day
    /// that does not exist, a time before the epoch, and a process id that is zero, has a leading
    /// zero or a sign, or does not fit in 32 bits.
    const LOOK_LIKE_RUN_IDS: &[&str] = &[
        "99999999T999999Z-0",
        "99999999T999999Z-1",
        "20261301T120000Z-1",
        "20260001T120000Z-1",
        "20261100T120000Z-1",
        "20261132T120000Z-1",
        "20261131T120000Z-1",
        "20260230T120000Z-1",
        "20260229T120000Z-1",
        "21000229T120000Z-1",
        "20261006T240000Z-1",
        "20261006T126000Z-1",
        "20261006T120060Z-1",
        "19691231T235959Z-1",
        "00000101T000000Z-1",
        "20261006T120000Z-0",
        "20261006T120000Z-00",
        "20261006T120000Z-007",
        "20261006T120000Z-+42",
        "20261006T120000Z-4294967296",
        "20261006T120000Z-7777777777",
        "20261006T120000Z-9999999999",
    ];

    #[test]
    fn the_latest_pointer_follows_the_newest_run() {
        let out = tempfile::tempdir().expect("a temporary directory");
        fs::create_dir(out.path().join(FIRST)).expect("run directory");
        fs::create_dir(out.path().join(SECOND)).expect("run directory");

        point_latest_at(out.path(), FIRST).expect("first pointer");
        point_latest_at(out.path(), SECOND).expect("second pointer");

        #[cfg(unix)]
        assert_eq!(
            fs::read_link(out.path().join("latest")).expect("a link"),
            Path::new(SECOND)
        );
        #[cfg(not(unix))]
        assert_eq!(
            fs::read_to_string(out.path().join("latest"))
                .expect("a file")
                .trim(),
            SECOND
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_a_run_made_is_replaced_even_when_it_points_nowhere() {
        let out = tempfile::tempdir().expect("a temporary directory");
        std::os::unix::fs::symlink("gone-run", out.path().join("latest")).expect("a dangling link");

        latest_can_be_replaced(out.path()).expect("a link a run made");
        point_latest_at(out.path(), THIRD).expect("the link is replaced");

        assert_eq!(
            fs::read_link(out.path().join("latest")).expect("a link"),
            Path::new(THIRD)
        );
    }

    #[test]
    fn anything_else_is_left_alone_and_the_error_names_it() {
        let out = tempfile::tempdir().expect("a temporary directory");
        let latest = out.path().join("latest");

        // A directory with something in it.
        fs::create_dir(&latest).expect("a directory");
        fs::write(latest.join("precious.txt"), b"keep me").expect("a file");
        let error = point_latest_at(out.path(), FIRST).expect_err("a directory is not a link");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("latest"), "{error}");
        assert_eq!(
            fs::read(latest.join("precious.txt")).expect("still there"),
            b"keep me"
        );
        fs::remove_dir_all(&latest).expect("cleaned up");

        // A regular file that is not a run id, and (on Unix) any regular file at all.
        fs::write(&latest, b"My notes, not a run id\n").expect("a file");
        for _ in 0..2 {
            let error = point_latest_at(out.path(), FIRST).expect_err("a file of notes");
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert!(error.to_string().contains("latest"), "{error}");
            assert_eq!(
                fs::read(&latest).expect("still there"),
                b"My notes, not a run id\n"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_a_regular_file_is_not_a_run_s_link_even_when_it_holds_a_run_id() {
        let out = tempfile::tempdir().expect("a temporary directory");
        fs::write(out.path().join("latest"), format!("{FIRST}\n")).expect("a file");

        let error = latest_can_be_replaced(out.path()).expect_err("a file");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(out.path().join("latest")).expect("still there"),
            format!("{FIRST}\n").as_bytes()
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn elsewhere_a_file_that_holds_a_run_id_is_replaced() {
        let out = tempfile::tempdir().expect("a temporary directory");
        fs::write(out.path().join("latest"), format!("{FIRST}\r\n")).expect("a file");

        point_latest_at(out.path(), SECOND).expect("a run's own file is replaced");

        assert_eq!(
            fs::read_to_string(out.path().join("latest"))
                .expect("a file")
                .trim(),
            SECOND
        );
    }

    /// What decides on Windows, where a run writes its id in a file, runs here too: it takes the
    /// platform's way of pointing as a parameter.
    #[test]
    fn where_a_run_writes_its_id_in_a_file_only_a_file_that_holds_one_is_replaced() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let decide = |content: &[u8]| {
            let path = dir.path().join("latest");
            fs::write(&path, content).expect("a file");
            made_by_a_run(
                &path,
                &fs::symlink_metadata(&path).expect("metadata"),
                false,
            )
        };

        for mine in [
            FIRST.to_owned(),
            format!("{FIRST}\n"),
            format!("{SECOND}\r\n"),
            "20261006T120000Z-4242".to_owned(),
            "19700101T000000Z-1".to_owned(),
            "20240229T000000Z-7\n".to_owned(),
            "20991231T235959Z-4294967295\n".to_owned(),
        ] {
            assert!(decide(mine.as_bytes()), "{mine:?} is a run's own");
        }
        for theirs in [
            "draft-1",
            "draft-1\n",
            "run-1",
            "run-1\r\n",
            "notes-2",
            "latest-1",
            "backup-20261006",
            "v1-2",
            "My notes, not a run id\n",
            "",
            "\n",
            "20261006T120000Z",
            "20261006T120000Z-",
            "20261006T120000Z-pid",
            "20261006T120000Z-4242 and a note",
            "20261006T120000Z-4242\nand a second line\n",
            " 20261006T120000Z-4242",
            "20261006T120000Z-4242 ",
            "20261006t120000z-4242",
            "20261006T120000Z-12345678901",
            // One line ending is allowed after the id, and no more, and nothing after it.
            "20261006T120000Z-4242\n\n",
            "20261006T120000Z-4242\r\n\r\n",
            "20261006T120000Z-4242\n\r\n",
            "20261006T120000Z-4242\r",
            "20261006T120000Z-4242\n\n\nMy notes follow, and are not a pointer\n",
            "20261006T120000Z-4294967295\n\n\nMy notes follow, and are not a pointer\n",
        ] {
            assert!(
                !decide(theirs.as_bytes()),
                "{theirs:?} is somebody's, and is left alone"
            );
        }
        for theirs in LOOK_LIKE_RUN_IDS {
            assert!(
                !decide(theirs.as_bytes()),
                "{theirs:?} only looks like a run id, and is left alone"
            );
        }

        // A directory is never a pointer; and where a run makes links, a file never is.
        let directory = dir.path().join("a-directory");
        fs::create_dir(&directory).expect("a directory");
        assert!(!made_by_a_run(
            &directory,
            &fs::symlink_metadata(&directory).expect("metadata"),
            false
        ));
        let file = dir.path().join("latest");
        fs::write(&file, FIRST).expect("a file");
        assert!(!made_by_a_run(
            &file,
            &fs::symlink_metadata(&file).expect("metadata"),
            true
        ));
    }

    #[test]
    fn a_run_id_is_the_start_of_the_run_and_the_process_that_ran_it() {
        for good in [
            "20261006T120000Z-4242",
            "20261006T120000Z-1",
            "19700101T000000Z-1",
            "20000229T000000Z-1",
            "20240229T235959Z-4242",
            "20261231T235959Z-4294967295",
            "20991231T235959Z-4294967295",
        ] {
            assert!(is_run_id(good), "{good}");
        }
        for bad in [
            "",
            "-",
            "run-1",
            "draft-1",
            "a",
            "9.x_y-z",
            "-run",
            ".hidden",
            "run 1",
            "run/1",
            "é",
            "a\nb",
            "20261006T120000Z",
            "20261006T120000Z-",
            "-4242",
            "2026100T1200000Z-4242",
            "20261006T12000Z-4242",
            "20261006 120000Z-4242",
            "20261006T120000-4242",
            "20261006T120000Z4242",
            "20261006T120000Z--4242",
            "20261006T120000Z-42-42",
            "20261006T120000Z-4242\n",
            "20261006T120000Z-\u{ff11}\u{ff12}",
            "\u{ff12}\u{ff10}\u{ff12}\u{ff16}1006T120000Z-4242",
        ] {
            assert!(!is_run_id(bad), "{bad:?}");
        }
        for bad in LOOK_LIKE_RUN_IDS {
            assert!(!is_run_id(bad), "{bad:?}");
        }
        assert!(is_run_id("20261006T120000Z-4294967295"));
        assert!(!is_run_id("20261006T120000Z-4294967296"));
        assert!(!is_run_id(&format!("20261006T120000Z-{}", "7".repeat(10))));
        assert!(!is_run_id(&format!("20261006T120000Z-{}", "7".repeat(11))));
        assert_eq!(MAX_RUN_ID_LEN, "20261006T120000Z-4294967295".len());
        assert_eq!(MAX_POINTER_LEN, "20261006T120000Z-4294967295\r\n".len());
    }

    #[test]
    fn february_has_a_29th_in_a_leap_year_only() {
        for (year, leap) in [
            (1970, false),
            (1972, true),
            (1999, false),
            (2000, true),
            (2024, true),
            (2026, false),
            (2100, false),
            (2400, true),
        ] {
            assert_eq!(is_leap_year(year), leap, "{year}");
            assert_eq!(days_in_month(year, 2), if leap { 29 } else { 28 }, "{year}");
        }
        let lengths: Vec<u32> = (1..=12).map(|month| days_in_month(2026, month)).collect();
        assert_eq!(lengths, [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]);
    }

    #[test]
    fn every_id_a_runner_can_write_is_a_run_id() {
        // A runner writes `<compact_utc>-<process id>` and nothing else: every day from the epoch
        // to the end of the century and after, at the start, the middle, and the end of the day,
        // with the smallest and the largest process id.
        for day in 0..50_000_u64 {
            for second_of_day in [0, 43_199, 86_399] {
                let stamp = compact_utc(at(day * 86_400 + second_of_day, 0));
                for process in [1, 4242, u32::MAX] {
                    let id = format!("{stamp}-{process}");
                    assert!(is_run_id(&id), "{id}");
                }
            }
        }
    }

    #[test]
    fn a_clock_set_before_the_epoch_writes_an_id_that_is_accepted() {
        let id = format!("{}-1", compact_utc(UNIX_EPOCH - Duration::from_secs(1)));

        assert_eq!(id, "19700101T000000Z-1");
        assert!(is_run_id(&id), "{id}");
    }

    #[test]
    fn a_file_is_read_to_its_end_before_it_is_taken_for_a_pointer() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("latest");
        let judge = |content: &[u8]| {
            fs::write(&path, content).expect("a file");
            holds_a_run_id(&path)
        };
        let longest = "20261006T120000Z-4294967295";

        assert!(judge(longest.as_bytes()));
        assert!(judge(format!("{longest}\n").as_bytes()));
        assert!(judge(format!("{longest}\r\n").as_bytes()));
        // The first bytes of each of these are a run id and the line endings that the window of
        // the first thirty bytes held, which is all that was once looked at: what follows is the
        // person's, and the file is not a pointer.
        let notes = "My own notes, which this file is, go on from here.\n".repeat(100);
        for content in [
            format!("{longest}\n\n\n{notes}"),
            format!("{longest}\r\n\r\n{notes}"),
            format!("{longest}\n{notes}"),
            format!("20261006T120000Z-4242{}{notes}", "\n".repeat(9)),
            format!("{longest}{}", "\n".repeat(4096)),
        ] {
            assert!(
                !judge(content.as_bytes()),
                "a file that goes on after an id was taken for a pointer"
            );
        }
    }

    #[test]
    fn the_digest_of_a_file_is_the_standard_one_whole_or_a_chunk_at_a_time() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("file");
        for (content, digest) in [
            (
                Vec::new(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc".to_vec(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                vec![b'a'; 1_000_000],
                "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
            ),
        ] {
            fs::write(&path, &content).expect("a file");

            assert_eq!(sha256_file(&path).expect("a digest"), digest);

            let mut chunked = FileDigest::open(&path).expect("a file to hash");
            let mut chunks = 0;
            while chunked.step().expect("a chunk") {
                chunks += 1;
            }
            assert_eq!(chunked.finish(), digest);
            assert_eq!(chunks > 1, content.len() > 64 * 1024, "{}", content.len());
        }
    }

    #[test]
    fn an_id_the_runners_write_is_a_run_id() {
        // Every runner names its run `<compact_utc>-<process id>`; this is that form.
        let id = format!("{}-{}", compact_utc(SystemTime::now()), std::process::id());

        assert!(is_run_id(&id), "{id}");
    }

    #[cfg(unix)]
    #[test]
    fn the_error_shows_the_path_it_names_escaped() {
        let out = tempfile::tempdir().expect("a temporary directory");
        let hostile = out.path().join("target\u{1b}[2J\nname");
        fs::create_dir(&hostile).expect("a hostile directory");
        fs::write(hostile.join("latest"), b"not a link").expect("a file");

        let error = latest_can_be_replaced(&hostile).expect_err("a regular file");

        let shown = error.to_string();
        assert!(
            shown.contains("[deceptive]") && shown.contains("\\x1b[2J\\nname"),
            "{shown}"
        );
        assert!(!shown.chars().any(is_deceptive), "{shown:?}");
    }

    #[test]
    fn a_path_is_shown_by_the_rule_excise_shows_paths_by() {
        for (path, shown) in [
            ("/plain/path", "/plain/path"),
            (
                "/with space/caf\u{e9}/\u{65e5}\u{672c}",
                "/with space/caf\u{e9}/\u{65e5}\u{672c}",
            ),
            (r"/back\slash", r"/back\\slash"),
            ("/a\nb", r"[deceptive] /a\nb"),
            ("/a\rb", r"[deceptive] /a\rb"),
            ("/a\tb", r"[deceptive] /a\tb"),
            ("/a\u{1b}[31mb", r"[deceptive] /a\x1b[31mb"),
            ("/a\u{202e}b", r"[deceptive] /a\u{202e}b"),
            ("/a\u{7f}b", r"[deceptive] /a\u{007f}b"),
            ("/a\u{85}b", r"[deceptive] /a\u{0085}b"),
            ("/a\u{2069}b", r"[deceptive] /a\u{2069}b"),
            ("/a\u{200b}b", "/a\u{200b}b"),
        ] {
            assert_eq!(safe_path_text(Path::new(path)), shown, "{path:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_text_is_shown_by_its_bytes() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt as _};

        let path = Path::new(OsStr::from_bytes(b"/bad\xffname\\\x1b[2J"));

        assert_eq!(safe_path_text(path), r"[deceptive] /bad\xffname\\\x1b[2J");
    }
}
