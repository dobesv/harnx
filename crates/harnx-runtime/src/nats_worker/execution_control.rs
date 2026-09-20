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

/// Specific reason for a worker failover (no Cancel log entry, non-terminal NAK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverCause {
    /// Graceful shutdown (SIGTERM, process terminating).
    Shutdown,
    /// Lease lost to another worker (or natural expiry).
    LeaseLost,
}

/// Why a worker session turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishCause {
    /// Turn completed normally. Terminal ACK only if settled AND no queued input.
    Completed {
        /// Turn settled normally (no pending work to redeliver).
        settled: bool,
        /// Input is queued behind the turn (requires redelivery for execution).
        has_queued_input: bool,
    },
    /// User interrupted via Ctrl-C/Ctrl-D (durable Cancel written, terminal ACK).
    UserCancelled,
    /// Worker failover (no Cancel, non-terminal NAK).
    Failover(FailoverCause),
}

impl FinishCause {
    /// Whether this cause warrants a terminal ACK.
    ///
    /// - `UserCancelled`: always terminal (Cancel was written).
    /// - `Completed`: terminal only if settled AND no queued input.
    /// - `Failover`: never terminal (NAK for redelivery).
    pub fn is_terminal(&self) -> bool {
        match self {
            FinishCause::UserCancelled => true,
            FinishCause::Completed {
                settled,
                has_queued_input,
            } => *settled && !has_queued_input,
            FinishCause::Failover(_) => false,
        }
    }
}

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

struct TurnCleanup {
    task: Option<tokio::task::JoinHandle<Result<bool>>>,
    config: crate::config::GlobalConfig,
}

#[derive(Clone, Copy)]
struct FinishCleanupContext<'a> {
    backend: &'a NatsSessionLogBackend,
    lease: &'a Arc<NatsSessionLease>,
    session_id: &'a str,
    worker_id: &'a str,
}

