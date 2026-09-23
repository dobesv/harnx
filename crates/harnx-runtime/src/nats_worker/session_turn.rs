//! Turn-local state. No session dispatcher, active-map or lease-release authority.
use super::agent_loop::{
    build_mid_turn_injection_callback, has_orphan_tool_calls, run_agent_loop_with_nats_outcome,
    NatsAgentLoopOutcome, RunAgentLoopArgs,
};
use super::backend::NatsSessionLogBackend;
use super::control::AppliedHitlDecision;
use super::daemon::SessionActivate;
use super::daemon_runtime::WorkerRuntime;
use super::daemon_session_exec::{build_durable_tool_round_callback, ToolRoundAttachmentSync};
use super::daemon_turn_input::TurnInputCtx;
use crate::config::session::SessionAppendSink;
use crate::config::{GlobalConfig, Input};
use crate::nats_event_sink::NatsEventSink;
use crate::nats_lease::NatsSessionLease;
use anyhow::{Context, Result};
use harnx_core::event::AgentEventSink;
use parking_lot::Mutex;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

#[derive(Clone)]
pub(super) struct TurnWorker {
    pub cluster: String,
    pub instance_id: harnx_core::instance::ServerScope,
    pub manage_servers: bool,
    pub activation_route: super::SessionActivationRoute,
    pub call_fn: Option<crate::agent_loop::AgentCallFn>,
    pub session_metadata: crate::nats_session_metadata::SessionMetadataStore,
    pub jetstream: async_nats::jetstream::Context,
    /// Core NATS handle, used to publish the interrupt this worker appends
    /// when it refuses to resume under an interrupted parent.
    pub client: async_nats::Client,
    pub replicas: usize,
    pub worker_id: String,
}

impl From<&WorkerRuntime> for TurnWorker {
    fn from(worker: &WorkerRuntime) -> Self {
        Self {
            cluster: worker.cluster.clone(),
            instance_id: worker.instance_id.clone(),
            manage_servers: worker.manage_servers,
            activation_route: worker.activation_route.clone(),
            call_fn: worker.call_fn.clone(),
            session_metadata: worker.session_metadata.clone(),
            jetstream: worker.jetstream.clone(),
            client: worker.client.clone(),
            replicas: worker.lease.replicas,
            worker_id: worker.worker_id.clone(),
        }
    }
}

pub(super) struct SessionTurn {
    pub worker: TurnWorker,
    pub activation: SessionActivate,
    pub lease: Arc<NatsSessionLease>,
    pub abort_signal: crate::utils::AbortSignal,
    pub hitl_decision_rx: tokio::sync::mpsc::UnboundedReceiver<AppliedHitlDecision>,
    pub per_session: GlobalConfig,
    pub backend: NatsSessionLogBackend,
    pub event_sink: Arc<NatsEventSink>,
    pub after_seq_observer: Arc<AtomicU64>,
    /// Set by the session watcher the moment a user message lands. The turn
    /// boundary consumes it so queued input runs without a new activation.
    pub pending_input: Arc<AtomicBool>,
    /// Pending manual compaction request detected on activation or mid-turn.
    /// Stores the compaction_id from the CompactRequest that needs resolution.
    pub pending_compaction: Arc<Mutex<Option<String>>>,
    /// Cached entries from prepare_turn() for reuse in execute_manual_compaction().
    pub(super) prepare_turn_entries: Option<Vec<(u64, harnx_core::session::SessionLogEntry)>>,
    pub agent_setup: Result<()>,
}

impl SessionTurn {
    /// Runs this activation's turns and reports whether it is settled: whether
    /// everything the activation was published for has now happened, so it may
    /// be acknowledged instead of redelivered.
    pub async fn run(mut self) -> Result<bool> {
        let event_sink = Arc::clone(&self.event_sink);
        harnx_core::sink::with_agent_event_sink(event_sink, async {
            // A half-installed agent cannot render its prompt. Let the caller
            // record the setup failure rather than failing deeper in the turn.
            match self.agent_setup {
                Ok(()) => self.run_turns().await,
                Err(error) => Err(error),
            }
        })
        .await
    }

