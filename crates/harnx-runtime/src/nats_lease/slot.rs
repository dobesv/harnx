//! Writes to one lease key that keep it expiring.
//!
//! A lease's TTL is what frees its key when the holder dies, and a removed
//! lease must not leave a marker that stays forever. async-nats'
//! `create_with_ttl` drops the TTL when the key holds a delete or purge
//! marker, which every released or expired lease leaves behind, so creation
//! publishes the lease itself.
use super::*;

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
    pub(super) async fn create(&self, value: Vec<u8>, ttl: Duration) -> Result<Option<u64>> {
        match self.publish(value.clone(), 0, ttl).await {
            Ok(revision) => return Ok(Some(revision)),
            Err(error) if error.kind() == PublishErrorKind::WrongLastSequence => {}
            Err(error) => return Err(error).context("Failed to acquire NATS lease"),
        }
        let latest = harnx_nats_common::leader_reads::entry(self.bucket, self.key).await?;
        let Some(marker) = latest.filter(|entry| entry.operation != kv::Operation::Put) else {
            return Ok(None);
        };
        match self.publish(value, marker.revision, ttl).await {
            Ok(revision) => Ok(Some(revision)),
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
    ) -> Result<u64, jetstream::context::PublishError> {
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(
            NATS_EXPECTED_LAST_SUBJECT_SEQUENCE,
            expected_revision.to_string(),
        );
        headers.insert(NATS_MESSAGE_TTL, ttl.as_secs().to_string());
        let subject = format!("$KV.{}.{}", self.bucket.name, self.key);
        let ack = self
            .jetstream
            .publish_with_headers(subject, headers, value.into())
            .await?
            .await?;
        Ok(ack.sequence)
    }
}
