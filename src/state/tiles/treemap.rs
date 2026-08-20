use ratatui::layout::Rect;

use crate::state::tiles::{FileMetadata, RectFloat, Tile};

const HEIGHT_WIDTH_RATIO: f64 = 2.5;
const MINIMUM_HEIGHT: u16 = 3;
const MINIMUM_WIDTH: u16 = 8;

pub struct TreeMap {
    pub tiles: Vec<Tile>,
    pub unrenderable_tile_coordinates: Option<(u16, u16)>,
    empty_space: RectFloat,
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

impl TreeMap {
    #[must_use]
    pub fn new(empty_space: Rect) -> Self {
        let empty_space = RectFloat::new(empty_space);
        Self {
            tiles: Vec::new(),
            unrenderable_tile_coordinates: None,
            total_size: empty_space.height * empty_space.width,
            empty_space,
        }
    }

    pub fn populate_tiles(&mut self, children: &[FileMetadata]) {
        self.squarify(children);
        if let Some((x, y)) = self.unrenderable_tile_coordinates {
            self.tiles.retain(|tile| tile.x < x || tile.y < y);
        }
    }

    fn layout_row(&mut self, row: &[FileMetadata]) {
        if row.is_empty() {
            return;
        }
        let row_total = row
            .iter()
            .fold(0.0, |total, file| total + file.percentage * self.total_size);
        if !row_total.is_finite() || row_total <= 0.0 {
            self.mark_remaining_unrenderable();
            return;
        }

        let horizontal = self.empty_space.width <= self.empty_space.height * HEIGHT_WIDTH_RATIO;
        let first_extent = if horizontal {
            self.empty_space.width
        } else {
            self.empty_space.height
        };
        if !first_extent.is_finite() || first_extent <= 0.0 {
            self.mark_remaining_unrenderable();
            return;
        }
        let second_extent = row_total / first_extent;
        if !second_extent.is_finite() || second_extent <= 0.0 {
            self.mark_remaining_unrenderable();
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

            let tile = Tile::new(&rect, file);
            if tile.height < MINIMUM_HEIGHT || tile.width < MINIMUM_WIDTH {
                self.add_unrenderable_tile(&tile);
            } else {
                self.tiles.push(tile);
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

    fn mark_remaining_unrenderable(&mut self) {
        let rect = self.empty_space.round();
        if rect.width > 0 && rect.height > 0 {
            self.unrenderable_tile_coordinates = Some((rect.x, rect.y));
        }
    }

    const fn add_unrenderable_tile(&mut self, tile: &Tile) {
        if tile.width == 0 || tile.height == 0 {
            return;
        }
        match self.unrenderable_tile_coordinates {
            Some((x, y)) => {
                self.unrenderable_tile_coordinates = Some((
                    if tile.x < x { tile.x } else { x },
                    if tile.y < y { tile.y } else { y },
                ));
            }
            None => self.unrenderable_tile_coordinates = Some((tile.x, tile.y)),
        }
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

    fn squarify(&mut self, children: &[FileMetadata]) {
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
                self.layout_row(&children[row_start..row_end]);
                row_start = row_end;
                metrics = RowMetrics::default();
            }
        }
        self.layout_row(&children[row_start..row_end]);
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

    #[test]
    fn geometry_is_finite_in_bounds_and_non_overlapping() {
        let area = Rect::new(3, 5, 190, 48);
        let files = files(10_000);
        let mut treemap = TreeMap::new(area);
        treemap.populate_tiles(&files);

        for (index, tile) in treemap.tiles.iter().enumerate() {
            assert!(tile.width >= MINIMUM_WIDTH);
            assert!(tile.height >= MINIMUM_HEIGHT);
            assert!(tile.x >= area.x && tile.y >= area.y);
            assert!(tile.x.saturating_add(tile.width) <= area.right());
            assert!(tile.y.saturating_add(tile.height) <= area.bottom());
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
    fn zero_size_entries_become_viewport_only_small_entries() {
        let mut files = files(2);
        for file in &mut files {
            file.size = 0;
            file.percentage = 0.0;
        }
        let mut treemap = TreeMap::new(Rect::new(0, 0, 80, 24));
        treemap.populate_tiles(&files);
        assert!(treemap.tiles.is_empty());
        assert_eq!(treemap.unrenderable_tile_coordinates, Some((0, 0)));
    }
}
