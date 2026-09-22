use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::mem::size_of;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use file_id::FileId;
use redb::{
    Builder as RedbBuilder, Database, Durability, ReadableDatabase, ReadableTable, TableDefinition,
};
#[cfg(not(windows))]
use tempfile::{Builder as TempBuilder, TempDir};

use super::{ByteBounds, ModelError, NodeId};
use crate::temporary_storage::{BoundedFileBackend, TemporaryStorage, TemporaryStorageReservation};

const IDENTITIES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("identities");
const IDENTITY_ENTRY_OVERHEAD: usize = size_of::<IdentityRecord>() + 96;
pub const SESSION_PREFIX: &str = ".excise-session-";
const SESSION_MARKER_FILE: &str = ".excise-session";
const IDENTITY_DATABASE_FILE: &str = "identities.redb";
const SESSION_MARKER_HEADER: &str = "excise-spill-session-v1";
const MAX_MARKER_BYTES: u64 = 256;
const STALE_SESSION_AGE: Duration = Duration::from_mins(15);
const MAX_CLEANUP_CANDIDATES: usize = 64;
const MAX_SESSION_ENTRIES: usize = 2;
#[cfg(windows)]
const SESSION_CREATE_ATTEMPTS: usize = 32;
const DISK_WRITE_BATCH: usize = 256;
const MIGRATION_RECORDS_PER_OBSERVATION: usize = 8;

const FILE_ID_INODE_TAG: u8 = 0;
const FILE_ID_LOW_RES_TAG: u8 = 1;
const FILE_ID_HIGH_RES_TAG: u8 = 2;
const IDENTITY_RECORD_VERSION: u8 = 1;
const IDENTITY_RECORD_HAS_DECLARED_LINKS: u8 = 1;
const IDENTITY_RECORD_DECLARED_LINKS_ONE: u8 = 1 << 1;
const IDENTITY_RECORD_HAS_ALLOCATED_UPPER: u8 = 1 << 2;
const IDENTITY_RECORD_ALLOCATED_UPPER_EQUALS_LOWER: u8 = 1 << 3;
const IDENTITY_RECORD_HAS_ALLOCATION_NODE: u8 = 1 << 4;
const IDENTITY_RECORD_IMPLICIT_SINGLE_PARTICIPANT: u8 = 1 << 5;
const IDENTITY_RECORD_IMPLICIT_SINGLE_OBSERVATION: u8 = 1 << 6;
const IDENTITY_RECORD_KNOWN_FLAGS: u8 = IDENTITY_RECORD_HAS_DECLARED_LINKS
    | IDENTITY_RECORD_DECLARED_LINKS_ONE
    | IDENTITY_RECORD_HAS_ALLOCATED_UPPER
    | IDENTITY_RECORD_ALLOCATED_UPPER_EQUALS_LOWER
    | IDENTITY_RECORD_HAS_ALLOCATION_NODE
    | IDENTITY_RECORD_IMPLICIT_SINGLE_PARTICIPANT
    | IDENTITY_RECORD_IMPLICIT_SINGLE_OBSERVATION;
const IDENTITY_RECORD_NODE_BYTES: usize = size_of::<u32>() + size_of::<u64>();

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentityRecord {
    pub observed_links: u64,
    pub declared_links: Option<u64>,
    pub allocated_bytes: ByteBounds,
    pub allocation_node: Option<NodeId>,
    /// Distinct participant nodes paired with their exact observed link counts.
    pub nodes: Vec<(NodeId, u64)>,
}

impl IdentityRecord {
    fn observe_node(&mut self, node: NodeId) {
        match self
            .nodes
            .binary_search_by_key(&node, |(existing, _)| *existing)
        {
            Ok(index) => {
                self.nodes[index].1 = self.nodes[index].1.saturating_add(1);
            }
            Err(index) => self.nodes.insert(index, (node, 1)),
        }
    }

    pub(crate) fn coalesce_nodes(&mut self) {
        self.nodes.sort_unstable_by_key(|(node, _)| *node);
        let mut retained = 0_usize;
        for index in 0..self.nodes.len() {
            let (node, links) = self.nodes[index];
            if retained > 0 && self.nodes[retained - 1].0 == node {
                let (_, retained_links) = &mut self.nodes[retained - 1];
                *retained_links = retained_links.saturating_add(links);
            } else {
                self.nodes[retained] = (node, links);
                retained += 1;
            }
        }
        self.nodes.truncate(retained);
    }

    fn unavailable() -> Self {
        Self {
            observed_links: 0,
            declared_links: None,
            allocated_bytes: ByteBounds::unknown(),
            allocation_node: None,
            nodes: Vec::new(),
        }
    }
}

pub struct IdentityStore {
    storage: Storage,
    // Rebuilds can begin after another private identity database fills the
    // shared temporary-storage budget. In that case this store remains
    // deliberately untracked instead of making structural scan updates fail.
    session: Option<SessionDirectory>,
    memory_limit: usize,
    estimated_bytes: usize,
    capacity_exhausted: bool,
    #[cfg(test)]
    broad_remap_scans: usize,
}

/// Keeps a declared link count exact only while every observation agrees.
pub(crate) fn merge_declared_links(current: Option<u64>, observed: Option<u64>) -> Option<u64> {
    match (current, observed) {
        (Some(current), Some(observed)) if current == observed => Some(current),
        _ => None,
    }
}
enum Storage {
    Memory(HashMap<FileId, IdentityRecord>),
    /// Existing in-memory records drain into a freshly initialized database in
    /// bounded maintenance steps, while new observations go straight to disk.
    Migrating {
        database: Database,
        count: usize,
        pending: HashMap<Vec<u8>, IdentityRecord>,
        records: HashMap<FileId, IdentityRecord>,
    },
    Disk {
        database: Database,
        count: usize,
        pending: HashMap<Vec<u8>, IdentityRecord>,
    },
}

struct SessionDirectory {
    path: PathBuf,
    marker: SessionMarker,
    #[cfg(not(windows))]
    temporary: TempDir,
    #[cfg(windows)]
    handle: crate::os::windows::PrivateDirectoryHandle,
    // Retained for Drop so marker bytes remain charged for this session's lifetime.
    _marker_reservation: TemporaryStorageReservation,
    database_reservation: Arc<Mutex<TemporaryStorageReservation>>,
    database_capacity_exhausted: Arc<AtomicBool>,
}

impl Drop for IdentityStore {
    fn drop(&mut self) {
        let storage = std::mem::replace(&mut self.storage, Storage::Memory(HashMap::new()));
        drop(storage);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionMarker {
    pid: u32,
    started_at: u64,
    nonce: String,
}

impl SessionDirectory {
    fn new(temporary_storage: &TemporaryStorage) -> Result<Self, ModelError> {
        let database_capacity_exhausted = Arc::new(AtomicBool::new(false));
        #[cfg(not(windows))]
        {
            let marker = SessionMarker::new()?;
            let marker_contents = marker.serialize();
            let marker_bytes = u64::try_from(marker_contents.len()).map_err(|_| {
                ModelError::Identity("spill session marker size does not fit in u64".to_string())
            })?;
            let marker_reservation = temporary_storage
                .reservation(marker_bytes)
                .map_err(identity_error)?;
            let database_reservation = Arc::new(Mutex::new(
                temporary_storage.reservation(0).map_err(identity_error)?,
            ));
            let temporary = TempBuilder::new()
                .prefix(SESSION_PREFIX)
                .rand_bytes(32)
                .tempdir()
                .map_err(identity_error)?;
            let path = temporary.path().to_path_buf();
            restrict_private_directory(&path)?;
            verify_private_directory(&path)?;
            write_session_marker(&path, &marker_contents)?;
            verify_active_session(&path, &marker)?;
            Ok(Self {
                path,
                marker,
                temporary,
                _marker_reservation: marker_reservation,
                database_reservation,
                database_capacity_exhausted,
            })
        }

        #[cfg(windows)]
        {
            let parent = std::env::temp_dir();
            for _ in 0..SESSION_CREATE_ATTEMPTS {
                let marker = SessionMarker::new()?;
                let marker_contents = marker.serialize();
                let marker_bytes = u64::try_from(marker_contents.len()).map_err(|_| {
                    ModelError::Identity(
                        "spill session marker size does not fit in u64".to_string(),
                    )
                })?;
                let marker_reservation = temporary_storage
                    .reservation(marker_bytes)
                    .map_err(identity_error)?;
                let database_reservation = Arc::new(Mutex::new(
                    temporary_storage.reservation(0).map_err(identity_error)?,
                ));
                let path = parent.join(format!("{SESSION_PREFIX}{}", random_session_token()?));
                let mut handle = match crate::os::windows::create_private_directory(&path) {
                    Ok(handle) => handle,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(identity_error(error)),
                };
                if let Err(error) = write_session_marker(&path, &marker_contents)
                    .and_then(|()| verify_active_session(&path, &marker))
                {
                    return match remove_verified_session_held(&path, &mut handle) {
                        Ok(()) => Err(error),
                        Err(cleanup_error) => Err(ModelError::Identity(format!(
                            "{error}; private spill-session cleanup also failed: {cleanup_error}"
                        ))),
                    };
                }
                return Ok(Self {
                    path,
                    marker,
                    handle,
                    _marker_reservation: marker_reservation,
                    database_reservation,
                    database_capacity_exhausted,
                });
            }
            Err(ModelError::Identity(
                "could not allocate a unique private spill directory".to_string(),
            ))
        }
    }

    fn path(&self) -> &Path {
        #[cfg(not(windows))]
        let _ = &self.temporary;
        &self.path
    }

    fn is_verified(&self) -> bool {
        verify_active_session(&self.path, &self.marker).is_ok()
    }

    fn database_reservation(&self) -> Arc<Mutex<TemporaryStorageReservation>> {
        Arc::clone(&self.database_reservation)
    }

    fn database_capacity_signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.database_capacity_exhausted)
    }

