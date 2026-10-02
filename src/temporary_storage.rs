use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(not(unix))]
use sysinfo::Disks;

use redb::StorageBackend;

pub(crate) const DEFAULT_TEMPORARY_STORAGE_MIB: usize = 4_096;
pub(crate) const MIN_SCAN_STORE_MIB: usize = 2;
pub(crate) const MIN_TEMPORARY_STORAGE_MIB: usize = 2;
const MIB: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct TemporaryStorage {
    state: Arc<TemporaryStorageState>,
}

#[derive(Debug)]
struct TemporaryStorageState {
    limit: u64,
    capacity_name: &'static str,
    increase_flag: &'static str,
    used: AtomicU64,
    #[cfg(feature = "internal")]
    peak_used: AtomicU64,
}

impl Default for TemporaryStorage {
    fn default() -> Self {
        Self::from_mib(DEFAULT_TEMPORARY_STORAGE_MIB)
            .expect("default temporary storage limit should fit in u64")
    }
}

impl TemporaryStorage {
    pub(crate) fn from_mib(mib: usize) -> io::Result<Self> {
        Self::from_mib_named(mib, "temporary storage", "--temporary-storage-mib")
    }

    #[cfg(any(test, feature = "internal"))]
    pub(crate) fn scan_store_from_mib(mib: usize) -> io::Result<Self> {
        Self::from_mib_named(mib, "scan store", "--scan-store-mib")
    }

    /// Bounds an explicit scan-store budget by the safe free capacity on the
    /// volume that hosts its private run files. An absent budget uses the same
    /// volume's safe capacity directly. An optional reserve overrides the
    /// default quarter-free-space reserve without consuming the minimum usable
    /// scan-store capacity.
    pub(crate) fn scan_store_from_mib_for_scratch(
        mib: Option<usize>,
        reserve_mib: Option<usize>,
        scratch: &Path,
    ) -> io::Result<Self> {
        let requested = mib.map(|mib| mib_to_bytes(mib, "scan store")).transpose()?;
        let reserve = reserve_mib
            .map(|mib| mib_to_bytes(mib, "scan-store reserve"))
            .transpose()?;
        let available = scratch_volume_available_bytes(scratch)?;
        Ok(Self::with_limit_bytes_named(
            scan_store_limit_bytes(requested, reserve, available),
            "scan store",
            "--scan-store-mib",
        ))
    }

    fn from_mib_named(
        mib: usize,
        capacity_name: &'static str,
        increase_flag: &'static str,
    ) -> io::Result<Self> {
        let bytes = mib_to_bytes(mib, capacity_name)?;
        Ok(Self::with_limit_bytes_named(
            bytes,
            capacity_name,
            increase_flag,
        ))
    }
}

fn mib_to_bytes(mib: usize, capacity_name: &str) -> io::Result<u64> {
    u64::try_from(mib)
        .ok()
        .and_then(|mib| mib.checked_mul(MIB))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{capacity_name} limit does not fit in bytes"),
            )
        })
}

const SCAN_STORE_FREE_SPACE_RESERVE_DIVISOR: u64 = 4;
const MIN_SCAN_STORE_BYTES: u64 = MIB * 2;

const fn scan_store_limit_bytes(
    requested: Option<u64>,
    reserve: Option<u64>,
    available: u64,
) -> u64 {
    let desired_reserve = match reserve {
        Some(reserve) => reserve,
        None => available / SCAN_STORE_FREE_SPACE_RESERVE_DIVISOR,
    };
    let maximum_reserve = available.saturating_sub(MIN_SCAN_STORE_BYTES);
    let effective_reserve = if desired_reserve < maximum_reserve {
        desired_reserve
    } else {
        maximum_reserve
    };
    let safe_available = available.saturating_sub(effective_reserve);
    match requested {
        Some(requested) if requested < safe_available => requested,
        Some(_) | None => safe_available,
    }
}

#[cfg(unix)]
fn scratch_volume_available_bytes(scratch: &Path) -> io::Result<u64> {
    let statistics = rustix::fs::statvfs(scratch).map_err(io::Error::from)?;
    available_bytes(statistics.f_bavail, statistics.f_frsize, statistics.f_bsize)
}

