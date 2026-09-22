use std::ffi::OsString;
use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use excise::benchmark::{CanonicalStoreBenchmark, CanonicalWorkload};
use excise::geometry::{FileMetadata, FileType, TreeMap};
use excise::model::NodeId;
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

const CANONICAL_WORKLOADS: [CanonicalWorkload; 5] = [
    CanonicalWorkload::Flat { entries: 1_024 },
    CanonicalWorkload::Flat { entries: 16_384 },
    CanonicalWorkload::Fanout {
        directories: 256,
        leaves_per_directory: 32,
    },
    CanonicalWorkload::Deep {
        depth: 256,
        leaves: 256,
    },
    CanonicalWorkload::SharedLinks {
        groups: 2_048,
        links_per_group: 2,
    },
];

fn benchmark_canonical_store(c: &mut Criterion) {
    let mut publication = c.benchmark_group("scan-store/publication");
    for workload in CANONICAL_WORKLOADS {
        publication.throughput(Throughput::Elements(
            u64::try_from(workload.entry_count()).expect("benchmark input should fit u64"),
        ));
        publication.bench_with_input(
            BenchmarkId::new(workload.label(), workload.entry_count()),
            &workload,
            |bencher, &workload| {
                bencher.iter_batched(
                    || (),
                    |()| {
                        let fixture = CanonicalStoreBenchmark::build(workload);
                        black_box(fixture.retained_bytes())
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }
    publication.finish();

    let mut query = c.benchmark_group("scan-store/page-query");
    query.throughput(Throughput::Elements(32));
    for workload in CANONICAL_WORKLOADS {
        let mut fixture = CanonicalStoreBenchmark::build(workload);
        query.bench_with_input(
            BenchmarkId::new(workload.label(), workload.entry_count()),
            &workload,
            |bencher, _| bencher.iter(|| black_box(fixture.query_representative_page())),
        );
    }
    query.finish();
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

criterion_group!(benches, benchmark_treemap, benchmark_canonical_store);
criterion_main!(benches);
