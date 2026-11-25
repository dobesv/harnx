//! Writes to one lease key that keep it expiring.
//!
//! A lease's TTL is what frees its key when the holder dies, and a removed
//! lease must not leave a marker that stays forever. async-nats'
//! `create_with_ttl` drops the TTL when the key holds a delete or purge
//! marker, which every released or expired lease leaves behind, so creation
//! publishes the lease itself.
use super::*;

/// A TTL-bearing create the broker accepted.
///
/// `sent_at` anchors the holder's confirmation deadline: it is captured
/// immediately before the publish that succeeded — including a create over a
/// marker after a reclaim — so a slow acquisition before it cannot spend the
/// lease's renewal budget, and the broker's acknowledgement after it cannot
/// overstate the remaining TTL.
#[derive(Debug)]
pub(super) struct CreatedLease {
    pub(super) revision: u64,
    pub(super) sent_at: time::Instant,
    /// When the broker's acknowledgement was observed. An acknowledgement
    /// that lands after `sent_at + ttl` means the lease may already have
    /// expired by the time the holder learns it owns the key; only the
    /// worker-activation policy checks that. A test-only seam can move this
    /// past the deadline to prove an expired lease is never used.
    pub(super) acked_at: time::Instant,
}

/// One lease key, the handles that write it, and how long the marker a
/// removal leaves on it lasts.
pub(super) struct LeaseSlot<'a> {
    pub(super) jetstream: &'a jetstream::Context,
    pub(super) bucket: &'a kv::Store,
    pub(super) key: &'a str,
    pub(super) marker_ttl: Duration,
}

impl LeaseSlot<'_> {
    /// Write `value` as the key's lease, expiring after `ttl`, if no lease
    /// holds the key. A delete or purge marker doesn't hold it. `Ok(None)`
    /// means another lease does, or appeared meanwhile.
    pub(super) async fn create(
        &self,
        value: Vec<u8>,
        ttl: Duration,
    ) -> Result<Option<CreatedLease>> {
        match self.publish(value.clone(), 0, ttl).await {
            Ok(created) => return Ok(Some(created)),
            Err(error) if error.kind() == PublishErrorKind::WrongLastSequence => {}
            Err(error) => return Err(error).context("Failed to acquire NATS lease"),
        }
        let latest = harnx_nats_common::leader_reads::entry(self.bucket, self.key).await?;
        let Some(marker) = latest.filter(|entry| entry.operation != kv::Operation::Put) else {
            return Ok(None);
        };
        match self.publish(value, marker.revision, ttl).await {
            Ok(created) => Ok(Some(created)),
            Err(error) if error.kind() == PublishErrorKind::WrongLastSequence => Ok(None),
            Err(error) => Err(error).context("Failed to acquire NATS lease"),
        }
    }

    /// Replace the lease at `revision` with a purge marker that expires
    /// after `marker_ttl`. The revision check fences a holder that renewed
    /// meanwhile; once the marker expires, that holder's next renewal still
    /// fails, since the revision it expects no longer exists.
    pub(super) async fn remove(&self, revision: u64) -> Result<(), kv::UpdateError> {
        self.bucket
            .purge_expect_revision_with_ttl(self.key, revision, self.marker_ttl)
            .await
    }

    async fn publish(
        &self,
        value: Vec<u8>,
        expected_revision: u64,
        ttl: Duration,
    ) -> Result<CreatedLease, jetstream::context::PublishError> {
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(
            NATS_EXPECTED_LAST_SUBJECT_SEQUENCE,
            expected_revision.to_string(),
        );
        headers.insert(NATS_MESSAGE_TTL, ttl.as_secs().to_string());
        let subject = kv_subject(&self.bucket.name, self.key);
        let sent_at = time::Instant::now();
        let ack = self
            .jetstream
            .publish_with_headers(subject, headers, value.into())
            .await?
            .await?;
        Ok(CreatedLease {
            revision: ack.sequence,
            sent_at,
            acked_at: create_ack_observed_at(),
        })
    }
}

/// When the broker's create acknowledgement was observed. Tests can delay
/// this past the create's own TTL (see [`set_create_ack_delay_for_test`]) to
/// prove an acknowledgement that outlives the lease never protects work.
fn create_ack_observed_at() -> time::Instant {
    #[cfg(test)]
    {
        time::Instant::now() + test_ack_delay()
    }
    #[cfg(not(test))]
    {
        time::Instant::now()
    }
}

#[cfg(test)]
static CREATE_ACK_DELAY: std::sync::Mutex<Option<Duration>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn test_ack_delay() -> Duration {
    let guard = CREATE_ACK_DELAY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.unwrap_or_default()
}

// Test-only: make every create's acknowledgement look `delay` later than
// it arrived, for one process. Reset with [`clear_create_ack_delay_for_test`].
#[cfg(test)]
pub(super) fn set_create_ack_delay_for_test(delay: Duration) {
    *CREATE_ACK_DELAY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(delay);
}

#[cfg(test)]
pub(super) fn clear_create_ack_delay_for_test() {
    *CREATE_ACK_DELAY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}
