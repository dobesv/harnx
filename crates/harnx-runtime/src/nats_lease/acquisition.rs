//! One lease observation and reclaim policy, shared by activation preflight
//! and the claim's CAS.
//!
//! Preflight is not ownership: it only decides whether a claim attempt is
//! worth making. The claim re-observes the key and re-applies this same
//! policy against its own configuration before any purge, so an eligibility
//! decision never carries across a write. [`LeaseSlot`] remains the only
//! thing that mutates the key.

use super::broker_clock::{self, BrokerTime};
use super::lease_ttl;
use super::{holder_is_stale, LeaseRecord, NatsLeaseConfig};
use anyhow::Result;
use async_nats::jetstream::{self, kv};
use harnx_nats_common::leader_reads;
use std::time::Duration;
use time::OffsetDateTime;

/// Whether an acquisition may reclaim a lease by broker-clock age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcquisitionPolicy {
    /// The default and only public acquire path: a holder provably dead on
    /// this host is reclaimable, nothing else, and no broker time is ever
    /// requested. The GC election lease and the k8s leader election both
    /// acquire under this policy.
    Generic,
    /// Worker activation acquisition: proven-dead holders stay immediately
    /// reclaimable without a clock request, and a lease still present past
    /// three effective TTLs on the broker's own clock is reclaimable too.
    /// This is the policy the 2920 recovery is scoped to.
    WorkerActivation,
}

/// Why an observed lease was judged reclaimable.
#[derive(Debug)]
pub(crate) enum ReclaimReason {
    /// The holder provably died on this host: the pid is gone, or is a
    /// different process or a zombie. No clock request is needed.
    DeadHolder,
    /// The lease is old on the broker's clock — past three effective TTLs of
    /// the larger of the claimant's and the observed writer's TTL. Age alone
    /// is never proof that the holder process died, and no log of a
    /// [`ReclaimReason::BrokerAge`] reclaim may say so.
    BrokerAge {
        age: Duration,
        effective_ttl: Duration,
    },
}

/// What one leader-read of a lease key concluded.
#[derive(Debug)]
pub(crate) enum LeaseAssessment {
    /// Nothing actively holds the key: it is absent, or its latest revision
    /// is a Delete or Purge marker. Markers are never age-classified as
    /// active leases; their own expiry is the tombstone TTL.
    NoHolder,
    /// A Put whose payload is not a lease record. Never a free lease and
    /// never a stale holder: preflight reports it as an error so its
    /// existing settlement runs, and a claim keeps deferring.
    MalformedPut,
    /// The holder may be reclaimed now. The revision is the one the entry
    /// was read at; a purge must CAS against it, so a renewal committed in
    /// between makes the purge lose.
    Reclaimable {
        revision: u64,
        record: LeaseRecord,
        reason: ReclaimReason,
    },
    /// Someone else legitimately holds the lease under this policy.
    Held { record: LeaseRecord, age: HeldAge },
}

/// Why a held lease was judged not reclaimable.
#[derive(Debug)]
pub(crate) enum HeldAge {
    /// The lease is young on the broker's clock, or age reclaim is disabled
    /// for it (a present but not-understood TTL header).
    Fresh,
    /// The broker answered, but its time evidence was unusable — `ts`
    /// missing or invalid, or a leaderless cluster answered from a
    /// follower's store. The holder stays protected and the caller parks.
    UnknownClock,
}