struct FinishDecision<'a> {
    settled: bool,
    config: &'a crate::config::GlobalConfig,
    failover_cause: Option<FailoverCause>,
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
        append_worker_cancel(backend, lease, self.worker_id.clone(), &entries)
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
        worker_id: String,
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
            worker_id,
            self.fence_token,
            notice.as_ref().map(|notice| notice.cancel_seq),
            notice
                .as_ref()
                .and_then(|notice| notice.cancellation_id.as_deref()),
        );
        Ok(outcome)
    }

    /// Wind the turn up if it was interrupted, release the lease, and report
    /// the cause for disposition.
    ///
    /// `settled` is the turn loop's own answer to "is there anything left this
    /// activation was published for": a pending HITL approval says no. Input
    /// queued behind the turn says no too, whatever the loop thought, because
    /// only a redelivered activation can run it.
    pub async fn finish(
        &self,
        backend: &NatsSessionLogBackend,
        lease: &Arc<NatsSessionLease>,
        turn: FinishedTurn,
    ) -> Result<FinishCause> {
        let FinishedTurn {
            task,
            settled,
            config: turn_config,
            failover_cause,
        } = turn;

        let cause = self
            .determine_finish_cause(
                backend,
                lease,
                FinishDecision {
                    settled,
                    config: &turn_config,
                    failover_cause,
                },
            )
            .await?;

        let cleanup_context = FinishCleanupContext {
            backend,
            lease,
            session_id: &self.session_id,
            worker_id: &self.worker_id,
        };
        let cleanup = TurnCleanup {
            task,
            config: turn_config,
        };
        match cause {
            FinishCause::Failover(failover) => {
                self.finish_failover(cleanup_context, cleanup, failover)
                    .await?;
            }
            FinishCause::Completed { .. } | FinishCause::UserCancelled => {
                self.finish_completed(cleanup_context, cleanup, cause)
                    .await?;
            }
        }
        Ok(cause)
    }

    fn abort_finish_state(config: &crate::config::GlobalConfig) -> (bool, bool) {
        config
            .read()
            .maintenance_abort
            .as_ref()
            .map_or((false, false), |signal| {
                (
                    signal.aborted() && !signal.aborted_failover(),
                    signal.aborted_failover(),
                )
            })
    }

    fn failover_for_lease(lease: &NatsSessionLease) -> FailoverCause {
        if lease.is_held() {
            FailoverCause::Shutdown
        } else {
            FailoverCause::LeaseLost
        }
    }
    async fn determine_finish_cause(
        &self,
        backend: &NatsSessionLogBackend,
        lease: &NatsSessionLease,
        decision: FinishDecision<'_>,
    ) -> Result<FinishCause> {
        if let Some(cause) = decision.failover_cause {
            log::info!(
                "turn finishing with explicit failover cause: session_id={} cause={cause:?}",
                self.session_id
            );
            return Ok(FinishCause::Failover(cause));
        }
        let (user_cancelled, failover_aborted) = Self::abort_finish_state(decision.config);
        if failover_aborted {
            return Ok(FinishCause::Failover(Self::failover_for_lease(lease)));
        }
        if user_cancelled && lease.is_held() {
            self.close_out_cancelled_turn(backend, lease, self.worker_id.clone())
                .await?;
            return Ok(FinishCause::UserCancelled);
        }
        if !lease.is_held() {
            log::debug!(
                "turn ended with lease lost (failover): session_id={}",
                self.session_id
            );
            return Ok(FinishCause::Failover(FailoverCause::LeaseLost));
        }
        Ok(FinishCause::Completed {
            settled: decision.settled,
            has_queued_input: self.has_queued_input(backend).await?,
        })
    }

    async fn finish_completed(
        &self,
        context: FinishCleanupContext<'_>,
        cleanup: TurnCleanup,
        cause: FinishCause,
    ) -> Result<()> {
        self.announce_interruption_if_cancelled(context.backend)
            .await;
        log::debug!(
            "releasing the session lease: session_id={} worker_id={} cause={cause:?}",
            context.session_id,
            context.worker_id,
        );
        context.lease.release().await?;
        if matches!(cause, FinishCause::UserCancelled) {
            drain_abandoned_turn(cleanup.task, cleanup.config);
        }
        Ok(())
    }

    async fn finish_failover(
        &self,
        context: FinishCleanupContext<'_>,
        cleanup: TurnCleanup,
        cause: FailoverCause,
    ) -> Result<()> {
        settle_abandoned_turn(cleanup.task, &cleanup.config).await;
        match cause {
            FailoverCause::Shutdown => context.lease.release().await?,
            FailoverCause::LeaseLost => {
                // Never release a possibly stale handle after ownership loss.
                self.announce_interruption_if_cancelled(context.backend)
                    .await;
            }
        }
        Ok(())
    }
    /// Tell attached frontends the turn ended in an interruption, IF a Cancel
    /// exists in the log. This handles lease-loss after a frontend wrote Cancel.
    async fn announce_interruption_if_cancelled(&self, backend: &NatsSessionLogBackend) {
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
    /// Turn settled normally (no pending HITL approval).
    pub settled: bool,
    /// The turn's own config, which carries the abort signal that says whether
    /// it was interrupted and the maintenance flags its drop still settles.
    pub config: crate::config::GlobalConfig,
    /// Explicit failover cause (Shutdown or LeaseLost). When set, bypasses
    /// abort signal interpretation and yields Failover disposition.
    pub failover_cause: Option<FailoverCause>,
}

impl FinishedTurn {
    /// Create a FinishedTurn with an explicit failover cause.
    /// Used when the worker is shutting down gracefully (Shutdown) or when
    /// lease loss is detected externally.
    pub fn for_failover(
        task: Option<tokio::task::JoinHandle<Result<bool>>>,
        config: crate::config::GlobalConfig,
        cause: FailoverCause,
    ) -> Self {
        Self {
            task,
            settled: false,
            config,
            failover_cause: Some(cause),
        }
    }
}

