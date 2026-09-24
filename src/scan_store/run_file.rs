use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek as _, SeekFrom, Write as _};
use std::path::PathBuf;

use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::scan_coordinator::ScanGeneration;
use crate::temporary_storage::TemporaryStorageReservation;

const RUN_MAGIC: [u8; 4] = *b"EXSR";
const BLOCK_MAGIC: [u8; 4] = *b"EXSB";
const FOOTER_MAGIC: [u8; 4] = *b"EXSF";
const RUN_VERSION: u16 = 1;
const HEADER_BYTES: usize = 28;
const BLOCK_HEADER_BYTES: usize = 44;
const FOOTER_BYTES: usize = 20;
const RECORD_LENGTH_BYTES: usize = 2 * size_of::<u32>();

/// Identifies the sorted record family stored in one immutable run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum RunKind {
    PathObservation = 1,
    IdentityObservation = 2,
    AllocationContribution = 3,
    DirectorySummary = 4,
    ChildQuery = 5,
    PathCatalog = 6,
}

impl RunKind {
    #[must_use]
    pub(crate) const fn code(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub(crate) const fn from_code(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::PathObservation),
            2 => Some(Self::IdentityObservation),
            3 => Some(Self::AllocationContribution),
            4 => Some(Self::DirectorySummary),
            5 => Some(Self::ChildQuery),
            6 => Some(Self::PathCatalog),
            _ => None,
        }
    }
}

/// Immutable identity embedded in a sealed run header and manifest entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunDescriptor {
    generation: ScanGeneration,
    run_id: u64,
    kind: RunKind,
}

impl RunDescriptor {
    #[must_use]
    pub(crate) const fn new(generation: ScanGeneration, run_id: u64, kind: RunKind) -> Self {
        Self {
            generation,
            run_id,
            kind,
        }
    }

    #[must_use]
    pub(crate) const fn generation(self) -> ScanGeneration {
        self.generation
    }

    #[must_use]
    pub(crate) const fn run_id(self) -> u64 {
        self.run_id
    }

    #[must_use]
    pub(crate) const fn kind(self) -> RunKind {
        self.kind
    }
}

#[derive(Debug, Error)]
pub(crate) enum RunError {
    #[error("scan run I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("scan run must begin with an empty file and reservation")]
    NonEmptyStart,
    #[error("scan run block capacity must be nonzero")]
    ZeroBlockCapacity,
    #[error("scan run record is larger than its bounded block")]
    RecordTooLarge,
    #[error("scan run keys must be strictly sorted")]
    UnsortedKey,
    #[error("scan run has an invalid header")]
    InvalidHeader,
    #[error("scan run has an invalid block")]
    InvalidBlock,
    #[error("scan run block checksum mismatch")]
    ChecksumMismatch,
    #[error("scan run ended before its sealed footer")]
    Truncated,
    #[error("scan run has data after its footer")]
    TrailingData,
    #[error("scan run was already consumed")]
    AlreadyConsumed,
    #[error("sealed run does not support indexed range reads")]
    UnindexedRange,
    #[error("scan run count overflow")]
    CountOverflow,
    #[error("scan run byte offset overflow")]
    OffsetOverflow,
}

#[derive(Debug)]
struct SparseRunIndex {
    block_capacity: usize,
    blocks: Vec<SparseRunBlock>,
}

#[derive(Debug)]
struct SparseRunBlock {
    first_key: Vec<u8>,
    offset: u64,
}

/// Builds one sorted immutable run using bounded blocks and sequential writes.
pub(crate) struct RunWriter {
    writer: BufWriter<File>,
    path: Option<PathBuf>,
    reservation: Option<TemporaryStorageReservation>,
    descriptor: RunDescriptor,
    block_capacity: usize,
    block: Vec<u8>,
    block_first_key: Option<Vec<u8>>,
    block_records: u32,
    total_records: u64,
    total_blocks: u64,
    bytes_written: u64,
    last_key: Vec<u8>,
    has_key: bool,
    sparse_index: Option<Vec<SparseRunBlock>>,
}

impl RunWriter {
    /// # Errors
    ///
    /// Returns an error when the caller supplies a nonempty file/reservation or
    /// when the header cannot be charged and written.
    pub(crate) fn new(
        file: File,
        reservation: TemporaryStorageReservation,
        descriptor: RunDescriptor,
        block_capacity: usize,
    ) -> Result<Self, RunError> {
        Self::new_with_path(file, None, reservation, descriptor, block_capacity)
    }

