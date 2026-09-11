#[cfg(not(windows))]
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use super::task_queue::DirectoryTask;
use crate::native_path::{EncodedNativePath, NativeIdentity, NativePath};
use crate::temporary_storage::{TemporaryStorage, TemporaryStorageReservation};

const MAX_SPILLED_TASK_BYTES: usize = 1024 * 1024;
pub(super) const SPILL_LENGTH_BYTES: u64 = 8;
const TASK_SPILL_COMPACTION_BYTES: u64 = 64 * 1024;

#[cfg(windows)]
type TaskSpillFile = tempfile::NamedTempFile;
#[cfg(not(windows))]
type TaskSpillFile = File;

pub(super) struct TaskSpill {
    file: TaskSpillFile,
    next_read: u64,
    next_write: u64,
    pending: usize,
    reservation: TemporaryStorageReservation,
}

impl TaskSpill {
    pub(super) fn new(temporary_storage: &TemporaryStorage) -> io::Result<(Self, Option<PathBuf>)> {
        #[cfg(windows)]
        let (file, spill_path) = {
            let file = tempfile::NamedTempFile::new()?;
            crate::os::windows::restrict_private_path(file.path(), false)?;
            crate::os::windows::verify_private_path(file.path(), false)?;
            let spill_path = Some(file.path().to_path_buf());
            (file, spill_path)
        };
        #[cfg(not(windows))]
        let (file, spill_path) = (tempfile::tempfile()?, None);

        Ok((
            Self {
                file,
                next_read: 0,
                next_write: 0,
                pending: 0,
                reservation: temporary_storage.reservation(0)?,
            },
            spill_path,
        ))
    }

    pub(super) fn push(&mut self, task: DirectoryTask) -> io::Result<()> {
        let encoded = NativePath::new(task.path).encode();
        let payload = serde_json::to_vec(&(encoded, task.identity)).map_err(io::Error::other)?;
        if payload.len() > MAX_SPILLED_TASK_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "scanner task path exceeds the spill record limit",
            ));
        }
        let payload_len = u64::try_from(payload.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "scanner task path length does not fit in a spill record",
            )
        })?;
        let next_write = self
            .next_write
            .checked_add(SPILL_LENGTH_BYTES)
            .and_then(|offset| offset.checked_add(payload_len))
            .ok_or_else(|| io::Error::other("scanner task spill offset overflow"))?;
        let pending = self
            .pending
            .checked_add(1)
            .ok_or_else(|| io::Error::other("scanner task spill count overflow"))?;

        self.reservation.grow_to(next_write)?;
        self.file.seek(SeekFrom::Start(self.next_write))?;
        self.file.write_all(&payload_len.to_le_bytes())?;
        self.file.write_all(&payload)?;
        self.next_write = next_write;
        self.pending = pending;
        Ok(())
    }

    pub(super) fn take(&mut self) -> io::Result<Option<DirectoryTask>> {
        if self.pending == 0 {
            return Ok(None);
        }

        self.file.seek(SeekFrom::Start(self.next_read))?;
        let mut length = [0_u8; std::mem::size_of::<u64>()];
        self.file.read_exact(&mut length)?;
        let payload_len = usize::try_from(u64::from_le_bytes(length)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "scanner task spill record length does not fit in memory",
            )
        })?;
        if payload_len > MAX_SPILLED_TASK_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "scanner task spill record exceeds the configured limit",
            ));
        }
        let mut payload = vec![0_u8; payload_len];
        self.file.read_exact(&mut payload)?;
        let (encoded, identity): (EncodedNativePath, Option<NativeIdentity>) =
            serde_json::from_slice(&payload).map_err(io::Error::other)?;
        let path = NativePath::decode(&encoded)
            .map_err(io::Error::other)?
            .as_path()
            .to_path_buf();
        let record_len = u64::try_from(payload_len)
            .map_err(|_| io::Error::other("scanner task spill record length overflow"))?;
        self.next_read = self
            .next_read
            .checked_add(SPILL_LENGTH_BYTES)
            .and_then(|offset| offset.checked_add(record_len))
            .ok_or_else(|| io::Error::other("scanner task spill offset overflow"))?;
        self.pending = self
            .pending
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("scanner task spill count underflow"))?;
        if self.pending == 0 {
            self.next_read = 0;
            self.next_write = 0;
            self.truncate(0)?;
            self.reservation.shrink_to(0);
        } else {
            self.compact_after_read()?;
        }
        Ok(Some(DirectoryTask { path, identity }))
    }

    #[cfg(test)]
    pub(super) fn pending(&self) -> usize {
        self.pending
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        #[cfg(windows)]
        {
            self.file.as_file().set_len(len)
        }
        #[cfg(not(windows))]
        {
            self.file.set_len(len)
        }
    }

    fn compact_after_read(&mut self) -> io::Result<()> {
        let remaining = self
            .next_write
            .checked_sub(self.next_read)
            .ok_or_else(|| io::Error::other("scanner task spill offsets are out of order"))?;
        if remaining == 0 {
            return Err(io::Error::other(
                "scanner task spill has queued tasks without record data",
            ));
        }
        let compact_after = self.reservation.bytes().min(TASK_SPILL_COMPACTION_BYTES);
        if self.next_read < compact_after && self.next_read < remaining {
            return Ok(());
        }

        let buffer_len = usize::try_from(remaining.min(TASK_SPILL_COMPACTION_BYTES))
            .map_err(|_| io::Error::other("scanner task spill compaction buffer is too large"))?;
        let mut buffer = vec![0_u8; buffer_len];
        let mut source = self.next_read;
        let mut destination = 0_u64;
        let mut bytes_left = remaining;
        while bytes_left > 0 {
            let chunk =
                usize::try_from(bytes_left.min(TASK_SPILL_COMPACTION_BYTES)).map_err(|_| {
                    io::Error::other("scanner task spill compaction chunk is too large")
                })?;
            self.file.seek(SeekFrom::Start(source))?;
            self.file.read_exact(&mut buffer[..chunk])?;
            self.file.seek(SeekFrom::Start(destination))?;
            self.file.write_all(&buffer[..chunk])?;
            let chunk = u64::try_from(chunk)
                .map_err(|_| io::Error::other("scanner task spill compaction chunk overflow"))?;
            source = source
                .checked_add(chunk)
                .ok_or_else(|| io::Error::other("scanner task spill offset overflow"))?;
            destination = destination
                .checked_add(chunk)
                .ok_or_else(|| io::Error::other("scanner task spill offset overflow"))?;
            bytes_left = bytes_left
                .checked_sub(chunk)
                .ok_or_else(|| io::Error::other("scanner task spill compaction underflow"))?;
        }
        self.truncate(remaining)?;
        self.reservation.shrink_to(remaining);
        self.next_read = 0;
        self.next_write = remaining;
        Ok(())
    }
}
