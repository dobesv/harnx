//! The `Nats-TTL` evidence on an observed lease Put, and the age rule it
//! feeds.
//!
//! harnx create and renewal encode the lease's TTL as plain integer seconds,
//! but NATS accepts Go duration syntax (`1h30m`) and `never` for per-message
//! expiry, and older or manual writers may put a record with no header at
//! all. The age policy this module serves has to fail closed: only a TTL it
//! fully understands may authorize an age-based reclaim, and a present but
//! unknown value must never fall back to a shorter guessed TTL.

mod parse;

use async_nats::header::NATS_MESSAGE_TTL;
use async_nats::HeaderMap;
use std::time::Duration;
use time::OffsetDateTime;

/// How many effective TTLs a lease must outlive before age alone makes it
/// reclaimable. Strictly greater-than: at exactly the threshold the lease
/// is still protected. Derived from the TTL, deliberately not configurable —
/// a knob here would let an operator trade a live holder's safety away.
const AGE_TTL_MULTIPLE: u32 = 3;

/// The TTL evidence carried by one observed Put message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ObservedTtl {
    /// No `Nats-TTL` header: a record from an older default-writer binary.
    /// The caller plans against a conservative default floor (see
    /// [`super::DEFAULT_LEASE_TTL`]) rather than guessing a shorter TTL.
    Absent,
    /// A positive, fully understood TTL.
    Bounded(Duration),
    /// Present but not understood: `never`, zero, malformed, unrecognized or
    /// overflowing. Age-based reclaim is disabled for such a lease — a
    /// present header the reader can't decode is evidence of a writer whose
    /// expiry this reader cannot plan against. A holder provably dead on
    /// this host stays reclaimable independently.
    NotUnderstood,
}

/// What the observed message's `Nats-TTL` header says.
pub(super) fn observed_ttl(headers: &HeaderMap) -> ObservedTtl {
    let Some(value) = headers.get(NATS_MESSAGE_TTL) else {
        return ObservedTtl::Absent;
    };
    let value = value.as_str().trim();
    if value.eq_ignore_ascii_case("never") {
        return ObservedTtl::NotUnderstood;
    }
    match parse::parse_ttl(value) {
        Some(ttl) if !ttl.is_zero() => ObservedTtl::Bounded(ttl),
        _ => ObservedTtl::NotUnderstood,
    }
}

/// The TTL an age check plans against: the larger of this claimant's
/// configured TTL and the observed writer's, so a writer that granted itself
/// a longer lease than the claimant configured is still protected. Only a
/// genuinely absent header falls back to the default-writer floor.
///
/// `None` disables age-based reclaim for the observed lease.
pub(super) fn effective_ttl(
    claimant_ttl: Duration,
    observed: ObservedTtl,
    absent_header_floor: Duration,
) -> Option<Duration> {
    match observed {
        ObservedTtl::Bounded(ttl) => Some(claimant_ttl.max(ttl)),
        ObservedTtl::Absent => Some(claimant_ttl.max(absent_header_floor)),
        ObservedTtl::NotUnderstood => None,
    }
}

