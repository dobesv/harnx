//! Thin-client driver for remote NATS agents (agent@cluster).
//!
//! P4.2 implementation: when an agent ref contains `@` (parsed as
//! `AgentRef::Remote { agent, cluster }`), the client runs in THIN mode:
//! it posts the user message + control commands to NATS and renders events
//! from the fan-out — it does NOT run `run_agent_loop` locally. A separate
//! `harnx worker --cluster <key>` daemon executes the turn.
//!
//! ## Architecture
//!
//! ```text
//! [Client]                                   [Worker Daemon]
//!    |                                            |
//!    |--1. Append user message (durable)--------->|
//!    |                                            |
//!    |--2. SessionEventStream::attach()           |
//!    |    (subscribe-first, then history)         |
//!    |                                            |
//!    |--3. publish_session_activate()------------>|-- wake up
//!    |                                            |-- claim lease
//!    |                                            |-- run_agent_loop
//!    |<----------- advisory events ---------------|
//!    |                                            |-- append final state
//!    |--4. Detect turn complete from durable -----|
//!    |                                            |
//!    |--5. Return final response                  |
//! ```
//!
//! ## Turn completion detection
//!
//! A turn is complete when the durable log contains a final AssistantMessage
//! with no pending ToolCalls, or when `reconstruct_state` yields `TurnStatus::Idle`
//! or `TurnStatus::InFlightCancelled`. The client polls the durable log after
//! each advisory batch to check for completion.

use anyhow::{Context, Result};
use async_nats::jetstream;
use harnx_core::abort::wait_abort_signal;
use harnx_core::event::{AgentEvent, AgentEventSink};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use harnx_core::session_reconstruct::{reconstruct_state_from_nats, TurnStatus};
use std::sync::Arc;

use crate::nats_event_sink::SessionEventStream;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_worker::{
    new_remote_session_id, publish_control_command, publish_session_activate, ControlCommand,
    SessionActivate,
};
use crate::utils::AbortSignal;

/// Generate a client-side message ID (UUID v4).
fn new_client_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Configuration for a thin-client session.
#[derive(Clone)]
pub struct ThinClientConfig {
    /// Cluster key (from AgentRef::Remote { cluster, ... }).
    pub cluster: String,
    /// Agent name (from AgentRef::Remote { agent, ... }).
    pub agent: String,
    /// Existing session ID to resume/attach (None = new session).
    pub session_id: Option<String>,
}

/// Thin-client session driver.
///
/// Orchestrates the remote-agent workflow: append user message, activate,
/// stream events, detect completion.
pub struct ThinClientSession {
    config: ThinClientConfig,
    session_id: String,
    jetstream: jetstream::Context,
    client: async_nats::Client,
    abort_signal: AbortSignal,
}

impl ThinClientSession {
    /// Create a new thin-client session.
    ///
    /// Connects to the NATS cluster and generates/reuses a session ID.
    ///
    /// This function takes the NATS connection components directly to avoid
    /// Send issues with GlobalConfig's parking_lot lock guard across await points.
    pub async fn new(
        config: ThinClientConfig,
        client: async_nats::Client,
        jetstream: jetstream::Context,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        // Resolve session ID (new or existing)
        let session_id = config
            .session_id
            .clone()
            .unwrap_or_else(new_remote_session_id);

        Ok(Self {
            config,
            session_id,
            jetstream,
            client,
            abort_signal,
        })
    }