    async fn run_turns(&mut self) -> Result<bool> {
        // Includes both folded input and mid-round injections. Drain checks only
        // detect new messages; only consumed messages advance this cursor.
        let mut activation_high_water = None;
        loop {
            // First hydrate and prepare turn - this may set pending_compaction
            // via prepare_turn -> derive_pending_compaction.
            let Some((input, seed_cursor)) = self.next_turn_input(activation_high_water).await?
            else {
                // No user turn to run. Check for pending maintenance (manual compaction)
                // before exiting - idle sessions must still execute compaction.
                self.maybe_execute_pending_compaction().await?;
                break;
            };
            // Check for pending maintenance that arrived mid-turn or during prepare.
            self.maybe_execute_pending_compaction().await?;
            log::info!(
                "execute_session turn: session_id={} seed_cursor={:?}",
                self.activation.session_id,
                Some(seed_cursor),
            );
            let turn_cursor = Arc::new(AtomicU64::new(seed_cursor));
            advance_high_water(&mut activation_high_water, seed_cursor);
            let outcome = self.run_agent_turn(input, Arc::clone(&turn_cursor)).await?;
            // The unmatched request keeps the tool round pending: only the
            // activation that follows the decision can run it.
            if outcome == NatsAgentLoopOutcome::AwaitingHitlApproval {
                return Ok(false);
            }
            let turn_cursor = turn_cursor.load(Ordering::SeqCst);
            advance_high_water(&mut activation_high_water, turn_cursor);
            self.record_turn_end(turn_cursor).await?;
            log::info!(
                "execute_session turn complete: session_id={} turn_cursor={} activation_high_water={:?}",
                self.activation.session_id,
                turn_cursor,
                activation_high_water,
            );
            // The handoff target is already activated. Reconstructing the source
            // tool result as an in-flight round would dispatch it a second time.
            if !self.is_running() || outcome == NatsAgentLoopOutcome::HandoffDispatched {
                return Ok(true);
            }
            // Retain the frontend-scoped activation route while draining queued
            // continuations. Requests to a detached frontend still fail closed.
            if self.finish_if_drained(activation_high_water).await? {
                return Ok(true);
            }
        }
        Ok(true)
    }

    fn is_running(&self) -> bool {
        self.lease.is_held() && !self.abort_signal.aborted()
    }

    async fn next_turn_input(&mut self, high_water: Option<u64>) -> Result<Option<(Input, u64)>> {
        if !self.is_running() || !self.prepare_turn().await? {
            return Ok(None);
        }
        self.derive_input(high_water).await
    }

    async fn prepare_turn(&mut self) -> Result<bool> {
        // Store entries for reuse in execute_manual_compaction() to avoid duplicate leader read.
        self.prepare_turn_entries = Some(self.backend.load_events_latest_async().await?);
        let entries = self
            .prepare_turn_entries
            .as_ref()
            .expect("entries just stored");
        // Repair attention bumps lost after their durable transcript append.
        self.backend.reconcile_attention_from_log(entries).await?;

        // Check for pending manual compaction and record it for execution.
        if let Some(compaction_id) = super::agent_loop::derive_pending_compaction(entries) {
            log::info!(
                "prepare_turn detected pending compaction: session_id={} compaction_id={compaction_id}",
                self.activation.session_id
            );
            *self.pending_compaction.lock() = Some(compaction_id);
        }

        let pending_hitl = super::agent_loop::derive_pending_hitl_approvals(entries)?;
        if pending_hitl.is_empty() {
            return Ok(true);
        }
        Ok(self.wait_for_hitl_decision().await)
    }

