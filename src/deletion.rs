use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::size_of;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::UNIX_EPOCH;

use cap_primitives::ambient_authority;
#[cfg(not(unix))]
use cap_primitives::fs::FollowSymlinks;
use cap_primitives::fs::{self as cap_fs};
use file_id::FileId;
#[cfg(target_vendor = "apple")]
use rustix::fs::lstat;
#[cfg(unix)]
use rustix::fs::{AtFlags, statat};
#[cfg(target_os = "linux")]
use rustix::fs::{Mode, OFlags, fstat, openat};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
#[cfg(not(unix))]
use sysinfo::{DiskRefreshKind, Disks};

#[cfg(unix)]
use crate::entry_metadata::EntryMetadata;
use crate::model::{EntrySnapshot, NodeId, NodeKind};
use crate::native_path::{
    EncodedNativePath, NativeIdentity, NativePath, identity_for, safe_display_os_str,
    safe_display_path_text, safe_display_text,
};
#[cfg(windows)]
use crate::os::windows::{physical_size_from_handle, remove_open_handle};
#[cfg(windows)]
use crate::private_files::PrivateFile;
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
use crate::private_files::PrivateFile as TransientName;
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
use crate::private_files::PrivateFiles;
use crate::state::FileToDelete;
use crate::temporary_storage::{TemporaryStorage, TemporaryStorageReservation};

pub const DEFAULT_PLAN_LIMIT_BYTES: usize = 64 * 1024 * 1024;
const SPILL_RECORD_LENGTH_BYTES: u64 = 8;
const SPILL_RECORD_MAC_BYTES: u64 = 32;
const SPILL_MAC_BYTES: usize = 32;
const HMAC_BLOCK_BYTES: usize = 64;
const MAX_PLAN_SPILL_RECORD_BYTES: usize = 1024 * 1024;
/// A complete directory report reserves one failure detail per entry before
/// consent. Keep each diagnostic concise so cache-sized directory deletions
/// remain practical within the shared temporary-storage budget.
const MAX_OUTCOME_DETAIL_BYTES: usize = 128;
const OUTCOME_DETAIL_TRUNCATION: &str = "…";
const MAX_JSON_ESCAPED_BYTES_PER_INPUT_BYTE: usize = 6;
const RESULT_SPILL_ENVELOPE_BYTES: usize = 64;
const MAX_RESULT_SPILL_RECORD_BYTES: usize = MAX_PLAN_SPILL_RECORD_BYTES
    + MAX_OUTCOME_DETAIL_BYTES * MAX_JSON_ESCAPED_BYTES_PER_INPUT_BYTE
    + RESULT_SPILL_ENVELOPE_BYTES;
const MAX_RESIDENT_DIRECTORY_TASKS: usize = 64;

#[cfg(windows)]
type PlanSpillFile = PrivateFile;
#[cfg(not(windows))]
type PlanSpillFile = File;

#[must_use]
pub const fn deletion_supported() -> bool {
    cfg!(any(target_os = "linux", target_vendor = "apple", windows))
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlannedKind {
    Directory,
    File,
    Link,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlannedSnapshot {
    pub identity: NativeIdentity,
    pub kind: PlannedKind,
    pub apparent_bytes: u128,
    pub allocated_bytes: Option<u128>,
    pub modified_nanos: Option<u128>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewedEntry {
    pub relative_path: PathBuf,
    pub snapshot: PlannedSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedEntry {
    pub relative_path: PathBuf,
    pub snapshot: PlannedSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfirmationChallenge {
    ConfirmFile,
    TypeName(String),
    TypePhrase(String),
    ReducedGuard,
}

impl ConfirmationChallenge {
    #[must_use]
    pub fn expected_input(&self) -> &str {
        match self {
            Self::ConfirmFile | Self::ReducedGuard => "y",
            Self::TypeName(name) | Self::TypePhrase(name) => name,
        }
    }
}

#[derive(Debug)]
enum PlanEntries {
    InMemory(Vec<PlannedEntry>),
    Spilled(RefCell<RecordSpill>),
}

#[derive(Debug)]
pub struct DeletionPlan {
    pub target: FileToDelete,
    pub root_relative_path: PathBuf,
    pub scan_root_identity: NativeIdentity,
    entries: PlanEntries,
    result_storage: PlannedResultStorage,
    root_snapshot: PlannedSnapshot,
    pub challenge: ConfirmationChallenge,
    pub apparent_bytes: u128,
    /// The session's registry of the files Excise keeps in the user's tree: the executor registers
    /// the transient names it makes there while an entry is isolated (`TransientNames`). Windows
    /// removes an entry through an open handle and makes no such name.
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    private_files: PrivateFiles,
    /// What the plan, and the report that follows it, hold in memory: the entries it keeps
    /// resident, and nothing once it has spilled, because every entry is then in a file. It never
    /// exceeds the budget the plan was built with, so the place the deletion history keeps for
    /// the report always fits it.
    pub estimated_bytes: usize,
}

impl DeletionPlan {
    #[must_use]
    pub fn planned_entries(&self) -> u64 {
        self.entries.len()
    }

    #[must_use]
    pub fn root_snapshot(&self) -> &PlannedSnapshot {
        &self.root_snapshot
    }
}

#[cfg(feature = "fuzzing")]
impl DeletionPlan {
    /// The entries the planner reviewed, for the fuzz target's model; `None` once the plan keeps
    /// them in a file.
    pub(crate) fn reviewed_entries_for_probe(&self) -> Option<Vec<PlannedEntry>> {
        match &self.entries {
            PlanEntries::InMemory(entries) => Some(entries.clone()),
            PlanEntries::Spilled(_) => None,
        }
    }
}

#[derive(Debug)]
struct RecordSpill {
    /// Where the spill is a named file in the user's tree (on Windows), the file is one with the
    /// registration that tells every reader of that tree it is Excise's own, and closing it
    /// releases the path in the same step ([`crate::private_files::PrivateFile`]).
    file: PlanSpillFile,
    length: u64,
    records: u64,
    reservation: TemporaryStorageReservation,
    maximum_payload: u64,
    authentication: SpillAuthenticationKey,
}

struct SpillAuthenticationKey([u8; SPILL_MAC_BYTES]);

impl fmt::Debug for SpillAuthenticationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SpillAuthenticationKey([redacted])")
    }
}

impl SpillAuthenticationKey {
    fn new() -> io::Result<Self> {
        let mut key = [0_u8; SPILL_MAC_BYTES];
        getrandom::fill(&mut key).map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self(key))
    }

    fn tag(&self, offset: u64, payload_len: [u8; 8], payload: &[u8]) -> [u8; SPILL_MAC_BYTES] {
        let mut key_block = [0_u8; HMAC_BLOCK_BYTES];
        key_block[..self.0.len()].copy_from_slice(&self.0);
        let mut inner_pad = key_block;
        let mut outer_pad = key_block;
        for byte in &mut inner_pad {
            *byte ^= 0x36;
        }
        for byte in &mut outer_pad {
            *byte ^= 0x5c;
        }

        let mut inner = Sha256::new();
        inner.update(inner_pad);
        inner.update(b"excise/deletion-spill-v1\0");
        inner.update(offset.to_le_bytes());
        inner.update(payload_len);
        inner.update(payload);
        let inner = inner.finalize();

        let mut outer = Sha256::new();
        outer.update(outer_pad);
        outer.update(inner);
        let digest = outer.finalize();
        let mut tag = [0_u8; SPILL_MAC_BYTES];
        tag.copy_from_slice(&digest);
        tag
    }

    fn matches(
        &self,
        offset: u64,
        payload_len: [u8; 8],
        payload: &[u8],
        actual: &[u8; SPILL_MAC_BYTES],
    ) -> bool {
        let expected = self.tag(offset, payload_len, payload);
        expected
            .iter()
            .zip(actual)
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }
}

#[derive(Deserialize, Serialize)]
struct SpilledPlanEntry {
    relative_path: EncodedNativePath,
    snapshot: PlannedSnapshot,
}

enum SpillVisitError<E> {
    Io(io::Error),
    Visitor(E),
}

#[derive(Debug)]
struct PendingDirectories {
    resident: Vec<PlannedEntry>,
    spill: Option<RecordSpill>,
}

impl PlanEntries {
    const fn is_spilled(&self) -> bool {
        matches!(self, Self::Spilled(_))
    }

    #[must_use]
    fn len(&self) -> u64 {
        match self {
            Self::InMemory(entries) => u64::try_from(entries.len()).unwrap_or(u64::MAX),
            Self::Spilled(spill) => spill.borrow().records,
        }
    }

    fn push(
        &mut self,
        entry: PlannedEntry,
        spill: bool,
        temporary_storage: &TemporaryStorage,
        spill_directory: &Path,
    ) -> io::Result<()> {
        if spill && matches!(self, Self::InMemory(_)) {
            let retained = match self {
                Self::InMemory(entries) => std::mem::take(entries),
                Self::Spilled(_) => Vec::new(),
            };
            let mut spilled = RecordSpill::new(
                temporary_storage,
                MAX_PLAN_SPILL_RECORD_BYTES,
                spill_directory,
            )?;
            for retained_entry in retained {
                spilled.push(&encode_spilled_entry(&retained_entry)?)?;
            }
            *self = Self::Spilled(RefCell::new(spilled));
        }
        match self {
            Self::InMemory(entries) => {
                entries
                    .try_reserve_exact(1)
                    .map_err(|error| io::Error::other(error.to_string()))?;
                entries.push(entry);
                Ok(())
            }
            Self::Spilled(spilled) => spilled.get_mut().push(&encode_spilled_entry(&entry)?),
        }
    }

    fn try_for_each<F>(&self, target: &Path, mut visit: F) -> Result<(), DeletionPlanError>
    where
        F: FnMut(&PlannedEntry) -> Result<(), DeletionPlanError>,
    {
        match self {
            Self::InMemory(entries) => {
                for entry in entries {
                    validate_entry_for_target(&entry.relative_path, target)?;
                    visit(entry)?;
                }
                Ok(())
            }
            Self::Spilled(spilled) => match spilled.borrow_mut().visit(|payload| {
                let entry =
                    decode_spilled_entry(&payload).map_err(|error| plan_io(target, error))?;
                validate_entry_for_target(&entry.relative_path, target)?;
                visit(&entry)
            }) {
                Ok(()) => Ok(()),
                Err(SpillVisitError::Io(error)) => Err(plan_io(target, error)),
                Err(SpillVisitError::Visitor(error)) => Err(error),
            },
        }
    }

    fn pop_reverse(&mut self, target: &Path) -> Result<Option<PlannedEntry>, DeletionPlanError> {
        match self {
            Self::InMemory(entries) => {
                let Some(entry) = entries.last() else {
                    return Ok(None);
                };
                validate_entry_for_target(&entry.relative_path, target)?;
                Ok(entries.pop())
            }
            Self::Spilled(spilled) => {
                let Some(payload) = spilled
                    .get_mut()
                    .pop()
                    .map_err(|error| plan_io(target, error))?
                else {
                    return Ok(None);
                };
                let entry =
                    decode_spilled_entry(&payload).map_err(|error| plan_io(target, error))?;
                validate_entry_for_target(&entry.relative_path, target)?;
                Ok(Some(entry))
            }
        }
    }
}

impl PendingDirectories {
    fn push(
        &mut self,
        entry: PlannedEntry,
        temporary_storage: &TemporaryStorage,
        spill_directory: &Path,
    ) -> io::Result<()> {
        if self.resident.len() < MAX_RESIDENT_DIRECTORY_TASKS {
            self.resident.push(entry);
            return Ok(());
        }
        if self.spill.is_none() {
            self.spill = Some(RecordSpill::new(
                temporary_storage,
                MAX_PLAN_SPILL_RECORD_BYTES,
                spill_directory,
            )?);
        }
        let Some(spill) = self.spill.as_mut() else {
            return Err(io::Error::other("directory plan spill was not initialized"));
        };
        spill.push(&encode_spilled_entry(&entry)?)
    }

    fn pop(&mut self, target: &Path) -> Result<Option<PlannedEntry>, DeletionPlanError> {
        let entry = if let Some(entry) = self.resident.pop() {
            entry
        } else {
            let Some(spill) = self.spill.as_mut() else {
                return Ok(None);
            };
            let Some(payload) = spill.pop().map_err(|error| plan_io(target, error))? else {
                return Ok(None);
            };
            decode_spilled_entry(&payload).map_err(|error| plan_io(target, error))?
        };
        validate_entry_for_target(&entry.relative_path, target)?;
        Ok(Some(entry))
    }
}

impl RecordSpill {
    fn new(
        temporary_storage: &TemporaryStorage,
        maximum_payload: usize,
        spill_directory: &Path,
    ) -> io::Result<Self> {
        let maximum_payload = u64::try_from(maximum_payload).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "deletion spill record limit does not fit in u64",
            )
        })?;
        let reservation = temporary_storage.reservation(0)?;
        let authentication = SpillAuthenticationKey::new()?;
        // Last, so that nothing fallible follows the creation of a named file.
        let file = new_plan_spill_file(spill_directory, temporary_storage)?;
        Ok(Self {
            file,
            length: 0,
            records: 0,
            reservation,
            maximum_payload,
            authentication,
        })
    }

    fn reserve_to(&mut self, bytes: u64) -> io::Result<()> {
        self.reservation.grow_to(bytes)
    }

    fn reserved_bytes(&self) -> u64 {
        self.reservation.bytes()
    }

    fn push(&mut self, payload: &[u8]) -> io::Result<()> {
        self.append(payload, true)
    }

    fn push_reserved(&mut self, payload: &[u8]) -> io::Result<()> {
        self.append(payload, false)
    }

    fn append(&mut self, payload: &[u8], grow_reservation: bool) -> io::Result<()> {
        let payload_len = self.payload_len(payload)?;
        let record_len = spill_record_len(payload_len)?;
        let next_length = self
            .length
            .checked_add(record_len)
            .ok_or_else(|| io::Error::other("deletion spill offset overflow"))?;
        let next_records = self
            .records
            .checked_add(1)
            .ok_or_else(|| io::Error::other("deletion spill count overflow"))?;
        self.verify_length()?;
        if grow_reservation {
            self.reservation.grow_to(next_length)?;
        } else if next_length > self.reservation.bytes() {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "deletion result exceeds its reserved temporary storage",
            ));
        }
        let header = payload_len.to_le_bytes();
        let tag = self.authentication.tag(self.length, header, payload);
        let file = plan_spill_file_mut(&mut self.file);
        file.seek(SeekFrom::Start(self.length))?;
        file.write_all(&header)?;
        file.write_all(payload)?;
        file.write_all(&tag)?;
        file.write_all(&header)?;
        self.length = next_length;
        self.records = next_records;
        Ok(())
    }

    fn pop(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.verify_length()?;
        if self.records == 0 {
            return if self.length == 0 {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "deletion spill has bytes without records",
                ))
            };
        }
        let trailer_offset = self
            .length
            .checked_sub(SPILL_RECORD_LENGTH_BYTES)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "deletion spill is truncated")
            })?;
        let mut trailer = [0_u8; 8];
        {
            let file = plan_spill_file_mut(&mut self.file);
            file.seek(SeekFrom::Start(trailer_offset))?;
            file.read_exact(&mut trailer)?;
        }
        let payload_len = u64::from_le_bytes(trailer);
        if payload_len > self.maximum_payload {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "deletion spill record exceeds the configured limit",
            ));
        }
        let record_len = spill_record_len(payload_len)?;
        let start = self.length.checked_sub(record_len).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "deletion spill record is truncated",
            )
        })?;
        let (payload, end) = self.read_record(start, self.length)?;
        if end != self.length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "deletion spill record length is inconsistent",
            ));
        }
        plan_spill_file_mut(&mut self.file).set_len(start)?;
        self.reservation.shrink_to(start);
        self.length = start;
        self.records = self
            .records
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("deletion spill count underflow"))?;
        Ok(Some(payload))
    }

    fn read_at(&mut self, offset: u64) -> io::Result<(Vec<u8>, u64)> {
        self.verify_length()?;
        self.read_record(offset, self.length)
    }

    fn visit<E>(
        &mut self,
        mut visit: impl FnMut(Vec<u8>) -> Result<(), E>,
    ) -> Result<(), SpillVisitError<E>> {
        self.verify_length().map_err(SpillVisitError::Io)?;
        let mut offset = 0_u64;
        for _ in 0..self.records {
            let (payload, next) = self
                .read_record(offset, self.length)
                .map_err(SpillVisitError::Io)?;
            visit(payload).map_err(SpillVisitError::Visitor)?;
            offset = next;
        }
        if offset != self.length {
            return Err(SpillVisitError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "deletion spill length does not match its records",
            )));
        }
        Ok(())
    }

    fn read_record(&mut self, offset: u64, end: u64) -> io::Result<(Vec<u8>, u64)> {
        read_spill_record(
            &mut self.file,
            &self.authentication,
            self.maximum_payload,
            offset,
            end,
        )
    }

    fn payload_len(&self, payload: &[u8]) -> io::Result<u64> {
        let length = u64::try_from(payload.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "deletion spill record length overflow",
            )
        })?;
        if length > self.maximum_payload {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "deletion spill record exceeds the configured limit",
            ));
        }
        Ok(length)
    }

    fn verify_length(&mut self) -> io::Result<()> {
        let actual = plan_spill_file_mut(&mut self.file).metadata()?.len();
        if actual == self.length {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "deletion spill file changed outside its accounting boundary",
            ))
        }
    }
}

fn encode_spilled_entry(entry: &PlannedEntry) -> io::Result<Vec<u8>> {
    serde_json::to_vec(&SpilledPlanEntry {
        relative_path: NativePath::new(entry.relative_path.clone()).encode(),
        snapshot: entry.snapshot.clone(),
    })
    .map_err(io::Error::other)
}

fn decode_spilled_entry(payload: &[u8]) -> io::Result<PlannedEntry> {
    let entry: SpilledPlanEntry = serde_json::from_slice(payload).map_err(io::Error::other)?;
    let relative_path = NativePath::decode(&entry.relative_path)
        .map_err(io::Error::other)?
        .as_path()
        .to_path_buf();
    Ok(PlannedEntry {
        relative_path,
        snapshot: entry.snapshot,
    })
}

fn spill_record_len(payload_len: u64) -> io::Result<u64> {
    SPILL_RECORD_LENGTH_BYTES
        .checked_add(payload_len)
        .and_then(|length| length.checked_add(SPILL_RECORD_MAC_BYTES))
        .and_then(|length| length.checked_add(SPILL_RECORD_LENGTH_BYTES))
        .ok_or_else(|| io::Error::other("deletion spill record length overflow"))
}

fn read_spill_record(
    file: &mut PlanSpillFile,
    authentication: &SpillAuthenticationKey,
    maximum_payload: u64,
    offset: u64,
    end: u64,
) -> io::Result<(Vec<u8>, u64)> {
    let header_end = offset
        .checked_add(SPILL_RECORD_LENGTH_BYTES)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "deletion spill offset overflow")
        })?;
    if header_end > end {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "deletion spill record is truncated",
        ));
    }
    let file = plan_spill_file_mut(file);
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0_u8; 8];
    file.read_exact(&mut header)?;
    let payload_len = u64::from_le_bytes(header);
    if payload_len > maximum_payload {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "deletion spill record exceeds the configured limit",
        ));
    }
    let record_len = spill_record_len(payload_len)?;
    let record_end = offset.checked_add(record_len).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "deletion spill offset overflow")
    })?;
    if record_end > end {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "deletion spill record is truncated",
        ));
    }
    let payload_len = usize::try_from(payload_len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "deletion spill record length does not fit in memory",
        )
    })?;
    let mut payload = vec![0_u8; payload_len];
    file.read_exact(&mut payload)?;
    let mut tag = [0_u8; SPILL_MAC_BYTES];
    file.read_exact(&mut tag)?;
    let mut trailer = [0_u8; 8];
    file.read_exact(&mut trailer)?;
    if trailer != header {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "deletion spill record length is inconsistent",
        ));
    }
    if !authentication.matches(offset, header, &payload, &tag) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "deletion spill record authentication failed",
        ));
    }
    Ok((payload, record_end))
}

/// A new spill file. On Windows, where an anonymous temporary file is not available, it is a
/// named file in the user's tree, kept with the registration that keeps every reader of that
/// tree away from it; the storage's registry is the session's.
#[cfg(windows)]
fn new_plan_spill_file(
    spill_directory: &Path,
    temporary_storage: &TemporaryStorage,
) -> io::Result<PlanSpillFile> {
    crate::os::windows::create_private_temporary_file(
        spill_directory,
        temporary_storage.private_files(),
    )
}

#[cfg(not(windows))]
fn new_plan_spill_file(
    _spill_directory: &Path,
    _temporary_storage: &TemporaryStorage,
) -> io::Result<PlanSpillFile> {
    tempfile::tempfile()
}

