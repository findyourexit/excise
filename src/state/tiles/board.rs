use std::time::Duration;

use ratatui::layout::Rect;

use crate::model::NodeId;
use crate::state::tiles::files_in_folder::FileType;
use crate::state::tiles::{FileMetadata, HALF_ROWS_PER_CELL, MapOverflow, Tile, TreeMap};
#[derive(Clone, Debug, Eq, PartialEq)]
struct DatasetView {
    folder: NodeId,
    filter: Option<String>,
    zoom_level: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PivotResolution {
    NoPivot,
    Resolved,
    AwaitingListSelection,
    Unresolved,
}

/// Map-space geometry keeps half-row coordinates wide enough for any terminal
/// `Rect`; terminal `Rect`s are only used at the public API boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TileGeometry {
    x: u16,
    y: u32,
    width: u16,
    height: u32,
}

impl TileGeometry {
    fn from_tile(tile: &Tile) -> Self {
        Self {
            x: tile.x,
            y: tile.y,
            width: tile.width,
            height: tile.height,
        }
    }

    fn from_terminal_rect(rect: Rect) -> Self {
        let half_rows = u32::from(HALF_ROWS_PER_CELL);
        Self {
            x: rect.x,
            y: u32::from(rect.y).saturating_mul(half_rows),
            width: rect.width,
            height: u32::from(rect.height).saturating_mul(half_rows),
        }
    }
}

pub struct Board {
    pub tiles: Vec<Tile>,
    overflow: Option<MapOverflow>,
    pub selected_index: Option<usize>, // None means nothing is selected
    /// Zoom level held in each folder on the way down, restored on the way back
    /// up. The cursor is not stacked with it: coming out of a folder selects that
    /// folder by identity, which survives a layout the index would not.
    previous_zoom_levels: Vec<usize>,
    pub zoom_level: usize,
    area: Rect,
    files: Vec<FileMetadata>,
    list_layout: bool,
    list_offset: usize,
    view: Option<DatasetView>,
    /// Geometry actually drawn this frame. Always identity-aligned with `tiles`;
    /// only the four rectangle fields differ, and only while a tween is running.
    rendered_tiles: Vec<Tile>,
    /// Entries the incoming layout no longer contains, kept on screen for the
    /// length of a drill so the map contracts into the entry that replaced it
    /// instead of blinking out of existence.
    departing_tiles: Vec<Tile>,
    departing_from: Vec<TileGeometry>,
    /// Origin geometry for each target entry still moving, plus retained entries
    /// held at their landing rectangle during an active drill. It is keyed by
    /// node and sorted for binary search so a frame never rescans the previous
    /// layout.
    transition_from: Vec<(NodeId, TileGeometry)>,
    /// Reused output storage while refreshes resolve visible geometry into the
    /// next set of moving target origins.
    transition_scratch: Vec<(NodeId, TileGeometry)>,
    transition_started: Option<Duration>,
    /// Time represented by `rendered_tiles`. Retargeting starts a new segment
    /// here while retaining the current transition's original deadline.
    transition_last_frame: Option<Duration>,
    /// The rectangle a drill pivots around: the entry being opened, or the entry
    /// just left. Incoming entries grow out of it and departing entries collapse
    /// into it, which is what makes the movement read as one zoom.
    transition_origin: Option<TileGeometry>,
    transition_span: Duration,
    /// Set the instant before a drill swaps the dataset, resolved into
    /// `transition_origin` once the incoming layout exists.
    pending_pivot: Option<Pivot>,
    pending_pivot_geometry: Option<TileGeometry>,
}