    /// Builds a run backed by a named private-session file.
    pub(crate) fn new_with_path(
        file: File,
        path: Option<PathBuf>,
        reservation: TemporaryStorageReservation,
        descriptor: RunDescriptor,
        block_capacity: usize,
    ) -> Result<Self, RunError> {
        if block_capacity == 0 {
            return Err(RunError::ZeroBlockCapacity);
        }
        if file.metadata()?.len() != 0 || reservation.bytes() != 0 {
            return Err(RunError::NonEmptyStart);
        }
        let mut writer = Self {
            writer: BufWriter::new(file),
            path,
            reservation: Some(reservation),
            descriptor,
            block_capacity,
            block: Vec::with_capacity(block_capacity),
            block_first_key: None,
            block_records: 0,
            total_records: 0,
            total_blocks: 0,
            bytes_written: 0,
            last_key: Vec::new(),
            has_key: false,
            sparse_index: matches!(
                descriptor.kind(),
                RunKind::ChildQuery | RunKind::PathCatalog
            )
            .then(Vec::new),
        };
        let header = encode_header(descriptor, block_capacity)?;
        writer.write_reserved(&header)?;
        Ok(writer)
    }

    #[must_use]
    pub(crate) const fn descriptor(&self) -> RunDescriptor {
        self.descriptor
    }

    /// Appends one strictly key-ordered record to the current bounded block.
    ///
    /// # Errors
    ///
    /// Returns an error if the record is too large, out of order, or cannot be
    /// reserved and written to the run.
    pub(crate) fn append(&mut self, key: &[u8], value: &[u8]) -> Result<(), RunError> {
        if self.has_key && key <= self.last_key.as_slice() {
            return Err(RunError::UnsortedKey);
        }
        let record_bytes = RECORD_LENGTH_BYTES
            .checked_add(key.len())
            .and_then(|size| size.checked_add(value.len()))
            .ok_or(RunError::OffsetOverflow)?;
        let key_length = u32::try_from(key.len()).map_err(|_| RunError::RecordTooLarge)?;
        let value_length = u32::try_from(value.len()).map_err(|_| RunError::RecordTooLarge)?;
        if record_bytes > self.block_capacity {
            return Err(RunError::RecordTooLarge);
        }
        if !self.block.is_empty()
            && self
                .block
                .len()
                .checked_add(record_bytes)
                .is_none_or(|size| size > self.block_capacity)
        {
            self.flush_block()?;
        }
        if self.block.is_empty() && self.sparse_index.is_some() {
            self.block_first_key = Some(key.to_vec());
        }
        self.block.extend_from_slice(&key_length.to_le_bytes());
        self.block.extend_from_slice(&value_length.to_le_bytes());
        self.block.extend_from_slice(key);
        self.block.extend_from_slice(value);
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.has_key = true;
        self.block_records = self
            .block_records
            .checked_add(1)
            .ok_or(RunError::CountOverflow)?;
        self.total_records = self
            .total_records
            .checked_add(1)
            .ok_or(RunError::CountOverflow)?;
        Ok(())
    }

    /// Seals and flushes the run. Only a sealed run can be read.
    pub(crate) fn seal(mut self) -> Result<SealedRun, RunError> {
        self.flush_block()?;
        let footer = encode_footer(self.total_records, self.total_blocks);
        self.write_reserved(&footer)?;
        self.writer.flush()?;
        self.writer.get_ref().sync_data()?;
        let file = self.writer.get_ref().try_clone()?;
        let path = self.path.take();
        Ok(SealedRun {
            file: Some(file),
            path,
            reservation: self.reservation.take(),
            descriptor: self.descriptor,
            bytes: self.bytes_written,
            sparse_index: self.sparse_index.map(|blocks| SparseRunIndex {
                block_capacity: self.block_capacity,
                blocks,
            }),
        })
    }

    fn flush_block(&mut self) -> Result<(), RunError> {
        if self.block.is_empty() {
            return Ok(());
        }
        let payload_length =
            u32::try_from(self.block.len()).map_err(|_| RunError::RecordTooLarge)?;
        let mut header = Vec::with_capacity(BLOCK_HEADER_BYTES);
        header.extend_from_slice(&BLOCK_MAGIC);
        header.extend_from_slice(&self.block_records.to_le_bytes());
        header.extend_from_slice(&payload_length.to_le_bytes());
        header.extend_from_slice(&Sha256::digest(&self.block));
        debug_assert_eq!(header.len(), BLOCK_HEADER_BYTES);
        if let Some(index) = self.sparse_index.as_mut() {
            let first_key = self
                .block_first_key
                .take()
                .expect("an indexed nonempty block has a first key");
            index.push(SparseRunBlock {
                first_key,
                offset: self.bytes_written,
            });
        }
        self.write_reserved(&header)?;
        let payload = std::mem::take(&mut self.block);
        self.write_reserved(&payload)?;
        self.block = Vec::with_capacity(self.block_capacity);
        self.block_records = 0;
        self.total_blocks = self
            .total_blocks
            .checked_add(1)
            .ok_or(RunError::CountOverflow)?;
        Ok(())
    }