/// The pure arithmetic behind [`scratch_volume_available_bytes`] on Unix: POSIX defines
/// `f_bavail` in `f_frsize` units (`statvfs(3)`), the volume's fundamental block size, not
/// `f_bsize` (its preferred I/O size). The two can differ by orders of magnitude: one measured
/// APFS volume reported `f_bsize = 1_048_576` (1 MiB) against `f_frsize = 4_096` (4 KiB), so
/// multiplying by the larger of the two overstated real free space by about 256×.
///
/// `f_frsize == 0` would multiply the quota down to nothing rather than up, but still deserves
/// a guard: Linux's `statvfs` wrapper already substitutes `f_bsize` for a zero raw `f_frsize`
/// before this code ever sees it, for kernels whose `statfs(2)` predates the field (rustix
/// 1.1.5 `src/backend/linux_raw/fs/syscalls.rs:938-960`). macOS's `statvfs(3)` sets `f_frsize`
/// straight from `statfs(2)`'s block size with no fallback of its own
/// (`apple-oss-distributions/Libc` `emulated/statvfs.c`: `to->f_frsize = from->f_bsize`), so a
/// filesystem driver that reports a zero block size would reach us unless we substitute here
/// too.
#[cfg(unix)]
fn available_bytes(f_bavail: u64, f_frsize: u64, f_bsize: u64) -> io::Result<u64> {
    let block_size = if f_frsize == 0 { f_bsize } else { f_frsize };
    f_bavail.checked_mul(block_size).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "scratch volume available-space calculation overflowed",
        )
    })
}

#[cfg(not(unix))]
fn scratch_volume_available_bytes(scratch: &Path) -> io::Result<u64> {
    let scratch = std::fs::canonicalize(scratch)?;
    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter_map(|disk| {
            std::fs::canonicalize(disk.mount_point())
                .ok()
                .filter(|mount| scratch.starts_with(mount))
                .map(|mount| (disk, mount))
        })
        .max_by_key(|(_, mount)| mount.components().count())
        .map(|(disk, _)| disk.available_space())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "could not identify the filesystem holding scan-store scratch storage",
            )
        })
}

