#![no_main]

use excise::fuzz::scan_store::reduce_identity_bytes;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let contributions = reduce_identity_bytes(data);
    assert!(contributions <= data.chunks(5).take(64).count());
});
