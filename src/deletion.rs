use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::UNIX_EPOCH;

use cap_primitives::ambient_authority;
use cap_primitives::fs::{self as cap_fs, FollowSymlinks};
use file_id::FileId;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
#[cfg(not(unix))]
use sysinfo::{DiskRefreshKind, Disks};

use crate::model::NodeId;
use crate::model::NodeKind;
use crate::native_path::{
    EncodedNativePath, NativeIdentity, NativePath, identity_for, safe_display_os_str,
    safe_display_path_text, safe_display_text,
};
use crate::state::FileToDelete;
use crate::temporary_storage::{TemporaryStorage, TemporaryStorageReservation};

pub const DEFAULT_PLAN_LIMIT_BYTES: usize = 64 * 1024 * 1024;
const SPILL_RECORD_LENGTH_BYTES: u64 = 8;
const SPILL_RECORD_MAC_BYTES: u64 = 32;
const SPILL_MAC_BYTES: usize = 32;
const HMAC_BLOCK_BYTES: usize = 64;
const MAX_PLAN_SPILL_RECORD_BYTES: usize = 1024 * 1024;
const MAX_OUTCOME_DETAIL_BYTES: usize = 512;
const OUTCOME_DETAIL_TRUNCATION: &str = "…";
const MAX_JSON_ESCAPED_BYTES_PER_INPUT_BYTE: usize = 6;
const RESULT_SPILL_ENVELOPE_BYTES: usize = 64;
const MAX_RESULT_SPILL_RECORD_BYTES: usize = MAX_PLAN_SPILL_RECORD_BYTES
    + MAX_OUTCOME_DETAIL_BYTES * MAX_JSON_ESCAPED_BYTES_PER_INPUT_BYTE
    + RESULT_SPILL_ENVELOPE_BYTES;
const MAX_RESIDENT_DIRECTORY_TASKS: usize = 64;

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