/// Observe the lease key from the stream leader — one message's decoded
/// record, revision, broker `created`, and `async-nats`-decoded `HeaderMap`
/// (single-value/last-wins on duplicate names, not lossless raw wire
/// headers) — and judge whether its holder may be reclaimed under `policy`.
/// The key is the caller's: session locks and scoped leases (an A2A
/// context's, say) live side by side in the bucket, and an assessment of
/// one is not an assessment of the other.
///
/// Read and broker-clock failures propagate: the calling stage settles them
/// through its existing error path, and no failure ever authorizes a
/// purge. Under [`AcquisitionPolicy::Generic`] no clock request is made at
/// all. The clock is only sampled when a Put is present and its holder is
/// not provably dead — the free path costs one leader read.
pub(crate) async fn assess_lease(
    jetstream: &jetstream::Context,
    bucket: &kv::Store,
    key: &str,
    config: &NatsLeaseConfig,
    policy: AcquisitionPolicy,
) -> Result<LeaseAssessment> {
    let read = harnx_metrics::time_nats_operation(
        "lease_get",
        leader_reads::entry_with_headers(bucket, key),
    )
    .await?;
    let Some(read) = read else {
        return Ok(LeaseAssessment::NoHolder);
    };
    let entry = read.entry;
    // A Delete or Purge marker holds nothing, whatever its own age: never
    // age-classify a marker as an active lease.
    if entry.operation != kv::Operation::Put {
        return Ok(LeaseAssessment::NoHolder);
    }
    let record: LeaseRecord = match serde_json::from_slice(&entry.value) {
        Ok(record) => record,
        Err(_) => return Ok(LeaseAssessment::MalformedPut),
    };
    if holder_is_stale(&record) {
        return Ok(LeaseAssessment::Reclaimable {
            revision: entry.revision,
            record,
            reason: ReclaimReason::DeadHolder,
        });
    }
    if policy == AcquisitionPolicy::Generic {
        // Generic acquisitions never sample the broker clock: age reclaim
        // does not exist for them, so no request is made even to rule it
        // out.
        return Ok(LeaseAssessment::Held {
            record,
            age: HeldAge::Fresh,
        });
    }
    let broker_now = broker_clock::broker_time(jetstream, &bucket.stream_name).await?;
    // Classification can't know the revision the entry was read at, so the
    // age decision's revision placeholder is filled in here, from the read.
    let mut assessment =
        classify_by_age(record, &read.headers, entry.created, config.ttl, broker_now);
    if let LeaseAssessment::Reclaimable {
        revision,
        reason: ReclaimReason::BrokerAge { .. },
        ..
    } = &mut assessment
    {
        *revision = entry.revision;
    }
    Ok(assessment)
}

