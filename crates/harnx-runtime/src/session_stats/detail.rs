//! One session's storage report, from targeted leader reads. The session
//! view may spend per-session requests; the aggregate views may not.
use super::report::{
    OwnedAttachments, OwnedCount, OwnedPlans, SessionHolds, SessionStatsDetail,
    SessionStreamSummary, StatsAgentSource,
};
use crate::nats_admin::a2a::{evaluate_a2a_holds, A2aHoldInputs};
use crate::nats_lease::NatsLeaseConfig;
use crate::nats_session_log::stream_name_for_session;
use crate::nats_session_metadata::MetadataRecord;
use anyhow::{Context as _, Result};
use async_nats::jetstream::stream::Info as StreamInfo;
use chrono::{DateTime, Utc};

pub(super) async fn collect_session_detail(
    cluster: &str,
    jetstream: &async_nats::jetstream::Context,
    agent: &str,
    session_id: &str,
) -> Result<SessionStatsDetail> {
    let storage_key = harnx_core::session_identity::session_key(Some(agent), session_id);
    let store = super::snapshot::open_metadata_store(jetstream).await?;
    let record = session_record(store.as_ref(), agent, session_id, cluster).await?;
    let activity = match &store {
        // Tolerant read, the same one cleanup uses: malformed activity JSON
        // must not fail the report, only transport/JetStream errors must.
        Some(store) => store.get_activity_for_cleanup(&storage_key).await?,
        None => None,
    };
    let stream = read_stream_summary(jetstream, &storage_key).await?;
    let lease_store = open_lease_store(jetstream).await?;
    let holds = read_holds(store.as_ref(), lease_store.as_ref(), &storage_key).await?;
    Ok(SessionStatsDetail {
        view: "session",
        cluster: cluster.to_string(),
        collected_at: Utc::now(),
        agent: StatsAgentSource::from_metadata(&record.metadata.agent),
        session_id: session_id.to_string(),
        storage_key: storage_key.clone(),
        metadata_revision: record.revision,
        metadata_created_at: record.metadata.created_at,
        activity,
        stream_present: stream.is_some(),
        stream,
        holds,
        journal_rows: journal_rows(jetstream, &storage_key).await?,
        checkpoints: checkpoints(jetstream, &storage_key).await?,
        plans: plans(jetstream, &storage_key).await?,
        attachments: attachments(jetstream, &storage_key).await?,
    })
}

/// The named session's canonical metadata, or a clear not-found error. An
/// absent metadata bucket means no session exists, the same as a missing key.
async fn session_record(
    store: Option<&crate::nats_session_metadata::SessionMetadataStore>,
    agent: &str,
    session_id: &str,
    cluster: &str,
) -> Result<MetadataRecord> {
    let not_found = || {
        anyhow::anyhow!(
            "session '{session_id}' of agent '{agent}' not found on cluster '{cluster}'"
        )
    };
    let Some(store) = store else {
        return Err(not_found());
    };
    store
        .get_for_agent(session_id, agent)
        .await
        .with_context(|| format!("Failed to read session metadata on cluster '{cluster}'"))?
        .ok_or_else(not_found)
}

async fn open_lease_store(
    jetstream: &async_nats::jetstream::Context,
) -> Result<Option<async_nats::jetstream::kv::Store>> {
    let bucket = NatsLeaseConfig::default().bucket;
    match jetstream.get_key_value(&bucket).await {
        Ok(store) => Ok(Some(store)),
        Err(error) if crate::nats_admin::kv_bucket_missing(&error) => Ok(None),
        Err(error) => Err(anyhow::Error::from(error))
            .with_context(|| format!("Failed to open NATS lease bucket '{bucket}'")),
    }
}

fn stream_summary(info: &StreamInfo) -> SessionStreamSummary {
    let has_messages = info.state.messages > 0;
    SessionStreamSummary {
        name: info.config.name.clone(),
        bytes: info.state.bytes,
        messages: info.state.messages,
        first_sequence: has_messages.then_some(info.state.first_sequence),
        last_sequence: has_messages.then_some(info.state.last_sequence),
        created_at: chrono_time(info.created),
        last_message_time: has_messages.then(|| chrono_time(info.state.last_timestamp)),
    }
}

/// JetStream infos carry `time` timestamps; the reports serialize `chrono`.
/// A timestamp outside chrono's range cannot occur for a live broker clock.
fn chrono_time(timestamp: time::OffsetDateTime) -> DateTime<Utc> {
    DateTime::from_timestamp(timestamp.unix_timestamp(), timestamp.nanosecond())
        .unwrap_or(DateTime::UNIX_EPOCH)
}

