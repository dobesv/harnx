//! Bulk session storage statistics for `harnx info sessions` (#2923).
//!
//! Three collection views — cluster, one named agent, one session — all
//! read-only. Aggregate views issue requests that scale with listing pages,
//! never with sessions: one bulk metadata snapshot (A2A facts included), one
//! bulk live-lease snapshot and one paginated stream listing. The session view
//! may use targeted reads. Nothing here creates a bucket or renews activity,
//! and snapshot-based eligibility is an estimate, never a deletion authority;
//! the real fences in [`crate::nats_admin`] stay unchanged.
use crate::config::Config;
use anyhow::Result;
use chrono::Utc;

mod aggregate;
pub mod report;
mod retention;
mod snapshot;

use crate::nats_session_metadata::{SessionActivity, SessionAgentSource};
pub use report::{
    AgeCounts, AgentStatsReport, AgentTotals, ClusterStatsReport, OwnedAttachments, OwnedCount,
    OwnedPlans, RetentionReport, SessionAgeEntry, SessionHolds, SessionSizeEntry,
    SessionStatsDetail, SessionStreamSummary, SessionStreamTotals, SharedBucketReport,
    StatsAgentSource, StreamBreakdown, AGENT_TOP_LIST_LIMIT,
};

/// Cluster-wide storage report: metadata-backed session count, session-stream
/// totals split matched/orphan, effective-activity ages, the seven shared
/// buckets, per-agent totals and snapshot-estimated retention eligibility.
pub async fn collect_cluster_stats(config: &Config, cluster: &str) -> Result<ClusterStatsReport> {
    let jetstream = config.nats_jetstream(cluster).await?;
    let inputs = snapshot::read_cluster_inputs(&jetstream).await?;
    let scope = aggregate::ReportScope::from_inputs(
        &inputs,
        Utc::now(),
        config.cleanup_remote_sessions_days,
    );
    Ok(aggregate::build_cluster_report(
        cluster,
        &scope,
        inputs.shared_buckets.clone(),
    ))
}

/// One named agent's scoped report. Inline sessions never join a named
/// agent's totals; the unfiltered cluster view includes them.
pub async fn collect_agent_stats(
    config: &Config,
    cluster: &str,
    agent: &str,
) -> Result<AgentStatsReport> {
    let jetstream = config.nats_jetstream(cluster).await?;
    let inputs = snapshot::read_cluster_inputs(&jetstream).await?;
    let scope = aggregate::ReportScope::from_inputs(
        &inputs,
        Utc::now(),
        config.cleanup_remote_sessions_days,
    );
    Ok(aggregate::build_agent_report(cluster, agent, &scope))
}

/// One session's storage report, from targeted leader reads. Requires the
/// named session's canonical metadata; an unknown session is a clear error.
pub async fn collect_session_stats(
    config: &Config,
    cluster: &str,
    agent: &str,
    session_id: &str,
) -> Result<SessionStatsDetail> {
    let jetstream = config.nats_jetstream(cluster).await?;
    detail::collect_session_detail(cluster, &jetstream, agent, session_id).await
}

mod detail;

#[cfg(test)]
mod nats_tests;
#[cfg(test)]
mod tests;
