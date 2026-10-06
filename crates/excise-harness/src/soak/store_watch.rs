//! The scan store's peak size, sampled away from the thread that reads the program.
//!
//! The scan store is a directory the program writes under `EXCISE_SCAN_STORE_DIR`, and its size is
//! a walk of it ([`StoreSampler`]). The thread that drives a session also stamps each frame event
//! with the time it reads the event, ties the screen to the frame, and looks for the person's
//! interrupt, so a walk on that thread, however long, would inflate the first-frame time, every
//! input-to-frame latency, and the longest gap between frames, and would delay an interrupt. The
//! walk runs on a thread of its own instead. The driving thread never waits for it: it reads the
//! peak once, when the session is over, and a walk still in progress then is left to end by itself
//! (its next look at a directory that is gone finds nothing).

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        mpsc::{self, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

use crate::metrics::StoreSampler;

/// A thread that samples one directory's size, at once and then every `every`, until it is told to
/// stop or its owner is gone.
pub(super) struct StoreWatch {
    stop: Option<Sender<()>>,
    peak: Arc<Mutex<Option<u64>>>,
}

impl StoreWatch {
    /// Starts sampling `dir`. A thread that cannot be started leaves the peak unknown: the metric
    /// is then absent, and nothing else is affected.
    pub(super) fn start(dir: PathBuf, every: Duration) -> Self {
        let (stop, stopped) = mpsc::channel::<()>();
        let peak = Arc::new(Mutex::new(None));
        let published = Arc::clone(&peak);
        let spawned = thread::Builder::new()
            .name("soak-store-watch".to_owned())
            .spawn(move || {
                let mut sampler = StoreSampler::default();
                loop {
                    sampler.sample(&dir);
                    *published.lock().unwrap_or_else(PoisonError::into_inner) =
                        sampler.peak_bytes();
                    if !matches!(stopped.recv_timeout(every), Err(RecvTimeoutError::Timeout)) {
                        return;
                    }
                }
            });
        Self {
            stop: spawned.is_ok().then_some(stop),
            peak,
        }
    }

    /// The largest size sampled so far, or `None` before the first sample.
    pub(super) fn peak(&self) -> Option<u64> {
        *self.peak.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Tells the thread to stop and returns the largest size it sampled. It does not wait for the
    /// thread, so a walk that is still going on costs the caller nothing.
    pub(super) fn finish(&mut self) -> Option<u64> {
        drop(self.stop.take());
        self.peak()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{Duration, Instant},
    };

    use super::*;

    fn wait_for_peak(watch: &StoreWatch, expected: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while watch.peak() != Some(expected) {
            assert!(
                Instant::now() < deadline,
                "the peak is {:?}, not {expected}",
                watch.peak()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_peak_is_the_largest_size_sampled_and_survives_the_store_shrinking_or_going() {
        let dir = tempfile::tempdir().expect("a directory");
        let file = dir.path().join("store.db");
        fs::write(&file, vec![0_u8; 1000]).expect("a file");
        let mut watch = StoreWatch::start(dir.path().to_path_buf(), Duration::from_millis(5));

        wait_for_peak(&watch, 1000);
        fs::write(&file, vec![0_u8; 5000]).expect("a bigger file");
        wait_for_peak(&watch, 5000);
        fs::write(&file, vec![0_u8; 10]).expect("a smaller file");
        thread::sleep(Duration::from_millis(50));
        assert_eq!(watch.peak(), Some(5000), "a smaller sample never lowers it");

        drop(dir);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(watch.finish(), Some(5000), "nor does a store that is gone");
    }

    #[test]
    fn finishing_does_not_wait_for_the_next_sample() {
        let dir = tempfile::tempdir().expect("a directory");
        fs::write(dir.path().join("store.db"), vec![0_u8; 42]).expect("a file");
        let mut watch = StoreWatch::start(dir.path().to_path_buf(), Duration::from_secs(3600));
        wait_for_peak(&watch, 42);

        let started = Instant::now();
        let peak = watch.finish();

        assert_eq!(peak, Some(42));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "finish waited for the hour between samples"
        );
    }
}