fn plan_spill_file_mut(file: &mut PlanSpillFile) -> &mut File {
    file
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeletionEntryOutcome {
    Deleted,
    Changed(String),
    Missing,
    Failed(String),
    Unattempted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionEntryResult {
    pub entry: PlannedEntry,
    pub outcome: DeletionEntryOutcome,
}

#[derive(Deserialize, Serialize)]
struct SpilledDeletionEntryResult {
    relative_path: EncodedNativePath,
    snapshot: PlannedSnapshot,
    outcome: DeletionEntryOutcome,
}

#[derive(Debug)]
enum PlannedResultStorage {
    InMemory {
        maximum_bytes: usize,
        potential_spill_bytes: u64,
    },
    Spilled(RecordSpill),
}

impl PlannedResultStorage {
    fn new(maximum_bytes: usize) -> Self {
        Self::InMemory {
            maximum_bytes,
            potential_spill_bytes: 0,
        }
    }

    fn reserve_for(
        &mut self,
        entry: &PlannedEntry,
        spill: bool,
        temporary_storage: &TemporaryStorage,
        spill_directory: &Path,
    ) -> io::Result<()> {
        let record_bytes = result_spill_record_capacity(entry)?;
        match self {
            Self::InMemory {
                potential_spill_bytes,
                ..
            } => {
                let next = potential_spill_bytes
                    .checked_add(record_bytes)
                    .ok_or_else(|| {
                        io::Error::other("deletion result spill reservation overflow")
                    })?;
                if spill {
                    let mut result_spill = RecordSpill::new(
                        temporary_storage,
                        MAX_RESULT_SPILL_RECORD_BYTES,
                        spill_directory,
                    )?;
                    result_spill.reserve_to(next)?;
                    *self = Self::Spilled(result_spill);
                } else {
                    *potential_spill_bytes = next;
                }
            }
            Self::Spilled(result_spill) => {
                let next = result_spill
                    .reserved_bytes()
                    .checked_add(record_bytes)
                    .ok_or_else(|| {
                        io::Error::other("deletion result spill reservation overflow")
                    })?;
                result_spill.reserve_to(next)?;
            }
        }
        Ok(())
    }

    fn into_collector(self, target: PathBuf) -> ResultCollector {
        match self {
            Self::InMemory { maximum_bytes, .. } => ResultCollector::InMemory {
                entries: Vec::new(),
                estimated_bytes: 0,
                maximum_bytes,
                target,
                target_removed: false,
                summary: DeletionSummary::default(),
            },
            Self::Spilled(result_spill) => ResultCollector::Spilled {
                result_spill,
                target,
                target_removed: false,
                summary: DeletionSummary::default(),
            },
        }
    }
}

#[derive(Debug)]
enum ResultCollector {
    InMemory {
        entries: Vec<DeletionEntryResult>,
        estimated_bytes: usize,
        maximum_bytes: usize,
        target: PathBuf,
        target_removed: bool,
        summary: DeletionSummary,
    },
    Spilled {
        result_spill: RecordSpill,
        target: PathBuf,
        target_removed: bool,
        summary: DeletionSummary,
    },
}

fn result_removes_target(result: &DeletionEntryResult, target: &Path) -> bool {
    result.entry.relative_path == target
        && matches!(
            result.outcome,
            DeletionEntryOutcome::Deleted | DeletionEntryOutcome::Missing
        )
}

impl ResultCollector {
    fn push(&mut self, mut result: DeletionEntryResult) -> io::Result<()> {
        bound_outcome_detail(&mut result.outcome);
        match self {
            Self::InMemory {
                entries,
                estimated_bytes,
                maximum_bytes,
                target,
                target_removed,
                summary,
            } => {
                *target_removed |= result_removes_target(&result, target);
                summary.note(&result);
                let required = result_entry_resident_bytes(&result);
                let next = estimated_bytes.saturating_add(required);
                if next > *maximum_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::StorageFull,
                        "deletion result exceeds its resident storage limit",
                    ));
                }
                entries
                    .try_reserve_exact(1)
                    .map_err(|error| io::Error::other(error.to_string()))?;
                *estimated_bytes = next;
                entries.push(result);
            }
            Self::Spilled {
                result_spill,
                target,
                target_removed,
                summary,
            } => {
                *target_removed |= result_removes_target(&result, target);
                summary.note(&result);
                let payload = encode_spilled_result(&result)?;
                result_spill.push_reserved(&payload)?;
            }
        }
        Ok(())
    }

    /// Records that a file or link that may have left the tree in this run may have had other
    /// links.
    const fn note_other_links(&mut self) {
        match self {
            Self::InMemory { summary, .. } | Self::Spilled { summary, .. } => {
                summary.deleted_with_other_links = true;
            }
        }
    }

    fn finish(self, target: &Path, complete: bool, error: Option<String>) -> DeletionEntries {
        let error = error.map(|detail| bounded_outcome_detail(&detail));
        match self {
            Self::InMemory {
                entries,
                target_removed,
                summary,
                ..
            } => DeletionEntries::in_memory(
                Some(target.to_path_buf()),
                entries,
                summary,
                target_removed,
                complete,
                error,
            ),
            Self::Spilled {
                result_spill,
                target_removed,
                summary,
                ..
            } => DeletionEntries::spilled(
                target.to_path_buf(),
                result_spill,
                summary,
                target_removed,
                complete,
                error,
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct DeletionSummary {
    deleted: u64,
    changed: u64,
    missing: u64,
    failed: u64,
    unattempted: u64,
    deleted_apparent_bytes: u128,
    deleted_allocated_bytes: u128,
    /// A file or link that may have left the tree during the run may have had other links when it
    /// went. A fixed-size answer for the whole run: the report keeps no list of them. The
    /// executor sets it from what it saw of the object's links as the entry was removed (the
    /// links that outlived the removal, counted through a reference or handle held across it, see
    /// [`removed_with_other_links`]), for every file or link it did not leave unattempted,
    /// whatever the entry's outcome says: a removal can be followed by a failure, and an entry
    /// that failed, changed, or was already gone may have been taken out by another process. It
    /// is not derived from the recorded entries, whose count is deliberately conservative where a
    /// platform cannot keep it: a Windows regular file's is never recorded, and every removal of
    /// one would then count.
    deleted_with_other_links: bool,
}

impl DeletionSummary {
    fn note(&mut self, result: &DeletionEntryResult) {
        match &result.outcome {
            DeletionEntryOutcome::Deleted => {
                self.deleted = self.deleted.saturating_add(1);
                self.deleted_apparent_bytes = self
                    .deleted_apparent_bytes
                    .saturating_add(result.entry.snapshot.apparent_bytes);
                if matches!(
                    result.entry.snapshot.kind,
                    PlannedKind::File | PlannedKind::Link
                ) && result.entry.snapshot.identity.link_count == Some(1)
                {
                    self.deleted_allocated_bytes = self
                        .deleted_allocated_bytes
                        .saturating_add(result.entry.snapshot.allocated_bytes.unwrap_or(0));
                }
            }
            DeletionEntryOutcome::Changed(_) => self.changed = self.changed.saturating_add(1),
            DeletionEntryOutcome::Missing => self.missing = self.missing.saturating_add(1),
            DeletionEntryOutcome::Failed(_) => self.failed = self.failed.saturating_add(1),
            DeletionEntryOutcome::Unattempted => {
                self.unattempted = self.unattempted.saturating_add(1);
            }
        }
    }

    fn from_entries(entries: &[DeletionEntryResult]) -> Self {
        let mut summary = Self::default();
        for entry in entries {
            summary.note(entry);
        }
        summary
    }
}

#[derive(Clone, Debug)]
pub struct DeletionEntries {
    storage: DeletionEntriesStorage,
    target: Option<PathBuf>,
    records: u64,
    summary: DeletionSummary,
    target_removed: bool,
    complete: bool,
    error: Option<String>,
}

#[derive(Clone, Debug)]
enum DeletionEntriesStorage {
    InMemory(Vec<DeletionEntryResult>),
    Spilled(Arc<Mutex<RecordSpill>>),
}

impl DeletionEntries {
    fn in_memory(
        target: Option<PathBuf>,
        entries: Vec<DeletionEntryResult>,
        summary: DeletionSummary,
        target_removed: bool,
        complete: bool,
        error: Option<String>,
    ) -> Self {
        let records = u64::try_from(entries.len()).unwrap_or(u64::MAX);
        Self {
            storage: DeletionEntriesStorage::InMemory(entries),
            target,
            records,
            summary,
            target_removed,
            complete,
            error,
        }
    }

    fn spilled(
        target: PathBuf,
        result_spill: RecordSpill,
        summary: DeletionSummary,
        target_removed: bool,
        complete: bool,
        error: Option<String>,
    ) -> Self {
        let records = result_spill.records;
        Self {
            storage: DeletionEntriesStorage::Spilled(Arc::new(Mutex::new(result_spill))),
            target: Some(target),
            records,
            summary,
            target_removed,
            complete,
            error,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        usize::try_from(self.records).unwrap_or(usize::MAX)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records == 0
    }

    #[must_use]
    pub fn reporting_complete(&self) -> bool {
        self.complete
    }

    #[must_use]
    pub fn reporting_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    #[must_use]
    pub(crate) const fn target_removed(&self) -> bool {
        self.target_removed
    }

    /// Whether a file or link that may have left the tree during the run may have had other links
    /// when it went: the executor did not see the removal take its last link with it (see
    /// [`removed_with_other_links`]), whatever the entry's outcome says, or the entry was already
    /// gone when it reached it.
    #[must_use]
    pub(crate) const fn deleted_with_other_links(&self) -> bool {
        self.summary.deleted_with_other_links
    }

    #[must_use]
    pub fn iter(&self) -> DeletionEntriesIter<'_> {
        let target = self.target.as_deref();
        let inner = match &self.storage {
            DeletionEntriesStorage::InMemory(entries) => {
                DeletionEntriesIterInner::InMemory(entries.iter())
            }
            DeletionEntriesStorage::Spilled(result_spill) => match result_spill.lock() {
                Ok(result_spill) => DeletionEntriesIterInner::Spilled {
                    result_spill,
                    target,
                    offset: 0,
                    remaining: self.records,
                },
                Err(_) => DeletionEntriesIterInner::Failed(Some(io::Error::other(
                    "deletion result storage lock was poisoned",
                ))),
            },
        };
        DeletionEntriesIter { inner }
    }

    #[cfg(test)]
    pub(crate) fn is_spilled(&self) -> bool {
        matches!(self.storage, DeletionEntriesStorage::Spilled(_))
    }
}

impl<'a> IntoIterator for &'a DeletionEntries {
    type Item = io::Result<DeletionEntryResult>;
    type IntoIter = DeletionEntriesIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl From<Vec<DeletionEntryResult>> for DeletionEntries {
    fn from(entries: Vec<DeletionEntryResult>) -> Self {
        let summary = DeletionSummary::from_entries(&entries);
        Self::in_memory(None, entries, summary, false, true, None)
    }
}

pub struct DeletionEntriesIter<'a> {
    inner: DeletionEntriesIterInner<'a>,
}

enum DeletionEntriesIterInner<'a> {
    InMemory(std::slice::Iter<'a, DeletionEntryResult>),
    Spilled {
        result_spill: MutexGuard<'a, RecordSpill>,
        target: Option<&'a Path>,
        offset: u64,
        remaining: u64,
    },
    Failed(Option<io::Error>),
}

impl Iterator for DeletionEntriesIter<'_> {
    type Item = io::Result<DeletionEntryResult>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            DeletionEntriesIterInner::InMemory(entries) => entries.next().cloned().map(Ok),
            DeletionEntriesIterInner::Spilled {
                result_spill,
                target,
                offset,
                remaining,
            } => {
                if *remaining == 0 {
                    return None;
                }
                match result_spill.read_at(*offset) {
                    Ok((payload, next)) => {
                        match decode_spilled_result_for_target(&payload, *target) {
                            Ok(result) => {
                                *offset = next;
                                *remaining = remaining.saturating_sub(1);
                                Some(Ok(result))
                            }
                            Err(error) => {
                                *remaining = 0;
                                Some(Err(error))
                            }
                        }
                    }
                    Err(error) => {
                        *remaining = 0;
                        Some(Err(error))
                    }
                }
            }
            DeletionEntriesIterInner::Failed(error) => error.take().map(Err),
        }
    }
}

fn encode_spilled_result(result: &DeletionEntryResult) -> io::Result<Vec<u8>> {
    serde_json::to_vec(&SpilledDeletionEntryResult {
        relative_path: NativePath::new(result.entry.relative_path.clone()).encode(),
        snapshot: result.entry.snapshot.clone(),
        outcome: result.outcome.clone(),
    })
    .map_err(io::Error::other)
}

fn decode_spilled_result(payload: &[u8]) -> io::Result<DeletionEntryResult> {
    let result: SpilledDeletionEntryResult =
        serde_json::from_slice(payload).map_err(io::Error::other)?;
    let relative_path = NativePath::decode(&result.relative_path)
        .map_err(io::Error::other)?
        .as_path()
        .to_path_buf();
    Ok(DeletionEntryResult {
        entry: PlannedEntry {
            relative_path,
            snapshot: result.snapshot,
        },
        outcome: result.outcome,
    })
}

fn decode_spilled_result_for_target(
    payload: &[u8],
    target: Option<&Path>,
) -> io::Result<DeletionEntryResult> {
    let result = decode_spilled_result(payload)?;
    if let Some(target) = target {
        validate_entry_for_target(&result.entry.relative_path, target).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "deletion result record is outside the selected target",
            )
        })?;
    }
    Ok(result)
}

fn result_spill_record_capacity(entry: &PlannedEntry) -> io::Result<u64> {
    let placeholder = DeletionEntryResult {
        entry: entry.clone(),
        outcome: DeletionEntryOutcome::Changed(String::new()),
    };
    let base = encode_spilled_result(&placeholder)?.len();
    let detail = MAX_OUTCOME_DETAIL_BYTES
        .checked_mul(MAX_JSON_ESCAPED_BYTES_PER_INPUT_BYTE)
        .ok_or_else(|| io::Error::other("deletion result spill detail length overflow"))?;
    let payload = base
        .checked_add(detail)
        .and_then(|bytes| bytes.checked_add(RESULT_SPILL_ENVELOPE_BYTES))
        .ok_or_else(|| io::Error::other("deletion result spill record length overflow"))?;
    if payload > MAX_RESULT_SPILL_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "deletion result spill record exceeds the configured limit",
        ));
    }
    let payload = u64::try_from(payload).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "deletion result spill record length does not fit in u64",
        )
    })?;
    spill_record_len(payload)
}

fn bound_outcome_detail(outcome: &mut DeletionEntryOutcome) {
    match outcome {
        DeletionEntryOutcome::Changed(detail) | DeletionEntryOutcome::Failed(detail) => {
            *detail = bounded_outcome_detail(detail);
        }
        DeletionEntryOutcome::Deleted
        | DeletionEntryOutcome::Missing
        | DeletionEntryOutcome::Unattempted => {}
    }
}

fn bounded_outcome_detail(detail: &str) -> String {
    if detail.len() <= MAX_OUTCOME_DETAIL_BYTES {
        return detail.to_owned();
    }
    let mut end = MAX_OUTCOME_DETAIL_BYTES.saturating_sub(OUTCOME_DETAIL_TRUNCATION.len());
    while !detail.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut bounded = String::with_capacity(MAX_OUTCOME_DETAIL_BYTES);
    bounded.push_str(&detail[..end]);
    bounded.push_str(OUTCOME_DETAIL_TRUNCATION);
    bounded
}

fn planned_entry_resident_bytes(entry: &PlannedEntry) -> usize {
    size_of::<PlannedEntry>()
        .saturating_add(
            entry
                .relative_path
                .as_os_str()
                .as_encoded_bytes()
                .len()
                .saturating_mul(2),
        )
        .saturating_add(128)
}

fn result_entry_resident_bytes(result: &DeletionEntryResult) -> usize {
    let detail_bytes = match &result.outcome {
        DeletionEntryOutcome::Changed(detail) | DeletionEntryOutcome::Failed(detail) => {
            detail.len()
        }
        DeletionEntryOutcome::Deleted
        | DeletionEntryOutcome::Missing
        | DeletionEntryOutcome::Unattempted => 0,
    };
    size_of::<DeletionEntryResult>()
        .saturating_add(
            result
                .entry
                .relative_path
                .as_os_str()
                .as_encoded_bytes()
                .len()
                .saturating_mul(2),
        )
        .saturating_add(detail_bytes)
        .saturating_add(128)
}

fn result_entry_bound(entry: &PlannedEntry) -> usize {
    size_of::<DeletionEntryResult>()
        .saturating_add(
            entry
                .relative_path
                .as_os_str()
                .as_encoded_bytes()
                .len()
                .saturating_mul(2),
        )
        .saturating_add(MAX_OUTCOME_DETAIL_BYTES)
        .saturating_add(128)
}

#[derive(Clone, Debug)]
pub struct DeletionReport {
    pub target_node_id: NodeId,
    pub root_relative_path: PathBuf,
    pub scan_root: PathBuf,
    pub entries: DeletionEntries,
    pub soft_cancelled: bool,
    pub precise: bool,
    /// What the deletion history is charged for the report: what the plan held in memory, which
    /// never exceeds the budget the plan was built with ([`DeletionPlan::estimated_bytes`]).
    pub estimated_bytes: usize,
}

impl DeletionReport {
    #[must_use]
    pub fn deleted_entries(&self) -> u64 {
        self.entries.summary.deleted
    }

    #[must_use]
    pub fn changed_entries(&self) -> u64 {
        self.entries.summary.changed
    }

    #[must_use]
    pub fn missing_entries(&self) -> u64 {
        self.entries.summary.missing
    }

    #[must_use]
    pub fn failed_entries(&self) -> u64 {
        self.entries.summary.failed
    }

    #[must_use]
    pub fn unattempted_entries(&self) -> u64 {
        self.entries.summary.unattempted
    }

    #[must_use]
    pub fn deleted_apparent_bytes(&self) -> u128 {
        self.entries.summary.deleted_apparent_bytes
    }

    #[must_use]
    pub fn deleted_allocated_bytes(&self) -> u128 {
        self.entries.summary.deleted_allocated_bytes
    }

    /// Whether the exact target itself was removed under a complete, precise run.
    #[must_use]
    pub(crate) fn target_was_removed(&self) -> bool {
        !self.soft_cancelled
            && self.precise
            && self.reporting_complete()
            && self.entries.target_removed()
    }

    /// Whether a file or link that may have left the tree during the run may have had other links
    /// when it went: the executor did not see the removal take its last link with it, whatever
    /// the entry's outcome says, or the entry was already gone when it reached it. The map has
    /// its files by path and cannot say where the others are, so it cannot be updated in place
    /// after such a removal.
    #[must_use]
    pub(crate) const fn deleted_files_may_have_other_links(&self) -> bool {
        self.entries.deleted_with_other_links()
    }

    /// This report, as one in which no file or link was seen to leave a link behind. For the tests
    /// of what the owner does with a report that says so, which are about the owner: what the
    /// executor can prove of links through the handle that removed a file depends on the file
    /// system, and is tested where the executor reads it (on Unix the report says so as it is).
    #[cfg(all(test, windows))]
    #[must_use]
    pub(crate) fn assuming_no_link_survived(mut self) -> Self {
        self.entries.summary.deleted_with_other_links = false;
        self
    }

    #[must_use]
    pub fn reporting_complete(&self) -> bool {
        self.entries.reporting_complete()
    }

    #[must_use]
    pub fn reporting_error(&self) -> Option<&str> {
        self.entries.reporting_error()
    }
}

#[derive(Clone, Debug)]
pub enum DeletionPlanError {
    Synthetic,
    Root,
    InvalidRelativePath,
    Changed,
    Missing(PathBuf),
    Cancelled,
    MemoryLimit {
        limit: usize,
    },
    Io {
        path: String,
        message: String,
        kind: io::ErrorKind,
    },
    /// The file system reported a value for an entry that planning cannot keep, such as a
    /// negative size. The entry, and so the plan, is refused.
    Unrepresentable {
        path: String,
        message: String,
    },
}
impl fmt::Display for DeletionPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Synthetic => formatter.write_str("synthetic summary nodes cannot be deleted"),
            Self::Root => {
                formatter.write_str("scan, filesystem, and mount roots cannot be deleted")
            }
            Self::InvalidRelativePath => {
                formatter.write_str("deletion target is not a safe relative path")
            }
            Self::Changed => {
                formatter.write_str("deletion target changed while its plan was built")
            }
            Self::Missing(path) => write!(
                formatter,
                "planned deletion entry is missing: {}",
                safe_display_path_text(path)
            ),
            Self::Cancelled => formatter.write_str("deletion planning was cancelled"),
            Self::MemoryLimit { limit } => write!(
                formatter,
                "deletion plan exceeds its {limit} byte memory limit"
            ),
            Self::Io { path, message, .. } | Self::Unrepresentable { path, message } => write!(
                formatter,
                "deletion planning failed for {}: {}",
                safe_display_text(path),
                safe_display_text(message)
            ),
        }
    }
}

impl std::error::Error for DeletionPlanError {}

impl DeletionPlanError {
    #[must_use]
    pub(crate) const fn is_stale(&self) -> bool {
        matches!(
            self,
            Self::Changed
                | Self::Io {
                    kind: io::ErrorKind::NotFound,
                    ..
                }
        )
    }

    #[must_use]
    pub(crate) const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    #[must_use]
    pub(crate) const fn is_missing(&self) -> bool {
        matches!(self, Self::Missing(_))
    }
}

/// Builds and revalidates an identity-bound deletion plan.
///
/// # Errors
/// Returns a planning error when the target is ineligible, changed, unreadable, cancelled, or
/// cannot be retained within its bounded plan and temporary storage budgets.
pub fn build_plan(
    scan_root: &Path,
    target: FileToDelete,
    reduced_guardrails: bool,
) -> Result<DeletionPlan, DeletionPlanError> {
    build_plan_cancellable(
        scan_root,
        target,
        reduced_guardrails,
        &AtomicBool::new(false),
        DEFAULT_PLAN_LIMIT_BYTES,
    )
}

/// Builds an identity-bound deletion plan with explicit cancellation and memory limits.
///
/// File and link plans stay resident and must fit within `maximum_bytes`. Directory plans spill
/// their reviewed identities to bounded private temporary storage after that resident budget.
///
/// # Errors
/// Returns a planning error when the target is ineligible, changed, unreadable, cancelled, or
/// cannot be retained within the configured bounds.
pub fn build_plan_cancellable(
    scan_root: &Path,
    target: FileToDelete,
    reduced_guardrails: bool,
    cancelled: &AtomicBool,
    maximum_bytes: usize,
) -> Result<DeletionPlan, DeletionPlanError> {
    let temporary_storage = TemporaryStorage::default();
    build_plan_cancellable_with_temporary_storage(
        scan_root,
        target,
        reduced_guardrails,
        cancelled,
        maximum_bytes,
        &temporary_storage,
    )
}

pub(crate) fn build_plan_cancellable_with_temporary_storage(
    scan_root: &Path,
    target: FileToDelete,
    reduced_guardrails: bool,
    cancelled: &AtomicBool,
    maximum_bytes: usize,
    temporary_storage: &TemporaryStorage,
) -> Result<DeletionPlan, DeletionPlanError> {
    let scan_root_identity = current_scan_root_identity(scan_root)?;
    build_plan_cancellable_with_root_identity_and_temporary_storage(
        scan_root,
        scan_root_identity,
        target,
        reduced_guardrails,
        cancelled,
        maximum_bytes,
        temporary_storage,
    )
}

#[allow(clippy::too_many_lines)]
pub(crate) fn build_plan_cancellable_with_root_identity_and_temporary_storage(
    scan_root: &Path,
    scan_root_identity: NativeIdentity,
    mut target: FileToDelete,
    reduced_guardrails: bool,
    cancelled: &AtomicBool,
    maximum_bytes: usize,
    temporary_storage: &TemporaryStorage,
) -> Result<DeletionPlan, DeletionPlanError> {
    if target.synthetic {
        return Err(DeletionPlanError::Synthetic);
    }
    let relative = relative_target(&target)?;
    let full_path = target.full_path();
    let spill_directory = deletion_spill_directory(&full_path)?;
    let directory_target = target.expected_snapshot.kind == NodeKind::Directory;
    let root = open_root(scan_root, &scan_root_identity)?;
    // The target, like every folder below it, is checked from the handle of the folder that holds
    // it and its own name, never from a whole path: how long that path is must not decide whether
    // a folder can be planned.
    let (snapshot, directory_handle) = {
        let (parent, name) = open_parent(&root, &relative)?;
        if directory_target {
            let mount_root = match entry_is_mount_root(scan_root, &relative, &parent, &name) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => return Err(plan_io(&relative, error)),
            };
            if mount_root {
                let unchanged =
                    target
                        .expected_snapshot
                        .identity
                        .as_ref()
                        .is_some_and(|expected| {
                            inspect_entry(&parent, &name, &relative).is_ok_and(|(actual, _)| {
                                actual.kind == PlannedKind::Directory
                                    && same_object(expected, &actual.identity)
                            })
                        });
                return Err(if unchanged {
                    DeletionPlanError::Root
                } else {
                    DeletionPlanError::Changed
                });
            }
        }
        inspect_entry(&parent, &name, &relative)?
    };
    validate_model_snapshot(&target, &snapshot)?;
    let challenge = challenge_for(&target, &snapshot, reduced_guardrails);
    let root_snapshot = snapshot.clone();
    let mut entries = PlanEntries::InMemory(Vec::new());
    let mut result_storage = PlannedResultStorage::new(maximum_bytes);
    let mut estimated_bytes = 0;
    let mut apparent_bytes = root_snapshot.apparent_bytes;
    {
        let mut planning_storage = PlanningStorage {
            entries: &mut entries,
            result_storage: &mut result_storage,
            temporary_storage,
            spill_directory,
        };
        planning_storage.push(
            PlannedEntry {
                relative_path: relative.clone(),
                snapshot,
            },
            &mut estimated_bytes,
            maximum_bytes,
            directory_target,
            &relative,
        )?;

        if directory_handle.is_some() {
            drop(directory_handle);
            let mut pending = PendingDirectories {
                resident: Vec::new(),
                spill: None,
            };
            pending
                .push(
                    PlannedEntry {
                        relative_path: relative.clone(),
                        snapshot: root_snapshot.clone(),
                    },
                    temporary_storage,
                    spill_directory,
                )
                .map_err(|error| plan_io(&relative, error))?;
            while let Some(directory) = pending.pop(&relative)? {
                if cancelled.load(Ordering::Acquire) {
                    return Err(DeletionPlanError::Cancelled);
                }
                let relative_path = directory.relative_path;
                let expected = directory.snapshot;
                let (actual, handle) = {
                    let (parent, name) = open_parent(&root, &relative_path)?;
                    if relative_path != relative {
                        let mounted =
                            entry_is_mount_root(scan_root, &relative_path, &parent, &name)
                                .map_err(|error| plan_io(&relative_path, error))?;
                        if mounted {
                            return Err(DeletionPlanError::Root);
                        }
                    }
                    match inspect_entry(&parent, &name, &relative_path) {
                        Err(DeletionPlanError::Missing(_)) => {
                            return Err(DeletionPlanError::Changed);
                        }
                        result => result?,
                    }
                };
                if actual != expected {
                    return Err(DeletionPlanError::Changed);
                }
                let handle = handle.ok_or(DeletionPlanError::Changed)?;
                let read_dir = cap_fs::read_base_dir(&handle)
                    .map_err(|error| plan_io(&relative_path, error))?;
                for child in read_dir {
                    if cancelled.load(Ordering::Acquire) {
                        return Err(DeletionPlanError::Cancelled);
                    }
                    let child = child.map_err(|error| plan_io(&relative_path, error))?;
                    let name = child.file_name();
                    validate_component(&name)?;
                    let child_relative = relative_path.join(&name);
                    let (child_snapshot, child_directory) =
                        inspect_child(&handle, &name, &child_relative)?;
                    let directory = child_directory.is_some();
                    drop(child_directory);
                    let entry = PlannedEntry {
                        relative_path: child_relative,
                        snapshot: child_snapshot,
                    };
                    apparent_bytes = apparent_bytes.saturating_add(entry.snapshot.apparent_bytes);
                    let pending_entry = directory.then(|| entry.clone());
                    planning_storage.push(
                        entry,
                        &mut estimated_bytes,
                        maximum_bytes,
                        directory_target,
                        &relative,
                    )?;
                    if let Some(directory) = pending_entry {
                        pending
                            .push(directory, temporary_storage, spill_directory)
                            .map_err(|error| plan_io(&relative, error))?;
                    }
                }
            }
        }
    }
    target
        .reviewed_entries
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    // Directory targets ordinarily have no retained model review. The planner
    // relies on the fresh live walk above. If a caller supplied one, compare it
    // without materializing spilled records.
    if !target.reviewed_entries.is_empty() {
        let reviewed_len = u64::try_from(target.reviewed_entries.len()).unwrap_or(u64::MAX);
        if entries.len() != reviewed_len {
            return Err(DeletionPlanError::Changed);
        }
        entries.try_for_each(&relative, |entry| {
            let matches_review = target
                .reviewed_entries
                .binary_search_by(|reviewed| reviewed.relative_path.cmp(&entry.relative_path))
                .ok()
                .and_then(|index| target.reviewed_entries.get(index))
                .is_some_and(|reviewed| reviewed.snapshot == entry.snapshot);
            if matches_review {
                Ok(())
            } else {
                Err(DeletionPlanError::Changed)
            }
        })?;
    }

    let plan = DeletionPlan {
        target,
        root_relative_path: relative,
        scan_root_identity,
        entries,
        result_storage,
        root_snapshot,
        challenge,
        apparent_bytes,
        #[cfg(any(target_os = "linux", target_vendor = "apple"))]
        private_files: temporary_storage.private_files().clone(),
        estimated_bytes,
    };
    revalidate_plan_cancellable(scan_root, &plan, cancelled)?;
    Ok(plan)
}

/// Revalidates every planned identity against the live filesystem.
///
/// # Errors
/// Returns an error if any entry changed, disappeared, became unreadable, or escaped the scan root.
pub fn revalidate_plan(scan_root: &Path, plan: &DeletionPlan) -> Result<(), DeletionPlanError> {
    revalidate_plan_cancellable(scan_root, plan, &AtomicBool::new(false))
}