/// The session's stream, or `None` when no `SESSION_` stream exists for it.
/// An absent stream is a normal state: metadata-only sessions have one.
async fn read_stream_summary(
    jetstream: &async_nats::jetstream::Context,
    storage_key: &str,
) -> Result<Option<SessionStreamSummary>> {
    let name = stream_name_for_session(storage_key);
    match jetstream.get_stream(&name).await {
        Ok(stream) => Ok(Some(stream_summary(stream.cached_info()))),
        Err(error) if crate::nats_admin::stream_missing(&error.kind()) => Ok(None),
        Err(error) => Err(anyhow::Error::from(error))
            .with_context(|| format!("Failed to inspect session stream '{name}'")),
    }
}

/// The session's read-only holds: live session and A2A leases from the lease
/// bucket, A2A admission holds through the shared leader-read predicate.
async fn read_holds(
    store: Option<&crate::nats_session_metadata::SessionMetadataStore>,
    lease_store: Option<&async_nats::jetstream::kv::Store>,
    storage_key: &str,
) -> Result<SessionHolds> {
    let session_lease =
        crate::remote_session_cleanup::session_lease_present(lease_store, storage_key).await?;
    let a2a_lease =
        crate::remote_session_cleanup::a2a_lease_present(lease_store, storage_key).await?;
    let a2a = match store {
        Some(store) => crate::nats_admin::a2a::leader_read_a2a_holds(store, storage_key).await?,
        // No metadata bucket: no A2A facts to observe, so no holds.
        None => evaluate_a2a_holds(storage_key, &A2aHoldInputs::absent())?,
    };
    Ok(SessionHolds {
        session_lease,
        a2a_lease,
        a2a_unresolved_context: a2a.unresolved_context,
        a2a_unsettled_first_reservation: a2a.unsettled_first_reservation,
    })
}

/// Live journal rows in `harnx_tool_invocations`; an absent bucket has nothing
/// to count.
async fn journal_rows(
    jetstream: &async_nats::jetstream::Context,
    storage_key: &str,
) -> Result<OwnedCount> {
    match jetstream
        .get_key_value(harnx_toolset_server::invocation_journal::BUCKET)
        .await
    {
        Ok(store) => {
            let journal = harnx_toolset_server::invocation_journal::InvocationJournal::from_store(
                jetstream, store,
            );
            Ok(OwnedCount {
                present: true,
                count: journal.live_rows_for_session(storage_key).await? as u64,
            })
        }
        Err(error) if crate::nats_admin::kv_bucket_missing(&error) => Ok(OwnedCount {
            present: false,
            count: 0,
        }),
        Err(error) => Err(anyhow::Error::from(error)).with_context(|| {
            format!(
                "Failed to open tool invocation journal bucket '{}'",
                harnx_toolset_server::invocation_journal::BUCKET
            )
        }),
    }
}

/// Live interaction checkpoint records; an absent bucket has nothing to count.
async fn checkpoints(
    jetstream: &async_nats::jetstream::Context,
    storage_key: &str,
) -> Result<OwnedCount> {
    let Some(store) =
        crate::interaction_checkpoint::store::CheckpointStore::open_existing(jetstream).await?
    else {
        return Ok(OwnedCount {
            present: false,
            count: 0,
        });
    };
    Ok(OwnedCount {
        present: true,
        count: store.live_session_record_count(storage_key).await? as u64,
    })
}

/// Live plan documents; an absent plans bucket has nothing to count.
async fn plans(
    jetstream: &async_nats::jetstream::Context,
    storage_key: &str,
) -> Result<OwnedPlans> {
    let Some(store) = harnx_blob_store::optional_plans_bucket(jetstream).await? else {
        return Ok(OwnedPlans::default());
    };
    let summary = harnx_blob_store::session_plan_summary(&store, storage_key).await?;
    Ok(OwnedPlans {
        present: true,
        plans: summary.plans as u64,
        documents: summary.documents as u64,
    })
}

/// Live canonical attachment objects with logical bytes; an absent object
/// store has nothing to count.
async fn attachments(
    jetstream: &async_nats::jetstream::Context,
    storage_key: &str,
) -> Result<OwnedAttachments> {
    let Some(bucket) = harnx_blob_store::optional_attachments_bucket(jetstream).await? else {
        return Ok(OwnedAttachments::default());
    };
    let summary = harnx_blob_store::session_attachment_summary(&bucket, storage_key).await?;
    Ok(OwnedAttachments {
        present: true,
        count: summary.count as u64,
        logical_bytes: summary.logical_bytes,
    })
}
