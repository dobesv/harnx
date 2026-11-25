//! A conservative subset of broker TTL syntax: harnx's plain integer seconds,
//! or positive Go durations — decimal components followed by units (`ns`,
//! `us`, `µs`, `μs`, `ms`, `s`, `m`, `h`). Fractions must fit nanosecond
//! precision, and values must fit Go's signed nanosecond duration range.
//! Unsupported or ambiguous inputs are `None`, never shortened evidence.

use std::time::Duration;

/// Nanoseconds per second, the smallest TTL accepted for age evidence.
const NANOS_PER_SECOND: u128 = 1_000_000_000;
/// Go's `time.Duration` is a signed int64 count of nanoseconds.
const MAX_TTL_NANOS: u128 = i64::MAX as u128;
/// Fractions beyond nanosecond resolution are not accepted.
const MAX_FRACTION_DIGITS: usize = 9;
/// The base the fractional digits of a component are scaled by.
const FRACTION_RADIX: u128 = 10;

pub(super) fn parse_ttl(value: &str) -> Option<Duration> {
    if value.is_empty() {
        return None;
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        let seconds: u128 = value.parse().ok()?;
        let nanos = seconds.checked_mul(NANOS_PER_SECOND)?;
        return (nanos <= MAX_TTL_NANOS).then(|| Duration::from_secs(seconds as u64));
    }
    let mut total_ns: u128 = 0;
    let mut rest = value;
    loop {
        let (component_ns, remainder) = parse_component(rest)?;
        total_ns = total_ns.checked_add(component_ns)?;
        rest = remainder;
        if rest.is_empty() {
            return (NANOS_PER_SECOND..=MAX_TTL_NANOS)
                .contains(&total_ns)
                .then(|| Duration::from_nanos(total_ns as u64));
        }
    }
}

/// One Go-duration component: a decimal number (an optional fraction) and
/// its unit, valued in nanoseconds, plus the input left after it.
fn parse_component(mut rest: &str) -> Option<(u128, &str)> {
    let int_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if int_end == 0 {
        return None;
    }
    let whole: u128 = rest[..int_end].parse().ok()?;
    rest = &rest[int_end..];
    let (fraction, scale) = parse_fraction(&mut rest)?;
    let (unit_ns, unit_len) = unit_nanos(rest)?;
    rest = &rest[unit_len..];
    let whole_ns = whole.checked_mul(unit_ns)?;
    let fraction_ns = fraction.checked_mul(unit_ns)? / scale;
    Some((whole_ns.checked_add(fraction_ns)?, rest))
}

/// The fractional part of a component, as (numerator, 10^digits), at most
/// nine digits; additional precision is rejected.
fn parse_fraction(rest: &mut &str) -> Option<(u128, u128)> {
    let Some(after_dot) = rest.strip_prefix('.') else {
        return Some((0, 1));
    };
    if !after_dot.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let frac_end = after_dot
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_dot.len());
    if frac_end > MAX_FRACTION_DIGITS {
        return None;
    }
    let kept = &after_dot[..frac_end];
    let fraction: u128 = if kept.is_empty() {
        0
    } else {
        kept.parse().ok()?
    };
    *rest = &after_dot[frac_end..];
    Some((fraction, FRACTION_RADIX.pow(kept.len() as u32)))
}

/// The unit at the start of `rest`, in nanoseconds, checked longest first so
/// `ms` wins over `s` and `m`.
fn unit_nanos(rest: &str) -> Option<(u128, usize)> {
    const UNITS: [(&str, u128); 8] = [
        ("ns", 1),
        ("us", 1_000),
        ("µs", 1_000),
        ("μs", 1_000),
        ("ms", 1_000_000),
        ("s", 1_000_000_000),
        ("m", 60_000_000_000),
        ("h", 3_600_000_000_000),
    ];
    UNITS
        .iter()
        .find_map(|(unit, ns)| rest.strip_prefix(unit).map(|_| (*ns, unit.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_seconds_and_go_durations_parse() {
        assert_eq!(parse_ttl("30"), Some(Duration::from_secs(30)));
        assert_eq!(parse_ttl("90m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_ttl("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_ttl("1h30m10s"), Some(Duration::from_secs(5410)));
        assert_eq!(parse_ttl("1.5h"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_ttl("1s500ms"), Some(Duration::from_millis(1500)));
        assert_eq!(
            parse_ttl("9223372036"),
            Some(Duration::from_secs(9_223_372_036))
        );
        assert_eq!(
            parse_ttl("2562047h47m16.854775807s"),
            Some(Duration::from_nanos(i64::MAX as u64))
        );
    }

    #[test]
    fn anything_else_is_not_understood() {
        for value in [
            "",
            "+",
            "+30",
            "-",
            "-30s",
            "0x10",
            "30x",
            "soon",
            "1h30",
            "h",
            ".5s",
            "1..5s",
            "1s1",
            "500ms",
            "0.999999999s",
            "1.0000000001s",
            "1.0000000001h",
            "9223372037",
            "9223372036854775808ns",
            "2562047h47m16.854775808s",
        ] {
            assert_eq!(parse_ttl(value), None, "{value:?}");
        }
    }

    #[test]
    fn overflowing_values_are_not_understood() {
        assert_eq!(parse_ttl("9999999999h"), None);
        assert_eq!(parse_ttl("999999999999999999999999999999"), None);
    }
}
