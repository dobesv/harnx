//! Durable completion polling must not block live events or cancellation.

use super::{NatsSessionLog, SessionLeaseWatchdog};
use anyhow::{Context, Result};
use futures_util::{stream, Stream};
use harnx_core::event::{
    AgentEvent, AgentEventSink, SubAgentProgress, SubAgentProgressStatus, TurnEvent,
};
use harnx_core::session::SessionLogEntry;
use std::{collections::HashSet, sync::Arc, time::Duration};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_READ_FAILURES: usize = 3;

#[cfg(test)]
#[path = "completion_tests.rs"]
mod tests;

pub(super) struct DurableUpdate {
    pub entries: Vec<(u64, SessionLogEntry)>,
    pub orphaned: Option<String>,
}

struct CompletionPoller {
    log: NatsSessionLog,
    jetstream: async_nats::jetstream::Context,
    session_id: String,
    entries: Vec<(u64, SessionLogEntry)>,
    watchdog: SessionLeaseWatchdog,
}

/// `unfold` retains its pending refresh future across `next()` calls. Advisory
/// traffic cannot restart a read or postpone its deadline, and dropping the
/// stream cancels the reader without leaving a background task behind.
pub(super) fn updates(
    jetstream: async_nats::jetstream::Context,
    session_id: String,
    entries: Vec<(u64, SessionLogEntry)>,
) -> impl Stream<Item = Result<DurableUpdate>> {
    let poller = CompletionPoller {
        log: NatsSessionLog::new(jetstream.clone(), session_id.clone()),
        jetstream,
        session_id,
        entries,
        watchdog: SessionLeaseWatchdog::new(),
    };
    stream::unfold(poller, |mut poller| async move {
        let result = poller.poll().await;
        Some((result, poller))
    })
}

impl CompletionPoller {
    async fn poll(&mut self) -> Result<DurableUpdate> {
        for attempt in 1..=MAX_READ_FAILURES {
            tokio::time::sleep(POLL_INTERVAL).await;
            let cursor = self.entries.last().map_or(0, |(seq, _)| *seq);
            let result = bounded_refresh(async {
                let entries = self.log.load_events_after_async(cursor).await?;
                let orphaned = self.watchdog.check(&self.jetstream, &self.session_id).await;
                Ok((entries, orphaned))
            })
            .await;
            match result {
                Ok((entries, orphaned)) => {
                    // Advance only after a complete successful read. Mutations
                    // remain in the raw log and are applied by the caller.
                    self.entries.extend(entries);
                    return Ok(DurableUpdate {
                        entries: self.entries.clone(),
                        orphaned,
                    });
                }
                Err(error) => {
                    log::warn!("session completion read failed: session_id={} attempt={attempt}/{MAX_READ_FAILURES} error={error:#}", self.session_id);
                    if attempt == MAX_READ_FAILURES {
                        return Err(error).with_context(|| format!(
                            "Cannot confirm completion of session '{}': NATS session log remained unavailable after {MAX_READ_FAILURES} attempts; execution may still be running. Inspect this session before retrying",
                            self.session_id
                        ));
                    }
                }
            }
        }
        unreachable!("poll either returns a snapshot or exhausts retries")
    }
}

pub(super) async fn bounded_refresh<T>(
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(REFRESH_TIMEOUT, future)
        .await
        .context("timed out refreshing durable session completion")?
}

/// A parent can keep working long after a child returns. Repair missed lossy
/// progress events from its durable tool results without replaying tool output
/// or mistaking a child's completion for the end of the parent's turn.
pub(super) fn reconcile_subagent_progress(
    entries: &[(u64, SessionLogEntry)],
    after_seq: u64,
    sink: &Arc<dyn AgentEventSink>,
    emitted: &mut HashSet<String>,
) {
    for (_, entry) in entries.iter().filter(|(seq, _)| *seq > after_seq) {
        let SessionLogEntry::ToolResults { results, .. } = entry else {
            continue;
        };
        for result in results {
            let Some(value) = result.output.get("sub_agent_progress") else {
                continue;
            };
            let Ok(progress) = serde_json::from_value::<SubAgentProgress>(value.clone()) else {
                continue;
            };
            if progress.status != SubAgentProgressStatus::Running
                && emitted.insert(progress.invocation_id.clone())
            {
                sink.emit(AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)));
            }
        }
    }
}