    fn write_reserved(&mut self, bytes: &[u8]) -> Result<(), RunError> {
        let next = self
            .bytes_written
            .checked_add(u64::try_from(bytes.len()).map_err(|_| RunError::OffsetOverflow)?)
            .ok_or(RunError::OffsetOverflow)?;
        self.reservation
            .as_mut()
            .expect("run writer must retain its reservation")
            .grow_to(next)?;
        self.writer.write_all(bytes)?;
        self.bytes_written = next;
        Ok(())
    }
}

/// A sealed run owns both its file and storage charge until it is dropped.
#[derive(Debug)]
pub(crate) struct SealedRun {
    file: Option<File>,
    path: Option<PathBuf>,
    reservation: Option<TemporaryStorageReservation>,
    descriptor: RunDescriptor,
    bytes: u64,
    sparse_index: Option<SparseRunIndex>,
}

impl Drop for SealedRun {
    fn drop(&mut self) {
        drop(self.file.take());
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl SealedRun {
    #[must_use]
    pub(crate) const fn descriptor(&self) -> RunDescriptor {
        self.descriptor
    }

    #[must_use]
    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Reopens one manifest-owned private-session run and validates its footer.
    pub(crate) fn open_named(
        file: File,
        path: PathBuf,
        reservation: TemporaryStorageReservation,
        descriptor: RunDescriptor,
        bytes: u64,
    ) -> Result<Self, RunError> {
        if file.metadata()?.len() != bytes {
            return Err(RunError::Truncated);
        }
        let sparse_index = matches!(
            descriptor.kind(),
            RunKind::ChildQuery | RunKind::PathCatalog
        )
        .then(|| rebuild_sparse_index(&file, bytes))
        .transpose()?;
        let mut file = file;
        file.seek(SeekFrom::Start(0))?;
        let mut reader = RunReader::open(file, Some(path), reservation, bytes, sparse_index)
            .map_err(|mut error| {
                error
                    .error
                    .take()
                    .expect("open error should retain its error")
            })?;
        if reader.descriptor() != descriptor {
            return Err(RunError::InvalidHeader);
        }
        reader.visit_records(|_, _| Ok(()))?;
        Ok(reader.into_sealed())
    }

    #[cfg(test)]
    pub(crate) fn preserve_path_for_recovery(&mut self) {
        let _ = self.path.take();
    }

    /// # Errors
    ///
    /// Returns an error when the sealed file cannot be rewound and validated.
    pub(crate) fn into_reader(mut self) -> Result<RunReader, RunError> {
        let mut file = self.file.take().expect("sealed run must own a file");
        let reservation = self
            .reservation
            .take()
            .expect("sealed run must own its reservation");
        file.seek(SeekFrom::Start(0))?;
        let path = self.path.take();
        RunReader::open(
            file,
            path,
            reservation,
            self.bytes,
            self.sparse_index.take(),
        )
        .map_err(|mut error| {
            error
                .error
                .take()
                .expect("open error should retain its error")
        })
    }

    /// Borrows this sealed run for one sequential pass, restoring ownership
    /// before returning even when `visit` fails.
    ///
    /// # Errors
    ///
    /// Returns a run validation error or the visitor's error.
    pub(crate) fn with_reader<T, E>(
        &mut self,
        visit: impl FnOnce(&mut RunReader) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<RunError>,
    {
        let mut file = self.file.take().expect("sealed run must own a file");
        let path = self.path.take();
        let reservation = self
            .reservation
            .take()
            .expect("sealed run must own its reservation");
        let sparse_index = self.sparse_index.take();
        if let Err(error) = file.seek(SeekFrom::Start(0)) {
            self.file = Some(file);
            self.path = path;
            self.reservation = Some(reservation);
            self.sparse_index = sparse_index;
            return Err(E::from(RunError::Io(error)));
        }
        let mut reader = match RunReader::open(file, path, reservation, self.bytes, sparse_index) {
            Ok(reader) => reader,
            Err(mut error) => {
                self.file = Some(error.file);
                self.path = error.path.take();
                self.reservation = Some(error.reservation);
                self.sparse_index = error.sparse_index;
                return Err(E::from(
                    error
                        .error
                        .take()
                        .expect("open error should retain its error"),
                ));
            }
        };
        let result = visit(&mut reader);
        let mut restored = reader.into_sealed();
        self.file = restored.file.take();
        self.path = restored.path.take();
        self.reservation = restored.reservation.take();
        self.sparse_index = restored.sparse_index.take();
        result
    }
    /// Starts a checksum-validating read near `lower_bound` using a sparse
    /// block index captured while this immutable child-query run was written.
    ///
    /// The reader may return records from the preceding block before `lower_bound`.
    /// Callers compare keys before consuming records.
    ///
    /// # Errors
    ///
    /// Returns an error when this is not an indexed child-query run or its
    /// backing file cannot be cloned and positioned.
    pub(crate) fn range_reader(&self, lower_bound: &[u8]) -> Result<RunRangeReader, RunError> {
        if !matches!(
            self.descriptor.kind(),
            RunKind::ChildQuery | RunKind::PathCatalog
        ) {
            return Err(RunError::UnindexedRange);
        }
        let index = self.sparse_index.as_ref().ok_or(RunError::UnindexedRange)?;
        let footer_bytes = u64::try_from(FOOTER_BYTES).expect("footer length fits u64");
        let fallback = self
            .bytes
            .checked_sub(footer_bytes)
            .ok_or(RunError::InvalidBlock)?;
        let block = index
            .blocks
            .partition_point(|block| block.first_key.as_slice() <= lower_bound)
            .checked_sub(1)
            .and_then(|position| index.blocks.get(position));
        let offset = block.map_or(fallback, |block| block.offset);
        let mut file = self
            .file
            .as_ref()
            .expect("sealed run must retain its file")
            .try_clone()?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(RunRangeReader::new(file, index.block_capacity))
    }
}

struct RunOpenError {
    error: Option<RunError>,
    file: File,
    path: Option<PathBuf>,
    reservation: TemporaryStorageReservation,
    sparse_index: Option<SparseRunIndex>,
}

struct RunPathGuard {
    path: Option<PathBuf>,
}

impl RunPathGuard {
    fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }

    fn take(&mut self) -> Option<PathBuf> {
        self.path.take()
    }
}

impl Drop for RunPathGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.as_ref() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Sequential reader for a sealed run with one reusable decoded block.
pub(crate) struct RunReader {
    reader: BufReader<File>,
    path_guard: RunPathGuard,
    reservation: TemporaryStorageReservation,
    descriptor: RunDescriptor,
    bytes: u64,
    block_capacity: usize,
    block: Vec<u8>,
    block_cursor: usize,
    block_records: u32,
    total_records: u64,
    total_blocks: u64,
    last_key: Vec<u8>,
    has_key: bool,
    consumed: bool,
    finished: bool,
    sparse_index: Option<SparseRunIndex>,
}

impl RunReader {
    fn open(
        file: File,
        path: Option<PathBuf>,
        reservation: TemporaryStorageReservation,
        bytes: u64,
        sparse_index: Option<SparseRunIndex>,
    ) -> Result<Self, RunOpenError> {
        let mut reader = BufReader::new(file);
        let (descriptor, block_capacity) = match decode_header(&mut reader) {
            Ok(decoded) => decoded,
            Err(error) => {
                return Err(RunOpenError {
                    error: Some(error),
                    file: reader.into_inner(),
                    path,
                    reservation,
                    sparse_index,
                });
            }
        };
        Ok(Self {
            reader,
            path_guard: RunPathGuard::new(path),
            reservation,
            descriptor,
            bytes,
            block_capacity,
            block: Vec::with_capacity(block_capacity),
            block_cursor: 0,
            block_records: 0,
            total_records: 0,
            total_blocks: 0,
            last_key: Vec::new(),
            has_key: false,
            consumed: false,
            finished: false,
            sparse_index,
        })
    }

    #[must_use]
    pub(crate) const fn descriptor(&self) -> RunDescriptor {
        self.descriptor
    }

    /// Returns the sealed run after a sequential pass.
    ///
    /// A subsequent reader always rewinds and validates the header again. The
    /// caller may stop early when its query has enough records.
    #[must_use]
    pub(crate) fn into_sealed(mut self) -> SealedRun {
        let path = self.path_guard.take();
        let Self {
            reader,
            reservation,
            descriptor,
            bytes,
            sparse_index,
            ..
        } = self;

        SealedRun {
            file: Some(reader.into_inner()),
            path,
            reservation: Some(reservation),
            descriptor,
            bytes,
            sparse_index,
        }
    }

    pub(crate) fn next_record_into(
        &mut self,
        key: &mut Vec<u8>,
        value: &mut Vec<u8>,
    ) -> Result<bool, RunError> {
        if self.finished {
            return Ok(false);
        }
        self.consumed = true;
        if self.block_records == 0 && !self.load_next_block()? {
            return Ok(false);
        }

        let key_length = read_length(&self.block, &mut self.block_cursor)?;
        let value_length = read_length(&self.block, &mut self.block_cursor)?;
        let key_end = self
            .block_cursor
            .checked_add(key_length)
            .ok_or(RunError::InvalidBlock)?;
        let value_end = key_end
            .checked_add(value_length)
            .ok_or(RunError::InvalidBlock)?;
        if value_end > self.block.len() {
            return Err(RunError::InvalidBlock);
        }
        let input_key = &self.block[self.block_cursor..key_end];
        if self.has_key && input_key <= self.last_key.as_slice() {
            return Err(RunError::UnsortedKey);
        }
        key.clear();
        key.extend_from_slice(input_key);
        value.clear();
        value.extend_from_slice(&self.block[key_end..value_end]);
        self.last_key.clear();
        self.last_key.extend_from_slice(input_key);
        self.has_key = true;
        self.block_cursor = value_end;
        self.block_records = self
            .block_records
            .checked_sub(1)
            .ok_or(RunError::InvalidBlock)?;
        if self.block_records == 0 && self.block_cursor != self.block.len() {
            return Err(RunError::InvalidBlock);
        }
        self.total_records = self
            .total_records
            .checked_add(1)
            .ok_or(RunError::CountOverflow)?;
        Ok(true)
    }

    /// Visits records in their sealed key order without materializing the run.
    ///
    /// # Errors
    ///
    /// Returns an error when any frame is malformed, out of order, corrupt, or
    /// when `visit` rejects a record.
    pub(crate) fn visit_records(
        &mut self,
        mut visit: impl FnMut(&[u8], &[u8]) -> Result<(), RunError>,
    ) -> Result<(), RunError> {
        if self.consumed {
            return Err(RunError::AlreadyConsumed);
        }
        let mut key = Vec::new();
        let mut value = Vec::new();
        while self.next_record_into(&mut key, &mut value)? {
            visit(&key, &value)?;
        }
        Ok(())
    }

    fn load_next_block(&mut self) -> Result<bool, RunError> {
        let mut tag = [0_u8; 4];
        read_exact(&mut self.reader, &mut tag)?;
        if tag == FOOTER_MAGIC {
            let expected_records = read_u64(&mut self.reader)?;
            let expected_blocks = read_u64(&mut self.reader)?;
            if self.total_records != expected_records || self.total_blocks != expected_blocks {
                return Err(RunError::InvalidBlock);
            }
            let mut trailing = [0_u8; 1];
            if self.reader.read(&mut trailing)? != 0 {
                return Err(RunError::TrailingData);
            }
            self.finished = true;
            return Ok(false);
        }
        if tag != BLOCK_MAGIC {
            return Err(RunError::InvalidBlock);
        }
        let records = read_u32(&mut self.reader)?;
        let payload_length =
            usize::try_from(read_u32(&mut self.reader)?).map_err(|_| RunError::InvalidBlock)?;
        if records == 0 || payload_length == 0 || payload_length > self.block_capacity {
            return Err(RunError::InvalidBlock);
        }
        let mut expected_digest = [0_u8; 32];
        read_exact(&mut self.reader, &mut expected_digest)?;
        self.block.clear();
        self.block.resize(payload_length, 0);
        read_exact(&mut self.reader, &mut self.block)?;
        if Sha256::digest(&self.block).as_slice() != expected_digest {
            return Err(RunError::ChecksumMismatch);
        }
        self.block_cursor = 0;
        self.block_records = records;
        self.total_blocks = self
            .total_blocks
            .checked_add(1)
            .ok_or(RunError::CountOverflow)?;
        Ok(true)
    }
}

/// Read cursor positioned from a sparse block index rather than the run head.
/// It validates every traversed frame, including its SHA-256 payload digest,
/// but deliberately cannot re-count records preceding its start block.
pub(crate) struct RunRangeReader {
    reader: BufReader<File>,
    block_capacity: usize,
    block: Vec<u8>,
    block_cursor: usize,
    block_records: u32,
    last_key: Vec<u8>,
    has_key: bool,
    finished: bool,
}

impl RunRangeReader {
    fn new(file: File, block_capacity: usize) -> Self {
        Self {
            reader: BufReader::new(file),
            block_capacity,
            block: Vec::with_capacity(block_capacity),
            block_cursor: 0,
            block_records: 0,
            last_key: Vec::new(),
            has_key: false,
            finished: false,
        }
    }

    /// Decodes the next traversed record into caller-owned reusable buffers.
    ///
    /// # Errors
    ///
    /// Returns an error when a traversed frame is malformed, out of order, or
    /// has a checksum mismatch.
    pub(crate) fn next_record_into(
        &mut self,
        key: &mut Vec<u8>,
        value: &mut Vec<u8>,
    ) -> Result<bool, RunError> {
        if self.finished {
            return Ok(false);
        }
        if self.block_records == 0 && !self.load_next_block()? {
            return Ok(false);
        }
        let key_length = read_length(&self.block, &mut self.block_cursor)?;
        let value_length = read_length(&self.block, &mut self.block_cursor)?;
        let key_end = self
            .block_cursor
            .checked_add(key_length)
            .ok_or(RunError::InvalidBlock)?;
        let value_end = key_end
            .checked_add(value_length)
            .ok_or(RunError::InvalidBlock)?;
        if value_end > self.block.len() {
            return Err(RunError::InvalidBlock);
        }
        let input_key = &self.block[self.block_cursor..key_end];
        if self.has_key && input_key <= self.last_key.as_slice() {
            return Err(RunError::UnsortedKey);
        }
        key.clear();
        key.extend_from_slice(input_key);
        value.clear();
        value.extend_from_slice(&self.block[key_end..value_end]);
        self.last_key.clear();
        self.last_key.extend_from_slice(input_key);
        self.has_key = true;
        self.block_cursor = value_end;
        self.block_records = self
            .block_records
            .checked_sub(1)
            .ok_or(RunError::InvalidBlock)?;
        if self.block_records == 0 && self.block_cursor != self.block.len() {
            return Err(RunError::InvalidBlock);
        }
        Ok(true)
    }

    fn load_next_block(&mut self) -> Result<bool, RunError> {
        let mut tag = [0_u8; 4];
        read_exact(&mut self.reader, &mut tag)?;
        if tag == FOOTER_MAGIC {
            let _expected_records = read_u64(&mut self.reader)?;
            let _expected_blocks = read_u64(&mut self.reader)?;
            let mut trailing = [0_u8; 1];
            if self.reader.read(&mut trailing)? != 0 {
                return Err(RunError::TrailingData);
            }
            self.finished = true;
            return Ok(false);
        }
        if tag != BLOCK_MAGIC {
            return Err(RunError::InvalidBlock);
        }
        let records = read_u32(&mut self.reader)?;
        let payload_length =
            usize::try_from(read_u32(&mut self.reader)?).map_err(|_| RunError::InvalidBlock)?;
        if records == 0 || payload_length == 0 || payload_length > self.block_capacity {
            return Err(RunError::InvalidBlock);
        }
        let mut expected_digest = [0_u8; 32];
        read_exact(&mut self.reader, &mut expected_digest)?;
        self.block.clear();
        self.block.resize(payload_length, 0);
        read_exact(&mut self.reader, &mut self.block)?;
        if Sha256::digest(&self.block).as_slice() != expected_digest {
            return Err(RunError::ChecksumMismatch);
        }
        self.block_cursor = 0;
        self.block_records = records;
        Ok(true)
    }
}

fn encode_header(
    descriptor: RunDescriptor,
    block_capacity: usize,
) -> Result<[u8; HEADER_BYTES], RunError> {
    let block_capacity = u32::try_from(block_capacity).map_err(|_| RunError::RecordTooLarge)?;
    let mut header = [0_u8; HEADER_BYTES];
    header[..4].copy_from_slice(&RUN_MAGIC);
    header[4..6].copy_from_slice(&RUN_VERSION.to_le_bytes());
    header[6] = descriptor.kind.code();
    header[7] = 0;
    header[8..16].copy_from_slice(&descriptor.generation.value().to_le_bytes());
    header[16..24].copy_from_slice(&descriptor.run_id.to_le_bytes());
    header[24..28].copy_from_slice(&block_capacity.to_le_bytes());
    Ok(header)
}

fn decode_header(reader: &mut impl Read) -> Result<(RunDescriptor, usize), RunError> {
    let mut header = [0_u8; HEADER_BYTES];
    read_exact(reader, &mut header)?;
    if header[..4] != RUN_MAGIC
        || u16::from_le_bytes([header[4], header[5]]) != RUN_VERSION
        || header[7] != 0
    {
        return Err(RunError::InvalidHeader);
    }
    let Some(kind) = RunKind::from_code(header[6]) else {
        return Err(RunError::InvalidHeader);
    };
    let generation = ScanGeneration::from_value(u64::from_le_bytes(
        header[8..16]
            .try_into()
            .map_err(|_| RunError::InvalidHeader)?,
    ));
    let run_id = u64::from_le_bytes(
        header[16..24]
            .try_into()
            .map_err(|_| RunError::InvalidHeader)?,
    );
    let block_capacity = usize::try_from(u32::from_le_bytes(
        header[24..28]
            .try_into()
            .map_err(|_| RunError::InvalidHeader)?,
    ))
    .map_err(|_| RunError::InvalidHeader)?;
    if block_capacity == 0 {
        return Err(RunError::InvalidHeader);
    }
    Ok((RunDescriptor::new(generation, run_id, kind), block_capacity))
}

fn rebuild_sparse_index(file: &File, bytes: u64) -> Result<SparseRunIndex, RunError> {
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let (_, block_capacity) = decode_header(&mut reader)?;
    let mut blocks = Vec::new();
    loop {
        let offset = reader.stream_position()?;
        let mut tag = [0_u8; 4];
        read_exact(&mut reader, &mut tag)?;
        if tag == FOOTER_MAGIC {
            let _records = read_u64(&mut reader)?;
            let _blocks = read_u64(&mut reader)?;
            if reader.stream_position()? != bytes {
                return Err(RunError::TrailingData);
            }
            break;
        }
        if tag != BLOCK_MAGIC {
            return Err(RunError::InvalidBlock);
        }
        let records = read_u32(&mut reader)?;
        let payload_length =
            usize::try_from(read_u32(&mut reader)?).map_err(|_| RunError::InvalidBlock)?;
        if records == 0 || payload_length == 0 || payload_length > block_capacity {
            return Err(RunError::InvalidBlock);
        }
        let mut expected_digest = [0_u8; 32];
        read_exact(&mut reader, &mut expected_digest)?;
        let mut payload = vec![0_u8; payload_length];
        read_exact(&mut reader, &mut payload)?;
        if Sha256::digest(&payload).as_slice() != expected_digest {
            return Err(RunError::ChecksumMismatch);
        }
        let mut cursor = 0;
        let key_length = read_length(&payload, &mut cursor)?;
        let value_length = read_length(&payload, &mut cursor)?;
        let key_end = cursor
            .checked_add(key_length)
            .ok_or(RunError::InvalidBlock)?;
        let value_end = key_end
            .checked_add(value_length)
            .ok_or(RunError::InvalidBlock)?;
        if value_end > payload.len() {
            return Err(RunError::InvalidBlock);
        }
        blocks.push(SparseRunBlock {
            first_key: payload[cursor..key_end].to_vec(),
            offset,
        });
    }
    Ok(SparseRunIndex {
        block_capacity,
        blocks,
    })
}

fn encode_footer(records: u64, blocks: u64) -> [u8; FOOTER_BYTES] {
    let mut footer = [0_u8; FOOTER_BYTES];
    footer[..4].copy_from_slice(&FOOTER_MAGIC);
    footer[4..12].copy_from_slice(&records.to_le_bytes());
    footer[12..20].copy_from_slice(&blocks.to_le_bytes());
    footer
}

fn read_exact(reader: &mut impl Read, bytes: &mut [u8]) -> Result<(), RunError> {
    reader.read_exact(bytes).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            RunError::Truncated
        } else {
            RunError::Io(error)
        }
    })
}

