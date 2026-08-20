use std::ffi::OsString;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
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

criterion_group!(benches, benchmark_treemap);
criterion_main!(benches);
