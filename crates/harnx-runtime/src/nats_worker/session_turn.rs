//! Turn-local state. No session dispatcher, active-map or lease-release authority.
use super::agent_loop::{
    build_mid_turn_injection_callback, run_agent_loop_with_nats_outcome, NatsAgentLoopOutcome,
    RunAgentLoopArgs,
};
use super::backend::NatsSessionLogBackend;
use super::control::AppliedHitlDecision;
use super::daemon::SessionActivate;
use super::daemon_runtime::WorkerRuntime;
use super::daemon_session_exec::{build_durable_tool_round_callback, ToolRoundAttachmentSync};
use super::daemon_turn_input::TurnInputCtx;
use crate::config::{GlobalConfig, Input};
use crate::nats_event_sink::NatsEventSink;
use crate::nats_lease::NatsSessionLease;
use anyhow::Result;
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
        while let Some((input, seed_cursor)) = self.next_turn_input(activation_high_water).await? {
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
        let entries = self.backend.load_events_latest_async().await?;
        // Repair attention bumps lost after their durable transcript append.
        self.backend.reconcile_attention_from_log(&entries).await?;
        let pending_hitl = super::agent_loop::derive_pending_hitl_approvals(&entries)?;
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
}

fn advance_high_water(high_water: &mut Option<u64>, consumed: u64) {
    *high_water = Some(high_water.map_or(consumed, |previous| previous.max(consumed)));
}