fn read_u32(reader: &mut impl Read) -> Result<u32, RunError> {
    let mut bytes = [0_u8; 4];
    read_exact(reader, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> Result<u64, RunError> {
    let mut bytes = [0_u8; 8];
    read_exact(reader, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_length(payload: &[u8], cursor: &mut usize) -> Result<usize, RunError> {
    let end = cursor
        .checked_add(size_of::<u32>())
        .ok_or(RunError::InvalidBlock)?;
    let Some(bytes) = payload.get(*cursor..end) else {
        return Err(RunError::InvalidBlock);
    };
    *cursor = end;
    usize::try_from(u32::from_le_bytes(
        bytes.try_into().map_err(|_| RunError::InvalidBlock)?,
    ))
    .map_err(|_| RunError::InvalidBlock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temporary_storage::TemporaryStorage;

    fn writer(storage: &TemporaryStorage, block_capacity: usize) -> Result<RunWriter, RunError> {
        RunWriter::new(
            tempfile::tempfile()?,
            storage.reservation(0)?,
            RunDescriptor::new(ScanGeneration::from_value(7), 42, RunKind::PathObservation),
            block_capacity,
        )
    }

    #[test]
    fn sealed_run_round_trips_sorted_records_across_blocks() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let mut writer = writer(&storage, 24).expect("run writer should initialize");
        writer
            .append(b"alpha", b"one")
            .expect("first record should append");
        writer
            .append(b"beta", b"two")
            .expect("second record should append");
        writer
            .append(b"gamma", b"three")
            .expect("third record should append");
        let sealed = writer.seal().expect("run should seal");
        assert_eq!(
            sealed.descriptor(),
            RunDescriptor::new(ScanGeneration::from_value(7), 42, RunKind::PathObservation)
        );
        assert!(
            sealed.bytes()
                > u64::try_from(HEADER_BYTES + FOOTER_BYTES).expect("constant should fit")
        );
        assert!(storage.used() > 0);

        let mut reader = sealed.into_reader().expect("sealed run should open");
        let mut records = Vec::new();
        reader
            .visit_records(|key, value| {
                records.push((key.to_vec(), value.to_vec()));
                Ok(())
            })
            .expect("sealed records should validate");
        assert_eq!(
            records,
            vec![
                (b"alpha".to_vec(), b"one".to_vec()),
                (b"beta".to_vec(), b"two".to_vec()),
                (b"gamma".to_vec(), b"three".to_vec()),
            ]
        );
        assert_eq!(reader.descriptor().run_id(), 42);
        drop(reader);
        assert_eq!(storage.used(), 0);
    }

    #[test]
    fn writer_rejects_duplicate_or_descending_keys_before_sealing() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let mut run = writer(&storage, 128).expect("run writer should initialize");
        run.append(b"beta", b"one")
            .expect("first record should append");
        assert!(matches!(
            run.append(b"beta", b"two"),
            Err(RunError::UnsortedKey)
        ));
        assert!(matches!(
            run.append(b"alpha", b"three"),
            Err(RunError::UnsortedKey)
        ));
        let mut empty_key_writer = writer(&storage, 128).expect("run writer should initialize");
        empty_key_writer
            .append(b"", b"one")
            .expect("empty root key should append once");
        assert!(matches!(
            empty_key_writer.append(b"", b"two"),
            Err(RunError::UnsortedKey)
        ));
    }

    #[test]
    fn reader_rejects_corrupt_block_payloads() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let mut writer = writer(&storage, 128).expect("run writer should initialize");
        writer
            .append(b"alpha", b"one")
            .expect("record should append");
        let sealed = writer.seal().expect("run should seal");
        let mut file = sealed
            .file
            .as_ref()
            .expect("sealed run should retain its file")
            .try_clone()
            .expect("sealed file should clone");
        file.seek(SeekFrom::Start(
            u64::try_from(HEADER_BYTES + BLOCK_HEADER_BYTES).expect("offset should fit"),
        ))
        .expect("file should seek");
        file.write_all(b"!").expect("file should corrupt");
        file.sync_data().expect("corruption should flush");

        let mut reader = sealed.into_reader().expect("sealed run should open");
        assert!(matches!(
            reader.visit_records(|_, _| Ok(())),
            Err(RunError::ChecksumMismatch)
        ));
    }

    #[test]
    fn reader_rejects_missing_footer() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let mut writer = writer(&storage, 128).expect("run writer should initialize");
        writer
            .append(b"alpha", b"one")
            .expect("record should append");
        let sealed = writer.seal().expect("run should seal");
        let file = sealed
            .file
            .as_ref()
            .expect("sealed run should retain its file")
            .try_clone()
            .expect("sealed file should clone");
        file.set_len(sealed.bytes() - 1)
            .expect("footer should truncate");
        file.sync_data().expect("truncation should flush");

        let mut reader = sealed
            .into_reader()
            .expect("truncated run header should open");
        assert!(matches!(
            reader.visit_records(|_, _| Ok(())),
            Err(RunError::Truncated)
        ));
    }

    #[test]
    fn child_query_range_reader_seeks_to_the_requested_block() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let mut writer = RunWriter::new(
            tempfile::tempfile().expect("child query file should open"),
            storage
                .reservation(0)
                .expect("empty child query reservation should fit"),
            RunDescriptor::new(ScanGeneration::initial(), 7, RunKind::ChildQuery),
            24,
        )
        .expect("child query writer should initialize");
        for (key, value) in [
            (b"alpha".as_slice(), b"one".as_slice()),
            (b"beta".as_slice(), b"two".as_slice()),
            (b"delta".as_slice(), b"four".as_slice()),
            (b"gamma".as_slice(), b"three".as_slice()),
        ] {
            writer
                .append(key, value)
                .expect("child query records should remain sorted");
        }
        let sealed = writer.seal().expect("child query should seal");
        let mut reader = sealed
            .range_reader(b"delta")
            .expect("child query should have a sparse index");
        let mut key = Vec::new();
        let mut value = Vec::new();
        assert!(
            reader
                .next_record_into(&mut key, &mut value)
                .expect("indexed block should validate")
        );
        assert_eq!(key, b"delta");
        assert_eq!(value, b"four");
        assert!(
            reader
                .next_record_into(&mut key, &mut value)
                .expect("following block should validate")
        );
        assert_eq!(key, b"gamma");
        assert_eq!(value, b"three");
    }

    #[test]
    fn reservation_prevents_run_growth_beyond_its_quota() {
        let storage = TemporaryStorage::with_limit_bytes(HEADER_BYTES as u64);
        let writer = writer(&storage, 128).expect("header should fit the quota");
        let error = writer.seal().expect_err("footer should exceed the quota");
        assert!(matches!(error, RunError::Io(error) if error.kind() == io::ErrorKind::StorageFull));
        assert_eq!(storage.used(), 0);
    }
}
