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
use harnx_core::event::{AgentEvent, AgentEventSink, SessionEvent, UserEvent};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use harnx_core::session_reconstruct::{
    active_context_window, reconstruct_state_from_nats, ActiveContextWindow, TurnStatus,
};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use crate::nats_event_sink::SessionEventStream;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_worker::{
    new_remote_session_id, publish_control_command, publish_session_activate, ControlCommand,
    SessionActivate,
};
use crate::utils::AbortSignal;

/// Generate a client-side message ID (UUID v4).
pub(crate) fn new_client_message_id() -> String {
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

    pub(crate) fn jetstream(&self) -> &jetstream::Context {
        &self.jetstream
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
        let effective_history =
            match harnx_core::session_reconstruct::apply_log_mutations_nats(history) {
                Ok(history) => history,
                Err(err) => {
                    log::warn!(
                    "failed to apply NATS log mutations while rendering thin-client history: {err}"
                );
                    history.to_vec()
                }
            };
        let history_window = active_context_window(&effective_history);
        replay_history_to_sink(
            &effective_history,
            &history_window,
            user_msg_seq,
            event_sink.clone(),
        );

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
        // Cached effective log for emitting LogSeqAssigned on live advisory
        // events. Refreshed on throttled durable reloads so we avoid O(N^2)
        // full-log load per streaming token.
        let mut cached_effective: Option<Vec<(u64, SessionLogEntry)>> = None;
        let mut pending_advisories = VecDeque::new();
        let mut emitted_logical_seqs = HashSet::new();
        // Throttle the durable-log completion check: advisory events arrive at
        // streaming-token frequency, and reloading the full log on each would be
        // O(N^2) in session size. Check at most once per interval instead — the
        // turn-complete signal (a final AssistantMessage) is not latency
        // sensitive, and the post-loop reload guarantees we never miss it.
        const COMPLETION_CHECK_INTERVAL: std::time::Duration =
            std::time::Duration::from_millis(500);
        let mut last_completion_check = std::time::Instant::now() - COMPLETION_CHECK_INTERVAL;
        let mut completion_interval = tokio::time::interval(COMPLETION_CHECK_INTERVAL);
        completion_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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

                // Poll durable completion independently so non-streaming workers
                // that persist a final assistant message without an advisory still
                // complete the turn promptly.
                _ = completion_interval.tick() => {
                    if let Ok(entries) = self.load_durable_entries().await {
                        // Refresh cached effective log for live LogSeqAssigned.
                        if let Ok(effective) = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries) {
                            cached_effective = Some(effective);
                            emit_all_logical_seqs_for_window(
                                cached_effective.as_deref(),
                                &event_sink,
                                &mut emitted_logical_seqs,
                            );
                            flush_pending_advisories(
                                &mut pending_advisories,
                                cached_effective.as_deref(),
                                &event_sink,
                                &mut emitted_logical_seqs,
                            );
                        }
                        if self.is_turn_complete(&entries, user_msg_seq) {
                            final_response = Self::extract_final_response(&entries, user_msg_seq);
                            turn_complete = true;
                            break;
                        }
                    }
                }

                // Receive advisory events
                maybe_envelope = event_stream.next() => {
                    match maybe_envelope {
                        Some(envelope) => {
                            // Check if this advisory should be rendered (dedup rule)
                            if event_stream.should_render(&envelope) {
                                pending_advisories.push_back(envelope.clone());
                                flush_pending_advisories(
                                    &mut pending_advisories,
                                    cached_effective.as_deref(),
                                    &event_sink,
                                    &mut emitted_logical_seqs,
                                );
                            }

                            // Poll durable log for turn completion, but at most
                            // once per COMPLETION_CHECK_INTERVAL so bursty
                            // streaming advisories do not trigger full log reload
                            // each time (avoids O(N^2) growth).
                            if last_completion_check.elapsed() >= COMPLETION_CHECK_INTERVAL {
                                last_completion_check = std::time::Instant::now();
                                if let Ok(entries) = self.load_durable_entries().await {
                                    if let Ok(effective) = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries) {
                                        cached_effective = Some(effective);
                                        emit_all_logical_seqs_for_window(
                                cached_effective.as_deref(),
                                &event_sink,
                                &mut emitted_logical_seqs,
                            );
                                        flush_pending_advisories(
                                            &mut pending_advisories,
                                            cached_effective.as_deref(),
                                            &event_sink,
                                            &mut emitted_logical_seqs,
                                        );
                                    }
                                    if self.is_turn_complete(&entries, user_msg_seq) {
                                        // Extract final response before we finish
                                        final_response = Self::extract_final_response(&entries, user_msg_seq);
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
            final_response = Self::extract_final_response(&entries, user_msg_seq);
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
        let seq = usize::try_from(seq).context("JetStream seq does not fit into usize")?;
        let edit_entry = SessionLogEntry::EditEntries {
            from: seq,
            to: seq,
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
            id: Some(new_client_message_id()),
            role: MessageRole::User,
            content: MessageContent::Text(new_text),
            timestamp: None,
            fence_token: None,
        };
        let replacement_yaml = serde_yaml::to_string(&replacement_entry)
            .context("failed to serialize replacement entry for user-message edit")?;
        let seq = usize::try_from(seq).context("JetStream seq does not fit into usize")?;
        let edit_entry = SessionLogEntry::EditEntries {
            from: seq,
            to: seq,
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
    fn is_turn_complete(&self, entries: &[(u64, SessionLogEntry)], user_msg_seq: u64) -> bool {
        let has_assistant_after_user = entries.iter().any(|(seq, entry)| {
            *seq > user_msg_seq
                && matches!(entry, SessionLogEntry::Message { role, .. } if role.is_assistant())
        });

        has_assistant_after_user
            || matches!(
                reconstruct_state_from_nats(entries).turn_status,
                TurnStatus::InFlightCancelled
            )
    }

    /// Extract final assistant response text for current turn from durable entries.
    fn extract_final_response(
        entries: &[(u64, SessionLogEntry)],
        user_msg_seq: u64,
    ) -> Option<String> {
        let turn_entries: Vec<_> = entries
            .iter()
            .filter(|(seq, _)| *seq > user_msg_seq)
            .cloned()
            .collect();

        // Apply mutations so retracted messages are excluded.
        let effective_entries =
            match harnx_core::session_reconstruct::apply_log_mutations_nats(&turn_entries) {
                Ok(entries) => entries,
                Err(err) => {
                    log::warn!(
                        "failed to apply NATS log mutations while extracting final response: {err}"
                    );
                    return None;
                }
            };
        // Find the last assistant message in effective entries for this turn only.
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
fn should_skip_replay_entry(seq: u64, user_msg_seq: u64) -> bool {
    seq == user_msg_seq
}

fn replay_history_to_sink(
    effective_history: &[(u64, SessionLogEntry)],
    history_window: &ActiveContextWindow<'_, (u64, SessionLogEntry)>,
    user_msg_seq: u64,
    sink: Arc<dyn AgentEventSink>,
) {
    for (seq, entry) in effective_history {
        if should_skip_replay_entry(*seq, user_msg_seq) {
            continue;
        }
        render_log_entry_to_sink(entry, *seq, history_window, sink.clone());
    }
}

fn render_log_entry_to_sink(
    entry: &SessionLogEntry,
    physical_seq: u64,
    history_window: &ActiveContextWindow<'_, (u64, SessionLogEntry)>,
    sink: Arc<dyn AgentEventSink>,
) {
    let rendered = match entry {
        SessionLogEntry::Message { role, content, .. } => {
            render_message_entry(role, content, &sink);
            true
        }
        SessionLogEntry::ToolCalls { text, calls, .. } => {
            render_tool_calls_entry(text, calls, &sink);
            true
        }
        SessionLogEntry::ToolResults { results, .. } => {
            render_tool_results_entry(results, &sink);
            false
        }
        SessionLogEntry::Cancel { .. } => {
            render_cancel_entry(&sink);
            false
        }
        SessionLogEntry::Header { .. }
        | SessionLogEntry::DataUrls { .. }
        | SessionLogEntry::Compress { .. }
        | SessionLogEntry::Title { .. }
        | SessionLogEntry::Clear
        | SessionLogEntry::EditEntries { .. }
        | SessionLogEntry::Rewind { .. }
        | SessionLogEntry::Unknown => false,
    };
    // Only emit logical seqs during replay when the window has a Header/Compress
    // boundary. A headerless-origin remote session has no boundary at replay
    // time (the worker inserts the Header via migration only AFTER activation),
    // so any replay-time number would be wrong and — because the TUI never
    // overrides a `Some(seq)` — would permanently mis-label the row. In that
    // case seq assignment is deferred to the post-migration reload pass
    // (`emit_all_logical_seqs_for_window`). An already-headered session (resume)
    // has a boundary and numbers replayed rows immediately here.
    if rendered && history_window.boundary_index().is_some() {
        for logical_index in logical_indices_for_entry(physical_seq, entry, history_window) {
            sink.emit(
                AgentEvent::Session(SessionEvent::LogSeqAssigned { seq: logical_index }),
                None,
            );
        }
    }
    // Note: ToolResults fence_token is not currently exposed in the entry type
    // (it belongs to ToolCalls which should be followed by ToolResults)
}

fn emit_live_logical_seq_for_physical(
    physical_seq: u64,
    history_window: &ActiveContextWindow<'_, (u64, SessionLogEntry)>,
    sink: &Arc<dyn AgentEventSink>,
    emitted_logical_seqs: &mut HashSet<usize>,
) -> Option<usize> {
    let logical_index = history_window
        .logical_indices_for_physical(physical_seq)
        .last()?;
    emit_deduped_logical_seq(logical_index, sink, emitted_logical_seqs);
    Some(logical_index)
}

fn flush_pending_advisories(
    pending_advisories: &mut VecDeque<crate::nats_event_sink::AdvisoryEnvelope>,
    effective_entries: Option<&[(u64, SessionLogEntry)]>,
    sink: &Arc<dyn AgentEventSink>,
    emitted_logical_seqs: &mut HashSet<usize>,
) {
    let Some(effective_entries) = effective_entries else {
        return;
    };
    let window = active_context_window(effective_entries);
    while let Some(envelope) = pending_advisories.front() {
        let Some(_) = emit_live_logical_seq_for_physical(
            envelope.after_seq,
            &window,
            sink,
            emitted_logical_seqs,
        ) else {
            break;
        };
        let envelope = pending_advisories
            .pop_front()
            .expect("front checked before pop");
        if !matches!(
            envelope.event,
            AgentEvent::Session(SessionEvent::LogSeqAssigned { .. })
        ) {
            sink.emit(envelope.event, None);
        }
    }
}

/// Assign logical `LogSeqAssigned` seqs for every renderable row in the
/// post-migration active window, deduped.
///
/// Remote sessions start headerless; the worker inserts the Header via an
/// `EditEntries` migration only AFTER activation (S2). Replay renders the
/// historical rows BEFORE that migration, when the window has no boundary and
/// logical numbers are not yet authoritative — so replay defers seq emission
/// (see `render_log_entry_to_sink`). This pass runs on each post-activation
/// durable reload: once the effective log carries a Header/Compress boundary,
/// it emits the FINAL logical index for each renderable row (replayed history
/// rows AND the just-submitted live user), exactly once via the shared dedup
/// set. The TUI backfills each `seq: None` row with its number and never
/// overrides it, so emitting the correct number once is essential.
///
/// Live worker rows (streamed assistant/tool events) are numbered via advisory
/// translation; the shared dedup set keeps this pass and that path consistent.
fn emit_all_logical_seqs_for_window(
    effective_entries: Option<&[(u64, SessionLogEntry)]>,
    sink: &Arc<dyn AgentEventSink>,
    emitted_logical_seqs: &mut HashSet<usize>,
) {
    let Some(effective_entries) = effective_entries else {
        return;
    };
    let window = active_context_window(effective_entries);
    // Wait for the header-insert migration to land: no boundary means the
    // numbering isn't authoritative yet.
    if window.boundary_index().is_none() {
        return;
    }
    // The logical index of a row is its position WITHIN the active window
    // (Header/Compress boundary = 0). Iterate the window slice directly so the
    // logical index stays aligned even when a boundary trims a pre-window prefix.
    for (logical_index, (_js_seq, entry)) in window.entries().iter().enumerate() {
        let renders = matches!(
            entry,
            SessionLogEntry::Message { .. } | SessionLogEntry::ToolCalls { .. }
        );
        if renders {
            emit_deduped_logical_seq(logical_index, sink, emitted_logical_seqs);
        }
    }
}

fn emit_deduped_logical_seq(
    logical_index: usize,
    sink: &Arc<dyn AgentEventSink>,
    emitted_logical_seqs: &mut HashSet<usize>,
) {
    if emitted_logical_seqs.insert(logical_index) {
        sink.emit(
            AgentEvent::Session(SessionEvent::LogSeqAssigned { seq: logical_index }),
            None,
        );
    }
}

pub(crate) fn logical_indices_for_entry(
    physical_seq: u64,
    entry: &SessionLogEntry,
    history_window: &ActiveContextWindow<'_, (u64, SessionLogEntry)>,
) -> Vec<usize> {
    let logical_indices: Vec<usize> = history_window
        .logical_indices_for_physical(physical_seq)
        .collect();
    if logical_indices.len() <= 1 {
        return logical_indices;
    }
    match entry {
        SessionLogEntry::Message { role, .. } if role.is_user() => logical_indices,
        _ => logical_indices.into_iter().rev().take(1).collect(),
    }
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
    } else if role.is_user() {
        sink.emit(
            AgentEvent::User(UserEvent::Message {
                content: content.to_text(),
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
                markdown: result.markdown.clone(),
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
    use harnx_core::event::AgentSource;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct TestSink {
        count: Arc<AtomicUsize>,
        events: Mutex<Vec<AgentEvent>>,
    }
    impl AgentEventSink for TestSink {
        fn emit(&self, event: AgentEvent, _source: Option<AgentSource>) {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.events.lock().unwrap().push(event);
        }
    }

    fn test_entries(entries: &[(u64, MessageRole, &str)]) -> Vec<(u64, SessionLogEntry)> {
        entries
            .iter()
            .map(|(seq, role, text)| {
                (
                    *seq,
                    SessionLogEntry::Message {
                        id: None,
                        role: *role,
                        content: MessageContent::Text((*text).to_string()),
                        timestamp: None,
                        fence_token: None,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn test_render_message_entry() {
        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSink {
            count: Arc::clone(&count),
            events: Mutex::new(Vec::new()),
        });

        // Test assistant message
        let entry = SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text("Hello".to_string()),
            timestamp: None,
            fence_token: None,
        };
        let empty: Vec<(u64, SessionLogEntry)> = vec![];
        let window = active_context_window(&empty);
        render_log_entry_to_sink(&entry, 0, &window, sink.clone());
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Test user message
        let entry = SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("Hi".to_string()),
            timestamp: None,
            fence_token: None,
        };
        let empty: Vec<(u64, SessionLogEntry)> = vec![];
        let window = active_context_window(&empty);
        render_log_entry_to_sink(&entry, 0, &window, sink.clone());
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_replay_history_skips_last_user_message_only() {
        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSink {
            count: Arc::clone(&count),
            events: Mutex::new(Vec::new()),
        });

        let effective_history = test_entries(&[
            (1, MessageRole::User, "first user"),
            (2, MessageRole::Assistant, "assistant"),
            (3, MessageRole::User, "current user"),
        ]);

        let window = active_context_window(&effective_history);
        replay_history_to_sink(&effective_history, &window, 3, sink.clone());

        // This fixture is HEADERLESS (no boundary): replay RENDERS the two
        // non-current rows but DEFERS seq emission (the worker will migrate a
        // Header in; numbering is only authoritative post-migration). So we see
        // 2 render events and ZERO LogSeqAssigned events.
        assert_eq!(count.load(Ordering::SeqCst), 2);
        let rendered_messages: Vec<String> = sink
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::User(UserEvent::Message { content }) => Some(content.clone()),
                AgentEvent::Model(harnx_core::event::ModelEvent::Final { output, .. }) => {
                    Some(output.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(rendered_messages, vec!["first user", "assistant"]);
        assert!(!rendered_messages.iter().any(|msg| msg == "current user"));
        let replayed_seqs: Vec<usize> = sink
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }) => Some(*seq),
                _ => None,
            })
            .collect();
        assert!(
            replayed_seqs.is_empty(),
            "headerless replay defers seq emission until the migration boundary exists, got {replayed_seqs:?}"
        );
    }

    #[test]
    fn test_replay_with_header_boundary_emits_row_seqs() {
        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSink {
            count: Arc::clone(&count),
            events: Mutex::new(Vec::new()),
        });

        // Resume case: the log already carries a Header (boundary present), so
        // replay numbers rows immediately. Header is logical 0 (renders no row);
        // the first user is logical 1, the assistant logical 2. The current
        // user (seq 4) is skipped from replay.
        let mut effective_history = vec![(
            1u64,
            SessionLogEntry::Header {
                model_id: "m".to_string(),
                temperature: None,
                top_p: None,
                use_tools: None,
                save_session: None,
                compress_threshold: None,
                agent_name: Some("a".to_string()),
                session_id: Some("s".to_string()),
                working_dir: None,
                git_branch: None,
                git_remote: None,
                terminal_session_id: None,
                agent_variables: Default::default(),
                agent_instructions: String::new(),
                model_fallbacks: vec![],
                compaction_agent: None,
            },
        )];
        effective_history.extend(test_entries(&[
            (2, MessageRole::User, "first user"),
            (3, MessageRole::Assistant, "assistant"),
            (4, MessageRole::User, "current user"),
        ]));

        let window = active_context_window(&effective_history);
        replay_history_to_sink(&effective_history, &window, 4, sink.clone());

        let replayed_seqs: Vec<usize> = sink
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }) => Some(*seq),
                _ => None,
            })
            .collect();
        assert_eq!(replayed_seqs, vec![1, 2]);
    }

    #[test]
    fn test_emit_all_logical_seqs_numbers_every_renderable_window_row() {
        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSink {
            count: Arc::clone(&count),
            events: Mutex::new(Vec::new()),
        });
        let sink_trait: Arc<dyn AgentEventSink> = sink.clone();
        let user_msg_id = "live-user".to_string();
        let effective_history = vec![
            (
                3,
                SessionLogEntry::Header {
                    model_id: "test-model".to_string(),
                    temperature: None,
                    top_p: None,
                    use_tools: None,
                    save_session: None,
                    compress_threshold: None,
                    agent_name: Some("test-agent".to_string()),
                    session_id: Some("session".to_string()),
                    working_dir: None,
                    git_branch: None,
                    git_remote: None,
                    terminal_session_id: None,
                    agent_variables: Default::default(),
                    agent_instructions: String::new(),
                    model_fallbacks: vec![],
                    compaction_agent: None,
                },
            ),
            (
                3,
                SessionLogEntry::Message {
                    id: Some("legacy-user".to_string()),
                    role: MessageRole::User,
                    content: MessageContent::Text("legacy".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                3,
                SessionLogEntry::Message {
                    id: Some(user_msg_id.clone()),
                    role: MessageRole::User,
                    content: MessageContent::Text("live user".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                4,
                SessionLogEntry::Message {
                    id: Some("assistant".to_string()),
                    role: MessageRole::Assistant,
                    content: MessageContent::Text("reply".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
        ];
        let mut emitted_logical_seqs = HashSet::new();

        emit_all_logical_seqs_for_window(
            Some(&effective_history),
            &sink_trait,
            &mut emitted_logical_seqs,
        );

        // Window = [Header(0), legacy-user(1), live-user(2), assistant(3)].
        // The Header renders no row; every message row is numbered by its
        // logical position, deduped and in order.
        let assigned_seqs: Vec<usize> = sink
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }) => Some(*seq),
                _ => None,
            })
            .collect();
        assert_eq!(assigned_seqs, vec![1, 2, 3]);
    }

    #[test]
    fn test_logical_indices_for_entry_user_fans_out_shared_seq() {
        let effective_history = vec![
            (
                50,
                SessionLogEntry::Header {
                    model_id: "test-model".to_string(),
                    temperature: None,
                    top_p: None,
                    use_tools: None,
                    save_session: None,
                    compress_threshold: None,
                    agent_name: Some("test-agent".to_string()),
                    session_id: Some("session".to_string()),
                    working_dir: None,
                    git_branch: None,
                    git_remote: None,
                    terminal_session_id: None,
                    agent_variables: Default::default(),
                    agent_instructions: String::new(),
                    model_fallbacks: vec![],
                    compaction_agent: None,
                },
            ),
            (
                51,
                SessionLogEntry::Message {
                    id: Some("u1".to_string()),
                    role: MessageRole::User,
                    content: MessageContent::Text("u1".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                51,
                SessionLogEntry::Message {
                    id: Some("u2".to_string()),
                    role: MessageRole::User,
                    content: MessageContent::Text("u2".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                51,
                SessionLogEntry::Message {
                    id: Some("a1".to_string()),
                    role: MessageRole::Assistant,
                    content: MessageContent::Text("a1".to_string()),
                    timestamp: None,
                    fence_token: None,
                },
            ),
        ];
        let window = active_context_window(&effective_history);

        let user_indices = logical_indices_for_entry(51, &effective_history[1].1, &window);
        let assistant_indices = logical_indices_for_entry(51, &effective_history[3].1, &window);

        assert_eq!(user_indices, vec![1, 2, 3]);
        assert_eq!(assistant_indices, vec![3]);
    }
    #[test]
    fn test_render_tool_results_entry() {
        use harnx_core::event::ToolEvent;
        use harnx_core::session::ToolOutput;

        let count = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(TestSink {
            count: Arc::clone(&count),
            events: Mutex::new(Vec::new()),
        });

        let entry = SessionLogEntry::ToolResults {
            results: vec![ToolOutput {
                id: Some("test".to_string()),
                name: "echo".to_string(),
                output: serde_json::json!({"result": "ok"}),
                markdown: Some("rendered summary".to_string()),
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        };
        let empty: Vec<(u64, SessionLogEntry)> = vec![];
        let window = active_context_window(&empty);
        render_log_entry_to_sink(&entry, 0, &window, sink.clone());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::Tool(ToolEvent::Completed { markdown, .. }) => {
                assert_eq!(markdown.as_deref(), Some("rendered summary"));
            }
            other => panic!("expected Completed event, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_final_response_ignores_stale_prior_turn_reply() {
        let entries = test_entries(&[
            (1, MessageRole::User, "old prompt"),
            (2, MessageRole::Assistant, "old"),
            (3, MessageRole::User, "new prompt"),
            (4, MessageRole::Assistant, "new"),
        ]);

        assert_eq!(
            ThinClientSession::extract_final_response(&entries, 3),
            Some("new".to_string())
        );
    }

    #[test]
    fn test_extract_final_response_returns_none_without_new_assistant_reply() {
        let entries = test_entries(&[
            (1, MessageRole::User, "old prompt"),
            (2, MessageRole::Assistant, "old"),
            (3, MessageRole::User, "new prompt"),
        ]);

        assert_eq!(ThinClientSession::extract_final_response(&entries, 3), None);
    }
}