/// Confirm an aborted execution body and its durable maintenance have settled
/// before voluntary shutdown releases ownership.
async fn settle_abandoned_turn(
    turn: Option<tokio::task::JoinHandle<Result<bool>>>,
    turn_config: &crate::config::GlobalConfig,
) {
    if let Some(turn) = turn {
        let _ = turn.await;
    }
    let maintenance = async {
        while turn_config
            .read()
            .session
            .as_ref()
            .is_some_and(|session| session.compressing() || session.titling())
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    if tokio::time::timeout(std::time::Duration::from_secs(3), maintenance)
        .await
        .is_err()
    {
        log::warn!("timed out waiting for abandoned turn maintenance to settle");
    }
}

/// Let an aborted turn finish dropping on its own, and its post-turn
/// maintenance settle, without holding the activation open for either.
fn drain_abandoned_turn(
    turn: Option<tokio::task::JoinHandle<Result<bool>>>,
    turn_config: crate::config::GlobalConfig,
) {
    tokio::spawn(async move {
        settle_abandoned_turn(turn, &turn_config).await;
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
    worker_id: String,
    entries: &[(u64, SessionLogEntry)],
) -> Result<u64> {
    let cancel = SessionLogEntry::Cancel {
        fence_token: lease.fence_token(),
        cancellation_id: Some(uuid::Uuid::now_v7().to_string()),
        requested_by: Some(format!("worker:{worker_id}")),
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
    use crate::nats_worker::tests::spawn_test_nats;
    use futures_util::StreamExt;
    use harnx_core::message::{MessageContent, MessageRole};
    use std::time::Duration;

    /// A turn that this worker aborted and then finished without its lease:
    /// exactly the shape of a lease-loss failover, and of a real Ctrl+C whose
    /// owner died before it could wind up.
    struct AbandonedTurn {
        execution: WorkerExecution,
        backend: NatsSessionLogBackend,
        lease: std::sync::Arc<NatsSessionLease>,
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
            let log = crate::nats_session_log::NatsSessionLog::new_with_replicas(
                jetstream.clone(),
                storage_key.clone(),
                1,
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
                backend: NatsSessionLogBackend::new(jetstream, &storage_key, 1),
                execution,
                lease,
                log,
                advisories,
            }
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

    /// Unit test: FinishCause::UserCancelled is terminal.
    #[test]
    fn user_cancelled_is_terminal() {
        harnx_core::require_nextest();
        assert!(FinishCause::UserCancelled.is_terminal());
    }

    /// Unit test: FinishCause::Completed is terminal only when settled and no queued input.
    #[test]
    fn completed_terminal_only_when_settled_and_no_queued_input() {
        harnx_core::require_nextest();
        // Terminal: settled, no queued input
        assert!(
            matches!(FinishCause::Completed { settled: true, has_queued_input: false }, cause if cause.is_terminal())
        );
        // Non-terminal: not settled
        assert!(
            !matches!(FinishCause::Completed { settled: false, has_queued_input: false }, cause if cause.is_terminal())
        );
        // Non-terminal: queued input
        assert!(
            !matches!(FinishCause::Completed { settled: true, has_queued_input: true }, cause if cause.is_terminal())
        );
        // Non-terminal: both issues
        assert!(
            !matches!(FinishCause::Completed { settled: false, has_queued_input: true }, cause if cause.is_terminal())
        );
    }

    /// Unit test: FinishCause::Failover is never terminal.
    #[test]
    fn failover_is_never_terminal() {
        harnx_core::require_nextest();
        assert!(
            !matches!(FinishCause::Failover(FailoverCause::Shutdown), cause if cause.is_terminal())
        );
        assert!(
            !matches!(FinishCause::Failover(FailoverCause::LeaseLost), cause if cause.is_terminal())
        );
    }

    async fn assert_announcement_behavior(cancel_id: Option<&str>) {
        harnx_core::require_nextest();
        let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
            return;
        };
        let mut turn = AbandonedTurn::seed(&url).await;
        if let Some(cancel_id) = cancel_id {
            turn.log
                .append_event_async(&SessionLogEntry::cancel_request(
                    cancel_id.into(),
                    "frontend".into(),
                ))
                .await
                .unwrap();
        }
        turn.lease.release().await.unwrap();
        let aborted = harnx_core::abort::create_abort_signal();
        aborted.set_ctrlc();
        let config = crate::config::Config {
            maintenance_abort: Some(aborted),
            ..Default::default()
        };
        let cause = turn
            .execution
            .finish(
                &turn.backend,
                &turn.lease,
                FinishedTurn {
                    task: None,
                    settled: true,
                    config: std::sync::Arc::new(parking_lot::RwLock::new(config)),
                    failover_cause: None,
                },
            )
            .await
            .unwrap();
        turn.execution
            .wind_up
            .as_ref()
            .unwrap()
            .event_sink
            .flush()
            .await
            .unwrap();

        assert_eq!(cause, FinishCause::Failover(FailoverCause::LeaseLost));
        let expected = cancel_id
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(turn.interrupted_advisories().await, expected);
        let _ = child.kill();
        let _ = child.wait();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_turn_abandoned_without_a_cancel_announces_no_interruption() {
        assert_announcement_behavior(None).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_turn_still_announces_its_interruption_once() {
        assert_announcement_behavior(Some("cancel-9")).await;
    }

    /// Test that explicit FailoverCause::Shutdown bypasses Cancel and yields non-terminal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_shutdown_failover_skips_cancel() {
        harnx_core::require_nextest();
        let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
            return;
        };
        let mut turn = AbandonedTurn::seed(&url).await;

        // Finish with explicit shutdown failover cause
        let config = crate::config::Config::default();
        let cause = turn
            .execution
            .finish(
                &turn.backend,
                &turn.lease,
                FinishedTurn::for_failover(
                    None,
                    std::sync::Arc::new(parking_lot::RwLock::new(config)),
                    FailoverCause::Shutdown,
                ),
            )
            .await
            .unwrap();
        turn.execution
            .wind_up
            .as_ref()
            .unwrap()
            .event_sink
            .flush()
            .await
            .unwrap();

        // Cause should be Failover(Shutdown), non-terminal
        assert!(matches!(
            cause,
            FinishCause::Failover(FailoverCause::Shutdown)
        ));
        assert!(!cause.is_terminal());

        // No Cancel written, no interruption announced
        let entries = turn.backend.load_events_latest_async().await.unwrap();
        let has_cancel = entries
            .iter()
            .any(|(_, e)| matches!(e, SessionLogEntry::Cancel { .. }));
        assert!(!has_cancel, "Shutdown failover should not write Cancel");

        assert!(
            turn.interrupted_advisories().await.is_empty(),
            "Shutdown failover should not announce interruption"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_lease_loss_does_not_release_a_still_held_handle() {
        harnx_core::require_nextest();
        let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
            return;
        };
        let turn = AbandonedTurn::seed(&url).await;

        let cause = turn
            .execution
            .finish(
                &turn.backend,
                &turn.lease,
                FinishedTurn::for_failover(
                    None,
                    std::sync::Arc::new(parking_lot::RwLock::new(crate::config::Config::default())),
                    FailoverCause::LeaseLost,
                ),
            )
            .await
            .unwrap();

        assert_eq!(cause, FinishCause::Failover(FailoverCause::LeaseLost));
        assert!(
            turn.lease.is_held(),
            "lease-loss cleanup must not release a possibly stale lease handle"
        );
        turn.lease.release().await.unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failover_abort_is_never_interpreted_as_user_cancellation() {
        harnx_core::require_nextest();
        let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
            return;
        };
        let turn = AbandonedTurn::seed(&url).await;
        let abort = harnx_core::abort::create_abort_signal();
        abort.set_failover();
        let config = crate::config::Config {
            maintenance_abort: Some(abort),
            ..Default::default()
        };

        let cause = turn
            .execution
            .finish(
                &turn.backend,
                &turn.lease,
                FinishedTurn {
                    task: None,
                    settled: true,
                    config: std::sync::Arc::new(parking_lot::RwLock::new(config)),
                    failover_cause: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(cause, FinishCause::Failover(FailoverCause::Shutdown));
        let entries = turn.backend.load_events_latest_async().await.unwrap();
        assert!(!entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })));
        let _ = child.kill();
        let _ = child.wait();
    }
}
