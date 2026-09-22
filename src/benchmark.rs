//! Internal Criterion fixtures for canonical scan-store measurements.

use std::path::{Path, PathBuf};

use crate::model::{ByteBounds, EntrySnapshot, NodeKind};
use crate::native_path::NativeIdentity;
use crate::scan_coordinator::{RelativePath, ScanGeneration};
use crate::scan_store::identity_observation::IdentityObservation;
use crate::scan_store::page::{PageCursor, PageRequest};
use crate::scan_store::path_reducer::{Coverage, PathEntryKind, PathObservation, SummaryMetrics};
use crate::scan_store::session::{MAX_OBSERVATIONS_PER_BATCH, ScanStore};
use crate::temporary_storage::TemporaryStorage;

const STORAGE_MIB: usize = 256;
const QUERY_PAGE_ENTRIES: usize = 32;

/// Canonical layouts that exercise distinct storage and query paths.
#[derive(Clone, Copy, Debug)]
pub enum CanonicalWorkload {
    Flat {
        entries: usize,
    },
    Fanout {
        directories: usize,
        leaves_per_directory: usize,
    },
    Deep {
        depth: usize,
        leaves: usize,
    },
    SharedLinks {
        groups: usize,
        links_per_group: usize,
    },
}

impl CanonicalWorkload {
    #[must_use]
    pub const fn entry_count(self) -> usize {
        match self {
            Self::Flat { entries } => entries,
            Self::Fanout {
                directories,
                leaves_per_directory,
            } => directories.saturating_mul(leaves_per_directory.saturating_add(1)),
            Self::Deep { depth, leaves } => depth.saturating_add(leaves),
            Self::SharedLinks {
                groups,
                links_per_group,
            } => groups.saturating_mul(links_per_group),
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Flat { .. } => "flat",
            Self::Fanout { .. } => "fanout",
            Self::Deep { .. } => "deep",
            Self::SharedLinks { .. } => "shared-links",
        }
    }
}

/// A published canonical generation and one representative bounded page query.
pub struct CanonicalStoreBenchmark {
    store: ScanStore,
    storage: TemporaryStorage,
    query: BenchmarkPageQuery,
}

#[derive(Clone)]
struct BenchmarkPageQuery {
    folder: RelativePath,
    after: Option<PageCursor>,
}

impl CanonicalStoreBenchmark {
    /// Builds and publishes a deterministic canonical workload.
    ///
    /// # Panics
    ///
    /// Panics when the benchmark's fixed scan-store budget cannot hold its
    /// generated workload or canonical publication fails.
    #[must_use]
    pub fn build(workload: CanonicalWorkload) -> Self {
        let storage = TemporaryStorage::scan_store_from_mib(STORAGE_MIB)
            .expect("benchmark scan-store capacity should be valid");
        let mut store = ScanStore::new(ScanGeneration::initial(), storage.clone())
            .expect("benchmark store should initialize");
        let (paths, identities, query) = observations_for(workload);
        let mut paths = paths.into_iter();
        let mut identities = identities.into_iter();
        loop {
            let paths = paths
                .by_ref()
                .take(MAX_OBSERVATIONS_PER_BATCH)
                .collect::<Vec<_>>();
            let identities = identities
                .by_ref()
                .take(MAX_OBSERVATIONS_PER_BATCH)
                .collect::<Vec<_>>();
            if paths.is_empty() && identities.is_empty() {
                break;
            }
            store
                .append_observation_batch(paths, identities)
                .expect("benchmark facts should append");
        }
        store
            .publish()
            .expect("benchmark generation should publish");
        Self {
            store,
            storage,
            query,
        }
    }

    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        self.storage.used()
    }

    /// Reads a representative bounded page without rebuilding the generation.
    ///
    /// # Panics
    ///
    /// Panics if the benchmark fixture no longer retains its published generation
    /// or the generated representative page cannot be read.
    #[must_use]
    pub fn query_representative_page(&mut self) -> usize {
        let request = self.query.after.as_ref().map_or_else(
            || PageRequest::first(self.query.folder.clone(), QUERY_PAGE_ENTRIES),
            |after| {
                PageRequest::after(self.query.folder.clone(), after.clone(), QUERY_PAGE_ENTRIES)
            },
        );
        self.store
            .published_mut()
            .expect("benchmark generation should remain published")
            .page(request)
            .expect("benchmark page should load")
            .entries
            .len()
    }
}

