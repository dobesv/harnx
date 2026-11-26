//! Bulk snapshot inputs: turn one metadata snapshot, one live-lease snapshot
//! and one stream listing into the aggregated inputs the pure builders take.
use super::report::{SharedBucketReport, StatsAgentSource};
use crate::nats_admin::a2a::{
    archive_key_when_inspected, context_key, evaluate_a2a_holds, registry_key,
    reservation_key_from_registry, A2aHoldInputs, A2aHoldSummary, RecordFact,
};
use crate::nats_session_metadata::{
    activity_key, SessionActivity, SessionMetadata, SessionMetadataStore, SessionSnapshot,
    SESSION_METADATA_BUCKET,
};
use crate::session_stream_orphans::{is_session_stream, storage_key_from_stream};
use anyhow::{Context as _, Result};
use async_nats::jetstream::kv;
use async_nats::jetstream::stream::Info as StreamInfo;
use futures_util::TryStreamExt;
use std::collections::HashSet;

/// One metadata-backed session, as the snapshot read it.
#[derive(Debug, Clone)]
pub(crate) struct SessionInput {
    pub storage_key: String,
    pub session_id: String,
    pub agent: StatsAgentSource,
    /// `activity.last_activity_at`, or `created_at` when the activity record
    /// is absent (malformed activity falls back the same way, matching
    /// cleanup's tolerant read).
    pub effective_activity: chrono::DateTime<chrono::Utc>,
    pub a2a_holds: A2aHoldSummary,
}

/// One `SESSION_` stream from the listing, already classified.
#[derive(Debug, Clone)]
pub(crate) struct StreamInput {
    /// Storage key recovered from the stream subject and verified against the
    /// stream name; `None` leaves the stream unmapped.
    pub storage_key: Option<String>,
    pub bytes: u64,
    pub messages: u64,
}

/// Everything a cluster- or agent-view report needs, collected with requests
/// that scale with listing pages, never with sessions.
#[derive(Debug, Default)]
pub(crate) struct ClusterInputs {
    pub sessions: Vec<SessionInput>,
    pub streams: Vec<StreamInput>,
    pub live_lease_keys: HashSet<String>,
    pub shared_buckets: Vec<SharedBucketReport>,
}

/// The shared buckets the storage-visibility issue names, by bucket and
/// backing stream. `KV_harnx_execution_control` is retired (#1949): its
/// absence is a normal `present=false`, not an error.
pub(crate) const SHARED_BUCKET_STREAMS: [(&str, &str); 7] = [
    ("harnx_sessions", "KV_harnx_sessions"),
    ("harnx_tool_invocations", "KV_harnx_tool_invocations"),
    ("harnx_execution_control", "KV_harnx_execution_control"),
    (
        "harnx_interaction_checkpoints",
        "KV_harnx_interaction_checkpoints",
    ),
    ("harnx_plans", "KV_harnx_plans"),
    ("harnx_attachments", "OBJ_harnx_attachments"),
    ("harnx_leases", "KV_harnx_leases"),
];

pub(crate) async fn read_cluster_inputs(
    jetstream: &async_nats::jetstream::Context,
) -> Result<ClusterInputs> {
    let metadata = open_metadata_store(jetstream).await?;
    let snapshot = match &metadata {
        Some(store) => store.snapshot().await?,
        None => SessionSnapshot::default(),
    };
    let lease_keys = crate::nats_lease::snapshot_live_lease_keys(jetstream)
        .await?
        .unwrap_or_default();
    let mut inputs = ClusterInputs {
        sessions: snapshot_sessions(&snapshot)?,
        live_lease_keys: lease_keys,
        shared_buckets: Vec::new(),
        streams: Vec::new(),
    };
    read_streams(jetstream, &mut inputs).await?;
    Ok(inputs)
}

/// Open `harnx_sessions` without creating it. An absent metadata bucket means
/// no session has canonical metadata on this cluster; other failures are
/// auth/transport errors and propagate.
pub(crate) async fn open_metadata_store(
    jetstream: &async_nats::jetstream::Context,
) -> anyhow::Result<Option<SessionMetadataStore>> {
    match jetstream.get_key_value(SESSION_METADATA_BUCKET).await {
        Ok(store) => Ok(Some(SessionMetadataStore::from_store(
            store,
            jetstream.client().clone(),
        ))),
        Err(error) if crate::nats_admin::kv_bucket_missing(&error) => Ok(None),
        Err(error) => Err(anyhow::Error::from(error)).with_context(|| {
            format!("Failed to open session metadata bucket '{SESSION_METADATA_BUCKET}'")
        }),
    }
}

/// Build session inputs from the bulk snapshot: metadata and activity joined
/// per session, plus each session's read-only A2A hold decision from the same
/// snapshot's context/registry/reservation/archive facts.
pub(crate) fn snapshot_sessions(snapshot: &SessionSnapshot) -> Result<Vec<SessionInput>> {
    let mut sessions = Vec::new();
    for (key, storage_key) in metadata_keys(snapshot) {
        let Some(entry) = snapshot.get(&key) else {
            continue;
        };
        let metadata: SessionMetadata = serde_json::from_slice(&entry.payload)
            .with_context(|| format!("Failed to deserialize session metadata '{key}'"))?;
        metadata.validate_storage_key(&storage_key)?;
        let a2a_holds = snapshot_a2a_holds(snapshot, &storage_key)?;
        sessions.push(SessionInput {
            storage_key,
            session_id: metadata.session_id.clone(),
            agent: StatsAgentSource::from_metadata(&metadata.agent),
            effective_activity: effective_activity(snapshot, &metadata),
            a2a_holds,
        });
    }
    Ok(sessions)
}