impl Board {
    pub fn new() -> Self {
        Self {
            tiles: vec![],
            overflow: None,
            files: Vec::new(),
            selected_index: None,
            previous_zoom_levels: Vec::new(),
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
            departing_tiles: Vec::new(),
            departing_from: Vec::new(),
            transition_from: Vec::new(),
            transition_scratch: Vec::new(),
            transition_started: None,
            transition_last_frame: None,
            transition_origin: None,
            transition_span: crate::animation::ROUTINE_MOTION,
            pending_pivot: None,
            pending_pivot_geometry: None,
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
        if changed && self.pending_pivot.is_none() && self.pending_pivot_geometry.is_none() {
            self.clear_retained_pivot();
        }
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
            self.expire_stationary_pivot();
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
    /// Drops a resolved drill pivot once no movement remains. A stationary pivot
    /// is retained only for same-view scan arrivals; resize and zoom are new
    /// layouts and must not make later entries grow from that old rectangle.
    fn expire_stationary_pivot(&mut self) {
        if self.transition_origin.is_some() && !self.is_transitioning() {
            self.clear_retained_pivot();
        }
    }

    fn fill(&mut self) {
        self.fill_from_selected(self.currently_selected().map(|tile| tile.node_id));
    }

    /// Lays the current dataset out for the area, as a list when the terminal is
    /// too narrow for a treemap to say anything and as a treemap otherwise.
    fn lay_out_tiles(&mut self, selected: Option<NodeId>) -> Vec<Tile> {
        if !self.list_layout {
            let mut tree_map = TreeMap::new(self.area);
            tree_map.populate_tiles(&self.files);
            self.overflow = tree_map.overflow();
            return tree_map.tiles;
        }
        let visible = usize::from(self.area.height);
        self.clamp_list_offset(visible);
        // Scroll the window to wherever the cursor went, so a selection can never
        // sit off screen.
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
        self.overflow = None;
        self.files
            .iter()
            .skip(self.list_offset)
            .take(visible)
            .enumerate()
            .map(|(row, file)| {
                let rect = crate::state::tiles::RectFloat {
                    x: f64::from(self.area.x),
                    y: f64::from(self.area.y) * f64::from(HALF_ROWS_PER_CELL)
                        + row as f64 * f64::from(HALF_ROWS_PER_CELL),
                    width: f64::from(self.area.width),
                    height: f64::from(HALF_ROWS_PER_CELL),
                };
                Tile::new(&rect, file)
            })
            .collect()
    }

    /// Resolves a pending drill into the rectangle this frame's movement radiates
    /// from, reporting whether it resolved, awaits list selection, or is
    /// unavailable.
    ///
    /// `Pivot::Entry` waits for the incoming layout to expose the folder being
    /// left; a narrow list may need to reveal that identity first.
    fn resolve_pivot(&mut self) -> PivotResolution {
        let geometry_pivot = self.pending_pivot_geometry.take();
        let Some(pivot) = self.pending_pivot else {
            let Some(origin) = geometry_pivot else {
                return PivotResolution::NoPivot;
            };
            self.transition_origin = Some(origin);
            return PivotResolution::Resolved;
        };
        let origin = geometry_pivot.or_else(|| match pivot {
            Pivot::Rect(rect) => Some(TileGeometry::from_terminal_rect(rect)),
            Pivot::Entry(node) => self
                .tiles
                .iter()
                .find(|tile| tile.node_id == node)
                .map(TileGeometry::from_tile),
        });
        let Some(origin) = origin else {
            let waiting_for_list_selection = match pivot {
                Pivot::Entry(node) => {
                    self.list_layout && self.files.iter().any(|file| file.node_id == node)
                }
                Pivot::Rect(_) => false,
            };
            self.transition_origin = None;
            if waiting_for_list_selection {
                return PivotResolution::AwaitingListSelection;
            }
            self.pending_pivot = None;
            return PivotResolution::Unresolved;
        };
        self.pending_pivot = None;
        self.transition_origin = Some(origin);
        PivotResolution::Resolved
    }

    /// Keeps drawing the entries the incoming layout has no room for, so the old
    /// contents visibly recede into the pivot instead of vanishing between two
    /// frames.
    fn collect_departures(&mut self) {
        self.departing_tiles.clear();
        self.departing_from.clear();
        self.reconcile_departures();
    }

    /// Keeps old departures and adds any newly omitted visible identities.
    fn reconcile_departures(&mut self) {
        let previous_tiles = std::mem::take(&mut self.departing_tiles);
        let previous_from = std::mem::take(&mut self.departing_from);
        let mut departing_tiles = Vec::with_capacity(previous_tiles.len());
        let mut departing_from = Vec::with_capacity(previous_from.len());

        for (tile, from) in previous_tiles.into_iter().zip(previous_from) {
            if !self.tiles.iter().any(|kept| kept.node_id == tile.node_id) {
                departing_tiles.push(tile);
                departing_from.push(from);
            }
        }
        for tile in &self.rendered_tiles {
            if self.tiles.iter().any(|kept| kept.node_id == tile.node_id)
                || departing_tiles
                    .iter()
                    .any(|departing| departing.node_id == tile.node_id)
            {
                continue;
            }
            departing_from.push(TileGeometry::from_tile(tile));
            departing_tiles.push(tile.clone());
        }

        self.departing_tiles = departing_tiles;
        self.departing_from = departing_from;
    }

    /// Whether the currently drawn layout already occupies every target rectangle.
    fn geometry_matches_rendered(&self) -> bool {
        self.rendered_tiles.len() == self.tiles.len()
            && self
                .rendered_tiles
                .iter()
                .zip(&self.tiles)
                .all(|(rendered, target)| {
                    rendered.node_id == target.node_id
                        && TileGeometry::from_tile(rendered) == TileGeometry::from_tile(target)
                })
    }

    /// Starts a fresh interpolation segment from the last geometry shown without
    /// extending the deadline that was already promised to the reader.
    fn retarget_transition_from_last_frame(&mut self) {
        let (Some(started), Some(last_frame)) =
            (self.transition_started, self.transition_last_frame)
        else {
            return;
        };

        self.transition_span = self
            .transition_span
            .saturating_sub(last_frame.saturating_sub(started));
        self.transition_started = Some(last_frame);
        for (from, departing) in self.departing_from.iter_mut().zip(&self.departing_tiles) {
            *from = TileGeometry::from_tile(departing);
        }
    }

    fn clear_retained_pivot(&mut self) {
        self.transition_origin = None;
        self.transition_span = crate::animation::ROUTINE_MOTION;
        self.transition_started = None;
        self.transition_last_frame = None;
        self.transition_from.clear();
        self.departing_tiles.clear();
        self.departing_from.clear();
    }

    /// Seeds target origins from every geometry currently visible on screen.
    ///
    /// A rapid drill reversal has both incoming tiles and the previous drill's
    /// departures on screen. Prefer the incoming copy when an identity appears
    /// in both, and retain a departing copy for parent entries returning to view.
    fn seed_transition_origins(&mut self) {
        self.transition_from.clear();
        self.transition_from.extend(
            self.rendered_tiles
                .iter()
                .map(|tile| (tile.node_id, TileGeometry::from_tile(tile))),
        );
        self.transition_from.sort_unstable_by_key(|(node, _)| *node);
        for tile in &self.departing_tiles {
            if self
                .transition_from
                .binary_search_by_key(&tile.node_id, |(node, _)| *node)
                .is_err()
            {
                self.transition_from
                    .push((tile.node_id, TileGeometry::from_tile(tile)));
            }
        }
        self.transition_from.sort_unstable_by_key(|(node, _)| *node);
    }

    fn fill_from_selected(&mut self, selected: Option<NodeId>) {
        self.list_layout = self.area.width < 72;
        let selected = selected
            .filter(|id| self.files.iter().any(|file| file.node_id == *id))
            .or_else(|| self.files.first().map(|file| file.node_id));
        self.tiles = self.lay_out_tiles(selected);
        self.selected_index =
            selected.and_then(|id| self.tiles.iter().position(|tile| tile.node_id == id));
        // The map always holds a cursor while it has entries to hold one on. A
        // folder whose contents were just replaced would otherwise come up with
        // nothing selected, emptying the inspector and leaving the reader to hunt
        // for the entry that matters — which is the biggest one, at index zero.
        if self.selected_index.is_none() {
            self.select_largest();
        }
        let pivot_resolution = self.resolve_pivot();
        let drilling = pivot_resolution == PivotResolution::Resolved;
        // An empty layout normally snaps immediately. A resolved drill pivot may
        // still be collapsing departures after an empty or overflow-only child
        // refresh, however, so leave that active movement intact.
        if pivot_resolution == PivotResolution::Unresolved
            || self.list_layout
            || (self.rendered_tiles.is_empty() && self.transition_origin.is_none())
        {
            self.rendered_tiles.clone_from(&self.tiles);
            self.settle_geometry();
            return;
        }
        // A refresh that leaves every identity in its already drawn rectangle has
        // nothing to animate. Keep a resolved, stationary drill pivot for later
        // arrivals in the same view, but do not report it as an active tween.
        if !drilling && self.departing_tiles.is_empty() && self.geometry_matches_rendered() {
            self.rendered_tiles.clone_from(&self.tiles);
            if self.transition_origin.is_none() || !self.transition_from.is_empty() {
                self.settle_geometry();
            }
            return;
        }
        // Scanning refreshes the dataset every batch. Re-aim from the geometry
        // currently on screen, using only the remaining portion of the original
        // transition rather than applying its elapsed fraction to this new origin.
        self.seed_transition_origins();
        // A drill replaces the dataset outright, and gets the longer budget: the
        // eye has to follow entries across the whole surface, not a few cells.
        if drilling {
            self.transition_started = None;
            self.transition_last_frame = None;
            self.transition_span = crate::animation::NAVIGATION_MOTION;
            self.collect_departures();
        } else {
            self.retarget_transition_from_last_frame();
            if self.transition_origin.is_some() {
                self.reconcile_departures();
            }
        }
        let active_pivot = self.transition_origin.is_some();
        self.rendered_tiles.clone_from(&self.tiles);
        // Resolve every target from the complete visible layout before discarding
        // origins. A retained identity that is already at its target still needs
        // an origin during an active drill: otherwise a later frame mistakes it
        // for a late arrival and sends it back to the pivot.
        {
            let previous_origins = &self.transition_from;
            let pivot = self.transition_origin;
            let next = &mut self.transition_scratch;
            next.clear();
            for rendered in &mut self.rendered_tiles {
                let target = TileGeometry::from_tile(rendered);
                let (origin, retained) = previous_origins
                    .binary_search_by_key(&rendered.node_id, |(node, _)| *node)
                    .map_or_else(
                        |_| (pivot.unwrap_or(target), false),
                        |index| (previous_origins[index].1, true),
                    );
                if origin != target || (active_pivot && retained) {
                    rendered.x = origin.x;
                    rendered.y = origin.y;
                    rendered.width = origin.width;
                    rendered.height = origin.height;
                    next.push((rendered.node_id, origin));
                }
            }
            next.sort_unstable_by_key(|(node, _)| *node);
        }
        std::mem::swap(&mut self.transition_from, &mut self.transition_scratch);

        // A stationary retained identity remains in `transition_from` during a
        // drill, so vector length alone no longer distinguishes real work from a
        // no-op. Keep the resolved pivot after a stationary first batch: later
        // arrivals in this view must still grow from it, but no frame is owed.
        let departures_move = self
            .transition_origin
            .is_some_and(|pivot| self.departing_from.iter().any(|from| *from != pivot));
        if self.geometry_matches_rendered() && !departures_move {
            if drilling || self.transition_origin.is_some() {
                self.clear_motion_state();
            } else {
                self.settle_geometry();
            }
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

    /// Whether the map still owes the terminal tween frames.
    #[must_use]
    pub fn is_transitioning(&self) -> bool {
        !self.transition_from.is_empty() || !self.departing_tiles.is_empty()
    }

    pub fn advance_geometry(&mut self, now: Duration, reduced_motion: bool) {
        if reduced_motion || self.list_layout {
            self.settle_geometry();
            return;
        }
        // A stationary resolved drill retains its pivot for later same-view
        // arrivals but owes no frames, so do not settle it away here.
        if !self.is_transitioning() {
            return;
        }
        let started = *self.transition_started.get_or_insert(now);
        if self.transition_span.is_zero() {
            self.settle_geometry();
            return;
        }
        let linear = now.saturating_sub(started).as_secs_f64() / self.transition_span.as_secs_f64();
        if linear >= 1.0 {
            self.settle_geometry();
            return;
        }
        let progress = ease_out(linear);
        for (rendered, target) in self.rendered_tiles.iter_mut().zip(self.tiles.iter()) {
            let origin = self
                .transition_from
                .binary_search_by_key(&target.node_id, |(node, _)| *node)
                .map_or_else(
                    |_| {
                        self.transition_origin
                            .unwrap_or_else(|| TileGeometry::from_tile(target))
                    },
                    |index| self.transition_from[index].1,
                );
            rendered.x = interpolate(origin.x, target.x, progress);
            rendered.y = interpolate_u32(origin.y, target.y, progress);
            rendered.width = interpolate(origin.width, target.width, progress);
            rendered.height = interpolate_u32(origin.height, target.height, progress);
        }
        // Departing entries run the same clock in the opposite direction: they
        // shrink onto the pivot and are dropped the moment they reach it.
        if let Some(pivot) = self.transition_origin {
            for (departing, from) in self
                .departing_tiles
                .iter_mut()
                .zip(self.departing_from.iter())
            {
                departing.x = interpolate(from.x, pivot.x, progress);
                departing.y = interpolate_u32(from.y, pivot.y, progress);
                departing.width = interpolate(from.width, pivot.width, progress);
                departing.height = interpolate_u32(from.height, pivot.height, progress);
            }
        }
        self.transition_last_frame = Some(now);
    }

    fn clear_motion_state(&mut self) {
        self.transition_from.clear();
        self.departing_tiles.clear();
        self.departing_from.clear();
        self.transition_started = None;
        self.transition_last_frame = None;
    }

    /// Drops the tween and pins every entry to its final rectangle.
    ///
    /// Frames that never draw the map call this directly: a tween nobody can see
    /// would otherwise keep asking the owner loop for frames forever.
    pub fn settle_geometry(&mut self) {
        for (rendered, target) in self.rendered_tiles.iter_mut().zip(self.tiles.iter()) {
            rendered.x = target.x;
            rendered.y = target.y;
            rendered.width = target.width;
            rendered.height = target.height;
        }
        self.clear_motion_state();
        self.transition_origin = None;
        self.transition_span = crate::animation::ROUTINE_MOTION;
    }

    /// Entries on their way out, drawn beneath the incoming layout.
    #[must_use]
    pub fn departing_tiles(&self) -> &[Tile] {
        &self.departing_tiles
    }

    /// Aims the next dataset swap at `pivot`.
    ///
    /// Called before the dataset changes: a drill in names a rectangle that is on
    /// screen right now, a drill out names the entry whose rectangle only exists
    /// once the parent layout is rebuilt.
    pub const fn pivot_transition_on(&mut self, pivot: Pivot) {
        self.pending_pivot = Some(pivot);
        self.pending_pivot_geometry = None;
    }

    pub(crate) fn pivot_transition_on_geometry(&mut self, geometry: TileGeometry) {
        self.pending_pivot = None;
        self.pending_pivot_geometry = Some(geometry);
    }

    #[must_use]
    pub fn rendered_tiles(&self) -> &[Tile] {
        &self.rendered_tiles
    }

    #[must_use]
    pub fn overflow(&self) -> Option<MapOverflow> {
        self.overflow
    }

    /// Returns the settled overflow summary when the map is not moving.
    ///
    /// The summary is withheld for the length of the tween rather than allowed to
    /// overwrite entries in motion through its corner.
    #[must_use]
    pub fn rendered_overflow(&self) -> Option<MapOverflow> {
        if self.is_transitioning() {
            None
        } else {
            self.overflow
        }
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
    pub const fn set_selected_index(&mut self, next_index: usize) {
        self.selected_index = Some(next_index);
    }
    pub const fn has_selected_index(&self) -> bool {
        self.selected_index.is_some()
    }
    pub const fn reset_selected_index(&mut self) {
        self.selected_index = None;
    }
    /// Returns the selected entry's data from the settled layout.
    ///
    /// The inspector deliberately reads `tiles`: a tween changes only rectangles,
    /// while identity and accounting belong to the node rather than the geometry.
    pub fn currently_selected(&self) -> Option<&Tile> {
        self.selected_index
            .as_ref()
            .and_then(|selected_index| self.tiles.get(*selected_index))
    }

    /// Resolves visible geometry back to the target layout, falling back to node
    /// identity if a refresh ever disrupts their ordinary index alignment.
    fn target_index_for_rendered(&self, rendered_index: usize, rendered: &Tile) -> Option<usize> {
        self.tiles
            .get(rendered_index)
            .filter(|target| target.node_id == rendered.node_id)
            .map(|_| rendered_index)
            .or_else(|| {
                self.tiles
                    .iter()
                    .position(|target| target.node_id == rendered.node_id)
            })
    }

    /// Selects the visible entry under a terminal cell, converting the pointer row
    /// into the half-row space the map lays out in.
    pub fn select_at(&mut self, x: u16, y: u16) -> bool {
        if x < self.area.x || x >= self.area.right() || y < self.area.y || y >= self.area.bottom() {
            return false;
        }

        // DenseGrid lifts a valid selected entry over its siblings in every
        // frame, then paints the remaining source order front-to-back in reverse.
        // Hit tests must walk that same visible stack rather than target-layout
        // order.
        let selected_on_top = self
            .selected_index
            .filter(|index| *index < self.rendered_tiles.len());
        let contains = |tile: &Tile| {
            x >= tile.x && x < tile.x.saturating_add(tile.width) && tile.covers_row(u32::from(y))
        };
        let rendered = selected_on_top
            .and_then(|index| {
                self.rendered_tiles
                    .get(index)
                    .filter(|tile| contains(tile))
                    .map(|tile| (index, tile))
            })
            .or_else(|| {
                self.rendered_tiles
                    .iter()
                    .enumerate()
                    .rev()
                    .filter(|(index, _)| Some(*index) != selected_on_top)
                    .find(|(_, tile)| contains(tile))
            });
        let selected = rendered.and_then(|(rendered_index, rendered)| {
            self.target_index_for_rendered(rendered_index, rendered)
        });
        if let Some(index) = selected {
            self.selected_index = Some(index);
            true
        } else {
            false
        }
    }
    pub fn pop_previous_zoom_level(&mut self) -> Option<usize> {
        self.previous_zoom_levels.pop()
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
                // Running off the edge holds the cursor where it is. Dropping the
                // selection instead would empty the inspector and re-lay the map
                // out, which is a lot of interface movement to report "no".
                if let Some(index) = next_index {
                    self.set_selected_index(index);
                }
            }
            None => self.select_largest(),
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
                // Running off the edge holds the cursor where it is. Dropping the
                // selection instead would empty the inspector and re-lay the map
                // out, which is a lot of interface movement to report "no".
                if let Some(index) = next_index {
                    self.set_selected_index(index);
                }
            }
            None => self.select_largest(),
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
                if let Some(index) = next_index {
                    self.set_selected_index(index);
                }
            }
            None => self.select_largest(),
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
                if let Some(index) = next_index {
                    self.set_selected_index(index);
                }
            }
            None => self.select_largest(),
        }
    }

