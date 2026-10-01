//! Peak size of a directory, sampled repeatedly.
//!
//! The scan store writes under `EXCISE_SCAN_STORE_DIR`, a directory the harness creates empty and
//! owns for the run. [`StoreSampler`] walks it with ordinary, safe `std::fs` calls (no `unsafe`,
//! no platform-specific API) and keeps the largest total apparent size seen across every sample,
//! the same shape as [`super::ProcessSampler`]'s peaks.
//!
//! A sample races a live writer: the scan store renames and removes files while a scan runs, so a
//! walk can meet a name that is gone by the time it is inspected. That is not a failure. The
//! vanished entry contributes nothing to that one sample, which undercounts it slightly instead of
//! poisoning the peak; the next sample, a few milliseconds later, sees the settled state.

use std::{fs, path::Path};

/// The peak total size seen over a series of samples of one directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreSampler {
    peak_bytes: Option<u64>,
}

impl StoreSampler {
    /// Walks `path` and folds its total apparent size into the peak.
    ///
    /// A directory that does not exist, or that a race empties mid-walk, samples as zero; zero
    /// never lowers an earlier peak.
    pub fn sample(&mut self, path: &Path) {
        let bytes = directory_bytes(path);
        self.peak_bytes = Some(self.peak_bytes.map_or(bytes, |peak| peak.max(bytes)));
    }

    /// The largest total seen in any one sample, or `None` before the first sample.
    #[must_use]
    pub const fn peak_bytes(&self) -> Option<u64> {
        self.peak_bytes
    }
}

/// The sum of the apparent size of every regular file at or below `path`.
///
/// A symbolic link counts as zero and is not followed. A path that cannot be inspected (gone, or
/// unreadable) contributes zero rather than failing the walk, so one vanished entry never loses
/// the rest of a sample.
fn directory_bytes(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.is_file() {
        return metadata.len();
    }
    if !metadata.is_dir() {
        return 0;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| directory_bytes(&entry.path()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_directory_samples_as_zero() {
        let mut sampler = StoreSampler::default();

        sampler.sample(Path::new("/does/not/exist/anywhere"));

        assert_eq!(sampler.peak_bytes(), Some(0));
    }

    #[test]
    fn nested_files_are_summed() {
        let root = tempfile::tempdir().expect("a temp dir");
        fs::write(root.path().join("a.txt"), [0_u8; 10]).expect("write a.txt");
        fs::create_dir(root.path().join("sub")).expect("create sub");
        fs::write(root.path().join("sub/b.txt"), [0_u8; 25]).expect("write sub/b.txt");
        let mut sampler = StoreSampler::default();

        sampler.sample(root.path());

        assert_eq!(sampler.peak_bytes(), Some(35));
    }

    #[test]
    fn peaks_only_ever_grow() {
        let root = tempfile::tempdir().expect("a temp dir");
        let mut sampler = StoreSampler::default();

        sampler.sample(root.path());
        assert_eq!(sampler.peak_bytes(), Some(0));

        fs::write(root.path().join("grown.bin"), [0_u8; 100]).expect("write grown.bin");
        sampler.sample(root.path());
        assert_eq!(sampler.peak_bytes(), Some(100));

        fs::remove_file(root.path().join("grown.bin")).expect("remove grown.bin");
        sampler.sample(root.path());
        assert_eq!(
            sampler.peak_bytes(),
            Some(100),
            "a later, smaller sample never lowers the peak"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_is_not_followed() {
        let outside = tempfile::tempdir().expect("a temp dir");
        fs::write(outside.path().join("target.bin"), [0_u8; 999]).expect("write target.bin");
        let root = tempfile::tempdir().expect("a temp dir");
        std::os::unix::fs::symlink(outside.path().join("target.bin"), root.path().join("link"))
            .expect("create link");
        let mut sampler = StoreSampler::default();

        sampler.sample(root.path());

        assert_eq!(
            sampler.peak_bytes(),
            Some(0),
            "a symlink is not followed, so its target outside the sampled directory never counts"
        );
    }
}
