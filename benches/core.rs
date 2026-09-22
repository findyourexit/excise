use std::ffi::OsString;
use std::fs;
use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use excise::benchmark::{
    CanonicalStoreBenchmark, CanonicalStoreMetrics, CanonicalWorkload, FilesystemScanBenchmark,
};
use excise::geometry::{FileMetadata, FileType, TreeMap};
use excise::model::NodeId;
use ratatui::layout::Rect;
use std::time::Duration;

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

const MILLION_TINY_FILES: CanonicalWorkload = CanonicalWorkload::Flat { entries: 1_000_000 };
const MILLION_STORAGE_MIB: usize = 2_048;
const BOUNDED_MILLION_STORAGE_MIB: usize = 8_192;

fn per_observation_milli(bytes: u64, observations: usize) -> u64 {
    let observations = u64::try_from(observations).unwrap_or(u64::MAX);
    bytes
        .saturating_mul(1_000)
        .checked_div(observations)
        .unwrap_or(0)
}

fn report_canonical_metrics(label: &str, metrics: CanonicalStoreMetrics) {
    let logical_read_bytes = metrics
        .merge_read_bytes
        .saturating_add(metrics.reduction_read_bytes)
        .saturating_add(metrics.publication_read_bytes);
    let logical_written_bytes = metrics
        .input_written_bytes
        .saturating_add(metrics.merge_written_bytes)
        .saturating_add(metrics.reduction_written_bytes)
        .saturating_add(metrics.publication_written_bytes);
    let merge_write_amplification_milli = metrics
        .merge_written_bytes
        .saturating_mul(1_000)
        .checked_div(metrics.input_written_bytes)
        .unwrap_or(0);
    eprintln!(
        "{label}: observations={}, logical_read_bytes={}, logical_written_bytes={}, read_bytes_per_observation_milli={}, written_bytes_per_observation_milli={}, merge_write_amplification_milli={}, retained_bytes={}, peak_temporary_bytes={}, ingestion_elapsed_ms={}, publication_elapsed_ms={}, ingestion_cpu_us={:?}, publication_cpu_us={:?}",
        metrics.observations,
        logical_read_bytes,
        logical_written_bytes,
        per_observation_milli(logical_read_bytes, metrics.observations),
        per_observation_milli(logical_written_bytes, metrics.observations),
        merge_write_amplification_milli,
        metrics.retained_bytes,
        metrics.peak_temporary_bytes,
        metrics.ingestion_elapsed.as_millis(),
        metrics.publication_elapsed.as_millis(),
        metrics.ingestion_cpu.map(|duration| duration.as_micros()),
        metrics.publication_cpu.map(|duration| duration.as_micros()),
    );
}

const SCAN_FIXTURE_DIRECTORIES: usize = 128;
const SCAN_FIXTURE_FILES_PER_DIRECTORY: usize = 128;
const FOCUS_REQUESTS: usize = 16;

fn scanner_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("scanner benchmark root should exist");
    for directory_index in 0..SCAN_FIXTURE_DIRECTORIES {
        let directory = root.path().join(format!("directory-{directory_index:04}"));
        fs::create_dir(&directory).expect("scanner benchmark directory should be created");
        for file_index in 0..SCAN_FIXTURE_FILES_PER_DIRECTORY {
            fs::write(directory.join(format!("file-{file_index:04}")), b"x")
                .expect("scanner benchmark file should be written");
        }
    }
    root
}

