#![no_main]

use std::ffi::OsString;
use std::path::PathBuf;

use excise::deletion::{
    DeletionEntryOutcome, DeletionEntryResult, DeletionReport, PlannedEntry, PlannedKind,
    PlannedSnapshot,
};
use excise::model::{EntrySnapshot, NodeId, NodeKind};
use excise::native_path::NativeIdentity;
use excise::{geometry::FileType, FileToDelete};
use file_id::FileId;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let identity = NativeIdentity {
        file_id: FileId::new_inode(1, 1),
        link_count: Some(1),
        reparse_point: false,
    };
    let entries = data
        .iter()
        .take(256)
        .enumerate()
        .map(|(index, byte)| DeletionEntryResult {
            entry: PlannedEntry {
                relative_path: PathBuf::from(format!("entry-{index}")),
                snapshot: PlannedSnapshot {
                    identity: identity.clone(),
                    kind: PlannedKind::File,
                    apparent_bytes: u128::from(*byte),
                    allocated_bytes: Some(u128::from(*byte)),
                    modified_nanos: Some(u128::from(*byte)),
                },
            },
            outcome: match byte % 5 {
                0 => DeletionEntryOutcome::Deleted,
                1 => DeletionEntryOutcome::Changed("changed".to_string()),
                2 => DeletionEntryOutcome::Missing,
                3 => DeletionEntryOutcome::Failed("failed".to_string()),
                _ => DeletionEntryOutcome::Unattempted,
            },
        })
        .collect::<Vec<_>>();
    let report = DeletionReport {
        target_node_id: NodeId(1),
        root_relative_path: PathBuf::from("target"),
        estimated_bytes: entries.len().saturating_mul(256),
        scan_root: PathBuf::from("root"),
        entries: entries.into(),
        soft_cancelled: false,
        precise: true,
    };
    let classified = report
        .deleted_entries()
        .saturating_add(report.changed_entries())
        .saturating_add(report.missing_entries())
        .saturating_add(report.failed_entries())
        .saturating_add(report.unattempted_entries());
    assert_eq!(classified as usize, report.entries.len());

    let _target = FileToDelete {
        node_id: NodeId(1),
        synthetic: false,
        path_in_filesystem: PathBuf::from("root"),
        path_to_file: vec![OsString::from("target")],
        file_type: FileType::File,
        num_descendants: None,
        size: 0,
        expected_snapshot: EntrySnapshot {
            identity: Some(identity),
            kind: NodeKind::File,
            apparent_bytes: 0,
            allocated_bytes: Some(0),
            modified_nanos: None,
        },
        reviewed_entries: Vec::new(),
    };
});
