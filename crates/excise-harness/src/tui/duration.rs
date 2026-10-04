//! Durations on the `cargo xtask tui` command line.

use std::time::Duration;

/// Parses a duration written as a whole number and a unit: `250ms`, `30s`, `15m`, or `2h`.
///
/// # Errors
///
/// Returns why the text is not a duration.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let invalid = || {
        format!(
            "`{text}` is not a duration: write a whole number and a unit, as in 250ms, 30s, 15m, \
             or 2h"
        )
    };
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let (number, unit) = text.split_at(digits);
    let number: u64 = number.parse().map_err(|_| invalid())?;
    let overflow = || format!("`{text}` is too long a duration");
    match unit {
        "ms" => Ok(Duration::from_millis(number)),
        "s" => Ok(Duration::from_secs(number)),
        "m" => number
            .checked_mul(60)
            .map(Duration::from_secs)
            .ok_or_else(overflow),
        "h" => number
            .checked_mul(3600)
            .map(Duration::from_secs)
            .ok_or_else(overflow),
        _ => Err(invalid()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_and_a_unit_make_a_duration() {
        assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("15m"), Ok(Duration::from_mins(15)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_hours(2)));
        assert_eq!(parse_duration("0s"), Ok(Duration::ZERO));
    }

    #[test]
    fn anything_else_is_refused_with_the_notation() {
        for text in [
            "", "s", "15", "15 m", "1.5h", "-3s", "15min", "1d", "m15", "0x10s",
        ] {
            let error = parse_duration(text).expect_err(text);
            assert!(error.contains("whole number and a unit"), "{text}: {error}");
        }
    }

    #[test]
    fn a_duration_too_long_to_count_is_refused() {
        let error = parse_duration("18446744073709551615h").expect_err("an overflow");
        assert!(error.contains("too long"), "{error}");
        assert!(parse_duration("99999999999999999999999s").is_err());
    }
}