impl TemporaryStorage {
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_limit_bytes(limit: u64) -> Self {
        Self::with_limit_bytes_named(limit, "temporary storage", "--temporary-storage-mib")
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn scan_store_with_limit_bytes(limit: u64) -> Self {
        Self::with_limit_bytes_named(limit, "scan store", "--scan-store-mib")
    }

    fn with_limit_bytes_named(
        limit: u64,
        capacity_name: &'static str,
        increase_flag: &'static str,
    ) -> Self {
        Self {
            state: Arc::new(TemporaryStorageState {
                limit,
                capacity_name,
                increase_flag,
                used: AtomicU64::new(0),
                #[cfg(feature = "internal")]
                peak_used: AtomicU64::new(0),
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
                        "{} capacity exhausted: more than {} bytes are required; increase {}",
                        self.state.capacity_name, self.state.limit, self.state.increase_flag,
                    ),
                )
            })?;
            if required > self.state.limit {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    format!(
                        "{} capacity exhausted: {required} bytes exceed the {} byte session limit; increase {}",
                        self.state.capacity_name, self.state.limit, self.state.increase_flag,
                    ),
                ));
            }
            match self.state.used.compare_exchange_weak(
                used,
                required,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    #[cfg(feature = "internal")]
                    self.state.peak_used.fetch_max(required, Ordering::AcqRel);
                    return Ok(());
                }
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

    #[must_use]
    pub(crate) fn used(&self) -> u64 {
        self.state.used.load(Ordering::Acquire)
    }

    #[cfg(feature = "internal")]
    #[must_use]
    pub(crate) fn peak_used(&self) -> u64 {
        self.state.peak_used.load(Ordering::Acquire)
    }

    #[must_use]
    pub(crate) fn limit(&self) -> u64 {
        self.state.limit
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

    fn read(&self, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let end = offset
            .checked_add(u64::try_from(bytes.len()).map_err(|_| {
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
        file.file.seek(SeekFrom::Start(offset))?;
        file.file.read_exact(bytes)
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

    fn sync_data(&self) -> io::Result<()> {
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
    fn default_temporary_storage_reserves_four_gib() {
        assert_eq!(DEFAULT_TEMPORARY_STORAGE_MIB, 4_096);
        let storage = TemporaryStorage::default();
        let bytes = u64::try_from(DEFAULT_TEMPORARY_STORAGE_MIB)
            .expect("default temporary-storage MiB should fit")
            .saturating_mul(MIB);
        let reservation = storage
            .reservation(bytes)
            .expect("the full default temporary-storage budget should fit");
        assert_eq!(storage.used(), bytes);
        assert!(storage.reserve(1).is_err());
        drop(reservation);
        assert_eq!(storage.used(), 0);
    }

    #[cfg(feature = "internal")]
    #[test]
    fn peak_usage_survives_reservation_release() {
        let storage = TemporaryStorage::with_limit_bytes(8);
        let first = storage
            .reservation(5)
            .expect("first reservation should fit");
        let second = storage
            .reservation(2)
            .expect("second reservation should fit");
        assert_eq!(storage.peak_used(), 7);
        drop(second);
        drop(first);
        assert_eq!(storage.used(), 0);
        assert_eq!(storage.peak_used(), 7);
    }

    #[test]
    fn scan_store_quota_uses_safe_scratch_capacity() {
        assert_eq!(scan_store_limit_bytes(None, None, 16 * MIB), 12 * MIB);
        assert_eq!(
            scan_store_limit_bytes(Some(14 * MIB), None, 16 * MIB),
            12 * MIB
        );
        assert_eq!(
            scan_store_limit_bytes(Some(2 * MIB), None, 16 * MIB),
            2 * MIB
        );
        assert_eq!(
            scan_store_limit_bytes(None, Some(8 * MIB), 16 * MIB),
            8 * MIB
        );
        assert_eq!(
            scan_store_limit_bytes(None, Some(64 * MIB), 16 * MIB),
            2 * MIB
        );
    }
    #[test]
    fn scan_store_scratch_volume_is_discoverable() {
        let scratch = tempfile::tempdir().expect("scratch directory should exist");
        scratch_volume_available_bytes(scratch.path())
            .expect("scratch directory should resolve to a mounted volume");
    }

    /// Injects the macOS statvfs values from a measured 57.1 GiB-free APFS volume
    /// (`f_bsize = 1_048_576`, `f_frsize = 4_096`, `f_bavail = 14_979_245`) and asserts the
    /// POSIX-correct product, `f_bavail × f_frsize`, not `f_bavail × f_bsize` (which would
    /// overstate free space by about 256×).
    #[cfg(unix)]
    #[test]
    fn scratch_volume_available_bytes_uses_frsize_when_block_sizes_differ() {
        let result = available_bytes(14_979_245, 4_096, 1_048_576)
            .expect("the injected statvfs values should multiply without overflow");
        assert_eq!(result, 14_979_245 * 4_096);
    }

    /// When `f_bsize == f_frsize` (true of most non-APFS volumes), the choice between them
    /// does not matter.
    #[cfg(unix)]
    #[test]
    fn scratch_volume_available_bytes_is_correct_when_block_sizes_already_agree() {
        assert_eq!(
            available_bytes(1_946, 4_096, 4_096).expect("matching block sizes should not overflow"),
            1_946 * 4_096
        );
    }

    /// A filesystem driver that reports no fundamental block size at all falls back to the
    /// preferred I/O size, the same substitution Linux's `statvfs` wrapper already makes for a
    /// raw zero `f_frsize` (see `available_bytes`'s doc comment).
    #[cfg(unix)]
    #[test]
    fn scratch_volume_available_bytes_falls_back_to_bsize_when_frsize_is_zero() {
        assert_eq!(
            available_bytes(14_979_245, 0, 1_048_576)
                .expect("a zero frsize should fall back to bsize without overflow"),
            14_979_245 * 1_048_576
        );
    }

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
    fn bounded_backend_reads_into_caller_buffer_within_reserved_length() {
        let storage = TemporaryStorage::with_limit_bytes(3);
        let reservation = Arc::new(Mutex::new(
            storage
                .reservation(0)
                .expect("empty database reservation should fit"),
        ));
        let backend = BoundedFileBackend::new(
            tempfile::tempfile().expect("temporary database file should open"),
            Arc::clone(&reservation),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("bounded database backend should initialize");
        backend
            .set_len(3)
            .expect("database growth within its shared storage limit should succeed");
        backend
            .write(0, b"red")
            .expect("bounded database backend should write reserved bytes");

        let mut bytes = [0; 3];
        backend
            .read(0, &mut bytes)
            .expect("bounded database backend should fill the caller buffer");
        assert_eq!(&bytes, b"red");
        let error = backend
            .read(1, &mut bytes)
            .expect_err("read beyond the reserved file length should fail");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(storage.used(), 3);

        drop(backend);
        drop(reservation);
        assert_eq!(storage.used(), 0);
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