/// Live metadata keys and the storage keys they belong to. Delete and purge
/// markers on `sessions/<storage_key>/meta` mean the session's canonical
/// record is gone, so they are skipped.
pub(crate) fn metadata_keys(snapshot: &SessionSnapshot) -> Vec<(String, String)> {
    snapshot
        .iter()
        .filter_map(|(key, entry)| {
            if entry.operation != kv::Operation::Put {
                return None;
            }
            let storage_key = key
                .strip_prefix("sessions/")?
                .strip_suffix("/meta")?
                .to_string();
            Some((key.to_string(), storage_key))
        })
        .collect()
}

/// Effective activity: the activity record's `last_activity_at`, or the
/// metadata's `created_at` when the record is absent or malformed — the same
/// tolerant fallback cleanup uses.
fn effective_activity(
    snapshot: &SessionSnapshot,
    metadata: &SessionMetadata,
) -> chrono::DateTime<chrono::Utc> {
    let key = activity_key(&metadata.storage_key());
    match snapshot.get(&key) {
        Some(entry) if entry.operation == kv::Operation::Put => {
            match serde_json::from_slice::<SessionActivity>(&entry.payload) {
                Ok(activity) => activity.last_activity_at,
                Err(error) => {
                    log::warn!(
                        "could not read session activity; using metadata timestamp: bucket={SESSION_METADATA_BUCKET} key={key} error={error:#}"
                    );
                    metadata.created_at
                }
            }
        }
        _ => metadata.created_at,
    }
}

/// A session's read-only A2A hold decision from snapshot facts. The snapshot
/// retains marker states for the context; registry, reservation and archive
/// markers count as absent, matching the fence's leader reads.
pub(crate) fn snapshot_a2a_holds(
    snapshot: &SessionSnapshot,
    storage: &str,
) -> Result<A2aHoldSummary> {
    let context = snapshot_fact(snapshot, &context_key(storage));
    let registry = snapshot_fact(snapshot, &registry_key(storage));
    let reservation_key = reservation_key_from_registry(&registry)?;
    let reservation = reservation_key
        .map(|key| snapshot_fact(snapshot, &key))
        .unwrap_or(RecordFact::Absent);
    let archive = archive_key_when_inspected(storage, &context, &registry, &reservation)?
        .map(|key| snapshot_fact(snapshot, &key))
        .unwrap_or(RecordFact::Absent);
    evaluate_a2a_holds(
        storage,
        &A2aHoldInputs {
            context,
            registry,
            reservation,
            archive,
        },
    )
}

/// A snapshot entry keeps the marker state the way a leader entry does, so the
/// same fact conversion applies.
fn snapshot_fact(snapshot: &SessionSnapshot, key: &str) -> RecordFact {
    match snapshot.get(key) {
        None => RecordFact::Absent,
        Some(entry) if entry.operation == kv::Operation::Put => {
            RecordFact::Live(entry.payload.clone())
        }
        Some(entry) => RecordFact::Marker(entry.operation),
    }
}

/// One paginated stream listing: classify every `SESSION_` stream and collect
/// the shared bucket streams' sizes from the same infos.
async fn read_streams(
    jetstream: &async_nats::jetstream::Context,
    inputs: &mut ClusterInputs,
) -> Result<()> {
    let mut shared: Vec<(&'static str, &'static str, bool, u64, u64)> = SHARED_BUCKET_STREAMS
        .iter()
        .map(|(bucket, stream)| (*bucket, *stream, false, 0, 0))
        .collect();
    let mut streams = jetstream.streams();
    while let Some(info) = streams.try_next().await? {
        let info = StreamInfoOwned::own(info);
        if let Some(index) = shared
            .iter()
            .position(|(_, stream, _, _, _)| *stream == info.name)
        {
            let entry = &mut shared[index];
            entry.2 = true;
            entry.3 = info.bytes;
            entry.4 = info.messages;
        }
        if is_session_stream(&info.config) {
            inputs.streams.push(classify_session_stream(info));
        }
    }
    inputs.shared_buckets = shared
        .into_iter()
        .map(
            |(bucket, stream, present, bytes, messages)| SharedBucketReport {
                bucket,
                stream,
                present,
                bytes,
                messages,
            },
        )
        .collect();
    Ok(())
}

/// The parts of a stream listing info the reports use.
#[derive(Debug, Clone)]
struct StreamInfoOwned {
    name: String,
    bytes: u64,
    messages: u64,
    config: async_nats::jetstream::stream::Config,
}

impl StreamInfoOwned {
    /// The parts of a stream info the reports use.
    ///
    /// Named `own`, not `from`: an inherent method named `from` makes
    /// rustqual treat every `Secret::from`-style call in the workspace as an
    /// own call, flagging functions this crate never touched.
    fn own(info: StreamInfo) -> Self {
        Self {
            name: info.config.name.clone(),
            bytes: info.state.bytes,
            messages: info.state.messages,
            config: info.config,
        }
    }
}

fn classify_session_stream(info: StreamInfoOwned) -> StreamInput {
    let storage_key = storage_key_from_stream(&info.config).ok();
    StreamInput {
        storage_key,
        bytes: info.bytes,
        messages: info.messages,
    }
}
