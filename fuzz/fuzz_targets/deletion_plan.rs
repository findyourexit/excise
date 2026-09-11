#![no_main]

use std::error::Error;
use std::ffi::OsString;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::UNIX_EPOCH;

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt as _;

use excise::deletion::{
    build_plan_cancellable, execute_plan, DeletionPlanError, PlannedKind, PlannedSnapshot,
    ReviewedEntry,
};
use excise::model::{EntrySnapshot, NodeId, NodeKind};
use excise::native_path::identity_for;
use excise::{geometry::FileType, FileToDelete};
use libfuzzer_sys::fuzz_target;

const FULL_PLAN_LIMIT_BYTES: usize = 4 * 1024 * 1024;
const SPILLED_PLAN_LIMIT_BYTES: usize = 1;
const MAX_DYNAMIC_ENTRIES: usize = 8;

struct FixtureTree {
    directories: Vec<PathBuf>,
    files: Vec<PathBuf>,
}

fuzz_target!(|data: &[u8]| {
    let _ = exercise(data);
});

fn exercise(data: &[u8]) -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let target_path = root.path().join("target");
    let outside = root.path().join("outside");
    std::fs::create_dir(&target_path)?;
    std::fs::create_dir(&outside)?;
    let outside_file = outside.join("must-survive");
    std::fs::write(&outside_file, b"outside")?;

    let fixture = create_fixture(&target_path, data)?;
    #[cfg(unix)]
    let retained_hard_link = add_unix_link_variants(&outside, &fixture, data)?;
    #[cfg(not(unix))]
    let retained_hard_link: Option<PathBuf> = None;

    let reviewed_entries = reviewed_entries(root.path(), &target_path)?;
    let reviewed_count = u64::try_from(reviewed_entries.len()).unwrap_or(u64::MAX);
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

    let mode = byte_at(data, 0) % 4;
    mutate_after_review(mode, &fixture, &outside, data)?;

    let maximum_bytes = if mode == 3 {
        SPILLED_PLAN_LIMIT_BYTES
    } else {
        FULL_PLAN_LIMIT_BYTES
    };
    let result = build_plan_cancellable(
        root.path(),
        target,
        false,
        &AtomicBool::new(false),
        maximum_bytes,
    );
    match (mode, result) {
        (0 | 3, Ok(plan)) => {
            assert_eq!(plan.planned_entries(), reviewed_count);
            let soft = AtomicBool::new(byte_at(data, 2) & 1 != 0);
            let hard = AtomicBool::new(byte_at(data, 2) & 2 != 0);
            let report = execute_plan(root.path(), plan, &soft, &hard);
            let classified = report
                .deleted_entries()
                .saturating_add(report.changed_entries())
                .saturating_add(report.missing_entries())
                .saturating_add(report.failed_entries())
                .saturating_add(report.unattempted_entries());
            assert_eq!(
                classified,
                u64::try_from(report.entries.len()).unwrap_or(u64::MAX)
            );
            assert!(report.reporting_complete());
            assert_eq!(
                report.precise,
                !hard.load(std::sync::atomic::Ordering::Acquire)
            );
        }
        (0 | 3, Err(DeletionPlanError::Changed)) => {
            // Rejecting an inconsistent snapshot is a valid safety outcome.
        }
        (1 | 2, Err(DeletionPlanError::Changed)) => {}
        (mode, result) => panic!("unexpected deletion plan result for mode {mode}: {result:?}"),
    }

    assert_eq!(
        std::fs::read(&outside_file)
            .expect("data outside the selected target must remain readable"),
        b"outside"
    );
    if let Some(retained_hard_link) = retained_hard_link {
        assert!(
            retained_hard_link.is_file(),
            "hard link outside the selected target must survive"
        );
    }
    Ok(())
}

