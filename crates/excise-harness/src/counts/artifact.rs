//! Reading a counts document that came from outside this build.
//!
//! A pull request's counts are an artifact that the pull request's own workflow run uploaded, and
//! a fork controls that workflow. So the artifact is untrusted input, and this is the only way
//! one is read: a regular file (never a link), of a bounded size, that is UTF-8 and JSON and
//! passes the `harness-counts` schema and the rules the schema cannot say. What it fails with
//! describes the problem in text that is safe to print (see [`super::text`]): it holds nothing
//! the document's author could use to forge a line of a log.

use std::{
    fmt::Write as _,
    fs::{self, File},
    io::{self, Read},
    path::Path,
};

use jsonschema::Validator;
use serde_json::Value;
use thiserror::Error;

use crate::report::{CountsInvalid, Document, HarnessCounts};

use super::text::log_safe;

/// The largest document that is read, in bytes. A document of every fixture and every count is
/// about three kilobytes; the schema itself bounds a document at sixteen cases of thirty-two
/// counts, well under this.
pub const MAX_DOCUMENT_BYTES: u64 = 64 * 1024;

/// How many violations of the schema an error lists. The rest are counted.
const LISTED_VIOLATIONS: usize = 5;

/// The longest piece of text from a document that an error repeats.
const SHOWN: usize = 80;

/// A document that cannot be used, and why.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ArtifactError {
    /// The file cannot be inspected or read.
    #[error("cannot read the document: {0}")]
    Unreadable(String),
    /// The path is not a regular file: a link, a directory, or a device.
    #[error("the document is not a regular file")]
    NotAFile,
    /// The document is larger than a counts document can be.
    #[error("the document is larger than {MAX_DOCUMENT_BYTES} bytes")]
    TooLarge,
    /// The document is not UTF-8 text.
    #[error("the document is not UTF-8 text")]
    NotText,
    /// The document is not JSON.
    #[error("the document is not JSON: {0}")]
    NotJson(String),
    /// The document breaks the `harness-counts` schema.
    #[error("the document breaks the harness-counts schema: {0}")]
    Schema(String),
    /// The document passes the schema but breaks a rule the schema cannot say.
    #[error("the document is not a valid harness-counts document: {0}")]
    Invalid(String),
}

/// Reads the counts document at `path`, which came from outside this build.
///
/// # Errors
///
/// Returns why the file is not a counts document this build accepts.
pub fn read_untrusted(path: &Path) -> Result<HarnessCounts, ArtifactError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| unreadable(&error))?;
    if !metadata.file_type().is_file() {
        return Err(ArtifactError::NotAFile);
    }
    if metadata.len() > MAX_DOCUMENT_BYTES {
        return Err(ArtifactError::TooLarge);
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|error| unreadable(&error))?
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| unreadable(&error))?;
    parse_untrusted(&bytes)
}

fn unreadable(error: &io::Error) -> ArtifactError {
    ArtifactError::Unreadable(log_safe(&error.to_string(), SHOWN * 2))
}

