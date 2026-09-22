use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::hint::black_box;
use std::path::{Path, PathBuf};

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use excise::geometry::{FileMetadata, FileType, TreeMap};
use excise::model::{Arena, MIN_PROCESS_MIB, MemoryBudget, NodeId};
use excise::native_path::{NativeIdentity, identity_for};
use ratatui::layout::Rect;

fn files(count: usize) -> Vec<FileMetadata> {
    let count_u32 = u32::try_from(count.max(1)).unwrap_or(u32::MAX);
    let total = f64::from(count_u32) * (f64::from(count_u32) + 1.0) / 2.0;
    (0..count)
        .map(|index| {
            let weight = u32::try_from(count - index).unwrap_or(u32::MAX);
            FileMetadata {
                node_id: NodeId(u32::try_from(index).unwrap_or(u32::MAX)),
                name: OsString::from(format!("entry-{index}")),
                size: u128::from(weight),
                apparent_size: u128::from(weight),
                descendants: None,
                percentage: f64::from(weight) / total,
                file_type: FileType::File,
                synthetic_kind: None,
                uncertain: false,
            }
        })
        .collect()
}

const CAP_TIME_DIRECTORIES: usize = 128;
const CAP_TIME_LEAVES_PER_DIRECTORY: usize = 32;

struct IngestionEntry {
    path: PathBuf,
    metadata: Metadata,
    identity: NativeIdentity,
}

fn add_path(arena: &mut Arena, path: &Path) {
    let metadata =
        fs::symlink_metadata(path).expect("benchmark fixture metadata should be readable");
    let identity = identity_for(path, &metadata)
        .expect("benchmark fixture identity should be readable")
        .expect("benchmark fixture should not contain links");
    arena
        .add_entry(path, &metadata, identity)
        .expect("benchmark fixture entry should be retained");
}

fn populate_cap_time_fixture(root: &Path) {
    for directory_index in 0..CAP_TIME_DIRECTORIES {
        let directory = root.join(format!("directory-{directory_index:03}"));
        fs::create_dir(&directory).expect("benchmark fixture directory should be created");
        for leaf_index in 0..CAP_TIME_LEAVES_PER_DIRECTORY {
            fs::write(directory.join(format!("leaf-{leaf_index:03}")), b"x")
                .expect("benchmark fixture file should be written");
        }
    }
}

fn scan_ingestion_entries(root: &Path) -> Vec<IngestionEntry> {
    let mut entries =
        Vec::with_capacity(CAP_TIME_DIRECTORIES * (CAP_TIME_LEAVES_PER_DIRECTORY + 1));
    for directory_index in 0..CAP_TIME_DIRECTORIES {
        let directory = root.join(format!("directory-{directory_index:03}"));
        let metadata = fs::symlink_metadata(&directory).expect("benchmark directory should exist");
        let identity = identity_for(&directory, &metadata)
            .expect("benchmark directory identity should be readable")
            .expect("benchmark directory should not be a link");
        entries.push(IngestionEntry {
            path: directory.clone(),
            metadata,
            identity,
        });
        for leaf_index in 0..CAP_TIME_LEAVES_PER_DIRECTORY {
            let path = directory.join(format!("leaf-{leaf_index:03}"));
            let metadata = fs::symlink_metadata(&path).expect("benchmark leaf should exist");
            let identity = identity_for(&path, &metadata)
                .expect("benchmark leaf identity should be readable")
                .expect("benchmark leaf should not be a link");
            entries.push(IngestionEntry {
                path,
                metadata,
                identity,
            });
        }
    }
    entries
}

fn cap_time_arena(root: &Path) -> Arena {
    let mut arena = Arena::new(
        root.to_path_buf(),
        MemoryBudget::from_mib(MIN_PROCESS_MIB)
            .expect("benchmark model budget should be available"),
    )
    .expect("benchmark arena should be created");
    for directory_index in 0..CAP_TIME_DIRECTORIES {
        let directory = root.join(format!("directory-{directory_index:03}"));
        add_path(&mut arena, &directory);
        for leaf_index in 0..CAP_TIME_LEAVES_PER_DIRECTORY {
            add_path(&mut arena, &directory.join(format!("leaf-{leaf_index:03}")));
        }
    }
    arena
}

fn compact_all_cold_subtrees(arena: &mut Arena) -> usize {
    let pinned = HashSet::from([arena.root()]);
    let mut compacted = 0;
    while arena
        .aggregate_cold_subtree(&pinned)
        .expect("benchmark compaction should succeed")
    {
        compacted += 1;
    }
    compacted
}

fn benchmark_cap_time_compaction(c: &mut Criterion) {
    let root = tempfile::tempdir().expect("benchmark root should be created");
    populate_cap_time_fixture(root.path());
    let mut group = c.benchmark_group("model/cap-time-compaction");
    group.throughput(Throughput::Elements(
        u64::try_from(CAP_TIME_DIRECTORIES * CAP_TIME_LEAVES_PER_DIRECTORY)
            .expect("benchmark input should fit u64"),
    ));
    group.bench_function("128x32", |bencher| {
        bencher.iter_batched_ref(
            || cap_time_arena(root.path()),
            |arena| {
                black_box((compact_all_cold_subtrees(arena), arena.memory_used()));
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn benchmark_scan_ingestion(c: &mut Criterion) {
    let root = tempfile::tempdir().expect("benchmark root should be created");
    populate_cap_time_fixture(root.path());
    let entries = scan_ingestion_entries(root.path());
    let mut group = c.benchmark_group("model/scan-ingestion");
    group.throughput(Throughput::Elements(
        u64::try_from(entries.len()).expect("benchmark entry count should fit u64"),
    ));
    group.bench_function("128x32", |bencher| {
        bencher.iter_batched_ref(
            || {
                Arena::new(
                    root.path().to_path_buf(),
                    MemoryBudget::from_mib(MIN_PROCESS_MIB)
                        .expect("benchmark model budget should be available"),
                )
                .expect("benchmark arena should be created")
            },
            |arena| {
                for entry in &entries {
                    arena
                        .add_entry(&entry.path, &entry.metadata, entry.identity.clone())
                        .expect("benchmark entry should be retained");
                }
                black_box(arena.memory_used());
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn benchmark_treemap(c: &mut Criterion) {
    let input = files(100_000);
    c.bench_function("treemap/layout/100k/190x48", |bencher| {
        bencher.iter(|| {
            let mut treemap = TreeMap::new(Rect::new(0, 0, 190, 48));
            treemap.populate_tiles(black_box(&input));
            black_box((treemap.tiles.len(), treemap.unrenderable_tile_coordinates));
        });
    });
}

criterion_group!(
    benches,
    benchmark_treemap,
    benchmark_cap_time_compaction,
    benchmark_scan_ingestion
);
criterion_main!(benches);