pub(crate) fn revalidate_plan_cancellable(
    scan_root: &Path,
    plan: &DeletionPlan,
    cancelled: &AtomicBool,
) -> Result<(), DeletionPlanError> {
    let root = open_root(scan_root, &plan.scan_root_identity)?;
    plan.entries
        .try_for_each(&plan.root_relative_path, |entry| {
            if cancelled.load(Ordering::Acquire) {
                return Err(DeletionPlanError::Cancelled);
            }
            let (actual, _) = match inspect_relative(&root, &entry.relative_path) {
                Err(DeletionPlanError::Missing(path)) if path == plan.root_relative_path => {
                    return Err(DeletionPlanError::Missing(path));
                }
                Err(DeletionPlanError::Missing(_)) => return Err(DeletionPlanError::Changed),
                result => result?,
            };
            if actual != entry.snapshot {
                return Err(DeletionPlanError::Changed);
            }
            Ok(())
        })
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
pub fn execute_plan(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
) -> DeletionReport {
    execute_plan_unix(scan_root, plan, soft_cancelled, hard_cancelled)
}

#[cfg(windows)]
pub fn execute_plan(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
) -> DeletionReport {
    execute_plan_windows(
        scan_root,
        plan,
        soft_cancelled,
        hard_cancelled,
        None,
        || true,
        || true,
    )
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
pub fn execute_plan(
    scan_root: &Path,
    mut plan: DeletionPlan,
    _soft_cancelled: &AtomicBool,
    _hard_cancelled: &AtomicBool,
) -> DeletionReport {
    let result_storage = std::mem::replace(&mut plan.result_storage, PlannedResultStorage::new(0));
    let target = plan.root_relative_path.clone();
    failed_report(
        scan_root,
        plan,
        result_storage.into_collector(target),
        "permanent deletion is unavailable on this target",
    )
}
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
pub(crate) fn execute_plan_counted<C, M>(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    progress: &AtomicU64,
    try_claim_mutation: C,
    try_begin_mutation: M,
) -> DeletionReport
where
    C: FnMut() -> bool,
    M: FnMut() -> bool,
{
    execute_plan_unix_with_mutation_gate(
        scan_root,
        plan,
        soft_cancelled,
        hard_cancelled,
        try_claim_mutation,
        try_begin_mutation,
        || {},
        |_| {
            progress.fetch_add(1, Ordering::Release);
        },
        isolate_entry,
    )
}

#[cfg(windows)]
pub(crate) fn execute_plan_counted<C, M>(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    progress: &AtomicU64,
    try_claim_mutation: C,
    try_begin_mutation: M,
) -> DeletionReport
where
    C: FnMut() -> bool,
    M: FnMut() -> bool,
{
    execute_plan_windows(
        scan_root,
        plan,
        soft_cancelled,
        hard_cancelled,
        Some(progress),
        try_claim_mutation,
        try_begin_mutation,
    )
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
pub(crate) fn execute_plan_counted<C, M>(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    _progress: &AtomicU64,
    _try_claim_mutation: C,
    _try_begin_mutation: M,
) -> DeletionReport
where
    C: FnMut() -> bool,
    M: FnMut() -> bool,
{
    execute_plan(scan_root, plan, soft_cancelled, hard_cancelled)
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn execute_plan_unix(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
) -> DeletionReport {
    execute_plan_unix_with_hook(scan_root, plan, soft_cancelled, hard_cancelled, || {})
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn execute_plan_unix_with_hook<F>(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    after_isolation: F,
) -> DeletionReport
where
    F: FnMut(),
{
    execute_plan_unix_with_hooks(
        scan_root,
        plan,
        soft_cancelled,
        hard_cancelled,
        after_isolation,
        |_| {},
    )
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn execute_plan_unix_with_hooks<F, G>(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    after_isolation: F,
    after_inspection: G,
) -> DeletionReport
where
    F: FnMut(),
    G: FnMut(&OsStr),
{
    execute_plan_unix_with_mutation_gate(
        scan_root,
        plan,
        soft_cancelled,
        hard_cancelled,
        || true,
        || true,
        after_isolation,
        after_inspection,
        isolate_entry,
    )
}

/// [`execute_plan`] with a test's hook run once the executor has inspected an entry where it
/// isolated it, given that name: the one moment of an entry's removal that a test cannot reach
/// from outside.
#[cfg(all(test, any(target_os = "linux", target_vendor = "apple")))]
pub(crate) fn execute_plan_with_hook_after_inspection<G>(
    scan_root: &Path,
    plan: DeletionPlan,
    after_inspection: G,
) -> DeletionReport
where
    G: FnMut(&OsStr),
{
    execute_plan_unix_with_hooks(
        scan_root,
        plan,
        &AtomicBool::new(false),
        &AtomicBool::new(false),
        || {},
        after_inspection,
    )
}

/// Whether the executor can show, in the folders tests make, that a file it removed left no link
/// behind. Linux and Windows always can: Linux reads the object's link count through a reference
/// held across the removal, and Windows through the handle that removes it. macOS can where the
/// system resolves an object by its identity, which it does on APFS, where the executor works
/// (it isolates an entry by exchanging two names, which HFS+ and exFAT refuse); that is asked of
/// a fresh file in the temporary folder with the system's own `stat`, not with the executor's
/// code. Where it cannot, every file or link counts as possibly linked, and the map is scanned
/// again after such a deletion.
#[cfg(test)]
pub(crate) fn proves_no_link_survived() -> bool {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::unix::fs::MetadataExt as _;

        static RESOLVES_BY_IDENTITY: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
            let probe = tempfile::NamedTempFile::new().expect("the probe file should exist");
            let metadata = probe
                .as_file()
                .metadata()
                .expect("the probe file should be readable");
            std::fs::symlink_metadata(format!("/.vol/{}/{}", metadata.dev(), metadata.ino()))
                .is_ok()
        });
        *RESOLVES_BY_IDENTITY
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        true
    }
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[allow(
    clippy::too_many_arguments,
    reason = "the executor needs cancellation flags and five allocation-free mutation hooks"
)]
fn execute_plan_unix_with_mutation_gate<C, M, F, G, I>(
    scan_root: &Path,
    mut plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    mut try_claim_mutation: C,
    mut try_begin_mutation: M,
    mut after_isolation: F,
    mut after_inspection: G,
    mut isolate: I,
) -> DeletionReport
where
    C: FnMut() -> bool,
    M: FnMut() -> bool,
    F: FnMut(),
    G: FnMut(&OsStr),
    I: FnMut(&File, &OsStr, &OsStr) -> io::Result<()>,
{
    let result_storage = std::mem::replace(&mut plan.result_storage, PlannedResultStorage::new(0));
    let root_relative_path = plan.root_relative_path.clone();
    let mut results = result_storage.into_collector(root_relative_path.clone());
    let root = match open_root(scan_root, &plan.scan_root_identity) {
        Ok(root) => root,
        Err(error) => return failed_report(scan_root, plan, results, &error.to_string()),
    };
    if let Err(error) = plan.entries.try_for_each(&root_relative_path, |_| Ok(())) {
        return failed_report(scan_root, plan, results, &error.to_string());
    }
    let mut stopped = false;
    while let Some(mut entry) = match plan.entries.pop_reverse(&root_relative_path) {
        Ok(entry) => entry,
        Err(error) => {
            return plan_read_failure_report(
                scan_root,
                plan,
                results,
                &error,
                soft_cancelled.load(Ordering::Acquire),
            );
        }
    } {
        if stopped
            || soft_cancelled.load(Ordering::Acquire)
            || hard_cancelled.load(Ordering::Acquire)
        {
            stopped = true;
            let result = DeletionEntryResult {
                entry,
                outcome: DeletionEntryOutcome::Unattempted,
            };
            if let Err(error) = results.push(result) {
                return result_storage_failure_report(
                    scan_root,
                    plan,
                    results,
                    &error,
                    soft_cancelled.load(Ordering::Acquire),
                );
            }
            continue;
        }
        let mut isolation = Isolation {
            deep: past_path_max(scan_root, &entry.relative_path),
            refused: false,
            no_link_survived: false,
        };
        let outcome = execute_unix_entry(
            &root,
            &TransientNames::new(scan_root, &entry.relative_path, &plan.private_files),
            &mut entry,
            soft_cancelled,
            &mut try_claim_mutation,
            &mut try_begin_mutation,
            &mut after_isolation,
            &mut after_inspection,
            &mut isolate,
            &mut isolation,
        );
        if matches!(outcome, DeletionEntryOutcome::Unattempted) {
            stopped = true;
            soft_cancelled.store(true, Ordering::Release);
        }
        if isolation.deep && isolation.refused {
            // The system refuses the rename that isolates an entry this deep, even with the
            // placeholder named as the exchange's source (`isolate_entry`), and a folder goes only
            // after everything in it, so the target cannot be removed whole. Stopping leaves the
            // rest of it as it is, instead of stripping the entries around the one that cannot
            // go. This entry is recorded failed, with the reason; every later one is not run. The
            // run is not marked cancelled: the user did not cancel it.
            stopped = true;
        }
        if removed_with_other_links(entry.snapshot.kind, &outcome, isolation.no_link_survived) {
            results.note_other_links();
        }
        if matches!(outcome, DeletionEntryOutcome::Deleted) {
            note_deleted_link(&mut entry);
        }
        let result = DeletionEntryResult { entry, outcome };
        if let Err(error) = results.push(result) {
            return result_storage_failure_report(
                scan_root,
                plan,
                results,
                &error,
                soft_cancelled.load(Ordering::Acquire),
            );
        }
    }
    finish_report(
        scan_root,
        plan,
        results,
        soft_cancelled.load(Ordering::Acquire),
        !hard_cancelled.load(Ordering::Acquire),
    )
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one entry's whole isolation protocol, with the hooks that tests and the progress count use"
)]
fn execute_unix_entry<C, M, F, G, I>(
    root: &File,
    names: &TransientNames<'_>,
    entry: &mut PlannedEntry,
    soft_cancelled: &AtomicBool,
    try_claim_mutation: &mut C,
    try_begin_mutation: &mut M,
    after_isolation: &mut F,
    after_inspection: &mut G,
    isolate: &mut I,
    isolation: &mut Isolation,
) -> DeletionEntryOutcome
where
    C: FnMut() -> bool,
    M: FnMut() -> bool,
    F: FnMut(),
    G: FnMut(&OsStr),
    I: FnMut(&File, &OsStr, &OsStr) -> io::Result<()>,
{
    let (parent, original_name) = match open_parent(root, &entry.relative_path) {
        Ok(value) => value,
        Err(DeletionPlanError::Io {
            kind: io::ErrorKind::NotFound,
            ..
        }) => {
            return DeletionEntryOutcome::Changed(
                "entry parent or namespace disappeared".to_string(),
            );
        }
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    if !try_claim_mutation() {
        return DeletionEntryOutcome::Unattempted;
    }
    // The registration of the detached name lasts until this entry's outcome is known, whichever
    // way it ends (it is released by its drop), and the name is gone for good by then.
    let (detached_name, placeholder, _registered) = match create_placeholder(&parent, names) {
        Ok(value) => value,
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    if !try_begin_mutation() {
        soft_cancelled.store(true, Ordering::Release);
        return remove_verified_placeholder(&parent, &detached_name, &placeholder).map_or_else(
            |error| {
                DeletionEntryOutcome::Failed(format!(
                    "cancellation placeholder cleanup failed: {error}"
                ))
            },
            |()| DeletionEntryOutcome::Unattempted,
        );
    }
    if let Err(error) = isolate(&parent, &original_name, &detached_name) {
        let disappeared = error.kind() == io::ErrorKind::NotFound;
        let cleanup = remove_verified_placeholder(&parent, &detached_name, &placeholder);
        return if disappeared {
            cleanup.map_or_else(
                |cleanup_error| {
                    DeletionEntryOutcome::Failed(format!(
                        "target disappeared; placeholder cleanup failed: {cleanup_error}"
                    ))
                },
                |()| DeletionEntryOutcome::Missing,
            )
        } else {
            isolation.refused = true;
            let detail = cleanup.map_or_else(
                |cleanup_error| format!("{error}; placeholder cleanup failed: {cleanup_error}"),
                |()| error.to_string(),
            );
            DeletionEntryOutcome::Failed(if isolation.deep {
                format!("the system refuses the rename that isolates an entry this deep: {detail}")
            } else {
                detail
            })
        };
    }
    after_isolation();
    let actual = match inspect_child(&parent, &detached_name, &entry.relative_path) {
        Ok((snapshot, handle)) => {
            drop(handle);
            entry.snapshot.identity.link_count = snapshot.identity.link_count;
            snapshot
        }
        Err(DeletionPlanError::Io {
            kind: io::ErrorKind::NotFound,
            ..
        }) => {
            return match finalize_placeholder(
                &parent,
                names,
                &original_name,
                &detached_name,
                &placeholder,
            ) {
                Ok(()) => DeletionEntryOutcome::Missing,
                Err(error) => DeletionEntryOutcome::Failed(format!(
                    "isolated entry disappeared; namespace cleanup failed: {error}"
                )),
            };
        }
        Err(error) => {
            let restore = restore_detached(&parent, &original_name, &detached_name, &placeholder);
            return DeletionEntryOutcome::Failed(restore.map_or_else(
                |restore_error| format!("{error}; namespace recovery failed: {restore_error}"),
                |()| error.to_string(),
            ));
        }
    };
    if !matches_for_execution(&entry.snapshot, &actual) {
        return match restore_detached(&parent, &original_name, &detached_name, &placeholder) {
            Ok(()) => DeletionEntryOutcome::Changed(
                "identity, type, size, allocation, or modification changed".to_string(),
            ),
            Err(error) => DeletionEntryOutcome::Failed(format!(
                "entry changed; namespace recovery failed: {error}"
            )),
        };
    }
    // The detached name lives in a parent that other processes may still write.
    // Revalidate it at the last practical point before naming it for deletion.
    after_inspection(&detached_name);
    let revalidated = match inspect_child(&parent, &detached_name, &entry.relative_path) {
        Ok((snapshot, handle)) => {
            drop(handle);
            snapshot
        }
        Err(DeletionPlanError::Io {
            kind: io::ErrorKind::NotFound,
            ..
        }) => {
            return match finalize_placeholder(
                &parent,
                names,
                &original_name,
                &detached_name,
                &placeholder,
            ) {
                Ok(()) => DeletionEntryOutcome::Missing,
                Err(error) => DeletionEntryOutcome::Failed(format!(
                    "isolated entry disappeared; namespace cleanup failed: {error}"
                )),
            };
        }
        Err(error) => {
            let restore = restore_detached(&parent, &original_name, &detached_name, &placeholder);
            return DeletionEntryOutcome::Failed(restore.map_or_else(
                |restore_error| format!("{error}; namespace recovery failed: {restore_error}"),
                |()| error.to_string(),
            ));
        }
    };
    if !matches_for_execution(&entry.snapshot, &revalidated) {
        return match restore_detached(&parent, &original_name, &detached_name, &placeholder) {
            Ok(()) => DeletionEntryOutcome::Changed(
                "identity, type, size, allocation, or modification changed".to_string(),
            ),
            Err(error) => DeletionEntryOutcome::Failed(format!(
                "entry changed; namespace recovery failed: {error}"
            )),
        };
    }

    // The link count at this last look before the removal is the one the space it frees is
    // counted from, so a link made after the first look is in it. Once a file or link is removed
    // only the reference below can keep that count: an entry it cannot be made for, or whose
    // count it cannot read, is left with none, and its space is not counted as freed.
    entry.snapshot.identity.link_count = revalidated.identity.link_count;

    // A reference to the object, held across its removal, so that the links that outlive the
    // removal can be counted through it once the removed name is gone. A count read through a
    // name has to be read while the name is there, and a link made after that read is not in it.
    // An object that already had other links needs none: it is known to have had them. One that
    // the system cannot make a reference to counts as possibly linked once it is removed.
    let reference = if matches!(entry.snapshot.kind, PlannedKind::File | PlannedKind::Link)
        && actual.identity.link_count == Some(1)
    {
        LinkReference::open(&parent, &detached_name, &actual.identity)
    } else {
        None
    };

    let removal = match entry.snapshot.kind {
        PlannedKind::Directory => cap_fs::remove_dir(&parent, Path::new(&detached_name)),
        PlannedKind::File | PlannedKind::Link => {
            cap_fs::remove_file(&parent, Path::new(&detached_name))
        }
    };
    match removal {
        Ok(()) => {
            if matches!(entry.snapshot.kind, PlannedKind::File | PlannedKind::Link) {
                // The removed name was the last name the executor held for the object, and no
                // other process can name a removed object, so a link the reference still counts
                // is one that outlived the removal. Only an object that had one link when it was
                // isolated and has none now is proven to have left none; anything the executor
                // could not read, or could not reference, is a link that may remain, and its
                // space is not counted as freed.
                isolation.no_link_survived =
                    reference.as_ref().and_then(LinkReference::links) == Some(0);
                if !isolation.no_link_survived {
                    entry.snapshot.identity.link_count = None;
                }
            }
            match finalize_placeholder(&parent, names, &original_name, &detached_name, &placeholder)
            {
                Ok(()) => DeletionEntryOutcome::Deleted,
                Err(error) => DeletionEntryOutcome::Failed(format!(
                    "target deleted; namespace cleanup failed: {error}"
                )),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match finalize_placeholder(&parent, names, &original_name, &detached_name, &placeholder)
            {
                Ok(()) => DeletionEntryOutcome::Missing,
                Err(error) => DeletionEntryOutcome::Failed(format!(
                    "target disappeared; namespace cleanup failed: {error}"
                )),
            }
        }
        Err(error) => {
            let restore = restore_detached(&parent, &original_name, &detached_name, &placeholder);
            if error.kind() == io::ErrorKind::DirectoryNotEmpty {
                restore.map_or_else(
                    |restore_error| {
                        DeletionEntryOutcome::Failed(format!(
                            "directory changed; namespace recovery failed: {restore_error}"
                        ))
                    },
                    |()| {
                        DeletionEntryOutcome::Changed(
                            "directory contains a new or changed entry".to_string(),
                        )
                    },
                )
            } else {
                DeletionEntryOutcome::Failed(restore.map_or_else(
                    |restore_error| format!("{error}; namespace recovery failed: {restore_error}"),
                    |()| error.to_string(),
                ))
            }
        }
    }
}

#[cfg(windows)]
fn execute_plan_windows<C, M>(
    scan_root: &Path,
    mut plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    progress: Option<&AtomicU64>,
    mut try_claim_mutation: C,
    mut try_begin_mutation: M,
) -> DeletionReport
where
    C: FnMut() -> bool,
    M: FnMut() -> bool,
{
    let result_storage = std::mem::replace(&mut plan.result_storage, PlannedResultStorage::new(0));
    let root_relative_path = plan.root_relative_path.clone();
    let mut results = result_storage.into_collector(root_relative_path.clone());
    let root = match open_root(scan_root, &plan.scan_root_identity) {
        Ok(root) => root,
        Err(error) => return failed_report(scan_root, plan, results, &error.to_string()),
    };
    if let Err(error) = plan.entries.try_for_each(&root_relative_path, |_| Ok(())) {
        return failed_report(scan_root, plan, results, &error.to_string());
    }
    let mut stopped = false;
    while let Some(mut entry) = match plan.entries.pop_reverse(&root_relative_path) {
        Ok(entry) => entry,
        Err(error) => {
            return plan_read_failure_report(
                scan_root,
                plan,
                results,
                &error,
                soft_cancelled.load(Ordering::Acquire),
            );
        }
    } {
        if stopped
            || soft_cancelled.load(Ordering::Acquire)
            || hard_cancelled.load(Ordering::Acquire)
        {
            stopped = true;
            let result = DeletionEntryResult {
                entry,
                outcome: DeletionEntryOutcome::Unattempted,
            };
            if let Err(error) = results.push(result) {
                return result_storage_failure_report(
                    scan_root,
                    plan,
                    results,
                    &error,
                    soft_cancelled.load(Ordering::Acquire),
                );
            }
            continue;
        }
        let mut no_link_survived = false;
        let outcome = execute_windows_entry(
            &root,
            &mut entry,
            &mut try_claim_mutation,
            &mut try_begin_mutation,
            &mut no_link_survived,
        );
        if matches!(outcome, DeletionEntryOutcome::Unattempted) {
            stopped = true;
            soft_cancelled.store(true, Ordering::Release);
        }
        if removed_with_other_links(entry.snapshot.kind, &outcome, no_link_survived) {
            results.note_other_links();
        }
        if matches!(outcome, DeletionEntryOutcome::Deleted) {
            note_deleted_link(&mut entry);
        }
        if !matches!(outcome, DeletionEntryOutcome::Unattempted)
            && let Some(progress) = progress
        {
            progress.fetch_add(1, Ordering::Release);
        }
        let result = DeletionEntryResult { entry, outcome };
        if let Err(error) = results.push(result) {
            return result_storage_failure_report(
                scan_root,
                plan,
                results,
                &error,
                soft_cancelled.load(Ordering::Acquire),
            );
        }
    }
    finish_report(
        scan_root,
        plan,
        results,
        soft_cancelled.load(Ordering::Acquire),
        !hard_cancelled.load(Ordering::Acquire),
    )
}

#[cfg(windows)]
fn execute_windows_entry<C, M>(
    root: &File,
    entry: &mut PlannedEntry,
    try_claim_mutation: &mut C,
    try_begin_mutation: &mut M,
    no_link_survived: &mut bool,
) -> DeletionEntryOutcome
where
    C: FnMut() -> bool,
    M: FnMut() -> bool,
{
    use cap_primitives::fs::{_WindowsByHandle as _, OpenOptionsExt as _};

    const DELETE: u32 = 0x0001_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    let (parent, name) = match open_parent(root, &entry.relative_path) {
        Ok(value) => value,
        Err(DeletionPlanError::Io {
            kind: io::ErrorKind::NotFound,
            ..
        }) => return DeletionEntryOutcome::Missing,
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    let mut options = cap_fs::OpenOptions::new();
    options
        .access_mode(DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        ._cap_fs_ext_follow(FollowSymlinks::No);
    let handle = match cap_fs::open(&parent, Path::new(&name), &options) {
        Ok(handle) => handle,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return DeletionEntryOutcome::Missing;
        }
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    let metadata = match cap_fs::Metadata::from_file(&handle) {
        Ok(metadata) => metadata,
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    let kind = if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        PlannedKind::Link
    } else if metadata.is_dir() {
        PlannedKind::Directory
    } else {
        PlannedKind::File
    };
    let actual = match snapshot_from_open_file(&handle, kind) {
        Ok(snapshot) => snapshot,
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    // The count as the executor found the file, before it removes it. It is not the answer: a link
    // made after this read, and before the removal, survives it. The answer is the count the same
    // handle reads after the removal.
    let links_when_opened = actual.identity.link_count;
    if kind == PlannedKind::File {
        // A pathname-independent post-open hard-link count can still change
        // before handle deletion. Report regular-file allocation conservatively.
        entry.snapshot.identity.link_count = None;
    } else {
        entry.snapshot.identity.link_count = actual.identity.link_count;
    }
    if !matches_for_execution(&entry.snapshot, &actual) {
        return DeletionEntryOutcome::Changed(
            "identity, type, size, allocation, or modification changed".to_string(),
        );
    }
    if !try_claim_mutation() || !try_begin_mutation() {
        return DeletionEntryOutcome::Unattempted;
    }
    match remove_open_handle(&handle) {
        Ok(()) => {
            // The removal took one name off the object. A file system that does not say so
            // through the handle that removed it (the name may leave the count only when the
            // handle closes, or the count may not be kept at all) leaves a count other than zero,
            // and then every removal of a file counts as possibly leaving a link, which is the
            // safe answer: only an object that had one link, and reads none after its removal, is
            // proven to have left none.
            *no_link_survived =
                links_when_opened == Some(1) && link_count_of_open_file(&handle) == Some(0);
            DeletionEntryOutcome::Deleted
        }
        Err(error) if error.raw_os_error() == Some(145) => {
            DeletionEntryOutcome::Changed("directory contains a new or changed entry".to_string())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => DeletionEntryOutcome::Missing,
        Err(error) => DeletionEntryOutcome::Failed(error.to_string()),
    }
}

fn failed_report(
    scan_root: &Path,
    mut plan: DeletionPlan,
    mut results: ResultCollector,
    message: &str,
) -> DeletionReport {
    let message = bounded_outcome_detail(message);
    let root_relative_path = plan.root_relative_path.clone();
    loop {
        let entry = match plan.entries.pop_reverse(&root_relative_path) {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                return plan_read_failure_report(scan_root, plan, results, &error, false);
            }
        };
        let result = DeletionEntryResult {
            entry,
            outcome: DeletionEntryOutcome::Failed(message.clone()),
        };
        if let Err(error) = results.push(result) {
            return result_storage_failure_report(scan_root, plan, results, &error, false);
        }
    }
    finish_report(scan_root, plan, results, false, true)
}

fn plan_read_failure_report(
    scan_root: &Path,
    plan: DeletionPlan,
    mut results: ResultCollector,
    error: &DeletionPlanError,
    soft_cancelled: bool,
) -> DeletionReport {
    let marker = DeletionEntryResult {
        entry: PlannedEntry {
            relative_path: plan.root_relative_path.clone(),
            snapshot: plan.root_snapshot.clone(),
        },
        outcome: DeletionEntryOutcome::Failed(format!(
            "deletion plan storage failed during execution: {error}"
        )),
    };
    if let Err(storage_error) = results.push(marker) {
        return result_storage_failure_report(
            scan_root,
            plan,
            results,
            &storage_error,
            soft_cancelled,
        );
    }
    incomplete_report(
        scan_root,
        plan,
        results,
        soft_cancelled,
        "deletion plan storage could not be read; no further entries were executed",
    )
}

fn result_storage_failure_report(
    scan_root: &Path,
    plan: DeletionPlan,
    results: ResultCollector,
    error: &io::Error,
    soft_cancelled: bool,
) -> DeletionReport {
    incomplete_report(
        scan_root,
        plan,
        results,
        soft_cancelled,
        &format!("deletion result storage failed during execution: {error}"),
    )
}

fn finish_report(
    scan_root: &Path,
    plan: DeletionPlan,
    results: ResultCollector,
    soft_cancelled: bool,
    precise: bool,
) -> DeletionReport {
    report_from_parts(
        scan_root,
        plan,
        results,
        soft_cancelled,
        precise,
        true,
        None,
    )
}

fn incomplete_report(
    scan_root: &Path,
    plan: DeletionPlan,
    results: ResultCollector,
    soft_cancelled: bool,
    error: &str,
) -> DeletionReport {
    report_from_parts(
        scan_root,
        plan,
        results,
        soft_cancelled,
        false,
        false,
        Some(bounded_outcome_detail(error)),
    )
}

fn report_from_parts(
    scan_root: &Path,
    plan: DeletionPlan,
    results: ResultCollector,
    soft_cancelled: bool,
    precise: bool,
    complete: bool,
    error: Option<String>,
) -> DeletionReport {
    let target_node_id = plan.target.node_id;
    let root_relative_path = plan.root_relative_path;
    let estimated_bytes = plan.estimated_bytes;
    let entries = results.finish(&root_relative_path, complete, error);
    DeletionReport {
        target_node_id,
        root_relative_path,
        scan_root: scan_root.to_path_buf(),
        entries,
        soft_cancelled,
        precise,
        estimated_bytes,
    }
}

/// What the executor tells `execute_unix_entry` about an entry's depth, and learns back about its
/// isolation and removal.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[derive(Clone, Copy, Debug)]
struct Isolation {
    /// The entry's path is too long for a path-based call: see [`past_path_max`].
    deep: bool,
    /// The system refused the rename that isolates the entry, for a reason other than the entry
    /// no longer being there.
    refused: bool,
    /// The executor removed a file or link and saw every link to it go with the removal: the
    /// object had one link when it was isolated, and has none now, which it read through a
    /// reference held across the removal (`LinkReference`). False for anything else, including an
    /// entry that was already gone, one the executor could not make a reference to, and one it
    /// could not count.
    no_link_survived: bool,
}

/// `PATH_MAX`: the longest path, terminator included, that a path-based system call accepts
/// (4,096 bytes on Linux, 1,024 on macOS).
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
const PATH_MAX: usize = nix::libc::PATH_MAX as usize;

/// Whether the path of the entry at `relative` below `scan_root` is too long for a path-based
/// system call. Planning and execution reach such an entry through directory handles, name by
/// name, but a system may still refuse the rename that isolates it: macOS 14 refuses with
/// `ENOSPC` any rename whose source is a folder whose path is that long, which is why
/// [`isolate_entry`] names the placeholder as the source. A system that refuses it anyway ends
/// the run at that entry.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn past_path_max(scan_root: &Path, relative: &Path) -> bool {
    scan_root
        .as_os_str()
        .len()
        .saturating_add(1)
        .saturating_add(relative.as_os_str().len())
        >= PATH_MAX
}

/// Where the executor makes the transient names of one entry, and the registry that keeps every
/// reader of the tree away from them: a name the executor makes in the folder that holds the entry
/// is registered by its exact path (the scan root, the entry's folder, and the name), which is the
/// path the scanner builds for the same name. The scanner and the overlay's listing skip a
/// registered path without reading it, so neither records the placeholder or the isolated target
/// as an entry of the user's while the name is registered. The registration ends when the entry's
/// outcome is known; a scanner that took a name from a listing before then, and reaches it after,
/// finds it gone.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
struct TransientNames<'a> {
    folder: PathBuf,
    files: &'a PrivateFiles,
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
impl<'a> TransientNames<'a> {
    fn new(scan_root: &Path, entry: &Path, files: &'a PrivateFiles) -> Self {
        Self {
            folder: scan_root.join(entry.parent().unwrap_or_else(|| Path::new(""))),
            files,
        }
    }

    fn register(&self, name: &OsStr) -> TransientName {
        self.files.register(self.folder.join(name))
    }
}

/// How many unpredictable names a placeholder operation tries before giving up.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
const PLACEHOLDER_NAME_ATTEMPTS: usize = 128;

/// An unpredictable name for a deletion placeholder, unique to this process and call.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn placeholder_name() -> io::Result<OsString> {
    use std::sync::atomic::AtomicU64;

    static NEXT_PLACEHOLDER: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_PLACEHOLDER.fetch_add(1, Ordering::Relaxed);
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    let token = random
        .iter()
        .fold(String::with_capacity(32), |mut token, byte| {
            use std::fmt::Write as _;

            let _ = write!(token, "{byte:02x}");
            token
        });
    Ok(OsString::from(format!(
        ".excise-delete-{token}-{:x}-{sequence:x}",
        std::process::id()
    )))
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn create_placeholder(
    parent: &File,
    names: &TransientNames<'_>,
) -> io::Result<(OsString, PlannedSnapshot, TransientName)> {
    for _ in 0..PLACEHOLDER_NAME_ATTEMPTS {
        let name = placeholder_name()?;
        // Registered before the name exists, and by its exact path: a scan that lists the folder
        // sees either no such name or one that is registered.
        let registered = names.register(&name);
        let mut options = cap_fs::OpenOptions::new();
        options.write(true).create_new(true);
        match cap_fs::open(parent, Path::new(&name), &options) {
            Ok(file) => {
                let metadata = file.metadata()?;
                return snapshot_from_std_metadata(&metadata, PlannedKind::File)
                    .map(|snapshot| (name, snapshot, registered));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve an isolated deletion name",
    ))
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn exchange_names(parent: &File, left: &OsStr, right: &OsStr) -> io::Result<()> {
    rustix::fs::renameat_with(
        parent,
        Path::new(left),
        parent,
        Path::new(right),
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .map_err(io::Error::from)
}

/// Isolates the entry `original` of `parent`: exchanges its name with `detached`, the name of the
/// placeholder reserved for it, so that `original` then holds the placeholder and `detached` the
/// entry.
///
/// The placeholder, not the entry, is named as the exchange's source. An exchange swaps both names
/// whichever is named first, but a system can tell them apart: macOS 14 refuses with `ENOSPC` ("No
/// space left on device") any rename whose source is a folder whose path is longer than
/// `PATH_MAX`, an exchange included, and accepts one whose source is a file at any depth. The
/// placeholder is always a file ([`create_placeholder`]), so this isolates a folder of any depth
/// wherever a file can be renamed. The executor's other renames name the placeholder as their
/// source too: [`restore_detached`] and [`finalize_placeholder`] check that `original` still holds
/// the placeholder before they rename from it.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn isolate_entry(parent: &File, original: &OsStr, detached: &OsStr) -> io::Result<()> {
    exchange_names(parent, detached, original)
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn remove_verified_placeholder(
    parent: &File,
    name: &OsStr,
    expected: &PlannedSnapshot,
) -> io::Result<()> {
    let (actual, handle) = inspect_child(parent, name, Path::new(name))
        .map_err(|error| io::Error::other(error.to_string()))?;
    drop(handle);
    if actual != *expected {
        return Err(io::Error::other(
            "isolated deletion placeholder identity changed",
        ));
    }
    cap_fs::remove_file(parent, Path::new(name))
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn restore_detached(
    parent: &File,
    original: &OsStr,
    detached: &OsStr,
    placeholder: &PlannedSnapshot,
) -> io::Result<()> {
    let (actual, handle) = inspect_child(parent, original, Path::new(original))
        .map_err(|error| io::Error::other(error.to_string()))?;
    drop(handle);
    if actual != *placeholder {
        return Err(io::Error::other(
            "original name no longer contains the deletion placeholder",
        ));
    }
    exchange_names(parent, original, detached)?;
    remove_verified_placeholder(parent, detached, placeholder)
}

/// Removes the placeholder that holds a deleted entry's original name.
///
/// The placeholder first moves to a private name, so that the entry removed is the one checked.
/// That is normally the detached name, which the deleted target has just vacated. APFS on macOS 14
/// can still refuse it as taken for a moment after the removal, so a refused name is never reused
/// or waited on: the placeholder moves to a fresh private name instead. The rename never replaces
/// an entry, and the placeholder is checked again before it is removed.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn finalize_placeholder(
    parent: &File,
    names: &TransientNames<'_>,
    original: &OsStr,
    detached: &OsStr,
    placeholder: &PlannedSnapshot,
) -> io::Result<()> {
    let (actual, handle) = inspect_child(parent, original, Path::new(original))
        .map_err(|error| io::Error::other(error.to_string()))?;
    drop(handle);
    if actual != *placeholder {
        return Err(io::Error::other(
            "original name no longer contains the deletion placeholder",
        ));
    }
    let mut private = detached.to_os_string();
    // The detached name is registered by the caller; a fresh one is registered here, before the
    // placeholder is moved to it, and released once the placeholder is gone from it.
    let mut registered_private: Option<TransientName> = None;
    for _ in 0..PLACEHOLDER_NAME_ATTEMPTS {
        match rustix::fs::renameat_with(
            parent,
            Path::new(original),
            parent,
            Path::new(&private),
            rustix::fs::RenameFlags::NOREPLACE,
        ) {
            Ok(()) => return remove_verified_placeholder(parent, &private, placeholder),
            Err(rustix::io::Errno::EXIST) => {
                private = placeholder_name()?;
                registered_private.replace(names.register(&private));
            }
            Err(error) => return Err(io::Error::from(error)),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a private name to remove the deletion placeholder",
    ))
}

/// A reference to the object an entry names, held across the removal of the names the executor
/// controls, so that the links that outlive the removal can be counted through it afterwards.
///
/// A count read through a name has to be read while the name is there, and a link made after that
/// read is not in it. A reference outlives every name, and its count is the object's own: zero
/// once the last link is gone, whatever the number was and whenever the links were made.
///
/// It is made without opening the object. A plain open cannot be trusted to leave an object
/// alone: the open of an entry that another process swapped for a FIFO blocks, and the open of a
/// file whose content is somewhere else makes the system fetch it.
///
/// Linux holds an `O_PATH` descriptor, opened without following a link, which has no effect on
/// any kind of object, a FIFO, a device, and a socket included, and cannot block or fetch
/// anything. An object that is not the one that was inspected gets none, and the executor then
/// counts the removal as one that may have left a link.
#[cfg(target_os = "linux")]
struct LinkReference(OwnedFd);

#[cfg(target_os = "linux")]
impl LinkReference {
    /// A reference to the object that `name` names in `parent`, when that is `expected`.
    fn open(parent: &File, name: &OsStr, expected: &NativeIdentity) -> Option<Self> {
        let descriptor = openat(
            parent,
            name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()?;
        let (_, identity) = EntryMetadata::from_stat(&fstat(&descriptor).ok()?).ok()?;
        same_object(expected, &identity).then_some(Self(descriptor))
    }

    /// How many links the object has now.
    fn links(&self) -> Option<u64> {
        let (_, identity) = EntryMetadata::from_stat(&fstat(&self.0).ok()?).ok()?;
        identity.link_count
    }
}

/// The longest path that names an object by its identity: `/.vol/`, a device number of up to ten
/// digits, a separator, and an inode number of up to twenty.
#[cfg(target_vendor = "apple")]
const BY_IDENTITY_PATH_BYTES: usize = 6 + 10 + 1 + 20;

/// A reference to the object an entry names, held across the removal of the names the executor
/// controls, so that the links that outlive the removal can be counted through it afterwards. A
/// count read through a name has to be read while the name is there, and a link made after that
/// read is not in it; the object's own count is zero once its last link is gone, whenever the
/// links were made.
///
/// macOS opens nothing it removes: a plain open can block on an entry that another process
/// swapped for a FIFO, can make the system fetch a file whose content is somewhere else, and
/// cannot open a socket at all. The reference is the object's identity instead. The system
/// resolves `/.vol/<device>/<inode>` to the object with that identity for as long as the object
/// has a link, and answers `ENOENT` once it has none. A lookup there is an `lstat`, which opens,
/// fetches, and waits for nothing, whatever the object is, and it names a symbolic link itself,
/// never what it points at. Any answer but `ENOENT` counts against the proof, whichever object
/// gives it.
///
/// `ENOENT` is also what the system answers when it cannot resolve the identity at all, for
/// whatever reason: a file system that does not support the lookup, a volume that has gone, a
/// lookup that fails at that moment. The answer after the removal therefore proves something
/// only when the same lookup found the object before it, and the reference is made only then; for
/// any other object the executor counts the removal as one that may have left a link. What is
/// left is a lookup that fails in the instant between the removal and the second look, and it
/// would also need another process to have linked the object in that instant, because a
/// reference is made only for an object with one link. The count decides only whether the map is
/// scanned again and what space is counted as freed, never whether a deletion is safe.
#[cfg(target_vendor = "apple")]
struct LinkReference {
    path: [u8; BY_IDENTITY_PATH_BYTES],
    len: usize,
}

#[cfg(target_vendor = "apple")]
impl LinkReference {
    /// A reference to the object that `expected` identifies, when the system resolves it by that
    /// identity now. Linux names the object by `parent` and `name`; this does not need them.
    fn open(_parent: &File, _name: &OsStr, expected: &NativeIdentity) -> Option<Self> {
        let FileId::Inode {
            device_id,
            inode_number,
        } = &expected.file_id
        else {
            return None;
        };
        // The path spells the device as the system's signed 32 bits; one that does not fit has no
        // spelling, and no reference.
        let device = i32::try_from(*device_id).ok()?;
        let mut path = [0_u8; BY_IDENTITY_PATH_BYTES];
        let mut unwritten = &mut path[..];
        write!(unwritten, "/.vol/{device}/{inode_number}").ok()?;
        let len = BY_IDENTITY_PATH_BYTES - unwritten.len();
        let reference = Self { path, len };
        let (_, found) = EntryMetadata::from_stat(&lstat(reference.bytes()).ok()?).ok()?;
        same_object(expected, &found).then_some(reference)
    }

    /// How many links the object has now: none once the system no longer resolves it.
    fn links(&self) -> Option<u64> {
        match lstat(self.bytes()) {
            Ok(stat) => EntryMetadata::from_stat(&stat).ok()?.1.link_count,
            Err(rustix::io::Errno::NOENT) => Some(0),
            Err(_) => None,
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.path[..self.len]
    }
}

/// The folder at `relative` below `scan_root` as it is now: the snapshot a scan would record for
/// it, and its direct entries, each handed to `visit` with its name, kind, and identity as the
/// scan reads them. `visit` returns whether to go on.
///
/// The folder is read the way the planner reads a target, through the handles of the folders
/// above it, so the length of the path does not matter and a link on the way is not followed.
/// Each entry is read by its name from the folder's own handle, with one no-follow stat and
/// never opened, so a folder this process cannot open is listed like any other, as the scan
/// listed it. The snapshot is read before the listing and again after it: a folder that changed
/// meanwhile is not described, because what was listed is then not one moment of it. Nothing is
/// held of the listing: its size does not grow what this keeps.
///
/// `skip` is asked of each name first, and a name it accepts is neither read nor visited: it is
/// one of Excise's own, which the scan leaves out of the map, and one that is held open with no
/// sharing cannot be read at all.
///
/// # Errors
/// Returns an error when the scan root is no longer the folder the scan read, when nothing is at
/// `relative`, when what is there is not a folder, when the folder or one of its entries cannot
/// be read, when the folder changed while it was listed, or when `visit` stopped the listing
/// ([`DeletionPlanError::Cancelled`]).
pub(crate) fn current_folder_listing(
    scan_root: &Path,
    scan_root_identity: &NativeIdentity,
    relative: &Path,
    skip: impl Fn(&OsStr) -> bool,
    mut visit: impl FnMut(&OsStr, PlannedKind, &NativeIdentity) -> bool,
) -> Result<EntrySnapshot, DeletionPlanError> {
    let root = open_root(scan_root, scan_root_identity)?;
    let opened;
    // The scan root is listed through its own handle, which `open_root` has checked.
    let (before, handle) = if relative.as_os_str().is_empty() {
        let before = snapshot_from_open_file(&root, PlannedKind::Directory)
            .map_err(|error| plan_io(relative, error))?;
        (before, &root)
    } else {
        let (before, handle) = inspect_relative(&root, relative)?;
        let Some(handle) = handle.filter(|_| before.kind == PlannedKind::Directory) else {
            return Err(DeletionPlanError::Changed);
        };
        opened = handle;
        (before, &opened)
    };
    let entries = cap_fs::read_base_dir(handle).map_err(|error| plan_io(relative, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| plan_io(relative, error))?;
        let name = entry.file_name();
        if skip(&name) {
            continue;
        }
        validate_component(&name)?;
        let (kind, identity) = list_entry(handle, &name, relative)?;
        if !visit(&name, kind, &identity) {
            return Err(DeletionPlanError::Cancelled);
        }
    }
    let after = snapshot_from_open_file(handle, PlannedKind::Directory)
        .map_err(|error| plan_io(relative, error))?;
    if after != before {
        return Err(DeletionPlanError::Changed);
    }
    Ok(EntrySnapshot {
        identity: Some(before.identity),
        kind: NodeKind::Directory,
        apparent_bytes: before.apparent_bytes,
        allocated_bytes: before.allocated_bytes,
        modified_nanos: before.modified_nanos,
    })
}

/// The kind and identity of the entry `name` of `folder`, read from the folder's handle `parent`
/// without following a link and without opening the entry, classified as the scanner classifies
/// what it lists: a link, or a reparse point, is a link, whatever it points at. The entry's path
/// is built only for an error.
#[cfg(unix)]
fn list_entry(
    parent: &File,
    name: &OsStr,
    folder: &Path,
) -> Result<(PlannedKind, NativeIdentity), DeletionPlanError> {
    let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| plan_io(&folder.join(name), io::Error::from(error)))?;
    let (metadata, identity) =
        EntryMetadata::from_stat(&stat).map_err(|message| DeletionPlanError::Unrepresentable {
            path: safe_display_path_text(&folder.join(name)),
            message: safe_display_text(&message),
        })?;
    let kind = if metadata.is_symlink() || identity.reparse_point {
        PlannedKind::Link
    } else if metadata.is_dir() {
        PlannedKind::Directory
    } else {
        PlannedKind::File
    };
    Ok((kind, identity))
}

/// As the Unix reading, from the metadata of a no-follow stat and the identity of a handle that
/// asks for no access to the entry's data: it works for a folder this process may not open.
#[cfg(not(unix))]
fn list_entry(
    parent: &File,
    name: &OsStr,
    folder: &Path,
) -> Result<(PlannedKind, NativeIdentity), DeletionPlanError> {
    let metadata = cap_fs::stat(parent, Path::new(name), FollowSymlinks::No)
        .map_err(|error| plan_io(&folder.join(name), error))?;
    let kind = if metadata.is_symlink() {
        PlannedKind::Link
    } else if metadata.is_dir() {
        PlannedKind::Directory
    } else {
        PlannedKind::File
    };
    let snapshot = snapshot_from_cap_metadata(parent, name, &metadata, kind)
        .map_err(|error| plan_io(&folder.join(name), error))?;
    let kind = if snapshot.identity.reparse_point {
        PlannedKind::Link
    } else {
        kind
    };
    Ok((kind, snapshot.identity))
}

fn inspect_relative(
    root: &File,
    relative: &Path,
) -> Result<(PlannedSnapshot, Option<File>), DeletionPlanError> {
    let (parent, name) = open_parent(root, relative)?;
    inspect_entry(&parent, &name, relative)
}

/// [`inspect_child`], with an entry that is no longer there reported as missing.
fn inspect_entry(
    parent: &File,
    name: &OsStr,
    relative: &Path,
) -> Result<(PlannedSnapshot, Option<File>), DeletionPlanError> {
    match inspect_child(parent, name, relative) {
        Err(DeletionPlanError::Io {
            kind: io::ErrorKind::NotFound,
            ..
        }) => Err(DeletionPlanError::Missing(relative.to_path_buf())),
        result => result,
    }
}

/// Reads the entry `name` of `parent` without following a link: its snapshot, and for a folder
/// the handle that snapshot was read through.
///
/// On Unix this is one `fstatat` against the parent's handle, converted by the same checked
/// conversion the scanner records identities with, so the planner and the scanner agree on every
/// value they compare, and an entry whose `stat` holds a value that cannot be kept is refused with
/// an error instead of panicking the planner.
#[cfg(unix)]
fn inspect_child(
    parent: &File,
    name: &OsStr,
    display_path: &Path,
) -> Result<(PlannedSnapshot, Option<File>), DeletionPlanError> {
    let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| plan_io(display_path, io::Error::from(error)))?;
    inspect_stat(parent, name, display_path, &stat)
}

/// What [`inspect_child`] makes of the `stat` of the entry `name` of `parent`.
#[cfg(unix)]
fn inspect_stat(
    parent: &File,
    name: &OsStr,
    display_path: &Path,
    stat: &rustix::fs::Stat,
) -> Result<(PlannedSnapshot, Option<File>), DeletionPlanError> {
    let (metadata, identity) =
        EntryMetadata::from_stat(stat).map_err(|message| DeletionPlanError::Unrepresentable {
            path: safe_display_path_text(display_path),
            message: safe_display_text(&message),
        })?;
    if metadata.is_dir() {
        let handle = cap_fs::open_dir_nofollow(parent, Path::new(name))
            .map_err(|error| plan_io(display_path, error))?;
        let snapshot = snapshot_from_open_file(&handle, PlannedKind::Directory)
            .map_err(|error| plan_io(display_path, error))?;
        Ok((snapshot, Some(handle)))
    } else {
        let kind = if metadata.is_symlink() {
            PlannedKind::Link
        } else {
            PlannedKind::File
        };
        let snapshot = snapshot_from_entry(&metadata, identity, kind, display_path)
            .map_err(|error| plan_io(display_path, error))?;
        Ok((snapshot, None))
    }
}

#[cfg(not(unix))]
fn inspect_child(
    parent: &File,
    name: &OsStr,
    display_path: &Path,
) -> Result<(PlannedSnapshot, Option<File>), DeletionPlanError> {
    let metadata = cap_fs::stat(parent, Path::new(name), FollowSymlinks::No)
        .map_err(|error| plan_io(display_path, error))?;
    if metadata.is_dir() && !metadata.is_symlink() {
        let handle = cap_fs::open_dir_nofollow(parent, Path::new(name))
            .map_err(|error| plan_io(display_path, error))?;
        let snapshot = snapshot_from_open_file(&handle, PlannedKind::Directory)
            .map_err(|error| plan_io(display_path, error))?;
        Ok((snapshot, Some(handle)))
    } else {
        let kind = if metadata.is_symlink() {
            PlannedKind::Link
        } else {
            PlannedKind::File
        };
        let snapshot = snapshot_from_cap_metadata(parent, name, &metadata, kind)
            .map_err(|error| plan_io(display_path, error))?;
        Ok((snapshot, None))
    }
}

pub(crate) fn current_scan_root_identity(path: &Path) -> Result<NativeIdentity, DeletionPlanError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| plan_io(path, error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(DeletionPlanError::Changed);
    }
    let identity = identity_for(path, &metadata)
        .map_err(|error| plan_io(path, error))?
        .ok_or(DeletionPlanError::Changed)?;
    if identity.reparse_point {
        return Err(DeletionPlanError::Changed);
    }
    Ok(identity)
}

pub(crate) fn validate_scan_root_identity(
    path: &Path,
    expected: &NativeIdentity,
) -> Result<(), DeletionPlanError> {
    if !same_object(expected, &current_scan_root_identity(path)?) {
        return Err(DeletionPlanError::Changed);
    }
    Ok(())
}

fn open_root(path: &Path, expected: &NativeIdentity) -> Result<File, DeletionPlanError> {
    validate_scan_root_identity(path, expected)?;
    let root = cap_fs::open_ambient_dir(path, ambient_authority())
        .map_err(|error| plan_io(path, error))?;
    let handle_snapshot = snapshot_from_open_file(&root, PlannedKind::Directory)
        .map_err(|error| plan_io(path, error))?;
    if !same_object(expected, &handle_snapshot.identity) {
        return Err(DeletionPlanError::Changed);
    }
    validate_scan_root_identity(path, expected)?;
    Ok(root)
}

fn validate_model_snapshot(
    target: &FileToDelete,
    actual: &PlannedSnapshot,
) -> Result<(), DeletionPlanError> {
    let expected_kind = match target.expected_snapshot.kind {
        NodeKind::Directory => PlannedKind::Directory,
        NodeKind::File => PlannedKind::File,
        NodeKind::Link => PlannedKind::Link,
        NodeKind::Root | NodeKind::Synthetic(_) => return Err(DeletionPlanError::Synthetic),
    };
    if expected_kind != actual.kind
        || target.expected_snapshot.apparent_bytes != actual.apparent_bytes
        || target.expected_snapshot.modified_nanos != actual.modified_nanos
        || target
            .expected_snapshot
            .identity
            .as_ref()
            .is_some_and(|identity| identity != &actual.identity)
    {
        return Err(DeletionPlanError::Changed);
    }
    Ok(())
}

fn open_parent(root: &File, relative: &Path) -> Result<(File, OsString), DeletionPlanError> {
    let components = validated_components(relative)?;
    let Some((name, parents)) = components.split_last() else {
        return Err(DeletionPlanError::Root);
    };
    let mut parent = root.try_clone().map_err(|error| plan_io(relative, error))?;
    for component in parents {
        parent = cap_fs::open_dir_nofollow(&parent, Path::new(component))
            .map_err(|error| plan_io(relative, error))?;
    }
    Ok((parent, name.clone()))
}

/// Whether `name`, an entry of the folder `parent` that lies at `relative` below `scan_root`, is
/// the root of a mount: a mount point, a bind mount, or the root of a file system.
///
/// On Unix the answer comes from the parent's handle and the entry's own name, so no step names a
/// whole path and the length of the path does not matter. Windows has no such call and checks the
/// path.
#[cfg(unix)]
fn entry_is_mount_root(
    _scan_root: &Path,
    _relative: &Path,
    parent: &File,
    name: &OsStr,
) -> io::Result<bool> {
    mount_root_in(parent, name)
}

#[cfg(not(unix))]
fn entry_is_mount_root(
    scan_root: &Path,
    relative: &Path,
    _parent: &File,
    _name: &OsStr,
) -> io::Result<bool> {
    is_mount_root(&scan_root.join(relative))
}

/// The mount-root check by path: Windows planning's, and the Unix tests', which ask about paths
/// they can name.
#[cfg(any(not(unix), test))]
fn is_mount_root(path: &Path) -> io::Result<bool> {
    let canonical = std::fs::canonicalize(path)?;
    #[cfg(unix)]
    {
        let (Some(parent), Some(name)) = (canonical.parent(), canonical.file_name()) else {
            // `/` has neither a parent nor a name: it is the root of the file system.
            return Ok(true);
        };
        mount_root_in(&File::open(parent)?, name)
    }
    #[cfg(not(unix))]
    {
        if canonical.parent().is_none_or(|parent| parent == canonical) {
            return Ok(true);
        }
        let disks = Disks::new_with_refreshed_list_specifics(DiskRefreshKind::nothing());
        Ok(disks.list().iter().any(|disk| {
            std::fs::canonicalize(disk.mount_point()).is_ok_and(|mount| mount == canonical)
        }))
    }
}

/// Whether the entry `name` of the folder `parent` is the root of a mount, from the handle of
/// `parent` and the name alone.
///
/// Elsewhere than Linux, an entry on another device than its parent is a mount root. On Linux the
/// kernel is asked: see [`linux_mount_root`].
#[cfg(unix)]
fn mount_root_in(parent: &File, name: &OsStr) -> io::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        linux_mount_root(parent, name)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let entry = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
        let folder = rustix::fs::fstat(parent)?;
        Ok(entry.st_dev != folder.st_dev)
    }
}

/// Recognises Linux bind mounts as well as mounts that change device ID.
///
/// `STATX_ATTR_MOUNT_ROOT` is a kernel-supplied mount-root answer where available, and `statx` of
/// the entry relative to its parent's handle, without following a link, reports it for the root of
/// any mount. Older kernels omit it, so `/proc/self/mountinfo` remains a fail-closed fallback.
#[cfg(target_os = "linux")]
fn linux_mount_root(parent: &File, name: &OsStr) -> io::Result<bool> {
    match rustix::fs::statx(
        parent,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
        rustix::fs::StatxFlags::empty(),
    ) {
        Ok(status)
            if status
                .stx_attributes_mask
                .contains(rustix::fs::StatxAttributes::MOUNT_ROOT) =>
        {
            Ok(status
                .stx_attributes
                .contains(rustix::fs::StatxAttributes::MOUNT_ROOT))
        }
        Ok(_) | Err(rustix::io::Errno::NOSYS) => linux_mountinfo_contains_entry(parent, name),
        Err(error) => Err(error.into()),
    }
}

/// The fallback of [`linux_mount_root`]: `/proc/self/mountinfo` names mount points by path, so the
/// entry's path is its parent's, as the kernel names it for the open handle, followed by its name.
///
/// A parent whose path the kernel cannot name (it is longer than the kernel will print, it was
/// deleted, or it lies outside the process's root) cannot be checked this way and is refused: such
/// an entry is never assumed to be no mount. The errors are not `NotFound`, which the planner reads
/// as an entry that vanished, and so as no mount root.
#[cfg(target_os = "linux")]
fn linux_mountinfo_contains_entry(parent: &File, name: &OsStr) -> io::Result<bool> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let folder =
        std::fs::read_link(format!("/proc/self/fd/{}", parent.as_raw_fd())).map_err(|error| {
            io::Error::other(format!(
                "the path of the parent folder is unavailable: {error}"
            ))
        })?;
    if !folder.is_absolute() || folder.as_os_str().as_bytes().ends_with(b" (deleted)") {
        return Err(io::Error::other(
            "the parent folder has no path that the mount table could name",
        ));
    }
    linux_mountinfo_contains(&folder.join(name))
}

#[cfg(target_os = "linux")]
fn linux_mountinfo_contains(path: &Path) -> io::Result<bool> {
    use std::os::unix::ffi::OsStrExt as _;

    let mountinfo = std::fs::read("/proc/self/mountinfo")
        .map_err(|error| io::Error::other(format!("the mount table is unreadable: {error}")))?;
    for line in mountinfo.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Some(mountpoint) = line.split(|byte| *byte == b' ').nth(4) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed Linux mountinfo entry",
            ));
        };
        if decode_linux_mountinfo_path(mountpoint)? == path.as_os_str().as_bytes() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(any(target_os = "linux", test))]
fn decode_linux_mountinfo_path(encoded: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        let byte = encoded[index];
        if byte != b'\\' {
            decoded.push(byte);
            index += 1;
            continue;
        }
        let Some(escape) = encoded.get(index + 1..index + 4) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated Linux mountinfo escape",
            ));
        };
        if !escape.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Linux mountinfo escape",
            ));
        }
        let value = (escape[0] - b'0') * 64 + (escape[1] - b'0') * 8 + (escape[2] - b'0');
        if value == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Linux mountinfo path contains a NUL escape",
            ));
        }
        decoded.push(value);
        index += 4;
    }
    Ok(decoded)
}

