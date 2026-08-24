//! Running a claimed session's turn loop to completion: the drain decision
//! between turns, recording a failed turn durably, and the lease-loss watch
//! that aborts promptly on failover.

use super::agent_loop::{
    build_mid_turn_injection_callback, run_agent_loop_with_nats_inner, RunAgentLoopArgs,
};
use super::backend::NatsSessionLogBackend;
use super::daemon::{should_append_control_log_entry, SessionActivate};
use super::daemon_runtime::WorkerRuntime;
use super::daemon_turn_input::TurnInputCtx;
use crate::nats_lease::NatsSessionLease;
use anyhow::Result;
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
    ) -> Result<()> {
        // Per-session config clone with the requested agent (loaded from the
        // worker's OWN config) and the session selected.
        let per_session = {
            let base = self.config.read().clone();
            Arc::new(parking_lot::RwLock::new(base))
        };
        let agent_setup = super::daemon::install_activation_agent(&per_session, &activation.agent);

        // Create event sink for live fan-out. `new` seeds `after_seq` from stream once.
        let event_sink = crate::nats_event_sink::NatsEventSink::new(
            self.client.clone(),
            self.jetstream.clone(),
            activation.session_id.clone(),
        )
        .await;
        let after_seq_observer = event_sink.after_seq_handle();
        let event_sink = Arc::new(event_sink);

        // Build the backend for control-plane operations and state reconstruction.
        // Share the `after_seq` high-water mark for event-sink fan-out advisories;
        // worker tail reads themselves use leader-authoritative `load_events_latest_async`.
        let backend = NatsSessionLogBackend::new(self.jetstream.clone(), &activation.session_id)
            .with_after_seq_observer(Arc::clone(&after_seq_observer));

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
                if !lease.is_held() {
                    break Ok(());
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
                    .await;

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

                let on_tool_round =
                    build_mid_turn_injection_callback(backend.clone(), Arc::clone(&turn_cursor));

                run_agent_loop_with_nats_inner(
                    RunAgentLoopArgs {
                        cluster_key: &self.cluster,
                        manage_servers: self.manage_servers,
                        session_id: &activation.session_id,
                        config: per_session.clone(),
                        instance_id: self.instance_id.clone(),
                        initial_input: input,
                        abort_signal: abort_signal.clone(),
                        call_fn: self.call_fn.clone(),
                        lease: None,
                        lease_config: self.lease.clone(),
                        after_seq_observer: None,
                        header_insert_observer: None,
                        session_index: self.session_index.as_ref(),
                        on_tool_round: Some(on_tool_round),
                        working_dir: None,
                    }
                    .with_lease(Arc::clone(&lease))
                    .with_after_seq_observer(Arc::clone(&after_seq_observer))
                    // The S2 header-insert migration re-maps this turn's
                    // leading-user block onto the migration seq, which is ABOVE
                    // the seed cursor. Point the observer at `turn_cursor` so
                    // BOTH consumers skip the re-mapped block: the mid-round
                    // injection callback (or round 2 re-injects the prompt this
                    // turn is already answering) and, via `turn_cursor` below,
                    // the end-of-turn drain (or it re-runs the answered turn).
                    .with_header_insert_observer(Arc::clone(&turn_cursor)),
                )
                .await?;

                // The shared agent loop starts compaction/title generation in
                // background tasks. Their session sink is lease-fenced, so do not
                // publish the durable turn boundary or release ownership while
                // either task can still append to this session.
                Self::wait_for_post_turn_maintenance(&per_session, &lease).await;

                // After turn completes, update activation high-water from turn_cursor.
                // turn_cursor covers everything this turn consumed: the seed
                // messages, the header-insert re-map, and any mid-round
                // injection during multi-round tool execution.
                let turn_cursor_val = turn_cursor.load(Ordering::SeqCst);
                if turn_cursor_val > 0 {
                    activation_high_water = Some(activation_high_water.map_or(turn_cursor_val, |h| h.max(turn_cursor_val)));
                }

                Self::record_session_turn_end(&backend, &lease, turn_cursor_val).await?;

                log::info!(
                    "execute_session turn complete: session_id={} turn_cursor={} activation_high_water={:?}",
                    activation.session_id,
                    turn_cursor_val,
                    activation_high_water,
                );

                if !lease.is_held() {
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
                    break Ok(());
                }
            }
        })
        .await;

        // Record the failure durably BEFORE releasing the lease: attached
        // clients treat an `Error` entry as a terminal boundary, and a client that
        // reconnects later still sees why the turn produced nothing.
        if let Err(error) = &result {
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

        watch_task.abort();
        control_task.abort();
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), watch_task).await;
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), control_task).await;

        let _ = lease.release().await;
        result
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
    async fn record_session_turn_end(
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
        through_seq: u64,
    ) -> Result<()> {
        if !should_append_control_log_entry(lease) {
            return Ok(());
        }
        backend
            .append_event(&harnx_core::session::SessionLogEntry::TurnEnd {
                through_seq,
                fence_token: lease.fence_token(),
                timestamp: Some(chrono::Utc::now()),
            })
            .await?;
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