#[derive(Debug)]
struct RecordSpill {
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
        Ok(Self {
            file: new_plan_spill_file(spill_directory)?,
            length: 0,
            records: 0,
            reservation: temporary_storage.reservation(0)?,
            maximum_payload,
            authentication: SpillAuthenticationKey::new()?,
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

#[cfg(windows)]
fn new_plan_spill_file(spill_directory: &Path) -> io::Result<PlanSpillFile> {
    crate::os::windows::create_private_temporary_file(spill_directory)
}

#[cfg(not(windows))]
fn new_plan_spill_file(_spill_directory: &Path) -> io::Result<PlanSpillFile> {
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

    fn into_collector(self) -> ResultCollector {
        match self {
            Self::InMemory { maximum_bytes, .. } => ResultCollector::InMemory {
                entries: Vec::new(),
                estimated_bytes: 0,
                maximum_bytes,
                summary: DeletionSummary::default(),
            },
            Self::Spilled(result_spill) => ResultCollector::Spilled {
                result_spill,
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
        summary: DeletionSummary,
    },
    Spilled {
        result_spill: RecordSpill,
        summary: DeletionSummary,
    },
}

impl ResultCollector {
    fn push(&mut self, mut result: DeletionEntryResult) -> io::Result<()> {
        bound_outcome_detail(&mut result.outcome);
        match self {
            Self::InMemory {
                entries,
                estimated_bytes,
                maximum_bytes,
                summary,
            } => {
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
                summary,
            } => {
                summary.note(&result);
                let payload = encode_spilled_result(&result)?;
                result_spill.push_reserved(&payload)?;
            }
        }
        Ok(())
    }

    fn finish(self, target: &Path, complete: bool, error: Option<String>) -> DeletionEntries {
        let error = error.map(|detail| bounded_outcome_detail(&detail));
        match self {
            Self::InMemory {
                entries, summary, ..
            } => DeletionEntries::in_memory(
                Some(target.to_path_buf()),
                entries,
                summary,
                complete,
                error,
            ),
            Self::Spilled {
                result_spill,
                summary,
            } => DeletionEntries::spilled(
                target.to_path_buf(),
                result_spill,
                summary,
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
        complete: bool,
        error: Option<String>,
    ) -> Self {
        let records = u64::try_from(entries.len()).unwrap_or(u64::MAX);
        Self {
            storage: DeletionEntriesStorage::InMemory(entries),
            target,
            records,
            summary,
            complete,
            error,
        }
    }

    fn spilled(
        target: PathBuf,
        result_spill: RecordSpill,
        summary: DeletionSummary,
        complete: bool,
        error: Option<String>,
    ) -> Self {
        let records = result_spill.records;
        Self {
            storage: DeletionEntriesStorage::Spilled(Arc::new(Mutex::new(result_spill))),
            target: Some(target),
            records,
            summary,
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

    pub(crate) fn as_slice(&self) -> Option<&[DeletionEntryResult]> {
        match &self.storage {
            DeletionEntriesStorage::InMemory(entries) => Some(entries),
            DeletionEntriesStorage::Spilled(_) => None,
        }
    }

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
        Self::in_memory(None, entries, summary, true, None)
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
}
impl fmt::Display for DeletionPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Synthetic => {
                formatter.write_str("aggregate and synthetic nodes cannot be deleted")
            }
            Self::Root => formatter.write_str("scan roots and filesystem roots cannot be deleted"),
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
            Self::Io { path, message, .. } => write!(
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

    #[must_use]
    pub(crate) fn is_missing_target(&self, plan: &DeletionPlan) -> bool {
        matches!(self, Self::Missing(path) if path == &plan.root_relative_path)
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
    if directory_target {
        let mount_root = match is_mount_root(&full_path) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(plan_io(&full_path, error)),
        };
        if mount_root {
            let unchanged = target
                .expected_snapshot
                .identity
                .as_ref()
                .is_some_and(|expected| {
                    current_scan_root_identity(&full_path)
                        .is_ok_and(|actual| same_object(expected, &actual))
                });
            return Err(if unchanged {
                DeletionPlanError::Root
            } else {
                DeletionPlanError::Changed
            });
        }
    }
    let root = open_root(scan_root, &scan_root_identity)?;
    let (snapshot, directory_handle) = inspect_relative(&root, &relative)?;
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
                let (actual, handle) = match inspect_relative(&root, &relative_path) {
                    Err(DeletionPlanError::Missing(_)) => return Err(DeletionPlanError::Changed),
                    result => result?,
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
    // Directory targets ordinarily have no retained model review: their fresh live walk above is
    // authoritative. If a caller supplied one, compare it without materializing spilled records.
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
    execute_plan_windows(scan_root, plan, soft_cancelled, hard_cancelled, None)
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
pub fn execute_plan(
    scan_root: &Path,
    mut plan: DeletionPlan,
    _soft_cancelled: &AtomicBool,
    _hard_cancelled: &AtomicBool,
) -> DeletionReport {
    let result_storage = std::mem::replace(&mut plan.result_storage, PlannedResultStorage::new(0));
    failed_report(
        scan_root,
        plan,
        result_storage.into_collector(),
        "permanent deletion is unavailable on this target",
    )
}
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
pub(crate) fn execute_plan_counted(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    progress: &AtomicU64,
) -> DeletionReport {
    execute_plan_unix_with_hooks(
        scan_root,
        plan,
        soft_cancelled,
        hard_cancelled,
        || {},
        |_| {
            progress.fetch_add(1, Ordering::Relaxed);
        },
    )
}

#[cfg(windows)]
pub(crate) fn execute_plan_counted(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    progress: &AtomicU64,
) -> DeletionReport {
    execute_plan_windows(
        scan_root,
        plan,
        soft_cancelled,
        hard_cancelled,
        Some(progress),
    )
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
pub(crate) fn execute_plan_counted(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    _progress: &AtomicU64,
) -> DeletionReport {
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
    mut plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    mut after_isolation: F,
    mut after_inspection: G,
) -> DeletionReport
where
    F: FnMut(),
    G: FnMut(&OsStr),
{
    let result_storage = std::mem::replace(&mut plan.result_storage, PlannedResultStorage::new(0));
    let mut results = result_storage.into_collector();
    let root = match open_root(scan_root, &plan.scan_root_identity) {
        Ok(root) => root,
        Err(error) => return failed_report(scan_root, plan, results, &error.to_string()),
    };
    let root_relative_path = plan.root_relative_path.clone();
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
        let outcome = execute_unix_entry(
            &root,
            &mut entry,
            &mut after_isolation,
            &mut after_inspection,
        );
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
#[allow(clippy::too_many_lines)]
fn execute_unix_entry<F, G>(
    root: &File,
    entry: &mut PlannedEntry,
    after_isolation: &mut F,
    after_inspection: &mut G,
) -> DeletionEntryOutcome
where
    F: FnMut(),
    G: FnMut(&OsStr),
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
    let (detached_name, placeholder) = match create_placeholder(&parent) {
        Ok(value) => value,
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    if let Err(error) = exchange_names(&parent, &original_name, &detached_name) {
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
            DeletionEntryOutcome::Failed(cleanup.map_or_else(
                |cleanup_error| format!("{error}; placeholder cleanup failed: {cleanup_error}"),
                |()| error.to_string(),
            ))
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
            return match finalize_placeholder(&parent, &original_name, &detached_name, &placeholder)
            {
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

    let link_hold = if matches!(entry.snapshot.kind, PlannedKind::File | PlannedKind::Link) {
        if actual.identity.link_count.is_some() {
            if let Ok(name) = create_link_hold(&parent, &detached_name) {
                Some(name)
            } else {
                entry.snapshot.identity.link_count = None;
                None
            }
        } else {
            entry.snapshot.identity.link_count = None;
            None
        }
    } else {
        None
    };
    after_inspection(&detached_name);

    let removal = match entry.snapshot.kind {
        PlannedKind::Directory => cap_fs::remove_dir(&parent, Path::new(&detached_name)),
        PlannedKind::File | PlannedKind::Link => {
            cap_fs::remove_file(&parent, Path::new(&detached_name))
        }
    };
    match removal {
        Ok(()) => {
            if let Some(link_hold) = link_hold.as_ref()
                && !link_hold_matches_count(&parent, link_hold, actual.identity.link_count)
                    .unwrap_or(false)
            {
                entry.snapshot.identity.link_count = None;
            }
            let finalization =
                finalize_placeholder(&parent, &original_name, &detached_name, &placeholder);
            let hold_cleanup = link_hold
                .as_ref()
                .map(|name| remove_link_hold(&parent, name, &actual.identity));
            match (finalization, hold_cleanup) {
                (Ok(()), Some(Ok(())) | None) => DeletionEntryOutcome::Deleted,
                (Ok(()), Some(Err(error))) => DeletionEntryOutcome::Failed(format!(
                    "target deleted; hard-link check cleanup failed: {error}"
                )),
                (Err(error), Some(Ok(())) | None) => DeletionEntryOutcome::Failed(format!(
                    "target deleted; namespace cleanup failed: {error}"
                )),
                (Err(error), Some(Err(cleanup_error))) => DeletionEntryOutcome::Failed(format!(
                    "target deleted; namespace cleanup failed: {error}; hard-link check cleanup failed: {cleanup_error}"
                )),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let finalization =
                finalize_placeholder(&parent, &original_name, &detached_name, &placeholder);
            let hold_cleanup = link_hold
                .as_ref()
                .map(|name| remove_link_hold(&parent, name, &actual.identity));
            match (finalization, hold_cleanup) {
                (Ok(()), Some(Ok(())) | None) => DeletionEntryOutcome::Missing,
                (Ok(()), Some(Err(cleanup_error))) => DeletionEntryOutcome::Failed(format!(
                    "target disappeared; hard-link check cleanup failed: {cleanup_error}"
                )),
                (Err(error), Some(Ok(())) | None) => DeletionEntryOutcome::Failed(format!(
                    "target disappeared; namespace cleanup failed: {error}"
                )),
                (Err(error), Some(Err(cleanup_error))) => DeletionEntryOutcome::Failed(format!(
                    "target disappeared; namespace cleanup failed: {error}; hard-link check cleanup failed: {cleanup_error}"
                )),
            }
        }
        Err(error) => {
            let restore = restore_detached(&parent, &original_name, &detached_name, &placeholder);
            let hold_cleanup = link_hold
                .as_ref()
                .map(|name| remove_link_hold(&parent, name, &actual.identity));
            let cleanup_error = hold_cleanup.and_then(Result::err);
            if error.kind() == io::ErrorKind::DirectoryNotEmpty {
                restore.map_or_else(
                    |restore_error| {
                        DeletionEntryOutcome::Failed(format!(
                            "directory changed; namespace recovery failed: {restore_error}"
                        ))
                    },
                    |()| {
                        if let Some(cleanup_error) = cleanup_error {
                            DeletionEntryOutcome::Failed(format!(
                                "directory changed; hard-link check cleanup failed: {cleanup_error}"
                            ))
                        } else {
                            DeletionEntryOutcome::Changed(
                                "directory contains a new or changed entry".to_string(),
                            )
                        }
                    },
                )
            } else {
                DeletionEntryOutcome::Failed(restore.map_or_else(
                    |restore_error| format!("{error}; namespace recovery failed: {restore_error}"),
                    |()| {
                        cleanup_error.map_or_else(
                            || error.to_string(),
                            |cleanup_error| {
                                format!("{error}; hard-link check cleanup failed: {cleanup_error}")
                            },
                        )
                    },
                ))
            }
        }
    }
}

#[cfg(windows)]
fn execute_plan_windows(
    scan_root: &Path,
    mut plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    progress: Option<&AtomicU64>,
) -> DeletionReport {
    let result_storage = std::mem::replace(&mut plan.result_storage, PlannedResultStorage::new(0));
    let mut results = result_storage.into_collector();
    let root = match open_root(scan_root, &plan.scan_root_identity) {
        Ok(root) => root,
        Err(error) => return failed_report(scan_root, plan, results, &error.to_string()),
    };
    let root_relative_path = plan.root_relative_path.clone();
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
        let outcome = execute_windows_entry(&root, &mut entry);
        if matches!(outcome, DeletionEntryOutcome::Deleted) {
            note_deleted_link(&mut entry);
        }
        if let Some(progress) = progress {
            progress.fetch_add(1, Ordering::Relaxed);
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
fn execute_windows_entry(root: &File, entry: &mut PlannedEntry) -> DeletionEntryOutcome {
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
    match crate::windows_delete::remove_open_handle(&handle) {
        Ok(()) => DeletionEntryOutcome::Deleted,
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

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn create_placeholder(parent: &File) -> io::Result<(OsString, PlannedSnapshot)> {
    use std::sync::atomic::AtomicU64;

    static NEXT_PLACEHOLDER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..128 {
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
        let name = OsString::from(format!(
            ".excise-delete-{token}-{:x}-{sequence:x}",
            std::process::id()
        ));
        let mut options = cap_fs::OpenOptions::new();
        options.write(true).create_new(true);
        match cap_fs::open(parent, Path::new(&name), &options) {
            Ok(file) => {
                let metadata = file.metadata()?;
                return snapshot_from_std_metadata(&metadata, PlannedKind::File)
                    .map(|snapshot| (name, snapshot));
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

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn finalize_placeholder(
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
    rustix::fs::renameat_with(
        parent,
        Path::new(original),
        parent,
        Path::new(detached),
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)?;
    remove_verified_placeholder(parent, detached, placeholder)
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn create_link_hold(parent: &File, source: &OsStr) -> io::Result<OsString> {
    use std::sync::atomic::AtomicU64;

    static NEXT_LINK_HOLD: AtomicU64 = AtomicU64::new(0);
    for _ in 0..128 {
        let sequence = NEXT_LINK_HOLD.fetch_add(1, Ordering::Relaxed);
        let name = OsString::from(format!(
            ".excise-link-check-{:x}-{sequence:x}",
            std::process::id()
        ));
        match cap_fs::hard_link(parent, Path::new(source), parent, Path::new(&name)) {
            Ok(()) => return Ok(name),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a hard-link verification name",
    ))
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn link_hold_matches_count(parent: &File, hold: &OsStr, expected: Option<u64>) -> io::Result<bool> {
    let metadata = cap_fs::stat(parent, Path::new(hold), FollowSymlinks::No)?;
    let snapshot = snapshot_from_cap_metadata(parent, hold, &metadata, PlannedKind::File)?;
    Ok(snapshot.identity.link_count == expected)
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn remove_link_hold(parent: &File, hold: &OsStr, expected: &NativeIdentity) -> io::Result<()> {
    let metadata = match cap_fs::stat(parent, Path::new(hold), FollowSymlinks::No) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let actual = snapshot_from_cap_metadata(parent, hold, &metadata, PlannedKind::File)?;
    if !same_object(expected, &actual.identity) {
        return Err(io::Error::other(
            "hard-link verification name no longer contains the target",
        ));
    }
    match cap_fs::remove_file(parent, Path::new(hold)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn inspect_relative(
    root: &File,
    relative: &Path,
) -> Result<(PlannedSnapshot, Option<File>), DeletionPlanError> {
    let (parent, name) = open_parent(root, relative)?;
    match inspect_child(&parent, &name, relative) {
        Err(DeletionPlanError::Io {
            kind: io::ErrorKind::NotFound,
            ..
        }) => Err(DeletionPlanError::Missing(relative.to_path_buf())),
        result => result,
    }
}

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

fn is_mount_root(path: &Path) -> io::Result<bool> {
    let canonical = std::fs::canonicalize(path)?;
    if canonical.parent().is_none_or(|parent| parent == canonical) {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let path_dev = std::fs::symlink_metadata(&canonical)?.dev();
        let parent = canonical.parent().expect("non-root path has a parent");
        let parent_dev = std::fs::symlink_metadata(parent)?.dev();
        Ok(path_dev != parent_dev)
    }
    #[cfg(not(unix))]
    {
        let disks = Disks::new_with_refreshed_list_specifics(DiskRefreshKind::nothing());
        Ok(disks.list().iter().any(|disk| {
            std::fs::canonicalize(disk.mount_point()).is_ok_and(|mount| mount == canonical)
        }))
    }
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

fn challenge_for(
    target: &FileToDelete,
    snapshot: &PlannedSnapshot,
    reduced_guardrails: bool,
) -> ConfirmationChallenge {
    let name = target.path_to_file.last().map_or_else(
        || safe_display_os_str(OsStr::new("")),
        |name| safe_display_os_str(name),
    );
    if name.deceptive {
        return ConfirmationChallenge::TypePhrase(format!(
            "DELETE {}",
            challenge_code(&snapshot.identity.file_id)
        ));
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

fn same_object(expected: &NativeIdentity, actual: &NativeIdentity) -> bool {
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
            .then(|| crate::os::physical_size_from_handle(handle))
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

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn snapshot_from_cap_metadata(
    _parent: &File,
    _name: &OsStr,
    metadata: &cap_fs::Metadata,
    kind: PlannedKind,
) -> io::Result<PlannedSnapshot> {
    use cap_primitives::fs::MetadataExt as _;

    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.into_std().duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok(PlannedSnapshot {
        identity: NativeIdentity {
            file_id: FileId::new_inode(metadata.dev(), metadata.ino()),
            link_count: Some(metadata.nlink()),
            reparse_point: metadata.is_symlink(),
        },
        kind,
        apparent_bytes: u128::from(metadata.len()),

        allocated_bytes: matches!(kind, PlannedKind::File | PlannedKind::Link)
            .then(|| u128::from(metadata.blocks()).saturating_mul(512)),
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
        *estimated_bytes = next;
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
        assert!(temporary_storage.used() > 0);
        drop(report);
        assert_eq!(temporary_storage.used(), 0);
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
        assert_eq!(report.deleted_allocated_bytes(), allocation);
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
        assert_eq!(report.deleted_allocated_bytes(), link_allocation);
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
        assert_eq!(second_report.deleted_allocated_bytes(), first_allocated);
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
        assert_eq!(report.deleted_allocated_bytes(), allocated);
        assert!(!directory.exists());
    }
}