    fn move_list_selection(&mut self, delta: isize) {
        let current = self.currently_selected().and_then(|tile| {
            self.files
                .iter()
                .position(|file| file.node_id == tile.node_id)
        });
        let next = current.map_or(0, |index| index.saturating_add_signed(delta));
        // The list holds its cursor at both ends for the same reason the map
        // does: running past the last row is not a reason to select nothing.
        if next >= self.files.len() {
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
    /// Selects the biggest entry in the folder.
    ///
    /// The treemap is laid out largest first, so index zero is both the entry
    /// occupying the most space and the one most worth looking at.
    pub fn select_largest(&mut self) {
        self.selected_index = (!self.tiles.is_empty()).then_some(0);
    }

    /// Selects `node`, revealing it first when a narrow list has paged it away.
    pub fn select_node(&mut self, node: NodeId) -> bool {
        if let Some(index) = self.tiles.iter().position(|tile| tile.node_id == node) {
            self.selected_index = Some(index);
            return true;
        }
        if self.list_layout && self.files.iter().any(|file| file.node_id == node) {
            // `lay_out_tiles` resolves list identities against the full dataset,
            // moves the page window, and lets `fill_from_selected` resolve a
            // pending return pivot only after this identity is selected.
            self.fill_from_selected(Some(node));
            return self
                .currently_selected()
                .is_some_and(|tile| tile.node_id == node);
        }
        false
    }

    pub fn zoom_in(&mut self, files: Vec<FileMetadata>) {
        if !files.is_empty() {
            self.expire_stationary_pivot();
            self.zoom_level += 1;
            self.list_offset = 0;
            self.files = files;
            self.fill_from_selected(None);
        }
    }

    pub fn zoom_out(&mut self, files: Vec<FileMetadata>) {
        if self.zoom_level > 0 {
            self.expire_stationary_pivot();
            self.zoom_level -= 1;
            self.list_offset = 0;
            self.files = files;
            self.fill_from_selected(None);
        }
    }

    pub fn reset_zoom(&mut self, files: Vec<FileMetadata>) {
        self.expire_stationary_pivot();
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

    pub fn record_current_zoom_level(&mut self) {
        self.previous_zoom_levels.push(self.zoom_level);
    }

    /// The terminal-cell rectangle the selected entry will occupy once the current
    /// tween settles.
    ///
    /// Use `selected_rendered_rect` for the rectangle the reader saw; this
    /// accessor stays on `tiles` for callers that need target geometry.
    #[allow(
        dead_code,
        reason = "terminal-cell accessors remain available for callers while drills use exact half-row geometry"
    )]
    #[must_use]
    pub fn selected_rect(&self) -> Option<Rect> {
        self.selected_geometry().and_then(terminal_rect_geometry)
    }

    pub(crate) fn selected_geometry(&self) -> Option<TileGeometry> {
        self.currently_selected().map(TileGeometry::from_tile)
    }

    pub(crate) fn selected_rendered_geometry(&self) -> Option<TileGeometry> {
        let selected = self.currently_selected()?;
        self.rendered_tiles
            .iter()
            .find(|tile| tile.node_id == selected.node_id)
            .map(TileGeometry::from_tile)
    }

    /// The terminal-cell rectangle the selected entry occupies on screen in this
    /// frame.
    #[allow(
        dead_code,
        reason = "terminal-cell accessors remain available for callers while drills use exact half-row geometry"
    )]
    #[must_use]
    pub fn selected_rendered_rect(&self) -> Option<Rect> {
        self.selected_rendered_geometry()
            .and_then(terminal_rect_geometry)
    }
}

