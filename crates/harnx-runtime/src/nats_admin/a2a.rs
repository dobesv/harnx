//! Session deletion fences A2A admission before removing shared identities.
use crate::{
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_metadata::SessionMetadataStore,
};
use anyhow::{ensure, Result};
use async_nats::jetstream::{self, kv::Operation};
use harnx_core::crypto::sha256;
use serde_json::Value;
mod hold;
mod state;
pub(crate) use hold::{
    archive_key_when_inspected, context_key, evaluate_a2a_holds, leader_read_a2a_holds,
    registry_key, reservation_key_from_registry, A2aHoldInputs, A2aHoldSummary, RecordFact,
};
pub(crate) use state::unresolved;
use state::{ensure_first_settled, entry_unresolved};

/// The deletion fence refused the session for live A2A state: an unresolved
/// context, an unsettled first-message reservation, or scoped-lease
/// contention. A refusal, not a transport or malformed-state failure — the
/// manual cleanup pass reports it as a late skipped session, while the
/// worker path still counts it the way it always did (an error).
#[derive(Debug)]
pub(crate) struct FenceRefused(&'static str);

impl FenceRefused {
    fn new(reason: &'static str) -> Self {
        Self(reason)
    }
}

// qual:allow(dry, boilerplate) reason: "One fixed sentence identifies the typed A2A refusal in logs; derive_more is not a workspace dependency and a macro for one impl is more boilerplate than the impl."
impl std::fmt::Display for FenceRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for FenceRefused {}

pub(crate) struct DeletionFence {
    lease: NatsSessionLease,
    registry: Option<Value>,
}

/// Acquire the scoped A2A lease that serializes deletion with A2A workers.
/// `Err(FenceRefused)` means live ownership; transport failures propagate
/// unchanged so the caller can distinguish them from a clean refusal.
async fn fence_lease(metadata: &SessionMetadataStore, storage: &str) -> Result<NatsSessionLease> {
    Ok(NatsSessionLease::acquire_scoped(
        NatsLeaseAcquireParams {
            jetstream: metadata.jetstream().clone(),
            session_id: storage,
            worker_id: format!("a2a-session-gc:{}", std::process::id()),
            generation: 0,
            config: NatsLeaseConfig {
                replicas: metadata.replicas(),
                ..Default::default()
            },
            session_metadata: None,
        },
        "a2a",
    )
    .await?
    .ok_or_else(|| FenceRefused::new("A2A session is owned; retry deletion after settlement"))?)
}

pub(crate) async fn fence(
    metadata: &SessionMetadataStore,
    storage: &str,
) -> Result<Option<DeletionFence>> {
    let kv = metadata.kv_store();
    let registry_key = format!("a2a.registry.{}", sha256(storage));
    let registry = metadata
        .leader_value(&registry_key)
        .await?
        .map(|bytes| serde_json::from_slice::<Value>(&bytes))
        .transpose()?;
    let context_key = format!("sessions/{storage}/a2a/context");
    let context = metadata.leader_entry(&context_key).await?;
    if registry.is_none() && context.is_none() {
        return Ok(None);
    }
    let lease = fence_lease(metadata, storage).await?;
    // Validate exactly the revision used by purge. An old prepared owner CAS
    // can still land after a lease expires; separate point checks aren't a fence.
    let current = metadata.leader_entry(&context_key).await?;
    let validation = async {
        if current
            .as_ref()
            .map(entry_unresolved)
            .transpose()?
            .unwrap_or(false)
        {
            return Err(FenceRefused::new(
                "A2A task has unresolved stop, projections or outbox; deletion refused",
            )
            .into());
        }
        if current
            .as_ref()
            .is_none_or(|entry| entry.operation == Operation::Put)
        {
            ensure_first_settled(metadata, registry.as_ref(), storage, current.as_ref()).await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = validation {
        lease.release().await?;
        return Err(error);
    }
    if current
        .as_ref()
        .is_none_or(|entry| entry.operation == Operation::Put)
    {
        kv.purge_expect_revision(
            &context_key,
            Some(current.map_or(0, |entry| entry.revision)),
        )
        .await?;
    }
    Ok(Some(DeletionFence { lease, registry }))
}

impl DeletionFence {
    pub(crate) async fn finish(self, metadata: &SessionMetadataStore, storage: &str) -> Result<()> {
        let js = metadata.jetstream();
        match js.get_stream(crate::a2a_events::STREAM).await {
            Ok(stream) => {
                stream
                    .purge()
                    .filter(format!("a2a.tasks.{storage}.*"))
                    .await?;
            }
            Err(error) if super::stream_missing(&error.kind()) => (),
            Err(error) => return Err(error.into()),
        }
        // Registry goes last, so a failed deletion can retry the first identity cleanup.
        if let Some(identity) = self
            .registry
            .as_ref()
            .and_then(|doc| doc.pointer("/first/identity"))
        {
            let key = format!(
                "a2a/first-messages/{}",
                sha256(&serde_json::to_string(identity)?)
            );
            if let Some(entry) = metadata.leader_entry(&key).await? {
                if entry.operation == Operation::Put {
                    let first: Value = serde_json::from_slice(&entry.value)?;
                    ensure!(
                        first
                            .pointer("/allocation/storage_key")
                            .and_then(Value::as_str)
                            == Some(storage),
                        "A2A first identity belongs to another session"
                    );
                    metadata
                        .kv_store()
                        .purge_expect_revision(key, Some(entry.revision))
                        .await?;
                }
            }
        }
        metadata
            .kv_store()
            .purge(format!("a2a.registry.{}", sha256(storage)))
            .await?;
        self.lease.release().await
    }
}

pub(crate) async fn metadata(js: &jetstream::Context) -> Result<Option<SessionMetadataStore>> {
    match js
        .get_key_value(crate::nats_session_metadata::SESSION_METADATA_BUCKET)
        .await
    {
        Ok(kv) => Ok(Some(SessionMetadataStore::from_store(
            kv,
            js.client().clone(),
        ))),
        Err(error) if super::kv_bucket_missing(&error) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
