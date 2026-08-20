use std::time::Duration;

use ratatui::layout::Rect;

use crate::model::NodeId;
use crate::state::tiles::files_in_folder::FileType;
use crate::state::tiles::{FileMetadata, Tile, TreeMap};
#[derive(Clone, Debug, Eq, PartialEq)]
struct DatasetView {
    folder: NodeId,
    filter: Option<String>,
    zoom_level: usize,
}

pub struct Board {
    pub tiles: Vec<Tile>,
    pub unrenderable_tile_coordinates: Option<(u16, u16)>,
    pub selected_index: Option<usize>, // None means nothing is selected
    pub previous_indices_and_zoom_level: Vec<(Option<usize>, usize)>, // Stack of previous stats
    pub zoom_level: usize,
    area: Rect,
    files: Vec<FileMetadata>,
    list_layout: bool,
    list_offset: usize,
    view: Option<DatasetView>,
    rendered_tiles: Vec<Tile>,
    transition_from: Vec<Tile>,
    transition_started: Option<Duration>,
    transition_origin: Option<Rect>,
}

impl Board {
    pub fn new() -> Self {
        Self {
            tiles: vec![],
            unrenderable_tile_coordinates: None,
            files: Vec::new(),
            selected_index: None,
            previous_indices_and_zoom_level: vec![],
            zoom_level: 0,
            area: Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            list_layout: false,
            list_offset: 0,
            view: None,
            rendered_tiles: Vec::new(),
            transition_from: Vec::new(),
            transition_started: None,
            transition_origin: None,
        }
    }
    #[cfg(test)]
    pub fn change_files(&mut self, files: Vec<FileMetadata>) {
        self.replace_files(files, false);
    }

    pub fn change_files_for_view(
        &mut self,
        files: Vec<FileMetadata>,
        folder: NodeId,
        filter: Option<&str>,
    ) {
        let changed = self.view.as_ref().is_none_or(|view| {
            view.folder != folder
                || view.filter.as_deref() != filter
                || view.zoom_level != self.zoom_level
        });
        if changed {
            self.view = Some(DatasetView {
                folder,
                filter: filter.map(str::to_owned),
                zoom_level: self.zoom_level,
            });
        }
        self.replace_files(files, changed);
    }
    pub fn change_area(&mut self, area: Rect) {
        if self.area != area {
            self.area = area;
            self.fill();
        }
    }

    fn replace_files(&mut self, files: Vec<FileMetadata>, reset_list_scroll: bool) {
        let selected = (!reset_list_scroll)
            .then(|| self.currently_selected().map(|tile| tile.node_id))
            .flatten();
        if reset_list_scroll {
            self.list_offset = 0;
        }
        self.files = files;
        self.fill_from_selected(selected);
    }

    fn fill(&mut self) {
        self.fill_from_selected(self.currently_selected().map(|tile| tile.node_id));
    }

    fn fill_from_selected(&mut self, selected: Option<NodeId>) {
        let previous = if self.rendered_tiles.is_empty() {
            self.tiles.clone()
        } else {
            self.rendered_tiles.clone()
        };
        self.list_layout = self.area.width < 72;
        let next_tiles = if self.list_layout {
            let visible = usize::from(self.area.height);
            self.clamp_list_offset(visible);
            if visible > 0
                && let Some(id) = selected
                && let Some(index) = self.files.iter().position(|file| file.node_id == id)
            {
                if index < self.list_offset {
                    self.list_offset = index;
                } else if index >= self.list_offset.saturating_add(visible) {
                    self.list_offset = index + 1 - visible;
                }
            }
            self.unrenderable_tile_coordinates = None;
            self.files
                .iter()
                .skip(self.list_offset)
                .take(visible)
                .enumerate()
                .map(|(row, file)| {
                    let rect = crate::state::tiles::RectFloat {
                        x: f64::from(self.area.x),
                        y: f64::from(self.area.y) + row as f64,
                        width: f64::from(self.area.width),
                        height: 1.0,
                    };
                    Tile::new(&rect, file)
                })
                .collect()
        } else {
            let mut tree_map = TreeMap::new(self.area);
            tree_map.populate_tiles(&self.files);
            self.unrenderable_tile_coordinates = tree_map.unrenderable_tile_coordinates;
            tree_map.tiles
        };
        self.tiles = next_tiles;
        self.selected_index =
            selected.and_then(|id| self.tiles.iter().position(|tile| tile.node_id == id));
        if self.list_layout || previous.is_empty() {
            self.transition_from.clear();
            self.transition_started = None;
            self.rendered_tiles.clone_from(&self.tiles);
        } else {
            self.transition_from = previous;
            self.transition_started = None;
        }
    }

