//! Per-invocation aggregation for NATS-backed sub-agent tools.

use crate::nats_event_sink::NatsEventSink;
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{
    AgentEvent, AgentEventSink, AgentSource, ModelEvent, SubAgentProgress, SubAgentProgressStatus,
    ToolEvent, TurnEvent,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
enum ProgressMetric {
    Usage(CompletionTokenUsage),
    ToolStarted,
}

impl ProgressMetric {
    /// Nested events belong to their own invocation and are deliberately not
    /// folded into the current child.
    fn from_event(event: AgentEvent) -> Option<Self> {
        match event {
            AgentEvent::Model(ModelEvent::Usage {
                input,
                output,
                cached,
                cache_write,
                ..
            }) => Some(Self::Usage(CompletionTokenUsage {
                input_tokens: input,
                output_tokens: output,
                cached_tokens: cached,
                cache_write_tokens: cache_write,
            })),
            AgentEvent::Tool(ToolEvent::Started { .. }) => Some(Self::ToolStarted),
            AgentEvent::SubAgent { .. } => None,
            _ => None,
        }
    }
}

#[derive(Debug)]
enum ProgressCommand {
    Metric(ProgressMetric),
    Finish {
        status: SubAgentProgressStatus,
        reply: oneshot::Sender<(SubAgentProgress, anyhow::Result<()>)>,
    },
}

struct ProgressEventSink {
    tx: mpsc::UnboundedSender<ProgressCommand>,
}

impl AgentEventSink for ProgressEventSink {
    fn emit(&self, event: AgentEvent) {
        if let Some(metric) = ProgressMetric::from_event(event) {
            let _ = self.tx.send(ProgressCommand::Metric(metric));
        }
    }
}

#[derive(Debug)]
struct ProgressTracker {
    snapshot: SubAgentProgress,
}

impl ProgressTracker {
    fn new(agent: String, session_id: String, invocation_id: String) -> Self {
        Self {
            snapshot: SubAgentProgress {
                invocation_id,
                agent,
                session_id,
                status: SubAgentProgressStatus::Running,
                elapsed_ms: 0,
                usage: CompletionTokenUsage::default(),
                tool_call_count: 0,
            },
        }
    }

    fn apply(&mut self, metric: ProgressMetric, elapsed_ms: u64) -> SubAgentProgress {
        match metric {
            ProgressMetric::Usage(usage) => self.snapshot.usage.accumulate(&usage),
            ProgressMetric::ToolStarted => {
                self.snapshot.tool_call_count = self.snapshot.tool_call_count.saturating_add(1);
            }
        }
        self.snapshot.elapsed_ms = elapsed_ms;
        self.snapshot.clone()
    }

    fn heartbeat(&mut self, elapsed_ms: u64) -> SubAgentProgress {
        self.snapshot.elapsed_ms = elapsed_ms;
        self.snapshot.clone()
    }

    fn finish(&mut self, status: SubAgentProgressStatus, elapsed_ms: u64) -> SubAgentProgress {
        self.snapshot.status = status;
        self.snapshot.elapsed_ms = elapsed_ms;
        self.snapshot.clone()
    }
}

pub(super) struct SubagentProgressReporter {
    sink: Arc<dyn AgentEventSink>,
    tx: mpsc::UnboundedSender<ProgressCommand>,
}

impl SubagentProgressReporter {
    pub(super) fn spawn(
        agent: String,
        session_id: String,
        invocation_id: String,
        parent_sink: Option<NatsEventSink>,
        heartbeat: Duration,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = Arc::new(ProgressEventSink { tx: tx.clone() });
        tokio::spawn(async move {
            let source = AgentSource {
                agent: agent.clone(),
                session_id: Some(session_id.clone()),
                model: None,
            };
            let mut tracker = ProgressTracker::new(agent, session_id, invocation_id);
            let started = tokio::time::Instant::now();
            let first_heartbeat = started + heartbeat;
            let mut heartbeats = tokio::time::interval_at(first_heartbeat, heartbeat);
            heartbeats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    Some(command) = rx.recv() => match command {
                        ProgressCommand::Metric(metric) => {
                            let snapshot = tracker.apply(metric, elapsed_ms(started));
                            publish_progress(parent_sink.as_ref(), &source, snapshot);
                        }
                        ProgressCommand::Finish { status, reply } => {
                            let snapshot = tracker.finish(status, elapsed_ms(started));
                            let delivery = publish_terminal_progress(
                                parent_sink.as_ref(),
                                &source,
                                snapshot.clone(),
                            ).await;
                            let _ = reply.send((snapshot, delivery));
                            break;
                        }
                    },
                    _ = heartbeats.tick() => {
                        let snapshot = tracker.heartbeat(elapsed_ms(started));
                        publish_progress(parent_sink.as_ref(), &source, snapshot);
                    }
                    else => break,
                }
            }
        });
        Self { sink, tx }
    }

    pub(super) fn sink(&self) -> Arc<dyn AgentEventSink> {
        Arc::clone(&self.sink)
    }

    pub(super) async fn finish(
        &self,
        status: SubAgentProgressStatus,
    ) -> anyhow::Result<SubAgentProgress> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ProgressCommand::Finish {
                status,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("sub-agent progress reporter stopped early"))?;
        let (snapshot, delivery) = reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sub-agent progress reporter dropped completion"))?;
        delivery?;
        Ok(snapshot)
    }
}