/// What the next dataset swap should zoom around.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pivot {
    /// A terminal-cell rectangle in the layout on screen now — the entry being opened.
    #[allow(
        dead_code,
        reason = "terminal-cell pivot remains available for compatibility; internal drills carry half-row geometry"
    )]
    Rect(Rect),
    /// An entry in the layout about to be built — the folder being left, which
    /// has no geometry until its parent is laid out again.
    Entry(NodeId),
}

/// Ease-out cubic.
///
/// A drill should leave immediately and arrive gently; linear motion over the
/// same span reads as a slide with a hard stop at both ends.
fn ease_out(progress: f64) -> f64 {
    let remaining = 1.0 - progress.clamp(0.0, 1.0);
    1.0 - remaining * remaining * remaining
}

fn interpolate(from: u16, to: u16, progress: f64) -> u16 {
    (f64::from(from) + (f64::from(to) - f64::from(from)) * progress)
        .round()
        .clamp(0.0, f64::from(u16::MAX)) as u16
}

fn interpolate_u32(from: u32, to: u32, progress: f64) -> u32 {
    (f64::from(from) + (f64::from(to) - f64::from(from)) * progress)
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32
}

#[cfg(test)]
fn terminal_rect(tile: &Tile) -> Option<Rect> {
    terminal_rect_geometry(TileGeometry::from_tile(tile))
}