fn benchmark_scanner(c: &mut Criterion) {
    let fixture = scanner_fixture();
    let scanned_entries =
        SCAN_FIXTURE_DIRECTORIES.saturating_mul(SCAN_FIXTURE_FILES_PER_DIRECTORY.saturating_add(1));
    let mut scan = c.benchmark_group("scanner/filesystem-walk");
    scan.sample_size(10);
    scan.measurement_time(Duration::from_secs(5));
    scan.throughput(Throughput::Elements(
        u64::try_from(scanned_entries).expect("benchmark input should fit u64"),
    ));
    for threads in [1_usize, 2, 8] {
        scan.bench_with_input(
            BenchmarkId::new("workers", threads),
            &threads,
            |bencher, &threads| {
                bencher.iter(|| {
                    black_box(FilesystemScanBenchmark::scan_to_completion(
                        fixture.path(),
                        threads,
                    ))
                });
            },
        );
    }
    scan.finish();

    let focus_paths = (0..FOCUS_REQUESTS)
        .map(|index| fixture.path().join(format!("directory-{index:04}")))
        .collect::<Vec<_>>();
    let mut focus = c.benchmark_group("scanner/focus-delivery");
    focus.sample_size(10);
    focus.measurement_time(Duration::from_secs(5));
    focus.bench_function("16-requests/4-workers", |bencher| {
        bencher.iter_batched_ref(
            || FilesystemScanBenchmark::ready_for_focus(fixture.path(), 4),
            |scan| black_box(scan.focus_latency(&focus_paths)),
            BatchSize::PerIteration,
        );
    });
    focus.finish();

    let mut cancellation = c.benchmark_group("scanner/rebuild-cancellation");
    cancellation.sample_size(10);
    cancellation.measurement_time(Duration::from_secs(5));
    cancellation.bench_function("4-workers", |bencher| {
        bencher.iter_batched_ref(
            || FilesystemScanBenchmark::ready_for_rebuild(fixture.path(), 4),
            |scan| black_box(scan.cancel_rebuild_latency()),
            BatchSize::PerIteration,
        );
    });
    cancellation.finish();
}
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
        query.bench_with_input(
            BenchmarkId::new(workload.label(), workload.entry_count()),
            &workload,
            |bencher, &workload| {
                let mut fixture = CanonicalStoreBenchmark::build(workload);
                bencher.iter(|| black_box(fixture.query_representative_page()));
            },
        );
    }
    query.finish();
}

fn benchmark_million_tiny_files(c: &mut Criterion) {
    if std::env::var_os("EXCISE_BENCH_MILLION").is_none() {
        return;
    }
    let mut publication = c.benchmark_group("scan-store/million-tiny-files/publication");
    publication.sample_size(10);
    publication.warm_up_time(Duration::from_secs(1));
    publication.measurement_time(Duration::from_secs(5));
    publication.throughput(Throughput::Elements(
        u64::try_from(MILLION_TINY_FILES.entry_count()).expect("benchmark input should fit u64"),
    ));
    publication.bench_function("canonical", |bencher| {
        bencher.iter(|| {
            let fixture =
                CanonicalStoreBenchmark::build_premerged(MILLION_TINY_FILES, MILLION_STORAGE_MIB);
            black_box(fixture.retained_bytes())
        });
    });
    publication.finish();

    let mut fixture =
        CanonicalStoreBenchmark::build_premerged(MILLION_TINY_FILES, MILLION_STORAGE_MIB);
    report_canonical_metrics("million-premerged", fixture.metrics());
    let mut query = c.benchmark_group("scan-store/million-tiny-files/page-query");
    query.sample_size(10);
    query.measurement_time(Duration::from_secs(5));
    query.throughput(Throughput::Elements(32));
    query.bench_function("late-page", |bencher| {
        bencher.iter(|| black_box(fixture.query_representative_page()));
    });
    query.finish();

    if std::env::var_os("EXCISE_BENCH_MILLION_FANIN").is_some() {
        let mut fixture = CanonicalStoreBenchmark::build_with_storage_mib(
            MILLION_TINY_FILES,
            BOUNDED_MILLION_STORAGE_MIB,
        );
        report_canonical_metrics("million-bounded-fan-in", fixture.metrics());
        let mut bounded_query =
            c.benchmark_group("scan-store/million-tiny-files/bounded-fan-in/page-query");
        bounded_query.sample_size(10);
        bounded_query.measurement_time(Duration::from_secs(5));
        bounded_query.throughput(Throughput::Elements(32));
        bounded_query.bench_function("late-page", |bencher| {
            bencher.iter(|| black_box(fixture.query_representative_page()));
        });
        bounded_query.finish();
    }
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
    benchmark_canonical_store,
    benchmark_million_tiny_files,
    benchmark_scanner
);
criterion_main!(benches);
