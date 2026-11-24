//! Durable middleware checkpoints and interaction ownership.
//! The round-start key and read-only probe remain compatible with T2.1 workers;
//! live writers commit ownership before any physical dispatch.

use anyhow::{ensure, Context, Result};
use async_nats::jetstream::{self, context::GetStreamErrorKind, ErrorCode};
use serde::{Deserialize, Serialize};

pub(crate) mod guard;

pub(crate) mod attempt;
mod completion;
mod evaluation;
mod listing;
pub mod live;
mod live_resume;
pub(crate) use live_resume::live_resume_owned;
pub(crate) mod records;
pub(crate) mod recovery;
mod resume;
mod snapshot;
pub(crate) mod store;
mod suspension;
pub use crate::interactions::{
    accept_interaction_response, apply_interaction_response, derive_pending_interactions,
    interaction_request_from_entry, InteractionDecisionOutcome, PendingInteraction,
};

/// Shared by all workers that may take over a session. No TTL: ownership must
/// survive a human wait and worker rollback, until ToolResults or session deletion purges the stream subjects.
pub const ROUND_START_BUCKET: &str = "harnx_interaction_checkpoints";

/// Private ownership proof, written before any call in the round starts. The
/// identity is checked on read so malformed/mis-keyed data cannot authorize replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundStart {
    pub version: u32,
    pub session_storage_key: String,
    pub tool_round_seq: u64,
    pub fence_token: u64,
}

/// Hash the whole storage key, including its namespace, into one NATS key token.
pub fn round_start_key(session_storage_key: &str, tool_round_seq: u64) -> String {
    format!(
        "sessions.{}.rounds.{tool_round_seq}.start",
        harnx_core::crypto::sha256(session_storage_key)
    )
}

/// Only a confirmed missing stream/key proves a pre-interaction round. Broker,
/// permissions, decode and identity errors must be handled as parked work.
pub async fn round_started(
    js: &jetstream::Context,
    session_storage_key: &str,
    tool_round_seq: u64,
) -> Result<bool> {
    if crate::fault_injection::is_store_error_simulated(session_storage_key) {
        return Err(anyhow::anyhow!("simulated checkpoint store unreachable"));
    }
    // get_key_value wraps every stream lookup error as GetBucket. Inspect the
    // exact server error first; a timeout is not evidence that the bucket is absent.
    match js.get_stream(format!("KV_{ROUND_START_BUCKET}")).await {
        Ok(_) => {}
        Err(error)
            if matches!(error.kind(), GetStreamErrorKind::JetStream(ref error)
                if error.kind() == ErrorCode::STREAM_NOT_FOUND) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error).context("read interaction checkpoint bucket"),
    }
    let store = js.get_key_value(ROUND_START_BUCKET).await?;
    let Some(bytes) = store
        .get(round_start_key(session_storage_key, tool_round_seq))
        .await?
    else {
        return Ok(false);
    };
    let start: RoundStart =
        serde_json::from_slice(&bytes).context("decode interaction round-start")?;
    ensure!(
        start.version == 1,
        "unsupported interaction round-start version"
    );
    ensure!(
        start.session_storage_key == session_storage_key && start.tool_round_seq == tool_round_seq,
        "interaction round-start identity mismatch"
    );
    Ok(true)
}

#[cfg(feature = "fault-injection")]
// qual:test_helper
pub async fn test_guard_is_parked(
    js: &async_nats::jetstream::Context,
    storage_key: &str,
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
) -> Result<bool> {
    guard::is_parked(js, storage_key, entries).await
}

#[cfg(feature = "fault-injection")]
// qual:test_helper
pub async fn test_purge_checkpoints(
    js: &async_nats::jetstream::Context,
    storage_key: &str,
    round_seq: u64,
    call_id: &str,
) -> Result<Option<String>> {
    let store = store::CheckpointStore::ensure(js, 1).await?;
    let attempts = store
        .read_ordered_attempts(storage_key, round_seq, call_id)
        .await?
        .map_err(anyhow::Error::msg)?;
    let reference = attempts
        .last()
        .and_then(|a| a.checkpoint.clone())
        .context("rerun attempt must reference a checkpoint")?;
    let kv = js.get_key_value(ROUND_START_BUCKET).await?;
    anyhow::ensure!(
        kv.get(&reference).await?.is_some(),
        "checkpoint must exist before purge"
    );
    kv.purge(&reference).await?;
    anyhow::ensure!(
        kv.get(&reference).await?.is_none(),
        "checkpoint must be missing after purge"
    );
    Ok(Some(reference))
}
