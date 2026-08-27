use crate::model::{NodeId, SyntheticKind};
use ::std::ffi::OsString;

use crate::state::tiles::{FileMetadata, FileType, RectFloat};

/// One terminal row is split into two independently coloured halves, so the
/// treemap lays out at twice the vertical resolution the terminal advertises.
pub const HALF_ROWS_PER_CELL: u16 = 2;
const HALF_ROWS_PER_CELL_U32: u32 = HALF_ROWS_PER_CELL as u32;

/// A laid-out map entry.
///
/// `x` and `width` are terminal columns. `y` and `height` are **half-rows**:
/// the renderer pairs them into `▀` cells, which lets an entry occupy half a
/// terminal row and keeps small siblings on the map instead of collapsing them
/// into the overflow summary.
/// Vertical coordinates are `u32`: a public terminal [`ratatui::layout::Rect`]
/// may use the full `u16` range for both its origin and its extent, and
/// converting each to half-rows can exceed `u16`.

#[derive(Clone, Debug)]
pub struct Tile {
    pub x: u16,
    pub y: u32,
    pub width: u16,
    pub height: u32,
    pub node_id: NodeId,
    pub name: OsString,
    pub size: u128,
    pub apparent_size: u128,
    pub descendants: Option<u64>,
    pub percentage: f64,
    pub file_type: FileType,
    pub synthetic_kind: Option<SyntheticKind>,
    pub uncertain: bool,
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

impl Tile {
    /// Creates a tile from layout geometry and file metadata.
    ///
    /// `rect.y` and `rect.height` are measured in half-rows, so this preserves
    /// the vertical precision the renderer uses to keep small sibling entries
    /// visible.
    #[must_use]
    pub fn new(rect: &RectFloat, file_metadata: &FileMetadata) -> Self {
        let x = terminal_coordinate(rect.x);
        let right = terminal_coordinate(rect.x + rect.width.max(0.0)).max(x);
        let y = half_row_coordinate(rect.y);
        let bottom = half_row_coordinate(rect.y + rect.height.max(0.0)).max(y);
        Self {
            x,
            y,
            width: right.saturating_sub(x),
            height: bottom.saturating_sub(y),
            node_id: file_metadata.node_id,
            name: file_metadata.name.clone(),
            size: file_metadata.size,
            apparent_size: file_metadata.apparent_size,
            descendants: file_metadata.descendants,
            percentage: file_metadata.percentage,
            file_type: file_metadata.file_type,
            synthetic_kind: file_metadata.synthetic_kind,
            uncertain: file_metadata.uncertain,
        }
    }

    /// First terminal row the entry paints into.
    #[must_use]
    pub const fn top_row(&self) -> u32 {
        self.y / HALF_ROWS_PER_CELL_U32
    }

    /// One past the last terminal row the entry paints into.
    #[must_use]
    pub const fn bottom_row(&self) -> u32 {
        if self.height == 0 {
            self.top_row()
        } else {
            self.y
                .saturating_add(self.height)
                .div_ceil(HALF_ROWS_PER_CELL_U32)
        }
    }

    /// Terminal rows the entry touches, including partially covered ones.
    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.bottom_row().saturating_sub(self.top_row())
    }

    /// Whether the entry covers the terminal row `row`.
    #[must_use]
    pub const fn covers_row(&self, row: u32) -> bool {
        row >= self.top_row() && row < self.bottom_row()
    }
    #[must_use]
    pub const fn is_directly_right_of(&self, other: &Self) -> bool {
        self.x == other.x.saturating_add(other.width)
    }

    #[must_use]
    pub const fn is_directly_left_of(&self, other: &Self) -> bool {
        self.x.saturating_add(self.width) == other.x
    }

    #[must_use]
    pub const fn is_directly_below(&self, other: &Self) -> bool {
        self.y == other.y.saturating_add(other.height)
    }

    #[must_use]
    pub const fn is_directly_above(&self, other: &Self) -> bool {
        self.y.saturating_add(self.height) == other.y
    }

    #[must_use]
    pub const fn horizontally_overlaps_with(&self, other: &Self) -> bool {
        (self.y >= other.y && self.y <= other.y.saturating_add(other.height))
            || (self.y.saturating_add(self.height) <= other.y.saturating_add(other.height)
                && self.y.saturating_add(self.height) > other.y)
            || (self.y <= other.y
                && self.y.saturating_add(self.height) >= other.y.saturating_add(other.height))
            || (other.y <= self.y
                && other.y.saturating_add(other.height) >= self.y.saturating_add(self.height))
    }

    #[must_use]
    pub const fn vertically_overlaps_with(&self, other: &Self) -> bool {
        (self.x >= other.x && self.x <= other.x.saturating_add(other.width))
            || (self.x.saturating_add(self.width) <= other.x.saturating_add(other.width)
                && self.x.saturating_add(self.width) > other.x)
            || (self.x <= other.x
                && self.x.saturating_add(self.width) >= other.x.saturating_add(other.width))
            || (other.x <= self.x
                && other.x.saturating_add(other.width) >= self.x.saturating_add(self.width))
    }

    #[must_use]
    pub fn get_vertical_overlap_with(&self, other: &Self) -> u16 {
        std::cmp::min(
            self.x.saturating_add(self.width),
            other.x.saturating_add(other.width),
        )
        .saturating_sub(std::cmp::max(self.x, other.x))
    }

    /// Returns the overlap along the map's vertical axis in half-rows.
    ///
    /// The map gives each terminal row two independently coloured halves, so
    /// callers must not compare this value directly with terminal-row geometry.
    #[must_use]
    pub fn get_horizontal_overlap_with(&self, other: &Self) -> u32 {
        std::cmp::min(
            self.y.saturating_add(self.height),
            other.y.saturating_add(other.height),
        )
        .saturating_sub(std::cmp::max(self.y, other.y))
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::model::NodeId;

    #[test]
    fn zero_height_tile_at_odd_half_row_covers_no_terminal_rows() {
        let tile = Tile {
            x: 0,
            y: 1,
            width: 1,
            height: 0,
            node_id: NodeId(0),
            name: OsString::new(),
            size: 0,
            apparent_size: 0,
            descendants: None,
            percentage: 0.0,
            file_type: FileType::File,
            synthetic_kind: None,
            uncertain: false,
        };

        assert_eq!(tile.bottom_row(), tile.top_row());
        assert_eq!(tile.rows(), 0);
        assert!(!tile.covers_row(tile.top_row()));
    }
}
