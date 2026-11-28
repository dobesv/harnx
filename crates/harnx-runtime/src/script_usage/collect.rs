//! Reads stored session logs from a cluster, read-only, and feeds them to the
//! report builder. Nothing here creates a bucket, stream or consumer, renews
//! activity or appends an entry.

use std::future::Future;
use std::pin::Pin;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use futures_util::{stream, FutureExt, StreamExt};
use globset::{GlobBuilder, GlobMatcher};

use super::report::{
    session_detail, ReportBuilder, ScriptUsageReport, ScriptUsageScope, SessionScriptUsageDetail,
    INLINE_AGENT_LABEL,
};
use super::session::{analyze_session, SessionScriptUsage, TimeWindow};
use crate::config::Config;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::ListedSession;
use crate::session_stats::open_metadata_store;

/// Each log read is one broker request per entry, so reads run a few at a time.
const CONCURRENT_READS: usize = 8;

/// What one session's read produced.
type SessionRead = (String, String, Result<SessionScriptUsage>);

/// Collects the report. `on_selected` is called once, with the number of
/// session logs about to be read, before the first read.
pub async fn collect_script_usage(
    config: &Config,
    cluster: &str,
    scope: &ScriptUsageScope,
    on_selected: &dyn Fn(u64),
) -> Result<ScriptUsageReport> {
    let jetstream = config.nats_jetstream(cluster).await?;
    collect_from(&jetstream, cluster, scope, on_selected).await
}

pub async fn collect_session_script_usage(
    config: &Config,
    cluster: &str,
    agent: &str,
    session_id: &str,
) -> Result<SessionScriptUsageDetail> {
    let jetstream = config.nats_jetstream(cluster).await?;
    collect_session_from(&jetstream, cluster, agent, session_id).await
}

pub(super) async fn collect_from(
    jetstream: &async_nats::jetstream::Context,
    cluster: &str,
    scope: &ScriptUsageScope,
    on_selected: &dyn Fn(u64),
) -> Result<ScriptUsageReport> {
    let mut builder = ReportBuilder::new(cluster, scope);
    let matcher = scope
        .agent_pattern
        .as_deref()
        .map(agent_matcher)
        .transpose()?;
    let Some(store) = open_metadata_store(jetstream).await? else {
        on_selected(0);
        return Ok(builder.finish());
    };
    let selected: Vec<ListedSession> = store
        .list()
        .await?
        .into_iter()
        .filter(|session| agent_matches(matcher.as_ref(), session))
        .filter(|session| in_window(session, &scope.window))
        .collect();
    on_selected(selected.len() as u64);
    let window = scope.window;
    let mut reads = stream::iter(selected)
        .map(|session| read_session(jetstream.clone(), session, window))
        .buffer_unordered(CONCURRENT_READS);
    while let Some((agent, session_id, result)) = reads.next().await {
        match result {
            Ok(usage) => builder.add_session(&agent, &session_id, usage),
            Err(error) => {
                log::warn!("could not read session log for script usage: agent={agent} session={session_id} error={error:#}");
                builder.add_unreadable();
            }
        }
    }
    drop(reads);
    Ok(builder.finish())
}

pub(super) async fn collect_session_from(
    jetstream: &async_nats::jetstream::Context,
    cluster: &str,
    agent: &str,
    session_id: &str,
) -> Result<SessionScriptUsageDetail> {
    let not_found = || {
        anyhow::anyhow!(
            "session '{session_id}' of agent '{agent}' not found on cluster '{cluster}'"
        )
    };
    let store = open_metadata_store(jetstream)
        .await?
        .ok_or_else(not_found)?;
    store
        .get_for_agent(session_id, agent)
        .await
        .with_context(|| format!("Failed to read session metadata on cluster '{cluster}'"))?
        .ok_or_else(not_found)?;
    let storage_key = harnx_core::session_identity::session_key(Some(agent), session_id);
    let entries = NatsSessionLog::new(jetstream.clone(), storage_key)
        .load_events_async()
        .await
        .with_context(|| format!("Failed to read the log of session '{session_id}'"))?;
    let usage = analyze_session(&entries, &TimeWindow::default());
    Ok(session_detail(cluster, agent, session_id, usage))
}

/// The matcher `--agent` patterns compile to. `*` also crosses `/`.
pub fn agent_matcher(pattern: &str) -> Result<GlobMatcher> {
    let glob = GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .with_context(|| format!("Invalid agent pattern '{pattern}'"))?;
    Ok(glob.compile_matcher())
}

/// A pattern never matches an inline session, which has no agent name.
fn agent_matches(matcher: Option<&GlobMatcher>, session: &ListedSession) -> bool {
    match matcher {
        None => true,
        Some(matcher) => session
            .metadata
            .agent
            .name()
            .is_some_and(|name| matcher.is_match(name)),
    }
}

/// A session idle since before the window opened, or created at or after it
/// closed, cannot hold an entry inside it, so its log is not read.
fn in_window(session: &ListedSession, window: &TimeWindow) -> bool {
    active_since(session, window.since)
        && window
            .until
            .is_none_or(|until| session.metadata.created_at < until)
}

fn active_since(session: &ListedSession, since: Option<DateTime<Utc>>) -> bool {
    let activity = session
        .activity
        .as_ref()
        .map_or(session.metadata.created_at, |activity| {
            activity.last_activity_at
        });
    since.is_none_or(|since| activity >= since)
}

/// Type-erased so the concurrent stream does not carry the broker client's
/// whole future type into dependent crates.
fn read_session(
    jetstream: async_nats::jetstream::Context,
    session: ListedSession,
    window: TimeWindow,
) -> Pin<Box<dyn Future<Output = SessionRead> + Send>> {
    async move {
        let agent = session
            .metadata
            .agent
            .name()
            .unwrap_or(INLINE_AGENT_LABEL)
            .to_string();
        let session_id = session.metadata.session_id.clone();
        let log = NatsSessionLog::new(jetstream, session.metadata.storage_key());
        let result = log
            .load_events_async()
            .await
            .map(|entries| analyze_session(&entries, &window));
        (agent, session_id, result)
    }
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::agent_matcher;

    #[test]
    fn agent_patterns_cross_slashes_and_bad_ones_fail() {
        let matches = |pattern: &str, name: &str| agent_matcher(pattern).unwrap().is_match(name);
        assert!(matches("*", "pantheon/atlas"));
        assert!(matches("pantheon*", "pantheon/atlas"));
        assert!(matches("pantheon/*", "pantheon/atlas"));
        assert!(!matches("pantheon/*", "coding/coder"));
        assert!(agent_matcher("a[").is_err());
    }
}
