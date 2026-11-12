use crate::{
    config::Config,
    nats_lease::NatsLeaseConfig,
    nats_session_log::stream_name_for_session,
    nats_session_metadata::{SessionMetadataStore, SESSION_METADATA_BUCKET},
};
use anyhow::{Context, Result};
use async_nats::jetstream::{
    context::{DeleteStreamErrorKind, GetStreamErrorKind, KeyValueErrorKind},
    kv::Operation,
    ErrorCode,
};
use std::error::Error as _;
pub(crate) mod a2a;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDeleteResult {
    pub stream_deleted: bool,
    pub lease_deleted: bool,
    pub metadata_keys_deleted: usize,
    pub attachments_deleted: usize,
}

impl SessionDeleteResult {
    pub fn removed_anything(&self) -> bool {
        self.stream_deleted
            || self.lease_deleted
            || self.metadata_keys_deleted > 0
            || self.attachments_deleted > 0
    }
}

/// Delete exactly one agent's session. A local ID alone is never sufficient.
pub async fn delete_remote_session(
    config: &Config,
    cluster: &str,
    agent: &str,
    session_id: &str,
) -> Result<SessionDeleteResult> {
    anyhow::ensure!(
        !agent.trim().is_empty() && agent != harnx_core::agent_config::TEMP_AGENT_NAME,
        "an explicit agent is required"
    );
    let key = harnx_core::session_identity::session_key(Some(agent), session_id);
    delete_remote_session_by_key(config, cluster, &key).await
}

pub(crate) async fn delete_remote_session_by_key(
    config: &Config,
    cluster: &str,
    session_id: &str,
) -> Result<SessionDeleteResult> {
    let jetstream = config.nats_jetstream(cluster).await?;
    let lease_bucket = load_optional_lease_bucket(config, cluster).await?;
    delete_session_state(&jetstream, lease_bucket, session_id, StreamDeletion::First).await
}

/// When a session deletion removes the session's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamDeletion {
    /// Before the rest, so nothing can append to the session meanwhile.
    First,
    /// After the rest, for an orphan that only its stream lets the GC find
    /// again: a step that fails part way leaves the stream, and with it the
    /// rest of the session, for the next pass.
    Last,
}

/// Everything [`delete_remote_session_by_key`] removes, through handles the
/// caller already holds.
pub(crate) async fn delete_session_state(
    jetstream: &async_nats::jetstream::Context,
    lease_bucket: Option<async_nats::jetstream::kv::Store>,
    session_id: &str,
    stream: StreamDeletion,
) -> Result<SessionDeleteResult> {
    let a2a = A2aDeletion::begin(jetstream, session_id).await?;
    let mut stream_deleted =
        stream == StreamDeletion::First && delete_session_stream(jetstream, session_id).await?;
    let result = delete_session_leftovers(jetstream, lease_bucket, session_id).await?;
    a2a.finish(session_id).await?;
    if stream == StreamDeletion::Last {
        stream_deleted = delete_session_stream(jetstream, session_id).await?;
    }
    Ok(SessionDeleteResult {
        stream_deleted,
        ..result
    })
}

/// The A2A admission fence a session deletion holds from start to finish.
struct A2aDeletion {
    metadata: Option<SessionMetadataStore>,
    fence: Option<a2a::DeletionFence>,
}

impl A2aDeletion {
    async fn begin(jetstream: &async_nats::jetstream::Context, session_id: &str) -> Result<Self> {
        let metadata = a2a::metadata(jetstream).await?;
        let fence = match &metadata {
            Some(metadata) => a2a::fence(metadata, session_id).await?,
            None => None,
        };
        Ok(Self { metadata, fence })
    }

    async fn finish(self, session_id: &str) -> Result<()> {
        if let (Some(fence), Some(metadata)) = (self.fence, self.metadata) {
            fence.finish(&metadata, session_id).await?;
        }
        Ok(())
    }
}

/// A session's lease, journal rows, metadata keys, attachments and plans.
async fn delete_session_leftovers(
    jetstream: &async_nats::jetstream::Context,
    lease_bucket: Option<async_nats::jetstream::kv::Store>,
    session_id: &str,
) -> Result<SessionDeleteResult> {
    let lease_deleted = delete_session_lease(lease_bucket, session_id).await?;
    purge_session_journal(jetstream, session_id).await?;
    let metadata_keys_deleted = purge_session_metadata(jetstream, session_id).await?;
    // delete_owner removes media and plans, legacy attachments included.
    let attachments_deleted = harnx_blob_store::delete_owner(jetstream, session_id).await?;
    Ok(SessionDeleteResult {
        stream_deleted: false,
        lease_deleted,
        metadata_keys_deleted,
        attachments_deleted,
    })
}

