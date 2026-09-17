//! What a worker keeps for the one turn it is running.
//!
//! The session log decides whether that turn was interrupted; this type holds
//! only the identity the worker writes under and the handles winding up an
//! interrupted turn needs.

use super::session_watcher::InterruptNotice;
use super::wind_up::{wind_up_interrupted_turn, WindUpInputs, WindUpOutcome};
use super::{NatsSessionLogBackend, SessionActivate};
use crate::nats_lease::NatsSessionLease;
use anyhow::Result;
use harnx_core::event::{AgentEvent, TurnEvent};
use harnx_core::session::SessionLogEntry;
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct WorkerExecution {
    /// Who this worker is and what it claimed. Diagnostic only: every append
    /// takes its revision from the live lease, which can have been renewed
    /// since, and is the authority.
    session_id: String,
    worker_id: String,
    fence_token: u64,
    /// Present for an execution claimed by a session activation. Absent for
    /// fixtures, which drive no tool traffic and so have nothing to wind up.
    wind_up: Option<Arc<WindUpContext>>,
}

/// What winding up an interrupted turn needs that the execution itself does
/// not carry: the session's NATS handles, its live tool calls, and the sink
/// that tells attached clients the turn ended in an interruption.
pub(super) struct WindUpContext {
    pub client: async_nats::Client,
    pub jetstream: async_nats::jetstream::Context,
    pub replicas: usize,
    pub in_flight: crate::nats_tool_provider::NatsInFlightCalls,
    pub event_sink: Arc<crate::nats_event_sink::NatsEventSink>,
    /// Recorded by the session watcher when a foreign `Cancel` interrupted
    /// the turn. A diagnostic hint only: the log decides who terminated it.
    pub interrupted: Arc<parking_lot::Mutex<Option<InterruptNotice>>>,
}

impl WorkerExecution {
    /// The lease the caller already holds IS the claim. Nothing else is
    /// reserved for a turn any more.
    pub fn claim(activation: &SessionActivate, lease: &NatsSessionLease) -> Self {
        Self {
            session_id: activation.session_id.clone(),
            worker_id: lease.worker_id().to_string(),
            fence_token: lease.fence_token(),
            wind_up: None,
        }
    }

    /// Attach what the wind-up of an interrupted turn needs. Called once, by
    /// the session activation that owns the turn about to run.
    pub fn with_wind_up(mut self, context: WindUpContext) -> Self {
        self.wind_up = Some(Arc::new(context));
        self
    }

    /// Make sure the log carries a `Cancel` terminator for the turn this
    /// worker is abandoning. A terminator that beat us there already says
    /// everything a second one would.
    async fn record_cancel(
        &self,
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
    ) -> Result<()> {
        anyhow::ensure!(
            lease.is_held(),
            "cannot record cancellation after lease loss"
        );
        let entries = backend.load_events_latest_async().await?;
        if harnx_core::session_reconstruct::current_turn_is_cancelled(&entries) {
            // A frontend, a parent session or a previous worker already
            // terminated the turn in progress. A second Cancel would say
            // nothing the first did not, and the wind-up answers either one.
            return Ok(());
        }
        append_worker_cancel(backend, lease, &entries)
            .await
            .map(drop)
    }

    /// Close out a turn that ended in cancellation, while the lease is still
    /// held: make sure the log carries a `Cancel` terminator for it, then
    /// answer the tool calls that terminator interrupted.
    async fn close_out_cancelled_turn(
        &self,
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
    ) -> Result<WindUpOutcome> {
        let notice = self
            .wind_up
            .as_ref()
            .and_then(|context| context.interrupted.lock().take());
        self.record_cancel(backend, lease).await?;
        let Some(context) = self.wind_up.as_ref() else {
            return Ok(WindUpOutcome::Nothing);
        };
        let outcome = wind_up_interrupted_turn(WindUpInputs {
            backend,
            lease,
            client: &context.client,
            jetstream: &context.jetstream,
            replicas: context.replicas,
            in_flight: &context.in_flight,
            event_sink: Some(context.event_sink.as_ref()),
        })
        .await?;
        log::info!(
            "cancelled turn closed out: session_id={} worker_id={} claimed_revision={} foreign_cancel_seq={:?} foreign_cancellation_id={:?} outcome={outcome:?}",
            self.session_id,
            self.worker_id,
            self.fence_token,
            notice.as_ref().map(|notice| notice.cancel_seq),
            notice
                .as_ref()
                .and_then(|notice| notice.cancellation_id.as_deref()),
        );
        Ok(outcome)
    }