fn relative_target(target: &FileToDelete) -> Result<PathBuf, DeletionPlanError> {
    let relative = target.path_to_file.iter().collect::<PathBuf>();
    validated_components(&relative)?;
    if relative.as_os_str().is_empty() {
        Err(DeletionPlanError::Root)
    } else {
        Ok(relative)
    }
}

/// The selected target's parent is structurally outside that target. It is used
/// only on Windows, where an anonymous temporary file is not available.
fn deletion_spill_directory(target: &Path) -> Result<&Path, DeletionPlanError> {
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let Some(parent) = parent else {
        return Err(DeletionPlanError::Root);
    };
    if parent.starts_with(target) {
        return Err(DeletionPlanError::Root);
    }
    Ok(parent)
}

fn validate_entry_for_target(entry: &Path, target: &Path) -> Result<(), DeletionPlanError> {
    let mut target_components = target.components();
    let mut entry_components = entry.components();
    let mut has_target_component = false;
    while let Some(target_component) = next_relative_component(&mut target_components)? {
        has_target_component = true;
        let Some(entry_component) = next_relative_component(&mut entry_components)? else {
            return Err(DeletionPlanError::InvalidRelativePath);
        };
        if entry_component != target_component {
            return Err(DeletionPlanError::InvalidRelativePath);
        }
    }
    if !has_target_component {
        return Err(DeletionPlanError::Root);
    }
    while next_relative_component(&mut entry_components)?.is_some() {}
    Ok(())
}