    fn clamp_list_offset(&mut self, visible: usize) {
        self.list_offset = if visible == 0 {
            0
        } else {
            self.list_offset
                .min(self.files.len().saturating_sub(visible))
        };
    }

    pub fn advance_geometry(&mut self, now: Duration, reduced_motion: bool) {
        if reduced_motion || self.list_layout || self.transition_from.is_empty() {
            self.rendered_tiles.clone_from(&self.tiles);
            self.transition_from.clear();
            self.transition_started = None;
            self.transition_origin = None;
            return;
        }
        let started = *self.transition_started.get_or_insert(now);
        let progress = now.saturating_sub(started).as_secs_f64() / 0.160;
        if progress >= 1.0 {
            self.rendered_tiles.clone_from(&self.tiles);
            self.transition_from.clear();
            self.transition_started = None;
            self.transition_origin = None;
            return;
        }
        self.rendered_tiles.clear();
        self.rendered_tiles.reserve(self.tiles.len());
        for target in &self.tiles {
            let source = self
                .transition_from
                .iter()
                .find(|tile| tile.node_id == target.node_id);
            let origin = source.map_or_else(
                || {
                    self.transition_origin.unwrap_or(Rect::new(
                        target.x,
                        target.y,
                        target.width,
                        target.height,
                    ))
                },
                |tile| Rect::new(tile.x, tile.y, tile.width, tile.height),
            );
            let mut rendered = target.clone();
            rendered.x = interpolate(origin.x, target.x, progress);
            rendered.y = interpolate(origin.y, target.y, progress);
            rendered.width = interpolate(origin.width, target.width, progress);
            rendered.height = interpolate(origin.height, target.height, progress);
            self.rendered_tiles.push(rendered);
        }
    }

    #[must_use]
    pub fn rendered_tiles(&self) -> &[Tile] {
        &self.rendered_tiles
    }

    #[must_use]
    pub const fn is_list_layout(&self) -> bool {
        self.list_layout
    }

    #[must_use]
    pub fn hidden_list_entries(&self) -> usize {
        self.files
            .len()
            .saturating_sub(self.list_offset.saturating_add(self.tiles.len()))
    }
    pub const fn get_selected_index(&self) -> Option<usize> {
        self.selected_index
    }
    pub const fn set_selected_index(&mut self, next_index: usize) {
        self.selected_index = Some(next_index);
    }
    pub const fn has_selected_index(&self) -> bool {
        self.selected_index.is_some()
    }
    pub const fn reset_selected_index(&mut self) {
        self.selected_index = None;
    }
    pub fn currently_selected(&self) -> Option<&Tile> {
        self.selected_index
            .as_ref()
            .and_then(|selected_index| self.tiles.get(*selected_index))
    }

