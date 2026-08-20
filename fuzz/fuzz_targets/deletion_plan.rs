#![no_main]

use std::error::Error;
use std::ffi::OsString;
use std::fs::Metadata;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::UNIX_EPOCH;

use excise::deletion::{
    DeletionPlanError, PlannedKind, PlannedSnapshot, ReviewedEntry, build_plan_cancellable,
    execute_plan,
};
use excise::model::{EntrySnapshot, NodeId, NodeKind};
use excise::native_path::identity_for;
use excise::{FileToDelete, geometry::FileType};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = exercise(data);
});

fn exercise(data: &[u8]) -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let target_path = root.path().join("target");
    std::fs::create_dir(&target_path)?;
    for (index, byte) in data.iter().copied().take(12).enumerate() {
        let path = target_path.join(format!("entry-{index}-{}", byte % 17));
        if byte % 3 == 0 {
            std::fs::create_dir(&path)?;
            std::fs::write(path.join("leaf"), vec![byte; usize::from(byte % 32)])?;
        } else {
            std::fs::write(path, vec![byte; usize::from(byte % 32)])?;
        }
    }

    let outside = tempfile::tempdir()?;
    let outside_file = outside.path().join("outside");
    std::fs::write(&outside_file, b"outside")?;
    #[cfg(unix)]
    if data.get(1).is_some_and(|byte| byte & 1 != 0) {
        std::os::unix::fs::symlink(&outside_file, target_path.join("outside-link"))?;
    }

    let reviewed_entries = reviewed_entries(root.path(), &target_path)?;
    let root_snapshot = reviewed_entries
        .iter()
        .find(|entry| entry.relative_path == Path::new("target"))
        .ok_or("reviewed target missing")?
        .snapshot
        .clone();
    let target = FileToDelete {
        node_id: NodeId(1),
        synthetic: false,
        path_in_filesystem: root.path().to_path_buf(),
        path_to_file: vec![OsString::from("target")],
        file_type: FileType::Folder,
        num_descendants: Some(
            u64::try_from(reviewed_entries.len().saturating_sub(1)).unwrap_or(u64::MAX),
        ),
        size: 0,
        expected_snapshot: EntrySnapshot {
            identity: Some(root_snapshot.identity.clone()),
            kind: NodeKind::Directory,
            apparent_bytes: root_snapshot.apparent_bytes,
            allocated_bytes: root_snapshot.allocated_bytes,
            modified_nanos: root_snapshot.modified_nanos,
        },
        reviewed_entries,
    };

    let mode = data.first().copied().unwrap_or(0) % 4;
    match mode {
        1 => std::fs::write(target_path.join("late-entry"), b"late")?,
        2 => {
            if let Some(entry) = target
                .reviewed_entries
                .iter()
                .find(|entry| entry.snapshot.kind == PlannedKind::File)
            {
                let path = root.path().join(&entry.relative_path);
                let backup = path.with_extension("reviewed-backup");
                std::fs::rename(&path, backup)?;
                std::fs::write(path, b"replacement")?;
            } else {
                std::fs::write(target_path.join("late-entry"), b"late")?;
            }
        }
        _ => {}
    }

    let maximum_bytes = if mode == 3 { 1 } else { 4 * 1024 * 1024 };
    let result = build_plan_cancellable(
        root.path(),
        target,
        false,
        &AtomicBool::new(false),
        maximum_bytes,
    );
    match (mode, result) {
        (0, Ok(plan)) => {
            let soft = AtomicBool::new(data.get(2).is_some_and(|byte| byte & 1 != 0));
            let hard = AtomicBool::new(data.get(2).is_some_and(|byte| byte & 2 != 0));
            let report = execute_plan(root.path(), plan, &soft, &hard);
            let classified = report
                .deleted_entries()
                .saturating_add(report.changed_entries())
                .saturating_add(report.missing_entries())
                .saturating_add(report.failed_entries())
                .saturating_add(report.unattempted_entries());
            assert_eq!(classified as usize, report.entries.len());
            assert_eq!(report.precise, !hard.load(std::sync::atomic::Ordering::Acquire));
        }
        (1 | 2, Err(DeletionPlanError::Changed)) => {}
        (3, Err(DeletionPlanError::MemoryLimit { limit: 1 })) => {}
        (mode, result) => panic!("unexpected deletion plan result for mode {mode}: {result:?}"),
    }
    assert!(outside_file.exists());
    Ok(())
}

fn reviewed_entries(root: &Path, target: &Path) -> Result<Vec<ReviewedEntry>, Box<dyn Error>> {
    let mut reviewed = Vec::new();
    let mut pending = vec![target.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)?;
        let snapshot = snapshot(&path, &metadata)?;
        if snapshot.kind == PlannedKind::Directory {
            for child in std::fs::read_dir(&path)? {
                pending.push(child?.path());
            }
        }
        reviewed.push(ReviewedEntry {
            relative_path: path.strip_prefix(root)?.to_path_buf(),
            snapshot,
        });
    }
    reviewed.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(reviewed)
}

fn snapshot(path: &Path, metadata: &Metadata) -> Result<PlannedSnapshot, Box<dyn Error>> {
    let kind = if metadata.is_dir() {
        PlannedKind::Directory
    } else if metadata.file_type().is_symlink() {
        PlannedKind::Link
    } else {
        PlannedKind::File
    };
    #[cfg(unix)]
    let allocated_bytes = if kind == PlannedKind::File {
        use std::os::unix::fs::MetadataExt as _;
        Some(u128::from(metadata.blocks()).saturating_mul(512))
    } else {
        None
    };
    #[cfg(not(unix))]
    let allocated_bytes = None;
    Ok(PlannedSnapshot {
        identity: identity_for(path, metadata)?.ok_or("identity unavailable")?,
        kind,
        apparent_bytes: if kind == PlannedKind::Directory {
            0
        } else {
            u128::from(metadata.len())
        },
        allocated_bytes,
        modified_nanos: metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos()),
    })
}