    async fn wait_for_hitl_decision(&mut self) -> bool {
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.hitl_decision_rx.recv(),
        )
        .await
        {
            Ok(Some(decision)) => {
                log::debug!(
                    "received durable HITL decision: session_id={} tool_call_id={} approved={}",
                    self.activation.session_id,
                    decision.tool_call_id,
                    decision.approved
                );
                true
            }
            Ok(None) | Err(_) => {
                log::debug!(
                    "ending activation while HITL approval remains pending: session_id={}",
                    self.activation.session_id
                );
                false
            }
        }
    }

    async fn derive_input(&self, high_water: Option<u64>) -> Result<Option<(Input, u64)>> {
        let (input, seed_cursor) = self
            .worker
            .derive_turn_input(
                TurnInputCtx {
                    activation: &self.activation,
                    per_session: &self.per_session,
                    backend: &self.backend,
                },
                &self.lease,
                high_water,
            )
            .await?;
        match (input.is_empty(), seed_cursor) {
            (true, None) => {
                log::info!(
                    "execute_session has no durable turn to run: session_id={}",
                    self.activation.session_id,
                );
                Ok(None)
            }
            (true, Some(_)) => {
                anyhow::bail!("refusing to complete a durable worker turn with empty input")
            }
            (false, None) => {
                anyhow::bail!("refusing to run a worker turn without a durable user cursor")
            }
            (false, Some(cursor)) => Ok(Some((input, cursor))),
        }
    }

    async fn run_agent_turn(
        &self,
        input: Input,
        turn_cursor: Arc<AtomicU64>,
    ) -> Result<NatsAgentLoopOutcome> {
        let injection = build_mid_turn_injection_callback(self.backend.clone(), turn_cursor);
        let attachment_sync = ToolRoundAttachmentSync {
            jetstream: self.worker.jetstream.clone(),
            config: self.per_session.clone(),
            replicas: self.worker.replicas,
            session_id: self.activation.session_id.clone(),
        };
        let on_tool_round = build_durable_tool_round_callback(injection, attachment_sync);
        // Recovery and model/tool dispatch make this future large. Inline
        // construction and polling can exhaust a Tokio worker's debug stack.
        let outcome = Box::pin(run_agent_loop_with_nats_outcome(
            RunAgentLoopArgs {
                cluster_key: &self.worker.cluster,
                manage_servers: self.worker.manage_servers,
                session_id: &self.activation.session_id,
                config: self.per_session.clone(),
                instance_id: self.worker.instance_id.clone(),
                initial_input: input,
                abort_signal: self.abort_signal.clone(),
                token_budget: self.activation.token_budget,
                call_fn: self.worker.call_fn.clone(),
                lease: None,
                activation_route: self.worker.activation_route.clone(),
                event_sink: Some(Arc::clone(&self.event_sink)),
                after_seq_observer: None,
                session_metadata: Some(&self.worker.session_metadata),
                on_tool_round: Some(on_tool_round),
                working_dir: None,
            }
            .with_lease(Arc::clone(&self.lease))
            .with_after_seq_observer(Arc::clone(&self.after_seq_observer)),
        ))
        .await?;
        // Compaction/title tasks can still append through the lease-fenced sink.
        // Wait before publishing TurnEnd or allowing ownership to be released.
        WorkerRuntime::wait_for_post_turn_maintenance(&self.per_session, &self.lease).await;
        Ok(outcome)
    }

    async fn record_turn_end(&self, turn_cursor: u64) -> Result<()> {
        if self.abort_signal.aborted() {
            return Ok(());
        }
        let usage = self
            .per_session
            .read()
            .session
            .as_ref()
            .map(|session| session.completion_usage().clone())
            .unwrap_or_default();
        WorkerRuntime::record_session_turn_end(
            &self.backend,
            &self.lease,
            Some(&*self.event_sink),
            turn_cursor,
            usage,
        )
        .await
    }

    async fn finish_if_drained(&self, high_water: Option<u64>) -> Result<bool> {
        // The watcher sees a user message the moment it lands, including one
        // that arrives while the tail read below is in flight. Consuming that
        // flag here costs at most one extra (empty) pass of the turn loop and
        // closes the window where queued input would wait for a new activation.
        if self.pending_input.swap(false, Ordering::Relaxed) {
            return Ok(false);
        }
        // Pending compaction maintenance must block drain.
        if self.has_pending_compaction() {
            return Ok(false);
        }
        // A fresh leader-authoritative read sees our completion boundary and
        // concurrent edits/retractions. Reconstruction preserves NATS sequences.
        let tail = self.backend.load_events_latest_async().await?;
        let reconstructed = harnx_core::session_reconstruct::reconstruct_state_from_nats(&tail);
        let has_resumable = reconstructed.resumable_ctx.is_some();
        let (new_messages, latest_new_seq) =
            super::agent_loop::fold_new_user_messages_since(&tail, high_water);
        log::info!(
            "execute_session drain check: session_id={} new_messages_count={} has_resumable={} latest_new_seq={:?}",
            self.activation.session_id,
            new_messages.len(),
            has_resumable,
            latest_new_seq,
        );
        // Do not advance high-water here: the next turn must still consume these
        // messages. Advancing on detection would derive empty continuation input.
        Ok(new_messages.is_empty() && !has_resumable)
    }

    /// Check whether there's a pending manual compaction request.
    fn has_pending_compaction(&self) -> bool {
        self.pending_compaction.lock().is_some()
    }

    async fn maybe_execute_pending_compaction(&self) -> Result<()> {
        if self.has_pending_compaction() {
            // SAFETY CHECK: Do not compact while there are orphan tool calls or pending HITL.
            // This prevents compaction from dropping pending tool round state or feeding
            // placeholder tool results to the model. Reload entries to get authoritative state.
            let entries = self.backend.load_events_latest_async().await?;
            let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)?;

            if should_defer_compaction(&effective, &entries)? {
                log::info!(
                    "defer manual compaction: session has pending tool round: session_id={}",
                    self.activation.session_id
                );
                // Keep pending_compaction set for next safe boundary
                return Ok(());
            }

            self.execute_manual_compaction().await?;
        }
        Ok(())
    }
}