    fn database_capacity_exhausted(&self) -> bool {
        self.database_capacity_exhausted.load(Ordering::Acquire)
    }

    fn remove_database(&self) -> Result<(), ModelError> {
        let database = self.path.join(IDENTITY_DATABASE_FILE);
        match fs::symlink_metadata(&database) {
            Ok(_) => {
                verify_private_file(&database)?;
                #[cfg(windows)]
                crate::os::windows::delete_verified_private_file(&database)
                    .map_err(identity_error)?;
                #[cfg(not(windows))]
                fs::remove_file(&database).map_err(identity_error)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(identity_error(error)),
        }
        let mut reservation = self
            .database_reservation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reservation.shrink_to(0);
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for SessionDirectory {
    fn drop(&mut self) {
        self.handle.close();
        if let Err(_error) = remove_verified_session(&self.path) {
            // Drop cannot report errors. The path remains private and is left
            // for a later bounded startup cleanup attempt.
        }
    }
}

#[allow(
    clippy::missing_errors_doc,
    reason = "IdentityStore methods expose one ModelError boundary for serialization, private-session validation, and disk-backed persistence."
)]
impl IdentityStore {
    pub fn new(memory_limit: usize) -> Result<Self, ModelError> {
        let temporary_storage = TemporaryStorage::default();
        Self::new_with_temporary_storage(memory_limit, &temporary_storage)
    }

    pub(crate) fn new_with_temporary_storage(
        memory_limit: usize,
        temporary_storage: &TemporaryStorage,
    ) -> Result<Self, ModelError> {
        cleanup_stale_sessions_once();
        let session = match SessionDirectory::new(temporary_storage) {
            Ok(session) => Some(session),
            Err(error) if is_temporary_storage_capacity_error(&error) => None,
            Err(error) => return Err(error),
        };
        let capacity_exhausted = session.is_none();
        Ok(Self {
            storage: Storage::Memory(HashMap::new()),
            session,
            memory_limit,
            estimated_bytes: 0,
            capacity_exhausted,
            #[cfg(test)]
            broad_remap_scans: 0,
        })
    }

    pub fn observe(
        &mut self,
        file_id: &FileId,
        declared_links: Option<u64>,
        allocated_bytes: ByteBounds,
        node: Option<NodeId>,
        allocation_node: Option<NodeId>,
    ) -> Result<(bool, IdentityRecord), ModelError> {
        if self.disable_if_capacity_exhausted()? {
            return Ok((false, IdentityRecord::unavailable()));
        }
        let result = (|| -> Result<(bool, IdentityRecord), ModelError> {
            let existing = self.get(file_id)?;
            if self.capacity_exhausted {
                return Ok((false, IdentityRecord::unavailable()));
            }
            let is_new = existing.is_none();
            let previous = existing.as_ref().map_or(0, estimated_identity_record_bytes);
            let mut record = existing.unwrap_or(IdentityRecord {
                observed_links: 0,
                declared_links,
                allocated_bytes,
                allocation_node,
                nodes: Vec::new(),
            });
            record.observed_links = record.observed_links.saturating_add(1);
            record.declared_links = merge_declared_links(record.declared_links, declared_links);
            if let Some(node) = node {
                record.observe_node(node);
            }

            if matches!(self.storage, Storage::Memory(_)) {
                self.estimated_bytes = self
                    .estimated_bytes
                    .saturating_sub(previous)
                    .saturating_add(estimated_identity_record_bytes(&record));
                if self.estimated_bytes > self.memory_limit {
                    self.spill_to_disk()?;
                }
            }
            self.insert(file_id, &record, is_new)?;
            Ok((is_new, record))
        })();
        let Some(observation) = self.recover_capacity(result)? else {
            return Ok((false, IdentityRecord::unavailable()));
        };
        let _ = self.advance_migration(MIGRATION_RECORDS_PER_OBSERVATION)?;
        if self.capacity_exhausted {
            Ok((false, IdentityRecord::unavailable()))
        } else {
            Ok(observation)
        }
    }

    pub fn get(&mut self, file_id: &FileId) -> Result<Option<IdentityRecord>, ModelError> {
        if self.disable_if_capacity_exhausted()? {
            return Ok(None);
        }
        let result = match &self.storage {
            Storage::Memory(records) => Ok(records.get(file_id).cloned()),
            Storage::Migrating {
                records,
                database,
                pending,
                ..
            } => {
                if let Some(record) = records.get(file_id) {
                    Ok(Some(record.clone()))
                } else {
                    read_identity_record(database, pending, file_id)
                }
            }
            Storage::Disk {
                database, pending, ..
            } => read_identity_record(database, pending, file_id),
        };
        Ok(self.recover_capacity(result)?.flatten())
    }

    #[must_use]
    pub fn is_spilled(&self) -> bool {
        matches!(
            self.storage,
            Storage::Migrating { .. } | Storage::Disk { .. }
        )
    }

    #[must_use]
    pub(crate) const fn capacity_exhausted(&self) -> bool {
        self.capacity_exhausted
    }

    #[cfg(test)]
    pub(crate) fn signal_database_capacity_exhaustion_for_test(&self) {
        self.session
            .as_ref()
            .expect("test spill session should exist")
            .database_capacity_exhausted
            .store(true, Ordering::Release);
    }

    #[must_use]
    pub fn spill_path(&self) -> Option<&Path> {
        self.session
            .as_ref()
            .filter(|_| self.is_spilled())
            .map(SessionDirectory::path)
    }
    #[cfg(test)]
    pub(crate) fn corrupt_spill_record_for_test(
        &mut self,
        file_id: &FileId,
    ) -> Result<(), ModelError> {
        self.flush_pending()?;
        let database = match &mut self.storage {
            Storage::Migrating { database, .. } | Storage::Disk { database, .. } => database,
            Storage::Memory(_) => {
                return Err(ModelError::Invariant(
                    "identity store did not spill".to_string(),
                ));
            }
        };
        let key = encode_file_id(file_id);
        let transaction = database.begin_write().map_err(identity_error)?;
        {
            let mut table = transaction.open_table(IDENTITIES).map_err(identity_error)?;
            table
                .insert(key.as_slice(), &b"corrupt"[..])
                .map_err(identity_error)?;
        }
        transaction.commit().map_err(identity_error)
    }

    #[must_use]
    pub fn internal_scan_paths(&self) -> Vec<PathBuf> {
        self.session
            .as_ref()
            .filter(|session| session.is_verified())
            .map_or_else(Vec::new, |session| vec![session.path().to_path_buf()])
    }

