use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::mem::size_of;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use file_id::FileId;
use redb::{Builder as RedbBuilder, Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
#[cfg(not(windows))]
use tempfile::{Builder as TempBuilder, TempDir};

use super::{ByteBounds, ModelError, NodeId};

const IDENTITIES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("identities");
const IDENTITY_ENTRY_OVERHEAD: usize = size_of::<IdentityRecord>() + 96;
pub const SESSION_PREFIX: &str = ".excise-session-";
const SESSION_MARKER_FILE: &str = ".excise-session";
const IDENTITY_DATABASE_FILE: &str = "identities.redb";
const SESSION_MARKER_HEADER: &str = "excise-spill-session-v1";
const MAX_MARKER_BYTES: u64 = 256;
const STALE_SESSION_AGE: Duration = Duration::from_secs(15 * 60);
const MAX_CLEANUP_CANDIDATES: usize = 64;
const MAX_SESSION_ENTRIES: usize = 2;
#[cfg(windows)]
const SESSION_CREATE_ATTEMPTS: usize = 32;
const DISK_WRITE_BATCH: usize = 256;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IdentityRecord {
    pub observed_links: u64,
    pub declared_links: Option<u64>,
    pub allocated_bytes: ByteBounds,
    pub allocation_node: Option<NodeId>,
    pub nodes: Vec<NodeId>,
}

pub struct IdentityStore {
    storage: Storage,
    session: SessionDirectory,
    memory_limit: usize,
    estimated_bytes: usize,
}

enum Storage {
    Memory(HashMap<Vec<u8>, IdentityRecord>),
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
    fn new() -> Result<Self, ModelError> {
        #[cfg(not(windows))]
        {
            let temporary = TempBuilder::new()
                .prefix(SESSION_PREFIX)
                .rand_bytes(32)
                .tempdir()
                .map_err(identity_error)?;
            let path = temporary.path().to_path_buf();
            restrict_private_directory(&path)?;
            verify_private_directory(&path)?;
            let marker = SessionMarker::new()?;
            write_session_marker(&path, &marker)?;
            verify_active_session(&path, &marker)?;
            Ok(Self {
                path,
                marker,
                temporary,
            })
        }

        #[cfg(windows)]
        {
            let parent = std::env::temp_dir();
            for _ in 0..SESSION_CREATE_ATTEMPTS {
                let path = parent.join(format!("{SESSION_PREFIX}{}", random_session_token()?));
                let mut handle = match crate::os::windows::create_private_directory(&path) {
                    Ok(handle) => handle,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(identity_error(error)),
                };
                let marker = match SessionMarker::new().and_then(|marker| {
                    write_session_marker(&path, &marker)?;
                    verify_active_session(&path, &marker)?;
                    Ok(marker)
                }) {
                    Ok(marker) => marker,
                    Err(error) => {
                        let _ = remove_verified_session_held(&path, &mut handle);
                        return Err(error);
                    }
                };
                return Ok(Self {
                    path,
                    marker,
                    handle,
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
}

#[cfg(windows)]
impl Drop for SessionDirectory {
    fn drop(&mut self) {
        self.handle.close();
        let _ = remove_verified_session(&self.path);
    }
}

#[allow(
    clippy::missing_errors_doc,
    reason = "IdentityStore methods expose one ModelError boundary for serialization, private-session validation, and disk-backed persistence."
)]
impl IdentityStore {
    pub fn new(memory_limit: usize) -> Result<Self, ModelError> {
        cleanup_stale_sessions_once();
        Ok(Self {
            storage: Storage::Memory(HashMap::new()),
            session: SessionDirectory::new()?,
            memory_limit,
            estimated_bytes: 0,
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
        let key = serde_json::to_vec(file_id).map_err(identity_error)?;
        let existing = self.get_by_key(&key)?;
        let is_new = existing.is_none();
        let mut record = existing.unwrap_or(IdentityRecord {
            observed_links: 0,
            declared_links,
            allocated_bytes,
            allocation_node,
            nodes: Vec::new(),
        });
        record.observed_links = record.observed_links.saturating_add(1);
        record.declared_links = record.declared_links.or(declared_links);
        if let Some(node) = node {
            record.nodes.push(node);
        }

        if matches!(self.storage, Storage::Memory(_)) {
            let value = serde_json::to_vec(&record).map_err(identity_error)?;
            self.estimated_bytes = self
                .estimated_bytes
                .saturating_add(key.len())
                .saturating_add(value.len())
                .saturating_add(IDENTITY_ENTRY_OVERHEAD);
            if self.estimated_bytes > self.memory_limit {
                self.spill_to_disk()?;
            }
        }
        self.insert_by_key(&key, &record, is_new)?;
        Ok((is_new, record))
    }

    pub fn get(&self, file_id: &FileId) -> Result<Option<IdentityRecord>, ModelError> {
        let key = serde_json::to_vec(file_id).map_err(identity_error)?;
        self.get_by_key(&key)
    }

    #[must_use]
    pub fn is_spilled(&self) -> bool {
        matches!(self.storage, Storage::Disk { .. })
    }

    #[must_use]
    pub fn spill_path(&self) -> Option<&Path> {
        self.is_spilled().then(|| self.session.path())
    }

    #[must_use]
    pub fn internal_scan_paths(&self) -> Vec<PathBuf> {
        if self.session.is_verified() {
            vec![self.session.path().to_path_buf()]
        } else {
            Vec::new()
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match &self.storage {
            Storage::Memory(records) => records.len(),
            Storage::Disk { count, .. } => *count,
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

    pub(crate) fn visit_records(
        &mut self,
        mut visitor: impl FnMut(FileId, IdentityRecord) -> Result<(), ModelError>,
    ) -> Result<(), ModelError> {
        self.flush_pending()?;
        match &self.storage {
            Storage::Memory(records) => {
                for (key, record) in records {
                    visitor(
                        serde_json::from_slice(key).map_err(identity_error)?,
                        record.clone(),
                    )?;
                }
            }
            Storage::Disk { database, .. } => {
                let transaction = database.begin_read().map_err(identity_error)?;
                let table = transaction.open_table(IDENTITIES).map_err(identity_error)?;
                for entry in table.iter().map_err(identity_error)? {
                    let (key, value) = entry.map_err(identity_error)?;
                    visitor(
                        serde_json::from_slice(key.value()).map_err(identity_error)?,
                        serde_json::from_slice(value.value()).map_err(identity_error)?,
                    )?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn upsert_record(
        &mut self,
        file_id: &FileId,
        record: &IdentityRecord,
    ) -> Result<Option<IdentityRecord>, ModelError> {
        let key = serde_json::to_vec(file_id).map_err(identity_error)?;
        let existing = self.get_by_key(&key)?;
        if matches!(self.storage, Storage::Memory(_)) {
            let value = serde_json::to_vec(record).map_err(identity_error)?;
            let previous = existing.as_ref().map_or(0, |current| {
                key.len()
                    .saturating_add(serde_json::to_vec(current).map_or(0, |encoded| encoded.len()))
                    .saturating_add(IDENTITY_ENTRY_OVERHEAD)
            });
            self.estimated_bytes = self
                .estimated_bytes
                .saturating_sub(previous)
                .saturating_add(key.len())
                .saturating_add(value.len())
                .saturating_add(IDENTITY_ENTRY_OVERHEAD);
            if self.estimated_bytes > self.memory_limit {
                self.spill_to_disk()?;
            }
        }
        self.insert_by_key(&key, record, existing.is_none())?;
        Ok(existing)
    }

    /// Repoints only one identity's participants using a caller-sorted removal set.
    ///
    /// This remains bounded for spilled stores because it reads and writes a
    /// single key rather than iterating every persisted identity.
    pub(crate) fn remap_nodes_for_identity(
        &mut self,
        file_id: &FileId,
        removed: &[NodeId],
        replacement: NodeId,
    ) -> Result<(), ModelError> {
        if removed.is_empty() {
            return Ok(());
        }
        let key = serde_json::to_vec(file_id).map_err(identity_error)?;
        let Some(mut record) = self.get_by_key(&key)? else {
            return Ok(());
        };
        if remap_record_nodes(&mut record, removed, replacement) {
            self.insert_by_key(&key, &record, false)?;
        }
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
        if removed.is_empty() {
            return Ok(());
        }
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
            Storage::Disk { database, .. } => {
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
                            let mut record =
                                serde_json::from_slice(value.value()).map_err(identity_error)?;
                            if remap_record_nodes(&mut record, removed, replacement) {
                                updates.push((
                                    key.clone(),
                                    serde_json::to_vec(&record).map_err(identity_error)?,
                                ));
                            }
                            last_key = Some(key);
                        }
                        let has_more = match entries.next() {
                            Some(Ok(_)) => true,
                            Some(Err(error)) => return Err(identity_error(error)),
                            None => false,
                        };
                        (updates, last_key, has_more)
                    };
                    if !updates.is_empty() {
                        let transaction = database.begin_write().map_err(identity_error)?;
                        {
                            let mut table =
                                transaction.open_table(IDENTITIES).map_err(identity_error)?;
                            for (key, value) in updates {
                                table
                                    .insert(key.as_slice(), value.as_slice())
                                    .map_err(identity_error)?;
                            }
                        }
                        transaction.commit().map_err(identity_error)?;
                    }
                    if !has_more {
                        break;
                    }
                    let Some(last_key) = last_key else {
                        return Err(ModelError::Invariant(
                            "identity store iteration advanced without a key".to_string(),
                        ));
                    };
                    resume_after = Some(last_key);
                }
                Ok(())
            }
        }
    }

    fn spill_to_disk(&mut self) -> Result<(), ModelError> {
        let (database, count) = {
            let Storage::Memory(records) = &self.storage else {
                return Ok(());
            };
            let path = self.session.path().join(IDENTITY_DATABASE_FILE);
            {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(identity_error)?;
            }
            restrict_private_file(&path)?;
            verify_private_file(&path)?;

            let cache_size = (self.memory_limit / 4).clamp(64 * 1024, 16 * 1024 * 1024);
            let mut builder = RedbBuilder::new();
            builder.set_cache_size(cache_size);
            let database = builder.create(&path).map_err(identity_error)?;
            let transaction = database.begin_write().map_err(identity_error)?;
            {
                let mut table = transaction.open_table(IDENTITIES).map_err(identity_error)?;
                for (key, record) in records {
                    let value = serde_json::to_vec(record).map_err(identity_error)?;
                    table
                        .insert(key.as_slice(), value.as_slice())
                        .map_err(identity_error)?;
                }
            }
            transaction.commit().map_err(identity_error)?;
            (database, records.len())
        };
        self.storage = Storage::Disk {
            database,
            count,
            pending: HashMap::new(),
        };
        Ok(())
    }

    fn get_by_key(&self, key: &[u8]) -> Result<Option<IdentityRecord>, ModelError> {
        match &self.storage {
            Storage::Memory(records) => Ok(records.get(key).cloned()),
            Storage::Disk {
                database, pending, ..
            } => {
                if let Some(record) = pending.get(key) {
                    return Ok(Some(record.clone()));
                }
                let transaction = database.begin_read().map_err(identity_error)?;
                let table = transaction.open_table(IDENTITIES).map_err(identity_error)?;
                table
                    .get(key)
                    .map_err(identity_error)?
                    .map(|value| serde_json::from_slice(value.value()).map_err(identity_error))
                    .transpose()
            }
        }
    }

    fn insert_by_key(
        &mut self,
        key: &[u8],
        record: &IdentityRecord,
        is_new: bool,
    ) -> Result<(), ModelError> {
        let should_flush = match &mut self.storage {
            Storage::Memory(records) => {
                records.insert(key.to_vec(), record.clone());
                false
            }
            Storage::Disk { count, pending, .. } => {
                pending.insert(key.to_vec(), record.clone());
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
        let Storage::Disk {
            database, pending, ..
        } = &mut self.storage
        else {
            return Ok(());
        };
        if pending.is_empty() {
            return Ok(());
        }
        let transaction = database.begin_write().map_err(identity_error)?;
        {
            let mut table = transaction.open_table(IDENTITIES).map_err(identity_error)?;
            for (key, record) in pending.iter() {
                let value = serde_json::to_vec(record).map_err(identity_error)?;
                table
                    .insert(key.as_slice(), value.as_slice())
                    .map_err(identity_error)?;
            }
        }
        transaction.commit().map_err(identity_error)?;
        pending.clear();
        Ok(())
    }
}

fn remap_record_nodes(
    record: &mut IdentityRecord,
    removed: &[NodeId],
    replacement: NodeId,
) -> bool {
    let mut changed = false;
    for node in &mut record.nodes {
        if removed.binary_search(&*node).is_ok() {
            *node = replacement;
            changed = true;
        }
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

fn identity_error(error: impl std::fmt::Display) -> ModelError {
    ModelError::Identity(error.to_string())
}

fn cleanup_stale_sessions_once() {
    static STARTUP_SESSION_CLEANUP: OnceLock<()> = OnceLock::new();
    STARTUP_SESSION_CLEANUP.get_or_init(|| {
        let _ = cleanup_stale_sessions();
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
        if matches!(reclaim_stale_session(&path, now), Ok(true)) {
            removed = removed.saturating_add(1);
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
    if !is_verified_stale_session(path, now) {
        return Ok(false);
    }
    remove_verified_session(path)?;
    Ok(true)
}

#[cfg(windows)]
fn reclaim_stale_session(path: &Path, now: SystemTime) -> Result<bool, ModelError> {
    let Ok(mut handle) = crate::os::windows::open_verified_private_directory_for_cleanup(path)
    else {
        return Ok(false);
    };
    if !is_stale_session(path, now) {
        return Ok(false);
    }
    remove_verified_session_held(path, &mut handle)?;
    Ok(true)
}

#[cfg(not(windows))]
fn is_verified_stale_session(path: &Path, now: SystemTime) -> bool {
    verify_private_directory(path).is_ok() && is_stale_session(path, now)
}

fn is_stale_session(path: &Path, now: SystemTime) -> bool {
    if session_contents_are_safe(path).is_err() {
        return false;
    }
    let Ok(marker) = read_session_marker(path) else {
        return false;
    };
    let Ok(now_seconds) = seconds_since_epoch(now) else {
        return false;
    };
    if now_seconds.saturating_sub(marker.started_at) < STALE_SESSION_AGE.as_secs() {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    if now
        .duration_since(modified)
        .map_or(true, |age| age < STALE_SESSION_AGE)
    {
        return false;
    }
    !session_process_is_active(marker.pid)
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

fn write_session_marker(directory: &Path, marker: &SessionMarker) -> Result<(), ModelError> {
    let path = directory.join(SESSION_MARKER_FILE);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(identity_error)?;
    file.write_all(marker.serialize().as_bytes())
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
    fn spill_is_permission_restricted_and_removed_on_drop() {
        let spill_path = {
            let mut store = IdentityStore::new(1).expect("private session should initialize");
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
        assert_eq!(record.nodes, vec![NodeId(12), NodeId(5)]);
        assert_eq!(record.allocation_node, Some(NodeId(12)));
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
    fn keyed_remap_updates_one_spilled_identity() {
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

        store
            .remap_nodes_for_identity(&file_id, &[NodeId(4)], NodeId(9))
            .expect("keyed remap should succeed");

        let record = store
            .get(&file_id)
            .expect("identity lookup should succeed")
            .expect("identity should remain");
        assert_eq!(record.nodes, vec![NodeId(9)]);
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
        write_session_marker(&path, marker).expect("stale session marker should be written");
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