    /// Wind the turn up if it was interrupted, release the lease, and report
    /// whether this activation may be acknowledged.
    ///
    /// `settled` is the turn loop's own answer to "is there anything left this
    /// activation was published for": a pending HITL approval says no. Input
    /// queued behind the turn says no too, whatever the loop thought, because
    /// only a redelivered activation can run it.
    pub async fn finish(
        &self,
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
        turn: FinishedTurn,
    ) -> Result<bool> {
        let FinishedTurn {
            task,
            settled,
            config: turn_config,
        } = turn;
        let interrupted = turn_config
            .read()
            .maintenance_abort
            .as_ref()
            .is_some_and(|signal| signal.aborted());
        let wound_up = if interrupted && lease.is_held() {
            self.close_out_cancelled_turn(backend, lease).await
        } else {
            Ok(WindUpOutcome::Nothing)
        };
        // Wind-up emits the advisory itself when it appends results. A turn
        // with no orphaned calls still ended in an interruption, and attached
        // frontends need to hear that exactly once either way.
        if interrupted && matches!(wound_up, Ok(WindUpOutcome::Nothing)) {
            self.announce_interruption(backend).await;
        }
        // The wind-up's own failure is the one worth reporting: a release that
        // then fails would otherwise hide what the log is still owed. Returning
        // here skips the explicit release below, not the release itself:
        // `Drop for NatsSessionLease` still deletes the lease key on its way out.
        let wound_up = wound_up?;
        log::debug!(
            "releasing the session lease: session_id={} wound_up={wound_up:?}",
            self.session_id
        );
        lease.release().await?;
        if interrupted {
            drain_abandoned_turn(task, turn_config);
        }
        Ok(settled && !self.has_queued_input(backend).await?)
    }

    /// Tell attached frontends the turn ended in an interruption, using the
    /// cancellation id the log records for it.
    ///
    /// The abort signal is not enough to say the turn WAS interrupted: losing
    /// the lease fires the same signal, and then a replacement worker picks
    /// the turn up and carries on. Frontends treat `Interrupted` like `Ended`,
    /// so announcing one for a failover stops the user's spinner on a turn
    /// that is still running. Only a `Cancel` terminates a turn, so the log is
    /// what decides; a log this worker cannot read announces nothing, and the
    /// frontend's own interrupt watch settles the turn from the `Cancel`
    /// itself either way.
    async fn announce_interruption(&self, backend: &NatsSessionLogBackend) {
        let Some(context) = self.wind_up.as_ref() else {
            return;
        };
        let entries = match backend.load_events_latest_async().await {
            Ok(entries) => entries,
            Err(error) => {
                log::debug!(
                    "interruption advisory skipped, session log unreadable: session_id={} error={error:#}",
                    backend.session_id()
                );
                return;
            }
        };
        if !harnx_core::session_reconstruct::current_turn_is_cancelled(&entries) {
            log::debug!(
                "no interruption to announce: the turn was aborted without a Cancel: session_id={}",
                backend.session_id()
            );
            return;
        }
        context
            .event_sink
            .emit_required(AgentEvent::Turn(TurnEvent::Interrupted {
                cancellation_id: interrupted_cancellation_id(&entries).unwrap_or_default(),
            }));
    }

    /// Whether the log holds user input no turn has answered. A redelivered
    /// activation is the only thing that will run it.
    async fn has_queued_input(&self, backend: &NatsSessionLogBackend) -> Result<bool> {
        let entries = backend.load_events_latest_async().await?;
        let state = harnx_core::session_reconstruct::reconstruct_state_from_nats(&entries);
        Ok(!state.next_turn_messages.is_empty())
    }
}

