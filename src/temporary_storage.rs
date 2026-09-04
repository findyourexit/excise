use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use redb::StorageBackend;

pub(crate) const DEFAULT_TEMPORARY_STORAGE_MIB: usize = 512;
pub(crate) const MIN_TEMPORARY_STORAGE_MIB: usize = 2;
const MIB: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct TemporaryStorage {
    state: Arc<TemporaryStorageState>,
}

#[derive(Debug)]
struct TemporaryStorageState {
    limit: u64,
    used: AtomicU64,
}

impl Default for TemporaryStorage {
    fn default() -> Self {
        Self::from_mib(DEFAULT_TEMPORARY_STORAGE_MIB)
            .expect("default temporary storage limit should fit in u64")
    }
}

impl TemporaryStorage {
    pub(crate) fn from_mib(mib: usize) -> io::Result<Self> {
        let bytes = u64::try_from(mib)
            .ok()
            .and_then(|mib| mib.checked_mul(MIB))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "temporary storage limit does not fit in bytes",
                )
            })?;
        Ok(Self::with_limit_bytes(bytes))
    }

    #[must_use]
    pub(crate) fn with_limit_bytes(limit: u64) -> Self {
        Self {
            state: Arc::new(TemporaryStorageState {
                limit,
                used: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) fn reserve(&self, bytes: u64) -> io::Result<()> {
        let mut used = self.state.used.load(Ordering::Acquire);
        loop {
            let required = used.checked_add(bytes).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::StorageFull,
                    format!(
                        "temporary storage capacity exhausted: more than {} bytes are required; increase --temporary-storage-mib",
                        self.state.limit
                    ),
                )
            })?;
            if required > self.state.limit {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    format!(
                        "temporary storage capacity exhausted: {required} bytes exceed the {} byte session limit; increase --temporary-storage-mib",
                        self.state.limit
                    ),
                ));
            }
            match self.state.used.compare_exchange_weak(
                used,
                required,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(current) => used = current,
            }
        }
    }

    pub(crate) fn reservation(&self, bytes: u64) -> io::Result<TemporaryStorageReservation> {
        self.reserve(bytes)?;
        Ok(TemporaryStorageReservation {
            storage: self.clone(),
            bytes,
        })
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn used(&self) -> u64 {
        self.state.used.load(Ordering::Acquire)
    }

    fn release(&self, bytes: u64) {
        let mut used = self.state.used.load(Ordering::Acquire);
        loop {
            let Some(remaining) = used.checked_sub(bytes) else {
                panic!("temporary storage accounting underflow");
            };
            match self.state.used.compare_exchange_weak(
                used,
                remaining,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => used = current,
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct TemporaryStorageReservation {
    storage: TemporaryStorage,
    bytes: u64,
}

impl TemporaryStorageReservation {
    #[must_use]
    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn grow_to(&mut self, bytes: u64) -> io::Result<()> {
        if bytes <= self.bytes {
            return Ok(());
        }
        self.storage.reserve(bytes - self.bytes)?;
        self.bytes = bytes;
        Ok(())
    }

    pub(crate) fn shrink_to(&mut self, bytes: u64) {
        if bytes >= self.bytes {
            return;
        }
        self.storage.release(self.bytes - bytes);
        self.bytes = bytes;
    }
}

impl Drop for TemporaryStorageReservation {
    fn drop(&mut self) {
        self.storage.release(self.bytes);
    }
}

#[derive(Debug)]
pub(crate) struct BoundedFileBackend {
    file: Mutex<BoundedFile>,
    reservation: Arc<Mutex<TemporaryStorageReservation>>,
    capacity_exhausted: Arc<AtomicBool>,
}

#[derive(Debug)]
struct BoundedFile {
    file: File,
    length: u64,
}

impl BoundedFileBackend {
    pub(crate) fn new(
        file: File,
        reservation: Arc<Mutex<TemporaryStorageReservation>>,
        capacity_exhausted: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let length = file.metadata()?.len();
        {
            let mut reservation = reservation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if reservation.bytes() != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "temporary storage database reservation was not empty",
                ));
            }
            if let Err(error) = reservation.grow_to(length) {
                if error.kind() == io::ErrorKind::StorageFull {
                    capacity_exhausted.store(true, Ordering::Release);
                }
                return Err(error);
            }
        }
        Ok(Self {
            file: Mutex::new(BoundedFile { file, length }),
            reservation,
            capacity_exhausted,
        })
    }

    fn note_capacity_error(&self, error: &io::Error) {
        if error.kind() == io::ErrorKind::StorageFull {
            self.capacity_exhausted.store(true, Ordering::Release);
        }
    }
}

impl StorageBackend for BoundedFileBackend {
    fn len(&self) -> io::Result<u64> {
        let file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(file.length)
    }

    fn read(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let end = offset
            .checked_add(u64::try_from(len).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "temporary storage read is too large",
                )
            })?)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "temporary storage read overflows",
                )
            })?;
        if end > file.length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "temporary storage read exceeds the bounded file",
            ));
        }
        let mut bytes = vec![0; len];
        file.file.seek(SeekFrom::Start(offset))?;
        file.file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        verify_length(&file)?;
        let mut reservation = self
            .reservation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if reservation.bytes() != file.length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "temporary storage database reservation does not match its file",
            ));
        }
        if len > file.length {
            if let Err(error) = reservation.grow_to(len) {
                self.note_capacity_error(&error);
                return Err(error);
            }
            if let Err(error) = file.file.set_len(len) {
                reservation.shrink_to(file.length);
                return Err(error);
            }
        } else if len < file.length {
            file.file.set_len(len)?;
            reservation.shrink_to(len);
        }
        file.length = len;
        Ok(())
    }

    fn sync_data(&self, _eventual: bool) -> io::Result<()> {
        let file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        file.file.sync_data()
    }

    fn write(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let end = offset
            .checked_add(u64::try_from(bytes.len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "temporary storage write is too large",
                )
            })?)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "temporary storage write overflows",
                )
            })?;
        if end > file.length {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "temporary storage write exceeds the reserved file length",
            ));
        }
        file.file.seek(SeekFrom::Start(offset))?;
        file.file.write_all(bytes)
    }
}

fn verify_length(file: &BoundedFile) -> io::Result<()> {
    if file.file.metadata()?.len() != file.length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "temporary storage file changed outside its accounting boundary",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_backend_signals_identity_capacity_exhaustion() {
        let storage = TemporaryStorage::with_limit_bytes(0);
        let capacity_exhausted = Arc::new(AtomicBool::new(false));
        let backend = BoundedFileBackend::new(
            tempfile::tempfile().expect("temporary database file should open"),
            Arc::new(Mutex::new(
                storage
                    .reservation(0)
                    .expect("empty database reservation should fit"),
            )),
            Arc::clone(&capacity_exhausted),
        )
        .expect("bounded database backend should initialize");

        let error = backend
            .set_len(1)
            .expect_err("database growth beyond its shared storage limit should fail");
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert!(capacity_exhausted.load(Ordering::Acquire));
    }

    #[test]
    fn reservations_enforce_the_shared_limit_and_release_on_drop() {
        let storage = TemporaryStorage::with_limit_bytes(8);
        let first = storage
            .reservation(5)
            .expect("first reservation should fit");
        assert_eq!(storage.used(), 5);
        let error = storage
            .reservation(4)
            .expect_err("reservation beyond the total limit should fail");
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert!(error.to_string().contains("--temporary-storage-mib"));
        drop(first);
        assert_eq!(storage.used(), 0);
    }
}