#[allow(
    dead_code,
    reason = "terminal-cell compatibility accessors convert exact half-row geometry for callers"
)]
fn terminal_rect_geometry(geometry: TileGeometry) -> Option<Rect> {
    let half_rows = u32::from(HALF_ROWS_PER_CELL);
    let top = geometry.y / half_rows;
    let bottom = if geometry.height == 0 {
        top
    } else {
        geometry
            .y
            .saturating_add(geometry.height)
            .div_ceil(half_rows)
    };
    Some(Rect::new(
        geometry.x,
        u16::try_from(top).ok()?,
        geometry.width,
        u16::try_from(bottom.saturating_sub(top)).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::model::NodeId;

    fn file(id: u32, percentage: f64) -> FileMetadata {
        file_with_size(id, 100, percentage)
    }

    fn file_with_size(id: u32, size: u128, percentage: f64) -> FileMetadata {
        FileMetadata {
            node_id: NodeId(id),
            name: OsString::from(format!("file-{id}")),
            size,
            apparent_size: size,
            descendants: None,
            percentage,
            file_type: FileType::File,
            synthetic_kind: None,
            uncertain: false,
        }
    }

    fn reordered_board_mid_tween() -> Board {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
        board.advance_geometry(Duration::ZERO, true);

        board.change_files(vec![file(2, 0.5), file(1, 0.5)]);
        board.advance_geometry(Duration::ZERO, false);
        assert!(board.is_transitioning());
        board
    }

    fn assert_rendered_identity_alignment(board: &Board) {
        assert!(
            board
                .rendered_tiles()
                .iter()
                .map(|tile| tile.node_id)
                .eq(board.tiles.iter().map(|tile| tile.node_id)),
            "rendered geometry must keep the target layout's identity order"
        );
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

    /// Drilling in has to read as one zoom: the folder's contents grow out of
    /// the rectangle the reader just opened, and the entries that rectangle
    /// shared the screen with collapse into it rather than blinking away.
    #[test]
    fn a_drill_grows_the_new_layout_out_of_the_entry_it_opened() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("a filled map holds a cursor");

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files(vec![file(10, 0.6), file(11, 0.4)]);
        board.advance_geometry(Duration::ZERO, false);
        assert_rendered_identity_alignment(&board);

        let opened_bottom = opened.bottom();
        for tile in board.rendered_tiles() {
            assert!(
                tile.x >= opened.x
                    && tile.top_row() >= u32::from(opened.y)
                    && tile.x.saturating_add(tile.width) <= opened.right()
                    && tile.bottom_row() <= u32::from(opened_bottom),
                "incoming entries must start inside the opened rectangle: {tile:?} vs {opened:?}"
            );
        }
        assert_eq!(
            board.departing_tiles().len(),
            2,
            "the folder's siblings, and the folder itself, recede behind its contents"
        );

        board.advance_geometry(Duration::from_millis(260), false);
        assert!(!board.is_transitioning(), "the drill has to finish");
        assert!(
            board.departing_tiles().is_empty(),
            "nothing lingers once the map has settled"
        );
        for (rendered, target) in board.rendered_tiles().iter().zip(board.tiles.iter()) {
            assert_eq!(
                (rendered.x, rendered.y, rendered.width, rendered.height),
                (target.x, target.y, target.width, target.height)
            );
        }
    }

    #[test]
    fn geometry_pivots_retain_odd_half_row_boundaries() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
        board.advance_geometry(Duration::ZERO, true);
        board.selected_index = Some(0);

        let odd = TileGeometry {
            x: 3,
            y: 1,
            width: 20,
            height: 4,
        };
        board.tiles[0].x = odd.x;
        board.tiles[0].y = odd.y;
        board.tiles[0].width = odd.width;
        board.tiles[0].height = odd.height;
        board.rendered_tiles[0] = board.tiles[0].clone();

        let pivot = board
            .selected_rendered_geometry()
            .expect("the odd-boundary selection should be rendered");
        assert_eq!(pivot, odd);
        board.pivot_transition_on_geometry(pivot);
        board.change_files(vec![file(10, 0.6), file(11, 0.4)]);

        assert_eq!(board.transition_origin, Some(odd));
        assert!(
            board
                .rendered_tiles()
                .iter()
                .all(|tile| TileGeometry::from_tile(tile) == odd),
            "incoming entries must start at the exact half-row pivot"
        );
    }

    #[test]
    fn a_drill_refresh_before_its_first_frame_keeps_new_entries_at_the_pivot() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("a filled map holds a cursor");
        let pivot = TileGeometry::from_terminal_rect(opened);
        let incoming = vec![file(10, 0.6), file(11, 0.4)];

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files(incoming.clone());
        assert!(board.is_transitioning());
        assert!(
            board
                .rendered_tiles()
                .iter()
                .all(|tile| TileGeometry::from_tile(tile) == pivot),
            "incoming entries must be initialized at the pivot before any frame"
        );

        board.change_files(incoming);
        assert!(
            board
                .rendered_tiles()
                .iter()
                .all(|tile| TileGeometry::from_tile(tile) == pivot),
            "a refresh before the first frame must retain the pivot geometry"
        );
        board.advance_geometry(Duration::ZERO, false);
        assert!(
            board
                .rendered_tiles()
                .iter()
                .all(|tile| TileGeometry::from_tile(tile) == pivot)
        );
    }

    #[test]
    fn a_late_drill_arrival_starts_at_the_active_pivot() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("a filled map holds a cursor");
        let pivot = TileGeometry::from_terminal_rect(opened);

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files(vec![file(10, 0.6), file(11, 0.4)]);
        board.advance_geometry(Duration::ZERO, false);
        board.advance_geometry(Duration::from_millis(80), false);

        board.change_files(vec![file(10, 0.5), file(11, 0.3), file(12, 0.2)]);
        assert!(board.is_transitioning());
        let late = board
            .rendered_tiles()
            .iter()
            .find(|tile| tile.node_id == NodeId(12))
            .map(TileGeometry::from_tile)
            .expect("the late entry must be visible");
        let target = board
            .tiles
            .iter()
            .find(|tile| tile.node_id == NodeId(12))
            .map(TileGeometry::from_tile)
            .expect("the late entry must have a landing rectangle");

        assert_ne!(
            target, pivot,
            "the fixture needs a distinct landing rectangle"
        );
        assert_eq!(
            late, pivot,
            "a late drill entry must not expose its unseen landing rectangle"
        );
    }

    /// Leaving a folder is the same movement played backwards: the parent layout
    /// contracts into the entry the reader is stepping out of.
    #[test]
    fn stepping_out_pivots_on_the_folder_being_left() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(10, 0.6), file(11, 0.4)]);
        board.advance_geometry(Duration::ZERO, true);

        board.pivot_transition_on(Pivot::Entry(NodeId(2)));
        board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
        board.advance_geometry(Duration::ZERO, false);

        let landing = board
            .tiles
            .iter()
            .find(|tile| tile.node_id == NodeId(2))
            .expect("the folder being left is present in the parent layout");
        for tile in board.rendered_tiles() {
            assert!(
                tile.x >= landing.x && tile.y >= landing.y,
                "the parent layout must expand out of the folder just left: {tile:?}"
            );
        }
        assert_eq!(
            board.departing_tiles().len(),
            2,
            "the folder's own contents"
        );
    }

    #[test]
    fn empty_child_refreshes_keep_active_departures_collapsing() {
        for child in [vec![], vec![file_with_size(10, 100, 0.0)]] {
            let mut board = Board::new();
            board.change_area(Rect::new(0, 0, 80, 24));
            board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
            board.advance_geometry(Duration::ZERO, true);
            let opened = board.selected_rect().expect("the parent map has a cursor");
            let pivot = TileGeometry::from_terminal_rect(opened);

            board.pivot_transition_on(Pivot::Rect(opened));
            board.change_files(child.clone());
            board.advance_geometry(Duration::ZERO, false);
            board.advance_geometry(Duration::from_millis(80), false);
            let (departing_id, before_refresh) = board
                .departing_tiles()
                .iter()
                .find(|tile| TileGeometry::from_tile(tile) != pivot)
                .map(|tile| (tile.node_id, TileGeometry::from_tile(tile)))
                .expect("the sibling departure must still be collapsing");

            board.change_files(child);
            assert!(
                board.is_transitioning(),
                "a repeated empty-child refresh must not clear active departures"
            );
            assert_eq!(
                board
                    .departing_tiles()
                    .iter()
                    .find(|tile| tile.node_id == departing_id)
                    .map(TileGeometry::from_tile),
                Some(before_refresh),
                "the refresh must continue from the geometry already shown"
            );

            board.advance_geometry(Duration::from_millis(160), false);
            assert_ne!(
                board
                    .departing_tiles()
                    .iter()
                    .find(|tile| tile.node_id == departing_id)
                    .map(TileGeometry::from_tile),
                Some(before_refresh),
                "the retained departure must keep collapsing after the refresh"
            );
            board.advance_geometry(Duration::from_millis(260), false);
            assert!(!board.is_transitioning());
            assert!(board.departing_tiles().is_empty());
        }
    }

    #[test]
    fn drilling_out_of_empty_or_overflow_only_children_starts_at_the_resolved_pivot() {
        for (child, expects_overflow) in
            [(vec![], false), (vec![file_with_size(10, 100, 0.0)], true)]
        {
            let parent = vec![file(1, 0.5), file(2, 0.5)];
            let mut board = Board::new();
            board.change_area(Rect::new(0, 0, 80, 24));
            board.change_files(parent.clone());
            board.advance_geometry(Duration::ZERO, true);
            let opened = board
                .tiles
                .iter()
                .find(|tile| tile.node_id == NodeId(1))
                .and_then(terminal_rect)
                .expect("the parent folder must have a visible rectangle");

            board.pivot_transition_on(Pivot::Rect(opened));
            board.change_files(child);
            board.advance_geometry(Duration::ZERO, true);
            assert!(
                board.rendered_tiles().is_empty(),
                "the child fixture must leave no retained geometry"
            );
            assert_eq!(board.overflow().is_some(), expects_overflow);

            board.pivot_transition_on(Pivot::Entry(NodeId(1)));
            board.change_files(parent);
            let pivot = board
                .tiles
                .iter()
                .find(|tile| tile.node_id == NodeId(1))
                .map(TileGeometry::from_tile)
                .expect("the returned parent must expose its folder pivot");

            assert!(
                board.is_transitioning(),
                "a resolved return from an empty child must use navigation motion"
            );
            assert_eq!(board.transition_span, crate::animation::NAVIGATION_MOTION);
            assert!(
                board
                    .rendered_tiles()
                    .iter()
                    .all(|tile| TileGeometry::from_tile(tile) == pivot),
                "every incoming parent entry must start at the resolved folder pivot"
            );
        }
    }

    #[test]
    fn stationary_first_drill_batch_keeps_pivot_for_late_same_view_arrival() {
        let initial = vec![file(1, 0.5), file(2, 0.5)];
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files_for_view(initial.clone(), NodeId(90), None);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("the initial map has a cursor");
        let pivot = TileGeometry::from_terminal_rect(opened);

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files_for_view(initial.clone(), NodeId(91), None);
        assert!(
            !board.is_transitioning(),
            "a geometrically stationary first drill batch must not request frames"
        );
        assert_eq!(board.transition_origin, Some(pivot));
        board.advance_geometry(Duration::ZERO, false);
        assert_eq!(board.transition_origin, Some(pivot));
        board.change_files_for_view(initial, NodeId(91), None);
        assert!(!board.is_transitioning());
        assert_eq!(board.transition_origin, Some(pivot));

        board.change_files_for_view(
            vec![file(1, 0.5), file(2, 0.25), file(3, 0.25)],
            NodeId(91),
            None,
        );
        let late = board
            .rendered_tiles()
            .iter()
            .find(|tile| tile.node_id == NodeId(3))
            .map(TileGeometry::from_tile)
            .expect("the late arrival must be visible");
        let target = board
            .tiles
            .iter()
            .find(|tile| tile.node_id == NodeId(3))
            .map(TileGeometry::from_tile)
            .expect("the late arrival must have a landing rectangle");

        assert_ne!(target, pivot, "the late arrival needs a distinct target");
        assert!(board.is_transitioning());
        assert_eq!(
            late, pivot,
            "the late arrival must start at the drill pivot"
        );
    }
    #[test]
    fn resize_discards_a_stationary_drill_pivot_before_relayout() {
        let initial = vec![file(1, 0.5), file(2, 0.5)];
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files_for_view(initial.clone(), NodeId(90), None);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("the initial map has a cursor");
        let old_geometry = board
            .rendered_tiles()
            .first()
            .map(TileGeometry::from_tile)
            .expect("the initial map has rendered geometry");

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files_for_view(initial, NodeId(91), None);
        assert_eq!(
            board.transition_origin,
            Some(TileGeometry::from_terminal_rect(opened))
        );
        assert!(!board.is_transitioning());

        board.change_area(Rect::new(0, 0, 100, 24));

        assert_eq!(board.transition_origin, None);
        assert_eq!(
            board.rendered_tiles().first().map(TileGeometry::from_tile),
            Some(old_geometry),
            "a resize must start from the geometry already shown"
        );
        assert!(
            board.is_transitioning(),
            "the resize still deserves a routine tween"
        );
    }

    #[test]
    fn zoom_relayout_does_not_reuse_a_stationary_drill_pivot() {
        let initial = vec![file(1, 0.5), file(2, 0.5)];
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files_for_view(initial.clone(), NodeId(90), None);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("the initial map has a cursor");

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files_for_view(initial, NodeId(91), None);
        assert!(!board.is_transitioning());

        let zoomed = vec![file(10, 0.5), file(11, 0.3), file(12, 0.2)];
        board.zoom_in(zoomed);

        assert_eq!(board.transition_origin, None);
        let zoomed_tile = board
            .tiles
            .iter()
            .find(|tile| tile.node_id == NodeId(12))
            .map(TileGeometry::from_tile)
            .expect("the zoomed map has a third entry");
        assert_eq!(
            board
                .rendered_tiles()
                .iter()
                .find(|tile| tile.node_id == NodeId(12))
                .map(TileGeometry::from_tile),
            Some(zoomed_tile),
            "a zoomed-in entry must not start at a stale drill pivot"
        );
    }

    #[test]
    fn stationary_retained_identity_does_not_restart_at_the_active_pivot() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("the parent map has a cursor");
        let pivot = TileGeometry::from_terminal_rect(opened);
        let child = vec![file(10, 0.6), file(11, 0.4)];

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files(child.clone());
        board.advance_geometry(Duration::ZERO, false);
        board.advance_geometry(Duration::from_millis(80), false);
        assert!(board.is_transitioning());
        assert!(
            !board.departing_tiles().is_empty(),
            "the parent departures keep this drill active during the refresh"
        );

        let (stationary_id, target) = board
            .tiles
            .iter()
            .map(|tile| (tile.node_id, TileGeometry::from_tile(tile)))
            .find(|(_, geometry)| *geometry != pivot)
            .expect("the child fixture needs a target distinct from the pivot");
        let rendered = board
            .rendered_tiles
            .iter_mut()
            .find(|tile| tile.node_id == stationary_id)
            .expect("the retained child must be on screen");
        rendered.x = target.x;
        rendered.y = target.y;
        rendered.width = target.width;
        rendered.height = target.height;

        board.change_files(child);
        assert!(
            board
                .transition_from
                .iter()
                .any(|(node, origin)| *node == stationary_id && *origin == target),
            "a retained target-equal identity must keep its origin during an active drill"
        );
        board.advance_geometry(Duration::from_millis(80), false);

        assert_eq!(
            board
                .rendered_tiles()
                .iter()
                .find(|tile| tile.node_id == stationary_id)
                .map(TileGeometry::from_tile),
            Some(target),
            "a stationary retained identity must not jump back to the drill pivot"
        );
    }

    #[test]
    fn unresolved_return_pivot_discards_departures_and_reveals_overflow() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("the parent map has a cursor");

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files(vec![file(10, 0.6), file(11, 0.4)]);
        board.advance_geometry(Duration::ZERO, false);
        board.advance_geometry(Duration::from_millis(80), false);
        assert!(board.is_transitioning());
        assert!(
            !board.departing_tiles().is_empty(),
            "the active drill must have departures to discard"
        );

        board.pivot_transition_on(Pivot::Entry(NodeId(1)));
        board.change_files(vec![file_with_size(1, 100, 0.0)]);

        assert!(
            board.files.iter().any(|file| file.node_id == NodeId(1)),
            "the return folder remains in the parent dataset"
        );
        assert!(board.tiles.is_empty(), "the return folder is overflow-only");
        assert!(board.overflow().is_some());
        assert!(!board.is_transitioning());
        assert!(board.departing_tiles().is_empty());
        assert!(board.transition_from.is_empty());
        assert_eq!(board.transition_origin, None);
        assert_eq!(board.rendered_overflow(), board.overflow());
    }

    #[test]
    fn a_dataset_refresh_mid_tween_retargets_from_current_geometry_and_keeps_deadline() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.7), file(2, 0.3)]);
        board.advance_geometry(Duration::ZERO, true);

        board.change_area(Rect::new(0, 0, 120, 30));
        board.advance_geometry(Duration::from_millis(500), false);
        board.advance_geometry(Duration::from_millis(580), false);
        let visible = board
            .rendered_tiles()
            .iter()
            .find(|tile| tile.node_id == NodeId(1))
            .map(TileGeometry::from_tile)
            .expect("the visible entry survives the refresh");

        board.change_files(vec![file(1, 0.3), file(2, 0.7)]);
        let target = board
            .tiles
            .iter()
            .find(|tile| tile.node_id == NodeId(1))
            .map(TileGeometry::from_tile)
            .expect("the refreshed entry remains in the target layout");
        assert_ne!(
            target, visible,
            "the refresh must actually retarget the entry"
        );
        assert_eq!(
            board
                .rendered_tiles()
                .iter()
                .find(|tile| tile.node_id == NodeId(1))
                .map(TileGeometry::from_tile),
            Some(visible)
        );

        // Re-rendering at the frame that supplied the origin must not apply the
        // old absolute elapsed fraction to that new origin.
        board.advance_geometry(Duration::from_millis(580), false);
        assert_eq!(
            board
                .rendered_tiles()
                .iter()
                .find(|tile| tile.node_id == NodeId(1))
                .map(TileGeometry::from_tile),
            Some(visible)
        );

        board.advance_geometry(Duration::from_millis(660), false);
        assert!(
            !board.is_transitioning(),
            "the original deadline still applies"
        );
        assert_eq!(
            board
                .rendered_tiles()
                .iter()
                .find(|tile| tile.node_id == NodeId(1))
                .map(TileGeometry::from_tile),
            Some(target)
        );
    }

    #[test]
    fn rendered_geometry_keeps_tile_identity_and_settles_exactly_once() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.5), file(2, 0.3), file(3, 0.2)]);
        board.advance_geometry(Duration::ZERO, true);
        assert!(!board.is_transitioning());

        board.change_files(vec![file(2, 0.6), file(3, 0.4)]);
        assert!(board.is_transitioning());

        for step in [10, 40, 90, 140] {
            board.advance_geometry(Duration::from_millis(step), false);
            let identities: Vec<NodeId> = board
                .rendered_tiles()
                .iter()
                .map(|tile| tile.node_id)
                .collect();
            let expected: Vec<NodeId> = board.tiles.iter().map(|tile| tile.node_id).collect();
            assert_eq!(identities, expected);
        }

        board.advance_geometry(Duration::from_millis(200), false);
        assert!(!board.is_transitioning());
        for (rendered, target) in board.rendered_tiles().iter().zip(board.tiles.iter()) {
            assert_eq!(
                (rendered.x, rendered.y, rendered.width, rendered.height),
                (target.x, target.y, target.width, target.height)
            );
        }
    }

    #[test]
    fn narrow_list_layout_scrolls_without_losing_identity() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 60, 3));
        board.change_files((1..=5).map(|id| file(id, 0.2)).collect());
        board.advance_geometry(Duration::ZERO, true);
        assert_rendered_identity_alignment(&board);
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
    fn return_selection_reveals_an_off_page_list_pivot() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 60, 3));
        let files: Vec<_> = (1..=5).map(|id| file(id, 0.2)).collect();
        board.change_files(files.clone());
        board.advance_geometry(Duration::ZERO, true);

        board.pivot_transition_on(Pivot::Entry(NodeId(5)));
        board.change_files(files);
        assert_eq!(board.pending_pivot, Some(Pivot::Entry(NodeId(5))));

        assert!(board.select_node(NodeId(5)));
        assert_eq!(board.list_offset, 2);
        assert_eq!(
            board.currently_selected().map(|tile| tile.node_id),
            Some(NodeId(5))
        );
        assert_eq!(board.pending_pivot, None);
    }

    #[test]
    fn overflow_summarizes_every_unrendered_entry_and_clears_when_they_fit() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 72, 1));
        board.change_files(vec![
            file_with_size(1, 11, 0.4),
            file_with_size(2, 22, 0.3),
            file_with_size(3, 33, 0.2),
            file_with_size(4, 44, 0.1),
        ]);

        assert!(board.tiles.is_empty(), "the pane cannot draw any entries");
        assert_eq!(
            board.overflow(),
            Some(MapOverflow {
                x: 0,
                y: 0,
                entries: 4,
                bytes: 110,
                uncertain: false,
            })
        );

        board.change_area(Rect::new(0, 0, 60, 1));
        assert!(board.is_list_layout());
        assert_eq!(board.overflow(), None);

        board.change_area(Rect::new(0, 0, 200, 80));
        assert_eq!(board.overflow(), None);
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
        let y = u16::try_from(first.top_row()).expect("the test pane fits in a terminal row");
        assert!(board.select_at(first.x.saturating_add(1), y));
        assert_eq!(
            board.currently_selected().map(|tile| tile.node_id),
            Some(NodeId(1))
        );
    }

    #[test]
    fn pointer_selects_the_tile_visible_mid_tween() {
        let mut board = reordered_board_mid_tween();
        let Some((x, y)) = board
            .rendered_tiles()
            .iter()
            .find(|tile| tile.node_id == NodeId(1))
            .map(|tile| {
                (
                    tile.x.saturating_add(1),
                    u16::try_from(tile.top_row()).expect("the test pane fits in a terminal row"),
                )
            })
        else {
            panic!("the moving entry must remain visible");
        };

        assert_eq!(
            board
                .tiles
                .iter()
                .find(|tile| x >= tile.x
                    && x < tile.x.saturating_add(tile.width)
                    && tile.covers_row(u32::from(y)))
                .map(|tile| tile.node_id),
            Some(NodeId(2)),
            "the target geometry deliberately puts the other entry under the pointer"
        );
        assert!(board.select_at(x, y));
        assert_eq!(
            board.currently_selected().map(|tile| tile.node_id),
            Some(NodeId(1))
        );
    }

    #[test]
    fn pointer_ignores_retained_geometry_outside_the_current_board_area() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 100, 30));
        board.change_files(vec![file(1, 1.0)]);
        board.advance_geometry(Duration::ZERO, true);

        board.change_area(Rect::new(20, 10, 80, 20));
        assert!(board.is_transitioning());
        let retained = &board.rendered_tiles()[0];
        assert!(
            retained.x <= 1
                && 1 < retained.x.saturating_add(retained.width)
                && retained.covers_row(0)
        );
        board.reset_selected_index();

        assert!(
            !board.select_at(1, 0),
            "the old transition rectangle is clipped outside the new board area"
        );
        assert_eq!(board.selected_index, None);
    }

    #[test]
    fn pointer_matches_transition_paint_order_for_overlapping_tiles() {
        let mut board = reordered_board_mid_tween();
        board.advance_geometry(Duration::from_millis(40), false);
        let (beneath_node, above_node, x, y) = {
            let beneath = &board.rendered_tiles()[0];
            let above = &board.rendered_tiles()[1];
            let x = beneath.x.max(above.x);
            let y = beneath.top_row().max(above.top_row());
            assert!(
                x < beneath.x.saturating_add(beneath.width)
                    && x < above.x.saturating_add(above.width)
                    && y < beneath.bottom_row()
                    && y < above.bottom_row(),
                "the reordered tiles must overlap mid-transition"
            );
            (beneath.node_id, above.node_id, x, y)
        };
        let y = u16::try_from(y).expect("the test pane fits in a terminal row");

        board.reset_selected_index();
        assert!(board.select_at(x, y));
        assert_eq!(
            board.currently_selected().map(|tile| tile.node_id),
            Some(above_node),
            "the later-painted source entry is on top without a selection"
        );

        assert!(board.select_node(beneath_node));
        assert!(board.select_at(x, y));
        assert_eq!(
            board.currently_selected().map(|tile| tile.node_id),
            Some(beneath_node),
            "the selected transition entry is painted after its peers"
        );
    }

    #[test]
    fn pointer_prefers_selected_tile_at_a_settled_half_row_boundary() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
        board.advance_geometry(Duration::ZERO, true);

        // Adjacent half-rows rasterize into the same terminal cell. DenseGrid
        // paints the selected sibling last even after motion has settled.
        board.tiles[0].x = 0;
        board.tiles[0].y = 0;
        board.tiles[0].width = 1;
        board.tiles[0].height = 1;
        board.tiles[1].x = 0;
        board.tiles[1].y = 1;
        board.tiles[1].width = 1;
        board.tiles[1].height = 1;
        board.settle_geometry();
        board.set_selected_index(0);

        assert!(board.rendered_tiles()[0].covers_row(0));
        assert!(board.rendered_tiles()[1].covers_row(0));
        assert!(board.select_at(0, 0));
        assert_eq!(
            board.currently_selected().map(|tile| tile.node_id),
            Some(NodeId(1)),
            "the selected tile is painted above the later source sibling"
        );
    }

    #[test]
    fn rendered_overflow_stays_visible_when_geometry_is_stable() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![
            file_with_size(1, 11, 0.4),
            file_with_size(2, 22, 0.3),
            file_with_size(3, 33, 0.2),
            file_with_size(4, 44, 0.1),
        ]);
        board.advance_geometry(Duration::ZERO, true);

        board.change_area(Rect::new(0, 0, 72, 1));
        assert!(!board.is_transitioning());
        assert_eq!(board.rendered_overflow(), board.overflow());
        assert_eq!(
            board.rendered_overflow(),
            Some(MapOverflow {
                x: 0,
                y: 0,
                entries: 4,
                bytes: 110,
                uncertain: false,
            })
        );
    }

    #[test]
    fn unchanged_refresh_keeps_overflow_rendered_without_a_transition() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        let mut files = vec![file(1, 0.8)];
        files.extend((2..=101).map(|id| file(id, 0.002)));
        board.change_files(files.clone());
        board.advance_geometry(Duration::ZERO, true);
        assert!(!board.tiles.is_empty());
        let overflow = board
            .rendered_overflow()
            .expect("the tiny entries require an overflow summary");

        board.change_files(files);

        assert!(!board.is_transitioning());
        assert_eq!(board.rendered_overflow(), Some(overflow));
    }

    #[test]
    fn disjoint_refresh_settles_without_hiding_overflow() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        let mut current = vec![file(1, 0.8)];
        current.extend((2..=101).map(|id| file(id, 0.002)));
        board.change_files(current);
        board.advance_geometry(Duration::ZERO, true);
        let overflow = board
            .rendered_overflow()
            .expect("the compact map needs an overflow summary");

        let mut replacement = vec![file(102, 0.8)];
        replacement.extend((103..=202).map(|id| file(id, 0.002)));
        board.change_files(replacement);

        assert!(!board.is_transitioning());
        assert!(board.departing_tiles().is_empty());
        assert_eq!(board.rendered_overflow(), Some(overflow));
        assert_rendered_identity_alignment(&board);
    }

    #[test]
    fn selected_rendered_rect_tracks_the_tween_until_it_settles() {
        let mut board = reordered_board_mid_tween();
        assert_eq!(
            board.currently_selected().map(|tile| tile.node_id),
            Some(NodeId(1))
        );
        assert_ne!(board.selected_rendered_rect(), board.selected_rect());

        board.settle_geometry();
        assert_eq!(board.selected_rendered_rect(), board.selected_rect());
    }

    #[test]
    fn reversing_a_drill_preserves_visible_parent_origins() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("the parent map has a cursor");
        let pivot = TileGeometry::from_terminal_rect(opened);

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files(vec![file(10, 0.6), file(11, 0.4)]);
        board.advance_geometry(Duration::ZERO, false);
        board.advance_geometry(Duration::from_millis(80), false);
        let visible_sibling = board
            .departing_tiles()
            .iter()
            .find(|tile| tile.node_id == NodeId(2))
            .map(TileGeometry::from_tile)
            .expect("the parent sibling should still be visible");
        assert_ne!(visible_sibling, pivot);

        board.pivot_transition_on(Pivot::Entry(NodeId(1)));
        board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
        let returning_sibling = board
            .rendered_tiles()
            .iter()
            .find(|tile| tile.node_id == NodeId(2))
            .map(TileGeometry::from_tile)
            .expect("the returning sibling should be rendered");
        assert_eq!(returning_sibling, visible_sibling);
    }

    #[test]
    fn list_fallback_reveals_largest_entry_after_selection_disappears() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 40, 3));
        board.change_files((1..=5).map(|id| file(id, 0.2)).collect());
        for _ in 0..4 {
            board.move_selected_down();
        }
        assert_eq!(
            board.currently_selected().map(|tile| tile.node_id),
            Some(NodeId(5))
        );
        assert_eq!(board.list_offset, 2);

        board.change_files((1..=4).map(|id| file(id, 0.25)).collect());

        assert_eq!(
            board.currently_selected().map(|tile| tile.node_id),
            Some(NodeId(1))
        );
        assert_eq!(board.list_offset, 0);
        assert_eq!(board.tiles[0].node_id, NodeId(1));
    }

    #[test]
    fn changing_dataset_view_clears_stationary_drill_pivot() {
        let initial = vec![file(1, 0.5), file(2, 0.5)];
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files_for_view(initial.clone(), NodeId(90), None);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("the initial map has a cursor");

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files_for_view(initial.clone(), NodeId(91), None);
        assert!(board.transition_origin.is_some());

        board.change_files_for_view(vec![file(3, 0.5), file(4, 0.5)], NodeId(92), None);

        assert_eq!(board.transition_origin, None);
        assert_eq!(board.transition_span, crate::animation::ROUTINE_MOTION);
        assert!(!board.is_transitioning());
        assert_rendered_identity_alignment(&board);
    }

    #[test]
    fn cancelling_a_drill_clears_departing_tiles_with_the_pivot() {
        let child = vec![file(10, 0.6), file(11, 0.4)];
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files_for_view(vec![file(1, 0.5), file(2, 0.5)], NodeId(90), None);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("the parent map has a cursor");

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files_for_view(child.clone(), NodeId(91), None);
        board.advance_geometry(Duration::ZERO, false);
        board.advance_geometry(Duration::from_millis(80), false);
        assert!(!board.departing_tiles().is_empty());

        board.change_files_for_view(child, NodeId(92), None);

        assert!(board.departing_tiles().is_empty());
        assert!(board.departing_from.is_empty());
        assert_eq!(board.transition_origin, None);
    }

    #[test]
    fn active_drill_retarget_keeps_newly_removed_tiles_departing() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 0.5), file(2, 0.5)]);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("the parent map has a cursor");

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files(vec![file(10, 0.6), file(11, 0.4)]);
        board.advance_geometry(Duration::ZERO, false);
        board.advance_geometry(Duration::from_millis(80), false);

        board.change_files(vec![file(10, 1.0)]);

        assert!(
            board
                .departing_tiles()
                .iter()
                .any(|tile| tile.node_id == NodeId(11)),
            "a rendered child omitted by a refresh must keep receding"
        );
    }

    #[test]
    fn a_stationary_late_arrival_keeps_the_drill_pivot_for_later_entries() {
        let mut board = Board::new();
        board.change_area(Rect::new(0, 0, 80, 24));
        board.change_files(vec![file(1, 1.0)]);
        board.advance_geometry(Duration::ZERO, true);
        let opened = board.selected_rect().expect("the parent map has a cursor");
        let pivot = TileGeometry::from_terminal_rect(opened);

        board.pivot_transition_on(Pivot::Rect(opened));
        board.change_files(Vec::new());
        assert_eq!(board.transition_origin, Some(pivot));

        board.change_files(vec![file(10, 1.0)]);
        assert_eq!(board.transition_origin, Some(pivot));

        board.change_files(vec![file(10, 0.5), file(11, 0.5)]);
        let late = board
            .rendered_tiles()
            .iter()
            .find(|tile| tile.node_id == NodeId(11))
            .map(TileGeometry::from_tile)
            .expect("the later entry should be rendered");
        assert_eq!(late, pivot);
    }
}
