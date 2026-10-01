//! Per-process resource sampling.
//!
//! The workspace forbids `unsafe`, so the numbers come from safe wrappers:
//!
//! * **macOS** uses the `libproc` crate. `proc_pid_rusage` reports the lifetime peak of the
//!   *physical footprint* (`ri_lifetime_max_phys_footprint`), the figure Activity Monitor and the
//!   memory budget call peak memory, and `proc_pidinfo` reports the thread and descriptor counts.
//! * **Linux** reads `/proc/<pid>/status` (`VmHWM` is the peak resident set size, `Threads` the
//!   thread count) and counts the entries of `/proc/<pid>/fd`.
//! * Other platforms report nothing. In particular Windows has no safe wrapper in this workspace,
//!   so a Windows run has no memory, thread, or descriptor figures.
//! * **A live CPU sample** (`live_cpu_ms`) answers for a process that is still running, so a step
//!   can isolate one window instead of a whole process life. On macOS the task's accounted CPU
//!   time comes back in the Mach timebase's raw ticks, not nanoseconds (about 42 ns each on
//!   Apple Silicon; a plain cast would under-report by that factor). Linux's equivalent is clock
//!   ticks (`/proc/<pid>/stat`, divided by `sysconf(_SC_CLK_TCK)`, commonly 100 Hz). Neither
//!   conversion has a safe wrapper of its own in this workspace, so `live_cpu_ms` reuses
//!   `sysinfo`'s existing, tested one (`Process::accumulated_cpu_time`) instead of a second,
//!   hand-rolled copy.
//!
//! The sampler reads one process by pid, so it measures that child alone. `RUSAGE_CHILDREN` cannot:
//! its `ru_maxrss` is a high-water mark over every child the harness ever reaped.
//!
//! A sample is only possible while the process exists. A zombie that has not been reaped still
//! answers on macOS, which is why the session samples an exited child before it reaps it.

/// The peaks seen over a series of samples of one process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessSampler {
    peak_memory_bytes: Option<u64>,
    max_threads: Option<u32>,
    max_fds: Option<u32>,
    samples: u32,
}

/// One reading of a process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Reading {
    peak_memory_bytes: Option<u64>,
    threads: Option<u32>,
    fds: Option<u32>,
}

impl ProcessSampler {
    /// Reads process `pid` and folds the reading into the peaks. A process that cannot be read is
    /// skipped.
    pub fn sample(&mut self, pid: u32) {
        if let Some(reading) = read(pid) {
            self.fold(reading);
        }
    }

    fn fold(&mut self, reading: Reading) {
        self.samples += 1;
        self.peak_memory_bytes = self.peak_memory_bytes.max(reading.peak_memory_bytes);
        self.max_threads = self.max_threads.max(reading.threads);
        self.max_fds = self.max_fds.max(reading.fds);
    }

    /// The peak memory in bytes: the peak physical footprint on macOS, the peak resident set size
    /// on Linux.
    #[must_use]
    pub const fn peak_memory_bytes(&self) -> Option<u64> {
        self.peak_memory_bytes
    }

    /// The most threads seen in one sample.
    #[must_use]
    pub const fn max_threads(&self) -> Option<u32> {
        self.max_threads
    }

    /// The most open descriptors seen in one sample.
    #[must_use]
    pub const fn max_fds(&self) -> Option<u32> {
        self.max_fds
    }

    /// How many samples succeeded.
    #[must_use]
    pub const fn samples(&self) -> u32 {
        self.samples
    }
}

/// The fields of `/proc/<pid>/status` that the sampler uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct ProcStatus {
    peak_resident_kib: Option<u64>,
    threads: Option<u32>,
}

/// Parses the text of `/proc/<pid>/status`.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_proc_status(text: &str) -> ProcStatus {
    let mut status = ProcStatus::default();
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name {
            "VmHWM" => {
                status.peak_resident_kib = value
                    .strip_suffix("kB")
                    .and_then(|kib| kib.trim().parse().ok());
            }
            "Threads" => status.threads = value.parse().ok(),
            _ => {}
        }
    }
    status
}

#[cfg(target_os = "macos")]
fn read(pid: u32) -> Option<Reading> {
    use libproc::libproc::{
        file_info::ListFDs,
        pid_rusage::{RUsageInfoV4, pidrusage},
        proc_pid::{listpidinfo, pidinfo},
        task_info::TaskAllInfo,
    };

    let pid = i32::try_from(pid).ok()?;
    let usage = pidrusage::<RUsageInfoV4>(pid).ok()?;
    let mut reading = Reading {
        peak_memory_bytes: Some(usage.ri_lifetime_max_phys_footprint),
        ..Reading::default()
    };
    // A zombie has its rusage but no task, so the counts are optional.
    if let Ok(info) = pidinfo::<TaskAllInfo>(pid, 0) {
        reading.threads = u32::try_from(info.ptinfo.pti_threadnum).ok();
        let capacity = usize::try_from(info.pbsd.pbi_nfiles).unwrap_or(0);
        reading.fds = listpidinfo::<ListFDs>(pid, capacity)
            .ok()
            .and_then(|fds| u32::try_from(fds.len()).ok());
    }
    Some(reading)
}

