#![no_main]

use excise::report::{DeletionHistoryDocument, ScanReportDocument};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(report) = serde_json::from_slice::<ScanReportDocument>(data) {
        let encoded = serde_json::to_vec(&report).expect("accepted scan report should serialize");
        let decoded: ScanReportDocument =
            serde_json::from_slice(&encoded).expect("serialized scan report should parse");
        assert_eq!(decoded, report);
    }
    if let Ok(history) = serde_json::from_slice::<DeletionHistoryDocument>(data) {
        let encoded =
            serde_json::to_vec(&history).expect("accepted deletion history should serialize");
        let decoded: DeletionHistoryDocument =
            serde_json::from_slice(&encoded).expect("serialized deletion history should parse");
        assert_eq!(decoded, history);
    }
});