/// The turn task as `finish` receives it, with the turn loop's own verdict on
/// whether this activation still has work to come back for.
pub(super) struct FinishedTurn {
    /// Present only when the turn was aborted and is still dropping.
    pub task: Option<tokio::task::JoinHandle<Result<bool>>>,
    pub settled: bool,
    /// The turn's own config, which carries the abort signal that says whether
    /// it was interrupted and the maintenance flags its drop still settles.
    pub config: crate::config::GlobalConfig,
}

/// Let an aborted turn finish dropping on its own, and its post-turn
/// maintenance settle, without holding the activation open for either.
fn drain_abandoned_turn(
    turn: Option<tokio::task::JoinHandle<Result<bool>>>,
    turn_config: crate::config::GlobalConfig,
) {
    tokio::spawn(async move {
        if let Some(turn) = turn {
            let _ = turn.await;
        }
        while turn_config
            .read()
            .session
            .as_ref()
            .is_some_and(|session| session.compressing() || session.titling())
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });
}

/// The cancellation id of the `Cancel` that terminated the current turn.
fn interrupted_cancellation_id(entries: &[(u64, SessionLogEntry)]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::Cancel {
                cancellation_id, ..
            } => Some(cancellation_id.clone()),
            _ => None,
        })
        .flatten()
}