#[cfg(target_os = "linux")]
fn read(pid: u32) -> Option<Reading> {
    let status = parse_proc_status(&std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?);
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .ok()
        .and_then(|entries| u32::try_from(entries.count()).ok());
    Some(Reading {
        peak_memory_bytes: status.peak_resident_kib.map(|kib| kib * 1024),
        threads: status.threads,
        fds,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read(_pid: u32) -> Option<Reading> {
    None
}

/// The live CPU time `pid` has used since it started, in milliseconds (user and system
/// combined), or `None` where the process cannot be found. Unlike [`ProcessSampler`], this
/// answers once for a process that is still running: a caller samples it at the start and the
/// end of a window and takes the difference, which `RUSAGE_CHILDREN` cannot do (it is a
/// high-water mark over the child's whole life, read only after it is reaped).
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[must_use]
pub fn live_cpu_ms(pid: u32) -> Option<f64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    let target = Pid::from_u32(pid);
    let mut system = System::new();
    if system.refresh_processes_specifics(
        ProcessesToUpdate::Some(std::slice::from_ref(&target)),
        false,
        ProcessRefreshKind::nothing().with_cpu(),
    ) == 0
    {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    system
        .process(target)
        .map(|process| process.accumulated_cpu_time() as f64)
}

/// The live CPU time of `pid`. Windows has no safe wrapper in this workspace.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[must_use]
pub fn live_cpu_ms(_pid: u32) -> Option<f64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_parser_reads_the_peak_and_the_thread_count() {
        let text = "Name:\texcise\nVmPeak:\t  212880 kB\nVmHWM:\t   61440 kB\nVmRSS:\t   40000 kB\nThreads:\t4\n";

        assert_eq!(
            parse_proc_status(text),
            ProcStatus {
                peak_resident_kib: Some(61_440),
                threads: Some(4)
            }
        );
    }

    #[test]
    fn a_status_without_the_fields_reads_as_unknown() {
        // A kernel thread or a zombie has no `VmHWM`.
        assert_eq!(
            parse_proc_status("Name:\tzombie\nState:\tZ (zombie)\n"),
            ProcStatus::default()
        );
        assert_eq!(
            parse_proc_status("VmHWM:\tgarbage\nThreads:\t-\n"),
            ProcStatus::default()
        );
    }

    #[test]
    fn peaks_only_ever_grow() {
        let mut sampler = ProcessSampler::default();
        for (bytes, threads, fds) in [(100, 3, 10), (500, 2, 30), (300, 5, 20)] {
            sampler.fold(Reading {
                peak_memory_bytes: Some(bytes),
                threads: Some(threads),
                fds: Some(fds),
            });
        }

        assert_eq!(sampler.peak_memory_bytes(), Some(500));
        assert_eq!(sampler.max_threads(), Some(5));
        assert_eq!(sampler.max_fds(), Some(30));
        assert_eq!(sampler.samples(), 3);
    }

    #[test]
    fn a_reading_with_missing_fields_keeps_the_known_peaks() {
        let mut sampler = ProcessSampler::default();
        sampler.fold(Reading {
            peak_memory_bytes: Some(700),
            threads: Some(4),
            fds: None,
        });
        sampler.fold(Reading::default());

        assert_eq!(sampler.peak_memory_bytes(), Some(700));
        assert_eq!(sampler.max_fds(), None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_running_test_process_can_be_sampled() {
        let mut sampler = ProcessSampler::default();

        sampler.sample(std::process::id());

        assert_eq!(sampler.samples(), 1);
        assert!(
            sampler
                .peak_memory_bytes()
                .is_some_and(|bytes| bytes > 100_000)
        );
        assert!(sampler.max_threads().is_some_and(|threads| threads >= 1));
        assert!(sampler.max_fds().is_some_and(|fds| fds >= 3));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn live_cpu_ms_reads_the_running_test_process_and_grows_with_real_work() {
        let pid = std::process::id();
        let before = live_cpu_ms(pid).expect("a live sample of the running test process");

        // Burn CPU on this thread for a bit so the accumulated total visibly moves.
        let start = std::time::Instant::now();
        let mut acc: u64 = 0;
        while start.elapsed() < std::time::Duration::from_millis(200) {
            acc = acc.wrapping_add(1);
        }
        std::hint::black_box(acc);

        let after = live_cpu_ms(pid).expect("a second live sample");
        assert!(
            after > before,
            "200 ms of real CPU-bound work should move the accumulated total: {before} -> {after}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn live_cpu_ms_is_none_for_a_pid_that_does_not_exist() {
        assert_eq!(live_cpu_ms(999_999), None);
    }
}
