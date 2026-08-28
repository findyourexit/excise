#![no_main]

use std::ffi::OsString;

use excise::geometry::{FileMetadata, FileType, HALF_ROWS_PER_CELL, TreeMap};
use excise::model::NodeId;
use libfuzzer_sys::fuzz_target;
use ratatui::layout::Rect;

fn u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([
        data.get(offset).copied().unwrap_or(0),
        data.get(offset.saturating_add(1)).copied().unwrap_or(0),
    ])
}

fn fuzz_size(record: &[u8]) -> u128 {
    if record[1] & 0x80 != 0 {
        u128::MAX
    } else {
        u128::from(record[0])
    }
}

fuzz_target!(|data: &[u8]| {
    // Decode the first eight fuzz bytes as independent `u16` components,
    // including zero and `u16::MAX`. A literal preserves them unchanged.
    let area = Rect {
        x: u16_at(data, 0),
        y: u16_at(data, 2),
        width: u16_at(data, 4),
        height: u16_at(data, 6),
    };
    let records = data.get(8..).unwrap_or(&[]);
    let count = (records.len() / 2).min(128);
    let records = &records[..count * 2];
    let total = records
        .chunks_exact(2)
        .fold(0_u128, |sum, record| sum.saturating_add(fuzz_size(record)));
    // Deliberately retain fuzzer order. TreeMap must not require largest-first
    // input to keep a renderable suffix after a tiny entry.
    let files = records
        .chunks_exact(2)
        .enumerate()
        .map(|(index, record)| {
            let size = fuzz_size(record);
            FileMetadata {
                node_id: NodeId(u32::try_from(index).expect("fuzz input caps entry count")),
                name: OsString::from(format!("entry-{index}")),
                size,
                apparent_size: size,
                descendants: None,
                percentage: if total == 0 {
                    1.0 / count.max(1) as f64
                } else {
                    size as f64 / total as f64
                },
                file_type: FileType::File,
                synthetic_kind: None,
                uncertain: record[1] & 1 != 0,
            }
        })
        .collect::<Vec<_>>();
    let mut treemap = TreeMap::new(area);
    treemap.populate_tiles(&files);

    let half_rows_per_cell = u32::from(HALF_ROWS_PER_CELL);
    let top_half_row = u32::from(area.y) * half_rows_per_cell;
    let bottom_half_row =
        (u32::from(area.y) + u32::from(area.height)) * half_rows_per_cell;
    let bottom_row = u32::from(area.y) + u32::from(area.height);
    let area_right = u32::from(area.right());
    for (index, tile) in treemap.tiles.iter().enumerate() {
        let tile_right = u32::from(tile.x) + u32::from(tile.width);
        let tile_bottom = tile.y.saturating_add(tile.height);
        assert!(tile.x >= area.x);
        assert!(tile_right <= area_right);
        assert!(tile.y >= top_half_row);
        assert!(tile_bottom <= bottom_half_row);
        assert!(tile.top_row() >= u32::from(area.y));
        assert!(tile.bottom_row() <= bottom_row);
        for other in &treemap.tiles[index + 1..] {
            let other_right = u32::from(other.x) + u32::from(other.width);
            let other_bottom = other.y.saturating_add(other.height);
            let overlaps = u32::from(tile.x) < other_right
                && u32::from(other.x) < tile_right
                && tile.y < other_bottom
                && other.y < tile_bottom;
            assert!(!overlaps, "tiles overlap: {tile:?} and {other:?}");
        }
    }

    let mut rendered = [false; 128];
    for tile in &treemap.tiles {
        let index = usize::try_from(tile.node_id.0).expect("fuzz node IDs fit usize");
        assert!(index < files.len(), "tile has no matching fuzz input: {tile:?}");
        assert!(!rendered[index], "input rendered twice: {tile:?}");
        rendered[index] = true;
    }
    let expected_entries = files
        .iter()
        .enumerate()
        .filter(|(index, _)| !rendered[*index])
        .count();
    let expected_bytes = files
        .iter()
        .enumerate()
        .filter(|(index, _)| !rendered[*index])
        .fold(0_u128, |sum, (_, file)| sum.saturating_add(file.size));
    let expected_uncertainty = files
        .iter()
        .enumerate()
        .any(|(index, file)| !rendered[index] && file.uncertain);
    let overflow = treemap.overflow();
    match overflow {
        Some(summary) => {
            assert_eq!(summary.entries, expected_entries);
            assert_eq!(summary.bytes, expected_bytes);
            assert_eq!(summary.uncertain, expected_uncertainty);
        }
        None => assert_eq!(expected_entries, 0),
    }
    if !files.is_empty() && (area.width == 0 || area.height == 0) {
        let summary = overflow.expect("zero-sized layout must retain omitted entries");
        assert_eq!(summary.entries, files.len());
        assert_eq!(summary.bytes, expected_bytes);
        assert_eq!(summary.uncertain, expected_uncertainty);
    }
});