/// Parses `bytes` as an untrusted counts document: the checks of [`read_untrusted`] after the
/// file has been read.
///
/// # Errors
///
/// Returns why the bytes are not a counts document this build accepts.
pub fn parse_untrusted(bytes: &[u8]) -> Result<HarnessCounts, ArtifactError> {
    if u64::try_from(bytes.len()).map_or(true, |length| length > MAX_DOCUMENT_BYTES) {
        return Err(ArtifactError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ArtifactError::NotText)?;
    let value: Value = serde_json::from_str(text)
        .map_err(|error| ArtifactError::NotJson(log_safe(&error.to_string(), SHOWN * 2)))?;
    let validator = validator()?;
    let violations: Vec<String> = validator
        .iter_errors(&value)
        .map(|error| {
            format!(
                "{}: {}",
                log_safe(&error.instance_path().to_string(), SHOWN),
                log_safe(&error.to_string(), SHOWN * 2)
            )
        })
        .collect();
    if !violations.is_empty() {
        let more = violations.len().saturating_sub(LISTED_VIOLATIONS);
        let mut listed = violations
            .into_iter()
            .take(LISTED_VIOLATIONS)
            .collect::<Vec<_>>()
            .join("; ");
        if more > 0 {
            let _ = write!(listed, "; and {more} more");
        }
        return Err(ArtifactError::Schema(listed));
    }
    let document: HarnessCounts = serde_json::from_value(value)
        .map_err(|error| ArtifactError::Invalid(log_safe(&error.to_string(), SHOWN * 2)))?;
    document.check().map_err(|error: CountsInvalid| {
        ArtifactError::Invalid(log_safe(&error.to_string(), SHOWN * 2))
    })?;
    Ok(document)
}

/// The compiled `harness-counts` schema, asserting `format` keywords such as `date-time`.
fn validator() -> Result<Validator, ArtifactError> {
    let schema: Value = serde_json::from_str(HarnessCounts::SCHEMA_JSON)
        .map_err(|error| ArtifactError::Unreadable(format!("the schema is not JSON: {error}")))?;
    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .map_err(|error| ArtifactError::Unreadable(format!("the schema does not compile: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        counts::test_support::{commit, for_pull_request, record, suite},
        report::{Document, MAX_COUNT},
    };

    fn valid() -> HarnessCounts {
        for_pull_request(
            record(&commit(0xee), suite(350)),
            7,
            &commit(0xb0),
            &commit(0xaa),
        )
    }

    fn text() -> String {
        valid().to_json_pretty().expect("a document renders")
    }

    /// `text` with trailing spaces added until it is `length` bytes long.
    fn padded(text: &str, length: usize) -> Vec<u8> {
        let mut bytes = text.trim_end().as_bytes().to_vec();
        assert!(
            bytes.len() <= length,
            "the document is longer than the padding"
        );
        bytes.resize(length, b' ');
        bytes
    }

    fn written(directory: &tempfile::TempDir, bytes: &[u8]) -> std::path::PathBuf {
        let path = directory.path().join("counts.json");
        fs::write(&path, bytes).expect("a file is written");
        path
    }

    /// Whether `message` is a single line that cannot be taken for a workflow command.
    fn is_log_safe(message: &str) -> bool {
        message.is_ascii()
            && !message.contains(['\n', '\r', '\u{1b}'])
            && !message.trim_start().starts_with("::")
    }

    #[test]
    fn a_valid_document_is_read_back_exactly() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = written(&directory, text().as_bytes());

        assert_eq!(read_untrusted(&path), Ok(valid()));
    }

    #[test]
    fn a_document_of_exactly_the_largest_size_is_read_and_one_byte_more_is_not() {
        let limit = usize::try_from(MAX_DOCUMENT_BYTES).expect("the limit fits");
        let directory = tempfile::tempdir().expect("a directory");

        let largest = written(&directory, &padded(&text(), limit));
        assert_eq!(read_untrusted(&largest), Ok(valid()));
        let too_large = written(&directory, &padded(&text(), limit + 1));
        assert_eq!(read_untrusted(&too_large), Err(ArtifactError::TooLarge));
        assert_eq!(
            parse_untrusted(&padded(&text(), limit + 1)),
            Err(ArtifactError::TooLarge),
            "bytes that were read past the limit are refused too"
        );
    }

    #[test]
    fn an_oversized_file_is_refused_without_being_read_whole() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = written(&directory, &vec![b'x'; 5 * 1024 * 1024]);

        assert_eq!(read_untrusted(&path), Err(ArtifactError::TooLarge));
    }

    #[test]
    #[cfg(unix)]
    fn a_link_is_refused_even_when_it_points_at_a_valid_document() {
        let directory = tempfile::tempdir().expect("a directory");
        let real = written(&directory, text().as_bytes());
        let link = directory.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).expect("a link is made");

        assert_eq!(read_untrusted(&link), Err(ArtifactError::NotAFile));
        assert_eq!(read_untrusted(&real), Ok(valid()));
    }

    #[test]
    fn a_directory_and_a_missing_file_are_refused() {
        let directory = tempfile::tempdir().expect("a directory");

        assert_eq!(
            read_untrusted(directory.path()),
            Err(ArtifactError::NotAFile)
        );
        let missing = read_untrusted(&directory.path().join("absent.json"));
        assert!(
            matches!(missing, Err(ArtifactError::Unreadable(_))),
            "{missing:?}"
        );
    }

    #[test]
    fn text_that_is_not_utf_8_or_not_json_is_refused() {
        assert_eq!(parse_untrusted(b"\xff\xfe{}"), Err(ArtifactError::NotText));
        for not_json in ["", "not json", "{", "{\"a\": }", "{} {}"] {
            let error = parse_untrusted(not_json.as_bytes()).expect_err(not_json);
            assert!(
                matches!(error, ArtifactError::NotJson(_)),
                "{not_json}: {error:?}"
            );
        }
    }

    #[test]
    fn nesting_cannot_exhaust_the_stack() {
        let deep = format!("{}{}", "[".repeat(30_000), "]".repeat(30_000));

        let error = parse_untrusted(deep.as_bytes()).expect_err("too deep");

        assert!(matches!(error, ArtifactError::NotJson(_)), "{error:?}");
    }

    #[test]
    fn json_that_is_not_a_counts_document_breaks_the_schema() {
        for other in ["[]", "\"counts\"", "null", "7", "{}"] {
            let error = parse_untrusted(other.as_bytes()).expect_err(other);
            assert!(
                matches!(error, ArtifactError::Schema(_)),
                "{other}: {error:?}"
            );
        }
    }

    #[test]
    fn a_document_that_breaks_the_schema_is_refused_whatever_the_break() {
        let good = text();
        for (what, bad) in [
            (
                "an undeclared field",
                good.replace("\"cases\"", "\"extra\": 1,\n  \"cases\""),
            ),
            (
                "another schema version",
                good.replace("\"schema_version\": 1", "\"schema_version\": 2"),
            ),
            (
                "another document kind",
                good.replace("harness-counts", "harness-ab"),
            ),
            ("a short commit", good.replace(&commit(0xee), "abc")),
            (
                "a negative count",
                good.replace("\"entries\": 1002", "\"entries\": -1"),
            ),
            (
                "a fractional count",
                good.replace("\"entries\": 1002", "\"entries\": 1.5"),
            ),
            (
                "a count a double cannot hold",
                good.replace(
                    "\"entries\": 1002",
                    &format!("\"entries\": {}", MAX_COUNT + 1),
                ),
            ),
            ("a time without a zone", good.replace("+11:00", "")),
        ] {
            let error = parse_untrusted(bad.as_bytes()).expect_err(what);
            assert!(
                matches!(error, ArtifactError::Schema(_)),
                "{what}: {error:?}"
            );
        }
    }

    #[test]
    fn a_fixture_counted_twice_under_one_profile_is_refused() {
        let mut twice = valid();
        twice.cases[1] = twice.cases[0].clone();
        let bytes = twice.to_json_pretty().expect("renders");

        let error = parse_untrusted(bytes.as_bytes()).expect_err("a duplicate");

        assert!(matches!(error, ArtifactError::Invalid(_)), "{error:?}");
    }

    #[test]
    fn what_a_hostile_document_makes_an_error_say_is_one_safe_line() {
        let hostile = "x\n::set-output name=pwn::1\r\u{1b}[2J";
        let mut document = valid();
        document.context.toolchain = hostile.to_owned();
        // Serialised as JSON this is one line with escapes, as an attacker would send it.
        let bytes = serde_json::to_string(&document).expect("serialises");

        let error = parse_untrusted(bytes.as_bytes()).expect_err("the schema refuses it");

        let message = error.to_string();
        assert!(matches!(error, ArtifactError::Schema(_)), "{message}");
        assert!(is_log_safe(&message), "{message}");
        assert!(
            message.contains("/context/toolchain"),
            "the place is named: {message}"
        );
    }

    #[test]
    fn an_error_stays_short_however_many_things_are_wrong() {
        let mut document = valid();
        for case in &mut document.cases {
            case.fixture.id = "BAD ".repeat(100);
        }
        document.context.git_sha = "z".repeat(40);
        document.context.runner.os = "../..".to_owned();
        document.context.runner.arch = "x-86".to_owned();
        document.context.toolchain = "z".repeat(5_000);
        document.context.runner.os_version = "\n".repeat(1_000);
        let bytes = serde_json::to_string(&document).expect("serialises");
        assert!(bytes.len() < usize::try_from(MAX_DOCUMENT_BYTES).expect("fits"));

        let message = parse_untrusted(bytes.as_bytes())
            .expect_err("the schema refuses it")
            .to_string();

        assert!(message.len() < 3_000, "{} bytes: {message}", message.len());
        assert!(is_log_safe(&message), "{message}");
        assert!(message.contains("more"), "the rest are counted: {message}");
    }
}