fn next_relative_component<'a>(
    components: &mut std::path::Components<'a>,
) -> Result<Option<&'a OsStr>, DeletionPlanError> {
    loop {
        match components.next() {
            Some(Component::Normal(component)) => {
                validate_component(component)?;
                return Ok(Some(component));
            }
            Some(Component::CurDir) => {}
            Some(Component::ParentDir | Component::RootDir | Component::Prefix(_)) => {
                return Err(DeletionPlanError::InvalidRelativePath);
            }
            None => return Ok(None),
        }
    }
}

fn validated_components(path: &Path) -> Result<Vec<OsString>, DeletionPlanError> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => components.push(component.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(DeletionPlanError::InvalidRelativePath);
            }
        }
    }
    if components.is_empty() {
        return Err(DeletionPlanError::Root);
    }
    Ok(components)
}

fn validate_component(name: &OsStr) -> Result<(), DeletionPlanError> {
    if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        Err(DeletionPlanError::InvalidRelativePath)
    } else {
        Ok(())
    }
}

/// Builds a confirmation challenge from the displayed identity before background
/// planning starts. Planning still revalidates that identity before mutation.
pub(crate) fn confirmation_challenge_for_target(
    target: &FileToDelete,
    reduced_guardrails: bool,
) -> Result<ConfirmationChallenge, DeletionPlanError> {
    let identity = target
        .expected_snapshot
        .identity
        .as_ref()
        .ok_or(DeletionPlanError::Changed)?;
    Ok(challenge_for_identity(
        target,
        &identity.file_id,
        reduced_guardrails,
    ))
}

fn challenge_for(
    target: &FileToDelete,
    snapshot: &PlannedSnapshot,
    reduced_guardrails: bool,
) -> ConfirmationChallenge {
    challenge_for_identity(target, &snapshot.identity.file_id, reduced_guardrails)
}

fn challenge_for_identity(
    target: &FileToDelete,
    file_id: &FileId,
    reduced_guardrails: bool,
) -> ConfirmationChallenge {
    let name = target.path_to_file.last().map_or_else(
        || safe_display_os_str(OsStr::new("")),
        |name| safe_display_os_str(name),
    );
    if name.deceptive {
        return ConfirmationChallenge::TypePhrase(format!("DELETE {}", challenge_code(file_id)));
    }
    if reduced_guardrails {
        return ConfirmationChallenge::ReducedGuard;
    }
    ConfirmationChallenge::ConfirmFile
}

fn challenge_code(file_id: &FileId) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let bytes = serde_json::to_vec(file_id).unwrap_or_default();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (0..4)
        .map(|shift| ALPHABET[((hash >> (shift * 5)) & 31) as usize] as char)
        .collect()
}

fn matches_for_execution(expected: &PlannedSnapshot, actual: &PlannedSnapshot) -> bool {
    same_object(&expected.identity, &actual.identity)
        && expected.kind == actual.kind
        && (expected.kind == PlannedKind::Directory
            || (expected.apparent_bytes == actual.apparent_bytes
                && expected.allocated_bytes == actual.allocated_bytes
                && expected.modified_nanos == actual.modified_nanos))
}

pub(crate) fn same_object(expected: &NativeIdentity, actual: &NativeIdentity) -> bool {
    expected.file_id == actual.file_id && expected.reparse_point == actual.reparse_point
}

fn note_deleted_link(entry: &mut PlannedEntry) {
    if matches!(entry.snapshot.kind, PlannedKind::File | PlannedKind::Link)
        && entry
            .snapshot
            .identity
            .link_count
            .is_some_and(|count| count > 1)
    {
        // A retained hard link may be outside the reviewed deletion target. Only
        // a final live link proves that its allocation was released, so avoid an
        // unbounded identity-count map and retain no speculative link count.
        entry.snapshot.identity.link_count = None;
    }
}

/// Whether an entry the run did not leave unattempted may have had other links when it left the
/// tree: a file or link whose last link the executor did not see go with its removal
/// (`no_link_survived`), whatever its outcome says. A removal can be followed by a failure (the
/// placeholder that held the entry's name is checked and removed once the entry is gone), and an
/// entry that failed, changed, or was already gone when the executor reached it can have been
/// taken out of its folder by another process, so that nothing says where its object is by then,
/// or under what other names. Only proof that no link survived says otherwise, so an entry the
/// executor could not count, or could not reference, counts too. A folder has no other links to
/// speak of.
const fn removed_with_other_links(
    kind: PlannedKind,
    outcome: &DeletionEntryOutcome,
    no_link_survived: bool,
) -> bool {
    matches!(kind, PlannedKind::File | PlannedKind::Link)
        && !matches!(outcome, DeletionEntryOutcome::Unattempted)
        && !no_link_survived
}

/// How many links the file behind `handle` has now: zero once the removal that the handle made
/// has taken its last one. `None` when the file system does not say.
#[cfg(windows)]
fn link_count_of_open_file(handle: &File) -> Option<u64> {
    use cap_primitives::fs::_WindowsByHandle as _;

    cap_fs::Metadata::from_file(handle)
        .ok()?
        .number_of_links()
        .map(u64::from)
}

#[cfg(unix)]
fn modified_nanos(metadata: &std::fs::Metadata) -> Option<u128> {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn snapshot_from_std_metadata(
    metadata: &std::fs::Metadata,
    kind: PlannedKind,
) -> io::Result<PlannedSnapshot> {
    use std::os::unix::fs::MetadataExt as _;

    Ok(PlannedSnapshot {
        identity: NativeIdentity {
            file_id: FileId::new_inode(metadata.dev(), metadata.ino()),
            link_count: Some(metadata.nlink()),
            reparse_point: metadata.file_type().is_symlink(),
        },
        kind,
        apparent_bytes: if kind == PlannedKind::Directory {
            0
        } else {
            u128::from(metadata.len())
        },
        allocated_bytes: matches!(kind, PlannedKind::File | PlannedKind::Link)
            .then(|| u128::from(metadata.blocks()).saturating_mul(512)),
        modified_nanos: modified_nanos(metadata),
    })
}

#[cfg(windows)]
fn snapshot_from_open_file(handle: &File, kind: PlannedKind) -> io::Result<PlannedSnapshot> {
    use cap_primitives::fs::_WindowsByHandle as _;

    let metadata = cap_fs::Metadata::from_file(handle)?;
    let volume = metadata
        .volume_serial_number()
        .ok_or_else(|| io::Error::other("file handle did not expose a volume serial number"))?;
    let index = metadata
        .file_index()
        .ok_or_else(|| io::Error::other("file handle did not expose a file index"))?;
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.into_std().duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok(PlannedSnapshot {
        identity: NativeIdentity {
            file_id: FileId::new_low_res(volume, index),
            link_count: metadata.number_of_links().map(u64::from),
            reparse_point: metadata.file_attributes() & 0x0000_0400 != 0,
        },
        kind,
        apparent_bytes: if kind == PlannedKind::Directory {
            0
        } else {
            u128::from(metadata.len())
        },
        allocated_bytes: (kind != PlannedKind::Directory)
            .then(|| physical_size_from_handle(handle))
            .transpose()?
            .map(u128::from),
        modified_nanos,
    })
}

#[cfg(not(windows))]
fn snapshot_from_open_file(handle: &File, kind: PlannedKind) -> io::Result<PlannedSnapshot> {
    snapshot_from_std_metadata(&handle.metadata()?, kind)
}

#[cfg(not(any(unix, windows)))]
fn snapshot_from_std_metadata(
    _metadata: &std::fs::Metadata,
    _kind: PlannedKind,
) -> io::Result<PlannedSnapshot> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "permanent deletion is unavailable on this target",
    ))
}

/// The snapshot of an entry that is not a folder, from the facts and identity the checked
/// conversion read.
#[cfg(unix)]
fn snapshot_from_entry(
    metadata: &EntryMetadata,
    identity: NativeIdentity,
    kind: PlannedKind,
    path: &Path,
) -> io::Result<PlannedSnapshot> {
    let modified_nanos = metadata
        .modified()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok(PlannedSnapshot {
        identity,
        kind,
        apparent_bytes: u128::from(metadata.len()),
        allocated_bytes: matches!(kind, PlannedKind::File | PlannedKind::Link)
            .then(|| metadata.physical_size(path))
            .transpose()?
            .map(u128::from),
        modified_nanos,
    })
}

#[cfg(windows)]
fn snapshot_from_cap_metadata(
    parent: &File,
    name: &OsStr,
    _metadata: &cap_fs::Metadata,
    kind: PlannedKind,
) -> io::Result<PlannedSnapshot> {
    use cap_primitives::fs::OpenOptionsExt as _;

    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut options = cap_fs::OpenOptions::new();
    options
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        ._cap_fs_ext_follow(FollowSymlinks::No);
    let handle = cap_fs::open(parent, Path::new(name), &options)?;
    snapshot_from_open_file(&handle, kind)
}

#[cfg(not(any(unix, windows)))]
fn snapshot_from_cap_metadata(
    _parent: &File,
    _name: &OsStr,
    _metadata: &cap_fs::Metadata,
    _kind: PlannedKind,
) -> io::Result<PlannedSnapshot> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "permanent deletion is unavailable on this target",
    ))
}

#[allow(clippy::needless_pass_by_value)]
fn plan_io(path: &Path, error: io::Error) -> DeletionPlanError {
    DeletionPlanError::Io {
        path: safe_display_path_text(path),
        message: safe_display_text(&error.to_string()),
        kind: error.kind(),
    }
}
struct PlanningStorage<'a> {
    entries: &'a mut PlanEntries,
    result_storage: &'a mut PlannedResultStorage,
    temporary_storage: &'a TemporaryStorage,
    spill_directory: &'a Path,
}

