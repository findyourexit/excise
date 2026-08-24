use std::path::{Path, PathBuf};

#[cfg(windows)]
use crate::native_path::safe_display_os_str;
use crate::native_path::{NativePath, ResolvedRoot, identity_for, safe_display_path};

#[test]
fn safe_display_escapes_terminal_and_bidi_controls() {
    let displayed = safe_display_path(Path::new("safe/\u{1b}[31m/\u{202e}\u{206a}name"));
    assert!(displayed.deceptive);
    assert!(!displayed.text.contains('\u{1b}'));
    assert!(!displayed.text.contains('\u{202e}'));
    assert!(!displayed.text.contains('\u{206a}'));
    assert!(displayed.text.contains("\\x1b"));
    assert!(displayed.text.contains("\\u{202e}"));
    assert!(displayed.text.contains("\\u{206a}"));
}

#[test]
fn native_codec_round_trips_through_json() {
    let original = NativePath::new(PathBuf::from("fixture/굿걸.txt"));
    let encoded = original.encode();
    let json = serde_json::to_string(&encoded).expect("path encoding should serialize");
    let decoded = serde_json::from_str(&json).expect("path encoding should deserialize");
    assert_eq!(
        NativePath::decode(&decoded).expect("path should decode"),
        original
    );
}

#[cfg(unix)]
#[test]
fn unix_codec_preserves_non_utf8_bytes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    let original = NativePath::new(PathBuf::from(OsString::from_vec(vec![b'a', 0xff, b'b'])));
    let encoded = original.encode();
    assert_eq!(
        NativePath::decode(&encoded).expect("path should decode"),
        original
    );
    assert!(original.safe_display().deceptive);
}

#[cfg(windows)]
#[test]
fn windows_codec_and_display_preserve_unpaired_utf16() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt as _;

    let original = OsString::from_wide(&[u16::from(b'a'), 0xd800, u16::from(b'b')]);
    let displayed = safe_display_os_str(&original);
    assert!(displayed.deceptive);
    assert!(displayed.text.contains("\\u{d800}"));

    let path = NativePath::new(std::path::PathBuf::from(original));
    let encoded = path.encode();
    assert_eq!(
        NativePath::decode(&encoded).expect("path should decode"),
        path
    );
}

#[test]
fn resolved_root_retains_request_and_stable_identity() {
    let directory = tempfile::tempdir().expect("temporary root should exist");
    let root = ResolvedRoot::resolve(directory.path().to_path_buf())
        .expect("temporary root should resolve");
    let metadata = std::fs::symlink_metadata(root.resolved.as_path())
        .expect("resolved root metadata should exist");
    let identity = identity_for(root.resolved.as_path(), &metadata)
        .expect("identity lookup should succeed")
        .expect("resolved root is not a link");

    assert_eq!(identity, root.identity);
    assert_eq!(root.requested.as_path(), directory.path());
}

#[cfg(unix)]
#[test]
fn root_symlink_is_resolved_exactly_once() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("temporary parent should exist");
    let target = parent.path().join("target");
    std::fs::create_dir(&target).expect("target directory should exist");
    let requested = parent.path().join("requested");
    symlink(&target, &requested).expect("root symlink should be created");

    let root = ResolvedRoot::resolve(requested.clone()).expect("root symlink should resolve");
    assert_eq!(root.requested.as_path(), requested);
    assert_eq!(
        root.resolved.as_path(),
        target.canonicalize().expect("target should canonicalize")
    );
}