    /// Convenience constructor that builds NATS connections from GlobalConfig.
    pub async fn from_global_config(
        config: ThinClientConfig,
        global_config: &crate::config::GlobalConfig,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        let cluster = config.cluster.clone();
        let server = {
            let cfg = global_config.read();
            cfg.nats_server(&cluster)?.clone()
        };

        let client = crate::config::Config::connect_nats_server(&server)
            .await
            .context("failed to connect to NATS cluster for thin client")?;
        let jetstream = async_nats::jetstream::new(client.clone());

        Self::new(config, client, jetstream, abort_signal).await
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Run a turn: append user message, activate worker, stream events until completion.
    ///
    /// This is the main entry point for all frontends (CLI, TUI, ACP).
    ///
    /// # Arguments
    /// * `user_message` - The user's prompt text.
    /// * `event_sink` - Event sink to render events (AgentEventSink impl).
    /// * `pending_cancel` - Optional channel to receive cancel requests.
    ///
    /// # Returns
    /// The final assistant response text (if any), plus the sequence number of the
    /// appended user message (for retract/edit), or an error.
    pub async fn run_turn(
        &self,
        user_message: &str,
        event_sink: Arc<dyn AgentEventSink>,
        pending_cancel: Option<tokio::sync::mpsc::Receiver<()>>,
    ) -> Result<ThinClientTurnResult> {
        // Step 1: Append user message to durable log BEFORE activating.
        // The worker derives input from the last user message.
        // Generate a client-side ID for retract/edit reference.
        let user_msg_id = new_client_message_id();
        let log = NatsSessionLog::new(self.jetstream.clone(), self.session_id.clone());
        let user_entry = SessionLogEntry::Message {
            id: Some(user_msg_id.clone()),
            role: MessageRole::User,
            content: MessageContent::Text(user_message.to_string()),
            timestamp: None,
            fence_token: None,
        };
        let user_msg_seq = log
            .append_event_async(&user_entry)
            .await
            .context("failed to append user message to session log")?;

        log::info!(
            "thin client: appended user message session_id={} len={}",
            self.session_id,
            user_message.len()
        );

        // Step 2: Attach to session event stream (subscribe-first, then history).
        let event_stream = SessionEventStream::attach(
            self.jetstream.clone(),
            self.client.clone(),
            &self.session_id,
        )
        .await
        .context("failed to attach to session event stream")?;

        // Render history to event sink (for resume/attach scenarios)
        let history = event_stream.history();
        // Apply mutations so retracted/edited entries are excluded from history render.
        let raw_with_seq: Vec<_> = history
            .iter()
            .map(|(seq, e)| (*seq as usize, e.clone()))
            .collect();
        let effective_history = harnx_core::session_reconstruct::apply_log_mutations(&raw_with_seq);
        for (_seq, entry) in effective_history {
            render_log_entry_to_sink(&entry, event_sink.clone());
        }

        // Step 3: Publish activation to wake a worker.
        let activation = SessionActivate::new(&self.session_id, &self.config.agent);
        publish_session_activate(&self.jetstream, &self.config.cluster, &activation)
            .await
            .context("failed to publish session activation")?;

        log::info!(
            "thin client: published activation session_id={} agent={} cluster={}",
            self.session_id,
            self.config.agent,
            self.config.cluster
        );

        // Step 4: Stream events and handle control until turn completion.
        let mut event_stream = event_stream;
        let mut final_response: Option<String> = None;
        let mut turn_complete = false;
        // Throttle the durable-log completion check: advisory events arrive at
        // streaming-token frequency, and reloading the full log on each would be
        // O(N^2) in session size. Check at most once per interval instead — the
        // turn-complete signal (a final AssistantMessage) is not latency
        // sensitive, and the post-loop reload guarantees we never miss it.
        const COMPLETION_CHECK_INTERVAL: std::time::Duration =
            std::time::Duration::from_millis(500);
        let mut last_completion_check = std::time::Instant::now() - COMPLETION_CHECK_INTERVAL;

        // Set up control command sender
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<ControlCommand>(16);
        let session_id_control = self.session_id.clone();
        let client_control = self.client.clone();

        let control_task = tokio::spawn(async move {
            while let Some(cmd) = control_rx.recv().await {
                if let Err(e) =
                    publish_control_command(&client_control, &session_id_control, &cmd).await
                {
                    log::warn!("thin client: failed to publish control command: {e}");
                }
            }
        });

        // Wrap channel in Option for select! handling
        let mut pending_cancel_rx = pending_cancel;

        // Main event loop with abort signal handling
        let abort_signal_clone = self.abort_signal.clone();
        loop {
            tokio::select! {
                // Check for cancellation via wait_abort_signal
                _ = wait_abort_signal(&abort_signal_clone) => {
                    log::info!("thin client: abort signal received, sending cancel");
                    let _ = control_tx.send(ControlCommand::Cancel).await;
                    break;
                }

                // Handle pending cancel requests
                Some(()) = async {
                    match &mut pending_cancel_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    log::info!("thin client: pending cancel received");
                    let _ = control_tx.send(ControlCommand::Cancel).await;
                }

                // Receive advisory events
                maybe_envelope = event_stream.next() => {
                    match maybe_envelope {
                        Some(envelope) => {
                            // Check if this advisory should be rendered (dedup rule)
                            if event_stream.should_render(&envelope) {
                                // Emit to sink for rendering
                                event_sink.emit(envelope.event.clone(), None);
                            }

                            // Poll the durable log for turn completion, but at
                            // most once per COMPLETION_CHECK_INTERVAL so a burst
                            // of streaming advisories doesn't trigger a full
                            // log reload each (avoids O(N^2) growth).
                            if last_completion_check.elapsed() >= COMPLETION_CHECK_INTERVAL {
                                last_completion_check = std::time::Instant::now();
                                if let Ok(entries) = self.load_durable_entries().await {
                                    if self.is_turn_complete(&entries) {
                                        // Extract final response before we finish
                                        final_response = self.extract_final_response(&entries);
                                        turn_complete = true;
                                        break;
                                    }
                                }
                            }
                        }
                        None => {
                            // Subscription closed, check final state
                            log::info!("thin client: event stream closed");
                            break;
                        }
                    }
                }
            }
        }

        // Cleanup: stop control task
        drop(control_tx);
        control_task.abort();

        // Re-load durable log to get final state if we haven't already
        if !turn_complete {
            let entries = self.load_durable_entries().await?;
            final_response = self.extract_final_response(&entries);
        }

        Ok(ThinClientTurnResult {
            response: final_response,
            session_id: self.session_id.clone(),
            was_cancelled: self.abort_signal.aborted(),
            user_msg_seq,
            user_msg_id,
        })
    }

    /// Retract (delete) a queued user message by appending an EditEntries delete.
    ///
    /// This is used to retract a user message that was appended but not yet
    /// consumed by a worker. The retract is only valid before an assistant
    /// barrier (response) appears in the log. The edit-vs-deliver race is
    /// accepted as benign per design.
    ///
    /// # Arguments
    /// * `seq` - The JetStream sequence number of the user message to retract.
    ///
    /// # Returns
    /// The sequence number of the appended EditEntries entry.
    pub async fn retract_user_message(&self, seq: u64) -> Result<u64> {
        let log = NatsSessionLog::new(self.jetstream.clone(), self.session_id.clone());
        let edit_entry = SessionLogEntry::EditEntries {
            from: seq as usize,
            to: seq as usize,
            replacements: vec![], // Empty = deletion
        };
        log.append_event_async(&edit_entry)
            .await
            .context("failed to append EditEntries for retraction")
    }

    /// Edit (replace) a queued user message by appending an EditEntries replace.
    ///
    /// This is used to edit a user message that was appended but not yet
    /// consumed by a worker. The edit is only valid before an assistant
    /// barrier (response) appears in the log. The edit-vs-deliver race is
    /// accepted as benign per design.
    ///
    /// # Arguments
    /// * `seq` - The JetStream sequence number of the user message to edit.
    /// * `new_text` - The replacement user-message text.
    ///
    /// # Returns
    /// The sequence number of the appended EditEntries entry.
    pub async fn edit_user_message(&self, seq: u64, new_text: String) -> Result<u64> {
        let log = NatsSessionLog::new(self.jetstream.clone(), self.session_id.clone());
        let replacement_entry = SessionLogEntry::Message {
            id: Some(uuid::Uuid::new_v4().to_string()),
            role: MessageRole::User,
            content: MessageContent::Text(new_text),
            timestamp: None,
            fence_token: None,
        };
        let replacement_yaml = serde_yaml::to_string(&replacement_entry)
            .context("failed to serialize replacement entry for user-message edit")?;
        let edit_entry = SessionLogEntry::EditEntries {
            from: seq as usize,
            to: seq as usize,
            replacements: vec![replacement_yaml],
        };
        log.append_event_async(&edit_entry)
            .await
            .context("failed to append EditEntries for edit")
    }

    /// Load all durable entries from the session log.
    async fn load_durable_entries(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
        let log = NatsSessionLog::new(self.jetstream.clone(), self.session_id.clone());
        log.load_events_async()
            .await
            .context("failed to load durable session log")
    }

    /// Check if the turn is complete based on durable entries.
    fn is_turn_complete(&self, entries: &[(u64, SessionLogEntry)]) -> bool {
        let state = reconstruct_state_from_nats(entries);
        matches!(
            state.turn_status,
            TurnStatus::Idle | TurnStatus::InFlightCancelled
        )
    }

    /// Extract the final assistant response text from durable entries.
    fn extract_final_response(&self, entries: &[(u64, SessionLogEntry)]) -> Option<String> {
        // Apply mutations so retracted messages are excluded.
        let raw_with_seq: Vec<_> = entries
            .iter()
            .map(|(seq, e)| (*seq as usize, e.clone()))
            .collect();
        let effective_entries = harnx_core::session_reconstruct::apply_log_mutations(&raw_with_seq);
        // Find the last assistant message in effective entries.
        for (_, entry) in effective_entries.iter().rev() {
            if let SessionLogEntry::Message { role, content, .. } = entry {
                if role.is_assistant() {
                    return Some(content.to_text());
                }
            }
        }
        None
    }
}

/// Result of a thin-client turn.
#[derive(Debug, Clone)]
pub struct ThinClientTurnResult {
    /// Final assistant response text (if any).
    pub response: Option<String>,
    /// Session ID (for resume/attach).
    pub session_id: String,
    /// Whether the turn was cancelled.
    pub was_cancelled: bool,
    /// Sequence number of the appended user message (for retract/edit).
    pub user_msg_seq: u64,
    /// Client-generated message ID of the appended user message.
    pub user_msg_id: String,
}

/// Render a session log entry to an event sink.
///
/// Used for rendering history when attaching to an existing session.
fn render_log_entry_to_sink(entry: &SessionLogEntry, sink: Arc<dyn AgentEventSink>) {
    match entry {
        SessionLogEntry::Message { role, content, .. } => {
            render_message_entry(role, content, &sink)
        }
        SessionLogEntry::ToolCalls { text, calls, .. } => {
            render_tool_calls_entry(text, calls, &sink)
        }
        SessionLogEntry::ToolResults { results, .. } => render_tool_results_entry(results, &sink),
        SessionLogEntry::Cancel { .. } => render_cancel_entry(&sink),
        SessionLogEntry::Header { .. }
        | SessionLogEntry::DataUrls { .. }
        | SessionLogEntry::Compress { .. }
        | SessionLogEntry::Clear
        | SessionLogEntry::EditEntries { .. }
        | SessionLogEntry::Rewind { .. }
        | SessionLogEntry::Unknown => {}
    }
    // Note: ToolResults fence_token is not currently exposed in the entry type
    // (it belongs to ToolCalls which should be followed by ToolResults)
}

fn render_message_entry(
    role: &harnx_core::message::MessageRole,
    content: &harnx_core::message::MessageContent,
    sink: &Arc<dyn AgentEventSink>,
) {
    if role.is_assistant() {
        use harnx_core::event::ModelEvent;
        sink.emit(
            AgentEvent::Model(ModelEvent::Final {
                output: content.to_text(),
                usage: Default::default(),
            }),
            None,
        );
    }
}

fn render_tool_calls_entry(
    text: &str,
    calls: &[harnx_core::tool::ToolCall],
    sink: &Arc<dyn AgentEventSink>,
) {
    use harnx_core::event::{ContentBlock, ModelEvent, ToolEvent, ToolKind};

    for call in calls {
        sink.emit(
            AgentEvent::Tool(ToolEvent::Started {
                id: call.id.clone().unwrap_or_default(),
                name: call.name.clone(),
                kind: ToolKind::Other,
                markdown: None,
                input: call.arguments.clone(),
                locations: vec![],
            }),
            None,
        );
    }

    if !text.is_empty() {
        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(text.to_string())],
            }),
            None,
        );
    }
}