    pub fn select_at(&mut self, x: u16, y: u16) -> bool {
        let selected = self.tiles.iter().position(|tile| {
            x >= tile.x
                && x < tile.x.saturating_add(tile.width)
                && y >= tile.y
                && y < tile.y.saturating_add(tile.height)
        });
        if let Some(index) = selected {
            self.selected_index = Some(index);
            true
        } else {
            false
        }
    }
    pub fn pop_previous_index_and_zoom_level(&mut self) -> Option<(Option<usize>, usize)> {
        self.previous_indices_and_zoom_level.pop()
    }
    pub fn move_to_largest_folder(&mut self) {
        let next_index = self
            .tiles
            .iter()
            .enumerate()
            .filter(|(_, tile)| tile.file_type == FileType::Folder)
            .map(|(index, _)| index)
            .next();

        if let Some(index) = next_index {
            self.set_selected_index(index);
        }
    }
    pub fn move_selected_right(&mut self) {
        match self.currently_selected() {
            Some(currently_selected) => {
                let next_index = self
                    .tiles
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| {
                        c.is_directly_right_of(currently_selected)
                            && c.horizontally_overlaps_with(currently_selected)
                    })
                    // get the index of the tile with the most overlap with currently selected
                    .max_by_key(|(_, c)| c.get_horizontal_overlap_with(currently_selected))
                    .map(|(index, _)| index);
                match next_index {
                    Some(i) => self.set_selected_index(i),
                    None => self.reset_selected_index(), // move off the edge of the screen resets selection
                }
            }
            None => self.select_first(),
        }
    }
    pub fn move_selected_left(&mut self) {
        match self.currently_selected() {
            Some(currently_selected) => {
                let next_index = self
                    .tiles
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| {
                        c.is_directly_left_of(currently_selected)
                            && c.horizontally_overlaps_with(currently_selected)
                    })
                    // get the index of the tile with the most overlap with currently selected
                    .max_by_key(|(_, c)| c.get_horizontal_overlap_with(currently_selected))
                    .map(|(index, _)| index);
                match next_index {
                    Some(i) => self.set_selected_index(i),
                    None => self.reset_selected_index(), // move off the edge of the screen resets selection
                }
            }
            None => self.select_first(),
        }
    }
    pub fn move_selected_down(&mut self) {
        if self.list_layout {
            self.move_list_selection(1);
            return;
        }
        match self.currently_selected() {
            Some(currently_selected) => {
                let next_index = self
                    .tiles
                    .iter()
                    .enumerate()
                    .filter(|(_, candidate)| {
                        candidate.is_directly_below(currently_selected)
                            && candidate.vertically_overlaps_with(currently_selected)
                    })
                    .max_by_key(|(_, candidate)| {
                        candidate.get_vertical_overlap_with(currently_selected)
                    })
                    .map(|(index, _)| index);
                match next_index {
                    Some(index) => self.set_selected_index(index),
                    None => self.reset_selected_index(),
                }
            }
            None => self.select_first(),
        }
    }

    pub fn move_selected_up(&mut self) {
        if self.list_layout {
            self.move_list_selection(-1);
            return;
        }
        match self.currently_selected() {
            Some(currently_selected) => {
                let next_index = self
                    .tiles
                    .iter()
                    .enumerate()
                    .filter(|(_, candidate)| {
                        candidate.is_directly_above(currently_selected)
                            && candidate.vertically_overlaps_with(currently_selected)
                    })
                    .max_by_key(|(_, candidate)| {
                        candidate.get_vertical_overlap_with(currently_selected)
                    })
                    .map(|(index, _)| index);
                match next_index {
                    Some(index) => self.set_selected_index(index),
                    None => self.reset_selected_index(),
                }
            }
            None => self.select_first(),
        }
    }

    fn move_list_selection(&mut self, delta: isize) {
        let current = self.currently_selected().and_then(|tile| {
            self.files
                .iter()
                .position(|file| file.node_id == tile.node_id)
        });
        let next = current.map_or(0, |index| index.saturating_add_signed(delta));
        if next >= self.files.len() {
            self.selected_index = None;
            return;
        }
        let visible = usize::from(self.area.height).max(1);
        if next < self.list_offset {
            self.list_offset = next;
        } else if next >= self.list_offset.saturating_add(visible) {
            self.list_offset = next + 1 - visible;
        }
        let id = self.files[next].node_id;
        self.fill();
        self.selected_index = self.tiles.iter().position(|tile| tile.node_id == id);
    }
    fn select_first(&mut self) {
        self.selected_index = (!self.tiles.is_empty()).then_some(0);
    }

    pub fn zoom_in(&mut self, files: Vec<FileMetadata>) {
        if !files.is_empty() {
            self.zoom_level += 1;
            self.list_offset = 0;
            self.files = files;
            self.fill_from_selected(None);
        }
    }

    pub fn zoom_out(&mut self, files: Vec<FileMetadata>) {
        if self.zoom_level > 0 {
            self.zoom_level -= 1;
            self.list_offset = 0;
            self.files = files;
            self.fill_from_selected(None);
        }
    }

    pub fn reset_zoom(&mut self, files: Vec<FileMetadata>) {
        self.zoom_level = 0;
        self.list_offset = 0;
        self.files = files;
        self.fill_from_selected(None);
    }

    pub fn reset_zoom_index(&mut self) {
        self.zoom_level = 0;
        self.list_offset = 0;
    }

    pub fn set_zoom_index(&mut self, index: usize) {
        self.zoom_level = index;
        self.list_offset = 0;
    }

    pub fn record_current_index_and_zoom_level(&mut self) {
        self.previous_indices_and_zoom_level
            .push((self.get_selected_index(), self.zoom_level));
        self.transition_origin = self
            .currently_selected()
            .map(|tile| Rect::new(tile.x, tile.y, tile.width, tile.height));
    }
}

