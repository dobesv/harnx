//! Pure aggregation over collected inputs. No I/O here, so the synthetic
//! 10,000-record test exercises the exact production joins, buckets and
//! sortings without a broker.
use super::report::{
    largest_sessions, oldest_sessions, ActivitySummary, AgeCounts, AgentStatsReport, AgentTotals,
    ClusterStatsReport, RetentionReport, SessionStreamTotals, SharedBucketReport, StatsAgentSource,
    StreamBreakdown,
};
use super::retention::{age_bucket, retention_report};
use super::snapshot::{SessionInput, StreamInput};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Scope inputs the cluster and agent reports share: the captured clock,
/// the bulk session/stream snapshots and the resolved retention days. The
/// collection entry points build this once per report.
pub(crate) struct ReportScope<'a> {
    pub(crate) now: DateTime<Utc>,
    pub(crate) sessions: &'a [SessionInput],
    pub(crate) streams: &'a [StreamInput],
    pub(crate) live_lease_keys: &'a HashSet<String>,
    pub(crate) retention_days: Option<u64>,
}

impl<'a> ReportScope<'a> {
    /// From collected cluster inputs, with the clock captured at the call
    /// and the configured retention days resolved.
    pub(crate) fn from_inputs(
        inputs: &'a super::snapshot::ClusterInputs,
        now: DateTime<Utc>,
        retention_days: Option<u64>,
    ) -> Self {
        Self {
            now,
            sessions: &inputs.sessions,
            streams: &inputs.streams,
            live_lease_keys: &inputs.live_lease_keys,
            retention_days,
        }
    }

    /// From raw inputs; the pure tests' assembly point.
    #[cfg(test)]
    pub(crate) fn new(
        now: DateTime<Utc>,
        sessions: &'a [SessionInput],
        streams: &'a [StreamInput],
        live_lease_keys: &'a HashSet<String>,
        retention_days: Option<u64>,
    ) -> Self {
        Self {
            now,
            sessions,
            streams,
            live_lease_keys,
            retention_days,
        }
    }
}

/// One session joined with the stream the listing matched to it.
pub(super) struct ScopedSession<'a> {
    pub(super) session: &'a SessionInput,
    pub(super) bytes: u64,
    pub(super) messages: u64,
    pub(super) stream_present: bool,
}

/// Streams whose subject mapped to a session key, summed per key.
fn matched_stream_bytes(streams: &[StreamInput]) -> HashMap<&str, (u64, u64)> {
    let mut matched = HashMap::new();
    for stream in streams {
        let Some(key) = stream.storage_key.as_deref() else {
            continue;
        };
        let entry = matched.entry(key).or_insert((0, 0));
        entry.0 += stream.bytes;
        entry.1 += stream.messages;
    }
    matched
}

/// Build the cluster report. Every stream is classified exactly once into
/// matched, orphan-mapped or orphan-unmapped, and `total` is their sum.
pub(crate) fn build_cluster_report(
    cluster: &str,
    scope: &ReportScope<'_>,
    shared_buckets: Vec<SharedBucketReport>,
) -> ClusterStatsReport {
    let session_keys: HashSet<&str> = scope
        .sessions
        .iter()
        .map(|session| session.storage_key.as_str())
        .collect();
    let matched = matched_stream_bytes(scope.streams);
    let mut totals = SessionStreamTotals::default();
    for stream in scope.streams {
        totals.total.add_stream(stream.bytes, stream.messages);
        match stream.storage_key.as_deref() {
            Some(key) if session_keys.contains(key) => {
                totals.matched.add_stream(stream.bytes, stream.messages)
            }
            Some(_) => totals
                .orphan_mapped
                .add_stream(stream.bytes, stream.messages),
            None => totals
                .orphan_unmapped
                .add_stream(stream.bytes, stream.messages),
        }
    }
    let scoped = scoped_sessions(scope.sessions, &matched);
    totals.metadata_only_sessions = scoped
        .iter()
        .filter(|session| !session.stream_present)
        .count() as u64;
    let mut agents: BTreeMap<StatsAgentSource, AgentTotals> = BTreeMap::new();
    let mut activity = ActivitySummary::default();
    for session in &scoped {
        record_activity(&mut activity, scope.now, session.session);
        record_agent(&mut agents, session, scope.now);
    }
    ClusterStatsReport {
        view: "cluster",
        cluster: cluster.to_string(),
        collected_at: scope.now,
        session_count: scope.sessions.len() as u64,
        streams: totals,
        activity,
        shared_buckets,
        agents: sorted_agent_totals(agents),
        retention: retention_report(
            scope.now,
            scope.sessions,
            scope.live_lease_keys,
            scope.retention_days,
        ),
    }
}

