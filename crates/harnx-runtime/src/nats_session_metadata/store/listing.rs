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

impl SessionMetadataStore {
    pub(super) async fn bulk_entries(&self) -> Result<Entries> {
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
            return Ok(Entries::new());
        }
        let mut messages = consumer.messages().await?;
        let mut entries = Entries::new();
        while let Some(message) = messages.next().await {
            let message = message?;
            let info = message.info().map_err(anyhow::Error::from_boxed)?;
            let key = message
                .subject
                .strip_prefix(&self.store.prefix)
                .context("Unexpected subject in session metadata snapshot")?;
            let operation = message
                .headers
                .as_ref()
                .and_then(|headers| headers.get("KV-Operation"))
                .and_then(|value| value.as_str().parse::<kv::Operation>().ok())
                .unwrap_or(kv::Operation::Put);
            if operation == kv::Operation::Put {
                entries.insert(
                    key.to_owned(),
                    (message.payload.clone(), info.stream_sequence),
                );
            } else {
                entries.remove(key);
            }
            if info.pending == 0 {
                return Ok(entries);
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