fn interpolate(from: u16, to: u16, progress: f64) -> u16 {
    (f64::from(from) + (f64::from(to) - f64::from(from)) * progress)
        .round()
        .clamp(0.0, f64::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::model::NodeId;

    fn file(id: u32, percentage: f64) -> FileMetadata {
        FileMetadata {
            node_id: NodeId(id),
            name: OsString::from(format!("file-{id}")),
            size: 100,
            apparent_size: 100,
            descendants: None,
            percentage,
            file_type: FileType::File,
            synthetic_kind: None,
            uncertain: false,
        }
    }

    #[test]
    fn resize_interpolates_stable_ids_and_reduced_motion_snaps() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.7), file(2, 0.3)]);
        board.advance_geometry(Duration::ZERO, true);
        let original = board.rendered_tiles().to_vec();

        board.change_area(Rect::new(0, 0, 100, 30));
        board.advance_geometry(Duration::ZERO, false);
        assert_eq!(board.rendered_tiles()[0].x, original[0].x);
        assert_eq!(board.rendered_tiles()[0].width, original[0].width);

        board.advance_geometry(Duration::from_millis(200), false);
        assert_eq!(board.rendered_tiles()[0].x, board.tiles[0].x);
        assert_eq!(board.rendered_tiles()[0].width, board.tiles[0].width);

        board.change_area(Rect::new(0, 0, 120, 30));
        board.advance_geometry(Duration::from_millis(201), true);
        assert_eq!(board.rendered_tiles()[0].width, board.tiles[0].width);
    }

    #[test]
    fn narrow_list_layout_scrolls_without_losing_identity() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 60, 3));
        board.change_files((1..=5).map(|id| file(id, 0.2)).collect());
        board.advance_geometry(Duration::ZERO, true);
        assert!(board.is_list_layout());
        assert_eq!(board.hidden_list_entries(), 2);

        for _ in 0..5 {
            board.move_selected_down();
        }

        assert_eq!(
            board.currently_selected().map(|tile| tile.node_id),
            Some(NodeId(5))
        );
        assert_eq!(board.hidden_list_entries(), 0);
    }

    #[test]
    fn same_view_refresh_clamps_scroll_and_keeps_selected_identity() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 60, 3));
        board.change_files((1..=5).map(|id| file(id, 0.2)).collect());
        for _ in 0..5 {
            board.move_selected_down();
        }

        board.change_files(vec![file(4, 0.5), file(5, 0.5)]);

        assert_eq!(board.list_offset, 0);
        assert_eq!(board.tiles.len(), 2);
        assert_eq!(
            board.currently_selected().map(|tile| tile.node_id),
            Some(NodeId(5))
        );
    }

    #[test]
    fn changing_folder_or_filter_dataset_resets_list_scroll() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 60, 3));
        board.change_files_for_view((1..=5).map(|id| file(id, 0.2)).collect(), NodeId(1), None);
        for _ in 0..5 {
            board.move_selected_down();
        }
        assert!(board.list_offset > 0);

        board.change_files_for_view(vec![file(10, 0.5), file(11, 0.5)], NodeId(2), Some("*.o"));

        assert_eq!(board.list_offset, 0);
        assert_eq!(
            board.tiles.first().map(|tile| tile.node_id),
            Some(NodeId(10))
        );
    }

    #[test]
    fn mouse_coordinates_select_real_tile_identity() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 100, 30));
        board.change_files(vec![file(1, 0.6), file(2, 0.4)]);
        board.advance_geometry(Duration::ZERO, true);
        let first = board.tiles[0].clone();
        assert!(board.select_at(first.x + 1, first.y + 1));
        assert_eq!(
            board.currently_selected().map(|tile| tile.node_id),
            Some(NodeId(1))
        );
    }
}
