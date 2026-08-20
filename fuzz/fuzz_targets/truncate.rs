#![no_main]

use libfuzzer_sys::fuzz_target;
use unicode_width::UnicodeWidthStr;

#[path = "../../src/ui/format/truncate.rs"]
mod truncate;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }

    let max_width = u16::from_le_bytes([data[0], data[1]]);
    let input = String::from_utf8_lossy(&data[2..]);

    let middle = truncate::truncate_middle(&input, max_width);
    assert!(middle.width() <= usize::from(max_width));

    let end = truncate::truncate_end(&input, max_width);
    assert!(end.width() <= usize::from(max_width));
});
