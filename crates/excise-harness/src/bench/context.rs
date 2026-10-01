//! Gathering the conditions a comparison ran under: the host, the toolchain, the
//! power state, the load average, and concurrent `excise` processes. Timings never transfer
//! across sessions, so a `harness-ab` document records enough of the session to judge whether two
//! reads of it are comparable.

use std::{ffi::OsStr, process::Command};

use sysinfo::System;

/// A snapshot of the machine, taken once at the start of a comparison. Reusing it for both the
/// host facts and the concurrent-process count avoids asking the OS for the process list twice.
pub struct Snapshot(System);

impl Snapshot {
    /// Takes a snapshot of CPUs and processes.
    #[must_use]
    pub fn take() -> Self {
        Self(System::new_all())
    }

    /// The CPU model (brand string), or `unknown` where the platform reports none.
    #[must_use]
    pub fn cpu_model(&self) -> String {
        self.0
            .cpus()
            .first()
            .map(|cpu| cpu.brand().trim().to_owned())
            .filter(|brand| !brand.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())
    }

    /// The number of logical CPUs, at least 1.
    #[must_use]
    pub fn logical_cpus(&self) -> u32 {
        u32::try_from(self.0.cpus().len())
            .unwrap_or(u32::MAX)
            .max(1)
    }

    /// How many *other* processes named `excise` are currently running. This process and its
    /// not-yet-spawned children are never named `excise` at the point this is called (the
    /// comparison takes the snapshot before building or launching anything), so every match is a
    /// genuinely concurrent session.
    #[must_use]
    pub fn concurrent_excise_processes(&self) -> u32 {
        let name = OsStr::new(excise_process_name());
        u32::try_from(self.0.processes_by_exact_name(name).count()).unwrap_or(u32::MAX)
    }
}

/// The name `excise`'s process list entry has on this platform.
const fn excise_process_name() -> &'static str {
    if cfg!(windows) {
        "excise.exe"
    } else {
        "excise"
    }
}

/// The operating system and version, for example `macOS 14.8.9` or `Ubuntu 24.04.1 LTS`.
#[must_use]
pub fn os_description() -> String {
    match (System::name(), System::os_version()) {
        (Some(name), Some(version)) if !version.is_empty() => format!("{name} {version}"),
        (Some(name), _) => name,
        (None, _) => std::env::consts::OS.to_owned(),
    }
}

/// `rustc -Vv`, trimmed, or `unknown` when `rustc` cannot be run. Both binaries are built by the
/// toolchain on `PATH` when this runs, so this is also what built them.
#[must_use]
pub fn toolchain() -> String {
    Command::new("rustc")
        .arg("-Vv")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The 1-minute load average.
#[must_use]
pub fn load_average_one_minute() -> f64 {
    System::load_average().one
}

/// The power state: `ac`, `battery`, or `unknown` where the platform cannot say. macOS reads
/// `pmset -g batt` and Linux `/sys/class/power_supply`; `sysinfo` has no battery API.
#[must_use]
pub fn power_state() -> String {
    #[cfg(target_os = "macos")]
    {
        macos::power_state()
    }
    #[cfg(target_os = "linux")]
    {
        linux::power_state()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "unknown".to_owned()
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::process::Command;

    /// Parses the first line of `pmset -g batt`: `Now drawing from 'AC Power'` or
    /// `'Battery Power'`.
    pub(super) fn power_state() -> String {
        let Ok(output) = Command::new("pmset").args(["-g", "batt"]).output() else {
            return "unknown".to_owned();
        };
        let text = String::from_utf8_lossy(&output.stdout);
        match text.lines().next() {
            Some(first) if first.contains("AC Power") => "ac".to_owned(),
            Some(first) if first.contains("Battery Power") => "battery".to_owned(),
            _ => "unknown".to_owned(),
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;

    /// Reads `/sys/class/power_supply/*/{type,online,status}`: `ac` when a mains or UPS supply
    /// is online or a battery is charging or full, `battery` when only a battery is present,
    /// `unknown` when the directory cannot be read or has neither.
    pub(super) fn power_state() -> String {
        let Ok(entries) = fs::read_dir("/sys/class/power_supply") else {
            return "unknown".to_owned();
        };
        let mut saw_battery = false;
        for entry in entries.flatten() {
            let path = entry.path();
            let kind = fs::read_to_string(path.join("type")).unwrap_or_default();
            match kind.trim() {
                "Mains" | "UPS" => {
                    let online = fs::read_to_string(path.join("online")).unwrap_or_default();
                    if online.trim() == "1" {
                        return "ac".to_owned();
                    }
                }
                "Battery" => {
                    saw_battery = true;
                    let status = fs::read_to_string(path.join("status")).unwrap_or_default();
                    if matches!(status.trim(), "Charging" | "Full") {
                        return "ac".to_owned();
                    }
                }
                _ => {}
            }
        }
        if saw_battery {
            "battery".to_owned()
        } else {
            "unknown".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_reports_at_least_one_logical_cpu_and_a_nonempty_model() {
        let snapshot = Snapshot::take();

        assert!(snapshot.logical_cpus() >= 1);
        assert!(!snapshot.cpu_model().is_empty());
    }

    #[test]
    fn the_toolchain_string_names_rustc() {
        // This test runs under the pinned toolchain, so `rustc -Vv` must succeed and mention
        // its own name.
        assert!(toolchain().starts_with("rustc "), "{}", toolchain());
    }

    #[test]
    fn the_os_description_is_never_empty() {
        assert!(!os_description().is_empty());
    }

    #[test]
    fn the_power_state_is_one_of_the_documented_values() {
        assert!(matches!(
            power_state().as_str(),
            "ac" | "battery" | "unknown"
        ));
    }

    #[test]
    fn the_load_average_is_never_negative() {
        assert!(load_average_one_minute() >= 0.0);
    }
}
