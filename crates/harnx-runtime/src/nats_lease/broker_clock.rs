//! Sampling the broker's wall clock from the lease stream.
//!
//! A lease that outlives its own TTL can only be judged stale against the
//! broker's clock — a worker's UTC can be skewed arbitrarily, so
//! [`super::LeaseRecord::acquired_at`] and any local timestamp are useless
//! as age evidence. NATS 2.11.6 stamps every `STREAM.INFO` answer with a
//! top-level `ts` set by the responding server's wall clock; async-nats'
//! typed `Info` drops it, so this module requests the same API subject the
//! rest of the runtime uses and decodes just what it needs.

use anyhow::Result;
use async_nats::jetstream::{self, response::Response};
use serde::Deserialize;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// What a successful `STREAM.INFO` answer said about the broker's clock.
#[derive(Debug)]
pub(super) enum BrokerTime {
    /// The responding server's wall clock, from a solo stream or a cluster
    /// that reports a leader. The leader need not be the responding server.
    Now(OffsetDateTime),
    /// A well-formed answer that carries no usable time evidence: `ts`
    /// missing or unparseable, or clustered metadata with no leader (a
    /// lagging follower answers `STREAM.INFO` from its own store, and its
    /// clock and its state are both untrustworthy). The caller treats the
    /// lease's age as unknown and leaves the holder protected.
    Unknown,
}

/// Sample the broker's current time from `stream_name`'s `STREAM.INFO`,
/// preserving the context's configured API prefix and timeout.
///
/// Request failures — a timeout, a transport failure, a JetStream API error —
/// propagate so the calling stage settles them through its existing error
/// path; only a successful answer without usable time becomes
/// [`BrokerTime::Unknown`]. A malformed `ts` is unusable evidence, not a
/// transport error: the server answered, it just can't be trusted to date
/// anything.
pub(super) async fn broker_time(
    jetstream: &jetstream::Context,
    stream_name: &str,
) -> Result<BrokerTime> {
    let info: StreamTime = harnx_metrics::time_nats_operation("lease_clock", async {
        match jetstream
            .request(format!("STREAM.INFO.{stream_name}"), &serde_json::json!({}))
            .await?
        {
            Response::Ok(info) => Ok(info),
            Response::Err { error } => Err(anyhow::Error::from(error)),
        }
    })
    .await?;
    if !info.reports_solo_or_leader() {
        return Ok(BrokerTime::Unknown);
    }
    let Some(ts) = info.ts.as_ref().and_then(Value::as_str) else {
        return Ok(BrokerTime::Unknown);
    };
    match OffsetDateTime::parse(ts.trim(), &Rfc3339) {
        Ok(now) => Ok(BrokerTime::Now(now)),
        Err(_) => Ok(BrokerTime::Unknown),
    }
}

/// The fields of a `STREAM.INFO` answer the clock sample needs.
#[derive(Deserialize)]
struct StreamTime {
    #[serde(default)]
    ts: Option<Value>,
    cluster: Option<TimeCluster>,
}

#[derive(Deserialize)]
struct TimeCluster {
    #[serde(default)]
    leader: Option<String>,
}

impl StreamTime {
    /// Reject leaderless clustered replies, which can come from a lagging
    /// replica. A named leader tells us about stream topology, not the
    /// responder's identity or clock synchronization.
    fn reports_solo_or_leader(&self) -> bool {
        self.cluster.as_ref().is_none_or(|cluster| {
            cluster
                .leader
                .as_deref()
                .is_some_and(|leader| !leader.is_empty())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(json: &str) -> StreamTime {
        serde_json::from_str(json).expect("stream info answer decodes")
    }

    #[test]
    fn a_leaderless_cluster_is_not_usable_evidence() {
        let leaderless = answer(
            r#"{"cluster": {"name": "harnx", "replicas": []}, "ts": "2026-11-25T07:00:00Z"}"#,
        );
        assert!(!leaderless.reports_solo_or_leader());
    }

    #[test]
    fn a_solo_server_and_a_named_leader_answer() {
        let solo = answer(r#"{"ts": "2026-11-25T07:00:00Z"}"#);
        assert!(solo.reports_solo_or_leader());
        let led = answer(
            r#"{"cluster": {"name": "harnx", "leader": "nats-1"}, "ts": "2026-11-25T07:00:00Z"}"#,
        );
        assert!(led.reports_solo_or_leader());
        // An empty leader name is no leader.
        let blank = answer(r#"{"cluster": {"name": "harnx", "leader": ""}}"#);
        assert!(!blank.reports_solo_or_leader());
    }

    #[test]
    fn unknown_fields_are_ignored_and_ts_is_optional() {
        // The real answer carries config and state the sample doesn't need.
        let full = answer(
            r#"{"type": "io.nats.jetstream.api.v1.stream_info_response",
                "config": {"name": "KV_harnx_leases"}, "state": {"messages": 1},
                "ts": "2026-11-25T07:00:00.5Z"}"#,
        );
        assert!(full.ts.is_some());
        let bare = answer("{}");
        assert_eq!(bare.ts, None);
    }

    #[test]
    fn rfc3339_ts_parses_at_nanos() {
        // Shaped like a real nats-server 2.11.6 STREAM.INFO `ts`.
        let ts = OffsetDateTime::parse("2026-11-25T07:00:00.123456789Z", &Rfc3339).unwrap();
        assert_eq!(ts.unix_timestamp_nanos(), 1_795_590_000_123_456_789);
    }
}
