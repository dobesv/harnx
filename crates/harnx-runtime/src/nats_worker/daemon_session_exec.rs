//! Running a claimed session's turn loop to completion: the drain decision
//! between turns, recording a failed turn durably, and the lease-loss watch
//! that aborts promptly on failover.

use super::backend::NatsSessionLogBackend;
use super::daemon::{should_append_control_log_entry, SessionActivate};
use super::daemon_runtime::WorkerRuntime;
use crate::nats_lease::NatsSessionLease;
use crate::OnToolRoundFn;
use anyhow::{Context, Result};
use harnx_core::api_types::CompletionTokenUsage;
use std::sync::Arc;
use tokio::task::JoinHandle;

impl WorkerRuntime {
    pub(super) async fn execute_session(
        &self,
        activation: SessionActivate,
        lease: Arc<NatsSessionLease>,
        abort_signal: crate::utils::AbortSignal,
        control_task: JoinHandle<()>,
        hitl_decision_rx: tokio::sync::mpsc::UnboundedReceiver<super::control::AppliedHitlDecision>,
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
            base.maintenance_abort = Some(abort_signal.clone());
            Arc::new(parking_lot::RwLock::new(base))
        };
        if let Some(subject) = activation.tool_confirmation_subject.as_ref() {
            let confirm = crate::nats_tool_confirmation::nats_confirm_tool_use(
                self.client.clone(),
                subject.clone(),
                metadata.session_id.clone(),
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

        // Build the backend for control-plane operations and state reconstruction.
        // Share the `after_seq` high-water mark for event-sink fan-out advisories;
        // worker tail reads themselves use leader-authoritative `load_events_latest_async`.
        let backend = NatsSessionLogBackend::new(
            self.jetstream.clone(),
            &activation.session_id,
            self.lease.replicas,
        )
        .with_after_seq_observer(Arc::clone(&after_seq_observer))
        .with_metadata_store(Some(self.session_metadata.clone()));

        // Follow the session's own stream concurrently with the turn: a
        // foreign `Cancel` is the only thing that can interrupt it, and a
        // foreign `Message` only flags that input is waiting. Holding this
        // instance's `NatsInFlightCalls` handle for the whole execution keeps
        // the shared map alive, so tool/hook registrations made during the
        // turn land where the watcher's snapshot can find them.
        let pending_input = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let interrupted = Arc::new(parking_lot::Mutex::new(None));
        let in_flight =
            crate::nats_tool_provider::NatsInFlightCalls::for_instance(&self.instance_id);
        let watcher_start_after = backend
            .load_events_latest_async()
            .await?
            .last()
            .map_or(0, |(seq, _)| *seq);
        let session_watcher = super::session_watcher::spawn_session_watcher(
            super::session_watcher::SessionWatcherCtx {
                jetstream: self.jetstream.clone(),
                client: self.client.clone(),
                session_id: activation.session_id.clone(),
                start_after: watcher_start_after,
                abort_signal: abort_signal.clone(),
                in_flight: in_flight.clone(),
                own_appends: Arc::clone(&after_seq_observer),
                pending_input: Arc::clone(&pending_input),
                interrupted: Arc::clone(&interrupted),
            },
        );

        // Winding the interrupted turn up happens in `finish`, once the turn
        // task is gone and while the lease is still held. Hand the execution
        // what that needs: the watcher's notice of the interrupt, and the
        // handles to reach the tool calls it cut off.
        let execution = execution.with_wind_up(super::execution_control::WindUpContext {
            client: self.client.clone(),
            jetstream: self.jetstream.clone(),
            replicas: self.lease.replicas,
            in_flight,
            event_sink: Arc::clone(&event_sink),
            interrupted,
        });

        // Abort turns promptly if lease is lost.
        let watch_task =
            Self::spawn_lease_loss_watch(&lease, &abort_signal, &activation.session_id);

        let inputs = super::session_turn::SessionTurn {
            worker: super::session_turn::TurnWorker::from(self),
            activation: activation.clone(),
            lease: lease.clone(),
            abort_signal: abort_signal.clone(),
            hitl_decision_rx,
            per_session: per_session.clone(),
            backend: backend.clone(),
            event_sink,
            after_seq_observer,
            pending_input,
            agent_setup,
        };
        // A spawned turn owns its poll/drop work. Aborting it requests a drop;
        // waiting for that drop belongs to cleanup, never the lease supervisor.
        let mut turn = tokio::spawn(Box::pin(inputs.run()));
        let (result, cleanup_turn) = tokio::select! {
            biased;
            _ = harnx_core::abort::wait_abort_signal(&abort_signal) => {
                turn.abort();
                (Ok(true), Some(turn))
            }
            result = &mut turn => (result.unwrap_or_else(|error| Err(error.into())), None),
        };
        // A turn that failed left a durable `Error`, which terminates it just
        // as a completion would: there is nothing for a redelivery to retry.
        let settled = *result.as_ref().unwrap_or(&true);

        // A turn whose append lost to a `Cancel` was interrupted, not broken.
        // It can notice the interruption before the session watcher does, so
        // the abort signal is fired from here rather than assumed to be set
        // already — an `Error` written after that `Cancel` would terminate the
        // turn a second time and leave `finish` nothing to wind up.
        if let Some(interrupted) = result.as_ref().err().and_then(cancel_that_interrupted) {
            log::info!(
                "turn append lost to a Cancel; aborting instead of recording an error: \
                 session_id={} cancel_seq={}",
                activation.session_id,
                interrupted.cancel_seq
            );
            abort_signal.set_ctrlc();
        }

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

        if !abort_signal.aborted() {
            Self::wait_for_post_turn_maintenance(&per_session, &lease).await;
        }

        watch_task.abort();
        control_task.abort();
        session_watcher.abort();
        tokio::spawn(async move {
            let _ = watch_task.await;
            let _ = control_task.await;
            let _ = session_watcher.await;
        });

        let terminal = execution
            .finish(
                &backend,
                &lease,
                super::execution_control::FinishedTurn {
                    task: cleanup_turn,
                    settled,
                    config: per_session,
                },
            )
            .await?;
        if abort_signal.aborted() {
            Ok(terminal)
        } else {
            result.map(|_| terminal)
        }
    }

    pub(super) async fn wait_for_post_turn_maintenance(
        config: &crate::config::GlobalConfig,
        lease: &NatsSessionLease,
    ) {
        while lease.is_held() {
            if config
                .read()
                .maintenance_abort
                .as_ref()
                .is_some_and(|abort| abort.aborted())
            {
                break;
            }
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
    /// client's orphan watchdog instead. Skipped too for an interruption,
    /// which the `Cancel` already terminated: a second terminator would end
    /// the turn before its wind-up could answer the calls it cut off.
    async fn record_session_error(
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
        error: &anyhow::Error,
    ) {
        if interrupted_by_cancel(error) {
            log::info!(
                "turn ended in an interruption, not a failure: session_id={} error={error:#}",
                backend.session_id()
            );
            return;
        }
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
                backend.session_id()
            );
        }
    }

    /// Persist the successful full-loop boundary before checking for another
    /// queued turn. Unlike the live Turn::Ended advisory, this cannot be lost
    /// when the client is briefly disconnected or under load.
    pub(super) async fn record_session_turn_end(
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
}

/// The `Cancel` that ended this turn while one of its appends was in flight,
/// when that is what the turn error is. The interruption can be wrapped in
/// context by the time it surfaces, so the whole cause chain is searched.
fn cancel_that_interrupted(error: &anyhow::Error) -> Option<&super::backend::TurnInterrupted> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<super::backend::TurnInterrupted>())
}