/// Build one named agent's scoped report. Only that agent's named sessions are
/// in scope; inline sessions and other agents never join its totals, and
/// orphan streams are attributed to nobody.
pub(crate) fn build_agent_report(
    cluster: &str,
    agent: &str,
    scope: &ReportScope<'_>,
) -> AgentStatsReport {
    let scoped_keys: HashSet<&str> = scope
        .sessions
        .iter()
        .filter(|session| named_like(session, agent))
        .map(|session| session.storage_key.as_str())
        .collect();
    let matched: HashMap<&str, (u64, u64)> = matched_stream_bytes(scope.streams)
        .into_iter()
        .filter(|(key, _)| scoped_keys.contains(key))
        .collect();
    let scoped: Vec<ScopedSession> = scope
        .sessions
        .iter()
        .filter(|session| named_like(session, agent))
        .map(|session| scoped_session(session, &matched))
        .collect();
    let mut report = AgentStatsReport {
        view: "agent",
        cluster: cluster.to_string(),
        collected_at: scope.now,
        agent: StatsAgentSource::Named {
            name: agent.to_string(),
        },
        session_count: scoped.len() as u64,
        streams: StreamBreakdown::default(),
        metadata_only_sessions: 0,
        activity: ActivitySummary::default(),
        retention: RetentionReport::default(),
        largest_sessions: largest_sessions(&scoped),
        oldest_sessions: oldest_sessions(&scoped),
    };
    for session in &scoped {
        // Only sessions a stream matched count as streams: a metadata-only
        // session owns no SESSION_* stream, so its totals stay zero.
        if session.stream_present {
            report.streams.add_stream(session.bytes, session.messages);
        } else {
            report.metadata_only_sessions += 1;
        }
        record_activity(&mut report.activity, scope.now, session.session);
    }
    report.retention = retention_report(
        scope.now,
        scope
            .sessions
            .iter()
            .filter(|session| named_like(session, agent)),
        scope.live_lease_keys,
        scope.retention_days,
    );
    report
}

fn named_like(session: &SessionInput, agent: &str) -> bool {
    matches!(&session.agent, StatsAgentSource::Named { name } if name == agent)
}

fn scoped_sessions<'a>(
    sessions: &'a [SessionInput],
    matched: &HashMap<&'a str, (u64, u64)>,
) -> Vec<ScopedSession<'a>> {
    sessions
        .iter()
        .map(|session| scoped_session(session, matched))
        .collect()
}

fn scoped_session<'a>(
    session: &'a SessionInput,
    matched: &HashMap<&'a str, (u64, u64)>,
) -> ScopedSession<'a> {
    let (bytes, messages) = matched
        .get(session.storage_key.as_str())
        .copied()
        .unwrap_or((0, 0));
    ScopedSession {
        session,
        bytes,
        messages,
        stream_present: matched.contains_key(session.storage_key.as_str()),
    }
}

fn record_activity(activity: &mut ActivitySummary, now: DateTime<Utc>, session: &SessionInput) {
    activity
        .age_counts
        .record(age_bucket(now, session.effective_activity));
    if activity
        .oldest_effective_activity
        .is_none_or(|oldest| session.effective_activity < oldest)
    {
        activity.oldest_effective_activity = Some(session.effective_activity);
    }
    if activity
        .newest_effective_activity
        .is_none_or(|newest| session.effective_activity > newest)
    {
        activity.newest_effective_activity = Some(session.effective_activity);
    }
}

fn record_agent(
    agents: &mut BTreeMap<StatsAgentSource, AgentTotals>,
    session: &ScopedSession,
    now: DateTime<Utc>,
) {
    let entry = agents
        .entry(session.session.agent.clone())
        .or_insert_with(|| AgentTotals {
            agent: session.session.agent.clone(),
            session_count: 0,
            streams: StreamBreakdown::default(),
            age_counts: AgeCounts::default(),
        });
    entry.session_count += 1;
    // Same rule as the agent report: a metadata-only session is not a stream.
    if session.stream_present {
        entry.streams.add_stream(session.bytes, session.messages);
    }
    entry
        .age_counts
        .record(age_bucket(now, session.session.effective_activity));
}

fn sorted_agent_totals(agents: BTreeMap<StatsAgentSource, AgentTotals>) -> Vec<AgentTotals> {
    let mut agents: Vec<AgentTotals> = agents.into_values().collect();
    // Descending stream bytes, agent identity as tie-breaker.
    agents.sort_by(|left, right| {
        right
            .streams
            .bytes
            .cmp(&left.streams.bytes)
            .then_with(|| left.agent.cmp(&right.agent))
    });
    agents
}
