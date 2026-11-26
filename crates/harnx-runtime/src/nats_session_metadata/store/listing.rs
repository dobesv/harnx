//! Bulk session listing via ordered JetStream consumer (LastPerSubject).
//!
//! Replaces N×3 serial KV point lookups with a single-pass snapshot of the
//! `harnx_sessions` bucket. Falls back to `list_by_keys` on transport failure.
//! All callers (CLI, TUI, ACP server, web via harnx-serve) benefit without API change.
use super::*;
use async_nats::jetstream::consumer::{push::OrderedConfig, DeliverPolicy, ReplayPolicy};
use bytes::Bytes;
use std::collections::HashMap;

// Keep payloads until the consumer catches up: metadata, activity, and read-state
// arrive in revision order, not grouped by session.
type Entries = HashMap<String, (Bytes, u64)>;

/// One key's latest revision as the snapshot consumer delivered it, retaining
/// delete and purge markers rather than dropping them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub operation: kv::Operation,
    pub payload: Bytes,
    pub revision: u64,
}

/// A single-pass snapshot of every key in the `harnx_sessions` bucket.
///
/// Stats and other read-only consumers use this instead of [`SessionMetadataStore::list`]:
/// it keeps marker states (an A2A context Delete or Purge marker is a fact, not
/// an absence) and never falls back to per-key reads, which would trade a bulk
/// snapshot for N×3 point lookups. Absent markers can be distinguished from
/// absent keys through the operation, not the payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSnapshot {
    entries: HashMap<String, SnapshotEntry>,
}

impl SessionSnapshot {
    pub fn get(&self, key: &str) -> Option<&SnapshotEntry> {
        self.entries.get(key)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every key with its latest observed entry, marker states included.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SnapshotEntry)> {
        self.entries
            .iter()
            .map(|(key, entry)| (key.as_str(), entry))
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: Vec<(String, SnapshotEntry)>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
        }
    }
}

impl SessionMetadataStore {
    pub(super) async fn bulk_entries(&self) -> Result<Entries> {
        let snapshot = self.consume_snapshot().await?;
        Ok(snapshot
            .entries
            .into_iter()
            .filter(|(_, entry)| entry.operation == kv::Operation::Put)
            .map(|(key, entry)| (key, (entry.payload, entry.revision)))
            .collect())
    }

    /// A bulk-only snapshot of the bucket, errors included. There is no
    /// per-key fallback: callers that must not fan out per session rely on
    /// this failing loudly.
    pub async fn snapshot(&self) -> Result<SessionSnapshot> {
        self.consume_snapshot().await
    }

    async fn consume_snapshot(&self) -> Result<SessionSnapshot> {
        // Unlike watch_all(), the consumer exposes its initial pending count.
        // An empty bucket must return immediately, not wait for a future write.
        let consumer = self
            .store
            .stream
            .create_consumer(OrderedConfig {
                deliver_subject: self.client.new_inbox(),
                description: Some("session list snapshot".into()),
                filter_subject: format!("{}>", self.store.prefix),
                deliver_policy: DeliverPolicy::LastPerSubject,
                replay_policy: ReplayPolicy::Instant,
                ..Default::default()
            })
            .await?;
        if consumer.cached_info().num_pending == 0 {
            return Ok(SessionSnapshot::default());
        }
        let mut messages = consumer.messages().await?;
        let mut entries: HashMap<String, SnapshotEntry> = HashMap::new();
        while let Some(message) = messages.next().await {
            let message = message?;
            let info = message.info().map_err(anyhow::Error::from_boxed)?;
            let key = message
                .subject
                .strip_prefix(&self.store.prefix)
                .context("Unexpected subject in session metadata snapshot")?;
            // LastPerSubject delivers one message per key, its latest revision,
            // so an insert always reflects the newest state — live or marker.
            // The same decoder the authoritative leader reads use: a revision
            // the server removed itself carries a marker reason, not a KV
            // operation, and must not count as a live entry.
            entries.insert(
                key.to_owned(),
                SnapshotEntry {
                    operation: harnx_nats_common::markers::operation(message.headers.as_ref()),
                    payload: message.payload.clone(),
                    revision: info.stream_sequence,
                },
            );
            if info.pending == 0 {
                return Ok(SessionSnapshot { entries });
            }
        }
        anyhow::bail!("Session metadata snapshot ended before catching up")
    }
}

pub(super) fn join_entries(entries: Entries) -> Result<Vec<ListedSession>> {
    let mut sessions = Vec::new();
    for (key, (payload, revision)) in &entries {
        let Some(storage_key) = key
            .strip_prefix("sessions/")
            .and_then(|key| key.strip_suffix("/meta"))
        else {
            continue;
        };
        let metadata: SessionMetadata = serde_json::from_slice(payload)
            .with_context(|| format!("Failed to deserialize session metadata '{key}'"))?;
        metadata.validate_storage_key(storage_key)?;
        let activity = parse_activity(&entries, storage_key);
        let read_state = parse_read_state(&entries, storage_key);
        sessions.push(ListedSession {
            metadata,
            metadata_revision: *revision,
            activity,
            unread: read_state.is_unread(),
        });
    }
    // keys() delivered metadata in revision order. Preserve that stable tie
    // order when two agents have the same local ID and activity timestamp.
    sessions.sort_by_key(|session| session.metadata_revision);
    sort_sessions_by_activity(&mut sessions);
    Ok(sessions)
}

fn parse_activity(entries: &Entries, storage_key: &str) -> Option<SessionActivity> {
    let key = activity_key(storage_key);
    let (payload, _) = entries.get(&key)?;
    serde_json::from_slice::<SessionActivity>(payload).map(Some).unwrap_or_else(|error| {
        log::warn!(
            "could not read session activity; using metadata timestamp: bucket={} key={} error={error:#}",
            SESSION_METADATA_BUCKET, key
        );
        None
    })
}

fn parse_read_state(entries: &Entries, storage_key: &str) -> SessionReadState {
    let key = read_cursor_key(storage_key, "default");
    let Some((payload, _)) = entries.get(&key) else {
        return SessionReadState::default();
    };
    serde_json::from_slice(payload).unwrap_or_else(|error| {
        log::warn!(
            "could not read session read-state; defaulting to read: bucket={} session_id={} error={error:#}",
            SESSION_METADATA_BUCKET, storage_key
        );
        SessionReadState::default()
    })
}
