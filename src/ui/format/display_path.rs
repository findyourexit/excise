use std::ffi::OsStr;
use std::path::Path;

use crate::native_path::{SafeDisplayPath, safe_display_os_str, safe_display_path};

pub use crate::native_path::DECEPTIVE_DISPLAY_MARKER;

#[cfg(test)]
#[must_use]
pub fn display_path_info(path: &Path) -> SafeDisplayPath {
    let raw = safe_display_path(path);
    if raw.deceptive {
        raw
    } else {
        let normalized = crate::tests::fixtures::snapshot_path(path);
        safe_display_path(Path::new(&normalized))
    }
}

#[cfg(not(test))]
#[must_use]
pub fn display_path_info(path: &Path) -> SafeDisplayPath {
    safe_display_path(path)
}

#[must_use]
pub fn display_os_str_info(value: &OsStr) -> SafeDisplayPath {
    safe_display_os_str(value)
}

#[must_use]
pub fn display_text_info(value: &str) -> SafeDisplayPath {
    safe_display_os_str(OsStr::new(value))
}

#[must_use]
pub fn display_text(value: &str) -> String {
    let displayed = display_text_info(value);
    if displayed.deceptive {
        format!("{DECEPTIVE_DISPLAY_MARKER} {}", displayed.text)
    } else if value.contains(DECEPTIVE_DISPLAY_MARKER) {
        value.to_string()
    } else {
        displayed.text
    }
}

#[must_use]
pub fn display_path_middle(path: &Path, width: u16) -> String {
    let displayed = display_path_info(path);
    truncate_marked(&displayed, width, crate::ui::format::truncate_middle)
}

#[must_use]
pub fn display_path_end(path: &Path, width: u16) -> String {
    let displayed = display_path_info(path);
    truncate_marked(&displayed, width, truncate_end)
}

#[must_use]
pub fn display_os_str_middle(value: &OsStr, width: u16) -> String {
    let displayed = display_os_str_info(value);
    truncate_marked(&displayed, width, crate::ui::format::truncate_middle)
}

pub(crate) fn truncate_marked(
    displayed: &SafeDisplayPath,
    width: u16,
    truncate: fn(&str, u16) -> String,
) -> String {
    if !displayed.deceptive {
        return truncate(&displayed.text, width);
    }
    let marker_width = u16::try_from(DECEPTIVE_DISPLAY_MARKER.len()).unwrap_or(u16::MAX);
    if width == 0 {
        return String::new();
    }
    if width < marker_width {
        return "!".to_string();
    }
    if width == marker_width {
        return DECEPTIVE_DISPLAY_MARKER.to_string();
    }
    let body_width = width.saturating_sub(marker_width + 1);
    let body = truncate(&displayed.text, body_width);
    if body.is_empty() {
        DECEPTIVE_DISPLAY_MARKER.to_string()
    } else {
        format!("{DECEPTIVE_DISPLAY_MARKER} {body}")
    }
}

fn truncate_end(value: &str, width: u16) -> String {
    let maximum = usize::from(width.saturating_sub(1));
    if value.chars().count() <= maximum {
        value.to_string()
    } else if maximum > 1 {
        let mut text = value.chars().take(maximum - 1).collect::<String>();
        text.push('…');
        text
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_text_is_escaped_and_marked() {
        let displayed = display_text("safe\n\u{202e}name\u{1b}[31m");
        assert!(displayed.starts_with(DECEPTIVE_DISPLAY_MARKER));
        assert!(displayed.contains("\\n"));
        assert!(displayed.contains("\\u{202e}"));
        assert!(displayed.contains("\\x1b"));
        assert!(!displayed.chars().any(char::is_control));
    }

    #[test]
    fn deceptive_marker_survives_narrow_middle_truncation() {
        let path = Path::new("prefix-\u{202e}hostile-name");
        for width in 1..=24 {
            let displayed = display_path_middle(path, width);
            assert!(!displayed.chars().any(char::is_control));
            assert!(!displayed.contains('\u{202e}'));
            assert!(
                displayed.starts_with('!') || displayed.starts_with(DECEPTIVE_DISPLAY_MARKER),
                "deception marker lost at width {width}: {displayed:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn invalid_native_bytes_remain_reversible() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let name = OsString::from_vec(b"bad\xffname".to_vec());
        let displayed = display_os_str_info(&name);
        assert!(displayed.deceptive);
        assert_eq!(displayed.text, "bad\\xffname");
        let narrow = display_os_str_middle(&name, 24);
        assert!(narrow.starts_with(DECEPTIVE_DISPLAY_MARKER));
        assert!(narrow.contains("\\xff"));
    }

    #[test]
    fn deceptive_marker_survives_narrow_end_truncation() {
        let path = Path::new("prefix-\u{202e}hostile-name");
        for width in 1..=24 {
            let displayed = display_path_end(path, width);
            assert!(!displayed.chars().any(char::is_control));
            assert!(!displayed.contains('\u{202e}'));
            assert!(
                displayed.starts_with('!') || displayed.starts_with(DECEPTIVE_DISPLAY_MARKER),
                "deception marker lost at width {width}: {displayed:?}"
            );
        }
    }

    #[test]
    fn safe_marked_text_is_not_escaped_again() {
        let displayed = display_text("bad\nname");
        assert_eq!(display_text(&displayed), displayed);
    }
}
