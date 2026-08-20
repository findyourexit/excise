#![no_main]

use std::path::PathBuf;

use excise::native_path::NativePath;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    #[cfg(unix)]
    let path = {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;
        PathBuf::from(OsString::from_vec(data.to_vec()))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(String::from_utf8_lossy(data).into_owned());

    let native = NativePath::new(path);
    let encoded = native.encode();
    assert_eq!(NativePath::decode(&encoded).expect("native encoding must decode"), native);
    let displayed = native.safe_display();
    assert!(!displayed.text.chars().any(char::is_control));
    assert!(!displayed.text.contains('\u{202e}'));
});