fn create_fixture(target: &Path, data: &[u8]) -> Result<FixtureTree, Box<dyn Error>> {
    let first_directory = target.join(fixture_component(0, data));
    let second_directory =
        first_directory.join(fixture_component(1, data.get(1..).unwrap_or_default()));
    std::fs::create_dir(&first_directory)?;
    std::fs::create_dir(&second_directory)?;

    let anchor = second_directory.join(fixture_component(2, data.get(2..).unwrap_or_default()));
    write_fixture_file(&anchor, byte_at(data, 0))?;

    let mut fixture = FixtureTree {
        directories: vec![target.to_path_buf(), first_directory, second_directory],
        files: vec![anchor],
    };
    for (index, bytes) in data.chunks(4).take(MAX_DYNAMIC_ENTRIES).enumerate() {
        let selector = bytes[0];
        let parent = &fixture.directories[usize::from(selector) % fixture.directories.len()];
        let path = parent.join(fixture_component(index + 3, bytes));
        if selector & 1 == 0 {
            std::fs::create_dir(&path)?;
            let leaf = path.join(fixture_component(index + 3 + MAX_DYNAMIC_ENTRIES, bytes));
            write_fixture_file(&leaf, selector)?;
            fixture.files.push(leaf);
            fixture.directories.push(path);
        } else {
            write_fixture_file(&path, selector)?;
            fixture.files.push(path);
        }
    }
    Ok(fixture)
}

fn write_fixture_file(path: &Path, selector: u8) -> std::io::Result<()> {
    std::fs::write(path, vec![selector; usize::from(selector % 32)])
}

fn fixture_component(index: usize, data: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        let mut name = format!("entry-{index:02x}-").into_bytes();
        let hostile: &[u8] = match byte_at(data, 0) % 6 {
            0 => b"\xff",
            1 => b"\x1b[31m",
            2 => b"\n",
            3 => b"\xe2\x80\xae",
            4 => b" leading-",
            _ => b"--",
        };
        name.extend_from_slice(hostile);
        name.extend(data.iter().copied().take(8).map(|byte| match byte {
            b'\0' | b'/' => b'_',
            _ => byte,
        }));
        OsString::from_vec(name)
    }
    #[cfg(not(unix))]
    {
        OsString::from(format!("entry-{index:02x}-{:02x}", byte_at(data, 0)))
    }
}

#[cfg(unix)]
fn add_unix_link_variants(
    outside: &Path,
    fixture: &FixtureTree,
    data: &[u8],
) -> Result<Option<PathBuf>, Box<dyn Error>> {
    let options = byte_at(data, 1);
    let retained_hard_link = if options & 1 != 0 {
        let source = &fixture.files[usize::from(byte_at(data, 3)) % fixture.files.len()];
        let inside = fixture.directories[usize::from(byte_at(data, 4)) % fixture.directories.len()]
            .join(fixture_component(240, data.get(4..).unwrap_or_default()));
        std::fs::hard_link(source, inside)?;
        let retained = outside.join("retained-hard-link");
        std::fs::hard_link(source, &retained)?;
        Some(retained)
    } else {
        None
    };
    if options & 2 != 0 {
        let link = fixture.directories[usize::from(byte_at(data, 5)) % fixture.directories.len()]
            .join(fixture_component(241, data.get(5..).unwrap_or_default()));
        std::os::unix::fs::symlink(outside, link)?;
    }
    Ok(retained_hard_link)
}

fn mutate_after_review(
    mode: u8,
    fixture: &FixtureTree,
    outside: &Path,
    data: &[u8],
) -> Result<(), Box<dyn Error>> {
    match mode {
        1 => {
            let parent =
                &fixture.directories[usize::from(byte_at(data, 3)) % fixture.directories.len()];
            std::fs::write(
                parent.join(fixture_component(242, data.get(6..).unwrap_or_default())),
                b"late",
            )?;
        }
        2 => {
            let replacement = if byte_at(data, 2) & 1 != 0 {
                fixture.directories
                    [1 + usize::from(byte_at(data, 3)) % (fixture.directories.len() - 1)]
                    .clone()
            } else {
                fixture.files[usize::from(byte_at(data, 3)) % fixture.files.len()].clone()
            };
            let backup = replacement
                .with_file_name(fixture_component(243, data.get(7..).unwrap_or_default()));
            std::fs::rename(&replacement, backup)?;
            #[cfg(unix)]
            {
                if byte_at(data, 2) & 2 != 0 {
                    std::os::unix::fs::symlink(outside, &replacement)?;
                } else {
                    std::fs::write(&replacement, b"replacement")?;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = outside;
                std::fs::write(&replacement, b"replacement")?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn byte_at(data: &[u8], index: usize) -> u8 {
    data.get(index).copied().unwrap_or_default()
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