/// Check whether compaction should be deferred due to pending tool round state.
/// Returns true if there are orphan tool calls or pending HITL approvals.
///
/// This is extracted as a pure function to allow unit testing of the deferral decision.
pub(crate) fn should_defer_compaction(
    effective_entries: &[(u64, harnx_core::session::SessionLogEntry)],
    raw_entries: &[(u64, harnx_core::session::SessionLogEntry)],
) -> Result<bool> {
    // Check for orphan tool calls (unmatched ToolCalls awaiting ToolResults)
    let has_orphans = has_orphan_tool_calls(effective_entries);

    // Check for pending HITL approvals
    let pending_hitl = super::agent_loop::derive_pending_hitl_approvals(raw_entries)?;
    let has_pending_hitl = !pending_hitl.is_empty();

    Ok(has_orphans || has_pending_hitl)
}

impl SessionTurn {
    /// Execute pending manual compaction at a safe boundary.
    ///
    /// This runs the existing `compact_session` code but with compaction_id tracking:
    /// - Emits `CompactingStarted { compaction_id: Some(id) }`
    /// - Runs compaction
    /// - Emits `CompactingCompleted/CompactingFailed` with outcome
    /// - Appends `CompactResult` via the fenced log sink
    ///
    /// SAFETY: This is only called after `record_turn_end` completes (turn boundary)
    /// or when `next_turn_input` returns None (idle session with no orphan tool calls
    /// or pending HITL approvals). Both cases guarantee no pending tool round state.
    async fn execute_manual_compaction(&self) -> Result<()> {
        let compaction_id = match self.pending_compaction.lock().take() {
            Some(id) => id,
            None => return Ok(()),
        };

        if !self.lease.is_held() {
            log::warn!(
                "manual compaction skipped: lease lost: session_id={} compaction_id={compaction_id}",
                self.activation.session_id
            );
            return Ok(());
        }

        log::info!(
            "manual compaction starting: session_id={} compaction_id={compaction_id}",
            self.activation.session_id
        );

        if let Some(outcome) = self.cached_compaction_outcome(&compaction_id) {
            self.emit_and_record_result(&compaction_id, outcome).await?;
            return Ok(());
        }

        let entries = match &self.prepare_turn_entries {
            Some(entries) => entries.clone(),
            None => self.backend.load_events_latest_async().await?,
        };
        self.hydrate_session_from_entries(&entries).await?;

        if self.session_is_compacting() {
            return self.resolve_coalesced_compaction(&compaction_id).await;
        }

        self.run_compaction_with_id(&compaction_id, entries).await?;
        log::info!(
            "manual compaction complete: session_id={} compaction_id={compaction_id}",
            self.activation.session_id
        );
        Ok(())
    }

    fn cached_compaction_outcome(
        &self,
        compaction_id: &str,
    ) -> Option<harnx_core::session::CompactOutcome> {
        self.prepare_turn_entries
            .as_deref()
            .and_then(|entries| detect_already_compacted(entries, compaction_id))
    }

    fn session_is_compacting(&self) -> bool {
        self.per_session
            .read()
            .session
            .as_ref()
            .is_some_and(|session| session.compressing())
    }

