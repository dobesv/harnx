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
            base.execution_control = Some((execution.store.clone(), execution.reference.clone()));
            base.generation_fence = execution.fence.clone();
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
        .await
        .with_execution(execution.fence.clone());
        let after_seq_observer = event_sink.after_seq_handle();
        let event_sink = Arc::new(event_sink);

        // Build the backend for control-plane operations and state reconstruction.
        // Share the `after_seq` high-water mark for event-sink fan-out advisories;
        // worker tail reads themselves use leader-authoritative `load_events_latest_async`.
        let backend = NatsSessionLogBackend::new(self.jetstream.clone(), &activation.session_id)
            .with_after_seq_observer(Arc::clone(&after_seq_observer))
            .with_metadata_store(Some(self.session_metadata.clone()))
            .with_execution(execution.fence.clone());

        // Abort turns promptly if lease is lost.
        let watch_task =
            Self::spawn_lease_loss_watch(&lease, &abort_signal, &activation.session_id);

        let inputs = super::session_turn::SessionTurn {
            worker: super::session_turn::TurnWorker::from(self),
            activation: activation.clone(),
            lease: lease.clone(),
            abort_signal: abort_signal.clone(),
            hitl_decision_rx,
            execution: execution.clone(),
            per_session: per_session.clone(),
            backend: backend.clone(),
            event_sink,
            after_seq_observer,
            agent_setup,
        };
        // A spawned turn owns its poll/drop work. Aborting it requests a drop;
        // waiting for that drop belongs to cleanup, never the lease supervisor.
        let mut turn = tokio::spawn(Box::pin(inputs.run()));
        let (result, cleanup_turn) = tokio::select! {
            biased;
            _ = harnx_core::abort::wait_abort_signal(&abort_signal) => {
                turn.abort();
                (Ok(()), Some(turn))
            }
            result = &mut turn => (result.unwrap_or_else(|error| Err(error.into())), None),
        };

        // Record the failure durably BEFORE releasing the lease: attached
        // clients treat an `Error` entry as a terminal boundary, and a client that
        // reconnects later still sees why the turn produced nothing.
        if result
            .as_ref()
            .is_err_and(|error| error.is::<harnx_execution_control::Interrupted>())
        {
            abort_signal.set_ctrlc();
        }
        let turn_error = result.as_ref().err().filter(|_| !abort_signal.aborted());
        let error_sequence = match turn_error {
            Some(error) => Self::record_session_error(&backend, &lease, error).await,
            None => None,
        };

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
        harnx_execution_control::CleanupTasks::process().spawn(async move {
            let _ = watch_task.await;
            let _ = control_task.await;
        });

        if let Some(sequence) = error_sequence.filter(|_| lease.is_held()) {
            // Coverage belongs to this Error commit, not a later tail that may
            // already contain a retry prompted by the terminal notification.
            execution.cover_turn(sequence).await?;
            execution
                .store
                .seal(&execution.reference, &execution.owner)
                .await?;
        }
        if result.is_err() {
            let operation = execution.store.status(&execution.reference).await?;
            // The durable Error already completed this prompt. Stop leftover
            // children, not the root: a root stop would misclassify the failure
            // as user interruption for acceptance-driven followers.
            for child in &operation.children {
                execution.store.cancel_operation(child, None, false).await?;
            }
        }
        let terminal = execution
            .finish(&backend, &lease, per_session, cleanup_turn)
            .await?;
        if abort_signal.aborted() {
            Ok(terminal)
        } else {
            result.map(|()| terminal)
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
    /// client's orphan watchdog instead.
    async fn record_session_error(
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
        error: &anyhow::Error,
    ) -> Option<u64> {
        if !should_append_control_log_entry(lease) {
            return None;
        }
        let entry = harnx_core::session::SessionLogEntry::Error {
            message: format!("{error:#}"),
            fence_token: lease.fence_token(),
            timestamp: Some(chrono::Utc::now()),
        };
        match backend.append_event(&entry).await {
            Ok(sequence) => Some(sequence),
            Err(append_error) => {
                log::warn!(
                    "failed to append Error entry: session_id={} err={append_error:#}",
                    backend.session_id()
                );
                None
            }
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
        let backend = NatsSessionLogBackend::new(jetstream.clone(), &storage_key)
            .with_metadata_store(Some(store.clone()));

        // Acquire a lease for the session
        let (lease, fence) =
            crate::nats_worker::backend::test_session_authority(&jetstream, &storage_key, &store)
                .await;
        let backend = backend.with_execution(Some(fence));

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

        let backend = NatsSessionLogBackend::new(jetstream.clone(), &storage_key)
            .with_metadata_store(Some(store.clone()));

        let (lease, fence) =
            crate::nats_worker::backend::test_session_authority(&jetstream, &storage_key, &store)
                .await;
        let backend = backend.with_execution(Some(fence));

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

#[cfg(test)]
#[path = "daemon_session_exec/coverage_tests.rs"]
mod coverage_tests;
