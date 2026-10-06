//! The bounds a soak runs under.
//!
//! The whole run is bounded, and so is every phase of it, and nothing waits without a deadline. A
//! phase that passes its bound ends its program with the program's whole process group and goes
//! on to the next; the bound on the whole run ends everything and says the run was interrupted.
//! A phase is never allowed longer than what is left of the run.

use std::time::Duration;

/// How long a whole soak may take when nothing else is said: thirty minutes.
pub const DEFAULT_RUN_LIMIT: Duration = Duration::from_mins(30);

/// How large a headless scan's report may grow in the scratch area when nothing else is said:
/// 16 GiB, which is some forty million entries and far more than any tree this is made for. A
/// scan that writes more is killed.
pub const DEFAULT_REPORT_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// The bounds of one soak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The whole run. When it passes, the run stops where it is and counts as interrupted.
    pub run: Duration,
    /// One headless scan, from the spawn to its exit.
    pub headless_scan: Duration,
    /// How large the report of one headless scan may grow in the scratch area. Every entry of a
    /// report carries its whole path twice, so a tree of very deep folders makes it grow with the
    /// square of the depth, and the soak keeps nothing of it but three counts. A scan whose report
    /// is larger is killed with its process group and the soak says so; the soak uses a quarter of
    /// the free space where the scratch area is instead when that is less.
    pub report_bytes: u64,
    /// A session, from the spawn to its first frame.
    pub first_frame: Duration,
    /// A session's scan, from its first frame to the end of the scan on the screen (the
    /// `scan_complete` event, and a header badge that says it ended). The keys sent while it
    /// runs are inside this bound, not before it.
    pub scan: Duration,
    /// One key, from its write to the frame that counts it.
    pub key: Duration,
    /// Opening a folder (the largest entry, or the first one the arrow keys reach), and going back
    /// up: each of the two phases may take this long, and a phase of its own is what it times out
    /// as.
    pub drill: Duration,
    /// The quit, all of it: the prompt and the program's exit after the confirmation share this
    /// bound.
    pub quit: Duration,
}

impl Limits {
    /// The bounds of a run that may take `run` altogether. A scan may use all of it, since a real
    /// tree takes as long as it takes, and every other phase has a bound of its own that is far
    /// shorter than any scan.
    #[must_use]
    pub fn for_run(run: Duration) -> Self {
        Self {
            run,
            headless_scan: run,
            report_bytes: DEFAULT_REPORT_BYTES,
            first_frame: Duration::from_secs(60),
            scan: run,
            key: Duration::from_secs(10),
            drill: Duration::from_secs(60),
            quit: Duration::from_secs(30),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::for_run(DEFAULT_RUN_LIMIT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_run_is_thirty_minutes_and_a_scan_may_use_all_of_it() {
        let limits = Limits::default();

        assert_eq!(limits.run, Duration::from_mins(30));
        assert_eq!(limits.report_bytes, DEFAULT_REPORT_BYTES);
        assert_eq!(limits.headless_scan, limits.run);
        assert_eq!(limits.scan, limits.run);
        assert!(limits.first_frame < limits.run);
        assert!(limits.key < limits.drill && limits.drill < limits.run);
    }
}
