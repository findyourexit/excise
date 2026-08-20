#![no_main]

use std::ffi::OsString;

use excise::geometry::{FileMetadata, FileType, TreeMap};
use excise::model::NodeId;
use libfuzzer_sys::fuzz_target;
use ratatui::layout::Rect;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let width = u16::from(data[0]).max(1);
    let height = u16::from(data[1]).max(1);
    let count = data.len().saturating_sub(2).min(128);
    let total = data[2..2 + count]
        .iter()
        .fold(0_u128, |sum, byte| sum.saturating_add(u128::from(*byte)));
    let files = data[2..2 + count]
        .iter()
        .enumerate()
        .map(|(index, byte)| FileMetadata {
            node_id: NodeId(u32::try_from(index).unwrap_or(u32::MAX)),
            name: OsString::from(format!("entry-{index}")),
            size: u128::from(*byte),
            apparent_size: u128::from(*byte),
            descendants: None,
            percentage: if total == 0 {
                0.0
            } else {
                f64::from(*byte) / total as f64
            },
            file_type: FileType::File,
            synthetic_kind: None,
            uncertain: false,
        })
        .collect::<Vec<_>>();
    let area = Rect::new(0, 0, width, height);
    let mut treemap = TreeMap::new(area);
    treemap.populate_tiles(&files);
    for (index, tile) in treemap.tiles.iter().enumerate() {
        assert!(tile.x.saturating_add(tile.width) <= area.right());
        assert!(tile.y.saturating_add(tile.height) <= area.bottom());
        for other in &treemap.tiles[index + 1..] {
            let overlaps = tile.x < other.x.saturating_add(other.width)
                && other.x < tile.x.saturating_add(tile.width)
                && tile.y < other.y.saturating_add(other.height)
                && other.y < tile.y.saturating_add(tile.height);
            assert!(!overlaps);
        }
    }
});