/// Whether `broker_now - created` puts the entry strictly past
/// `effective_ttl` × [`AGE_TTL_MULTIPLE`]. Returns the age when it does.
///
/// Never eligible: an entry timestamped after the sample (a future
/// `created` says nothing about staleness, and neither does a sample the
/// other side of a clock jump), a threshold that overflows, or an age that
/// doesn't fit a wall-clock duration. Equality is not expiry.
pub(super) fn age_past_threshold(
    broker_now: OffsetDateTime,
    created: OffsetDateTime,
    effective_ttl: Duration,
) -> Option<Duration> {
    let threshold = effective_ttl.checked_mul(AGE_TTL_MULTIPLE)?;
    let age = broker_now - created;
    if age.is_negative() {
        return None;
    }
    let age = std::time::Duration::try_from(age).ok()?;
    (age > threshold).then_some(age)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ttl(header: Option<&str>) -> ObservedTtl {
        let mut headers = HeaderMap::new();
        if let Some(value) = header {
            headers.insert(NATS_MESSAGE_TTL, value.to_string());
        }
        observed_ttl(&headers)
    }

    #[test]
    fn absent_header_means_absent() {
        assert_eq!(ttl(None), ObservedTtl::Absent);
    }

    #[test]
    fn understood_values_classify_as_bounded() {
        assert_eq!(
            ttl(Some("30")),
            ObservedTtl::Bounded(Duration::from_secs(30))
        );
        // A long compound duration a short-TTL claimant must respect: the
        // age threshold grows with it, not with the claimant.
        assert_eq!(
            ttl(Some("1h30m")),
            ObservedTtl::Bounded(Duration::from_secs(5400))
        );
        assert_eq!(
            ttl(Some("1s500ms")),
            ObservedTtl::Bounded(Duration::from_millis(1500))
        );
        // Whitespace around the value, as a stray header edit might leave.
        assert_eq!(
            ttl(Some(" 30 ")),
            ObservedTtl::Bounded(Duration::from_secs(30))
        );
    }

    #[test]
    fn present_but_unknown_ttl_fails_closed() {
        for value in [
            "never", "NEVER", "0", "0s", "500ms", "soon", "30x", "-30s", "1h30", "", "h", ".5s",
        ] {
            assert_eq!(ttl(Some(value)), ObservedTtl::NotUnderstood, "{value:?}");
        }
        // Overflowing durations are not understood either.
        assert_eq!(ttl(Some("9999999999h")), ObservedTtl::NotUnderstood);
        assert_eq!(
            ttl(Some("999999999999999999999999999999")),
            ObservedTtl::NotUnderstood
        );
    }

    #[test]
    fn effective_ttl_takes_the_larger_of_claimant_and_writer() {
        let claimant = Duration::from_secs(30);
        assert_eq!(
            effective_ttl(
                claimant,
                ObservedTtl::Bounded(Duration::from_secs(60)),
                claimant
            ),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            effective_ttl(
                claimant,
                ObservedTtl::Bounded(Duration::from_secs(10)),
                claimant
            ),
            Some(claimant)
        );
        // A disabled TTL never plans against a shorter guess.
        assert_eq!(
            effective_ttl(claimant, ObservedTtl::NotUnderstood, claimant),
            None
        );
    }

    #[test]
    fn absent_ttl_uses_the_default_writer_floor() {
        // A claimant with a shorter configured TTL still plans against the
        // default-writer floor for headerless legacy records.
        assert_eq!(
            effective_ttl(
                Duration::from_secs(2),
                ObservedTtl::Absent,
                Duration::from_secs(30)
            ),
            Some(Duration::from_secs(30))
        );
        // A claimant configured above the floor uses its own TTL.
        assert_eq!(
            effective_ttl(
                Duration::from_secs(90),
                ObservedTtl::Absent,
                Duration::from_secs(30)
            ),
            Some(Duration::from_secs(90))
        );
    }

    fn sample(now_minus: Duration) -> (OffsetDateTime, OffsetDateTime) {
        let created = OffsetDateTime::now_utc();
        let broker_now = created + now_minus;
        (broker_now, created)
    }

    #[test]
    fn age_eligibility_is_strictly_past_three_ttls() {
        let ttl = Duration::from_secs(30);
        // Exactly at the threshold: still protected.
        let (now, created) = sample(ttl * 3);
        assert_eq!(age_past_threshold(now, created, ttl), None);
        // Just over: eligible.
        let (now, created) = sample(ttl * 3 + Duration::from_millis(1));
        assert_eq!(
            age_past_threshold(now, created, ttl),
            Some(ttl * 3 + Duration::from_millis(1))
        );
    }

    #[test]
    fn future_entry_timestamp_is_never_eligible() {
        let created = OffsetDateTime::now_utc();
        let broker_now = created - Duration::from_secs(3600);
        assert_eq!(
            age_past_threshold(broker_now, created, Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn overflowing_age_or_threshold_is_never_eligible() {
        let (now, created) = sample(Duration::from_secs(3600));
        // A threshold that overflows a wall-clock duration.
        assert_eq!(age_past_threshold(now, created, Duration::MAX / 2), None);
    }

    #[test]
    fn nondefault_ttl_derives_its_own_threshold() {
        // A 2s TTL claims back after strictly more than 6s, not the default 90s.
        let (now, created) = sample(Duration::from_secs(6));
        assert_eq!(
            age_past_threshold(now, created, Duration::from_secs(2)),
            None
        );
        let (now, created) = sample(Duration::from_secs(6) + Duration::from_millis(1));
        assert_eq!(
            age_past_threshold(now, created, Duration::from_secs(2)),
            Some(Duration::from_secs(6) + Duration::from_millis(1))
        );
    }
}
