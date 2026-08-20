use crate::model::{NodeId, SyntheticKind};
use ::std::ffi::OsString;

use crate::state::tiles::{FileMetadata, FileType, RectFloat};

#[derive(Clone, Debug)]
pub struct Tile {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
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

impl Tile {
    #[must_use]
    pub fn new(rect: &RectFloat, file_metadata: &FileMetadata) -> Self {
        let rounded = rect.round();
        Self {
            x: rounded.x,
            y: rounded.y,
            width: rounded.width,
            height: rounded.height,
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
    #[must_use]
    pub const fn is_directly_right_of(&self, other: &Self) -> bool {
        self.x == other.x + other.width
    }

    #[must_use]
    pub const fn is_directly_left_of(&self, other: &Self) -> bool {
        self.x + self.width == other.x
    }

    #[must_use]
    pub const fn is_directly_below(&self, other: &Self) -> bool {
        self.y == other.y + other.height
    }

    #[must_use]
    pub const fn is_directly_above(&self, other: &Self) -> bool {
        self.y + self.height == other.y
    }

    #[must_use]
    pub const fn horizontally_overlaps_with(&self, other: &Self) -> bool {
        (self.y >= other.y && self.y <= (other.y + other.height))
            || ((self.y + self.height) <= (other.y + other.height)
                && (self.y + self.height) > other.y)
            || (self.y <= other.y && (self.y + self.height >= (other.y + other.height)))
            || (other.y <= self.y && (other.y + other.height >= (self.y + self.height)))
    }

    #[must_use]
    pub const fn vertically_overlaps_with(&self, other: &Self) -> bool {
        (self.x >= other.x && self.x <= (other.x + other.width))
            || ((self.x + self.width) <= (other.x + other.width) && (self.x + self.width) > other.x)
            || (self.x <= other.x && (self.x + self.width >= (other.x + other.width)))
            || (other.x <= self.x && (other.x + other.width >= (self.x + self.width)))
    }

    #[must_use]
    pub fn get_vertical_overlap_with(&self, other: &Self) -> u16 {
        std::cmp::min(self.x + self.width, other.x + other.width) - std::cmp::max(self.x, other.x)
    }

    #[must_use]
    pub fn get_horizontal_overlap_with(&self, other: &Self) -> u16 {
        std::cmp::min(self.y + self.height, other.y + other.height) - std::cmp::max(self.y, other.y)
    }
}
