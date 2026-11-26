//! Serializable report types for `harnx info sessions` (T3 renders these).
//!
//! All views serialize to JSON with the same field-name conventions: counts
//! are unsigned integers, missing timestamps and sequences are `null` rather
//! than fabricated zero dates, and agent identity is structured —
//! `{"kind":"named","name":…}` or `{"kind":"inline"}` — never a display
//! string like `<inline>` that could collide with a named agent.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Sessions whose effective activity falls in each age window, measured
/// against the one `now` the report captured at collection time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgeCounts {
    pub under_1_day: u64,
    pub one_to_7_days: u64,
    pub seven_to_30_days: u64,
    pub over_30_days: u64,
}

/// Effective activity — `activity.last_activity_at`, or metadata `created_at`
/// when no activity record exists, the same fallback cleanup uses. Age counts
/// cover metadata-backed sessions; oldest/newest are `null` for an empty scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivitySummary {
    pub age_counts: AgeCounts,
    pub oldest_effective_activity: Option<DateTime<Utc>>,
    pub newest_effective_activity: Option<DateTime<Utc>>,
}

/// Bytes and messages of one group of `SESSION_` streams.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamBreakdown {
    pub count: u64,
    pub bytes: u64,
    pub messages: u64,
}

impl StreamBreakdown {
    pub(crate) fn add_stream(&mut self, bytes: u64, messages: u64) {
        self.count += 1;
        self.bytes += bytes;
        self.messages += messages;
    }
}

/// How every `SESSION_*` stream on the cluster was classified against
/// metadata: `total` is the sum of the three breakdowns, so the parts
/// reconcile with the whole. Metadata-only sessions (canonical metadata, no
/// stream) are counted separately: they hold no stream bytes or messages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStreamTotals {
    pub total: StreamBreakdown,
    pub matched: StreamBreakdown,
    pub orphan_mapped: StreamBreakdown,
    pub orphan_unmapped: StreamBreakdown,
    pub metadata_only_sessions: u64,
}

/// One shared bucket named by the storage-visibility issue. Sizes are the
/// bucket's physical backing-stream bytes and messages, not logical
/// owned-resource bytes; `present=false` with zero sizes means the bucket's
/// stream does not exist on this cluster. Absence of the retired legacy
/// `KV_harnx_execution_control` bucket is normal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedBucketReport {
    pub bucket: &'static str,
    pub stream: &'static str,
    pub present: bool,
    pub bytes: u64,
    pub messages: u64,
}

/// Snapshot-estimated retention eligibility for metadata-backed sessions in
/// scope. `age_expired = held + eligible`, and each candidate counts once even
/// when several reasons hold; the reason counters sum to more than `held` only
/// when one candidate has several reasons. This is an estimate, not a
/// transactional prediction: new leases, activity, revisions and cascade
/// failures can all change what a real deletion pass does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionReport {
    /// Configured `cleanup_remote_sessions_days`, when set.
    pub configured_days: Option<u64>,
    /// Whether retention is on: a configured nonzero day count.
    pub enabled: bool,
    /// Metadata-backed sessions whose effective activity is strictly older
    /// than the retention cutoff.
    pub age_expired: u64,
    /// Age-expired sessions observed to hold a session lease, an A2A lease,
    /// unresolved A2A context or an unsettled first-message reservation.
    pub held: u64,
    /// Age-expired sessions no observed hold protects.
    pub eligible: u64,
    pub held_session_lease: u64,
    pub held_a2a_lease: u64,
    pub held_unresolved_context: u64,
    pub held_unsettled_first_reservation: u64,
}

/// A named agent or the shared inline group. Structured identity, not a
/// display string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StatsAgentSource {
    Named { name: String },
    Inline,
}

impl StatsAgentSource {
    pub(crate) fn from_metadata(agent: &super::SessionAgentSource) -> Self {
        match agent {
            super::SessionAgentSource::Named { name } => Self::Named { name: name.clone() },
            super::SessionAgentSource::Inline { .. } => Self::Inline,
        }
    }
}

/// Per-agent totals for the cluster view, sorted by descending stream bytes
/// with agent identity as tie-breaker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTotals {
    pub agent: StatsAgentSource,
    pub session_count: u64,
    pub streams: StreamBreakdown,
    pub age_counts: AgeCounts,
}

/// A session's stream footprint, for largest-session lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSizeEntry {
    pub session_id: String,
    pub storage_key: String,
    pub bytes: u64,
    pub messages: u64,
}

/// A session's effective activity, for oldest-session lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAgeEntry {
    pub session_id: String,
    pub storage_key: String,
    pub effective_activity: DateTime<Utc>,
}