    async fn resolve_coalesced_compaction(&self, compaction_id: &str) -> Result<()> {
        log::info!(
            "manual compaction coalesced with in-progress automatic: session_id={} compaction_id={compaction_id}",
            self.activation.session_id
        );
        WorkerRuntime::wait_for_post_turn_maintenance(&self.per_session, &self.lease).await;

        let entries = self.backend.load_events_latest_async().await?;
        if let Some(outcome) = detect_already_compacted(&entries, compaction_id) {
            self.emit_and_record_result(compaction_id, outcome).await?;
            return Ok(());
        }

        log::info!(
            "manual compaction: auto compaction finished but no Compress found, running manually: session_id={}",
            self.activation.session_id
        );
        self.run_compaction_with_id(compaction_id, entries).await
    }

    async fn prepare_compaction_run(
        &self,
        compaction_id: &str,
        entries: &[(u64, harnx_core::session::SessionLogEntry)],
    ) -> Result<()> {
        self.hydrate_session_from_entries(entries).await?;
        if let Some(session) = self.per_session.write().session.as_mut() {
            session.set_compressing(true);
        }
        harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
            harnx_core::event::SessionEvent::CompactingStarted {
                compaction_id: Some(compaction_id.to_string()),
            },
        ));
        Ok(())
    }

    fn clear_compacting(&self) {
        if let Some(session) = self.per_session.write().session.as_mut() {
            session.set_compressing(false);
        }
    }

    async fn run_compaction_with_id(
        &self,
        compaction_id: &str,
        entries: Vec<(u64, harnx_core::session::SessionLogEntry)>,
    ) -> Result<()> {
        self.prepare_compaction_run(compaction_id, &entries).await?;
        let result = crate::config::Config::compact_session(&self.per_session).await;
        self.clear_compacting();

        let outcome = self.compaction_outcome_from_result(&result);
        self.emit_and_record_result(compaction_id, outcome).await
    }
}

/// Check if a Compress marker already landed after the CompactRequest.
/// Returns Some(outcome) if we should skip running compaction.
///
/// `derive_pending_compaction` only returns a request ID if there's NO matching
/// `CompactResult` for that ID in the log. So we check: did a `Compress` marker
/// appear after our request? If so, compaction may have already run (e.g., automatic
/// compaction coalesced with our request). Any user message that appears after
/// ANY `CompactResult` (or after a `TurnEnd` following `Compress`) is new user content.
pub(crate) fn detect_already_compacted(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
    compaction_id: &str,
) -> Option<harnx_core::session::CompactOutcome> {
    let mut found_request = false;
    let mut found_compress = false;
    // Track ANY CompactResult after Compress (automatic compaction may have different ID)
    let mut found_any_compact_result_after_compress = false;
    // Track TurnEnd after Compress as another boundary for new user content
    let mut found_turn_end_after_compress = false;
    let mut has_new_user_content = false;

    for (_, entry) in entries {
        match entry {
            harnx_core::session::SessionLogEntry::CompactRequest {
                compaction_id: id, ..
            } => {
                if id == compaction_id {
                    found_request = true;
                }
            }
            harnx_core::session::SessionLogEntry::Compress { .. } => {
                if found_request {
                    found_compress = true;
                }
            }
            harnx_core::session::SessionLogEntry::CompactResult { .. } => {
                // ANY CompactResult after Compress marks compaction as done
                if found_compress {
                    found_any_compact_result_after_compress = true;
                }
            }
            harnx_core::session::SessionLogEntry::TurnEnd { .. } => {
                if found_compress {
                    found_turn_end_after_compress = true;
                }
            }
            harnx_core::session::SessionLogEntry::Message { role, .. }
                if role.is_user()
                    && (found_any_compact_result_after_compress
                        || found_turn_end_after_compress) =>
            {
                // New user content after compaction completed or turn ended
                has_new_user_content = true;
            }
            _ => {}
        }
    }

    // AlreadyCompacted if we found Compress after our request and no new user content.
    // The re-logged suffix messages between Compress and CompactResult/TurnEnd don't count as new.
    if found_request && found_compress && !has_new_user_content {
        Some(harnx_core::session::CompactOutcome::Unchanged(
            harnx_core::session::UnchangedReason::AlreadyCompacted,
        ))
    } else {
        None
    }
}

