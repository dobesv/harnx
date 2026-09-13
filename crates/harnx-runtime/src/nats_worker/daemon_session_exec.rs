//! Running a claimed session's turn loop to completion: the drain decision
//! between turns, recording a failed turn durably, and the lease-loss watch
//! that aborts promptly on failover.

use super::agent_loop::{
    build_mid_turn_injection_callback, run_agent_loop_with_nats_outcome, NatsAgentLoopOutcome,
    RunAgentLoopArgs,
};
use super::backend::NatsSessionLogBackend;
use super::daemon::{should_append_control_log_entry, SessionActivate};
use super::daemon_runtime::WorkerRuntime;
use super::daemon_turn_input::TurnInputCtx;
use crate::nats_lease::NatsSessionLease;
use crate::OnToolRoundFn;
use anyhow::{Context, Result};
use harnx_core::api_types::CompletionTokenUsage;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;

impl WorkerRuntime {
    pub(super) async fn execute_session(
        &self,
        activation: SessionActivate,
        lease: Arc<NatsSessionLease>,
        abort_signal: crate::utils::AbortSignal,
        control_task: JoinHandle<()>,
        mut hitl_decision_rx: tokio::sync::mpsc::UnboundedReceiver<
            super::control::AppliedHitlDecision,
        >,
        execution: super::execution_control::WorkerExecution,
    ) -> Result<bool> {
        let metadata = self
            .session_metadata
            .get(&activation.session_id)
            .await?
            .with_context(|| {
                format!(
                    "refusing activation without canonical session metadata: {}",
                    activation.session_id
                )
            })?
            .metadata;
        // Per-session config clone with the canonical session agent loaded
        // fresh from the worker's configuration.
        let per_session = {
            let mut base = self.config.read().clone();
            base.execution_control = Some((execution.store.clone(), execution.reference.clone()));
            base.maintenance_abort = Some(abort_signal.clone());
            Arc::new(parking_lot::RwLock::new(base))
        };
        if let Some(subject) = activation.tool_confirmation_subject.as_ref() {
            let confirm = crate::nats_tool_confirmation::nats_confirm_tool_use(
                self.client.clone(),
                subject.clone(),
                activation.session_id.clone(),
                abort_signal.clone(),
            );
            per_session.write().set_tui_confirm_tool_use(Some(confirm));
        } else {
            per_session.write().set_tui_confirm_tool_use(Some(Arc::new(
                |_call, _arguments, _reason| crate::tool::ToolUseConfirmation::Defer,
            )));
        }
        let agent_setup = super::daemon::install_session_metadata_agent(&per_session, &metadata);

        // Create event sink for live fan-out. `new` seeds `after_seq` from stream once.
        let event_sink = crate::nats_event_sink::NatsEventSink::new(
            self.client.clone(),
            self.jetstream.clone(),
            activation.session_id.clone(),
        )
        .await;
        let after_seq_observer = event_sink.after_seq_handle();
        let event_sink = Arc::new(event_sink);
        let event_sink_for_loop = Arc::clone(&event_sink);

        // Build the backend for control-plane operations and state reconstruction.
        // Share the `after_seq` high-water mark for event-sink fan-out advisories;
        // worker tail reads themselves use leader-authoritative `load_events_latest_async`.
        let backend = NatsSessionLogBackend::new(self.jetstream.clone(), &activation.session_id)
            .with_after_seq_observer(Arc::clone(&after_seq_observer))
            .with_metadata_store(Some(self.session_metadata.clone()));

        // Abort turns promptly if lease is lost.
        let watch_task =
            Self::spawn_lease_loss_watch(&lease, &abort_signal, &activation.session_id);

        let result = harnx_core::sink::with_agent_event_sink(event_sink, async {
            // A half-installed agent cannot render its prompt, so stop here and
            // let the caller record the failure rather than failing deeper in.
            agent_setup?;

            // Activation high-water cursor: max log seq of ANY user message we've fed into this
            // activation. Includes messages folded into turn inputs AND mid-round injections.
            // A continuation turn only runs if there's a user message with seq > this cursor.
            let mut activation_high_water: Option<u64> = None;

            loop {
                if !lease.is_held() || abort_signal.aborted() {
                    break Ok(());
                }

                let entries = backend.load_events_latest_async().await?;
                // Reconcile attention state from log on worker resume (repair lost bumps)
                if let Err(error) = backend.reconcile_attention_from_log(&entries).await {
                    log::warn!(
                        "failed to reconcile attention on worker resume: session_id={} error={error:#}",
                        activation.session_id
                    );
                }
                let pending_hitl = super::agent_loop::derive_pending_hitl_approvals(&entries)?;
                if !pending_hitl.is_empty() {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        hitl_decision_rx.recv(),
                    )
                    .await
                    {
                        Ok(Some(decision)) => {
                            log::debug!(
                                "received durable HITL decision: session_id={} tool_call_id={} approved={}",
                                activation.session_id,
                                decision.tool_call_id,
                                decision.approved
                            );
                        }
                        Ok(None) | Err(_) => {
                            log::debug!(
                                "ending activation while HITL approval remains pending: session_id={}",
                                activation.session_id
                            );
                            break Ok(());
                        }
                    }
                }

                let (input, seed_cursor) = self
                    .derive_turn_input(
                        TurnInputCtx {
                            activation: &activation,
                            per_session: &per_session,
                            backend: &backend,
                        },
                        &lease,
                        activation_high_water,
                    )
                    .await?;

                if seed_cursor.is_none() && input.is_empty() {
                    log::info!(
                        "execute_session has no durable turn to run: session_id={}",
                        activation.session_id,
                    );
                    if execution.store.seal(&execution.reference, &execution.owner).await? { break Ok(()); }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    continue;
                }
                if input.is_empty() {
                    anyhow::bail!("refusing to complete a durable worker turn with empty input");
                }
                if seed_cursor.is_none() {
                    anyhow::bail!("refusing to run a worker turn without a durable user cursor");
                }

                log::info!(
                    "execute_session turn: session_id={} seed_cursor={:?}",
                    activation.session_id,
                    seed_cursor,
                );

                // Initialize cursor for this turn. Use seed_cursor (from derive_turn_input)
                // which is the max seq of messages folded into the input.
                let turn_cursor = Arc::new(AtomicU64::new(seed_cursor.unwrap_or(0)));

                // Update activation high-water if seed_cursor is higher.
                if let Some(seed) = seed_cursor {
                    activation_high_water = Some(activation_high_water.map_or(seed, |h| h.max(seed)));
                }

                let injection =
                    build_mid_turn_injection_callback(backend.clone(), Arc::clone(&turn_cursor));
                let attachment_sync = ToolRoundAttachmentSync {
                    jetstream: self.jetstream.clone(),
                    config: per_session.clone(),
                    replicas: self.lease.replicas,
                    session_id: activation.session_id.clone(),
                };
                let on_tool_round = build_durable_tool_round_callback(
                    injection,
                    attachment_sync,
                );

                let loop_outcome = run_agent_loop_with_nats_outcome(
                    RunAgentLoopArgs {
                        cluster_key: &self.cluster,
                        manage_servers: self.manage_servers,
                        session_id: &activation.session_id,
                        config: per_session.clone(),
                        instance_id: self.instance_id.clone(),
                        initial_input: input,
                        abort_signal: abort_signal.clone(),
                        token_budget: activation.token_budget,
                        call_fn: self.call_fn.clone(),
                        lease: None,
                        activation_route: self.activation_route.clone(),
                        event_sink: Some(Arc::clone(&event_sink_for_loop)),
                        after_seq_observer: None,
                        session_metadata: Some(&self.session_metadata),
                        on_tool_round: Some(on_tool_round),
                        working_dir: None,
                    }
                    .with_lease(Arc::clone(&lease))
                    .with_after_seq_observer(Arc::clone(&after_seq_observer)),
                )
                .await?;

                // The shared agent loop starts compaction/title generation in
                // background tasks. Their session sink is lease-fenced, so do not
                // publish the durable turn boundary or release ownership while
                // either task can still append to this session.
                Self::wait_for_post_turn_maintenance(&per_session, &lease).await;

                // HITL ends this activation without a TurnEnd. The unmatched
                // request keeps the durable tool round pending for reactivation.
                if loop_outcome == NatsAgentLoopOutcome::AwaitingHitlApproval {
                    break Ok(());
                }

                // After turn completes, update activation high-water from turn_cursor.
                // turn_cursor covers everything this turn consumed: the seed
                // messages and any mid-round injection during multi-round
                // tool execution.
                let turn_cursor_val = turn_cursor.load(Ordering::SeqCst);
                if turn_cursor_val > 0 {
                    activation_high_water = Some(activation_high_water.map_or(turn_cursor_val, |h| h.max(turn_cursor_val)));
                }

                if !abort_signal.aborted() {
                    let usage = per_session
                        .read()
                        .session
                        .as_ref()
                        .map(|session| session.completion_usage().clone())
                        .unwrap_or_default();
                    Self::record_session_turn_end_impl(
                        &backend,
                        &lease,
                        Some(&*event_sink_for_loop),
                        turn_cursor_val,
                        usage,
                    )
                    .await?;
                    execution.cover_turn(turn_cursor_val).await?;
                }

                // The activation carries a frontend-scoped route, not a
                // turn-scoped responder. Keep it while this activation drains
                // queued continuations; a detached frontend unsubscribes and
                // requests to its dead subject fail closed.

                log::info!(
                    "execute_session turn complete: session_id={} turn_cursor={} activation_high_water={:?}",
                    activation.session_id,
                    turn_cursor_val,
                    activation_high_water,
                );

                if !lease.is_held() || abort_signal.aborted() {
                    break Ok(());
                }

                // A handoff is a terminal source-turn outcome. Its target prompt
                // is already durable and activated, so reconstructing the source
                // tool result as an in-flight round would dispatch it a second
                // time. New source prompts will publish their own activation.
                if loop_outcome == NatsAgentLoopOutcome::HandoffDispatched {
                    break Ok(());
                }

                // DRAIN DECISION: cursor-based, not barrier-based.
                // Re-run another turn ONLY if there's a user message with seq > activation_high_water.
                // This prevents re-running when we've already consumed everything.
                // Use the fresh leader-authoritative load so this re-read reflects
                // both the worker's own just-persisted completion boundary and any client
                // edit/retract committed just before the read (otherwise it would
                // re-fold already-answered or retracted messages).
                let tail = backend.load_events_latest_async().await?;

                // Check for resumable in-flight tool rounds (multi-turn tool execution).
                // Use reconstruct_state_from_nats to preserve NATS seqs for EditEntries resolution.
                let reconstructed = harnx_core::session_reconstruct::reconstruct_state_from_nats(&tail);
                let has_resumable = reconstructed.resumable_ctx.is_some();

                // Check for new user messages beyond the high-water cursor.
                let (new_messages, latest_new_seq) =
                    super::agent_loop::fold_new_user_messages_since(&tail, activation_high_water);

                log::info!(
                    "execute_session drain check: session_id={} new_messages_count={} has_resumable={} latest_new_seq={:?}",
                    activation.session_id,
                    new_messages.len(),
                    has_resumable,
                    latest_new_seq,
                );

                // Continue only if there are genuinely new user messages OR a resumable tool context.
                // A completed turn with nothing new => exactly one execution.
                //
                // Do NOT advance `activation_high_water` here. The drain only
                // DETECTS that unanswered messages exist; the continuation turn
                // CONSUMES them and advances the high-water from its own
                // `seed_cursor` at the top of the loop. Advancing here would mark
                // the messages consumed before the turn runs, so the continuation
                // turn would derive an empty input and never answer them.
                if new_messages.is_empty() && !has_resumable {
                    if execution.store.seal(&execution.reference, &execution.owner).await? { break Ok(()); }
                    tokio::task::yield_now().await;
                }
            }
        })
        .await;