fn observations_for(
    workload: CanonicalWorkload,
) -> (
    Vec<PathObservation>,
    Vec<IdentityObservation>,
    BenchmarkPageQuery,
) {
    match workload {
        CanonicalWorkload::Flat { entries } => flat_observations(entries),
        CanonicalWorkload::Fanout {
            directories,
            leaves_per_directory,
        } => fanout_observations(directories, leaves_per_directory),
        CanonicalWorkload::Deep { depth, leaves } => deep_observations(depth, leaves),
        CanonicalWorkload::SharedLinks {
            groups,
            links_per_group,
        } => shared_link_observations(groups, links_per_group),
    }
}

fn flat_observations(
    entries: usize,
) -> (
    Vec<PathObservation>,
    Vec<IdentityObservation>,
    BenchmarkPageQuery,
) {
    let mut paths = Vec::with_capacity(entries);
    let mut identities = Vec::new();
    for index in 0..entries {
        let (path, identity) = file_observation(relative_path(index), index, 1);
        paths.push(path);
        identities.extend(identity);
    }
    let after = entries
        .checked_sub(QUERY_PAGE_ENTRIES.saturating_add(1))
        .map(|index| PageCursor::at(relative_path(index), 1));
    (
        paths,
        identities,
        BenchmarkPageQuery {
            folder: RelativePath::root(),
            after,
        },
    )
}

fn fanout_observations(
    directories: usize,
    leaves_per_directory: usize,
) -> (
    Vec<PathObservation>,
    Vec<IdentityObservation>,
    BenchmarkPageQuery,
) {
    let mut paths = Vec::with_capacity(directories.saturating_mul(leaves_per_directory + 1));
    let mut identities = Vec::new();
    let mut file_index = 0_usize;
    for directory_index in 0..directories {
        let directory = branch_path(directory_index);
        paths.push(directory_observation(directory.clone(), directory_index));
        for leaf_index in 0..leaves_per_directory {
            let leaf = directory
                .to_path_buf()
                .join(format!("leaf-{leaf_index:08}"));
            let relative =
                RelativePath::from_path(&leaf).expect("benchmark path should be relative");
            let (path, identity) = file_observation(relative, file_index, 1);
            paths.push(path);
            identities.extend(identity);
            file_index = file_index.saturating_add(1);
        }
    }
    let after = directories
        .checked_sub(QUERY_PAGE_ENTRIES.saturating_add(1))
        .map(|index| {
            PageCursor::at(
                branch_path(index),
                u128::try_from(leaves_per_directory).unwrap_or(u128::MAX),
            )
        });
    (
        paths,
        identities,
        BenchmarkPageQuery {
            folder: RelativePath::root(),
            after,
        },
    )
}

fn deep_observations(
    depth: usize,
    leaves: usize,
) -> (
    Vec<PathObservation>,
    Vec<IdentityObservation>,
    BenchmarkPageQuery,
) {
    let mut paths = Vec::with_capacity(depth.saturating_add(leaves));
    let mut identities = Vec::new();
    let mut directory = PathBuf::new();
    for level in 0..depth {
        directory.push(format!("level-{level:08}"));
        let relative =
            RelativePath::from_path(&directory).expect("benchmark path should be relative");
        paths.push(directory_observation(relative, level));
    }
    let folder = RelativePath::from_path(&directory).expect("benchmark path should be relative");
    for leaf_index in 0..leaves {
        let leaf = directory.join(format!("leaf-{leaf_index:08}"));
        let relative = RelativePath::from_path(&leaf).expect("benchmark path should be relative");
        let (path, identity) = file_observation(relative, leaf_index, 1);
        paths.push(path);
        identities.extend(identity);
    }
    let after = leaves
        .checked_sub(QUERY_PAGE_ENTRIES.saturating_add(1))
        .map(|index| {
            let path = directory.join(format!("leaf-{index:08}"));
            PageCursor::at(
                RelativePath::from_path(&path).expect("benchmark path should be relative"),
                1,
            )
        });
    (paths, identities, BenchmarkPageQuery { folder, after })
}