/// Terminate the turn this worker is abandoning, and report the sequence the
/// terminator landed at. A `Cancel` that beat us to the tail is that
/// terminator: the turn is over either way, so its sequence is the answer
/// rather than a reason to fail.
async fn append_worker_cancel(
    backend: &NatsSessionLogBackend,
    lease: &NatsSessionLease,
    entries: &[(u64, SessionLogEntry)],
) -> Result<u64> {
    let cancel = SessionLogEntry::Cancel {
        fence_token: lease.fence_token(),
        cancellation_id: Some(uuid::Uuid::now_v7().to_string()),
        requested_by: Some(format!("worker:{}", lease.worker_id())),
        timestamp: Some(chrono::Utc::now()),
    };
    let expected_tail = entries.last().map_or(0, |(seq, _)| *seq);
    match backend
        .append_event_fenced_with_lease(&cancel, lease, expected_tail)
        .await
    {
        Ok(seq) => Ok(seq),
        Err(error) => match error.downcast_ref::<super::backend::TurnInterrupted>() {
            Some(interrupted) => Ok(interrupted.cancel_seq),
            None => Err(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nats_event_sink::{events_subject, AdvisoryEnvelope, NatsEventSink};
    use crate::nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore};
    use crate::nats_test_common::spawn_nats_server;
    use futures_util::StreamExt;
    use harnx_core::message::{MessageContent, MessageRole};
    use std::time::Duration;

    /// A turn that this worker aborted and then finished without its lease:
    /// exactly the shape of a lease-loss failover, and of a real Ctrl+C whose
    /// owner died before it could wind up.
    struct AbandonedTurn {
        execution: WorkerExecution,
        backend: NatsSessionLogBackend,
        lease: std::sync::Arc<crate::nats_lease::NatsSessionLease>,
        log: crate::nats_session_log::NatsSessionLog,
        advisories: async_nats::Subscriber,
    }

    impl AbandonedTurn {
        async fn seed(url: &str) -> Self {
            let client = async_nats::connect(url).await.unwrap();
            let jetstream = async_nats::jetstream::new(client.clone());
            let metadata = SessionMetadataStore::ensure(&jetstream, 1).await.unwrap();
            let session_id = crate::nats_worker::new_remote_session_id();
            let storage_key = harnx_core::session_identity::session_key(Some("metis"), &session_id);
            metadata
                .create(&SessionMetadata::new(
                    &session_id,
                    SessionInitializer::named("metis", Default::default()),
                ))
                .await
                .unwrap();
            let log = crate::nats_session_log::NatsSessionLog::new(
                jetstream.clone(),
                storage_key.clone(),
            );
            log.append_event_async(&SessionLogEntry::Message {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("go".into()),
                timestamp: None,
                fence_token: None,
            })
            .await
            .unwrap();
            let advisories = client
                .subscribe(events_subject(&storage_key))
                .await
                .unwrap();
            client.flush().await.unwrap();

            let lease =
                super::super::backend::test_session_authority(&jetstream, &storage_key, &metadata)
                    .await;
            let execution = WorkerExecution::claim(&SessionActivate::new(&storage_key), &lease)
                .with_wind_up(WindUpContext {
                    client: client.clone(),
                    jetstream: jetstream.clone(),
                    replicas: 1,
                    in_flight: crate::nats_tool_provider::NatsInFlightCalls::default(),
                    event_sink: std::sync::Arc::new(
                        NatsEventSink::new(client, jetstream.clone(), storage_key.clone()).await,
                    ),
                    interrupted: std::sync::Arc::new(parking_lot::Mutex::new(None)),
                });
            Self {
                backend: NatsSessionLogBackend::new(jetstream, &storage_key),
                execution,
                lease,
                log,
                advisories,
            }
        }

        /// Finish the turn the way `execute_session` does after its abort
        /// signal fired — with the lease already gone, so no wind-up runs and
        /// the advisory is the only thing `finish` can still emit.
        async fn finish_after_lease_loss(&self) {
            let aborted = harnx_core::abort::create_abort_signal();
            aborted.set_ctrlc();
            let config = crate::config::Config {
                maintenance_abort: Some(aborted),
                ..Default::default()
            };
            self.lease.release().await.unwrap();
            self.execution
                .finish(
                    &self.backend,
                    &self.lease,
                    FinishedTurn {
                        task: None,
                        settled: true,
                        config: std::sync::Arc::new(parking_lot::RwLock::new(config)),
                    },
                )
                .await
                .unwrap();
            self.execution
                .wind_up
                .as_ref()
                .unwrap()
                .event_sink
                .flush()
                .await
                .unwrap();
        }

        /// The `Interrupted` advisories published while finishing, if any.
        async fn interrupted_advisories(&mut self) -> Vec<String> {
            let mut seen = Vec::new();
            while let Ok(Some(message)) =
                tokio::time::timeout(Duration::from_millis(250), self.advisories.next()).await
            {
                let envelope: AdvisoryEnvelope = serde_json::from_slice(&message.payload).unwrap();
                if let AgentEvent::Turn(TurnEvent::Interrupted { cancellation_id }) = envelope.event
                {
                    seen.push(cancellation_id);
                }
            }
            seen
        }
    }

    /// Losing the lease fires the same abort signal a `Cancel` does, and then
    /// a replacement worker resumes the turn. Frontends treat `Interrupted`
    /// like `Ended`, so announcing one here stops the user's spinner on a turn
    /// that is still running.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_turn_abandoned_without_a_cancel_announces_no_interruption() {
        let Some(server) = spawn_nats_server().await.unwrap() else {
            return;
        };
        let mut turn = AbandonedTurn::seed(server.url()).await;

        turn.finish_after_lease_loss().await;

        assert!(
            turn.interrupted_advisories().await.is_empty(),
            "nothing terminated this turn, so nothing may tell a frontend it ended"
        );
    }

    /// A `Cancel` really did terminate the turn, and with no tool calls to
    /// wind up the advisory is the only thing that says so.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_turn_still_announces_its_interruption_once() {
        let Some(server) = spawn_nats_server().await.unwrap() else {
            return;
        };
        let mut turn = AbandonedTurn::seed(server.url()).await;
        turn.log
            .append_event_async(&SessionLogEntry::cancel_request(
                "cancel-9".into(),
                "frontend".into(),
            ))
            .await
            .unwrap();

        turn.finish_after_lease_loss().await;

        assert_eq!(turn.interrupted_advisories().await, vec!["cancel-9"]);
    }
}