    #[must_use]
    pub fn len(&self) -> usize {
        if self.capacity_exhausted {
            return 0;
        }
        match &self.storage {
            Storage::Memory(records) => records.len(),
            Storage::Migrating { count, .. } | Storage::Disk { count, .. } => *count,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub(crate) const fn memory_limit(&self) -> usize {
        self.memory_limit
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn broad_remap_scans(&self) -> usize {
        self.broad_remap_scans
    }

    pub(crate) fn visit_records(
        &mut self,
        mut visitor: impl FnMut(FileId, IdentityRecord) -> Result<(), ModelError>,
    ) -> Result<(), ModelError> {
        if self.disable_if_capacity_exhausted()? {
            return Ok(());
        }
        let flush = self.flush_pending();
        if self.recover_capacity(flush)?.is_none() {
            return Ok(());
        }
        let result = match &self.storage {
            Storage::Memory(records) => {
                for (file_id, record) in records {
                    visitor(*file_id, record.clone())?;
                }
                Ok(())
            }
            Storage::Migrating {
                records, database, ..
            } => {
                for (file_id, record) in records {
                    visitor(*file_id, record.clone())?;
                }
                visit_disk_records(database, &mut visitor)
            }
            Storage::Disk { database, .. } => visit_disk_records(database, &mut visitor),
        };
        let _ = self.recover_capacity(result)?;
        Ok(())
    }

    pub(crate) fn upsert_record(
        &mut self,
        file_id: &FileId,
        record: &IdentityRecord,
    ) -> Result<Option<IdentityRecord>, ModelError> {
        if self.disable_if_capacity_exhausted()? {
            return Ok(None);
        }
        let result = (|| -> Result<Option<IdentityRecord>, ModelError> {
            let existing = self.get(file_id)?;
            if self.capacity_exhausted {
                return Ok(None);
            }
            if matches!(self.storage, Storage::Memory(_)) {
                let previous = existing.as_ref().map_or(0, estimated_identity_record_bytes);
                self.estimated_bytes = self
                    .estimated_bytes
                    .saturating_sub(previous)
                    .saturating_add(estimated_identity_record_bytes(record));
                if self.estimated_bytes > self.memory_limit {
                    self.spill_to_disk()?;
                }
            }
            self.insert(file_id, record, existing.is_none())?;
            Ok(existing)
        })();
        let Some(existing) = self.recover_capacity(result)? else {
            return Ok(None);
        };
        let _ = self.advance_migration(MIGRATION_RECORDS_PER_OBSERVATION)?;
        if self.capacity_exhausted {
            Ok(None)
        } else {
            Ok(existing)
        }
    }
    pub(crate) fn refresh_declared_links(
        &mut self,
        file_id: &FileId,
        declared_links: Option<u64>,
    ) -> Result<(), ModelError> {
        let Some(mut record) = self.get(file_id)? else {
            return Ok(());
        };
        record.declared_links = declared_links;
        self.upsert_record(file_id, &record).map(|_| ())
    }

    /// Repoints a known subset of identities using one bounded database pass per batch.
    ///
    /// The caller supplies a sorted removal set. Records represented by an
    /// aggregate cannot be identified from their removed nodes and must use
    /// [`Self::remap_removed_nodes`] instead.
    pub(crate) fn remap_nodes_for_identities(
        &mut self,
        file_ids: &[FileId],
        removed: &[NodeId],
        replacement: NodeId,
    ) -> Result<(), ModelError> {
        if self.disable_if_capacity_exhausted()? || file_ids.is_empty() || removed.is_empty() {
            return Ok(());
        }
        let result = match &mut self.storage {
            Storage::Memory(records) => {
                for file_id in file_ids {
                    if let Some(record) = records.get_mut(file_id) {
                        remap_record_nodes(record, removed, replacement);
                    }
                }
                Ok(())
            }
            Storage::Migrating {
                records,
                database,
                pending,
                ..
            } => {
                let mut persisted_ids = Vec::with_capacity(file_ids.len());
                for file_id in file_ids {
                    if let Some(record) = records.get_mut(file_id) {
                        remap_record_nodes(record, removed, replacement);
                    } else {
                        persisted_ids.push(*file_id);
                    }
                }
                for file_ids in persisted_ids.chunks(DISK_WRITE_BATCH) {
                    remap_disk_records(database, pending, file_ids, removed, replacement)?;
                }
                Ok(())
            }
            Storage::Disk {
                database, pending, ..
            } => {
                for file_ids in file_ids.chunks(DISK_WRITE_BATCH) {
                    remap_disk_records(database, pending, file_ids, removed, replacement)?;
                }
                Ok(())
            }
        };
        let _ = self.recover_capacity(result)?;
        Ok(())
    }

    /// Repoints stored participants that will be structurally aggregated.
    ///
    /// `removed` is sorted in place so both in-memory and spilled records can
    /// perform bounded membership checks without materializing the store.
    pub(crate) fn remap_removed_nodes(
        &mut self,
        removed: &mut [NodeId],
        replacement: NodeId,
    ) -> Result<(), ModelError> {
        if self.disable_if_capacity_exhausted()? || removed.is_empty() {
            return Ok(());
        }

        #[cfg(test)]
        if self.is_spilled() {
            self.broad_remap_scans = self.broad_remap_scans.saturating_add(1);
        }
        let result = (|| -> Result<(), ModelError> {
            removed.sort_unstable();
            if self.is_spilled() {
                self.flush_pending()?;
            }
            match &mut self.storage {
                Storage::Memory(records) => {
                    for record in records.values_mut() {
                        remap_record_nodes(record, removed, replacement);
                    }
                    Ok(())
                }
                Storage::Migrating {
                    records, database, ..
                } => {
                    for record in records.values_mut() {
                        remap_record_nodes(record, removed, replacement);
                    }
                    remap_all_disk_records(database, removed, replacement)
                }
                Storage::Disk { database, .. } => {
                    remap_all_disk_records(database, removed, replacement)
                }
            }
        })();
        let _ = self.recover_capacity(result)?;
        Ok(())
    }

    fn spill_to_disk(&mut self) -> Result<(), ModelError> {
        if !matches!(&self.storage, Storage::Memory(_)) {
            return Ok(());
        }
        let database = self.create_spill_database()?;
        let records = match std::mem::replace(&mut self.storage, Storage::Memory(HashMap::new())) {
            Storage::Memory(records) => records,
            storage => {
                self.storage = storage;
                return Ok(());
            }
        };
        self.estimated_bytes = 0;
        self.storage = Storage::Migrating {
            database,
            count: records.len(),
            pending: HashMap::new(),
            records,
        };
        Ok(())
    }

    /// Moves at most `maximum_records` pre-spill observations into the disk batch.
    ///
    /// A spill begins at the memory boundary, so eagerly rewriting every old
    /// record would freeze the owner loop precisely when the scan is busiest.
    pub(crate) fn advance_migration(&mut self, maximum_records: usize) -> Result<bool, ModelError> {
        if self.disable_if_capacity_exhausted()? || maximum_records == 0 {
            return Ok(false);
        }
        let result = (|| -> Result<bool, ModelError> {
            let Storage::Migrating {
                database,
                pending,
                records,
                ..
            } = &mut self.storage
            else {
                return Ok(false);
            };
            let mut migrated = false;
            for _ in 0..maximum_records {
                let Some(file_id) = records.keys().next().copied() else {
                    break;
                };
                let record = records
                    .remove(&file_id)
                    .expect("migration key must still identify a record");
                let key = encode_file_id(&file_id);
                pending.insert(key, record);
                migrated = true;
                if pending.len() >= DISK_WRITE_BATCH {
                    flush_pending_records(database, pending)?;
                }
            }
            Ok(migrated)
        })();
        let Some(migrated) = self.recover_capacity(result)? else {
            return Ok(false);
        };
        self.finish_migration_if_empty();
        Ok(migrated)
    }

    fn finish_migration_if_empty(&mut self) {
        if !matches!(&self.storage, Storage::Migrating { records, .. } if records.is_empty()) {
            return;
        }
        let storage = std::mem::replace(&mut self.storage, Storage::Memory(HashMap::new()));
        let Storage::Migrating {
            database,
            count,
            pending,
            ..
        } = storage
        else {
            unreachable!("only a finished migration may be finalized");
        };
        self.storage = Storage::Disk {
            database,
            count,
            pending,
        };
    }

    fn create_spill_database(&self) -> Result<Database, ModelError> {
        let session = self.session.as_ref().ok_or_else(|| {
            ModelError::Invariant("untracked identity store must not spill".to_string())
        })?;
        let path = session.path().join(IDENTITY_DATABASE_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(identity_error)?;
        restrict_private_file(&path)?;
        verify_private_file(&path)?;

        let cache_size = (self.memory_limit / 4).clamp(64 * 1024, 16 * 1024 * 1024);
        let mut builder = RedbBuilder::new();
        builder.set_cache_size(cache_size);
        let backend = BoundedFileBackend::new(
            file,
            session.database_reservation(),
            session.database_capacity_signal(),
        )
        .map_err(identity_error)?;
        let database = builder
            .create_with_backend(backend)
            .map_err(identity_error)?;
        let transaction = begin_ephemeral_write(&database)?;
        {
            let _table = transaction.open_table(IDENTITIES).map_err(identity_error)?;
        }
        transaction.commit().map_err(identity_error)?;
        Ok(database)
    }

    fn disable_for_capacity(&mut self) -> Result<(), ModelError> {
        let storage = std::mem::replace(&mut self.storage, Storage::Memory(HashMap::new()));
        drop(storage);
        self.estimated_bytes = 0;
        self.capacity_exhausted = true;
        self.session
            .as_ref()
            .map_or(Ok(()), SessionDirectory::remove_database)
    }

    /// Releases the private database as soon as its bounded backend signals a
    /// capacity breach. The signal can precede the redb error that exposes it.
    fn disable_if_capacity_exhausted(&mut self) -> Result<bool, ModelError> {
        if !self.capacity_exhausted
            && self
                .session
                .as_ref()
                .is_some_and(SessionDirectory::database_capacity_exhausted)
        {
            self.disable_for_capacity()?;
        }
        Ok(self.capacity_exhausted)
    }

    fn recover_capacity<T>(
        &mut self,
        result: Result<T, ModelError>,
    ) -> Result<Option<T>, ModelError> {
        if self.disable_if_capacity_exhausted()? {
            return Ok(None);
        }
        match result {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                // A redb I/O failure can surface after the backend's capacity
                // signal was lost with its failed transaction. Its exact bounded
                // storage diagnostic remains sufficient to retire this private,
                // reconstructible identity cache without aborting the scan.
                if !self.capacity_exhausted && is_temporary_storage_capacity_error(&error) {
                    self.disable_for_capacity()?;
                }
                if self.disable_if_capacity_exhausted()? {
                    Ok(None)
                } else {
                    Err(error)
                }
            }
        }
    }

    fn insert(
        &mut self,
        file_id: &FileId,
        record: &IdentityRecord,
        is_new: bool,
    ) -> Result<(), ModelError> {
        let should_flush = match &mut self.storage {
            Storage::Memory(records) => {
                records.insert(*file_id, record.clone());
                false
            }
            Storage::Migrating {
                records,
                count,
                pending,
                ..
            } => {
                if records.contains_key(file_id) {
                    records.insert(*file_id, record.clone());
                    false
                } else {
                    let key = encode_file_id(file_id);
                    pending.insert(key, record.clone());
                    if is_new {
                        *count = count.saturating_add(1);
                    }
                    pending.len() >= DISK_WRITE_BATCH
                }
            }
            Storage::Disk { count, pending, .. } => {
                let key = encode_file_id(file_id);
                pending.insert(key, record.clone());
                if is_new {
                    *count = count.saturating_add(1);
                }
                pending.len() >= DISK_WRITE_BATCH
            }
        };
        if should_flush {
            self.flush_pending()?;
        }
        Ok(())
    }

    fn flush_pending(&mut self) -> Result<(), ModelError> {
        match &mut self.storage {
            Storage::Migrating {
                database, pending, ..
            }
            | Storage::Disk {
                database, pending, ..
            } => flush_pending_records(database, pending),
            Storage::Memory(_) => Ok(()),
        }
    }
}

/// Identity records are a private, reconstructible scan cache. Keep their live
/// transactions atomic, but do not synchronously persist every scan batch from
/// the UI owner thread. Redb retains a consistent reader-visible state and a
/// crashed session is discarded rather than reused.
fn begin_ephemeral_write(database: &Database) -> Result<redb::WriteTransaction, ModelError> {
    let mut transaction = database.begin_write().map_err(identity_error)?;
    transaction
        .set_durability(Durability::None)
        .map_err(identity_error)?;
    Ok(transaction)
}

fn read_identity_record(
    database: &Database,
    pending: &HashMap<Vec<u8>, IdentityRecord>,
    file_id: &FileId,
) -> Result<Option<IdentityRecord>, ModelError> {
    let key = encode_file_id(file_id);
    if let Some(record) = pending.get(&key) {
        return Ok(Some(record.clone()));
    }
    let transaction = database.begin_read().map_err(identity_error)?;
    let table = transaction.open_table(IDENTITIES).map_err(identity_error)?;
    table
        .get(key.as_slice())
        .map_err(identity_error)?
        .map(|value| decode_identity_record(value.value()))
        .transpose()
}

fn visit_disk_records(
    database: &Database,
    visitor: &mut impl FnMut(FileId, IdentityRecord) -> Result<(), ModelError>,
) -> Result<(), ModelError> {
    let transaction = database.begin_read().map_err(identity_error)?;
    let table = transaction.open_table(IDENTITIES).map_err(identity_error)?;
    for entry in table.iter().map_err(identity_error)? {
        let (key, value) = entry.map_err(identity_error)?;
        visitor(
            decode_file_id(key.value())?,
            decode_identity_record(value.value())?,
        )?;
    }
    Ok(())
}

fn flush_pending_records(
    database: &Database,
    pending: &mut HashMap<Vec<u8>, IdentityRecord>,
) -> Result<(), ModelError> {
    if pending.is_empty() {
        return Ok(());
    }
    let transaction = begin_ephemeral_write(database)?;
    {
        let mut table = transaction.open_table(IDENTITIES).map_err(identity_error)?;
        for (key, record) in pending.iter() {
            let value = encode_identity_record(record)?;
            table
                .insert(key.as_slice(), value.as_slice())
                .map_err(identity_error)?;
        }
    }
    transaction.commit().map_err(identity_error)?;
    pending.clear();
    Ok(())
}

fn remap_disk_records(
    database: &Database,
    pending: &mut HashMap<Vec<u8>, IdentityRecord>,
    file_ids: &[FileId],
    removed: &[NodeId],
    replacement: NodeId,
) -> Result<(), ModelError> {
    let mut updates = Vec::with_capacity(file_ids.len());
    {
        let transaction = database.begin_read().map_err(identity_error)?;
        let table = transaction.open_table(IDENTITIES).map_err(identity_error)?;
        for file_id in file_ids {
            let key = encode_file_id(file_id);
            if let Some(record) = pending.get_mut(&key) {
                remap_record_nodes(record, removed, replacement);
                continue;
            }
            let Some(value) = table.get(key.as_slice()).map_err(identity_error)? else {
                continue;
            };
            let mut record = decode_identity_record(value.value())?;
            if remap_record_nodes(&mut record, removed, replacement) {
                updates.push((key, record));
            }
        }
    }
    for (key, record) in updates {
        pending.insert(key, record);
        if pending.len() >= DISK_WRITE_BATCH {
            flush_pending_records(database, pending)?;
        }
    }
    Ok(())
}

fn remap_all_disk_records(
    database: &Database,
    removed: &[NodeId],
    replacement: NodeId,
) -> Result<(), ModelError> {
    let mut resume_after = None;
    loop {
        let (updates, last_key, has_more) = {
            let transaction = database.begin_read().map_err(identity_error)?;
            let table = transaction.open_table(IDENTITIES).map_err(identity_error)?;
            let mut entries = match resume_after.as_deref() {
                Some(key) => table
                    .range::<&[u8]>((Bound::Excluded(key), Bound::Unbounded))
                    .map_err(identity_error)?,
                None => table.iter().map_err(identity_error)?,
            };
            let mut updates = Vec::new();
            let mut last_key = None;
            for _ in 0..DISK_WRITE_BATCH {
                let Some(entry) = entries.next() else {
                    break;
                };
                let (key, value) = entry.map_err(identity_error)?;
                let key = key.value().to_vec();
                let mut record = decode_identity_record(value.value())?;
                if remap_record_nodes(&mut record, removed, replacement) {
                    updates.push((key.clone(), encode_identity_record(&record)?));
                }
                last_key = Some(key);
            }
            let has_more = match last_key.as_deref() {
                Some(last_key) => table
                    .range::<&[u8]>((Bound::Excluded(last_key), Bound::Unbounded))
                    .map_err(identity_error)?
                    .next()
                    .transpose()
                    .map_err(identity_error)?
                    .is_some(),
                None => false,
            };
            (updates, last_key, has_more)
        };
        if !updates.is_empty() {
            let transaction = begin_ephemeral_write(database)?;
            {
                let mut table = transaction.open_table(IDENTITIES).map_err(identity_error)?;
                for (key, value) in updates {
                    table
                        .insert(key.as_slice(), value.as_slice())
                        .map_err(identity_error)?;
                }
            }
            transaction.commit().map_err(identity_error)?;
        }
        if !has_more {
            return Ok(());
        }
        let Some(last_key) = last_key else {
            return Err(ModelError::Invariant(
                "identity store iteration advanced without a key".to_string(),
            ));
        };
        resume_after = Some(last_key);
    }
}

fn remap_record_nodes(
    record: &mut IdentityRecord,
    removed: &[NodeId],
    replacement: NodeId,
) -> bool {
    let mut changed = false;
    for (node, _) in &mut record.nodes {
        if removed.binary_search(node).is_ok() {
            *node = replacement;
            changed = true;
        }
    }
    if changed {
        record.coalesce_nodes();
    }
    if record
        .allocation_node
        .is_some_and(|node| removed.binary_search(&node).is_ok())
    {
        record.allocation_node = Some(replacement);
        changed = true;
    }
    changed
}

fn is_temporary_storage_capacity_error(error: &ModelError) -> bool {
    matches!(
        error,
        ModelError::Identity(message) if message.contains("temporary storage capacity exhausted")
    )
}

fn identity_error(error: impl std::fmt::Display) -> ModelError {
    ModelError::Identity(error.to_string())
}

fn estimated_identity_record_bytes(record: &IdentityRecord) -> usize {
    size_of::<FileId>()
        .saturating_add(IDENTITY_ENTRY_OVERHEAD)
        .saturating_add(
            record
                .nodes
                .capacity()
                .saturating_mul(size_of::<(NodeId, u64)>()),
        )
}

fn encode_file_id(file_id: &FileId) -> Vec<u8> {
    match *file_id {
        FileId::Inode {
            device_id,
            inode_number,
        } => {
            let mut encoded = Vec::with_capacity(1 + 2 * size_of::<u64>());
            encoded.push(FILE_ID_INODE_TAG);
            push_u64(&mut encoded, device_id);
            push_u64(&mut encoded, inode_number);
            encoded
        }
        FileId::LowRes {
            volume_serial_number,
            file_index,
        } => {
            let mut encoded = Vec::with_capacity(1 + size_of::<u32>() + size_of::<u64>());
            encoded.push(FILE_ID_LOW_RES_TAG);
            push_u32(&mut encoded, volume_serial_number);
            push_u64(&mut encoded, file_index);
            encoded
        }
        FileId::HighRes {
            volume_serial_number,
            file_id,
        } => {
            let mut encoded = Vec::with_capacity(1 + size_of::<u64>() + size_of::<u128>());
            encoded.push(FILE_ID_HIGH_RES_TAG);
            push_u64(&mut encoded, volume_serial_number);
            push_u128(&mut encoded, file_id);
            encoded
        }
    }
}

fn decode_file_id(encoded: &[u8]) -> Result<FileId, ModelError> {
    let (&tag, payload) = encoded
        .split_first()
        .ok_or_else(|| invalid_identity_spill("key"))?;
    match tag {
        FILE_ID_INODE_TAG if payload.len() == 2 * size_of::<u64>() => {
            let mut payload = payload;
            Ok(FileId::new_inode(
                take_u64(&mut payload, "key")?,
                take_u64(&mut payload, "key")?,
            ))
        }
        FILE_ID_LOW_RES_TAG if payload.len() == size_of::<u32>() + size_of::<u64>() => {
            let mut payload = payload;
            Ok(FileId::new_low_res(
                take_u32(&mut payload, "key")?,
                take_u64(&mut payload, "key")?,
            ))
        }
        FILE_ID_HIGH_RES_TAG if payload.len() == size_of::<u64>() + size_of::<u128>() => {
            let mut payload = payload;
            Ok(FileId::new_high_res(
                take_u64(&mut payload, "key")?,
                take_u128(&mut payload, "key")?,
            ))
        }
        _ => Err(invalid_identity_spill("key")),
    }
}

fn encode_identity_record(record: &IdentityRecord) -> Result<Vec<u8>, ModelError> {
    let implicit_single_participant = matches!(
        (record.allocation_node, record.nodes.as_slice()),
        (Some(allocation_node), [(node, 1)]) if allocation_node == *node
    );
    let node_count = (!implicit_single_participant)
        .then(|| u32::try_from(record.nodes.len()))
        .transpose()
        .map_err(|_| ModelError::Identity("identity record has too many nodes".to_string()))?;
    let mut flags = 0_u8;
    if let Some(declared_links) = record.declared_links {
        flags |= IDENTITY_RECORD_HAS_DECLARED_LINKS;
        if declared_links == 1 {
            flags |= IDENTITY_RECORD_DECLARED_LINKS_ONE;
        }
    }
    if let Some(upper) = record.allocated_bytes.upper {
        flags |= IDENTITY_RECORD_HAS_ALLOCATED_UPPER;
        if upper == record.allocated_bytes.lower {
            flags |= IDENTITY_RECORD_ALLOCATED_UPPER_EQUALS_LOWER;
        }
    }
    if record.allocation_node.is_some() {
        flags |= IDENTITY_RECORD_HAS_ALLOCATION_NODE;
    }
    if implicit_single_participant {
        flags |= IDENTITY_RECORD_IMPLICIT_SINGLE_PARTICIPANT;
    }
    if record.observed_links == 1 {
        flags |= IDENTITY_RECORD_IMPLICIT_SINGLE_OBSERVATION;
    }
    let capacity = 2_usize
        .saturating_add(size_of::<u128>())
        .saturating_add(if record.observed_links == 1 {
            0
        } else {
            size_of::<u64>()
        })
        .saturating_add(
            record
                .declared_links
                .map_or(0, |links| if links == 1 { 0 } else { size_of::<u64>() }),
        )
        .saturating_add(record.allocated_bytes.upper.map_or(0, |upper| {
            if upper == record.allocated_bytes.lower {
                0
            } else {
                size_of::<u128>()
            }
        }))
        .saturating_add(record.allocation_node.map_or(0, |_| size_of::<u32>()))
        .saturating_add(node_count.map_or(0, |_| size_of::<u32>()))
        .saturating_add(node_count.map_or(0, |_| {
            record
                .nodes
                .len()
                .saturating_mul(IDENTITY_RECORD_NODE_BYTES)
        }));
    let mut encoded = Vec::with_capacity(capacity);
    encoded.push(IDENTITY_RECORD_VERSION);
    encoded.push(flags);
    if record.observed_links != 1 {
        push_u64(&mut encoded, record.observed_links);
    }
    if let Some(declared_links) = record.declared_links
        && declared_links != 1
    {
        push_u64(&mut encoded, declared_links);
    }
    push_u128(&mut encoded, record.allocated_bytes.lower);
    if let Some(upper) = record.allocated_bytes.upper
        && upper != record.allocated_bytes.lower
    {
        push_u128(&mut encoded, upper);
    }
    if let Some(allocation_node) = record.allocation_node {
        push_u32(&mut encoded, allocation_node.0);
    }
    if let Some(node_count) = node_count {
        push_u32(&mut encoded, node_count);
        for (node, links) in &record.nodes {
            push_u32(&mut encoded, node.0);
            push_u64(&mut encoded, *links);
        }
    }
    Ok(encoded)
}

fn decode_identity_record(mut encoded: &[u8]) -> Result<IdentityRecord, ModelError> {
    if take_u8(&mut encoded, "record")? != IDENTITY_RECORD_VERSION {
        return Err(invalid_identity_spill("record"));
    }
    let flags = take_u8(&mut encoded, "record")?;
    let invalid_flags = flags & !IDENTITY_RECORD_KNOWN_FLAGS != 0
        || flags & IDENTITY_RECORD_DECLARED_LINKS_ONE != 0
            && flags & IDENTITY_RECORD_HAS_DECLARED_LINKS == 0
        || flags & IDENTITY_RECORD_ALLOCATED_UPPER_EQUALS_LOWER != 0
            && flags & IDENTITY_RECORD_HAS_ALLOCATED_UPPER == 0
        || flags & IDENTITY_RECORD_IMPLICIT_SINGLE_PARTICIPANT != 0
            && flags & IDENTITY_RECORD_HAS_ALLOCATION_NODE == 0;
    if invalid_flags {
        return Err(invalid_identity_spill("record"));
    }
    let observed_links = if flags & IDENTITY_RECORD_IMPLICIT_SINGLE_OBSERVATION != 0 {
        1
    } else {
        take_u64(&mut encoded, "record")?
    };
    let declared_links = if flags & IDENTITY_RECORD_HAS_DECLARED_LINKS == 0 {
        None
    } else if flags & IDENTITY_RECORD_DECLARED_LINKS_ONE != 0 {
        Some(1)
    } else {
        Some(take_u64(&mut encoded, "record")?)
    };
    let lower = take_u128(&mut encoded, "record")?;
    let upper = if flags & IDENTITY_RECORD_HAS_ALLOCATED_UPPER == 0 {
        None
    } else if flags & IDENTITY_RECORD_ALLOCATED_UPPER_EQUALS_LOWER != 0 {
        Some(lower)
    } else {
        Some(take_u128(&mut encoded, "record")?)
    };
    let allocation_node = (flags & IDENTITY_RECORD_HAS_ALLOCATION_NODE != 0)
        .then(|| take_u32(&mut encoded, "record").map(NodeId))
        .transpose()?;
    let nodes = if flags & IDENTITY_RECORD_IMPLICIT_SINGLE_PARTICIPANT != 0 {
        vec![(
            allocation_node.ok_or_else(|| invalid_identity_spill("record"))?,
            1,
        )]
    } else {
        let node_count = usize::try_from(take_u32(&mut encoded, "record")?)
            .map_err(|_| invalid_identity_spill("record"))?;
        let node_bytes = node_count
            .checked_mul(IDENTITY_RECORD_NODE_BYTES)
            .ok_or_else(|| invalid_identity_spill("record"))?;
        if encoded.len() != node_bytes {
            return Err(invalid_identity_spill("record"));
        }
        let mut nodes = Vec::with_capacity(node_count);
        for _ in 0..node_count {
            nodes.push((
                NodeId(take_u32(&mut encoded, "record")?),
                take_u64(&mut encoded, "record")?,
            ));
        }
        nodes
    };
    if !encoded.is_empty() {
        return Err(invalid_identity_spill("record"));
    }
    Ok(IdentityRecord {
        observed_links,
        declared_links,
        allocated_bytes: ByteBounds { lower, upper },
        allocation_node,
        nodes,
    })
}

fn push_u32(encoded: &mut Vec<u8>, value: u32) {
    encoded.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(encoded: &mut Vec<u8>, value: u64) {
    encoded.extend_from_slice(&value.to_le_bytes());
}

fn push_u128(encoded: &mut Vec<u8>, value: u128) {
    encoded.extend_from_slice(&value.to_le_bytes());
}

fn take_u8(encoded: &mut &[u8], subject: &'static str) -> Result<u8, ModelError> {
    let (&value, remaining) = encoded
        .split_first()
        .ok_or_else(|| invalid_identity_spill(subject))?;
    *encoded = remaining;
    Ok(value)
}

fn take_u32(encoded: &mut &[u8], subject: &'static str) -> Result<u32, ModelError> {
    Ok(u32::from_le_bytes(take_array(encoded, subject)?))
}

fn take_u64(encoded: &mut &[u8], subject: &'static str) -> Result<u64, ModelError> {
    Ok(u64::from_le_bytes(take_array(encoded, subject)?))
}

fn take_u128(encoded: &mut &[u8], subject: &'static str) -> Result<u128, ModelError> {
    Ok(u128::from_le_bytes(take_array(encoded, subject)?))
}

fn take_array<const N: usize>(
    encoded: &mut &[u8],
    subject: &'static str,
) -> Result<[u8; N], ModelError> {
    if encoded.len() < N {
        return Err(invalid_identity_spill(subject));
    }
    let mut value = [0_u8; N];
    value.copy_from_slice(&encoded[..N]);
    *encoded = &encoded[N..];
    Ok(value)
}

fn invalid_identity_spill(subject: &str) -> ModelError {
    ModelError::Identity(format!("invalid identity spill {subject}"))
}

fn cleanup_stale_sessions_once() {
    static STARTUP_SESSION_CLEANUP: OnceLock<()> = OnceLock::new();
    STARTUP_SESSION_CLEANUP.get_or_init(|| {
        if let Err(_error) = cleanup_stale_sessions() {
            // Startup cleanup is best effort. A parent-enumeration failure
            // must not prevent creating a new private spill session.
        }
    });
}

fn cleanup_stale_sessions() -> Result<usize, ModelError> {
    cleanup_stale_sessions_in(&std::env::temp_dir(), SystemTime::now())
}

fn cleanup_stale_sessions_in(parent: &Path, now: SystemTime) -> Result<usize, ModelError> {
    let entries = fs::read_dir(parent).map_err(identity_error)?;
    let mut visited = 0_usize;
    let mut removed = 0_usize;
    for entry in entries {
        if visited >= MAX_CLEANUP_CANDIDATES {
            break;
        }
        visited = visited.saturating_add(1);
        let Ok(entry) = entry else {
            continue;
        };
        if !is_session_directory_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        match reclaim_stale_session(&path, now) {
            Ok(true) => removed = removed.saturating_add(1),
            Ok(false) => {}
            Err(_error) => {
                // Failed ownership/type/liveness checks are conservative:
                // leave this candidate untouched and continue the bounded scan.
            }
        }
    }
    Ok(removed)
}

fn is_session_directory_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(SESSION_PREFIX))
}

#[cfg(not(windows))]
fn reclaim_stale_session(path: &Path, now: SystemTime) -> Result<bool, ModelError> {
    // This private-path check is the ownership proof on platforms without a
    // held cleanup handle. Failure means this candidate is left untouched.
    verify_private_directory(path)?;
    if !is_stale_session(path, now)? {
        return Ok(false);
    }
    remove_verified_session(path)?;
    Ok(true)
}

#[cfg(windows)]
fn reclaim_stale_session(path: &Path, now: SystemTime) -> Result<bool, ModelError> {
    // The held handle proves ownership and prevents namespace replacement
    // through the classification and deletion sequence.
    let mut handle = crate::os::windows::open_verified_private_directory_for_cleanup(path)
        .map_err(identity_error)?;
    if !is_stale_session(path, now)? {
        return Ok(false);
    }
    remove_verified_session_held(path, &mut handle)?;
    Ok(true)
}

fn is_stale_session(path: &Path, now: SystemTime) -> Result<bool, ModelError> {
    session_contents_are_safe(path)?;
    let marker = read_session_marker(path)?;
    let now_seconds = seconds_since_epoch(now)?;
    if now_seconds.saturating_sub(marker.started_at) < STALE_SESSION_AGE.as_secs() {
        return Ok(false);
    }
    let metadata = fs::symlink_metadata(path).map_err(identity_error)?;
    let modified = metadata.modified().map_err(identity_error)?;
    if now
        .duration_since(modified)
        .map_or(true, |age| age < STALE_SESSION_AGE)
    {
        return Ok(false);
    }
    Ok(!session_process_is_active(marker.pid))
}

fn session_contents_are_safe(directory: &Path) -> Result<(), ModelError> {
    let mut marker_present = false;
    let mut database_present = false;
    let mut entries = 0_usize;
    for entry in fs::read_dir(directory).map_err(identity_error)? {
        let entry = entry.map_err(identity_error)?;
        entries = entries.saturating_add(1);
        if entries > MAX_SESSION_ENTRIES {
            return Err(ModelError::Identity(
                "spill session contains too many entries for safe cleanup".to_string(),
            ));
        }
        let path = entry.path();
        if entry.file_name() == SESSION_MARKER_FILE {
            if marker_present {
                return Err(ModelError::Identity(
                    "spill session contains duplicate markers".to_string(),
                ));
            }
            verify_private_file(&path)?;
            marker_present = true;
        } else if entry.file_name() == IDENTITY_DATABASE_FILE {
            if database_present {
                return Err(ModelError::Identity(
                    "spill session contains duplicate databases".to_string(),
                ));
            }
            verify_private_file(&path)?;
            database_present = true;
        } else {
            return Err(ModelError::Identity(
                "spill session contains an unexpected entry".to_string(),
            ));
        }
    }
    if !marker_present {
        return Err(ModelError::Identity(
            "spill session has no ownership marker".to_string(),
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn remove_verified_session(directory: &Path) -> Result<(), ModelError> {
    verify_private_directory(directory)?;
    session_contents_are_safe(directory)?;
    let database = directory.join(IDENTITY_DATABASE_FILE);
    if let Err(error) = fs::remove_file(&database)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(identity_error(error));
    }
    let marker = directory.join(SESSION_MARKER_FILE);
    fs::remove_file(&marker).map_err(identity_error)?;
    fs::remove_dir(directory).map_err(identity_error)
}

#[cfg(windows)]
fn remove_verified_session(directory: &Path) -> Result<(), ModelError> {
    let mut handle = crate::os::windows::open_verified_private_directory_for_cleanup(directory)
        .map_err(identity_error)?;
    remove_verified_session_held(directory, &mut handle)
}

#[cfg(windows)]
fn remove_verified_session_held(
    directory: &Path,
    handle: &mut crate::os::windows::PrivateDirectoryHandle,
) -> Result<(), ModelError> {
    session_contents_are_safe(directory)?;
    let database = directory.join(IDENTITY_DATABASE_FILE);
    match fs::symlink_metadata(&database) {
        Ok(_) => {
            crate::os::windows::delete_verified_private_file(&database).map_err(identity_error)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(identity_error(error)),
    }
    crate::os::windows::delete_verified_private_file(&directory.join(SESSION_MARKER_FILE))
        .map_err(identity_error)?;
    handle.delete_on_close().map_err(identity_error)?;
    handle.close();
    Ok(())
}

impl SessionMarker {
    fn new() -> Result<Self, ModelError> {
        Ok(Self {
            pid: std::process::id(),
            started_at: seconds_since_epoch(SystemTime::now())?,
            nonce: random_session_token()?,
        })
    }

    fn serialize(&self) -> String {
        format!(
            "{SESSION_MARKER_HEADER}\npid={}\nstarted={}\nnonce={}\n",
            self.pid, self.started_at, self.nonce
        )
    }

    fn parse(contents: &str) -> Result<Self, ModelError> {
        let mut lines = contents.lines();
        if lines.next() != Some(SESSION_MARKER_HEADER) {
            return Err(ModelError::Identity(
                "invalid spill session marker".to_string(),
            ));
        }
        let pid = lines
            .next()
            .and_then(|line| line.strip_prefix("pid="))
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| ModelError::Identity("invalid spill session PID".to_string()))?;
        let started_at = lines
            .next()
            .and_then(|line| line.strip_prefix("started="))
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| ModelError::Identity("invalid spill session timestamp".to_string()))?;
        let nonce = lines
            .next()
            .and_then(|line| line.strip_prefix("nonce="))
            .filter(|nonce| is_session_nonce(nonce))
            .map(ToOwned::to_owned)
            .ok_or_else(|| ModelError::Identity("invalid spill session nonce".to_string()))?;
        if lines.next().is_some() {
            return Err(ModelError::Identity(
                "spill session marker has unexpected data".to_string(),
            ));
        }
        Ok(Self {
            pid,
            started_at,
            nonce,
        })
    }
}

fn write_session_marker(directory: &Path, contents: &str) -> Result<(), ModelError> {
    let path = directory.join(SESSION_MARKER_FILE);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(identity_error)?;
    file.write_all(contents.as_bytes())
        .map_err(identity_error)?;
    file.sync_data().map_err(identity_error)?;
    restrict_private_file(&path)?;
    verify_private_file(&path)
}

fn read_session_marker(directory: &Path) -> Result<SessionMarker, ModelError> {
    let path = directory.join(SESSION_MARKER_FILE);
    let metadata = fs::symlink_metadata(&path).map_err(identity_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_MARKER_BYTES
    {
        return Err(ModelError::Identity(
            "spill session marker is not a bounded regular file".to_string(),
        ));
    }
    verify_private_file(&path)?;
    let mut file = fs::File::open(&path).map_err(identity_error)?;
    let mut contents = String::new();
    std::io::Read::by_ref(&mut file)
        .take(MAX_MARKER_BYTES.saturating_add(1))
        .read_to_string(&mut contents)
        .map_err(identity_error)?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_MARKER_BYTES {
        return Err(ModelError::Identity(
            "spill session marker exceeds its bound".to_string(),
        ));
    }
    SessionMarker::parse(&contents)
}

fn verify_active_session(directory: &Path, expected: &SessionMarker) -> Result<(), ModelError> {
    verify_private_directory(directory)?;
    if &read_session_marker(directory)? != expected {
        return Err(ModelError::Identity(
            "spill session marker no longer matches the active session".to_string(),
        ));
    }
    Ok(())
}

fn random_session_token() -> Result<String, ModelError> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(identity_error)?;
    Ok(random
        .iter()
        .fold(String::with_capacity(32), |mut token, byte| {
            use std::fmt::Write as _;

            let _ = write!(token, "{byte:02x}");
            token
        }))
}

fn is_session_nonce(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn seconds_since_epoch(time: SystemTime) -> Result<u64, ModelError> {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(identity_error)
}

#[cfg(unix)]
fn session_process_is_active(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    match kill(Pid::from_raw(pid), None) {
        Err(Errno::ESRCH) => false,
        Ok(()) | Err(_) => true,
    }
}

#[cfg(windows)]
fn session_process_is_active(pid: u32) -> bool {
    crate::os::windows::is_process_active(pid)
}

#[cfg(not(any(unix, windows)))]
const fn session_process_is_active(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
fn restrict_private_directory(path: &Path) -> Result<(), ModelError> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(identity_error)
}

#[cfg(unix)]
fn restrict_private_file(path: &Path) -> Result<(), ModelError> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(identity_error)
}

#[cfg(unix)]
fn verify_private_directory(path: &Path) -> Result<(), ModelError> {
    verify_unix_private_path(path, true, 0o700)
}

#[cfg(unix)]
fn verify_private_file(path: &Path) -> Result<(), ModelError> {
    verify_unix_private_path(path, false, 0o600)
}

#[cfg(unix)]
fn verify_unix_private_path(
    path: &Path,
    directory: bool,
    expected_mode: u32,
) -> Result<(), ModelError> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = fs::symlink_metadata(path).map_err(identity_error)?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o777 != expected_mode
    {
        return Err(ModelError::Identity(
            "spill path is not private to the current user".to_string(),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn restrict_private_file(path: &Path) -> Result<(), ModelError> {
    crate::os::windows::restrict_private_path(path, false).map_err(identity_error)
}

#[cfg(windows)]
fn verify_private_directory(path: &Path) -> Result<(), ModelError> {
    crate::os::windows::verify_private_path(path, true).map_err(identity_error)
}

#[cfg(windows)]
fn verify_private_file(path: &Path) -> Result<(), ModelError> {
    crate::os::windows::verify_private_path(path, false).map_err(identity_error)
}

#[cfg(not(any(unix, windows)))]
fn restrict_private_directory(_path: &Path) -> Result<(), ModelError> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn restrict_private_file(_path: &Path) -> Result<(), ModelError> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn verify_private_directory(path: &Path) -> Result<(), ModelError> {
    let metadata = fs::symlink_metadata(path).map_err(identity_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ModelError::Identity(
            "spill directory is not a directory".to_string(),
        ));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn verify_private_file(path: &Path) -> Result<(), ModelError> {
    let metadata = fs::symlink_metadata(path).map_err(identity_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ModelError::Identity(
            "spill file is not a regular file".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use file_id::FileId;

    use super::*;

    #[test]
    fn compact_identity_spill_codecs_round_trip_common_and_irregular_records() {
        let common = IdentityRecord {
            observed_links: 1,
            declared_links: Some(1),
            allocated_bytes: ByteBounds::exact(4096),
            allocation_node: Some(NodeId(7)),
            nodes: vec![(NodeId(7), 1)],
        };
        let common_encoded = encode_identity_record(&common).expect("common record should encode");
        assert_eq!(
            common_encoded.len(),
            22,
            "the ordinary one-link identity must retain its compact fixed representation"
        );
        assert_eq!(
            decode_identity_record(&common_encoded).expect("common record should decode"),
            common
        );

        let irregular = IdentityRecord {
            observed_links: 3,
            declared_links: Some(4),
            allocated_bytes: ByteBounds::unknown(),
            allocation_node: None,
            nodes: vec![(NodeId(2), 2), (NodeId(9), 1)],
        };
        let irregular_encoded =
            encode_identity_record(&irregular).expect("irregular record should encode");
        assert_eq!(
            decode_identity_record(&irregular_encoded).expect("irregular record should decode"),
            irregular
        );

        for file_id in [
            FileId::new_inode(1, 2),
            FileId::new_low_res(3, 4),
            FileId::new_high_res(5, 6),
        ] {
            assert_eq!(
                decode_file_id(&encode_file_id(&file_id)).expect("identity key should decode"),
                file_id
            );
        }
        assert!(decode_file_id(&[]).is_err());
        assert!(decode_identity_record(&[IDENTITY_RECORD_VERSION, 0xff]).is_err());
    }

    #[test]
    fn spill_is_permission_restricted_and_removed_on_drop() {
        const TEMPORARY_STORAGE_LIMIT: u64 = 2 * 1024 * 1024;
        let temporary_storage = TemporaryStorage::with_limit_bytes(TEMPORARY_STORAGE_LIMIT);
        let spill_path = {
            let mut store = IdentityStore::new_with_temporary_storage(1, &temporary_storage)
                .expect("private session should initialize");
            store
                .observe(
                    &FileId::new_inode(1, 1),
                    Some(1),
                    ByteBounds::exact(4096),
                    None,
                    None,
                )
                .expect("identity should spill");
            assert!(store.is_spilled());
            assert!(temporary_storage.used() > MAX_MARKER_BYTES);
            assert!(temporary_storage.used() <= TEMPORARY_STORAGE_LIMIT);
            let path = store
                .spill_path()
                .expect("spill path should exist")
                .to_path_buf();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(&path)
                    .expect("spill metadata should exist")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o700);
                for name in [SESSION_MARKER_FILE, IDENTITY_DATABASE_FILE] {
                    let mode = std::fs::metadata(path.join(name))
                        .expect("spill file should exist")
                        .permissions()
                        .mode()
                        & 0o777;
                    assert_eq!(mode, 0o600);
                }
            }
            path
        };
        assert!(!spill_path.exists());
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn identity_spill_capacity_switches_to_untracked_and_releases_storage() {
        let temporary_storage = TemporaryStorage::with_limit_bytes(MAX_MARKER_BYTES);
        let spill_path = {
            let mut store = IdentityStore::new_with_temporary_storage(1, &temporary_storage)
                .expect("private session marker should fit the test limit");
            let path = store
                .internal_scan_paths()
                .pop()
                .expect("active private session should be tracked");

            let (is_new, record) = store
                .observe(
                    &FileId::new_inode(13, 1),
                    Some(1),
                    ByteBounds::exact(4096),
                    None,
                    None,
                )
                .expect("capacity exhaustion should preserve the scan as uncertain");
            assert!(!is_new);
            assert_eq!(record.allocated_bytes, ByteBounds::unknown());
            assert!(store.capacity_exhausted());
            assert_eq!(store.len(), 0);
            assert!(!store.is_spilled());
            assert!(
                store
                    .get(&FileId::new_inode(13, 1))
                    .expect("untracked identity lookup should not fail")
                    .is_none()
            );
            store
                .observe(
                    &FileId::new_inode(13, 2),
                    Some(1),
                    ByteBounds::exact(4096),
                    None,
                    None,
                )
                .expect("subsequent identities should remain untracked without another error");
            assert!(!path.join(IDENTITY_DATABASE_FILE).exists());
            assert!(temporary_storage.used() <= MAX_MARKER_BYTES);
            path
        };
        assert_eq!(temporary_storage.used(), 0);
        assert!(!spill_path.exists());
    }

    #[test]
    fn exhausted_shared_storage_starts_identity_store_untracked() {
        let temporary_storage = TemporaryStorage::with_limit_bytes(MAX_MARKER_BYTES);
        let reservation = temporary_storage
            .reservation(MAX_MARKER_BYTES)
            .expect("test reservation should fill the shared storage budget");
        let mut store = IdentityStore::new_with_temporary_storage(1, &temporary_storage)
            .expect("an exhausted private spill budget should leave identity accounting untracked");

        assert!(store.capacity_exhausted());
        assert!(store.internal_scan_paths().is_empty());
        assert_eq!(store.len(), 0);
        let (is_new, record) = store
            .observe(
                &FileId::new_inode(13, 4),
                Some(1),
                ByteBounds::exact(4096),
                None,
                None,
            )
            .expect("untracked identity observations should not abort the scan");
        assert!(!is_new);
        assert_eq!(record.allocated_bytes, ByteBounds::unknown());

        drop(store);
        assert_eq!(temporary_storage.used(), MAX_MARKER_BYTES);
        drop(reservation);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn unsignalled_temporary_storage_exhaustion_becomes_untracked() {
        let temporary_storage = TemporaryStorage::with_limit_bytes(1_024);
        let mut store = IdentityStore::new_with_temporary_storage(1, &temporary_storage)
            .expect("private session should initialize");
        let error = ModelError::Identity(
            "I/O error: temporary storage capacity exhausted: 550985720 bytes exceed the 536870912 byte session limit; increase --temporary-storage-mib".to_string(),
        );

        assert!(
            store
                .recover_capacity::<()>(Err(error))
                .expect("a private spill capacity failure must not abort the scan")
                .is_none()
        );
        assert!(store.capacity_exhausted());
        assert!(
            store
                .observe(
                    &FileId::new_inode(13, 3),
                    Some(1),
                    ByteBounds::exact(4096),
                    None,
                    None,
                )
                .expect("later identity observations should remain untracked")
                .1
                .allocated_bytes
                .upper
                .is_none()
        );
        drop(store);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn ordinary_identity_errors_are_not_silenced_as_capacity_failures() {
        let temporary_storage = TemporaryStorage::with_limit_bytes(1_024);
        let mut store = IdentityStore::new_with_temporary_storage(1, &temporary_storage)
            .expect("private session should initialize");

        let error = store
            .recover_capacity::<()>(Err(ModelError::Identity(
                "I/O error: permission denied".to_string(),
            )))
            .expect_err("non-capacity identity I/O errors must remain visible");
        assert_eq!(
            error.to_string(),
            "identity accounting failed: I/O error: permission denied"
        );
        assert!(!store.capacity_exhausted());
    }

    #[test]
    fn signalled_spill_capacity_is_recovered_before_identity_reads() {
        const TEMPORARY_STORAGE_LIMIT: u64 = 2 * 1024 * 1024;

        let temporary_storage = TemporaryStorage::with_limit_bytes(TEMPORARY_STORAGE_LIMIT);
        let spill_path = {
            let mut store = IdentityStore::new_with_temporary_storage(1, &temporary_storage)
                .expect("private session should initialize");
            let file_id = FileId::new_inode(13, 1);
            store
                .observe(&file_id, Some(1), ByteBounds::exact(4096), None, None)
                .expect("identity should spill");
            assert!(store.is_spilled());
            let path = store
                .spill_path()
                .expect("spilled identity should expose its private session")
                .to_path_buf();

            store.signal_database_capacity_exhaustion_for_test();
            let mut visited = false;
            store
                .visit_records(|_, _| {
                    visited = true;
                    Ok(())
                })
                .expect("a signalled capacity limit must not escape through iteration");
            assert!(!visited, "an exhausted store must not visit stale records");

            assert!(
                store
                    .get(&file_id)
                    .expect("a signalled capacity limit must not escape through lookup")
                    .is_none()
            );
            assert!(store.capacity_exhausted());
            assert!(!store.is_spilled());
            assert!(!path.join(IDENTITY_DATABASE_FILE).exists());
            assert!(temporary_storage.used() <= MAX_MARKER_BYTES);
            path
        };
        assert_eq!(temporary_storage.used(), 0);
        assert!(!spill_path.exists());
    }
    #[test]
    fn conflicting_declared_link_counts_are_unknown() {
        let file_id = FileId::new_inode(2, 2);
        let mut store = IdentityStore::new(usize::MAX).expect("private session should initialize");
        for declared_links in [Some(1), Some(2), Some(2)] {
            store
                .observe(
                    &file_id,
                    declared_links,
                    ByteBounds::exact(4096),
                    None,
                    None,
                )
                .expect("identity observation should succeed");
        }

        let record = store
            .get(&file_id)
            .expect("identity lookup should succeed")
            .expect("identity should remain");
        assert_eq!(record.declared_links, None);
    }
    #[test]
    fn spilled_store_retains_many_exact_records() {
        let spill_path = {
            let mut store = IdentityStore::new(1).expect("private session should initialize");
            for index in 0..1_000_u64 {
                store
                    .observe(
                        &FileId::new_inode(7, index),
                        Some(1),
                        ByteBounds::exact(u128::from(index)),
                        Some(NodeId(u32::try_from(index).expect("test ID should fit"))),
                        Some(NodeId(u32::try_from(index).expect("test ID should fit"))),
                    )
                    .expect("spilled identity should be stored");
            }
            assert_eq!(store.len(), 1_000);
            assert_eq!(
                store
                    .get(&FileId::new_inode(7, 999))
                    .expect("identity lookup should succeed")
                    .expect("identity should exist")
                    .allocated_bytes,
                ByteBounds::exact(999)
            );
            store
                .spill_path()
                .expect("store should spill")
                .to_path_buf()
        };
        assert!(!spill_path.exists());
    }

    #[test]
    fn spill_migrates_existing_records_in_bounded_steps() {
        const EXISTING: usize = MIGRATION_RECORDS_PER_OBSERVATION * 2 + 1;

        let mut store = IdentityStore::new(usize::MAX).expect("private session should initialize");
        for index in 0..EXISTING {
            let index = u64::try_from(index).expect("test ID should fit");
            store
                .observe(
                    &FileId::new_inode(14, index),
                    Some(1),
                    ByteBounds::exact(u128::from(index)),
                    Some(NodeId(u32::try_from(index).expect("test ID should fit"))),
                    Some(NodeId(u32::try_from(index).expect("test ID should fit"))),
                )
                .expect("in-memory identity should be stored");
        }
        store.memory_limit = 1;
        let trigger = FileId::new_inode(14, u64::try_from(EXISTING).expect("test ID should fit"));
        store
            .observe(
                &trigger,
                Some(1),
                ByteBounds::exact(4096),
                Some(NodeId(u32::try_from(EXISTING).expect("test ID should fit"))),
                Some(NodeId(u32::try_from(EXISTING).expect("test ID should fit"))),
            )
            .expect("spill trigger should be stored");

        let Storage::Migrating { records, .. } = &store.storage else {
            panic!("existing identities should drain incrementally");
        };
        assert_eq!(
            records.len(),
            EXISTING.saturating_sub(MIGRATION_RECORDS_PER_OBSERVATION),
            "one observation may only migrate its bounded slice"
        );
        assert_eq!(store.len(), EXISTING.saturating_add(1));
        assert_eq!(
            store
                .get(&FileId::new_inode(14, 0))
                .expect("migrating identity lookup should succeed")
                .expect("migrating identity should remain")
                .allocated_bytes,
            ByteBounds::exact(0)
        );
        let mut visited = 0;
        store
            .visit_records(|_, _| {
                visited += 1;
                Ok(())
            })
            .expect("migration should retain every identity for traversal");
        assert_eq!(visited, EXISTING.saturating_add(1));

        while store
            .advance_migration(MIGRATION_RECORDS_PER_OBSERVATION)
            .expect("bounded migration step should succeed")
        {}
        assert!(matches!(store.storage, Storage::Disk { .. }));
        assert_eq!(store.len(), EXISTING.saturating_add(1));
        assert_eq!(
            store
                .get(&trigger)
                .expect("migrated trigger lookup should succeed")
                .expect("migrated trigger should remain")
                .allocated_bytes,
            ByteBounds::exact(4096)
        );
    }
    #[test]
    fn remapping_spilled_participants_preserves_allocation_owner() {
        let file_id = FileId::new_inode(9, 9);
        let mut store = IdentityStore::new(1).expect("private session should initialize");
        store
            .observe(
                &file_id,
                Some(2),
                ByteBounds::exact(4096),
                Some(NodeId(4)),
                Some(NodeId(4)),
            )
            .expect("identity should spill");
        store
            .observe(
                &file_id,
                Some(2),
                ByteBounds::exact(4096),
                Some(NodeId(5)),
                Some(NodeId(4)),
            )
            .expect("second participant should be stored");

        let mut removed = [NodeId(4)];
        store
            .remap_removed_nodes(&mut removed, NodeId(12))
            .expect("spilled records should be remapped");

        let record = store
            .get(&file_id)
            .expect("identity lookup should succeed")
            .expect("identity should remain");
        assert_eq!(record.nodes, vec![(NodeId(5), 1), (NodeId(12), 1)]);
        assert_eq!(record.allocation_node, Some(NodeId(12)));
    }

    #[test]
    fn remapping_spilled_participants_preserves_page_boundary_records() {
        const RECORDS: u32 = 600;
        let mut store = IdentityStore::new(1).expect("private session should initialize");
        for index in 0..RECORDS {
            let node = NodeId(index);
            store
                .observe(
                    &FileId::new_inode(10, u64::from(index)),
                    Some(1),
                    ByteBounds::exact(4096),
                    Some(node),
                    Some(node),
                )
                .expect("spilled identity should be stored");
        }

        let mut removed = (0..RECORDS).map(NodeId).collect::<Vec<_>>();
        let replacement = NodeId(RECORDS + 1);
        store
            .remap_removed_nodes(&mut removed, replacement)
            .expect("all spilled records should be remapped");

        for index in 0..RECORDS {
            let record = store
                .get(&FileId::new_inode(10, u64::from(index)))
                .expect("identity lookup should succeed")
                .expect("identity should remain");
            assert_eq!(record.nodes, vec![(replacement, 1)]);
            assert_eq!(record.allocation_node, Some(replacement));
        }
    }

    #[test]
    fn spilled_repeated_participants_are_coalesced_after_remap() {
        const OBSERVATIONS: u64 = 1_024;
        let file_id = FileId::new_inode(11, 11);
        let mut store = IdentityStore::new(1).expect("private session should initialize");
        for _ in 0..OBSERVATIONS {
            store
                .observe(
                    &file_id,
                    Some(OBSERVATIONS),
                    ByteBounds::exact(4096),
                    Some(NodeId(4)),
                    Some(NodeId(4)),
                )
                .expect("repeated spilled participant should be stored");
        }
        assert!(store.is_spilled());

        store
            .remap_nodes_for_identities(std::slice::from_ref(&file_id), &[NodeId(4)], NodeId(9))
            .expect("spilled participant should be remapped");

        let Storage::Disk { pending, .. } = &store.storage else {
            panic!("identity store should spill");
        };
        let pending_record = pending
            .values()
            .next()
            .expect("remapped record should be pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending_record.nodes.len(), 1);
        assert_eq!(pending_record.nodes, vec![(NodeId(9), OBSERVATIONS)]);

        let record = store
            .get(&file_id)
            .expect("identity lookup should succeed")
            .expect("identity should remain");
        assert_eq!(record.observed_links, OBSERVATIONS);
        assert_eq!(record.nodes.len(), 1);
        assert_eq!(record.nodes, vec![(NodeId(9), OBSERVATIONS)]);
        assert_eq!(record.allocation_node, Some(NodeId(9)));
    }

    #[test]
    fn internal_scan_path_exists_before_spill_and_requires_its_marker() {
        let store = IdentityStore::new(usize::MAX).expect("private session should initialize");
        let paths = store.internal_scan_paths();
        assert_eq!(paths.len(), 1);
        let marker = paths[0].join(SESSION_MARKER_FILE);
        std::fs::write(&marker, b"tampered").expect("marker should be writable by its owner");
        assert!(store.internal_scan_paths().is_empty());
    }

    #[test]
    fn keyed_remap_stages_one_spilled_identity() {
        let file_id = FileId::new_inode(12, 34);
        let mut store = IdentityStore::new(1).expect("private session should initialize");
        store
            .observe(
                &file_id,
                Some(1),
                ByteBounds::exact(4096),
                Some(NodeId(4)),
                Some(NodeId(4)),
            )
            .expect("identity should spill");
        for index in 0..DISK_WRITE_BATCH.saturating_sub(1) {
            store
                .observe(
                    &FileId::new_inode(13, u64::try_from(index).expect("test ID should fit")),
                    Some(1),
                    ByteBounds::exact(4096),
                    Some(NodeId(u32::try_from(index).expect("test ID should fit"))),
                    Some(NodeId(u32::try_from(index).expect("test ID should fit"))),
                )
                .expect("filler identity should be stored");
        }

        store
            .remap_nodes_for_identities(std::slice::from_ref(&file_id), &[NodeId(4)], NodeId(9))
            .expect("keyed remap should succeed");
        let Storage::Disk { pending, .. } = &store.storage else {
            panic!("identity should spill");
        };
        assert_eq!(
            pending.len(),
            1,
            "one keyed remap should join the regular bounded write batch"
        );

        let record = store
            .get(&file_id)
            .expect("identity lookup should succeed")
            .expect("identity should remain");
        assert_eq!(record.nodes, vec![(NodeId(9), 1)]);
        assert_eq!(record.allocation_node, Some(NodeId(9)));
    }

    #[cfg(unix)]
    fn stale_session(
        parent: &std::path::Path,
        name: &str,
        marker: &SessionMarker,
        with_database: bool,
    ) -> std::path::PathBuf {
        let path = parent.join(name);
        std::fs::create_dir(&path).expect("stale session directory should be created");
        restrict_private_directory(&path).expect("stale session directory should be private");
        write_session_marker(&path, &marker.serialize())
            .expect("stale session marker should be written");
        if with_database {
            let database = path.join(IDENTITY_DATABASE_FILE);
            std::fs::write(&database, b"interrupted redb state")
                .expect("stale database should be written");
            restrict_private_file(&database).expect("stale database should be private");
        }
        path
    }

    #[cfg(unix)]
    fn expired_time() -> (std::time::SystemTime, std::time::SystemTime) {
        let created = std::time::SystemTime::now();
        let cleanup = created
            .checked_add(STALE_SESSION_AGE + std::time::Duration::from_secs(1))
            .expect("test clock should advance");
        (created, cleanup)
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_rejects_unverifiable_session_directory() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().expect("cleanup parent should exist");
        let target = parent.path().join("outside-target");
        std::fs::create_dir(&target).expect("link target should be created");
        let candidate = parent.path().join(".excise-session-link");
        symlink(&target, &candidate).expect("session lookalike link should be created");

        let (_, cleanup) = expired_time();
        assert!(reclaim_stale_session(&candidate, cleanup).is_err());
        assert!(
            std::fs::symlink_metadata(&candidate)
                .expect("candidate should remain")
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_cleanup_reclaims_old_verified_interrupted_sessions() {
        let parent = tempfile::tempdir().expect("cleanup parent should exist");
        let (created, cleanup) = expired_time();
        let marker = SessionMarker {
            pid: 999_999_999,
            started_at: seconds_since_epoch(created).expect("test timestamp should be valid"),
            nonce: "0123456789abcdef0123456789abcdef".to_string(),
        };
        let session = stale_session(parent.path(), ".excise-session-interrupted", &marker, true);

        assert_eq!(
            cleanup_stale_sessions_in(parent.path(), cleanup)
                .expect("cleanup should inspect the private parent"),
            1
        );
        assert!(!session.exists());
    }

    #[cfg(unix)]
    #[test]
    fn startup_cleanup_preserves_lookalikes_links_and_live_sessions() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().expect("cleanup parent should exist");
        let (created, cleanup) = expired_time();
        let lookalike = parent.path().join(".excise-session-user-data");
        std::fs::create_dir(&lookalike).expect("lookalike directory should be created");
        restrict_private_directory(&lookalike).expect("lookalike directory should be private");
        let important = lookalike.join("important");
        std::fs::write(&important, b"keep").expect("lookalike content should be written");
        restrict_private_file(&important).expect("lookalike content should be private");

        let target = parent.path().join("outside-target");
        std::fs::create_dir(&target).expect("link target should be created");
        let protected = target.join("protected");
        std::fs::write(&protected, b"keep").expect("protected content should be written");
        let link = parent.path().join(".excise-session-link");
        symlink(&target, &link).expect("lookalike link should be created");

        let live_marker = SessionMarker {
            pid: std::process::id(),
            started_at: seconds_since_epoch(created).expect("test timestamp should be valid"),
            nonce: "fedcba9876543210fedcba9876543210".to_string(),
        };
        let live = stale_session(parent.path(), ".excise-session-live", &live_marker, true);

        assert_eq!(
            cleanup_stale_sessions_in(parent.path(), cleanup)
                .expect("cleanup should inspect the private parent"),
            0
        );
        assert!(lookalike.exists());
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("link should remain")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(&protected).expect("link target should remain"),
            b"keep"
        );
        assert!(live.exists());
    }
}