/// Cluster-wide storage report. No per-session RPC fan-out: one bulk metadata
/// snapshot, one bulk live-lease snapshot and one paginated stream listing
/// feed every number here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterStatsReport {
    pub view: &'static str,
    pub cluster: String,
    pub collected_at: DateTime<Utc>,
    /// Canonical metadata-backed sessions, inline and metadata-only included.
    pub session_count: u64,
    pub streams: SessionStreamTotals,
    pub activity: ActivitySummary,
    pub shared_buckets: Vec<SharedBucketReport>,
    pub agents: Vec<AgentTotals>,
    pub retention: RetentionReport,
}

/// One named agent's scoped report. Only that agent's named sessions appear;
/// inline sessions never join a named agent's totals. Shared-bucket bytes are
/// cluster-wide and cannot be attributed per agent, so they are not here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStatsReport {
    pub view: &'static str,
    pub cluster: String,
    pub collected_at: DateTime<Utc>,
    pub agent: StatsAgentSource,
    pub session_count: u64,
    pub streams: StreamBreakdown,
    pub metadata_only_sessions: u64,
    pub activity: ActivitySummary,
    pub retention: RetentionReport,
    /// Up to [`AGENT_TOP_LIST_LIMIT`] sessions by descending stream bytes.
    pub largest_sessions: Vec<SessionSizeEntry>,
    /// Up to [`AGENT_TOP_LIST_LIMIT`] sessions by ascending effective activity.
    pub oldest_sessions: Vec<SessionAgeEntry>,
}

/// How many entries the agent view's largest/oldest lists hold.
pub const AGENT_TOP_LIST_LIMIT: usize = 10;

/// Live journal-row count in `harnx_tool_invocations`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedCount {
    /// Whether the owning bucket exists on this cluster.
    pub present: bool,
    pub count: u64,
}

/// Live plan documents in `harnx_plans`: distinct plans and total documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OwnedPlans {
    pub present: bool,
    pub plans: u64,
    pub documents: u64,
}

/// Live canonical attachment objects in `harnx_attachments`, with logical
/// bytes from object metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OwnedAttachments {
    pub present: bool,
    pub count: u64,
    pub logical_bytes: u64,
}

/// A single session's storage report, from targeted leader reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatsDetail {
    pub view: &'static str,
    pub cluster: String,
    pub collected_at: DateTime<Utc>,
    pub agent: StatsAgentSource,
    pub session_id: String,
    pub storage_key: String,
    pub metadata_revision: u64,
    /// When canonical metadata was created; distinct from the stream's
    /// creation timestamp below.
    pub metadata_created_at: DateTime<Utc>,
    pub activity: Option<super::SessionActivity>,
    pub stream_present: bool,
    pub stream: Option<SessionStreamSummary>,
    pub holds: SessionHolds,
    pub journal_rows: OwnedCount,
    pub checkpoints: OwnedCount,
    pub plans: OwnedPlans,
    pub attachments: OwnedAttachments,
}

/// A session stream's state. Sequence numbers and the last message time are
/// `null` when the stream has no messages — an empty stream's message
/// timestamps are not activity timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStreamSummary {
    pub name: String,
    pub bytes: u64,
    pub messages: u64,
    pub first_sequence: Option<u64>,
    pub last_sequence: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub last_message_time: Option<DateTime<Utc>>,
}

/// Holds split by kind, from read-only snapshots and leader reads. A real
/// fence remains authoritative over these flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHolds {
    pub session_lease: bool,
    pub a2a_lease: bool,
    pub a2a_unresolved_context: bool,
    pub a2a_unsettled_first_reservation: bool,
}

/// The agent view's largest sessions, by descending stream bytes with the
/// storage key as tie-breaker, truncated to [`AGENT_TOP_LIST_LIMIT`].
pub(super) fn largest_sessions(
    scoped: &[super::aggregate::ScopedSession],
) -> Vec<SessionSizeEntry> {
    let mut entries: Vec<SessionSizeEntry> = scoped
        .iter()
        .map(|session| SessionSizeEntry {
            session_id: session.session.session_id.clone(),
            storage_key: session.session.storage_key.clone(),
            bytes: session.bytes,
            messages: session.messages,
        })
        .collect();
    entries.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.storage_key.cmp(&right.storage_key))
    });
    entries.truncate(AGENT_TOP_LIST_LIMIT);
    entries
}

/// The agent view's oldest sessions, by ascending effective activity with
/// the storage key as tie-breaker, truncated to [`AGENT_TOP_LIST_LIMIT`].
pub(super) fn oldest_sessions(scoped: &[super::aggregate::ScopedSession]) -> Vec<SessionAgeEntry> {
    let mut entries: Vec<SessionAgeEntry> = scoped
        .iter()
        .map(|session| SessionAgeEntry {
            session_id: session.session.session_id.clone(),
            storage_key: session.session.storage_key.clone(),
            effective_activity: session.session.effective_activity,
        })
        .collect();
    entries.sort_by(|left, right| {
        left.effective_activity
            .cmp(&right.effective_activity)
            .then_with(|| left.storage_key.cmp(&right.storage_key))
    });
    entries.truncate(AGENT_TOP_LIST_LIMIT);
    entries
}
