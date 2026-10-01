//! Helpers the runners share: the identity of a run (host, binary digest, timestamps), the
//! `latest` pointer beside a run's output, and plain-text tables.

use std::{
    fmt::Write as _,
    fs::{self, File},
    io::{self, Read},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

/// Makes `<out_root>/latest` refer to the run `run_id`: a symbolic link on Unix, and a text file
/// holding the run id elsewhere, where creating a link needs a privilege.
pub(crate) fn point_latest_at(out_root: &Path, run_id: &str) -> io::Result<()> {
    let latest = out_root.join("latest");
    match fs::remove_file(&latest) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(run_id, &latest)
    }
    #[cfg(not(unix))]
    {
        fs::write(&latest, format!("{run_id}\n"))
    }
}

/// Lays `rows` out as a table: every column as wide as its widest cell, two spaces between
/// columns, and no trailing spaces.
pub(crate) fn render_rows<const N: usize>(rows: &[[String; N]]) -> String {
    let widths: Vec<usize> = (0..N)
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        let _ = writeln!(out, "{}", line.join("  ").trim_end());
    }
    out
}

/// The median of `values`, or `None` for none. The median of an even count is the midpoint of the
/// two middle values.
pub(crate) fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        f64::midpoint(sorted[middle - 1], sorted[middle])
    })
}

/// The largest of `values`, or `None` for none.
pub(crate) fn worst(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::max)
}

/// Milliseconds as `42 ms`, or as `1.50 s` from a second on; `-` for no value.
pub(crate) fn format_ms(value: Option<f64>) -> String {
    value.map_or_else(
        || "-".to_owned(),
        |ms| {
            if ms >= 1000.0 {
                format!("{:.2} s", ms / 1000.0)
            } else {
                format!("{ms:.0} ms")
            }
        },
    )
}

// ---------------------------------------------------------------------------------------------
// Identity of the run.

pub(crate) fn host_name() -> String {
    #[cfg(unix)]
    {
        let name = rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned();
        if !name.is_empty() {
            return name;
        }
    }
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The lowercase hexadecimal SHA-256 of a file.
pub(crate) fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// `time` as an RFC 3339 UTC timestamp with millisecond precision, such as
/// `2026-09-30T13:37:56.123Z`.
pub(crate) fn rfc3339(time: SystemTime) -> String {
    let (date, seconds_of_day, millis) = civil(time);
    format!(
        "{date}T{:02}:{:02}:{:02}.{millis:03}Z",
        seconds_of_day / 3600,
        seconds_of_day / 60 % 60,
        seconds_of_day % 60,
    )
}

/// `time` as a compact UTC timestamp for a run id, such as `20260930T133756Z`.
pub(crate) fn compact_utc(time: SystemTime) -> String {
    let (date, seconds_of_day, _) = civil(time);
    format!(
        "{}T{:02}{:02}{:02}Z",
        date.replace('-', ""),
        seconds_of_day / 3600,
        seconds_of_day / 60 % 60,
        seconds_of_day % 60,
    )
}

/// The UTC date as `YYYY-MM-DD`, the seconds since midnight, and the milliseconds of the second.
///
/// The date comes from the days-since-epoch to civil-date algorithm: shift the epoch to 0000-03-01
/// so leap days fall at the end of the year, then work in 400-year eras of 146097 days.
fn civil(time: SystemTime) -> (String, u64, u32) {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let days = i64::try_from(since_epoch.as_secs() / 86_400).unwrap_or(0);
    let seconds_of_day = since_epoch.as_secs() % 86_400;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        format!("{year:04}-{month:02}-{day:02}"),
        seconds_of_day,
        since_epoch.subsec_millis(),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(seconds: u64, millis: u32) -> SystemTime {
        UNIX_EPOCH + Duration::new(seconds, millis * 1_000_000)
    }

    #[test]
    fn timestamps_are_rfc3339_in_utc() {
        assert_eq!(rfc3339(at(0, 0)), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(at(951_782_400, 5)), "2000-02-29T00:00:00.005Z");
        assert_eq!(rfc3339(at(1_790_775_476, 123)), "2026-09-30T13:37:56.123Z");
        assert_eq!(rfc3339(at(4_102_444_799, 999)), "2099-12-31T23:59:59.999Z");
    }

    #[test]
    fn a_run_id_timestamp_is_a_valid_id_component() {
        let id = compact_utc(at(1_790_775_476, 0));

        assert_eq!(id, "20260930T133756Z");
        assert!(id.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    }

    #[test]
    fn leap_days_and_year_ends_are_dated_correctly() {
        for (seconds, date) in [
            (68_256_000, "1972-03-01"),
            (94_608_000, "1972-12-31"),
            (1_709_164_800, "2024-02-29"),
            (1_709_251_200, "2024-03-01"),
            (4_107_456_000, "2100-02-28"),
            (4_107_542_400, "2100-03-01"),
        ] {
            assert_eq!(civil(at(seconds, 0)).0, date, "{seconds}");
        }
    }

    #[test]
    fn medians_and_worst_values_ignore_missing_metrics() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0]), Some(3.0));
        assert_eq!(median(&[5.0, 1.0, 9.0]), Some(5.0));
        assert_eq!(median(&[1.0, 2.0, 3.0, 10.0]), Some(2.5));
        assert_eq!(worst(&[]), None);
        assert_eq!(worst(&[1.0, 7.5, 3.0]), Some(7.5));
    }

    #[test]
    fn durations_read_in_milliseconds_or_seconds() {
        assert_eq!(format_ms(None), "-");
        assert_eq!(format_ms(Some(42.4)), "42 ms");
        assert_eq!(format_ms(Some(1500.0)), "1.50 s");
    }

    #[test]
    fn the_latest_pointer_follows_the_newest_run() {
        let out = tempfile::tempdir().expect("a temporary directory");
        fs::create_dir(out.path().join("run-1")).expect("run directory");
        fs::create_dir(out.path().join("run-2")).expect("run directory");

        point_latest_at(out.path(), "run-1").expect("first pointer");
        point_latest_at(out.path(), "run-2").expect("second pointer");

        #[cfg(unix)]
        assert_eq!(
            fs::read_link(out.path().join("latest")).expect("a link"),
            Path::new("run-2")
        );
        #[cfg(not(unix))]
        assert_eq!(
            fs::read_to_string(out.path().join("latest"))
                .expect("a file")
                .trim(),
            "run-2"
        );
    }
}