impl PlanningStorage<'_> {
    fn push(
        &mut self,
        entry: PlannedEntry,
        estimated_bytes: &mut usize,
        maximum_bytes: usize,
        allow_spill: bool,
        context: &Path,
    ) -> Result<(), DeletionPlanError> {
        let required =
            planned_entry_resident_bytes(&entry).saturating_add(result_entry_bound(&entry));
        let next = estimated_bytes.saturating_add(required);
        if next > maximum_bytes && !allow_spill {
            return Err(DeletionPlanError::MemoryLimit {
                limit: maximum_bytes,
            });
        }
        let spill = allow_spill && next > maximum_bytes;
        self.result_storage
            .reserve_for(&entry, spill, self.temporary_storage, self.spill_directory)
            .map_err(|error| plan_io(context, error))?;
        self.entries
            .push(entry, spill, self.temporary_storage, self.spill_directory)
            .map_err(|error| plan_io(context, error))?;
        // A plan that spills moves every entry it holds into a file, and its report follows it
        // there, so from then on it holds none in memory and is charged nothing. What the history
        // is charged for is what a report holds in memory, which the budget bounds. Counting the
        // entries on disk as well would charge a large folder's report more than the place the
        // history kept for it, and the report of a deletion that ran would not fit.
        *estimated_bytes = if self.entries.is_spilled() { 0 } else { next };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::temporary_storage::TemporaryStorage;
    use std::ffi::OsString;
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::state::tiles::FileType;

    fn reviewed_snapshot(path: &Path, metadata: &std::fs::Metadata) -> PlannedSnapshot {
        let identity = crate::native_path::identity_for(path, metadata)
            .expect("fixture identity lookup should succeed")
            .expect("fixture identity should be readable");
        let kind = if metadata.file_type().is_symlink() || identity.reparse_point {
            PlannedKind::Link
        } else if metadata.is_dir() {
            PlannedKind::Directory
        } else {
            PlannedKind::File
        };
        PlannedSnapshot {
            identity,
            kind,
            apparent_bytes: if kind == PlannedKind::Directory {
                0
            } else {
                u128::from(metadata.len())
            },
            allocated_bytes: matches!(kind, PlannedKind::File | PlannedKind::Link)
                .then(|| {
                    crate::os::physical_size(path, metadata)
                        .ok()
                        .map(u128::from)
                })
                .flatten(),
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos()),
        }
    }

    fn reviewed_entries(root: &Path, target: &Path) -> Vec<ReviewedEntry> {
        let mut entries = Vec::new();
        let mut stack = vec![target.to_path_buf()];
        while let Some(path) = stack.pop() {
            let metadata =
                std::fs::symlink_metadata(&path).expect("fixture metadata should be readable");
            let snapshot = reviewed_snapshot(&path, &metadata);
            if snapshot.kind == PlannedKind::Directory {
                for child in std::fs::read_dir(&path).expect("fixture directory should be readable")
                {
                    stack.push(child.expect("fixture entry should be readable").path());
                }
            }
            entries.push(ReviewedEntry {
                relative_path: path
                    .strip_prefix(root)
                    .expect("fixture should be below root")
                    .to_path_buf(),
                snapshot,
            });
        }
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        entries
    }

    fn target(root: &Path, name: OsString, file_type: FileType) -> FileToDelete {
        let path = root.join(&name);
        let reviewed_entries = reviewed_entries(root, &path);
        let snapshot = reviewed_entries
            .iter()
            .find(|entry| entry.relative_path == Path::new(&name))
            .expect("target should be reviewed")
            .snapshot
            .clone();
        let kind = match snapshot.kind {
            PlannedKind::Directory => NodeKind::Directory,
            PlannedKind::File => NodeKind::File,
            PlannedKind::Link => NodeKind::Link,
        };
        FileToDelete {
            node_id: NodeId(1),
            synthetic: false,
            path_in_filesystem: root.to_path_buf(),
            path_to_file: vec![name],
            file_type,
            num_descendants: (kind == NodeKind::Directory).then(|| {
                u64::try_from(reviewed_entries.len().saturating_sub(1)).unwrap_or(u64::MAX)
            }),
            size: 0,
            expected_snapshot: crate::model::EntrySnapshot {
                identity: Some(snapshot.identity.clone()),
                kind,
                apparent_bytes: snapshot.apparent_bytes,
                allocated_bytes: snapshot.allocated_bytes,
                modified_nanos: snapshot.modified_nanos,
            },
            reviewed_entries,
        }
    }

    /// A target for the entry at `relative` below `root`, whose snapshot is read from `metadata`,
    /// with nothing reviewed below it. Unlike [`target`] it does not walk the entry, so it can name
    /// an entry whose path no call accepts, and a folder that holds a mount.
    #[cfg(unix)]
    fn lone_target(root: &Path, relative: &Path, metadata: &std::fs::Metadata) -> FileToDelete {
        let snapshot = reviewed_snapshot(relative, metadata);
        let (kind, file_type) = match snapshot.kind {
            PlannedKind::Directory => (NodeKind::Directory, FileType::Folder),
            PlannedKind::File => (NodeKind::File, FileType::File),
            PlannedKind::Link => (NodeKind::Link, FileType::File),
        };
        FileToDelete {
            node_id: NodeId(1),
            synthetic: false,
            path_in_filesystem: root.to_path_buf(),
            path_to_file: relative.iter().map(OsStr::to_os_string).collect(),
            file_type,
            num_descendants: None,
            size: 0,
            expected_snapshot: crate::model::EntrySnapshot {
                identity: Some(snapshot.identity),
                kind,
                apparent_bytes: snapshot.apparent_bytes,
                allocated_bytes: snapshot.allocated_bytes,
                modified_nanos: snapshot.modified_nanos,
            },
            reviewed_entries: Vec::new(),
        }
    }

    #[test]
    fn plan_io_display_escapes_hostile_path_and_message() {
        let path = Path::new("plan-\u{1b}[31m-\u{202e}name");
        let error = plan_io(path, io::Error::other("metadata failed\t\u{202e}"));
        let rendered = error.to_string();

        assert!(rendered.contains("[deceptive]"));
        assert!(rendered.contains("\\x1b"));
        assert!(rendered.contains("\\u{202e}"));
        assert!(rendered.contains("\\t"));
        assert!(!rendered.chars().any(char::is_control));
        assert!(!rendered.contains('\u{202e}'));
    }

    #[test]
    fn missing_parent_is_stale_but_missing_target_is_explicit() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let parent = root.path().join("parent");
        std::fs::create_dir(&parent).expect("parent should be created");
        let nested = parent.join("target");
        std::fs::write(&nested, b"payload").expect("nested target should be written");

        let reviewed = target(root.path(), OsString::from("parent/target"), FileType::File);
        std::fs::remove_dir_all(&parent).expect("parent should be removed");
        let parent_error = build_plan(root.path(), reviewed, false)
            .expect_err("missing parent should reject the stale namespace");
        assert!(parent_error.is_stale());
        assert!(!parent_error.is_missing());
        assert!(matches!(
            parent_error,
            DeletionPlanError::Io {
                kind: io::ErrorKind::NotFound,
                ..
            }
        ));

        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let reviewed = target(root.path(), OsString::from("target"), FileType::File);
        std::fs::remove_file(&path).expect("target should be removed");
        let target_error = build_plan(root.path(), reviewed, false)
            .expect_err("missing target should be reported explicitly");
        assert!(target_error.is_missing());
        assert!(!target_error.is_stale());
        assert!(matches!(
            target_error,
            DeletionPlanError::Missing(path) if path == Path::new("target")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn hard_link_created_after_identity_inspection_is_not_reclaimable() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        let late_link = root.path().join("late-link");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");

        let report = execute_plan_unix_with_hooks(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            || {},
            |detached| {
                std::fs::hard_link(root.path().join(detached), &late_link)
                    .expect("late hard link should be created");
            },
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(report.deleted_allocated_bytes(), 0);
        assert!(!path.exists());
        assert!(late_link.exists());
    }

    #[test]
    fn unchanged_file_plan_deletes_exact_identity() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        assert_eq!(plan.planned_entries(), 1);
        assert_eq!(plan.challenge, ConfirmationChallenge::ConfirmFile);

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert!(report.precise);
        assert!(!path.exists());
    }
    #[test]
    fn synthetic_and_scan_root_targets_are_rejected() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");

        let mut synthetic = target(root.path(), OsString::from("target"), FileType::File);
        synthetic.synthetic = true;
        assert!(matches!(
            build_plan(root.path(), synthetic, false),
            Err(DeletionPlanError::Synthetic)
        ));

        let mut scan_root = target(root.path(), OsString::from("target"), FileType::File);
        scan_root.path_to_file.clear();
        assert!(matches!(
            build_plan(root.path(), scan_root, false),
            Err(DeletionPlanError::Root)
        ));
        assert!(path.exists());
    }

    #[test]
    fn replaced_file_is_skipped() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"original").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        std::fs::rename(&path, root.path().join("original"))
            .expect("original identity should remain allocated");
        std::fs::write(&path, b"replacement").expect("replacement should be written");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.changed_entries(), 1);
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn replacement_after_isolation_is_never_deleted() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"reviewed").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        let displaced_placeholder = root.path().join("displaced-placeholder");

        let report = execute_plan_unix_with_hook(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            || {
                std::fs::rename(&path, &displaced_placeholder)
                    .expect("placeholder should be displaced");
                std::fs::write(&path, b"replacement")
                    .expect("replacement should occupy the original name");
            },
        );

        assert_eq!(
            std::fs::read(&path).expect("replacement should survive"),
            b"replacement"
        );
        assert_eq!(report.failed_entries(), 1);
        assert!(report.precise);
    }

    #[cfg(unix)]
    #[test]
    fn replacement_after_final_isolation_check_is_never_deleted() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        let displaced = root.path().join("reviewed");
        std::fs::write(&path, b"reviewed").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");

        let report = execute_plan_unix_with_hooks(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            || {},
            |detached| {
                let detached = root.path().join(detached);
                std::fs::rename(&detached, &displaced)
                    .expect("reviewed target should be displaced after isolation");
                std::fs::write(&detached, b"replacement")
                    .expect("replacement should occupy the detached name");
            },
        );

        assert_eq!(
            std::fs::read(&path).expect("replacement should return to its original name"),
            b"replacement"
        );
        assert_eq!(
            std::fs::read(&displaced).expect("reviewed target should remain available"),
            b"reviewed"
        );
        assert_eq!(report.changed_entries(), 1);
        assert_eq!(report.deleted_entries(), 0);
    }

    #[test]
    fn new_directory_child_is_never_swept() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let planned_child = directory.join("planned");
        std::fs::write(&planned_child, b"planned").expect("planned child should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::Folder),
            false,
        )
        .expect("directory plan should build");
        let new_child = directory.join("new");
        std::fs::write(&new_child, b"new").expect("new child should be written");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(report.changed_entries(), 1);
        assert!(!planned_child.exists());
        assert!(new_child.exists());
        assert!(directory.exists());
    }

    /// While an entry is isolated, the name that holds it is a name the executor made in the
    /// folder that holds the entry. It is registered by its exact path, the one a scan builds for
    /// the same name, so a scan of the folder records no entry of the executor's and fails to read
    /// none; and the registration is gone once the entry is.
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn the_name_that_holds_an_isolated_entry_is_registered_by_its_exact_path() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let folder = root.path().join("folder");
        std::fs::create_dir(&folder).expect("folder should be created");
        let path = folder.join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        std::fs::write(folder.join("other"), b"stay").expect("other should be written");
        let metadata = std::fs::symlink_metadata(&path).expect("target metadata");
        let storage = TemporaryStorage::default();
        let plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            lone_target(root.path(), Path::new("folder/target"), &metadata),
            false,
            &AtomicBool::new(false),
            1 << 20,
            &storage,
        )
        .expect("plan should build");

        let mut seen = None;
        let report = execute_plan_with_hook_after_inspection(root.path(), plan, |name| {
            let registered = folder.join(name);
            let (recorded, failed) = crate::runtime::recorded_by_a_scan_of(
                root.path(),
                &folder,
                storage.private_files(),
            );
            seen = Some((
                registered.clone(),
                storage.private_files().contains(&registered),
                recorded,
                failed,
            ));
        });

        assert_eq!(report.deleted_entries(), 1);
        assert!(!path.exists());
        let (registered, was_registered, recorded, failed) = seen.expect("the hook should run");
        assert!(
            was_registered,
            "the isolated entry's name is registered: {registered:?}"
        );
        assert!(
            recorded.iter().all(|path| !path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".excise-delete-"))),
            "a scan records none of the executor's names: {recorded:?}"
        );
        assert!(recorded.contains(&folder.join("other")));
        assert!(failed.is_empty(), "and fails to read none: {failed:?}");
        assert!(
            !storage.private_files().contains(&registered),
            "the registration ends with the entry's isolation"
        );
    }

    /// The registration is released however the entry's isolation ends, not only when it is
    /// removed: here the entry changed, so it is put back.
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn the_registration_of_an_isolated_name_ends_when_the_entry_is_restored() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let storage = TemporaryStorage::default();
        let plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
            &AtomicBool::new(false),
            1 << 20,
            &storage,
        )
        .expect("plan should build");

        let mut registered = None;
        let report = execute_plan_with_hook_after_inspection(root.path(), plan, |name| {
            registered = Some(root.path().join(name));
            // The entry changes after the executor looked at it.
            std::fs::write(root.path().join(name), b"changed afterwards")
                .expect("the isolated entry should change");
        });

        assert_eq!(report.deleted_entries(), 0);
        assert!(path.exists(), "the entry is put back");
        let registered = registered.expect("the hook should run");
        assert!(!storage.private_files().contains(&registered));
        assert!(!registered.exists());
    }

    #[test]
    fn soft_cancel_leaves_every_entry_unattempted() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(true),
            &AtomicBool::new(false),
        );

        assert!(report.soft_cancelled);
        assert_eq!(report.unattempted_entries(), 1);
        assert!(path.exists());
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn a_placeholder_is_removed_even_when_its_detached_name_is_taken() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let parent = std::fs::File::open(root.path()).expect("the root should open");
        let files = PrivateFiles::default();
        let names = TransientNames::new(root.path(), Path::new("target"), &files);
        let (detached, placeholder, _registered) =
            create_placeholder(&parent, &names).expect("a placeholder should be reserved");
        // After a deletion the placeholder holds the entry's original name and moves back to the
        // detached name, which the target has just vacated. Here another entry holds that name.
        std::fs::rename(root.path().join(&detached), root.path().join("target"))
            .expect("the placeholder should take the original name");
        std::fs::write(root.path().join(&detached), b"not ours")
            .expect("another entry should take the detached name");

        finalize_placeholder(
            &parent,
            &names,
            std::ffi::OsStr::new("target"),
            &detached,
            &placeholder,
        )
        .expect("the placeholder should be removed under a fresh private name");

        let names: Vec<OsString> = std::fs::read_dir(root.path())
            .expect("the root should list")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(names, std::slice::from_ref(&detached));
        assert_eq!(
            std::fs::read(root.path().join(&detached)).expect("the other entry should remain"),
            b"not ours"
        );
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
    #[test]
    fn counted_execution_stops_when_the_mutation_gate_rejects_the_first_entry() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        let progress = std::sync::atomic::AtomicU64::new(0);
        let soft_cancelled = AtomicBool::new(false);
        let gate_attempted = AtomicBool::new(false);

        let report = execute_plan_counted(
            root.path(),
            plan,
            &soft_cancelled,
            &AtomicBool::new(false),
            &progress,
            || {
                gate_attempted.store(true, std::sync::atomic::Ordering::Release);
                false
            },
            || panic!("a rejected mutation gate must not publish mutation"),
        );

        assert!(gate_attempted.load(std::sync::atomic::Ordering::Acquire));
        assert!(soft_cancelled.load(std::sync::atomic::Ordering::Acquire));
        assert!(report.soft_cancelled);
        assert_eq!(report.unattempted_entries(), 1);
        assert_eq!(progress.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(path.exists());
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
    #[test]
    fn counted_execution_stops_when_mutation_commit_is_rejected() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        let progress = std::sync::atomic::AtomicU64::new(0);
        let soft_cancelled = AtomicBool::new(false);
        let commit_attempted = AtomicBool::new(false);

        let report = execute_plan_counted(
            root.path(),
            plan,
            &soft_cancelled,
            &AtomicBool::new(false),
            &progress,
            || true,
            || {
                commit_attempted.store(true, std::sync::atomic::Ordering::Release);
                false
            },
        );

        assert!(commit_attempted.load(std::sync::atomic::Ordering::Acquire));
        assert!(soft_cancelled.load(std::sync::atomic::Ordering::Acquire));
        assert!(report.soft_cancelled);
        assert_eq!(report.unattempted_entries(), 1);
        assert_eq!(progress.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn counted_windows_execution_keeps_changed_entries_before_mutation() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"original").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        std::fs::write(&path, b"changed content").expect("target should change");
        let progress = std::sync::atomic::AtomicU64::new(0);

        let report = execute_plan_counted(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            &progress,
            || panic!("a changed entry must not claim mutation"),
            || panic!("a changed entry must not publish mutation"),
        );

        assert_eq!(report.changed_entries(), 1);
        assert_eq!(progress.load(std::sync::atomic::Ordering::Acquire), 1);
    }

    #[test]
    fn changed_scan_snapshot_is_rejected_before_consent() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"original").expect("target should be written");
        let reviewed = target(root.path(), OsString::from("target"), FileType::File);
        std::fs::write(&path, b"changed content").expect("target should change");

        assert!(matches!(
            build_plan(root.path(), reviewed, false),
            Err(DeletionPlanError::Changed)
        ));
        assert!(path.exists());
    }

    #[test]
    fn stale_reviewed_subtree_is_rejected_before_consent() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let reviewed_child = directory.join("reviewed");
        std::fs::write(&reviewed_child, b"reviewed").expect("reviewed child should be written");
        let reviewed = target(root.path(), OsString::from("target"), FileType::Folder);
        let stale_child = directory.join("not-reviewed");
        std::fs::write(&stale_child, b"late").expect("late child should be written");

        assert!(matches!(
            build_plan(root.path(), reviewed, false),
            Err(DeletionPlanError::Changed)
        ));
        assert!(reviewed_child.exists());
        assert!(stale_child.exists());
    }

    #[test]
    fn file_to_directory_replacement_is_skipped() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"original").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        std::fs::rename(&path, root.path().join("original"))
            .expect("original identity should remain allocated");
        std::fs::create_dir(&path).expect("replacement directory should be created");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert_eq!(report.changed_entries(), 1);
        assert!(path.is_dir());
    }
    #[test]
    fn directory_to_file_replacement_is_skipped() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::create_dir(&path).expect("target directory should be created");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::Folder),
            false,
        )
        .expect("directory plan should build");
        std::fs::rename(&path, root.path().join("original"))
            .expect("original directory identity should remain allocated");
        std::fs::write(&path, b"replacement").expect("replacement file should be written");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert_eq!(report.changed_entries(), 1);
        assert!(path.is_file());
    }

    #[test]
    fn changed_entry_does_not_block_safe_sibling() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let changed = directory.join("changed");
        let safe = directory.join("safe");
        std::fs::write(&changed, b"original").expect("changed fixture should be written");
        std::fs::write(&safe, b"safe").expect("safe fixture should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::Folder),
            false,
        )
        .expect("directory plan should build");
        std::fs::rename(&changed, directory.join("original"))
            .expect("original identity should remain allocated");
        std::fs::write(&changed, b"replacement").expect("replacement should be written");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert!(report.deleted_entries() >= 1);
        assert!(report.changed_entries() >= 1);
        assert!(!safe.exists());
        assert!(changed.exists());
    }

    #[test]
    fn hard_cancel_marks_report_imprecise() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(true),
        );
        assert!(!report.precise);
        assert_eq!(report.unattempted_entries(), 1);
        assert!(path.exists());
    }

    #[test]
    fn deletion_plan_respects_memory_limit() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let target = target(root.path(), OsString::from("target"), FileType::File);

        assert!(matches!(
            build_plan_cancellable(root.path(), target, false, &AtomicBool::new(false), 1,),
            Err(DeletionPlanError::MemoryLimit { limit: 1 })
        ));
    }

    #[test]
    fn directory_plan_spills_before_plan_and_result_residency_exceed_limit() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let snapshot = reviewed_snapshot(
            &directory,
            &std::fs::symlink_metadata(&directory).expect("target metadata should be readable"),
        );
        let plan_only_bytes = planned_entry_resident_bytes(&PlannedEntry {
            relative_path: PathBuf::from("target"),
            snapshot,
        });
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);

        let plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &AtomicBool::new(false),
            plan_only_bytes,
            &temporary_storage,
        )
        .expect("directory plan should spill rather than retain plan and result together");

        assert!(matches!(&plan.entries, PlanEntries::Spilled(_)));
        assert!(matches!(
            &plan.result_storage,
            PlannedResultStorage::Spilled(_)
        ));
        drop(plan);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn large_directory_plan_spills_within_shared_temporary_storage() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        for index in 0..(MAX_RESIDENT_DIRECTORY_TASKS + 8) {
            let child = directory.join(format!("child-{index}"));
            std::fs::create_dir(&child).expect("child directory should be created");
            std::fs::write(child.join("file"), b"payload").expect("child file should be created");
        }
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &AtomicBool::new(false),
            1,
            &temporary_storage,
        )
        .expect("large directory plan should spill instead of hitting the resident limit");
        let planned_entries = plan.planned_entries();
        assert_eq!(
            planned_entries,
            u64::try_from(1 + 2 * (MAX_RESIDENT_DIRECTORY_TASKS + 8))
                .expect("fixture entry count should fit"),
        );
        assert!(matches!(&plan.entries, PlanEntries::Spilled(_)));
        assert!(temporary_storage.used() > 0);
        assert!(temporary_storage.used() <= 2 * 1024 * 1024);
        revalidate_plan(root.path(), &plan).expect("spilled plan should revalidate before consent");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), planned_entries);
        assert_eq!(report.unattempted_entries(), 0);
        assert_eq!(
            u64::try_from(report.entries.len()).expect("report entry count should fit"),
            planned_entries,
        );
        let reported = report
            .entries
            .iter()
            .try_fold(0_u64, |count, result| {
                result.map(|_| count.saturating_add(1))
            })
            .expect("spilled result should stream every entry");
        assert_eq!(reported, planned_entries);
        assert!(!directory.exists());
        assert!(report.entries.is_spilled());
        assert!(report.reporting_complete());
        assert!(report.target_was_removed());
        assert!(temporary_storage.used() > 0);
        drop(report);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn large_directory_plan_keeps_complete_results_with_bounded_storage() {
        const FILES: usize = 512;

        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should exist");
        for index in 0..FILES {
            std::fs::write(directory.join(format!("artifact-{index:04}")), b"payload")
                .expect("target artifact should exist");
        }
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let temporary_storage = TemporaryStorage::with_limit_bytes(1024 * 1024);

        let plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &AtomicBool::new(false),
            1,
            &temporary_storage,
        )
        .expect("large directory plan should fit its bounded complete result store");
        let planned_entries = plan.planned_entries();
        assert_eq!(
            planned_entries,
            u64::try_from(FILES + 1).expect("count should fit")
        );

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert_eq!(report.deleted_entries(), planned_entries);
        assert!(report.reporting_complete());
        assert!(!directory.exists());
        drop(report);
        assert_eq!(temporary_storage.used(), 0);
    }

    /// The history keeps a place for the report of every deletion allowed to start, as large as
    /// the budget the plan was given. A plan that spills keeps none of its entries in memory once
    /// it has, so it and the report it ends in are charged for what it held in memory and never
    /// for the entries on disk: charged for those, a large folder's report would outgrow the
    /// place kept for it and be dropped after its files were gone.
    #[test]
    fn a_plan_that_spills_charges_its_report_no_more_than_the_budget_it_was_given() {
        const FILES: usize = 64;

        for budget in [1, 4 * 1024, 16 * 1024] {
            let root = tempfile::tempdir().expect("deletion root should exist");
            let directory = root.path().join("target");
            std::fs::create_dir(&directory).expect("target directory should exist");
            for index in 0..FILES {
                std::fs::write(directory.join(format!("file-{index:03}")), b"payload")
                    .expect("target file should exist");
            }
            let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
            target.reviewed_entries.clear();
            let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);

            let plan = build_plan_cancellable_with_temporary_storage(
                root.path(),
                target,
                false,
                &AtomicBool::new(false),
                budget,
                &temporary_storage,
            )
            .expect("a directory plan spills instead of exceeding its budget");
            assert!(
                matches!(&plan.entries, PlanEntries::Spilled(_)),
                "the fixture should be too large for a budget of {budget} bytes"
            );
            assert!(
                plan.estimated_bytes <= budget,
                "a plan with a budget of {budget} bytes charged {}",
                plan.estimated_bytes
            );

            let report = execute_plan(
                root.path(),
                plan,
                &AtomicBool::new(false),
                &AtomicBool::new(false),
            );
            assert_eq!(
                report.deleted_entries(),
                u64::try_from(FILES + 1).expect("count should fit")
            );
            assert!(
                report.estimated_bytes <= budget,
                "a report whose plan had a budget of {budget} bytes charged {}",
                report.estimated_bytes
            );
        }
    }

    /// A deletion reports whether a file it removed may have had other links: the executor counts
    /// the links the removal left, and the owner scans the map again, instead of updating it in
    /// place, when it cannot show there were none. A link gained after the plan was made counts
    /// as much as one the plan saw, and so does one that is outside the folder deleted.
    #[cfg(unix)]
    #[test]
    fn a_deletion_reports_whether_a_file_it_removed_may_have_had_other_links() {
        fn deleted_with(
            folder: bool,
            budget: usize,
            before_planning: impl FnOnce(&Path),
            after_planning: impl FnOnce(&Path),
        ) -> DeletionReport {
            let root = tempfile::tempdir().expect("deletion root should exist");
            let path = root.path().join("target");
            if folder {
                std::fs::create_dir(&path).expect("target folder should exist");
                for index in 0..4 {
                    std::fs::write(path.join(format!("file-{index}")), b"payload")
                        .expect("target file should exist");
                }
            } else {
                std::fs::write(&path, b"payload").expect("target file should exist");
            }
            before_planning(root.path());
            let file_type = if folder {
                FileType::Folder
            } else {
                FileType::File
            };
            let mut target = target(root.path(), OsString::from("target"), file_type);
            if folder {
                target.reviewed_entries.clear();
            }
            let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
            let plan = build_plan_cancellable_with_temporary_storage(
                root.path(),
                target,
                false,
                &AtomicBool::new(false),
                budget,
                &temporary_storage,
            )
            .expect("the plan should build");
            after_planning(root.path());
            execute_plan(
                root.path(),
                plan,
                &AtomicBool::new(false),
                &AtomicBool::new(false),
            )
        }
        let link = |root: &Path, from: &str| {
            std::fs::hard_link(root.join(from), root.join("another-name"))
                .expect("a second link should be created");
        };

        let sole_link = deleted_with(false, DEFAULT_PLAN_LIMIT_BYTES, |_| {}, |_| {});
        assert_eq!(sole_link.deleted_entries(), 1);
        assert_eq!(
            sole_link.deleted_files_may_have_other_links(),
            !proves_no_link_survived(),
            "a file with one link"
        );

        let linked_when_planned = deleted_with(
            false,
            DEFAULT_PLAN_LIMIT_BYTES,
            |root| link(root, "target"),
            |_| {},
        );
        assert!(
            linked_when_planned.deleted_files_may_have_other_links(),
            "a file with a second link when it was planned"
        );

        let linked_after_planning = deleted_with(
            false,
            DEFAULT_PLAN_LIMIT_BYTES,
            |_| {},
            |root| link(root, "target"),
        );
        assert_eq!(linked_after_planning.deleted_entries(), 1);
        assert!(
            linked_after_planning.deleted_files_may_have_other_links(),
            "a file that gained a second link after it was planned"
        );

        let folder_of_sole_links = deleted_with(true, 1, |_| {}, |_| {});
        assert_eq!(folder_of_sole_links.deleted_entries(), 5);
        assert!(folder_of_sole_links.entries.is_spilled());
        assert_eq!(
            folder_of_sole_links.deleted_files_may_have_other_links(),
            !proves_no_link_survived(),
            "a folder of files with one link each, in a plan that spilled"
        );

        let folder_with_a_linked_file =
            deleted_with(true, 1, |root| link(root, "target/file-0"), |_| {});
        assert!(folder_with_a_linked_file.entries.is_spilled());
        assert!(
            folder_with_a_linked_file.deleted_files_may_have_other_links(),
            "a file of the folder has another link outside it, and the report was spilled"
        );
    }

    /// A link made after the executor last looked at a file, and before it removes the file, is a
    /// link that outlives the removal. The executor counts what is left once the file is gone,
    /// not what it read before.
    #[cfg(unix)]
    #[test]
    fn a_link_made_between_the_last_inspection_and_the_removal_is_counted() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        std::fs::write(root.path().join("target"), b"payload").expect("target file should exist");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("the plan should build");
        let late_link = root.path().join("late-link");

        let report = execute_plan_unix_with_hooks(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            || {},
            |detached| {
                std::fs::hard_link(root.path().join(detached), &late_link)
                    .expect("the late link should be made");
            },
        );

        assert_eq!(report.deleted_entries(), 1);
        assert!(
            report.deleted_files_may_have_other_links(),
            "a link made just before the removal outlived it"
        );
        assert!(
            late_link.exists(),
            "the link made before the removal is what survives it"
        );
    }

    /// The same on Windows: a link made after the handle was opened, and before the file is
    /// removed through it, outlives the removal. The count the handle read when it was opened
    /// does not say so.
    #[cfg(windows)]
    #[test]
    fn a_link_made_between_opening_a_file_and_removing_it_is_counted() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        std::fs::write(root.path().join("target"), b"payload").expect("target file should exist");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("the plan should build");
        let late_link = root.path().join("late-link");
        let mut made = false;

        let report = execute_plan_windows(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            None,
            || true,
            || {
                if !made {
                    std::fs::hard_link(root.path().join("target"), &late_link)
                        .expect("the late link should be made");
                    made = true;
                }
                true
            },
        );

        assert_eq!(report.deleted_entries(), 1);
        assert!(
            report.deleted_files_may_have_other_links(),
            "a link made just before the removal outlived it"
        );
        assert!(
            late_link.exists(),
            "the link made before the removal is what survives it"
        );
    }

    /// A symbolic link is referenced itself, never what it points at (`O_PATH` does not follow
    /// one, and the identity that macOS looks up is the link's own), so where the executor can
    /// prove a file left no link behind it proves it of a link.
    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_removed_and_counted_as_a_file_is() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::os::unix::fs::symlink("points-nowhere", &path).expect("the link should be created");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("the plan should build");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(
            report.deleted_files_may_have_other_links(),
            !proves_no_link_survived(),
            "a link with no other link"
        );
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "the link is gone"
        );
    }

    /// A FIFO is removed without being opened, which a plain open would block on, and a device
    /// without the side effects an open can have. A reference to it is made without opening it
    /// (Linux's `O_PATH`, macOS's lookup by identity), so the executor proves it had no other
    /// link.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_removed_without_being_opened() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        // macOS has no `mknodat`, so the system's own `mkfifo` makes the fixture.
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo should run");
        assert!(made.success(), "the FIFO should be created");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("the plan should build");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(
            report.deleted_files_may_have_other_links(),
            !proves_no_link_survived(),
            "the FIFO was referenced without being opened, so its removal is proven"
        );
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "the FIFO is gone"
        );
    }

    /// A socket cannot be opened at all, so only a reference that opens nothing can count the
    /// links of one: the executor removes it as it removes a file, and proves it had no other
    /// link.
    #[cfg(unix)]
    #[test]
    fn a_socket_is_removed_and_counted_as_a_file_is() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        // Closing the listener leaves the socket's file where it was bound.
        drop(std::os::unix::net::UnixListener::bind(&path).expect("the socket should be bound"));
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("the plan should build");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(
            report.deleted_files_may_have_other_links(),
            !proves_no_link_survived(),
            "a socket with no other link"
        );
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "the socket is gone"
        );
    }

    /// The identity of the entry at `path` as the planner reads it.
    #[cfg(target_vendor = "apple")]
    fn identity_at(path: &Path) -> NativeIdentity {
        let metadata =
            std::fs::symlink_metadata(path).expect("fixture metadata should be readable");
        crate::native_path::identity_for(path, &metadata)
            .expect("fixture identity lookup should succeed")
            .expect("fixture identity should be readable")
    }

    /// The reference to an object counts the links the object has when it is asked, not the names
    /// the executor knew: a link made after the reference was made is in the count, one that is
    /// removed is not, and none is left once the last link is removed.
    #[cfg(target_vendor = "apple")]
    #[test]
    fn a_reference_counts_the_links_an_object_has_when_it_is_asked() {
        let root = tempfile::tempdir().expect("root should exist");
        let first = root.path().join("first");
        std::fs::write(&first, b"payload").expect("file should exist");
        let parent = std::fs::File::open(root.path()).expect("folder should open");

        let reference =
            LinkReference::open(&parent, std::ffi::OsStr::new("first"), &identity_at(&first));

        assert_eq!(
            reference.is_some(),
            proves_no_link_survived(),
            "a reference exists exactly where the system resolves a file by its identity"
        );
        let Some(reference) = reference else { return };
        assert_eq!(reference.links(), Some(1));
        let second = root.path().join("second");
        std::fs::hard_link(&first, &second).expect("a second link should be made");
        assert_eq!(
            reference.links(),
            Some(2),
            "a link made after the reference is in the count"
        );
        std::fs::remove_file(&first).expect("the first link should be removed");
        assert_eq!(
            reference.links(),
            Some(1),
            "the object lives on under its other name"
        );
        std::fs::remove_file(&second).expect("the second link should be removed");
        assert_eq!(
            reference.links(),
            Some(0),
            "nothing names the object any more"
        );
    }

    /// A lookup that fails for a reason other than the object being gone proves nothing: a link
    /// that survives in a folder nobody can search is out of its reach, and is not taken for
    /// none.
    #[cfg(target_vendor = "apple")]
    #[test]
    fn a_link_the_lookup_cannot_reach_is_not_taken_for_none() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().expect("root should exist");
        let first = root.path().join("first");
        std::fs::write(&first, b"payload").expect("file should exist");
        let parent = std::fs::File::open(root.path()).expect("folder should open");
        let Some(reference) =
            LinkReference::open(&parent, std::ffi::OsStr::new("first"), &identity_at(&first))
        else {
            return;
        };
        let locked = root.path().join("locked");
        std::fs::create_dir(&locked).expect("folder should exist");
        std::fs::hard_link(&first, locked.join("survivor")).expect("a second link should be made");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("folder should lock");
        std::fs::remove_file(&first).expect("the first link should be removed");

        let links = reference.links();

        // Unlocked before anything is asserted, so that the temporary folder can be removed.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("folder should unlock");
        assert_ne!(
            links,
            Some(0),
            "the object lives on under a name the lookup cannot reach"
        );
    }

    /// A reference is made only to the object that was inspected, found by the identity it was
    /// inspected with: an identity that names no object, a device number the path cannot spell,
    /// and a file found where a link was inspected get none, so the executor counts the removal as
    /// one that may have left a link.
    #[cfg(target_vendor = "apple")]
    #[test]
    fn an_object_that_is_not_the_one_inspected_gets_no_reference() {
        let root = tempfile::tempdir().expect("root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("file should exist");
        let parent = std::fs::File::open(root.path()).expect("folder should open");
        let identity = identity_at(&path);
        let FileId::Inode {
            device_id,
            inode_number,
        } = &identity.file_id
        else {
            panic!("a Unix identity is an inode")
        };
        let gets_none = |changed: NativeIdentity| {
            LinkReference::open(&parent, std::ffi::OsStr::new("target"), &changed).is_none()
        };

        assert!(
            gets_none(NativeIdentity {
                file_id: FileId::new_inode(*device_id, u64::MAX),
                ..identity.clone()
            }),
            "an inode number that nothing has"
        );
        assert!(
            gets_none(NativeIdentity {
                file_id: FileId::new_inode(u64::MAX, *inode_number),
                ..identity.clone()
            }),
            "a device number the path cannot spell"
        );
        assert!(
            gets_none(NativeIdentity {
                reparse_point: true,
                ..identity.clone()
            }),
            "a file found where a link was inspected"
        );
    }

    /// An entry that was already gone when the executor reached it records `Missing`, though the
    /// run removed the rest of its folder: nothing says where its object is by then, or under
    /// what other names, so the map cannot be updated in place.
    #[cfg(any(unix, windows))]
    #[test]
    fn an_entry_that_vanished_before_its_removal_counts_as_possibly_linked() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let folder = root.path().join("target");
        std::fs::create_dir(&folder).expect("target folder should exist");
        std::fs::write(folder.join("kept"), b"payload").expect("kept file should exist");
        std::fs::write(folder.join("vanishes"), b"payload").expect("vanishing file should exist");
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let plan = build_plan(root.path(), target, false).expect("the plan should build");
        // Another process unlinks a file of the folder between the plan and the run.
        std::fs::remove_file(folder.join("vanishes")).expect("the file should be unlinked");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert!(report.target_was_removed());
        assert_eq!(
            report.deleted_entries(),
            2,
            "the kept file, then the folder"
        );
        assert!(
            report.deleted_files_may_have_other_links(),
            "a file that was gone when the executor reached it may live on under another name"
        );
    }

    /// The executor can fail after it has removed a file: the placeholder that held the file's
    /// name is checked and removed once the file is gone, and when that fails the entry records
    /// `Failed`, though the file is gone and the folder that held it can still be removed. The
    /// flag does not follow the outcome but what the executor knows of the file's links: a file
    /// with another link may live on whatever the cleanup did, and so may one whose last link the
    /// executor did not see go.
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn a_file_removed_and_then_failed_on_its_cleanup_still_counts_as_possibly_linked() {
        let removed_then_failed = |other_link: bool| {
            let root = tempfile::tempdir().expect("deletion root should exist");
            let folder = root.path().join("target");
            std::fs::create_dir(&folder).expect("target folder should exist");
            std::fs::write(folder.join("file"), b"payload").expect("target file should exist");
            if other_link {
                std::fs::hard_link(folder.join("file"), root.path().join("another-name"))
                    .expect("the other link should be made");
            }
            let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
            target.reviewed_entries.clear();
            let plan = build_plan(root.path(), target, false).expect("the plan should build");
            let spoiled = std::cell::Cell::new(false);

            let report = execute_plan_with_hook_after_inspection(root.path(), plan, |detached| {
                // At the file's turn its name holds the placeholder. Take it away, so that the
                // check that follows the file's removal finds nothing there.
                if !spoiled.get() && folder.join(detached).exists() {
                    std::fs::remove_file(folder.join("file"))
                        .expect("the placeholder should be removed");
                    spoiled.set(true);
                }
            });

            assert!(spoiled.get(), "the placeholder was never taken away");
            assert_eq!(report.failed_entries(), 1, "the file's cleanup failed");
            assert_eq!(report.deleted_entries(), 1, "the folder");
            assert!(report.target_was_removed(), "the folder went all the same");
            report
        };

        assert!(
            removed_then_failed(true).deleted_files_may_have_other_links(),
            "a file with another link, whose removal was followed by a failure"
        );
        assert_eq!(
            removed_then_failed(false).deleted_files_may_have_other_links(),
            !proves_no_link_survived(),
            "a file with one link: counted unless the executor proved that none survived"
        );
    }

    /// On Windows a spill is a named file in the folder that holds the target, and it is Excise's
    /// own for as long as it exists: registered, so that nothing reads it as the user's, from
    /// before it is created until after it is gone.
    #[cfg(windows)]
    #[test]
    fn a_spill_registers_its_file_for_as_long_as_the_file_exists() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let spill = RecordSpill::new(&temporary_storage, MAX_PLAN_SPILL_RECORD_BYTES, root.path())
            .expect("spill should open");
        let files: Vec<PathBuf> = std::fs::read_dir(root.path())
            .expect("the folder should list")
            .map(|entry| entry.expect("the entry should read").path())
            .collect();

        assert_eq!(files.len(), 1, "the spill is the one file in its folder");
        assert!(
            temporary_storage.private_files().contains(&files[0]),
            "the spill's file is registered while it exists"
        );

        drop(spill);

        assert!(!files[0].exists(), "the file went with its handle");
        assert!(
            !temporary_storage.private_files().contains(&files[0]),
            "the registration ended with the file"
        );
    }

    /// The spill's file and the registration of its path end as one step: the registry is locked
    /// before the file is closed, and so removed, and every lookup waits for it. A lookup that
    /// holds the registry therefore keeps the spill's file where it is, and no lookup can find
    /// the path free, for another process to take, while it is still registered as Excise's own.
    #[cfg(windows)]
    #[test]
    fn a_spill_is_not_removed_while_a_lookup_holds_the_registry() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let spill = RecordSpill::new(&temporary_storage, MAX_PLAN_SPILL_RECORD_BYTES, root.path())
            .expect("spill should open");
        let present = || {
            std::fs::read_dir(root.path())
                .expect("the folder should list")
                .count()
        };
        assert_eq!(present(), 1, "the spill is the one file in its folder");
        let lookups = temporary_storage.private_files().hold_lookups();
        let (started, running) = std::sync::mpsc::channel();
        let closer = std::thread::spawn(move || {
            started.send(()).expect("the test is waiting");
            drop(spill);
        });
        running.recv().expect("the closing thread should start");
        std::thread::sleep(std::time::Duration::from_millis(100));

        assert_eq!(
            present(),
            1,
            "the spill's file was removed while a lookup held the registry"
        );

        drop(lookups);
        closer.join().expect("the closing thread should end");
        assert_eq!(present(), 0, "the file went with its handle");
    }

    /// A name the listing is told to skip is neither read nor visited.
    #[test]
    fn a_name_the_listing_skips_is_neither_read_nor_visited() {
        let root = tempfile::tempdir().expect("root should exist");
        let folder = root.path().join("folder");
        std::fs::create_dir(&folder).expect("folder should exist");
        std::fs::write(folder.join("kept"), b"payload").expect("kept file should exist");
        std::fs::write(folder.join("skipped"), b"payload").expect("skipped file should exist");
        let root_identity =
            current_scan_root_identity(root.path()).expect("the root should have an identity");
        let mut visited = Vec::new();

        current_folder_listing(
            root.path(),
            &root_identity,
            Path::new("folder"),
            |name| name == OsStr::new("skipped"),
            |name, _, _| {
                visited.push(name.to_os_string());
                true
            },
        )
        .expect("the folder should be listed");

        assert_eq!(visited, [OsString::from("kept")]);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_is_listed_as_the_scan_records_it_without_opening_its_entries() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let root = tempfile::tempdir().expect("root should exist");
        let folder = root.path().join("folder");
        std::fs::create_dir_all(folder.join("sub")).expect("subfolder should exist");
        std::fs::write(folder.join("file"), b"payload").expect("file should exist");
        std::os::unix::fs::symlink("sub", folder.join("link")).expect("link should exist");
        let sealed = folder.join("sealed");
        std::fs::create_dir(&sealed).expect("sealed folder should exist");
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000))
            .expect("sealed folder should be locked");
        let root_identity =
            current_scan_root_identity(root.path()).expect("the root should have an identity");

        let mut listed = Vec::new();
        let snapshot = current_folder_listing(
            root.path(),
            &root_identity,
            Path::new("folder"),
            |_| false,
            |name, kind, identity| {
                listed.push((name.to_os_string(), kind, identity.file_id));
                true
            },
        );
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755))
            .expect("sealed folder should be unlocked for its removal");

        let snapshot = snapshot.expect("the folder should be listed");
        let metadata = std::fs::symlink_metadata(&folder).expect("folder should have metadata");
        assert_eq!(
            snapshot.identity.map(|identity| identity.file_id),
            Some(FileId::new_inode(metadata.dev(), metadata.ino()))
        );
        listed.sort_by(|left, right| left.0.cmp(&right.0));
        let expected: Vec<_> = [
            ("file", PlannedKind::File),
            ("link", PlannedKind::Link),
            ("sealed", PlannedKind::Directory),
            ("sub", PlannedKind::Directory),
        ]
        .into_iter()
        .map(|(name, kind)| {
            let metadata =
                std::fs::symlink_metadata(folder.join(name)).expect("entry should have metadata");
            (
                OsString::from(name),
                kind,
                FileId::new_inode(metadata.dev(), metadata.ino()),
            )
        })
        .collect();
        assert_eq!(
            listed, expected,
            "a link is listed as a link and not followed, and a folder that cannot be opened is listed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_changes_while_it_is_listed_is_not_described() {
        let root = tempfile::tempdir().expect("root should exist");
        let folder = root.path().join("folder");
        std::fs::create_dir(&folder).expect("folder should exist");
        std::fs::write(folder.join("early"), b"payload").expect("file should exist");
        // A time long ago: a change to the folder moves it to now, however coarse the file
        // system's timestamps are.
        std::fs::File::open(&folder)
            .expect("folder should open")
            .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_001))
            .expect("the folder's modification time should be set");
        let root_identity =
            current_scan_root_identity(root.path()).expect("the root should have an identity");

        let result = current_folder_listing(
            root.path(),
            &root_identity,
            Path::new("folder"),
            |_| false,
            |_, _, _| {
                // Another process makes an entry in the folder while it is being read.
                std::fs::write(folder.join("late"), b"payload").expect("file should be made");
                true
            },
        );

        assert!(
            matches!(result, Err(DeletionPlanError::Changed)),
            "{result:?}"
        );
    }

    #[test]
    fn a_listing_that_is_stopped_describes_nothing() {
        let root = tempfile::tempdir().expect("root should exist");
        std::fs::create_dir(root.path().join("folder")).expect("folder should exist");
        std::fs::write(root.path().join("folder/entry"), b"payload").expect("file should exist");
        let root_identity =
            current_scan_root_identity(root.path()).expect("the root should have an identity");

        let result = current_folder_listing(
            root.path(),
            &root_identity,
            Path::new("folder"),
            |_| false,
            |_, _, _| false,
        );

        assert!(
            matches!(result, Err(DeletionPlanError::Cancelled)),
            "{result:?}"
        );
    }

    /// What is at the path must be a folder, not what a link there points at.
    #[cfg(unix)]
    #[test]
    fn only_a_folder_is_listed() {
        let root = tempfile::tempdir().expect("root should exist");
        std::fs::create_dir(root.path().join("folder")).expect("folder should exist");
        std::fs::write(root.path().join("file"), b"payload").expect("file should exist");
        std::os::unix::fs::symlink("folder", root.path().join("link")).expect("link should exist");
        let root_identity =
            current_scan_root_identity(root.path()).expect("the root should have an identity");

        for name in ["file", "link"] {
            let result = current_folder_listing(
                root.path(),
                &root_identity,
                Path::new(name),
                |_| false,
                |_, _, _| panic!("nothing is listed of {name}"),
            );
            assert!(
                matches!(result, Err(DeletionPlanError::Changed)),
                "{name}: {result:?}"
            );
        }
        assert!(matches!(
            current_folder_listing(
                root.path(),
                &root_identity,
                Path::new("missing"),
                |_| false,
                |_, _, _| true
            ),
            Err(DeletionPlanError::Missing(_))
        ));
    }

    #[test]
    fn tampered_spilled_plan_record_is_rejected_before_execution() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let child = directory.join("planned");
        std::fs::write(&child, b"payload").expect("planned child should be created");
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let mut plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &AtomicBool::new(false),
            1,
            &temporary_storage,
        )
        .expect("directory plan should spill");
        let PlanEntries::Spilled(spilled) = &mut plan.entries else {
            panic!("directory plan should use a spill file");
        };
        let spill = spilled.get_mut();
        let file = plan_spill_file_mut(&mut spill.file);
        file.seek(SeekFrom::Start(SPILL_RECORD_LENGTH_BYTES))
            .expect("spill payload should be seekable");
        file.write_all(&[0])
            .expect("spill payload should be mutable for the adversarial fixture");

        assert!(matches!(
            revalidate_plan(root.path(), &plan),
            Err(DeletionPlanError::Io {
                kind: io::ErrorKind::InvalidData,
                ..
            })
        ));
        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert!(!report.reporting_complete());
        assert!(directory.exists());
        assert!(child.exists());
        drop(report);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn tampered_spilled_result_record_returns_a_bounded_read_error() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        std::fs::write(directory.join("planned"), b"payload")
            .expect("planned child should be created");
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &AtomicBool::new(false),
            1,
            &temporary_storage,
        )
        .expect("directory plan should spill");
        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        let DeletionEntriesStorage::Spilled(result_spill) = &report.entries.storage else {
            panic!("spilled directory result should use a spill file");
        };
        {
            let mut result_spill = result_spill
                .lock()
                .expect("result spill should not be poisoned");
            let file = plan_spill_file_mut(&mut result_spill.file);
            file.seek(SeekFrom::Start(SPILL_RECORD_LENGTH_BYTES))
                .expect("result payload should be seekable");
            file.write_all(&[0])
                .expect("result payload should be mutable for the adversarial fixture");
        }

        let error = report
            .entries
            .iter()
            .next()
            .expect("result iterator should yield its first record")
            .expect_err("tampered result record should fail authentication");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(report.deleted_entries(), 2);
        drop(report);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn oversized_spilled_plan_record_is_rejected_without_execution() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let mut plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &AtomicBool::new(false),
            1,
            &temporary_storage,
        )
        .expect("directory plan should spill");
        let PlanEntries::Spilled(spilled) = &mut plan.entries else {
            panic!("directory plan should use a spill file");
        };
        let file = plan_spill_file_mut(&mut spilled.get_mut().file);
        file.seek(SeekFrom::Start(0))
            .expect("spill header should be seekable");
        file.write_all(&u64::MAX.to_le_bytes())
            .expect("spill header should be mutable for the adversarial fixture");

        assert!(matches!(
            revalidate_plan(root.path(), &plan),
            Err(DeletionPlanError::Io {
                kind: io::ErrorKind::InvalidData,
                ..
            })
        ));
        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert!(!report.reporting_complete());
        assert!(directory.exists());
        drop(report);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn cross_subtree_spilled_plan_record_is_rejected_before_execution() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let child = directory.join("planned");
        std::fs::write(&child, b"payload").expect("planned child should be created");
        let sibling = root.path().join("sibling");
        std::fs::write(&sibling, b"sibling").expect("sibling should be created");
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let mut plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &AtomicBool::new(false),
            1,
            &temporary_storage,
        )
        .expect("directory plan should spill");
        let sibling_snapshot = reviewed_snapshot(
            &sibling,
            &std::fs::symlink_metadata(&sibling).expect("sibling metadata should be readable"),
        );
        let mut injected =
            RecordSpill::new(&temporary_storage, MAX_PLAN_SPILL_RECORD_BYTES, root.path())
                .expect("spill should open");
        injected
            .push(
                &encode_spilled_entry(&PlannedEntry {
                    relative_path: PathBuf::from("sibling"),
                    snapshot: sibling_snapshot,
                })
                .expect("sibling entry should encode"),
            )
            .expect("sibling entry should spill");
        plan.entries = PlanEntries::Spilled(RefCell::new(injected));

        assert!(matches!(
            revalidate_plan(root.path(), &plan),
            Err(DeletionPlanError::InvalidRelativePath)
        ));
        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert!(!report.reporting_complete());
        assert!(directory.exists());
        assert!(child.exists());
        assert!(sibling.exists());
        drop(report);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn deletion_spill_directory_uses_target_parent() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let target = root.path().join("target");
        std::fs::create_dir(&target).expect("target directory should be created");

        assert_eq!(
            deletion_spill_directory(&target).expect("target parent should be usable"),
            root.path(),
        );
    }

    #[test]
    fn pending_spill_rejects_cross_subtree_record() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let sibling = root.path().join("sibling");
        std::fs::write(&sibling, b"sibling").expect("sibling should be created");
        let snapshot = reviewed_snapshot(
            &sibling,
            &std::fs::symlink_metadata(&sibling).expect("sibling metadata should be readable"),
        );
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let mut spill =
            RecordSpill::new(&temporary_storage, MAX_PLAN_SPILL_RECORD_BYTES, root.path())
                .expect("spill should open");
        spill
            .push(
                &encode_spilled_entry(&PlannedEntry {
                    relative_path: PathBuf::from("sibling"),
                    snapshot,
                })
                .expect("sibling entry should encode"),
            )
            .expect("sibling entry should spill");
        let mut pending = PendingDirectories {
            resident: Vec::new(),
            spill: Some(spill),
        };

        assert!(matches!(
            pending.pop(Path::new("target")),
            Err(DeletionPlanError::InvalidRelativePath)
        ));
        drop(pending);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn spilled_result_rejects_cross_subtree_record() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let sibling = root.path().join("sibling");
        std::fs::write(&sibling, b"sibling").expect("sibling should be created");
        let snapshot = reviewed_snapshot(
            &sibling,
            &std::fs::symlink_metadata(&sibling).expect("sibling metadata should be readable"),
        );
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let mut spill = RecordSpill::new(
            &temporary_storage,
            MAX_RESULT_SPILL_RECORD_BYTES,
            root.path(),
        )
        .expect("spill should open");
        spill
            .push(
                &encode_spilled_result(&DeletionEntryResult {
                    entry: PlannedEntry {
                        relative_path: PathBuf::from("sibling"),
                        snapshot,
                    },
                    outcome: DeletionEntryOutcome::Deleted,
                })
                .expect("sibling result should encode"),
            )
            .expect("sibling result should spill");
        let entries = DeletionEntries::spilled(
            PathBuf::from("target"),
            spill,
            DeletionSummary::default(),
            false,
            true,
            None,
        );

        let error = entries
            .iter()
            .next()
            .expect("result iterator should yield its first record")
            .expect_err("cross-subtree result must fail validation");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        drop(entries);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn result_spill_capacity_is_reserved_before_consent() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let mut initial_target = target(root.path(), OsString::from("target"), FileType::Folder);
        initial_target.reviewed_entries.clear();
        let plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            initial_target,
            false,
            &AtomicBool::new(false),
            1,
            &temporary_storage,
        )
        .expect("directory plan should spill");
        let plan_bytes = match &plan.entries {
            PlanEntries::InMemory(_) => panic!("directory plan should use a spill file"),
            PlanEntries::Spilled(spill) => spill.borrow().reservation.bytes(),
        };
        let result_bytes = match &plan.result_storage {
            PlannedResultStorage::InMemory { .. } => {
                panic!("spilled directory plan should reserve spilled results")
            }
            PlannedResultStorage::Spilled(spill) => spill.reservation.bytes(),
        };
        drop(plan);
        assert_eq!(temporary_storage.used(), 0);

        let capacity = plan_bytes
            .checked_add(result_bytes)
            .and_then(|total| total.checked_sub(1))
            .expect("fixture reservation should be nonzero");
        let constrained_storage = TemporaryStorage::with_limit_bytes(capacity);
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let error = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &AtomicBool::new(false),
            1,
            &constrained_storage,
        )
        .expect_err("result capacity must be reserved before consent");
        assert!(matches!(
            error,
            DeletionPlanError::Io {
                kind: io::ErrorKind::StorageFull,
                ..
            }
        ));
        assert!(directory.exists());
        assert_eq!(constrained_storage.used(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn spilled_directory_plan_preserves_large_hard_link_accounting() {
        const LINKS: usize = 96;

        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let first = directory.join("link-0");
        std::fs::write(&first, b"payload").expect("hard-link source should be created");
        for index in 1..LINKS {
            std::fs::hard_link(&first, directory.join(format!("link-{index}")))
                .expect("hard link should be created");
        }
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &AtomicBool::new(false),
            1,
            &temporary_storage,
        )
        .expect("hard-link directory plan should spill");
        let mut allocation = None;
        plan.entries
            .try_for_each(Path::new("target"), |entry| {
                if entry.relative_path == Path::new("target/link-0") {
                    allocation = entry.snapshot.allocated_bytes;
                }
                Ok(())
            })
            .expect("spilled plan should be readable");
        let allocation = allocation.expect("hard-link allocation should be known");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert!(report.entries.is_spilled());
        assert!(report.reporting_complete());
        assert_eq!(
            report.deleted_entries(),
            u64::try_from(LINKS + 1).expect("fixture entry count should fit"),
        );
        assert_eq!(
            report.deleted_allocated_bytes(),
            if proves_no_link_survived() {
                allocation
            } else {
                0
            }
        );
        assert!(!directory.exists());
        drop(report);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn spilled_directory_plan_revalidation_rejects_replacement_before_consent() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let planned = directory.join("planned");
        std::fs::write(&planned, b"original").expect("planned child should be created");
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &AtomicBool::new(false),
            1,
            &temporary_storage,
        )
        .expect("directory plan should spill");
        assert!(matches!(&plan.entries, PlanEntries::Spilled(_)));

        let original = directory.join("original");
        std::fs::rename(&planned, &original).expect("reviewed identity should be displaced");
        std::fs::write(&planned, b"replacement").expect("replacement should be created");

        assert!(matches!(
            revalidate_plan(root.path(), &plan),
            Err(DeletionPlanError::Changed)
        ));
        assert!(directory.exists());
        assert!(original.exists());
        assert!(planned.exists());
        drop(plan);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn spilled_directory_plan_soft_cancel_reports_every_identity() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        for index in 0..8 {
            std::fs::write(directory.join(format!("file-{index}")), b"payload")
                .expect("planned child should be created");
        }
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);
        let plan = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &AtomicBool::new(false),
            1,
            &temporary_storage,
        )
        .expect("directory plan should spill");
        let planned_entries = plan.planned_entries();
        assert!(matches!(&plan.entries, PlanEntries::Spilled(_)));

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(true),
            &AtomicBool::new(false),
        );

        assert!(report.soft_cancelled);
        assert_eq!(report.unattempted_entries(), planned_entries);
        assert_eq!(
            u64::try_from(report.entries.len()).expect("report entry count should fit"),
            planned_entries,
        );
        assert!(directory.exists());
        for index in 0..8 {
            assert!(directory.join(format!("file-{index}")).exists());
        }
        assert!(report.entries.is_spilled());
        assert!(report.reporting_complete());
        assert!(temporary_storage.used() > 0);
        drop(report);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn directory_plan_storage_exhaustion_stops_before_consent() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let planned = directory.join("planned");
        std::fs::write(&planned, b"payload").expect("planned child should be created");
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let temporary_storage = TemporaryStorage::with_limit_bytes(0);

        let error = build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &AtomicBool::new(false),
            0,
            &temporary_storage,
        )
        .expect_err("an unretainable directory plan must never reach confirmation");

        assert!(matches!(
            error,
            DeletionPlanError::Io {
                kind: io::ErrorKind::StorageFull,
                ..
            }
        ));
        assert!(error.to_string().contains("--temporary-storage-mib"));
        assert!(directory.exists());
        assert!(planned.exists());
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn cancelled_spilled_directory_plan_releases_storage() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let planned = directory.join("planned");
        std::fs::write(&planned, b"payload").expect("planned child should be created");
        let mut target = target(root.path(), OsString::from("target"), FileType::Folder);
        target.reviewed_entries.clear();
        let temporary_storage = TemporaryStorage::with_limit_bytes(2 * 1024 * 1024);

        assert!(matches!(
            build_plan_cancellable_with_temporary_storage(
                root.path(),
                target,
                false,
                &AtomicBool::new(true),
                0,
                &temporary_storage,
            ),
            Err(DeletionPlanError::Cancelled)
        ));
        assert!(directory.exists());
        assert!(planned.exists());
        assert_eq!(temporary_storage.used(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn deep_plan_does_not_retain_one_handle_per_level() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let target_path = root.path().join("target");
        std::fs::create_dir(&target_path).expect("target directory should be created");
        let mut deepest = target_path;
        for _ in 0..300 {
            deepest.push("d");
            std::fs::create_dir(&deepest).expect("deep directory should be created");
        }
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::Folder),
            false,
        )
        .expect("deep plan should stay within the process file descriptor limit");

        assert_eq!(plan.planned_entries(), 301);
    }

    /// Deleting trees nested past `PATH_MAX`, planned and executed through directory handles.
    ///
    /// Each test builds the tree, asks the system whether it accepts the renames that deleting a
    /// folder takes that deep (`rename_refusal_at` runs the executor's own), and then deletes.
    /// What a test requires of the outcome depends on the answer: where the system accepts them, a
    /// complete deletion; where it does not, only a history that is still true to the tree and
    /// that says why. No test asserts one system's behavior on the other. macOS 14 refuses a
    /// rename whose source is a folder that deep, which is what isolation avoids.
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    mod past_path_max {
        use std::collections::BTreeSet;

        use crate::tests::deep_tree::{DeepTree, LEVELS};

        use super::*;

        /// A target for the file or folder at `relative`, with the snapshot a scan records, read
        /// through the handles of the folders above it: no path to it can be named whole.
        fn deep_target(tree: &DeepTree, relative: &Path) -> FileToDelete {
            let metadata = tree
                .open_entry(relative)
                .metadata()
                .expect("the target should be readable through its handle");
            lone_target(tree.root(), relative, &metadata)
        }

        /// The tree before a deletion, what the deletion reported, and the tree after it.
        struct Deletion {
            /// The error of the first rename the system refused in the tree's deepest folder.
            refusal: Option<io::Error>,
            before: Vec<PathBuf>,
            planned: u64,
            report: DeletionReport,
            after: Vec<PathBuf>,
        }

        /// The error of the first rename the system refuses when a folder is deleted in the
        /// deepest folder of `tree`, or `None` when it accepts them all. It runs, on a folder made
        /// for the purpose, the renames the executor makes to delete one: `isolate_entry`, the
        /// removal, and `finalize_placeholder`.
        ///
        /// macOS 14 refuses a rename whose source is a folder that deep and accepts one whose
        /// source is a file. So the probe also tries a reference sequence that only ever names a
        /// file as a source (swap a file with a folder, then move the file), and requires
        /// isolation to succeed wherever that does: an isolation that named the folder first would
        /// be refused on macOS 14 and fail here, not pass as a refusal.
        fn rename_refusal_at(tree: &DeepTree) -> Option<io::Error> {
            const FOLDER: &str = ".probe-folder";
            const REFERENCE: &str = ".probe-reference";
            const MOVED: &str = ".probe-moved";

            let parent = tree.open_folder(LEVELS);
            let mode = rustix::fs::Mode::from_raw_mode(0o755);
            let remove = |name: &OsStr| {
                if rustix::fs::unlinkat(&parent, name, AtFlags::empty()).is_err() {
                    let _ = rustix::fs::unlinkat(&parent, name, AtFlags::REMOVEDIR);
                }
            };

            let files = PrivateFiles::default();
            let names = TransientNames::new(tree.root(), Path::new(FOLDER), &files);
            rustix::fs::mkdirat(&parent, FOLDER, mode).expect("the probe folder should be created");
            let (detached, placeholder, _registered) =
                create_placeholder(&parent, &names).expect("a placeholder should be reserved");
            let refusal = isolate_entry(&parent, OsStr::new(FOLDER), &detached)
                .and_then(|()| cap_fs::remove_dir(&parent, Path::new(&detached)))
                .and_then(|()| {
                    finalize_placeholder(
                        &parent,
                        &names,
                        OsStr::new(FOLDER),
                        &detached,
                        &placeholder,
                    )
                })
                .err();
            remove(OsStr::new(FOLDER));
            remove(&detached);

            let (file, _, _registered) =
                create_placeholder(&parent, &names).expect("a reference file should be created");
            rustix::fs::mkdirat(&parent, REFERENCE, mode)
                .expect("the reference folder should be created");
            let reference = exchange_names(&parent, &file, OsStr::new(REFERENCE)).and_then(|()| {
                rustix::fs::renameat_with(
                    &parent,
                    REFERENCE,
                    &parent,
                    MOVED,
                    rustix::fs::RenameFlags::NOREPLACE,
                )
                .map_err(io::Error::from)
            });
            for name in [OsStr::new(REFERENCE), file.as_os_str(), OsStr::new(MOVED)] {
                remove(name);
            }

            assert!(
                refusal.is_none() || reference.is_err(),
                "isolation was refused where renames whose source is a file are accepted: {refusal:?}"
            );
            refusal
        }

        /// Plans and executes the deletion of `relative`. `None` when planning refused on a kernel
        /// that leaves the mount-root answer to the mount table, which cannot name a path this long
        /// (see `the_mountinfo_fallback_agrees_with_the_kernel_and_refuses_what_it_cannot_name`):
        /// that refusal is the specified one, and it changes nothing. A kernel that reports mount
        /// roots plans at any depth.
        fn delete(tree: &DeepTree, relative: &Path) -> Option<Deletion> {
            let refusal = rename_refusal_at(tree);
            let before = tree.listing();
            let plan = match build_plan(tree.root(), deep_target(tree, relative), false) {
                Ok(plan) => plan,
                Err(error) => {
                    assert!(
                        !kernel_reports_mount_roots(&tree.open_folder(LEVELS - 1)),
                        "an entry past PATH_MAX should plan, as no step of planning names its whole path: {error:?}"
                    );
                    assert_eq!(tree.listing(), before, "a refused plan changes nothing");
                    return None;
                }
            };
            let planned = plan.planned_entries();
            let report = execute_plan(
                tree.root(),
                plan,
                &AtomicBool::new(false),
                &AtomicBool::new(false),
            );
            Some(Deletion {
                refusal,
                before,
                planned,
                report,
                after: tree.listing(),
            })
        }

        fn check(deletion: &Deletion, relative: &Path) {
            let mut deleted = BTreeSet::new();
            let mut reasons = Vec::new();
            for result in &deletion.report.entries {
                let result = result.expect("the history should read back");
                match result.outcome {
                    DeletionEntryOutcome::Deleted => {
                        deleted.insert(result.entry.relative_path);
                    }
                    DeletionEntryOutcome::Failed(reason) => reasons.push(reason),
                    DeletionEntryOutcome::Changed(_)
                    | DeletionEntryOutcome::Missing
                    | DeletionEntryOutcome::Unattempted => {}
                }
            }

            // Whichever way the system answered, the tree keeps everything the history does not
            // record as deleted, and nothing the history records as deleted.
            let remaining: Vec<PathBuf> = deletion
                .before
                .iter()
                .filter(|path| !deleted.contains(*path))
                .cloned()
                .collect();
            assert_eq!(
                deletion.after, remaining,
                "the tree and the history disagree (probe: {:?}, failures: {reasons:?})",
                deletion.refusal
            );

            let complete = deletion.report.deleted_entries() == deletion.planned;
            if deletion.refusal.is_none() {
                assert!(
                    complete
                        && deletion.report.failed_entries() == 0
                        && deletion.report.changed_entries() == 0
                        && deletion.report.missing_entries() == 0
                        && deletion.report.unattempted_entries() == 0,
                    "a system that accepts these renames deletes the tree completely: {reasons:?}"
                );
            }
            if complete {
                assert!(
                    deletion
                        .after
                        .iter()
                        .all(|path| !path.starts_with(relative)),
                    "a complete deletion leaves nothing of the target, not even a placeholder"
                );
            } else {
                assert!(
                    deletion.after.iter().any(|path| path == relative),
                    "a deletion that did not finish leaves its target in place"
                );
                assert!(
                    reasons.iter().any(|reason| reason.contains("(os error")),
                    "a deletion that did not finish says why: {reasons:?}"
                );
            }
        }

        #[test]
        fn a_folder_whose_own_path_is_past_path_max_is_deleted() {
            let tree = DeepTree::build();
            let relative = tree.relative_folder(LEVELS - 2);
            assert!(
                tree.root().join(&relative).as_os_str().len() > 4096,
                "the folder's own path should pass PATH_MAX on every system"
            );

            let Some(deletion) = delete(&tree, &relative) else {
                return;
            };

            assert_eq!(
                deletion.planned, 7,
                "three folders, their files, and the link"
            );
            check(&deletion, &relative);
        }

        #[test]
        fn a_shallow_folder_holding_a_chain_past_path_max_is_deleted() {
            let tree = DeepTree::build();
            let relative = tree.relative_folder(1);

            let Some(deletion) = delete(&tree, &relative) else {
                return;
            };

            assert_eq!(
                deletion.planned,
                u64::try_from(2 * LEVELS + 1).expect("a small count fits"),
                "every folder of the chain, its file, and the link"
            );
            check(&deletion, &relative);
        }

        #[test]
        fn the_deepest_file_past_path_max_is_deleted() {
            let tree = DeepTree::build();
            let relative = tree.relative_file(LEVELS);
            assert!(
                tree.root().join(&relative).as_os_str().len() > 4096,
                "the file's own path should pass PATH_MAX on every system"
            );

            let Some(deletion) = delete(&tree, &relative) else {
                return;
            };

            assert_eq!(deletion.planned, 1);
            check(&deletion, &relative);
        }

        #[test]
        fn a_refused_isolation_this_deep_ends_the_deletion_before_anything_is_removed() {
            let tree = DeepTree::build();
            let relative = tree.relative_folder(1);
            let before = tree.listing();
            let plan = build_plan(tree.root(), deep_target(&tree, &relative), false)
                .expect("a plan past PATH_MAX should build");
            let planned = plan.planned_entries();
            let mut attempts = 0_u32;

            // Every isolation is refused with "No space left on device", as macOS 14 refuses a
            // rename whose source is a folder this deep, and as a system that refused them all would.
            let report = execute_plan_unix_with_mutation_gate(
                tree.root(),
                plan,
                &AtomicBool::new(false),
                &AtomicBool::new(false),
                || true,
                || true,
                || {},
                |_| {},
                |_, _, _| {
                    attempts += 1;
                    Err(io::Error::from(rustix::io::Errno::NOSPC))
                },
            );

            assert_eq!(attempts, 1, "the deletion ends at the first refusal");
            assert_eq!(
                tree.listing(),
                before,
                "nothing was removed, and no placeholder was left behind"
            );
            assert_eq!(report.deleted_entries(), 0);
            assert_eq!(report.failed_entries(), 1, "the entry that was refused");
            assert_eq!(
                report.unattempted_entries(),
                planned - 1,
                "every other entry is recorded as not run"
            );
            assert!(!report.soft_cancelled, "the user did not cancel it");
            let reasons: Vec<String> = report
                .entries
                .iter()
                .filter_map(
                    |result| match result.expect("the history should read").outcome {
                        DeletionEntryOutcome::Failed(reason) => Some(reason),
                        _ => None,
                    },
                )
                .collect();
            assert!(
                reasons.len() == 1 && reasons[0].contains("No space left on device"),
                "the reason the system gave is in the history: {reasons:?}"
            );
        }
    }

    #[test]
    fn directory_uses_confirm_file_and_reduced_guardrails_uses_reduced_guard() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::create_dir(&path).expect("target directory should be created");
        let reviewed = target(root.path(), OsString::from("target"), FileType::Folder);
        let guarded =
            build_plan(root.path(), reviewed.clone(), false).expect("guarded plan should build");
        let reduced = build_plan(root.path(), reviewed, true).expect("reduced plan should build");

        assert_eq!(guarded.challenge, ConfirmationChallenge::ConfirmFile);
        assert_eq!(reduced.challenge, ConfirmationChallenge::ReducedGuard);
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_link_never_deletes_its_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("deletion root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        let outside_file = outside.path().join("outside");
        std::fs::write(&outside_file, b"outside").expect("outside target should be written");
        let link = root.path().join("link");
        symlink(&outside_file, &link).expect("link should be created");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("link"), FileType::File),
            false,
        )
        .expect("link plan should build without following it");
        assert_eq!(plan.root_snapshot().kind, PlannedKind::Link);
        let link_allocation = plan
            .root_snapshot()
            .allocated_bytes
            .expect("link-object allocation should be known");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(
            report.deleted_allocated_bytes(),
            if proves_no_link_survived() {
                link_allocation
            } else {
                0
            }
        );
        assert!(!link.exists());
        assert!(outside_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn mount_root_is_rejected_but_a_link_to_it_is_plannable() {
        use std::os::unix::fs::symlink;

        assert!(is_mount_root(Path::new("/")).expect("root mount should be inspectable"));
        let root = tempfile::tempdir().expect("deletion root should exist");
        let link = root.path().join("root-link");
        symlink("/", &link).expect("root link should be created");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("root-link"), FileType::File),
            false,
        )
        .expect("a link to a mount root should not follow its target");
        assert_eq!(plan.root_snapshot().kind, PlannedKind::Link);
    }

    #[test]
    fn linux_mountinfo_decoder_preserves_escaped_mount_paths() {
        let decoded = decode_linux_mountinfo_path(br"/mnt/a\040b\011c\012d\134e")
            .expect("mountinfo escape sequence should decode");
        assert_eq!(decoded, b"/mnt/a b\tc\nd\\e");
        assert!(decode_linux_mountinfo_path(br"/mnt/bad\x00").is_err());
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn a_refused_isolation_at_ordinary_depth_does_not_end_the_deletion() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        for name in ["a", "b", "c"] {
            std::fs::write(directory.join(name), name).expect("planned child should be written");
        }
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::Folder),
            false,
        )
        .expect("directory plan should build");
        let mut attempts = 0_u32;

        let report = execute_plan_unix_with_mutation_gate(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            || true,
            || true,
            || {},
            |_| {},
            |parent, left, right| {
                attempts += 1;
                if attempts == 1 {
                    Err(io::Error::from(rustix::io::Errno::NOSPC))
                } else {
                    isolate_entry(parent, left, right)
                }
            },
        );

        assert_eq!(report.failed_entries(), 1, "the entry that was refused");
        assert_eq!(report.deleted_entries(), 2, "the others are still deleted");
        assert_eq!(report.unattempted_entries(), 0, "the deletion went on");
        assert_eq!(
            report.changed_entries(),
            1,
            "the folder stays while the refused entry is in it"
        );
        assert_eq!(
            std::fs::read_dir(&directory)
                .expect("the folder should list")
                .count(),
            1
        );
    }

    /// The `stat` of a regular file, for a test to bend into one that no file system would give.
    #[cfg(unix)]
    fn stat_of_a_file() -> (tempfile::TempDir, File, rustix::fs::Stat) {
        let root = tempfile::tempdir().expect("fixture root should exist");
        std::fs::write(root.path().join("entry"), b"payload")
            .expect("fixture file should be written");
        let folder = File::open(root.path()).expect("fixture root should open");
        let stat =
            statat(&folder, "entry", AtFlags::SYMLINK_NOFOLLOW).expect("fixture file should stat");
        (root, folder, stat)
    }

    #[cfg(unix)]
    #[test]
    fn a_device_node_whose_major_does_not_fit_a_signed_device_number_is_planned() {
        let (_root, folder, mut stat) = stat_of_a_file();
        // macOS `dev_t` is a signed 32-bit value, so major 128 sets its sign bit. The `Metadata`
        // that cap-primitives builds from a `stat` unwraps `u64::try_from(st_rdev)`, which cannot
        // hold that, and panicked the planner. Nothing the planner keeps needs `st_rdev`.
        stat.st_mode =
            (stat.st_mode & !0o170_000) | rustix::fs::FileType::CharacterDevice.as_raw_mode();
        stat.st_rdev = rustix::fs::makedev(128, 1);

        let (snapshot, handle) =
            inspect_stat(&folder, OsStr::new("entry"), Path::new("entry"), &stat)
                .expect("a device node is planned like any other entry that is not a folder");

        assert_eq!(snapshot.kind, PlannedKind::File);
        assert!(handle.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn an_entry_reporting_a_negative_size_is_refused_with_a_message() {
        let (_root, folder, mut stat) = stat_of_a_file();
        stat.st_size = -1;

        let error = inspect_stat(&folder, OsStr::new("entry"), Path::new("entry"), &stat)
            .expect_err("a size that no file has should refuse the entry");

        let DeletionPlanError::Unrepresentable { path, message } = error else {
            panic!("the refusal should name the value, not an I/O failure: {error:?}");
        };
        assert_eq!(path, "entry");
        assert!(
            message.contains("size") && message.contains("-1"),
            "the message should name what the file system reported: {message}"
        );
    }

    /// A mount point directly below the root of the file system, with the root's handle: `/dev` is
    /// a device file system wherever this builds except Linux, where `/proc` is the mount every
    /// environment has.
    #[cfg(unix)]
    fn a_mount_point_below_the_root() -> (File, &'static OsStr) {
        let name = if cfg!(target_os = "linux") {
            "proc"
        } else {
            "dev"
        };
        (
            File::open("/").expect("the root of the file system should open"),
            OsStr::new(name),
        )
    }

    /// Whether this kernel reports the root of a mount itself. One that does not leaves the
    /// answer to the mount table, which can refuse a parent whose path it cannot name.
    #[cfg(unix)]
    fn kernel_reports_mount_roots(parent: &File) -> bool {
        #[cfg(target_os = "linux")]
        {
            rustix::fs::statx(
                parent,
                ".",
                AtFlags::empty(),
                rustix::fs::StatxFlags::empty(),
            )
            .is_ok_and(|status| {
                status
                    .stx_attributes_mask
                    .contains(rustix::fs::StatxAttributes::MOUNT_ROOT)
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = parent;
            true
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_mount_point_is_a_mount_root_from_its_parents_handle() {
        let (root, name) = a_mount_point_below_the_root();

        assert!(mount_root_in(&root, name).expect("a mount point should be inspectable"));
    }

    #[cfg(unix)]
    #[test]
    fn an_ordinary_folder_is_no_mount_root() {
        let root = tempfile::tempdir().expect("fixture root should exist");
        std::fs::create_dir(root.path().join("folder")).expect("fixture folder should be created");
        let parent = File::open(root.path()).expect("fixture root should open");

        assert!(
            !mount_root_in(&parent, OsStr::new("folder"))
                .expect("an ordinary folder should be inspectable")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_target_that_is_a_mount_root_is_refused_before_anything_below_it_is_read() {
        let (_, name) = a_mount_point_below_the_root();
        let relative = Path::new(name);
        let metadata = std::fs::symlink_metadata(Path::new("/").join(relative))
            .expect("the mount point should stat");
        let target = lone_target(Path::new("/"), relative, &metadata);

        let result = build_plan(Path::new("/"), target, false);

        assert!(
            matches!(result, Err(DeletionPlanError::Root)),
            "a mount root is not planned: {result:?}"
        );
    }

    /// Needs a real mount, which only an operator who opts in with `EXCISE_HARNESS_PRIVILEGED=1`
    /// gets: the harness attaches a small volume of its own under the folder, and detaches it
    /// again when the test ends.
    #[cfg(unix)]
    #[test]
    fn a_folder_that_holds_a_mounted_volume_is_refused_and_the_volume_is_left_alone() {
        use excise_harness::fixture::{PRIVILEGED_ENV, PrivilegedOptIn, Volume, VolumeSpec};

        let Some(opt_in) = PrivilegedOptIn::from_env() else {
            eprintln!("skipped: {PRIVILEGED_ENV}=1 is not set");
            return;
        };
        let root = tempfile::tempdir().expect("deletion root should exist");
        let work = tempfile::tempdir().expect("volume work area should exist");
        let folder = root.path().join("folder");
        let mount = folder.join("mount");
        std::fs::create_dir_all(&mount).expect("the mount point should be created");
        std::fs::write(folder.join("file"), b"payload").expect("a file should be written");
        let spec = VolumeSpec::new(8, "delmount").expect("the volume spec should be valid");
        let _volume =
            Volume::attach(opt_in, &spec, work.path(), &mount).expect("the volume should attach");
        std::fs::write(mount.join("inside"), b"payload")
            .expect("a file on the volume should be written");
        let parent = File::open(&folder).expect("the folder should open");
        let metadata = std::fs::symlink_metadata(&folder).expect("the folder should stat");

        assert!(
            mount_root_in(&parent, OsStr::new("mount"))
                .expect("the mount point should be inspectable"),
            "the root of the volume is the root of a mount"
        );
        assert!(
            !mount_root_in(&parent, OsStr::new("file"))
                .expect("an ordinary file should be inspectable")
        );
        let result = build_plan(
            root.path(),
            lone_target(root.path(), Path::new("folder"), &metadata),
            false,
        );

        assert!(
            matches!(result, Err(DeletionPlanError::Root)),
            "a folder that holds a mount is not planned: {result:?}"
        );
        assert!(
            mount.join("inside").exists(),
            "nothing on the volume was touched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_past_path_max_is_asked_about_without_its_path() {
        use crate::tests::deep_tree::{DeepTree, LEVELS};

        let tree = DeepTree::build();
        let parent = tree.open_folder(LEVELS - 1);
        let name = tree
            .relative_folder(LEVELS)
            .file_name()
            .expect("a folder has a name")
            .to_owned();

        match mount_root_in(&parent, &name) {
            Ok(mounted) => assert!(!mounted, "a folder of the chain is not the root of a mount"),
            Err(error) => assert!(
                !kernel_reports_mount_roots(&parent),
                "a kernel that reports mount roots answers at any depth: {error}"
            ),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_mountinfo_fallback_agrees_with_the_kernel_and_refuses_what_it_cannot_name() {
        use std::os::fd::AsRawFd as _;

        use crate::tests::deep_tree::{DeepTree, LEVELS};

        let (root, name) = a_mount_point_below_the_root();
        assert!(
            linux_mountinfo_contains_entry(&root, name)
                .expect("the mount table should name a mount point")
        );
        let ordinary = tempfile::tempdir().expect("fixture root should exist");
        std::fs::create_dir(ordinary.path().join("folder")).expect("fixture folder should exist");
        let parent = File::open(ordinary.path()).expect("fixture root should open");
        assert!(
            !linux_mountinfo_contains_entry(&parent, OsStr::new("folder"))
                .expect("the mount table should name an ordinary folder's parent")
        );

        // A parent whose path the kernel will not print, as a path past PATH_MAX is, cannot be
        // looked up in the table. The fallback must answer exactly when it can name the parent,
        // and refuse otherwise, never read an unnameable path as no mount.
        let tree = DeepTree::build();
        let deepest = tree.open_folder(LEVELS);
        let nameable = std::fs::read_link(format!("/proc/self/fd/{}", deepest.as_raw_fd())).is_ok();
        assert_eq!(
            linux_mountinfo_contains_entry(&deepest, OsStr::new("leaf.dat")).is_ok(),
            nameable
        );
    }

    #[cfg(windows)]
    #[test]
    fn deleting_a_junction_never_deletes_its_target() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        let outside_file = outside.path().join("outside");
        std::fs::write(&outside_file, b"outside").expect("outside target should be written");
        let junction = root.path().join("junction");
        let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "''"));
        let command = format!(
            "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path {} -Target {} | Out-Null",
            quote(&junction),
            quote(outside.path())
        );
        let output = std::process::Command::new("pwsh")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &command,
            ])
            .output()
            .expect("junction command should start");
        assert!(
            output.status.success(),
            "junction command failed with {}: stdout={:?} stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("junction"), FileType::Folder),
            false,
        )
        .expect("junction plan should build without following its target");
        assert_eq!(plan.root_snapshot().kind, PlannedKind::Link);
        let junction_allocation = plan
            .root_snapshot()
            .allocated_bytes
            .expect("reparse-object allocation should be known");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(report.deleted_allocated_bytes(), junction_allocation);
        assert!(!junction.exists());
        assert!(outside_file.exists());
    }

    #[cfg(windows)]
    #[test]
    fn sharing_violation_is_reported_without_deleting_target() {
        use std::os::windows::fs::OpenOptionsExt as _;

        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        const DELETE: u32 = 0x0001_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;

        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        let blocker = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .expect("sharing blocker should open");
        let denied_delete = std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .expect_err("delete access should be denied by the sharing blocker");
        assert_eq!(
            denied_delete.raw_os_error(),
            i32::try_from(ERROR_SHARING_VIOLATION).ok()
        );

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.failed_entries(), 1);
        assert!(path.exists());
        drop(blocker);
    }

    #[cfg(windows)]
    #[test]
    fn regular_file_deletion_reports_allocation_as_unknown() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(report.deleted_allocated_bytes(), 0);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn hostile_directory_name_uses_generated_challenge() {
        use std::os::unix::ffi::OsStringExt as _;

        let root = tempfile::tempdir().expect("deletion root should exist");
        let name = OsString::from_vec(b"bad\x1bname".to_vec());
        std::fs::create_dir(root.path().join(&name)).expect("hostile directory should be created");
        let plan = build_plan(
            root.path(),
            target(root.path(), name, FileType::Folder),
            false,
        )
        .expect("hostile directory plan should build");

        assert!(matches!(
            plan.challenge,
            ConfirmationChallenge::TypePhrase(_)
        ));
    }
    #[cfg(unix)]
    #[test]
    fn hostile_file_name_uses_generated_challenge() {
        use std::os::unix::ffi::OsStringExt as _;

        let root = tempfile::tempdir().expect("deletion root should exist");
        let name = OsString::from_vec(b"bad\x1bfile".to_vec());
        std::fs::write(root.path().join(&name), b"payload")
            .expect("hostile file should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), name, FileType::File),
            false,
        )
        .expect("hostile file plan should build");

        assert!(matches!(
            plan.challenge,
            ConfirmationChallenge::TypePhrase(ref phrase) if phrase.starts_with("DELETE ")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn replaced_scan_root_is_rejected_before_execution() {
        let parent = tempfile::tempdir().expect("deletion parent should exist");
        let scan_root = parent.path().join("scan-root");
        let original = parent.path().join("original-root");
        std::fs::create_dir(&scan_root).expect("scan root should be created");
        std::fs::write(scan_root.join("target"), b"original").expect("target should be written");
        let plan = build_plan(
            &scan_root,
            target(&scan_root, OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");

        std::fs::rename(&scan_root, &original).expect("original root should be displaced");
        std::fs::create_dir(&scan_root).expect("replacement root should be created");
        std::fs::write(scan_root.join("target"), b"replacement")
            .expect("replacement target should be written");

        let report = execute_plan(
            &scan_root,
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.failed_entries(), 1);
        assert!(scan_root.join("target").exists());
        assert!(original.join("target").exists());
    }

    #[cfg(unix)]
    #[test]
    fn allocated_bytes_are_reported_only_after_last_hard_link_deletion() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::write(&first, b"payload").expect("hard-link source should be written");
        std::fs::hard_link(&first, &second).expect("hard link should be created");

        let first_plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("first"), FileType::File),
            false,
        )
        .expect("first hard-link plan should build");
        let first_allocated = first_plan
            .root_snapshot()
            .allocated_bytes
            .expect("first allocation should be known");
        let first_report = execute_plan(
            root.path(),
            first_plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert_eq!(first_report.deleted_allocated_bytes(), 0);
        assert!(second.exists());

        let second_report = execute_plan(
            root.path(),
            build_plan(
                root.path(),
                target(root.path(), OsString::from("second"), FileType::File),
                false,
            )
            .expect("last hard-link plan should build"),
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert_eq!(
            second_report.deleted_allocated_bytes(),
            if proves_no_link_survived() {
                first_allocated
            } else {
                0
            }
        );
        assert!(!second.exists());
    }
    #[cfg(unix)]
    #[test]
    fn external_hard_link_created_after_planning_is_not_reported_as_freed() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let outside = tempfile::tempdir().expect("external root should exist");
        let external = outside.path().join("external");
        let first = root.path().join("first");
        std::fs::write(&first, b"payload").expect("hard-link source should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("first"), FileType::File),
            false,
        )
        .expect("file plan should build");
        std::fs::hard_link(&first, &external).expect("external hard link should be created");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(report.deleted_allocated_bytes(), 0);
        assert_eq!(
            external
                .metadata()
                .expect("external link should remain")
                .len(),
            7
        );
    }

    #[cfg(unix)]
    #[test]
    fn planned_hard_links_still_report_allocation_after_both_delete() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let first = directory.join("first");
        let second = directory.join("second");
        std::fs::write(&first, b"payload").expect("hard-link source should be written");
        std::fs::hard_link(&first, &second).expect("hard link should be created");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::Folder),
            false,
        )
        .expect("directory plan should build");
        let mut allocated = None;
        plan.entries
            .try_for_each(Path::new("target"), |entry| {
                if entry.relative_path == Path::new("target/first") {
                    allocated = entry.snapshot.allocated_bytes;
                }
                Ok(())
            })
            .expect("directory plan should be readable");
        let allocated = allocated.expect("allocation should be known");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 3);
        assert_eq!(
            report.deleted_allocated_bytes(),
            if proves_no_link_survived() {
                allocated
            } else {
                0
            }
        );
        assert!(!directory.exists());
    }
}
