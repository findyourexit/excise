#![no_main]

use std::path::Path;

use excise::filter::FilterPattern;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let query = String::from_utf8_lossy(data);
    if let Ok(filter) = FilterPattern::new(query.into_owned()) {
        let _ = filter.is_glob();
        let _ = filter.matches_name(Path::new("hostile\nname").as_os_str());
        let _ = filter.matches_path(Path::new("root/deep/file"), Path::new("root"));
    }
});
