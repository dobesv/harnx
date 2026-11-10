//! Rows that nothing else removes expire on their own: a deleted session's
//! tombstone, and the row of a call made outside any session. A session's own
//! rows carry no TTL; they go when the session is deleted.
use super::*;
use async_nats::header::{NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, NATS_MESSAGE_TTL};
use std::time::{Duration, SystemTime};

/// How long a deleted session's tombstone refuses new calls. It only has to
/// outlast a worker that loaded the session's calls before the deletion, and
/// that worker's next lease renewal, seconds later, fails and stops its turn.
/// A session started again under the same agent and name has the same storage
/// key, so it can't run a journaled tool until its tombstone has passed.
pub(super) const TOMBSTONE_TTL: Duration = Duration::from_secs(60 * 60);

/// How long a standalone row lasts after its last write. Nothing replays these
/// rows, and once one holds its reply only a retried request reads it. A long
/// call may write nothing between its first row and its reply, and a row gone
/// by then turns the reply into an error, so this stays far above any call's
/// running time.
pub(super) const STANDALONE_ROW_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long the marker NATS leaves in place of an expired row lasts. A KV
/// bucket's config turns per-message TTLs on only together with these markers,
/// and the journal reads a marker as a missing row, so it need not last.
pub(super) const EXPIRY_MARKER_TTL: Duration = Duration::from_secs(60);

/// What the key of a row for a call with no parent session starts with.
pub(super) const STANDALONE_PREFIX: &str = "standalone/";

impl InvocationJournal {
    /// Write the first revision of `key`, failing if it already has one.
    pub(super) async fn create_row(&self, key: &str, value: Vec<u8>) -> Result<()> {
        match self.row_ttl(key) {
            Some(ttl) => self.0.create_with_ttl(key, value.into(), ttl).await?,
            None => self.0.create(key, value.into()).await?,
        };
        Ok(())
    }

    /// `kv::Store::update`, keeping the TTL `key`'s rows carry. async-nats sets
    /// a TTL only when it creates a key, and a revision written without one
    /// never expires.
    pub(super) async fn update_row(
        &self,
        key: &str,
        value: Vec<u8>,
        revision: u64,
    ) -> Result<u64, kv::UpdateError> {
        let Some(ttl) = self.row_ttl(key) else {
            return self.0.update(key, value.into(), revision).await;
        };
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, revision.to_string());
        headers.insert(NATS_MESSAGE_TTL, ttl.as_secs().to_string());
        let ack = self
            .1
            .publish_with_headers(format!("{}{key}", self.0.prefix), headers, value.into())
            .await?
            .await?;
        Ok(ack.sequence)
    }

    /// Record that `session` was deleted, for [`TOMBSTONE_TTL`].
    pub(super) async fn write_tombstone(&self, session: &str) -> Result<()> {
        let key = tombstone_key(session);
        if !self.accepts_ttl() {
            self.0.put(&key, "deleted".into()).await?;
            return Ok(());
        }
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(NATS_MESSAGE_TTL, TOMBSTONE_TTL.as_secs().to_string());
        self.1
            .publish_with_headers(format!("{}{key}", self.0.prefix), headers, "deleted".into())
            .await?
            .await?;
        Ok(())
    }

    /// Whether `session` was deleted less than [`TOMBSTONE_TTL`] ago. The age
    /// is checked here as well as by the broker, because a tombstone written
    /// before tombstones expired, or into a bucket that took no TTL, stays.
    pub(super) async fn recently_deleted(&self, session: &str) -> Result<bool> {
        Ok(self
            .live_entry(&tombstone_key(session))
            .await?
            .is_some_and(|tombstone| still_refuses(tombstone.created.into(), SystemTime::now())))
    }

    /// The TTL a row at `key` is written with: none for a session's rows, and
    /// none while the bucket takes no TTL.
    pub(super) fn row_ttl(&self, key: &str) -> Option<Duration> {
        (key.starts_with(STANDALONE_PREFIX) && self.accepts_ttl()).then_some(STANDALONE_ROW_TTL)
    }

    /// Whether the bucket takes per-message TTLs. One that predates them
    /// rejects a message that carries one until a tool server from this build
    /// reconciles it, and deleting a session has to work before that. A
    /// stream can't turn TTLs off again, so the cached config never claims
    /// TTLs the bucket doesn't take.
    fn accepts_ttl(&self) -> bool {
        self.0.stream.cached_info().config.allow_message_ttl
    }
}

fn tombstone_key(session: &str) -> String {
    format!("deleted/{session}")
}

/// Whether a tombstone the broker stamped `written` still refuses calls at
/// `now`. One that reads as newer than this host's clock still does.
fn still_refuses(written: SystemTime, now: SystemTime) -> bool {
    now.duration_since(written)
        .map_or(true, |age| age < TOMBSTONE_TTL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tombstone_refuses_calls_for_an_hour() {
        let written = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let second = Duration::from_secs(1);
        assert!(still_refuses(written, written));
        assert!(still_refuses(written, written + TOMBSTONE_TTL - second));
        assert!(!still_refuses(written, written + TOMBSTONE_TTL));
        assert!(
            still_refuses(written, written - second),
            "a clock behind the broker's must not let calls through"
        );
    }
}
