use anyhow::{Context, Result};
use async_nats::jetstream;
use std::future::Future;

use crate::nats_admin::stream_missing;
use crate::nats_session_log::{stream_name_for_session, NatsSessionLog};
use crate::nats_session_metadata::{
    MetadataRecord, SessionInitializer, SessionMetadata, SessionMetadataStore,
};

/// Result of trying to reserve a resolved session ID. A returned record is the
/// canonical record to authorize, regardless of who attempted the reservation.
#[derive(Debug)]
pub enum SessionMetadataClaim {
    /// Canonical metadata and its initial activity were created.
    Created(MetadataRecord),
    /// Canonical metadata already exists. No activity repair was performed.
    Existing(MetadataRecord),
    /// A transcript stream exists, but a leader reread found no metadata.
    Orphaned,
    /// The metadata create collided, but a leader reread found no winner.
    Vanished,
}

/// Reserve metadata for an explicit local ID, or read the canonical record.
///
/// `store` selects the resolved cluster, and `initializer` supplies the resolved
/// agent and initial properties (including any trusted user identity stamps).
/// `session_id` is used as-is and must not be blank. The transcript is only
/// checked for existence; even an empty stream without metadata blocks a claim.
/// No transcript is created or appended to, and an existing record and its
/// activity are left unchanged. Broker failures are returned as errors.
pub async fn create_or_read_session_metadata(
    store: &SessionMetadataStore,
    session_id: &str,
    initializer: &SessionInitializer,
) -> Result<SessionMetadataClaim> {
    anyhow::ensure!(
        !session_id.trim().is_empty(),
        "session ID must not be blank"
    );
    let key = initializer.session_key(session_id);
    create_or_read_metadata(
        store,
        &key,
        session_id,
        initializer,
        transcript_exists(store.jetstream(), &key),
    )
    .await
}

async fn transcript_exists(jetstream: &jetstream::Context, key: &str) -> Result<bool> {
    let name = stream_name_for_session(key);
    match jetstream.get_stream(&name).await {
        Ok(_) => Ok(true),
        Err(error) if stream_missing(&error.kind()) => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect session stream '{name}'"))
        }
    }
}

pub(super) async fn ensure_session_metadata(
    store: &SessionMetadataStore,
    transcript: &NatsSessionLog,
    session_id: &str,
    initializer: &SessionInitializer,
) -> Result<()> {
    let key = initializer.session_key(session_id);
    // Legacy opens reject only a nonempty transcript without metadata, and
    // repair activity before use. Keep both behaviors out of the claim path.
    let has_entries = async {
        let entries = transcript
            .load_events_async()
            .await
            .context("failed to inspect transcript before creating session metadata")?;
        Ok(!entries.is_empty())
    };
    // This constructor runs inside deep TUI futures. Keep the nested probe
    // and KV futures off their stack.
    let outcome = Box::pin(create_or_read_metadata(
        store,
        &key,
        session_id,
        initializer,
        has_entries,
    ))
    .await?;
    match outcome {
        SessionMetadataClaim::Created(_) => Ok(()),
        SessionMetadataClaim::Existing(_) => store.ensure_reserved_activity(&key).await,
        SessionMetadataClaim::Orphaned => anyhow::bail!(
            "session '{session_id}' has transcript entries but no canonical metadata; legacy or inconsistent sessions are not supported"
        ),
        SessionMetadataClaim::Vanished => {
            anyhow::bail!("session metadata creation race for '{session_id}' had no winner")
        }
    }
}

async fn create_or_read_metadata(
    store: &SessionMetadataStore,
    key: &str,
    session_id: &str,
    initializer: &SessionInitializer,
    probe_transcript: impl Future<Output = Result<bool>>,
) -> Result<SessionMetadataClaim> {
    let existing = if let Some(record) = store.get(key).await? {
        record
    } else if probe_transcript.await? {
        // A creator may have reserved metadata and opened the stream since
        // the first read. Check the leader again before calling it an orphan.
        let Some(record) = store.get(key).await? else {
            return Ok(SessionMetadataClaim::Orphaned);
        };
        record
    } else {
        let metadata = SessionMetadata::new(session_id, initializer.clone());
        if let Some(revision) = store.create(&metadata).await? {
            return Ok(SessionMetadataClaim::Created(MetadataRecord {
                metadata,
                revision,
            }));
        }
        let Some(winner) = store.get(key).await? else {
            return Ok(SessionMetadataClaim::Vanished);
        };
        winner
    };
    existing.metadata.validate_initializer(initializer)?;
    Ok(SessionMetadataClaim::Existing(existing))
}