/// The age half of the worker-activation policy, pure: given a live Put's
/// TTL evidence, the broker's own timestamp for it and a usable clock
/// sample, decide between an age reclaim, a fresh hold and an unusable
/// clock. Everything that needs a broker is in [`assess_lease`].
fn classify_by_age(
    record: LeaseRecord,
    headers: &async_nats::HeaderMap,
    created: OffsetDateTime,
    claimant_ttl: Duration,
    broker_now: BrokerTime,
) -> LeaseAssessment {
    let BrokerTime::Now(broker_now) = broker_now else {
        return LeaseAssessment::Held {
            record,
            age: HeldAge::UnknownClock,
        };
    };
    let observed = lease_ttl::observed_ttl(headers);
    // A headerless record is legacy, not meaningless: plan against the
    // default-writer floor rather than a shorter guess. A header this
    // reader doesn't understand disables age reclaim entirely.
    let Some(effective_ttl) =
        lease_ttl::effective_ttl(claimant_ttl, observed, super::DEFAULT_LEASE_TTL)
    else {
        return LeaseAssessment::Held {
            record,
            age: HeldAge::Fresh,
        };
    };
    match lease_ttl::age_past_threshold(broker_now, created, effective_ttl) {
        Some(age) => LeaseAssessment::Reclaimable {
            // The caller fills the revision; classification only decides.
            revision: 0,
            record,
            reason: ReclaimReason::BrokerAge { age, effective_ttl },
        },
        None => LeaseAssessment::Held {
            record,
            age: HeldAge::Fresh,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(ttl: Option<&str>) -> async_nats::HeaderMap {
        let mut headers = async_nats::HeaderMap::new();
        if let Some(value) = ttl {
            headers.insert(async_nats::header::NATS_MESSAGE_TTL, value.to_string());
        }
        headers
    }

    fn classify(
        ttl_header: Option<&str>,
        claimant_ttl: Duration,
        age_secs: u64,
    ) -> LeaseAssessment {
        let created = OffsetDateTime::now_utc();
        let broker_now = created + time::Duration::seconds(age_secs as i64);
        classify_by_age(
            LeaseRecord::new("holder".into(), 1),
            &header(ttl_header),
            created,
            claimant_ttl,
            BrokerTime::Now(broker_now),
        )
    }

    fn is_broker_age_reclaimable(assessment: &LeaseAssessment) -> bool {
        matches!(
            assessment,
            LeaseAssessment::Reclaimable {
                reason: ReclaimReason::BrokerAge { .. },
                ..
            }
        )
    }

    #[test]
    fn an_unusable_clock_sample_keeps_the_holder_protected() {
        let assessment = classify_by_age(
            LeaseRecord::new("holder".into(), 1),
            &header(None),
            OffsetDateTime::now_utc() - time::Duration::seconds(3600),
            Duration::from_secs(1),
            BrokerTime::Unknown,
        );
        assert!(matches!(
            assessment,
            LeaseAssessment::Held {
                age: HeldAge::UnknownClock,
                ..
            }
        ));
    }

    #[test]
    fn an_old_headerless_record_is_reclaimed_at_the_legacy_floor() {
        // The effective TTL is the 30s legacy floor, not the claimant's
        // shorter one: exactly three of them is still protected, strictly
        // past them is a reclaim.
        let fresh = classify(None, Duration::from_secs(1), 89);
        assert!(!is_broker_age_reclaimable(&fresh));
        let exact = classify(None, Duration::from_secs(1), 90);
        assert!(!is_broker_age_reclaimable(&exact), "equality is not expiry");
        let old = classify(None, Duration::from_secs(1), 91);
        assert!(is_broker_age_reclaimable(&old));
    }

    #[test]
    fn an_old_record_with_a_bounded_ttl_is_reclaimed_at_its_own_ttl() {
        // At a one-second writer TTL the record is reclaimable after three
        // seconds, whatever the claimant configured.
        let short_writer = classify(Some("1"), Duration::from_secs(1), 30);
        assert!(is_broker_age_reclaimable(&short_writer));
        // A writer that granted itself thirty seconds stays protected well
        // past the claimant's own threshold: the writer's TTL extends the
        // threshold, never shortens it.
        let long_writer = classify(Some("30"), Duration::from_secs(1), 30);
        assert!(!is_broker_age_reclaimable(&long_writer));
        let old_enough = classify(Some("30"), Duration::from_secs(1), 91);
        assert!(is_broker_age_reclaimable(&old_enough));
    }

    #[test]
    fn a_not_understood_ttl_disables_age_reclaim_at_any_age() {
        for value in ["never", "NEVER", "0", "0s", "soon", "1h30"] {
            let assessment = classify(Some(value), Duration::from_secs(1), 3600);
            assert!(
                !is_broker_age_reclaimable(&assessment),
                "{value:?} must disable age reclaim"
            );
            assert!(matches!(
                assessment,
                LeaseAssessment::Held {
                    age: HeldAge::Fresh,
                    ..
                }
            ));
        }
    }

    #[test]
    fn a_record_from_the_future_is_never_reclaimable() {
        // A clock jump either way must not authorize a purge.
        let created = OffsetDateTime::now_utc();
        let broker_now = created - time::Duration::seconds(60);
        let assessment = classify_by_age(
            LeaseRecord::new("holder".into(), 1),
            &header(Some("1")),
            created,
            Duration::from_secs(1),
            BrokerTime::Now(broker_now),
        );
        assert!(!is_broker_age_reclaimable(&assessment));
    }

    #[test]
    fn a_broker_age_reclaim_reports_its_age_and_effective_ttl() {
        let assessment = classify(None, Duration::from_secs(1), 91);
        match assessment {
            LeaseAssessment::Reclaimable {
                reason: ReclaimReason::BrokerAge { age, effective_ttl },
                ..
            } => {
                assert_eq!(effective_ttl, super::super::DEFAULT_LEASE_TTL);
                assert!(
                    age > effective_ttl * 3,
                    "the recorded age must be past the threshold"
                );
            }
            other => panic!("expected a broker-age reclaim, got {other:?}"),
        }
    }
}