/// Whether this turn error is a `Cancel` that ended the turn while one of its
/// appends was in flight.
fn interrupted_by_cancel(error: &anyhow::Error) -> bool {
    cancel_that_interrupted(error).is_some()
}

#[derive(Clone)]
pub(super) struct ToolRoundAttachmentSync {
    pub(super) jetstream: async_nats::jetstream::Context,
    pub(super) config: crate::config::GlobalConfig,
    pub(super) replicas: usize,
    pub(super) session_id: String,
}

pub(super) fn build_durable_tool_round_callback(
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
    use crate::nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore};
    use harnx_core::require_nextest;

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
        let storage_key = harnx_core::session_identity::session_key(Some("metis"), &session_id);

        // Create session metadata
        store
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named("metis", Default::default()),
            ))
            .await
            .unwrap();

        // Create backend with metadata store attached
        let backend = NatsSessionLogBackend::new(jetstream.clone(), &storage_key, 1)
            .with_metadata_store(Some(store.clone()));

        // Acquire a lease for the session
        let lease =
            crate::nats_worker::backend::test_session_authority(&jetstream, &storage_key, &store)
                .await;

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
        let state = store.get_read_state(&storage_key).await.unwrap();
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

    /// An interrupted turn is already terminated by its `Cancel`. Writing an
    /// `Error` behind that `Cancel` would terminate it a second time, and
    /// reconstruction would then see an idle session with the interrupted
    /// round's tool calls still unanswered — nothing left to wind up.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_interrupted_turn_records_no_error_entry() {
        require_nextest();
        let Some((url, mut child, _store_dir)) = crate::nats_worker::tests::spawn_test_nats().await
        else {
            return;
        };
        let client = async_nats::connect(&url).await.unwrap();
        let jetstream = async_nats::jetstream::new(client);
        let store = SessionMetadataStore::ensure(&jetstream, 1).await.unwrap();
        let session_id = crate::nats_worker::new_remote_session_id();
        let storage_key = harnx_core::session_identity::session_key(Some("metis"), &session_id);
        store
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named("metis", Default::default()),
            ))
            .await
            .unwrap();
        let backend = NatsSessionLogBackend::new(jetstream.clone(), &storage_key, 1)
            .with_metadata_store(Some(store.clone()));
        let lease =
            crate::nats_worker::backend::test_session_authority(&jetstream, &storage_key, &store)
                .await;

        // The interruption surfaces wrapped in context, as it does when it
        // travels up through the tool round that lost the append.
        let interrupted =
            anyhow::Error::new(crate::nats_worker::backend::TurnInterrupted { cancel_seq: 7 })
                .context("failed to durably persist tool results");
        WorkerRuntime::record_session_error(&backend, &lease, &interrupted).await;

        // Read through the log itself: the structural guard over this file's
        // family counts the worker's leader-authoritative decision points.
        let log = crate::nats_session_log::NatsSessionLog::new(jetstream.clone(), &storage_key);
        assert!(
            !log.load_events_async()
                .await
                .unwrap()
                .iter()
                .any(|(_, entry)| matches!(
                    entry,
                    harnx_core::session::SessionLogEntry::Error { .. }
                )),
            "an interruption is not a turn failure"
        );

        // An ordinary failure still lands, so the guard is about the cause and
        // not about silencing errors.
        WorkerRuntime::record_session_error(&backend, &lease, &anyhow::anyhow!("model exploded"))
            .await;
        assert!(log.load_events_async().await.unwrap().iter().any(|(_, entry)| matches!(
            entry,
            harnx_core::session::SessionLogEntry::Error { message, .. } if message.contains("model exploded")
        )));

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
        let storage_key = harnx_core::session_identity::session_key(Some("metis"), &session_id);

        store
            .create(&SessionMetadata::new(
                &session_id,
                SessionInitializer::named("metis", Default::default()),
            ))
            .await
            .unwrap();

        let backend = NatsSessionLogBackend::new(jetstream.clone(), &storage_key, 1)
            .with_metadata_store(Some(store.clone()));

        let lease =
            crate::nats_worker::backend::test_session_authority(&jetstream, &storage_key, &store)
                .await;

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
        let state = store.get_read_state(&storage_key).await.unwrap();
        assert!(
            !state.is_unread(),
            "session should NOT be unread after failed TurnEnd"
        );

        lease.release().await.unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}