fn render_tool_results_entry(
    results: &[harnx_core::session::ToolOutput],
    sink: &Arc<dyn AgentEventSink>,
) {
    use harnx_core::event::ToolEvent;

    for result in results {
        sink.emit(
            AgentEvent::Tool(ToolEvent::Completed {
                id: result.id.clone().unwrap_or_default(),
                output: result.output.clone(),
                markdown: None,
            }),
            None,
        );
    }
}

fn render_cancel_entry(sink: &Arc<dyn AgentEventSink>) {
    use harnx_core::event::NoticeEvent;

    sink.emit(
        AgentEvent::Notice(NoticeEvent::Warning("Session cancelled".to_string())),
        None,
    );
}

/// Send a control command to a remote session.
///
/// Helper for frontends to send cancel/set-pending without full ThinClientSession.
pub async fn send_control_command(
    client: &async_nats::Client,
    session_id: &str,
    command: ControlCommand,
) -> Result<()> {
    publish_control_command(client, session_id, &command).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestSink {
        count: Arc<AtomicUsize>,
    }
    impl AgentEventSink for TestSink {
        fn emit(&self, _event: AgentEvent, _source: Option<harnx_core::event::AgentSource>) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn test_render_message_entry() {
        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSink {
            count: Arc::clone(&count),
        });

        // Test assistant message
        let entry = SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text("Hello".to_string()),
            timestamp: None,
            fence_token: None,
        };
        render_log_entry_to_sink(&entry, sink.clone());
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Test user message (should not render)
        let entry = SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("Hi".to_string()),
            timestamp: None,
            fence_token: None,
        };
        render_log_entry_to_sink(&entry, sink.clone());
        assert_eq!(count.load(Ordering::SeqCst), 1); // Still 1
    }

    #[test]
    fn test_render_tool_results_entry() {
        use harnx_core::session::ToolOutput;

        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSink {
            count: Arc::clone(&count),
        });

        let entry = SessionLogEntry::ToolResults {
            results: vec![ToolOutput {
                id: Some("test".to_string()),
                name: "echo".to_string(),
                output: serde_json::json!({"result": "ok"}),
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        };
        render_log_entry_to_sink(&entry, sink.clone());
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