fn shared_link_observations(
    groups: usize,
    links_per_group: usize,
) -> (
    Vec<PathObservation>,
    Vec<IdentityObservation>,
    BenchmarkPageQuery,
) {
    let mut paths = Vec::with_capacity(groups.saturating_mul(links_per_group));
    let mut identities = Vec::with_capacity(groups.saturating_mul(links_per_group));
    for group in 0..groups {
        for link in 0..links_per_group {
            let path = PathBuf::from(format!("link-{group:08}-{link:04}"));
            let relative =
                RelativePath::from_path(&path).expect("benchmark path should be relative");
            let (observation, identity) = file_observation(relative, group, links_per_group);
            paths.push(observation);
            identities.extend(identity);
        }
    }
    (
        paths,
        identities,
        BenchmarkPageQuery {
            folder: RelativePath::root(),
            after: None,
        },
    )
}

fn directory_observation(path: RelativePath, index: usize) -> PathObservation {
    PathObservation::with_snapshot(
        path,
        PathEntryKind::Directory,
        SummaryMetrics::leaf(0, ByteBounds::exact(0), ByteBounds::exact(0)),
        Coverage::Complete,
        Some(EntrySnapshot {
            identity: Some(NativeIdentity {
                file_id: file_id::FileId::new_inode(2, u64::try_from(index).unwrap_or(u64::MAX)),
                link_count: Some(1),
                reparse_point: false,
            }),
            kind: NodeKind::Directory,
            apparent_bytes: 0,
            allocated_bytes: None,
            modified_nanos: Some(1),
        }),
    )
}

fn file_observation(
    path: RelativePath,
    index: usize,
    link_count: usize,
) -> (PathObservation, Option<IdentityObservation>) {
    let file_id = file_id::FileId::new_inode(1, u64::try_from(index).unwrap_or(u64::MAX));
    let link_count = u64::try_from(link_count).unwrap_or(u64::MAX);
    let single_link = link_count == 1;
    let allocated = single_link.then_some(ByteBounds::exact(1));
    let observation = PathObservation::with_snapshot(
        path.clone(),
        PathEntryKind::File,
        SummaryMetrics::leaf(
            1,
            allocated.unwrap_or_else(|| ByteBounds::exact(0)),
            allocated.unwrap_or_else(|| ByteBounds::exact(0)),
        ),
        Coverage::Complete,
        Some(EntrySnapshot {
            identity: Some(NativeIdentity {
                file_id,
                link_count: Some(link_count),
                reparse_point: false,
            }),
            kind: NodeKind::File,
            apparent_bytes: 1,
            allocated_bytes: Some(1),
            modified_nanos: Some(1),
        }),
    );
    let identity = (!single_link).then_some(IdentityObservation {
        path,
        file_id,
        declared_links: Some(link_count),
        allocated_bytes: ByteBounds::exact(1),
    });
    (observation, identity)
}

fn relative_path(index: usize) -> RelativePath {
    let name = format!("entry-{index:08}");
    RelativePath::from_path(Path::new(&name)).expect("benchmark path should be relative")
}

fn branch_path(index: usize) -> RelativePath {
    let name = format!("branch-{index:08}");
    RelativePath::from_path(Path::new(&name)).expect("benchmark path should be relative")
}