impl SessionTurn {
    /// Hydrate per_session.session from log entries if not already set.
    async fn hydrate_session_from_entries(
        &self,
        entries: &[(u64, harnx_core::session::SessionLogEntry)],
    ) -> Result<()> {
        // Check if session is already hydrated
        {
            let guard = self.per_session.read();
            if guard.session.is_some() {
                return Ok(());
            }
        }

        // Get metadata store from backend - use public accessor
        let store = self
            .backend
            .metadata_store_opt()
            .context("No metadata store on backend")?;
        let metadata = store
            .get(&self.activation.session_id)
            .await?
            .context("Session metadata not found")?;

        // Build base session from metadata
        let mut session = crate::config::session::new(
            &self.per_session.read(),
            &metadata.metadata.session_id,
            None,
        )?;
        session.id = metadata.metadata.session_id.clone();
        session.session_id = Some(metadata.metadata.session_id.clone());
        session.working_dir = None;
        session.git_branch = None;
        session.git_remote = None;
        session.terminal_session_id = None;
        session.agent_variables = metadata.metadata.variables.clone();
        session.title = metadata.metadata.title.value.clone();
        session.title_last_updated_tokens = if metadata.metadata.title.manual {
            usize::MAX
        } else {
            metadata.metadata.title.last_updated_tokens
        };

        // Replay entries into session - entries are already (u64, SessionLogEntry)
        let entries_vec: Vec<(u64, harnx_core::session::SessionLogEntry)> = entries.to_vec();

        let session =
            crate::nats_session_log::load_session_from_entries_with_metadata_preserving_pending(
                &entries_vec,
                &self.activation.session_id,
                session,
            )?;

        // Set the hydrated session with a persistence sink for compaction.
        // Without session.runtime, the Compress marker and re-logged suffix messages
        // would be dropped ("no persistence sink attached").
        {
            let mut guard = self.per_session.write();
            guard.session = Some(session);
            // Attach a fenced sink for compaction's Compress + suffix re-logging.
            if let Some(session) = guard.session.as_mut() {
                let sink: Arc<dyn crate::config::session::SessionAppendSink> =
                    Arc::new(super::backend::FencedSessionLogSink::new(
                        self.backend.clone(),
                        Arc::clone(&self.lease),
                    ));
                session.runtime = Some(Arc::new(sink));
            }
        }

        Ok(())
    }

    /// Convert compaction result to outcome.
    fn compaction_outcome_from_result(
        &self,
        result: &anyhow::Result<()>,
    ) -> harnx_core::session::CompactOutcome {
        match result {
            Ok(()) => harnx_core::session::CompactOutcome::Compacted,
            Err(error) => crate::config::session_ops_compaction::classify_compaction_error(error),
        }
    }

    /// Emit the completion event and append CompactResult.
    async fn emit_and_record_result(
        &self,
        compaction_id: &str,
        outcome: harnx_core::session::CompactOutcome,
    ) -> Result<()> {
        match &outcome {
            harnx_core::session::CompactOutcome::Compacted => {
                self.event_sink.emit(harnx_core::event::AgentEvent::Session(
                    harnx_core::event::SessionEvent::CompactingCompleted {
                        compaction_id: Some(compaction_id.to_string()),
                        outcome: harnx_core::session::CompactOutcome::Compacted,
                    },
                ));
            }
            harnx_core::session::CompactOutcome::Unchanged(reason) => {
                harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
                    harnx_core::event::SessionEvent::CompactingCompleted {
                        compaction_id: Some(compaction_id.to_string()),
                        outcome: harnx_core::session::CompactOutcome::Unchanged(reason.clone()),
                    },
                ));
            }
            harnx_core::session::CompactOutcome::Failed(error) => {
                harnx_core::sink::emit_agent_event(harnx_core::event::AgentEvent::Session(
                    harnx_core::event::SessionEvent::CompactingFailed {
                        compaction_id: Some(compaction_id.to_string()),
                        error: error.clone(),
                    },
                ));
            }
        }
        self.append_compact_result(compaction_id, outcome).await
    }

    /// Append CompactResult entry to the log via the fenced sink.
    async fn append_compact_result(
        &self,
        compaction_id: &str,
        outcome: harnx_core::session::CompactOutcome,
    ) -> Result<()> {
        let sink = super::backend::FencedSessionLogSink::new(
            self.backend.clone(),
            Arc::clone(&self.lease),
        );

        let entry = harnx_core::session::SessionLogEntry::compact_result(
            compaction_id.to_string(),
            outcome,
        );

        sink.append(&entry)?;
        Ok(())
    }
}

