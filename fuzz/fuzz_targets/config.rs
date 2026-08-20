#![no_main]

use std::path::PathBuf;

use excise::config::{Cli, EnvironmentOverrides, RuntimeConfig, parse_file_config};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let input = String::from_utf8_lossy(data);
    if let Ok(file) = parse_file_config(&input) {
        let cli = Cli {
            folder: None,
            config: None,
            apparent_size: false,
            scan_threads: None,
            event_buffer: None,
            cross_filesystems: false,
            exclusions: Vec::new(),
            memory_mib: None,
            reduced_motion: false,
            theme: None,
            ascii: false,
            mouse: false,
            keymap: None,
            format: None,
            output: None,
            disable_delete_confirmation: false,
        };
        let _ = RuntimeConfig::from_layers(
            cli,
            Some(&file),
            EnvironmentOverrides::default(),
            PathBuf::from("."),
            None,
        );
    }
});
