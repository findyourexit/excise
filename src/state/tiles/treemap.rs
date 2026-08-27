use ratatui::layout::Rect;

use crate::state::tiles::{FileMetadata, HALF_ROWS_PER_CELL, MapOverflow, RectFloat, Tile};

/// Columns that visually match one half-row of vertical extent.
///
/// A terminal cell is roughly two and a half times taller than it is wide, so a
/// half-row — the unit the map lays out in — is worth about 1.25 columns.
const HEIGHT_WIDTH_RATIO: f64 = 1.25;
/// Half-rows, i.e. one full terminal row plus the label row beneath it.
const MINIMUM_HEIGHT: u32 = 4;
const MINIMUM_WIDTH: u16 = 8;
const HALF_ROWS_PER_CELL_U32: u32 = HALF_ROWS_PER_CELL as u32;

const fn half_rows(rows: u16) -> u32 {
    (rows as u32) * HALF_ROWS_PER_CELL_U32
}

fn area_bottom_rows(area: Rect) -> u32 {
    u32::from(area.y) + u32::from(area.height)
}

fn area_bottom_half_rows(area: Rect) -> u32 {
    half_rows(area.y) + half_rows(area.height)
}

fn rounded_coordinate(value: f64, maximum: f64) -> f64 {
    if value.is_finite() {
        (value + 1.0e-9).round().clamp(0.0, maximum)
    } else {
        0.0
    }
}

fn terminal_coordinate(value: f64) -> u16 {
    rounded_coordinate(value, f64::from(u16::MAX)) as u16
}

fn half_row_coordinate(value: f64) -> u32 {
    rounded_coordinate(value, f64::from(u32::MAX)) as u32
}

pub struct TreeMap {
    pub tiles: Vec<Tile>,
    /// This records the overflow summary anchor as `(terminal_column, half_row)`.
    ///
    /// The second component uses half-rows because the renderer colours each
    /// terminal row independently above and below its midpoint. It is `u32` so
    /// the full public `Rect` coordinate range survives the conversion.
    pub unrenderable_tile_coordinates: Option<(u16, u32)>,
    overflow: Option<MapOverflow>,
    empty_space: RectFloat,
    area: Rect,
    total_size: f64,
}

#[derive(Clone, Copy, Default)]
struct RowMetrics {
    sum: f64,
    minimum: f64,
    maximum: f64,
}