fn advance_high_water(high_water: &mut Option<u64>, consumed: u64) {
    *high_water = Some(high_water.map_or(consumed, |previous| previous.max(consumed)));
}

#[cfg(test)]
mod compaction_deferral_tests {
    use super::*;
    use harnx_core::session::SessionLogEntry;

    /// Test that compaction is deferred when there are orphan tool calls
    /// (ToolCalls without matching ToolResults).
    #[test]
    fn defers_when_orphan_tool_calls_exist() {
        // Create entries with orphan tool calls
        let entries = vec![(
            1u64,
            SessionLogEntry::ToolCalls {
                text: "calling tools".to_string(),
                thought: None,
                calls: vec![],
                timestamp: None,
                fence_token: None,
            },
        )];

        // Apply log mutations to get effective entries
        let effective =
            harnx_core::session_reconstruct::apply_log_mutations_nats(&entries).unwrap();

        // Should defer because there's an orphan tool call
        let should_defer = should_defer_compaction(&effective, &entries).unwrap();
        assert!(
            should_defer,
            "should defer compaction when orphan tool calls exist"
        );
    }

    /// Test that compaction is NOT deferred when tool round is settled.
    #[test]
    fn does_not_defer_when_tool_round_settled() {
        // Create entries with matched ToolCalls and ToolResults
        let entries = vec![
            (
                1u64,
                SessionLogEntry::ToolCalls {
                    text: "calling tools".to_string(),
                    thought: None,
                    calls: vec![],
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                2u64,
                SessionLogEntry::ToolResults {
                    results: vec![],
                    timestamp: None,
                },
            ),
        ];

        // Apply log mutations to get effective entries
        let effective =
            harnx_core::session_reconstruct::apply_log_mutations_nats(&entries).unwrap();

        // Should NOT defer because tool round is settled
        let should_defer = should_defer_compaction(&effective, &entries).unwrap();
        assert!(
            !should_defer,
            "should NOT defer compaction when tool round is settled"
        );
    }

    /// Test that compaction is deferred when there's a pending HITL approval.
    #[test]
    fn defers_when_pending_hitl_approval() {
        // Create entries with orphan tool call and pending HITL approval
        let entries = vec![
            (
                1u64,
                SessionLogEntry::ToolCalls {
                    text: "calling tools".to_string(),
                    thought: None,
                    calls: vec![],
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                2u64,
                SessionLogEntry::HitlApprovalRequested {
                    tool_call_id: "call-1".to_string(),
                    summary: "Approve this tool".to_string(),
                    fence_token: 0,
                },
            ),
        ];

        // Apply log mutations to get effective entries
        let effective =
            harnx_core::session_reconstruct::apply_log_mutations_nats(&entries).unwrap();

        // Should defer because there's a pending HITL approval
        let should_defer = should_defer_compaction(&effective, &entries).unwrap();
        assert!(
            should_defer,
            "should defer compaction when pending HITL approval exists"
        );
    }

    /// Test that compaction is NOT deferred when HITL approval was decided.
    #[test]
    fn does_not_defer_when_hitl_decision_made() {
        // Create entries with tool call, HITL request, and decision
        let entries = vec![
            (
                1u64,
                SessionLogEntry::ToolCalls {
                    text: "calling tools".to_string(),
                    thought: None,
                    calls: vec![],
                    timestamp: None,
                    fence_token: None,
                },
            ),
            (
                2u64,
                SessionLogEntry::HitlApprovalRequested {
                    tool_call_id: "call-1".to_string(),
                    summary: "Approve this tool".to_string(),
                    fence_token: 0,
                },
            ),
            (
                3u64,
                SessionLogEntry::HitlApprovalDecision {
                    tool_call_id: "call-1".to_string(),
                    approved: true,
                    note: None,
                    fence_token: 0,
                },
            ),
            (
                4u64,
                SessionLogEntry::ToolResults {
                    results: vec![],
                    timestamp: None,
                },
            ),
        ];

        // Apply log mutations to get effective entries
        let effective =
            harnx_core::session_reconstruct::apply_log_mutations_nats(&entries).unwrap();

        // Should NOT defer because HITL decision was made and tool round settled
        let should_defer = should_defer_compaction(&effective, &entries).unwrap();
        assert!(
            !should_defer,
            "should NOT defer compaction when HITL decision made and tool round settled"
        );
    }
}
