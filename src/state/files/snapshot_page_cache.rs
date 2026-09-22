use std::collections::VecDeque;

use crate::scan_coordinator::{RelativePath, ScanGeneration};
use crate::scan_store::page::PageCursor;

use super::snapshot_tree::SnapshotTree;

/// Retain only a small LRU set of immutable direct-child page views.
const MAX_CACHED_SNAPSHOT_PAGES: usize = 8;

struct CachedSnapshotPage {
    generation: ScanGeneration,
    folder: RelativePath,
    after: Option<PageCursor>,
    bytes: usize,
    tree: SnapshotTree,
}

/// Bounded cache for the current immutable page and recently visited siblings.
///
/// The canonical `ScanStore` remains the source of truth. This cache only
/// avoids rebuilding page-local presentation trees while users move back and
/// forth through pages or folders.
pub(crate) struct SnapshotPageCache {
    current: SnapshotTree,
    current_after: Option<PageCursor>,
    inactive: VecDeque<CachedSnapshotPage>,
    inactive_bytes: usize,
    inactive_budget_bytes: usize,
}

impl SnapshotPageCache {
    #[must_use]
    pub(crate) fn new(current: SnapshotTree, current_after: Option<PageCursor>) -> Self {
        let inactive_budget_bytes = current.page_cache_budget_bytes();
        Self {
            current,
            current_after,
            inactive: VecDeque::with_capacity(MAX_CACHED_SNAPSHOT_PAGES),
            inactive_bytes: 0,
            inactive_budget_bytes,
        }
    }

    #[must_use]
    pub(crate) const fn current(&self) -> &SnapshotTree {
        &self.current
    }

    pub(crate) fn current_mut(&mut self) -> &mut SnapshotTree {
        &mut self.current
    }

    #[must_use]
    pub(crate) fn current_after(&self) -> Option<&PageCursor> {
        self.current_after.as_ref()
    }

    /// Activates a cached page only when it belongs to the current immutable generation.
    pub(crate) fn activate(
        &mut self,
        generation: ScanGeneration,
        folder: &RelativePath,
        after: Option<&PageCursor>,
    ) -> bool {
        if self.matches_current(generation, folder, after) {
            return true;
        }
        let Some(index) = self.inactive.iter().position(|entry| {
            entry.generation == generation
                && entry.folder == *folder
                && entry.after.as_ref() == after
        }) else {
            return false;
        };
        let cached = self
            .inactive
            .remove(index)
            .expect("located cached snapshot page should remain present");
        self.inactive_bytes = self.inactive_bytes.saturating_sub(cached.bytes);
        let previous = CachedSnapshotPage {
            generation: self.current.generation(),
            folder: self.current.current_relative().clone(),
            after: self.current_after.take(),
            bytes: self.current.retained_bytes(),
            tree: std::mem::replace(&mut self.current, cached.tree),
        };
        self.current_after = cached.after;
        self.insert_inactive(previous);
        true
    }

    /// Replaces the active page and retains the prior page only within the same generation.
    pub(crate) fn install(&mut self, tree: SnapshotTree, after: Option<PageCursor>) {
        if tree.generation() != self.current.generation() {
            self.clear();
            self.inactive_budget_bytes = tree.page_cache_budget_bytes();
            self.current = tree;
            self.current_after = after;
            return;
        }
        if self.matches_current(tree.generation(), tree.current_relative(), after.as_ref()) {
            self.current = tree;
            self.current_after = after;
            return;
        }
        let previous = CachedSnapshotPage {
            generation: self.current.generation(),
            folder: self.current.current_relative().clone(),
            after: self.current_after.take(),
            bytes: self.current.retained_bytes(),
            tree: std::mem::replace(&mut self.current, tree),
        };
        self.current_after = after;
        self.insert_inactive(previous);
    }

    /// Drops inactive pages while retaining the currently displayed page.
    pub(crate) fn clear(&mut self) {
        self.inactive.clear();
        self.inactive_bytes = 0;
    }