impl RowMetrics {
    fn with(self, size: f64) -> Self {
        if self.sum == 0.0 {
            Self {
                sum: size,
                minimum: size,
                maximum: size,
            }
        } else {
            Self {
                sum: self.sum + size,
                minimum: self.minimum.min(size),
                maximum: self.maximum.max(size),
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct OverflowAccounting {
    entries: usize,
    bytes: u128,
    uncertain: bool,
    /// Number of rendered tiles preceding the first omitted input entry.
    ///
    /// A summary field may remove earlier tiles to make room, but it must never
    /// erase a later renderable entry just because an earlier sibling was tiny.
    first_omission_after: Option<usize>,
}

impl OverflowAccounting {
    fn add_file(&mut self, file: &FileMetadata, rendered_before: usize) {
        self.entries = self.entries.saturating_add(1);
        self.bytes = self.bytes.saturating_add(file.size);
        self.uncertain |= file.uncertain;
        if self.first_omission_after.is_none() {
            self.first_omission_after = Some(rendered_before);
        }
    }

    fn add_tile(&mut self, tile: &Tile) {
        self.entries = self.entries.saturating_add(1);
        self.bytes = self.bytes.saturating_add(tile.size);
        self.uncertain |= tile.uncertain;
    }

    fn merge(&mut self, other: Self) {
        self.entries = self.entries.saturating_add(other.entries);
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.uncertain |= other.uncertain;
    }
}

impl TreeMap {
    #[must_use]
    pub fn new(area: Rect) -> Self {
        let empty_space = RectFloat {
            x: f64::from(area.x),
            y: f64::from(half_rows(area.y)),
            width: f64::from(area.width),
            height: f64::from(half_rows(area.height)),
        };
        let total_size = empty_space.height * empty_space.width;
        Self {
            tiles: Vec::new(),
            unrenderable_tile_coordinates: None,
            overflow: None,
            empty_space,
            area,
            total_size,
        }
    }

    pub fn populate_tiles(&mut self, children: &[FileMetadata]) {
        let mut omitted = OverflowAccounting::default();
        self.squarify(children, &mut omitted);
        if omitted.entries == 0 {
            self.overflow = None;
            return;
        }

        let fallback = (
            terminal_coordinate(self.empty_space.x),
            half_row_coordinate(self.empty_space.y),
        );
        let (raw_x, raw_y) = self.unrenderable_tile_coordinates.unwrap_or(fallback);
        if self.unrenderable_tile_coordinates.is_none() {
            self.unrenderable_tile_coordinates = Some((raw_x, raw_y));
        }

        let (x, y) = if let Some((x, y)) = self.reserve_overflow_region(raw_x, raw_y, &mut omitted)
        {
            self.unrenderable_tile_coordinates = Some((x, y));
            (x, y)
        } else {
            // Keep both public anchor fields non-drawable when the summary would
            // overlap retained geometry or there is no drawable cell.
            let fallback = (raw_x, area_bottom_half_rows(self.area));
            self.unrenderable_tile_coordinates = Some(fallback);
            fallback
        };

        self.overflow = Some(MapOverflow {
            x,
            y,
            entries: omitted.entries,
            bytes: omitted.bytes,
            uncertain: omitted.uncertain,
        });
    }

    /// Returns the entries that do not fit in the laid-out map.
    ///
    /// The summary starts at a terminal column and a half-row, matching the
    /// coordinates used by `Tile` so a renderer can preserve the map's vertical
    /// precision. It remains available even when this layout has no drawable
    /// summary cell.
    #[must_use]
    pub const fn overflow(&self) -> Option<MapOverflow> {
        self.overflow
    }

    fn reserve_overflow_region(
        &mut self,
        raw_x: u16,
        raw_y: u32,
        omitted: &mut OverflowAccounting,
    ) -> Option<(u16, u32)> {
        let last_column = self.area.right().checked_sub(1)?;
        let last_row = area_bottom_rows(self.area).checked_sub(1)?;
        if last_column < self.area.x || last_row < u32::from(self.area.y) {
            return None;
        }

        let x = raw_x.clamp(self.area.x, last_column);
        let row = raw_y
            .div_ceil(HALF_ROWS_PER_CELL_U32)
            .clamp(u32::from(self.area.y), last_row);
        let y = row * HALF_ROWS_PER_CELL_U32;
        let area = self.area;

        // The renderer expands this anchor into a lower-right terminal-cell
        // rectangle. If that would cover a tile laid out after the first omitted
        // input, leave the summary logically present but non-drawable instead.
        // An early tiny entry must not erase a later renderable sibling.
        let protected_start = omitted.first_omission_after.unwrap_or(self.tiles.len());
        if self
            .tiles
            .iter()
            .skip(protected_start)
            .any(|tile| Self::crosses_overflow_region(tile, area, x, y))
        {
            return None;
        }

        // Carving the summary out of a partially intersecting tile would erase
        // geometry that lies outside the summary field. Keep the logical
        // overflow accounting, but make the field non-drawable in that case.
        if self.tiles.iter().any(|tile| {
            Self::crosses_overflow_region(tile, area, x, y)
                && !Self::fits_overflow_region(tile, area, x, y)
        }) {
            return None;
        }

        let mut reserved = OverflowAccounting::default();
        self.tiles.retain(|tile| {
            if Self::crosses_overflow_region(tile, area, x, y) {
                reserved.add_tile(tile);
                false
            } else {
                true
            }
        });
        omitted.merge(reserved);
        Some((x, y))
    }

    fn fits_overflow_region(tile: &Tile, area: Rect, x: u16, y: u32) -> bool {
        tile.x >= x
            && tile.y >= y
            && tile.x.saturating_add(tile.width) <= area.right()
            && tile.y.saturating_add(tile.height) <= area_bottom_half_rows(area)
    }

    fn crosses_overflow_region(tile: &Tile, area: Rect, x: u16, y: u32) -> bool {
        tile.x < area.right()
            && x < tile.x.saturating_add(tile.width)
            && tile.y < area_bottom_half_rows(area)
            && y < tile.y.saturating_add(tile.height)
    }

    fn layout_row(&mut self, row: &[FileMetadata], omitted: &mut OverflowAccounting) {
        if row.is_empty() {
            return;
        }
        let row_total = row
            .iter()
            .fold(0.0, |total, file| total + file.percentage * self.total_size);
        if !row_total.is_finite() || row_total <= 0.0 {
            self.add_unrenderable_row(row, omitted);
            return;
        }

        let horizontal = self.empty_space.width <= self.empty_space.height * HEIGHT_WIDTH_RATIO;
        let first_extent = if horizontal {
            self.empty_space.width
        } else {
            self.empty_space.height
        };
        if !first_extent.is_finite() || first_extent <= 0.0 {
            self.add_unrenderable_row(row, omitted);
            return;
        }
        let second_extent = row_total / first_extent;
        if !second_extent.is_finite() || second_extent <= 0.0 {
            self.add_unrenderable_row(row, omitted);
            return;
        }

        let first_origin = if horizontal {
            self.empty_space.x
        } else {
            self.empty_space.y
        };
        let first_limit = first_origin + first_extent;
        let mut first_start = first_origin;
        let mut cumulative = 0.0;
        for (index, file) in row.iter().enumerate() {
            cumulative += file.percentage * self.total_size;
            let first_end = if index + 1 == row.len() {
                first_limit
            } else {
                (first_origin + first_extent * cumulative / row_total)
                    .clamp(first_start, first_limit)
            };
            let rect = if horizontal {
                RectFloat {
                    x: first_start,
                    y: self.empty_space.y,
                    width: first_end - first_start,
                    height: second_extent.min(self.empty_space.height),
                }
            } else {
                RectFloat {
                    x: self.empty_space.x,
                    y: first_start,
                    width: second_extent.min(self.empty_space.width),
                    height: first_end - first_start,
                }
            };
            first_start = first_end;

            // `Tile::new` clones the file name, so round and reject undersized
            // geometry before materializing metadata for an omitted entry.
            let x = terminal_coordinate(rect.x);
            let right = terminal_coordinate(rect.x + rect.width.max(0.0)).max(x);
            let y = half_row_coordinate(rect.y);
            let bottom = half_row_coordinate(rect.y + rect.height.max(0.0)).max(y);
            let width = right.saturating_sub(x);
            let height = bottom.saturating_sub(y);
            if height < MINIMUM_HEIGHT || width < MINIMUM_WIDTH {
                self.add_unrenderable_tile(x, y, file, omitted);
            } else {
                self.tiles.push(Tile::new(&rect, file));
            }
        }

        if horizontal {
            let consumed = second_extent.min(self.empty_space.height);
            self.empty_space.height = (self.empty_space.height - consumed).max(0.0);
            self.empty_space.y += consumed;
        } else {
            let consumed = second_extent.min(self.empty_space.width);
            self.empty_space.width = (self.empty_space.width - consumed).max(0.0);
            self.empty_space.x += consumed;
        }
    }

    fn add_unrenderable_row(&mut self, row: &[FileMetadata], omitted: &mut OverflowAccounting) {
        self.mark_remaining_unrenderable();
        let rendered_before = self.tiles.len();
        for file in row {
            omitted.add_file(file, rendered_before);
        }
    }

    fn mark_remaining_unrenderable(&mut self) {
        if self.unrenderable_tile_coordinates.is_none() {
            self.unrenderable_tile_coordinates = Some((
                terminal_coordinate(self.empty_space.x),
                half_row_coordinate(self.empty_space.y),
            ));
        }
    }

    fn add_unrenderable_tile(
        &mut self,
        x: u16,
        y: u32,
        file: &FileMetadata,
        omitted: &mut OverflowAccounting,
    ) {
        if self.unrenderable_tile_coordinates.is_none() {
            self.unrenderable_tile_coordinates = Some((x, y));
        }
        omitted.add_file(file, self.tiles.len());
    }

    fn row_shape(&self) -> (f64, f64, f64) {
        if self.empty_space.height * HEIGHT_WIDTH_RATIO < self.empty_space.width {
            (
                self.empty_space.height * HEIGHT_WIDTH_RATIO,
                f64::from(MINIMUM_HEIGHT) * HEIGHT_WIDTH_RATIO,
                f64::from(MINIMUM_WIDTH) / HEIGHT_WIDTH_RATIO,
            )
        } else {
            (
                self.empty_space.width / HEIGHT_WIDTH_RATIO,
                f64::from(MINIMUM_WIDTH) / HEIGHT_WIDTH_RATIO,
                f64::from(MINIMUM_HEIGHT) * HEIGHT_WIDTH_RATIO,
            )
        }
    }

    fn worst_renderable_ratio(
        metrics: RowMetrics,
        length: f64,
        minimum_first: f64,
        minimum_second: f64,
    ) -> Option<f64> {
        if metrics.sum <= 0.0 || metrics.minimum <= 0.0 || length <= 0.0 {
            return None;
        }
        let second = metrics.sum / length;
        let smallest_first = metrics.minimum / second;
        if !second.is_finite()
            || !smallest_first.is_finite()
            || second < minimum_second
            || smallest_first < minimum_first
        {
            return None;
        }
        let largest_first = metrics.maximum / second;
        let smallest_ratio = smallest_first.min(second) / smallest_first.max(second);
        let largest_ratio = largest_first.min(second) / largest_first.max(second);
        Some(smallest_ratio.min(largest_ratio))
    }

    fn squarify(&mut self, children: &[FileMetadata], omitted: &mut OverflowAccounting) {
        let mut row_start = 0;
        let mut row_end = 0;
        let mut metrics = RowMetrics::default();

        while row_end < children.len() {
            let (length, minimum_first, minimum_second) = self.row_shape();
            let size = children[row_end].percentage * self.total_size;
            let candidate = metrics.with(size);
            let current_ratio =
                Self::worst_renderable_ratio(metrics, length, minimum_first, minimum_second);
            let candidate_ratio =
                Self::worst_renderable_ratio(candidate, length, minimum_first, minimum_second);
            let add = match (current_ratio, candidate_ratio) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(current), Some(next)) => next > current,
            };
            if add {
                metrics = candidate;
                row_end += 1;
            } else {
                self.layout_row(&children[row_start..row_end], omitted);
                row_start = row_end;
                metrics = RowMetrics::default();
            }
        }
        self.layout_row(&children[row_start..row_end], omitted);
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::model::NodeId;
    use crate::state::tiles::FileType;

    fn files(count: usize) -> Vec<FileMetadata> {
        let total = (count.max(1) * (count.max(1) + 1) / 2) as f64;
        (0..count)
            .map(|index| {
                let weight = (count - index) as f64;
                FileMetadata {
                    node_id: NodeId(u32::try_from(index).expect("test node ID should fit")),
                    name: OsString::from(format!("entry-{index}")),
                    size: weight as u128,
                    apparent_size: weight as u128,
                    descendants: None,
                    percentage: weight / total,
                    file_type: FileType::File,
                    synthetic_kind: None,
                    uncertain: false,
                }
            })
            .collect()
    }

    fn file(index: u32, size: u128, percentage: f64) -> FileMetadata {
        FileMetadata {
            node_id: NodeId(index),
            name: OsString::from(format!("entry-{index}")),
            size,
            apparent_size: size,
            descendants: None,
            percentage,
            file_type: FileType::File,
            synthetic_kind: None,
            uncertain: false,
        }
    }

    #[test]
    fn geometry_is_finite_in_bounds_and_non_overlapping() {
        let area = Rect::new(3, 5, 190, 48);
        let files = files(10_000);
        let mut treemap = TreeMap::new(area);
        treemap.populate_tiles(&files);

        for (index, tile) in treemap.tiles.iter().enumerate() {
            assert!(tile.width >= MINIMUM_WIDTH);
            assert!(tile.height >= MINIMUM_HEIGHT);
            assert!(tile.x >= area.x && tile.y >= half_rows(area.y));
            assert!(tile.x.saturating_add(tile.width) <= area.right());
            assert!(tile.y.saturating_add(tile.height) <= area_bottom_half_rows(area));
            for other in &treemap.tiles[index + 1..] {
                let overlaps = tile.x < other.x.saturating_add(other.width)
                    && other.x < tile.x.saturating_add(tile.width)
                    && tile.y < other.y.saturating_add(other.height)
                    && other.y < tile.y.saturating_add(tile.height);
                assert!(!overlaps, "tiles overlap: {tile:?} and {other:?}");
            }
        }
    }

    #[test]
    fn adjacent_fractional_boundaries_round_without_overlap() {
        let weights = [
            192_u8, 128, 194, 128, 194, 128, 194, 128, 66, 128, 194, 128, 0, 0, 0, 0, 0, 0, 50, 0,
            0, 226, 128, 226, 128, 194, 128, 194, 128, 194, 128, 72, 194,
        ];
        let total = weights.iter().map(|weight| u32::from(*weight)).sum::<u32>();
        let files = weights
            .iter()
            .enumerate()
            .map(|(index, weight)| FileMetadata {
                node_id: NodeId(u32::try_from(index).expect("test node ID should fit")),
                name: OsString::from(format!("entry-{index}")),
                size: u128::from(*weight),
                apparent_size: u128::from(*weight),
                descendants: None,
                percentage: f64::from(*weight) / f64::from(total),
                file_type: FileType::File,
                synthetic_kind: None,
                uncertain: false,
            })
            .collect::<Vec<_>>();
        let mut treemap = TreeMap::new(Rect::new(0, 0, 195, 128));
        treemap.populate_tiles(&files);

        for (index, tile) in treemap.tiles.iter().enumerate() {
            for other in &treemap.tiles[index + 1..] {
                let overlaps = tile.x < other.x.saturating_add(other.width)
                    && other.x < tile.x.saturating_add(tile.width)
                    && tile.y < other.y.saturating_add(other.height)
                    && other.y < tile.y.saturating_add(tile.height);
                assert!(!overlaps, "tiles overlap: {tile:?} and {other:?}");
            }
        }
    }

    #[test]
    fn zero_size_entries_become_overflow() {
        let mut files = files(2);
        for file in &mut files {
            file.size = 0;
            file.percentage = 0.0;
        }
        let mut treemap = TreeMap::new(Rect::new(0, 0, 80, 24));
        treemap.populate_tiles(&files);
        assert!(treemap.tiles.is_empty());
        assert_eq!(treemap.unrenderable_tile_coordinates, Some((0, 0)));
        assert_eq!(
            treemap.overflow(),
            Some(MapOverflow {
                x: 0,
                y: 0,
                entries: 2,
                bytes: 0,
                uncertain: false,
            })
        );
    }

    #[test]
    fn zero_size_omission_keeps_the_partial_tile_and_accounting() {
        let area = Rect::new(
            0,
            0,
            MINIMUM_WIDTH,
            u16::try_from(MINIMUM_HEIGHT / HALF_ROWS_PER_CELL_U32)
                .expect("minimum height should fit in terminal rows"),
        );
        let children = [file(0, 1, 1.0), file(1, 0, 0.0)];
        let mut treemap = TreeMap::new(area);
        treemap.populate_tiles(&children);

        let Some(overflow) = treemap.overflow() else {
            panic!("zero-size entry should overflow");
        };
        assert_eq!(
            treemap
                .tiles
                .iter()
                .map(|tile| tile.node_id)
                .collect::<Vec<_>>(),
            vec![NodeId(0)],
            "the summary must not erase a tile it only partially intersects"
        );
        assert_eq!(overflow.entries, 1);
        assert_eq!(overflow.bytes, 0);
        assert!(
            overflow.x >= area.right()
                || overflow.y.div_ceil(HALF_ROWS_PER_CELL_U32) >= area_bottom_rows(area)
        );
        assert_eq!(
            treemap.unrenderable_tile_coordinates,
            Some((overflow.x, overflow.y))
        );
    }

    #[test]
    fn overflow_bytes_sum_only_omitted_entries_after_rendered_total_saturates() {
        let children = [file(0, u128::MAX, 0.5), file(1, 1, 0.0)];
        let mut treemap = TreeMap::new(Rect::new(0, 0, 16, 4));
        treemap.populate_tiles(&children);

        assert_eq!(treemap.tiles.len(), 1);
        assert_eq!(
            treemap.overflow(),
            Some(MapOverflow {
                x: 8,
                y: 0,
                entries: 1,
                bytes: 1,
                uncertain: false,
            })
        );
    }

    #[test]
    fn full_range_rect_vertical_geometry_remains_exact_in_half_rows() {
        let area = Rect {
            x: 7,
            y: 50_000,
            width: 80,
            height: 50_000,
        };
        let mut treemap = TreeMap::new(area);
        treemap.populate_tiles(&[file(0, 1, 1.0)]);

        let [tile] = treemap.tiles.as_slice() else {
            panic!("full-height entry should remain renderable");
        };
        assert_eq!(tile.y, 100_000);
        assert_eq!(tile.height, 100_000);
        assert_eq!(tile.top_row(), 50_000);
        assert_eq!(tile.bottom_row(), 100_000);
        assert_eq!(tile.y + tile.height, area_bottom_half_rows(area));
    }

    #[test]
    fn zero_extent_layouts_keep_overflow_accounting_without_a_drawable_anchor() {
        for area in [Rect::new(7, 11, 0, 24), Rect::new(7, 11, 80, 0)] {
            let mut uncertain = file(1, 5, 0.5);
            uncertain.uncertain = true;
            let children = [file(0, 3, 0.5), uncertain];
            let mut treemap = TreeMap::new(area);
            treemap.populate_tiles(&children);

            assert!(treemap.tiles.is_empty());
            assert_eq!(
                treemap.unrenderable_tile_coordinates,
                Some((area.x, area_bottom_half_rows(area)))
            );
            assert_eq!(
                treemap.overflow(),
                Some(MapOverflow {
                    x: area.x,
                    y: area_bottom_half_rows(area),
                    entries: 2,
                    bytes: 8,
                    uncertain: true,
                })
            );
        }
    }

    #[test]
    fn early_below_minimum_entry_keeps_a_later_renderable_entry() {
        let children = [file(0, 1, 0.0001), file(1, 1_000_000, 0.9999)];
        let area = Rect::new(0, 0, 80, 24);
        let mut treemap = TreeMap::new(area);
        treemap.populate_tiles(&children);

        assert_eq!(
            treemap
                .tiles
                .iter()
                .map(|tile| tile.node_id)
                .collect::<Vec<_>>(),
            vec![NodeId(1)]
        );
        assert_eq!(
            treemap.unrenderable_tile_coordinates,
            Some((0, area_bottom_half_rows(area)))
        );
        assert_eq!(
            treemap.overflow(),
            Some(MapOverflow {
                x: 0,
                y: area_bottom_half_rows(area),
                entries: 1,
                bytes: 1,
                uncertain: false,
            })
        );
    }

    #[test]
    fn late_below_minimum_entry_does_not_erase_a_large_tile() {
        let children = [file(0, 999_900, 0.9999), file(1, 100, 0.0001)];
        let area = Rect::new(0, 0, 80, 24);
        let mut treemap = TreeMap::new(area);
        treemap.populate_tiles(&children);

        assert_eq!(
            treemap
                .tiles
                .iter()
                .map(|tile| tile.node_id)
                .collect::<Vec<_>>(),
            vec![NodeId(0)],
            "an overflow field must not erase a tile it only partially intersects"
        );
        assert_eq!(treemap.overflow().map(|overflow| overflow.entries), Some(1));
        assert_eq!(treemap.overflow().map(|overflow| overflow.bytes), Some(100));
    }

    #[test]
    fn many_tiny_entries_preserve_a_later_renderable_sibling_and_overflow() {
        const OMITTED: usize = 1_024;
        const TINY_PERCENTAGE: f64 = 1.0e-9;

        let mut children = (0..OMITTED)
            .map(|index| {
                file(
                    u32::try_from(index).expect("test node ID should fit"),
                    1,
                    TINY_PERCENTAGE,
                )
            })
            .collect::<Vec<_>>();
        children.push(file(
            u32::try_from(OMITTED).expect("test node ID should fit"),
            1_000_000,
            1.0 - TINY_PERCENTAGE * OMITTED as f64,
        ));
        let area = Rect::new(0, 0, 80, 24);
        let mut treemap = TreeMap::new(area);
        treemap.populate_tiles(&children);

        assert_eq!(
            treemap
                .tiles
                .iter()
                .map(|tile| tile.node_id)
                .collect::<Vec<_>>(),
            vec![NodeId(
                u32::try_from(OMITTED).expect("test node ID should fit")
            )]
        );
        assert_eq!(
            treemap.overflow(),
            Some(MapOverflow {
                x: 0,
                y: area_bottom_half_rows(area),
                entries: OMITTED,
                bytes: OMITTED as u128,
                uncertain: false,
            })
        );
    }

    #[test]
    fn overflow_marks_uncertain_omitted_metadata_as_a_lower_bound() {
        let mut uncertain = file(1, 6, 0.0);
        uncertain.uncertain = true;
        let children = [file(0, 9, 0.5), uncertain];
        let mut treemap = TreeMap::new(Rect::new(0, 0, 16, 4));
        treemap.populate_tiles(&children);

        let Some(overflow) = treemap.overflow() else {
            panic!("zero-sized entry should produce an overflow summary");
        };
        assert_eq!(overflow.entries, 1);
        assert_eq!(overflow.bytes, 6);
        assert!(overflow.uncertain);
    }
}
