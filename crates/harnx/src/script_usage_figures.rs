//! Number formatting for the script usage text report: grouped digits, KiB,
//! means and percentages, with `-` wherever the denominator is zero.

use crate::session_stats_render::rfc3339;
use chrono::{DateTime, Utc};

const KIB: f64 = 1024.0;
const GROUP: usize = 3;
const TENTHS: f64 = 10.0;
const TENTHS_PER_UNIT: u64 = 10;
pub(crate) const NONE: &str = "-";

/// Digits grouped in threes: `1234567` -> `1,234,567`.
pub(crate) fn num(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / GROUP);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(GROUP) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Bytes as KiB with one decimal and grouped integer digits.
pub(crate) fn kib(bytes: u64) -> String {
    let tenths = (bytes as f64 / KIB * TENTHS).round() as u64;
    format!(
        "{}.{} KiB",
        num(tenths / TENTHS_PER_UNIT),
        tenths % TENTHS_PER_UNIT
    )
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    numerator as f64 / denominator as f64
}

/// A mean with one decimal, or `-` when nothing was counted.
pub(crate) fn mean(total: u64, count: u64) -> String {
    if count == 0 {
        return NONE.to_string();
    }
    let tenths = (ratio(total, count) * TENTHS).round() as u64;
    format!(
        "{}.{}",
        num(tenths / TENTHS_PER_UNIT),
        tenths % TENTHS_PER_UNIT
    )
}

/// A whole-number mean, or `-` when nothing was counted.
pub(crate) fn whole_mean(total: u64, count: u64) -> String {
    if count == 0 {
        return NONE.to_string();
    }
    num(ratio(total, count).round() as u64)
}

pub(crate) fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return NONE.to_string();
    }
    format!("{}%", (ratio(part, whole) * 100.0).round() as u64)
}

pub(crate) fn when(time: Option<DateTime<Utc>>) -> String {
    time.map(rfc3339).unwrap_or_else(|| NONE.to_string())
}

pub(crate) fn exit_code(code: Option<i64>) -> String {
    code.map(|code| code.to_string())
        .unwrap_or_else(|| NONE.to_string())
}

pub(crate) fn window_text(since: Option<DateTime<Utc>>, until: Option<DateTime<Utc>>) -> String {
    match (since, until) {
        (None, None) => "all time".to_string(),
        (Some(since), None) => format!("since {}", rfc3339(since)),
        (None, Some(until)) => format!("until {}", rfc3339(until)),
        (Some(since), Some(until)) => format!("{} to {}", rfc3339(since), rfc3339(until)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_helpers_group_digits_and_guard_zero() {
        assert_eq!(num(0), "0");
        assert_eq!(num(999), "999");
        assert_eq!(num(1_000), "1,000");
        assert_eq!(num(1_234_567), "1,234,567");
        assert_eq!(kib(1_536), "1.5 KiB");
        assert_eq!(kib(5_242_880), "5,120.0 KiB");
        assert_eq!(mean(10, 0), "-");
        assert_eq!(mean(1_800, 40), "45.0");
        assert_eq!(percent(1, 0), "-");
        assert_eq!(percent(1_800, 2_400), "75%");
    }
}