    fn matches_current(
        &self,
        generation: ScanGeneration,
        folder: &RelativePath,
        after: Option<&PageCursor>,
    ) -> bool {
        self.current.generation() == generation
            && self.current.current_relative() == folder
            && self.current_after.as_ref() == after
    }

    fn insert_inactive(&mut self, page: CachedSnapshotPage) {
        if page.bytes > self.inactive_budget_bytes {
            return;
        }
        if let Some(index) = self.inactive.iter().position(|entry| {
            entry.generation == page.generation
                && entry.folder == page.folder
                && entry.after == page.after
        }) {
            let removed = self
                .inactive
                .remove(index)
                .expect("located cached snapshot page should remain present");
            self.inactive_bytes = self.inactive_bytes.saturating_sub(removed.bytes);
        }
        while self.inactive.len() == MAX_CACHED_SNAPSHOT_PAGES
            || self.inactive_bytes.saturating_add(page.bytes) > self.inactive_budget_bytes
        {
            let Some(removed) = self.inactive.pop_front() else {
                break;
            };
            self.inactive_bytes = self.inactive_bytes.saturating_sub(removed.bytes);
        }
        self.inactive_bytes = self.inactive_bytes.saturating_add(page.bytes);
        self.inactive.push_back(page);
    }

    #[cfg(test)]
    #[must_use]
    fn inactive_len(&self) -> usize {
        self.inactive.len()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::ByteBounds;
    use crate::scan_store::page::{PageCursor, ScanPage};
    use crate::scan_store::path_reducer::{Coverage, SummaryMetrics};

    fn tree(generation: u64, folder: &str) -> SnapshotTree {
        SnapshotTree::from_page(
            PathBuf::from("/scan"),
            ScanPage {
                generation: ScanGeneration::from_value(generation),
                folder: RelativePath::from_path(std::path::Path::new(folder))
                    .expect("fixture folder should be relative"),
                folder_metrics: SummaryMetrics::default(),
                folder_coverage: Coverage::Complete,
                root_metrics: SummaryMetrics::leaf(0, ByteBounds::exact(0), ByteBounds::exact(0)),
                root_coverage: Coverage::Complete,
                entries: Vec::new(),
                next_after: None,
                shared_allocation: None,
                unrecorded_path_count: 0,
            },
            64 * 1024,
            (0, 4 * 1024 * 1024),
        )
        .expect("fixture page should fit")
    }

    fn path(value: &str) -> RelativePath {
        RelativePath::from_path(std::path::Path::new(value))
            .expect("fixture path should be relative")
    }

    #[test]
    fn cache_reactivates_a_page_without_growing_past_its_fixed_bound() {
        let mut cache = SnapshotPageCache::new(tree(0, ""), None);
        cache.install(tree(0, "alpha"), None);
        assert_eq!(cache.inactive_len(), 1);
        assert!(cache.activate(ScanGeneration::initial(), &RelativePath::root(), None));
        assert_eq!(cache.current().current_relative(), &RelativePath::root());
        let cursor = PageCursor::at(path("entry"), 1);
        cache.install(tree(0, ""), Some(cursor.clone()));
        assert!(cache.activate(ScanGeneration::initial(), &RelativePath::root(), None));
        assert!(cache.current_after().is_none());
        assert!(cache.activate(
            ScanGeneration::initial(),
            &RelativePath::root(),
            Some(&cursor),
        ));
        assert_eq!(cache.current_after(), Some(&cursor));

        for index in 0..=MAX_CACHED_SNAPSHOT_PAGES {
            cache.install(tree(0, &format!("page-{index}")), None);
        }
        assert_eq!(cache.inactive_len(), MAX_CACHED_SNAPSHOT_PAGES);
        assert!(cache.inactive_bytes <= cache.inactive_budget_bytes);

        cache.install(tree(1, "fresh"), None);
        assert_eq!(cache.current().generation(), ScanGeneration::from_value(1));
        assert_eq!(
            cache.inactive_len(),
            0,
            "new generations must evict stale pages"
        );
        assert!(!cache.activate(ScanGeneration::initial(), &path("alpha"), None));
    }
}