        // Record the failure durably BEFORE releasing the lease: attached
        // clients treat an `Error` entry as a terminal boundary, and a client that
        // reconnects later still sees why the turn produced nothing.
        let turn_error = result.as_ref().err().filter(|_| !abort_signal.aborted());
        if let Some(error) = turn_error {
            Self::record_session_error(&backend, &lease, error).await;
        }

        if !lease.is_held() {
            log::warn!(
                "session execution ended after failover: session_id={} worker_id={} revision={}",
                activation.session_id,
                lease.worker_id(),
                lease.fence_token()
            );
        }

        Self::wait_for_post_turn_maintenance(&per_session, &lease).await;

        watch_task.abort();
        control_task.abort();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), watch_task).await;
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), control_task).await;

        if abort_signal.aborted() && lease.is_held() {
            execution.record_cancel(&backend, &lease).await?;
        }
        if result.is_err() && lease.is_held() {
            // A durable Error covers all prompts already present in the log.
            let entries = backend.load_events_latest_async().await?;
            execution
                .cover_turn(entries.last().map_or(0, |(seq, _)| *seq))
                .await?;
            execution
                .store
                .seal(&execution.reference, &execution.owner)
                .await?;
        }
        if result.is_err() {
            let operation = execution.store.status(&execution.reference).await?;
            if !operation.children.is_empty() {
                execution
                    .store
                    .cancel_operation(&execution.reference, None, false)
                    .await?;
            }
        }
        let terminal = execution.finish(&backend, &lease).await?;
        result.map(|()| terminal)
    }

    async fn wait_for_post_turn_maintenance(
        config: &crate::config::GlobalConfig,
        lease: &NatsSessionLease,
    ) {
        while lease.is_held() {
            let pending = config
                .read()
                .session
                .as_ref()
                .is_some_and(|session| session.compressing() || session.titling());
            if !pending {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Append an `Error` entry for a turn that failed.
    ///
    /// Skipped when the lease is gone: a newer worker owns the session and
    /// writing behind it would corrupt the log. That case is covered by the
    /// client's orphan watchdog instead.
    async fn record_session_error(
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
        error: &anyhow::Error,
    ) {
        if !should_append_control_log_entry(lease) {
            return;
        }
        let entry = harnx_core::session::SessionLogEntry::Error {
            message: format!("{error:#}"),
            fence_token: lease.fence_token(),
            timestamp: Some(chrono::Utc::now()),
        };
        if let Err(append_error) = backend.append_event(&entry).await {
            log::warn!(
                "failed to append Error entry: session_id={} err={append_error:#}",
                backend.session_id(),
            );
        }
    }

    /// Persist the successful full-loop boundary before checking for another
    /// queued turn. Unlike the live Turn::Ended advisory, this cannot be lost
    /// when the client is briefly disconnected or under load.
    #[cfg(test)]
    pub(crate) async fn record_session_turn_end(
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
        event_sink: Option<&crate::nats_event_sink::NatsEventSink>,
        through_seq: u64,
        usage: CompletionTokenUsage,
    ) -> Result<()> {
        Self::record_session_turn_end_impl(backend, lease, event_sink, through_seq, usage).await
    }

    /// Persist the successful full-loop boundary before checking for another
    /// queued turn. Unlike the live Turn::Ended advisory, this cannot be lost
    /// when the client is briefly disconnected or under load.
    #[allow(dead_code)]
    async fn record_session_turn_end_impl(
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
        event_sink: Option<&crate::nats_event_sink::NatsEventSink>,
        through_seq: u64,
        usage: CompletionTokenUsage,
    ) -> Result<()> {
        if through_seq == 0 {
            anyhow::bail!("refusing to persist a zero-sequence turn boundary");
        }
        if !should_append_control_log_entry(lease) {
            return Ok(());
        }
        let assigned_seq = backend
            .append_event(&harnx_core::session::SessionLogEntry::TurnEnd {
                through_seq,
                fence_token: lease.fence_token(),
                timestamp: Some(chrono::Utc::now()),
                usage: Some(usage),
            })
            .await?;
        // Bump attention seq on durable TurnEnd append
        if let Some(store) = backend.metadata_store_opt() {
            if let Err(error) = store
                .bump_attention(backend.session_id(), assigned_seq)
                .await
            {
                log::warn!(
                    "failed to bump attention after TurnEnd: session_id={} seq={} error={error:#}",
                    backend.session_id(),
                    assigned_seq
                );
            }
        }
        // Wake attached clients after durable control append
        if let Some(sink) = event_sink {
            sink.publish_session_updated();
        }
        Ok(())
    }

    /// Spawn a task that watches for lease loss and aborts on loss.
    fn spawn_lease_loss_watch(
        lease: &Arc<NatsSessionLease>,
        abort_signal: &crate::utils::AbortSignal,
        session_id: &str,
    ) -> tokio::task::JoinHandle<()> {
        let mut lost = lease.lost_watch();
        let abort_for_watch = abort_signal.clone();
        let watch_session_id = session_id.to_string();
        let watch_lease = Arc::clone(lease);
        tokio::spawn(async move {
            while lost.changed().await.is_ok() {
                if !*lost.borrow() {
                    log::warn!(
                        "failover abort: session_id={} worker_id={} revision={} reason=lease_lost",
                        watch_session_id,
                        watch_lease.worker_id(),
                        watch_lease.fence_token()
                    );
                    abort_for_watch.set_ctrlc();
                    break;
                }
            }
        })
    }

    /// Reconstruct session state using the canonical algorithm.
    ///
    /// Returns the session's turn status, effective pending message, and
    /// resumable context for driving the agent loop correctly.
    pub(super) async fn reconstruct_session_state(
        &self,
        backend: &NatsSessionLogBackend,
    ) -> harnx_core::session_reconstruct::ReconstructedState {
        match backend.load_events_latest_async().await {
            Ok(entries) => harnx_core::session_reconstruct::reconstruct_state_from_nats(&entries),
            Err(err) => {
                log::warn!(
                    "failed to load session log for reconstruction: session_id={} worker_id={} err={err}",
                    backend.session_id(),
                    self.worker_id,
                );
                harnx_core::session_reconstruct::ReconstructedState {
                    turn_status: harnx_core::session_reconstruct::TurnStatus::Idle,
                    next_turn_messages: Vec::new(),
                    resumable_ctx: None,
                }
            }
        }
    }
}

#[derive(Clone)]
struct ToolRoundAttachmentSync {
    jetstream: async_nats::jetstream::Context,
    config: crate::config::GlobalConfig,
    replicas: usize,
    session_id: String,
}

fn build_durable_tool_round_callback(
    injection: OnToolRoundFn,
    attachment_sync: ToolRoundAttachmentSync,
) -> OnToolRoundFn {
    Arc::new(move |merged_input, results| {
        let injection = Arc::clone(&injection);
        let attachment_sync = attachment_sync.clone();
        Box::pin(async move {
            injection(merged_input, results).await?;
            crate::nats_attachments::sync_session_attachments(
                &attachment_sync.jetstream,
                &attachment_sync.config,
                attachment_sync.replicas,
                &attachment_sync.session_id,
            )
            .await
        })
    })
}

#[cfg(test)]
mod attention_tests {
    use super::*;
    use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
    use crate::nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore};
    use harnx_core::require_nextest;
    use std::sync::Arc;

    /// Test that `record_session_turn_end` appends TurnEnd and bumps attention.
    /// Verifies the direct bump path (not via `reconcile_attention_from_log`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn record_session_turn_end_bumps_attention_directly() {
        require_nextest();
        let Some((url, mut child, _store_dir)) = crate::nats_worker::tests::spawn_test_nats().await
        else {
            return;
        };

        let client = async_nats::connect(&url).await.unwrap();
        let jetstream = async_nats::jetstream::new(client);
        let store = SessionMetadataStore::ensure(&jetstream, 1).await.unwrap();
        let session_id = crate::nats_worker::new_remote_session_id();

        // Create session metadata
        store
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named("metis", Default::default()),
            ))
            .await
            .unwrap();

        // Create backend with metadata store attached
        let backend = NatsSessionLogBackend::new(jetstream.clone(), &session_id)
            .with_metadata_store(Some(store.clone()));

        // Acquire a lease for the session
        let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: jetstream.clone(),
            session_id: &session_id,
            worker_id: "test-worker".to_string(),
            generation: 1,
            config: NatsLeaseConfig {
                ttl: std::time::Duration::from_secs(5),
                renew_interval: std::time::Duration::from_millis(500),
                replicas: 1,
                ..Default::default()
            },
            session_metadata: Some(store.clone()),
        })
        .await
        .unwrap()
        .expect("lease should be acquired");
        let lease = Arc::new(lease);

        // Append a user message first so we have valid through_seq
        backend
            .append_event(&harnx_core::session::SessionLogEntry::Message {
                id: None,
                role: harnx_core::message::MessageRole::User,
                content: harnx_core::message::MessageContent::Text("test".to_string()),
                timestamp: None,
                fence_token: None,
            })
            .await
            .unwrap();

        // Call record_session_turn_end (test helper exposing the impl)
        WorkerRuntime::record_session_turn_end(
            &backend,
            &lease,
            None,
            1, // through_seq
            CompletionTokenUsage::default(),
        )
        .await
        .unwrap();

        // Verify session is now unread with correct attention seq
        let state = store.get_read_state(&session_id).await.unwrap();
        assert!(
            state.is_unread(),
            "session should be unread after record_session_turn_end"
        );
        assert_eq!(
            state.last_attention_seq, 2,
            "last_attention_seq should be set to TurnEnd seq"
        );

        lease.release().await.unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Test that record_session_turn_end skips when through_seq is zero.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn record_session_turn_end_rejects_zero_through_seq() {
        require_nextest();
        let Some((url, mut child, _store_dir)) = crate::nats_worker::tests::spawn_test_nats().await
        else {
            return;
        };

        let client = async_nats::connect(&url).await.unwrap();
        let jetstream = async_nats::jetstream::new(client);
        let store = SessionMetadataStore::ensure(&jetstream, 1).await.unwrap();
        let session_id = crate::nats_worker::new_remote_session_id();

        store
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named("metis", Default::default()),
            ))
            .await
            .unwrap();

        let backend = NatsSessionLogBackend::new(jetstream.clone(), &session_id)
            .with_metadata_store(Some(store.clone()));

        let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: jetstream.clone(),
            session_id: &session_id,
            worker_id: "test-worker".to_string(),
            generation: 1,
            config: NatsLeaseConfig {
                ttl: std::time::Duration::from_secs(5),
                renew_interval: std::time::Duration::from_millis(500),
                replicas: 1,
                ..Default::default()
            },
            session_metadata: Some(store.clone()),
        })
        .await
        .unwrap()
        .expect("lease should be acquired");
        let lease = Arc::new(lease);

        // through_seq = 0 should bail
        let result = WorkerRuntime::record_session_turn_end(
            &backend,
            &lease,
            None,
            0,
            CompletionTokenUsage::default(),
        )
        .await;
        assert!(result.is_err(), "zero through_seq should fail");

        // Session should NOT be unread (no bump happened)
        let state = store.get_read_state(&session_id).await.unwrap();
        assert!(
            !state.is_unread(),
            "session should NOT be unread after failed TurnEnd"
        );

        lease.release().await.unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}