fn elapsed_ms(started: tokio::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn progress_event(source: &AgentSource, snapshot: SubAgentProgress) -> AgentEvent {
    AgentEvent::sub_agent(
        source.clone(),
        AgentEvent::Turn(TurnEvent::SubAgentProgress(snapshot)),
    )
}

fn publish_progress(
    parent_sink: Option<&NatsEventSink>,
    source: &AgentSource,
    snapshot: SubAgentProgress,
) {
    if let Some(parent_sink) = parent_sink {
        parent_sink.emit(progress_event(source, snapshot));
    }
}

async fn publish_terminal_progress(
    parent_sink: Option<&NatsEventSink>,
    source: &AgentSource,
    snapshot: SubAgentProgress,
) -> anyhow::Result<()> {
    let Some(parent_sink) = parent_sink else {
        return Ok(());
    };
    parent_sink.emit_required(progress_event(source, snapshot));
    parent_sink.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::{ToolKind, ToolLocation};

    fn tracker() -> ProgressTracker {
        ProgressTracker::new("researcher".into(), "session-1".into(), "inv-1".into())
    }

    #[test]
    fn aggregates_usage_and_direct_tool_starts() {
        let mut tracker = tracker();
        tracker.apply(
            ProgressMetric::Usage(CompletionTokenUsage::new(Some(10), Some(4), Some(3))),
            20,
        );
        tracker.apply(
            ProgressMetric::Usage(CompletionTokenUsage::new(Some(6), Some(2), Some(1))),
            40,
        );
        let snapshot = tracker.apply(ProgressMetric::ToolStarted, 50);

        assert_eq!(snapshot.usage.input_tokens, 16);
        assert_eq!(snapshot.usage.output_tokens, 6);
        assert_eq!(snapshot.usage.cached_tokens, 4);
        assert_eq!(snapshot.tool_call_count, 1);
        assert_eq!(snapshot.elapsed_ms, 50);
    }

    #[test]
    fn usage_event_preserves_cache_write_tokens() {
        let event = AgentEvent::Model(ModelEvent::Usage {
            input: 12,
            output: 4,
            cached: 5,
            cache_write: 3,
            session_label: None,
        });

        let Some(ProgressMetric::Usage(usage)) = ProgressMetric::from_event(event) else {
            panic!("expected usage metric");
        };
        assert_eq!(
            usage,
            CompletionTokenUsage {
                input_tokens: 12,
                output_tokens: 4,
                cached_tokens: 5,
                cache_write_tokens: 3,
            }
        );
    }

    #[test]
    fn ignores_nested_agent_metrics() {
        let nested = AgentEvent::sub_agent(
            AgentSource {
                agent: "nested".into(),
                session_id: Some("nested-session".into()),
                model: None,
            },
            AgentEvent::Model(ModelEvent::Usage {
                input: 99,
                output: 88,
                cached: 77,
                cache_write: 66,
                session_label: None,
            }),
        );
        assert!(ProgressMetric::from_event(nested).is_none());
    }

    #[test]
    fn counts_delegation_tools_started_by_the_direct_session() {
        let event = AgentEvent::Tool(ToolEvent::Started {
            id: "call-1".into(),
            name: "reviewer_session_prompt".into(),
            kind: ToolKind::Other,
            markdown: None,
            input: serde_json::json!({}),
            locations: Vec::<ToolLocation>::new(),
        });
        assert!(matches!(
            ProgressMetric::from_event(event),
            Some(ProgressMetric::ToolStarted)
        ));
    }

    #[test]
    fn heartbeat_updates_elapsed_without_changing_metrics() {
        let mut tracker = tracker();
        tracker.apply(ProgressMetric::ToolStarted, 5);
        let heartbeat = tracker.heartbeat(10_000);

        assert_eq!(heartbeat.status, SubAgentProgressStatus::Running);
        assert_eq!(heartbeat.elapsed_ms, 10_000);
        assert_eq!(heartbeat.tool_call_count, 1);
    }

    #[test]
    fn terminal_snapshot_freezes_done_or_failed_state() {
        let mut done = tracker();
        let done = done.finish(SubAgentProgressStatus::Done, 12_345);
        assert_eq!(done.status, SubAgentProgressStatus::Done);
        assert_eq!(done.elapsed_ms, 12_345);

        let mut failed = tracker();
        let failed = failed.finish(SubAgentProgressStatus::Failed, 98);
        assert_eq!(failed.status, SubAgentProgressStatus::Failed);
        assert_eq!(failed.elapsed_ms, 98);
    }
}