async fn purge_session_journal(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<()> {
    match jetstream
        .get_key_value(harnx_toolset_server::invocation_journal::BUCKET)
        .await
    {
        Ok(store) => {
            harnx_toolset_server::invocation_journal::InvocationJournal::from_store(
                jetstream, store,
            )
            .purge_session(session_id)
            .await
        }
        Err(error) if kv_bucket_missing(&error) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn purge_session_metadata(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<usize> {
    let store = match jetstream.get_key_value(SESSION_METADATA_BUCKET).await {
        Ok(store) => store,
        Err(error) if kv_bucket_missing(&error) => return Ok(0),
        Err(error) => return Err(error).context("failed to open session metadata for deletion"),
    };
    SessionMetadataStore::from_store(store, jetstream.client().clone())
        .purge_session_prefix(session_id)
        .await
}

async fn load_optional_lease_bucket(
    config: &Config,
    cluster: &str,
) -> Result<Option<async_nats::jetstream::kv::Store>> {
    match config.nats_kv_bucket(cluster, "harnx_leases").await {
        Ok(bucket) => Ok(Some(bucket)),
        Err(error) => {
            // Check if error chain contains a KeyValueError with bucket-not-found
            if error
                .chain()
                .find_map(|cause| {
                    cause.downcast_ref::<async_nats::jetstream::context::KeyValueError>()
                })
                .is_some_and(kv_bucket_missing)
            {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

/// Delete one session's JetStream stream by its storage key. Orphan-stream
/// reconciliation calls this directly once it has verified no other session
/// state (metadata, leases, A2A tasks) exists.
pub(crate) async fn delete_session_stream(
    jetstream: &async_nats::jetstream::Context,
    storage_key: &str,
) -> Result<bool> {
    let stream_name = stream_name_for_session(storage_key);
    match jetstream.delete_stream(&stream_name).await {
        Ok(_) => Ok(true),
        Err(error) if delete_stream_missing(&error.kind()) => Ok(false),
        Err(error) => Err(error).with_context(|| {
            format!("Failed to delete session stream '{stream_name}' for '{storage_key}'")
        }),
    }
}

/// Remove the session's lease, leaving a purge marker that expires. A marker
/// already on the key is replaced too, so one an older build left without a
/// TTL goes with its session. The purge is conditioned on the revision read
/// here, so a lease taken or renewed meanwhile fails the deletion, which can
/// be retried, instead of being removed from under its holder. `true` means a
/// live lease was removed.
async fn delete_session_lease(
    lease_bucket: Option<async_nats::jetstream::kv::Store>,
    session_id: &str,
) -> Result<bool> {
    let Some(lease_bucket) = lease_bucket else {
        return Ok(false);
    };
    let config = NatsLeaseConfig::default();
    let lease_key = config.key_for_session(session_id);
    let entry = harnx_nats_common::leader_reads::entry(&lease_bucket, &lease_key)
        .await
        .with_context(|| format!("Failed to inspect session lease key '{lease_key}'"))?;
    let Some(entry) = entry else {
        return Ok(false);
    };
    lease_bucket
        .purge_expect_revision_with_ttl(&lease_key, entry.revision, config.tombstone_ttl)
        .await
        .with_context(|| format!("Failed to delete session lease key '{lease_key}'"))?;
    Ok(entry.operation == Operation::Put)
}

/// Whether a get-stream error means the stream does not exist.
pub(crate) fn stream_missing(kind: &GetStreamErrorKind) -> bool {
    match kind {
        GetStreamErrorKind::JetStream(error) => error.kind() == ErrorCode::STREAM_NOT_FOUND,
        _ => false,
    }
}

fn delete_stream_missing(kind: &DeleteStreamErrorKind) -> bool {
    match kind {
        DeleteStreamErrorKind::JetStream(error) => error.kind() == ErrorCode::STREAM_NOT_FOUND,
        _ => false,
    }
}

/// Check if an async_nats KeyValueError indicates a missing bucket (stream not found).
pub fn kv_bucket_missing(error: &async_nats::jetstream::context::KeyValueError) -> bool {
    match error.kind() {
        KeyValueErrorKind::GetBucket => error
            .source()
            .and_then(|source| {
                source.downcast_ref::<async_nats::jetstream::context::GetStreamError>()
            })
            .is_some_and(|stream_error| stream_missing(&stream_error.kind())),
        _ => false,
    }
}
