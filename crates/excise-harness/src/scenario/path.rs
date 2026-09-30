//! The fixture-relative path rule.
//!
//! Scenarios never name a scan root and never address a real path. Every path in a scenario is
//! relative to the fixture root that the runner creates, and this module is the single place
//! that decides what "relative to the fixture" means. The rule is purely textual and stricter
//! than any one operating system needs, so a scenario validates identically everywhere and a
//! validated path names the same entry on Windows, macOS, and Linux.

use thiserror::Error;

/// Why a string is not an acceptable fixture-relative path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PathViolation {
    /// The path is the empty string.
    #[error("the path is empty")]
    Empty,
    /// The path starts with a separator, so it is absolute (or a UNC path).
    #[error("the path is absolute")]
    Absolute,
    /// A component starts with a Windows drive designator such as `C:` or `C:x`. On Windows,
    /// joining a component that has a prefix but no root replaces the whole base path, so such a
    /// component can leave the fixture.
    #[error(
        "a component starts with a Windows drive prefix such as `C:`, which replaces the \
         fixture root when joined on Windows"
    )]
    WindowsPrefix,
    /// The path contains a backslash. `/` is the only separator scenarios use.
    #[error("the path contains a backslash; use `/` as the only separator")]
    Backslash,
    /// A component is `..`, which could leave the fixture.
    #[error("the path contains a `..` component")]
    ParentDirectory,
    /// A component is `.`, which is meaningless and can name the fixture root itself.
    #[error("the path contains a `.` component")]
    CurrentDirectory,
    /// The path contains a doubled or trailing `/`.
    #[error("the path contains an empty component (a doubled or trailing `/`)")]
    EmptyComponent,
    /// A component contains `:` without being a drive designator, which is reported as
    /// [`PathViolation::WindowsPrefix`]. Windows uses `:` for alternate data streams
    /// (`name:stream`).
    #[error(
        "a component contains `:`, which Windows uses for drive prefixes and alternate data \
         streams"
    )]
    Colon,
    /// A component ends with `.` or a space. Windows silently removes them, so the same text
    /// would name a different entry there.
    #[error("a component ends with `.` or a space, which Windows silently removes")]
    TrailingDotOrSpace,
    /// A component is a Windows reserved device name, in any case and with or without an
    /// extension.
    #[error(
        "a component is a Windows reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, \
         `CONOUT$`, `COM0` to `COM9`, `LPT0` to `LPT9`, `COM¹`, `COM²`, `COM³`, `LPT¹`, `LPT²`, \
         or `LPT³`), in any case and with or without an extension"
    )]
    ReservedDeviceName,
}

/// Checks that `path` is a canonical fixture-relative path.
///
/// A fixture-relative path is one or more `/`-separated normal components that name the same
/// entry on every operating system. It is never empty and never absolute, and it contains no
/// backslash and no `.`, `..`, or empty component. No component:
///
/// * starts with a Windows drive designator such as `C:` (on Windows, joining `C:x` onto a base
///   path replaces the base);
/// * contains `:` (Windows alternate data streams);
/// * ends with `.` or a space (Windows removes them); or
/// * is a Windows reserved device name: `CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, `COM0`
///   to `COM9`, `LPT0` to `LPT9`, or the superscript forms `COM¹`, `COM²`, `COM³`, `LPT¹`, `LPT²`,
///   and `LPT³`, in any case, with or without an extension.
///
/// Runners MUST resolve every scenario path through this rule (which
/// [`Scenario::validate`](super::Scenario::validate) already applies) and MUST NOT follow
/// symbolic links while resolving it.
///
/// # Errors
///
/// Returns the first [`PathViolation`] found.
pub fn check_fixture_relative_path(path: &str) -> Result<(), PathViolation> {
    if path.is_empty() {
        return Err(PathViolation::Empty);
    }
    if path.starts_with(['/', '\\']) {
        return Err(PathViolation::Absolute);
    }
    // Before the backslash rule, so that `C:\Windows` is reported as the drive it names.
    if starts_with_drive_designator(path) {
        return Err(PathViolation::WindowsPrefix);
    }
    if path.contains('\\') {
        return Err(PathViolation::Backslash);
    }
    path.split('/').try_for_each(check_component)
}

fn check_component(component: &str) -> Result<(), PathViolation> {
    match component {
        "" => Err(PathViolation::EmptyComponent),
        "." => Err(PathViolation::CurrentDirectory),
        ".." => Err(PathViolation::ParentDirectory),
        _ if starts_with_drive_designator(component) => Err(PathViolation::WindowsPrefix),
        _ if component.contains(':') => Err(PathViolation::Colon),
        _ if component.ends_with(['.', ' ']) => Err(PathViolation::TrailingDotOrSpace),
        _ if is_reserved_device_name(component) => Err(PathViolation::ReservedDeviceName),
        _ => Ok(()),
    }
}

/// Whether `text` starts with a drive designator: an ASCII letter and a colon, alone or followed
/// by more text.
fn starts_with_drive_designator(text: &str) -> bool {
    matches!(text.as_bytes(), [letter, b':', ..] if letter.is_ascii_alphabetic())
}

/// Windows device names that stand alone. `COM` and `LPT` also need a trailing digit, which is
/// checked separately.
const STANDALONE_DEVICE_NAMES: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];

/// Whether Windows would open `component` as a DOS device rather than as a file.
///
/// Windows compares the part before the first `.`, ignoring trailing spaces and letter case, so
/// `nul`, `NUL.txt`, and `nul .tar.gz` all name the NUL device. The reserved names are `CON`,
/// `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, and `COM` or `LPT` followed by exactly one digit:
/// `0` to `9`, or a superscript one, two, or three (`¹`, `²`, `³`; U+00B9, U+00B2, U+00B3).
/// Letters match without regard to ASCII case.
fn is_reserved_device_name(component: &str) -> bool {
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _extension)| stem)
        .trim_end_matches(' ');
    if STANDALONE_DEVICE_NAMES
        .iter()
        .any(|device| stem.eq_ignore_ascii_case(device))
    {
        return true;
    }
    let (Some(device), Some(number)) = (stem.get(..3), stem.get(3..)) else {
        return false;
    };
    let mut number = number.chars();
    (device.eq_ignore_ascii_case("COM") || device.eq_ignore_ascii_case("LPT"))
        && matches!(
            (number.next(), number.next()),
            (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
        )
}
