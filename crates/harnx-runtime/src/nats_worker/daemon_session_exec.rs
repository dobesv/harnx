//! Running a claimed session's turn loop to completion: the drain decision
//! between turns, recording a failed turn durably, and the lease-loss watch
//! that aborts promptly on failover.

use super::backend::NatsSessionLogBackend;
use super::daemon::{should_append_control_log_entry, SessionActivate};
use super::daemon_runtime::WorkerRuntime;
use super::execution_control::{FailoverCause, FinishCause, FinishedTurn, WorkerExecution};
use crate::nats_lease::NatsSessionLease;
use crate::OnToolRoundFn;
use anyhow::{Context, Result};
use harnx_core::api_types::CompletionTokenUsage;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::task::JoinHandle;

pub(super) const MAX_ACTIVATION_FAILURES: u64 = 10;
/// Deliveries that must refuse an activation before the refusal is recorded.
/// The second look costs one redelivery and keeps a single wrong read, a
/// misconfigured broker say, from failing a prompt for good.
const REFUSAL_CONFIRMATIONS: u64 = 2;

/// Marks an execution failure whose durable Error entry makes the activation
/// safe to remove from JetStream.
#[derive(Debug)]
pub(super) struct DurableActivationError {
    source: anyhow::Error,
}

impl DurableActivationError {
    fn new(source: anyhow::Error) -> Self {
        Self { source }
    }
}

impl std::fmt::Display for DurableActivationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for DurableActivationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

pub(super) fn is_durable_activation_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<DurableActivationError>().is_some())
}

/// A refusal no redelivery can change, such as a prompt without a durable run
/// admission. It is recorded on the session once a second delivery confirms
/// it, instead of after the ten failures the budget allows for failures that
/// may clear up.
#[derive(Debug)]
pub(super) struct ActivationRefusal(String);

impl std::fmt::Display for ActivationRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ActivationRefusal {}

pub(super) fn is_activation_refusal(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<ActivationRefusal>().is_some())
}

/// The id of the latest prompt, or compaction request, at or before
/// `requested`.
fn latest_prompt_id(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
    requested: u64,
) -> Option<String> {
    use harnx_core::session::SessionLogEntry;
    entries.iter().rev().find_map(|(seq, entry)| match entry {
        SessionLogEntry::Message {
            id: Some(id), role, ..
        } if role.is_user() && *seq <= requested => Some(id.clone()),
        SessionLogEntry::CompactRequest { compaction_id, .. } if *seq <= requested => {
            Some(compaction_id.clone())
        }
        _ => None,
    })
}

/// The frontend writes a prompt's admission before it appends the prompt, so
/// a prompt in the log without one is from before admissions existed or was
/// appended by a path that skipped admission. Running it anyway would have
/// the worker grant run authority that only a frontend may grant.
fn unadmitted_prompt(prompt_id: &str) -> anyhow::Error {
    anyhow::Error::new(ActivationRefusal(format!(
        "refusing executable prompt without durable run admission: prompt '{prompt_id}' \
         was sent before run admissions existed or appended without one; send it again \
         to start a new run"
    )))
}

pub(super) struct SessionExecutionInputs {
    pub(super) activation: SessionActivate,
    pub(super) activation_failure_key: String,
    pub(super) lease: Arc<NatsSessionLease>,
    pub(super) abort_signal: crate::utils::AbortSignal,
    pub(super) control_task: JoinHandle<()>,
    pub(super) hitl_decision_rx:
        tokio::sync::mpsc::UnboundedReceiver<super::control::AppliedHitlDecision>,
    pub(super) execution: super::execution_control::WorkerExecution,
    pub(super) shutdown: tokio_util::sync::CancellationToken,
}

struct TurnBodyOutcome {
    result: Result<bool>,
    cleanup_turn: Option<JoinHandle<Result<bool>>>,
    failover_cause: Option<FailoverCause>,
}

struct FinishExecutionInputs {
    result: Result<bool>,
    cleanup_turn: Option<JoinHandle<Result<bool>>>,
    settled: bool,
    failover_cause: Option<FailoverCause>,
    config: crate::config::GlobalConfig,
    execution_abort: crate::utils::AbortSignal,
    shutdown: tokio_util::sync::CancellationToken,
}

struct SessionRuntimeStartup {
    activation: SessionActivate,
    activation_failure_key: String,
    lease: Arc<NatsSessionLease>,
    execution_abort: crate::utils::AbortSignal,
    abort_relay: JoinHandle<()>,
    shutdown_relay: JoinHandle<()>,
    control_task: JoinHandle<()>,
    hitl_decision_rx: tokio::sync::mpsc::UnboundedReceiver<super::control::AppliedHitlDecision>,
    execution: WorkerExecution,
    shutdown: tokio_util::sync::CancellationToken,
    per_session: crate::config::GlobalConfig,
    backend: NatsSessionLogBackend,
    event_sink: Arc<crate::nats_event_sink::NatsEventSink>,
    after_seq_observer: Arc<AtomicU64>,
    agent_setup: Result<()>,
}

struct PreparedSessionRuntime {
    activation: SessionActivate,
    lease: Arc<NatsSessionLease>,
    execution_abort: crate::utils::AbortSignal,
    abort_relay: JoinHandle<()>,
    shutdown_relay: JoinHandle<()>,
    control_task: JoinHandle<()>,
    hitl_decision_rx: tokio::sync::mpsc::UnboundedReceiver<super::control::AppliedHitlDecision>,
    execution: WorkerExecution,
    shutdown: tokio_util::sync::CancellationToken,
    per_session: crate::config::GlobalConfig,
    backend: NatsSessionLogBackend,
    event_sink: Arc<crate::nats_event_sink::NatsEventSink>,
    after_seq_observer: Arc<AtomicU64>,
    pending_input: Arc<std::sync::atomic::AtomicBool>,
    interrupted: Arc<parking_lot::Mutex<Option<super::session_watcher::InterruptNotice>>>,
    pending_compaction: Arc<parking_lot::Mutex<Option<String>>>,
    in_flight: crate::nats_tool_provider::NatsInFlightCalls,
    session_watcher: JoinHandle<()>,
    agent_setup: Result<()>,
}

struct RunningSessionRuntime {
    activation: SessionActivate,
    lease: Arc<NatsSessionLease>,
    execution_abort: crate::utils::AbortSignal,
    abort_relay: JoinHandle<()>,
    shutdown_relay: JoinHandle<()>,
    control_task: JoinHandle<()>,
    execution: WorkerExecution,
    shutdown: tokio_util::sync::CancellationToken,
    per_session: crate::config::GlobalConfig,
    backend: NatsSessionLogBackend,
    session_watcher: JoinHandle<()>,
    watch_task: JoinHandle<()>,
    turn: Option<JoinHandle<Result<bool>>>,
}

/// Fail a local named-agent activation early when no chat model resolved.
///
/// Frontends may now boot with no local model (remote-only deployments); the
/// requirement moves here, to the worker that actually runs the agent loop. A
/// clear setup error is recorded as a durable turn failure instead of the agent
/// loop failing later with the opaque `Invalid model ''` from the client layer.
///
/// Only `Named` sources are checked. `Inline` sessions carry a resolved model
/// enforced upstream at `SessionInitializer::from_config` ("inline NATS sessions
/// require a resolved model"), so an unset model can't reach here for them; the
/// early return keeps this from being a second, divergent gate on that path.
fn ensure_named_agent_has_model(
    per_session: &crate::config::GlobalConfig,
    metadata: &crate::nats_session_metadata::SessionMetadata,
) -> Result<()> {
    let crate::nats_session_metadata::SessionAgentSource::Named { name } = &metadata.agent else {
        return Ok(());
    };
    anyhow::ensure!(
        per_session.read().current_model_id().is_some(),
        "no chat model configured for local agent '{name}'; configure a client/model or route to a remote worker"
    );
    Ok(())
}
impl WorkerRuntime {
    pub(super) async fn count_activation_failure(
        &self,
        activation: &SessionActivate,
        activation_failure_key: &str,
    ) -> Result<u64> {
        self.activation_failures
            .increment(activation_failure_key)
            .await
            .with_context(|| {
                format!(
                    "count pre-turn failure for session '{}'",
                    activation.session_id
                )
            })
    }

    pub(super) async fn classify_pre_turn_failure(
        &self,
        activation: &SessionActivate,
        activation_failure_key: &str,
        lease: &NatsSessionLease,
        error: anyhow::Error,
    ) -> anyhow::Error {
        let budget = if is_activation_refusal(&error) {
            REFUSAL_CONFIRMATIONS
        } else {
            MAX_ACTIVATION_FAILURES
        };
        if self
            .failure_budget_spent(activation, activation_failure_key, budget)
            .await
        {
            return self
                .durabilize_pre_turn_failure(activation, lease, error)
                .await;
        }
        error
    }

    /// Count this failure against the activation. A counter that can't be
    /// updated leaves the budget unspent, so the activation is retried.
    async fn failure_budget_spent(
        &self,
        activation: &SessionActivate,
        activation_failure_key: &str,
        budget: u64,
    ) -> bool {
        match self
            .count_activation_failure(activation, activation_failure_key)
            .await
        {
            Ok(failures) => failures >= budget,
            Err(counter_error) => {
                log::warn!(
                    "failed to count activation failure: session_id={} error={counter_error:#}",
                    activation.session_id
                );
                false
            }
        }
    }

    pub(super) async fn durabilize_pre_turn_failure(
        &self,
        activation: &SessionActivate,
        lease: &NatsSessionLease,
        error: anyhow::Error,
    ) -> anyhow::Error {
        let backend = NatsSessionLogBackend::new(
            self.jetstream.clone(),
            &activation.session_id,
            self.lease.replicas,
        );
        match Self::record_session_error(&backend, lease, &error).await {
            Ok(true) => anyhow::Error::new(DurableActivationError::new(error)),
            Ok(false) | Err(_) => error,
        }
    }

    pub(super) async fn execute_session(
        &self,
        inputs: SessionExecutionInputs,
    ) -> Result<FinishCause> {
        let startup = self.prepare_session_runtime(inputs).await?;
        let prepared = self.start_session_runtime_services(startup).await?;

        let mut running = self.start_session_execution(prepared);
        let turn = running.turn.take().expect("prepared session turn");
        let outcome =
            Self::await_turn_body(turn, &running.execution_abort, &running.shutdown).await;
        self.complete_session_execution(running, outcome).await
    }

    /// Load the canonical session metadata for an activation, refusing to run
    /// without it — a worker never invents identity for a session it claimed.
    async fn load_activation_metadata(
        &self,
        session_id: &str,
    ) -> Result<crate::nats_session_metadata::SessionMetadata> {
        Ok(self
            .session_metadata
            .get(session_id)
            .await?
            .with_context(|| {
                format!("refusing activation without canonical session metadata: {session_id}")
            })?
            .metadata)
    }

    fn session_backend(
        &self,
        activation: &SessionActivate,
        after_seq_observer: Arc<AtomicU64>,
    ) -> NatsSessionLogBackend {
        NatsSessionLogBackend::new(
            self.jetstream.clone(),
            &activation.session_id,
            self.lease.replicas,
        )
        .with_after_seq_observer(after_seq_observer)
        .with_metadata_store(Some(self.session_metadata.clone()))
    }

    async fn prepare_session_runtime(
        &self,
        inputs: SessionExecutionInputs,
    ) -> Result<SessionRuntimeStartup> {
        let SessionExecutionInputs {
            activation,
            activation_failure_key,
            lease,
            abort_signal,
            control_task,
            hitl_decision_rx,
            execution,
            shutdown,
        } = inputs;
        let metadata = match self.load_activation_metadata(&activation.session_id).await {
            Ok(metadata) => metadata,
            Err(error) => {
                Self::stop_execution_tasks([control_task]).await;
                return Err(self
                    .classify_pre_turn_failure(&activation, &activation_failure_key, &lease, error)
                    .await);
            }
        };
        let (execution_abort, abort_relay, shutdown_relay) =
            Self::spawn_execution_abort_relays(abort_signal, shutdown.clone());
        let per_session = {
            let mut base = self.config.read().clone();
            base.maintenance_abort = Some(execution_abort.clone());
            Arc::new(crate::config::ConfigLock::new(base))
        };
        self.configure_tool_confirmation(
            &per_session,
            activation.tool_confirmation_subject.as_ref(),
            &metadata.session_id,
            &execution_abort,
        );
        let agent_setup = super::daemon::install_session_metadata_agent(&per_session, &metadata)
            .and_then(|()| ensure_named_agent_has_model(&per_session, &metadata));
        let event_sink = Arc::new(
            crate::nats_event_sink::NatsEventSink::new(
                self.client.clone(),
                self.jetstream.clone(),
                activation.session_id.clone(),
            )
            .await,
        );
        let after_seq_observer = event_sink.after_seq_handle();
        let backend = self.session_backend(&activation, Arc::clone(&after_seq_observer));
        let startup = SessionRuntimeStartup {
            activation,
            activation_failure_key,
            lease,
            execution_abort,
            abort_relay,
            shutdown_relay,
            control_task,
            hitl_decision_rx,
            execution,
            shutdown,
            per_session,
            backend,
            event_sink,
            after_seq_observer,
            agent_setup,
        };
        match self.freeze_run_policy(&startup).await {
            Ok(()) => Ok(startup),
            Err(error) => Err(self.abandon_startup(startup, error).await),
        }
    }

    /// Freeze the limits this activation runs under, recover a child result
    /// that completed before its deadline, and arm the invocation's deadline.
    async fn freeze_run_policy(&self, startup: &SessionRuntimeStartup) -> Result<()> {
        self.freeze_activation_limits(&startup.activation, &startup.per_session)
            .await?;
        super::agent_loop::recover_completed_before_deadline(
            &startup.backend,
            &startup.lease,
            &startup.per_session,
            &self.jetstream,
            self.lease.replicas,
        )
        .await?;
        self.start_invocation_deadline(
            &startup.activation,
            &startup.per_session,
            &startup.execution_abort,
        )
        .await
    }

    /// Give up on a session whose turn can't start. The control listener and
    /// abort relays started for it would otherwise outlive the attempt, and
    /// every redelivery would leave another listener on the session's control
    /// subject.
    async fn abandon_startup(
        &self,
        startup: SessionRuntimeStartup,
        error: anyhow::Error,
    ) -> anyhow::Error {
        Self::stop_execution_tasks([
            startup.control_task,
            startup.abort_relay,
            startup.shutdown_relay,
        ])
        .await;
        self.classify_pre_turn_failure(
            &startup.activation,
            &startup.activation_failure_key,
            &startup.lease,
            error,
        )
        .await
    }

    async fn freeze_activation_limits(
        &self,
        activation: &SessionActivate,
        config: &crate::config::GlobalConfig,
    ) -> Result<()> {
        use crate::nats_session_metadata::{CallTimeoutOverride, RunLimitsRecord};
        let Some(intent) = self.requested_prompt_admission(activation).await? else {
            return Ok(());
        };
        let (global, target) = {
            let config = config.read();
            (
                config.data.run_limits,
                config
                    .agent
                    .as_ref()
                    .map(|agent| agent.clone().into_config()),
            )
        };
        let root = if intent.origin == crate::nats_session_metadata::AdmissionOrigin::External {
            Some(
                self.session_metadata
                    .load_or_create_run_limits(
                        &activation.session_id,
                        intent.run_id.as_str(),
                        || {
                            Ok(RunLimitsRecord::admit_root(
                                intent.run_id.clone(),
                                intent.invocation_id.clone(),
                                intent.admitted_at,
                                global,
                                target.as_ref(),
                                CallTimeoutOverride::from_optional(intent.timeout_secs),
                            )?)
                        },
                    )
                    .await?,
            )
        } else {
            None
        };
        let record = self
            .session_metadata
            .load_or_create_invocation_limits(
                &activation.session_id,
                intent.run_id.as_str(),
                intent.invocation_id.as_str(),
                || match &intent.parent {
                    Some(parent) => Ok(RunLimitsRecord::admit_child(
                        parent,
                        intent.invocation_id.clone(),
                        intent.edge.context("inherited admission has no edge")?,
                        intent.admitted_at,
                        global,
                        target.as_ref(),
                        CallTimeoutOverride::from_optional(intent.timeout_secs),
                    )?),
                    None => Ok(root.clone().context("root policy missing")?),
                },
            )
            .await?;
        if record.parent_invocation.is_none() {
            self.session_metadata
                .put_run_limits(&activation.session_id, &record)
                .await?;
        }
        config.write().run_context = Some(record);
        Ok(())
    }

    /// The admission of the prompt this activation runs, the latest one at or
    /// before its requested sequence, or `None` when there is no prompt to run.
    /// Never infer external authority from role: every executable prompt must
    /// reference a durable admission established before it was appended.
    async fn requested_prompt_admission(
        &self,
        activation: &SessionActivate,
    ) -> Result<Option<crate::nats_session_metadata::InvocationAdmission>> {
        let entries = crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            &activation.session_id,
        )
        .load_events_latest_async()
        .await?;
        let requested = activation
            .requested_seq
            .unwrap_or_else(|| entries.last().map_or(0, |(seq, _)| *seq));
        let Some(prompt_id) = latest_prompt_id(&entries, requested) else {
            return Ok(None);
        };
        self.session_metadata
            .prompt_admission(&activation.session_id, &prompt_id)
            .await?
            .map(Some)
            .ok_or_else(|| unadmitted_prompt(&prompt_id))
    }

    async fn start_invocation_deadline(
        &self,
        activation: &SessionActivate,
        config: &crate::config::GlobalConfig,
        execution_abort: &crate::utils::AbortSignal,
    ) -> Result<()> {
        let Some(record) = config.read().run_context.clone() else {
            return Ok(());
        };
        let Some(deadline) = record.deadline else {
            return Ok(());
        };
        let log = crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            &activation.session_id,
        );
        let js = self.jetstream.clone();
        let client = self.client.clone();
        let route = self.activation_route.clone();
        let cluster = self.cluster.clone();
        let replicas = self.lease.replicas;
        let storage = activation.session_id.clone();
        let abort = execution_abort.clone();
        let remaining = deadline
            .signed_duration_since(chrono::Utc::now())
            .to_std()
            .unwrap_or_default();
        if remaining.is_zero() {
            let entries = log.load_events_latest_async().await?;
            let prompt = self
                .session_metadata
                .invocation_prompt_seq(&storage, record.invocation_id.as_str(), &entries)
                .await?
                .context("expired admission has no prompt binding")?;
            let request = crate::nats_session::InterruptRequest {
                session_id: storage,
                cluster,
                replicas,
                cancellation_id: format!("deadline:{}", record.invocation_id.as_str()),
                reason: "autonomous invocation deadline expired".into(),
                requested_by: crate::TimeoutTerminal::from_record(&record)
                    .expect("finite worker deadline")
                    .message(),
            };
            let outcome = crate::nats_session::interrupt::interrupt_invocation(
                &js, &client, &route, request, prompt,
            )
            .await?;
            if outcome.cancel_seq().is_some() {
                execution_abort.set_ctrlc();
            }
            return Ok(());
        }
        // Detached on approval pause: deadline is owned by the worker, not by a
        // frontend waiter. Fencing makes late completion/timer races harmless.
        tokio::spawn(async move {
            tokio::time::sleep(remaining).await;
            let latest = match log.load_events_latest_async().await {
                Ok(entries) => entries,
                Err(error) => {
                    abort.set_failover();
                    log::error!("deadline log unavailable: {error:#}");
                    return;
                }
            };
            let metadata =
                match crate::nats_session_metadata::SessionMetadataStore::ensure(&js, replicas)
                    .await
                {
                    Ok(store) => store,
                    Err(error) => {
                        abort.set_failover();
                        log::error!("deadline metadata unavailable: {error:#}");
                        return;
                    }
                };
            let prompt = match metadata
                .invocation_prompt_seq(&storage, record.invocation_id.as_str(), &latest)
                .await
            {
                Ok(Some(prompt)) => prompt,
                Ok(None) => return,
                Err(error) => {
                    abort.set_failover();
                    log::error!("deadline binding unavailable: {error:#}");
                    return;
                }
            };
            let request = crate::nats_session::InterruptRequest {
                session_id: storage.clone(),
                cluster,
                replicas,
                cancellation_id: format!("deadline:{}", record.invocation_id.as_str()),
                reason: "autonomous invocation deadline expired".into(),
                requested_by: crate::TimeoutTerminal::from_record(&record)
                    .expect("finite worker deadline")
                    .message(),
            };
            match crate::nats_session::interrupt::interrupt_invocation(
                &js, &client, &route, request, prompt,
            )
            .await
            {
                Ok(outcome) if outcome.cancel_seq().is_some() => abort.set_ctrlc(),
                Ok(_) => {}
                Err(error) => {
                    // Stop dispatch even if the broker cannot confirm Cancel.
                    // Failover must recover the same frozen expired admission.
                    abort.set_failover();
                    log::error!("deadline cancellation unconfirmed, external cleanup unknown: storage={storage} error={error:#}");
                }
            }
        });
        Ok(())
    }

    async fn start_session_runtime_services(
        &self,
        startup: SessionRuntimeStartup,
    ) -> Result<PreparedSessionRuntime> {
        let pending_input = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let interrupted = Arc::new(parking_lot::Mutex::new(None));
        let pending_compaction = Arc::new(parking_lot::Mutex::new(None));
        let in_flight =
            crate::nats_tool_provider::NatsInFlightCalls::for_instance(&self.instance_id);
        let watcher_start_after = match startup.backend.load_events_latest_async().await {
            Ok(entries) => entries.last().map_or(0, |(seq, _)| *seq),
            Err(error) => return Err(self.abandon_startup(startup, error).await),
        };
        let session_watcher = super::session_watcher::spawn_session_watcher(
            super::session_watcher::SessionWatcherCtx {
                jetstream: self.jetstream.clone(),
                client: self.client.clone(),
                session_id: startup.activation.session_id.clone(),
                start_after: watcher_start_after,
                abort_signal: startup.execution_abort.clone(),
                in_flight: in_flight.clone(),
                own_appends: Arc::clone(&startup.after_seq_observer),
                pending_input: Arc::clone(&pending_input),
                interrupted: Arc::clone(&interrupted),
                pending_compaction: Arc::clone(&pending_compaction),
            },
        );
        Ok(PreparedSessionRuntime {
            activation: startup.activation,
            lease: startup.lease,
            execution_abort: startup.execution_abort,
            abort_relay: startup.abort_relay,
            shutdown_relay: startup.shutdown_relay,
            control_task: startup.control_task,
            hitl_decision_rx: startup.hitl_decision_rx,
            execution: startup.execution,
            shutdown: startup.shutdown,
            per_session: startup.per_session,
            backend: startup.backend,
            event_sink: startup.event_sink,
            after_seq_observer: startup.after_seq_observer,
            pending_input,
            interrupted,
            pending_compaction,
            in_flight,
            session_watcher,
            agent_setup: startup.agent_setup,
        })
    }

    fn start_session_execution(&self, prepared: PreparedSessionRuntime) -> RunningSessionRuntime {
        let invocation_id = prepared
            .per_session
            .read()
            .run_context
            .as_ref()
            .map(|record| record.invocation_id.as_str().to_owned());
        let execution = prepared
            .execution
            .with_invocation(invocation_id)
            .with_wind_up(super::execution_control::WindUpContext {
                client: self.client.clone(),
                jetstream: self.jetstream.clone(),
                replicas: self.lease.replicas,
                in_flight: prepared.in_flight,
                event_sink: Arc::clone(&prepared.event_sink),
                interrupted: prepared.interrupted,
            });
        let watch_task = Self::spawn_lease_loss_watch(
            &prepared.lease,
            &prepared.execution_abort,
            &prepared.activation.session_id,
        );
        let turn = super::session_turn::SessionTurn {
            worker: super::session_turn::TurnWorker::from(self),
            activation: prepared.activation.clone(),
            lease: prepared.lease.clone(),
            abort_signal: prepared.execution_abort.clone(),
            hitl_decision_rx: prepared.hitl_decision_rx,
            per_session: prepared.per_session.clone(),
            backend: prepared.backend.clone(),
            event_sink: prepared.event_sink,
            after_seq_observer: prepared.after_seq_observer,
            pending_input: prepared.pending_input,
            pending_compaction: prepared.pending_compaction,
            prepare_turn_entries: None,
            agent_setup: prepared.agent_setup,
        };
        RunningSessionRuntime {
            activation: prepared.activation,
            lease: prepared.lease,
            execution_abort: prepared.execution_abort,
            abort_relay: prepared.abort_relay,
            shutdown_relay: prepared.shutdown_relay,
            control_task: prepared.control_task,
            execution,
            shutdown: prepared.shutdown,
            per_session: prepared.per_session,
            backend: prepared.backend,
            session_watcher: prepared.session_watcher,

            watch_task,
            turn: Some(tokio::spawn(Box::pin(turn.run()))),
        }
    }

    async fn record_execution_error(
        &self,
        running: &RunningSessionRuntime,
        result: &Result<bool>,
    ) -> bool {
        let Some(error) = result.as_ref().err() else {
            return false;
        };
        if error
            .downcast_ref::<crate::nats_session_metadata::run_limits::DeadlineExpired>()
            .is_some()
        {
            let record = running.per_session.read().run_context.clone();
            if let Some(record) = record {
                if let Ok(entries) = running.backend.load_events_latest_async().await {
                    if let Ok(Some(prompt)) = self
                        .session_metadata
                        .invocation_prompt_seq(
                            &running.activation.session_id,
                            record.invocation_id.as_str(),
                            &entries,
                        )
                        .await
                    {
                        let request = crate::nats_session::InterruptRequest {
                            session_id: running.activation.session_id.clone(),
                            cluster: self.cluster.clone(),
                            replicas: self.lease.replicas,
                            cancellation_id: format!("deadline:{}", record.invocation_id.as_str()),
                            reason: "autonomous invocation deadline expired".into(),
                            requested_by: crate::TimeoutTerminal::from_record(&record)
                                .expect("finite worker deadline")
                                .message(),
                        };
                        match crate::nats_session::interrupt::interrupt_invocation(
                            &self.jetstream,
                            &self.client,
                            &self.activation_route,
                            request,
                            prompt,
                        )
                        .await
                        {
                            Ok(outcome) if outcome.cancel_seq().is_some() => {
                                running.execution_abort.set_ctrlc()
                            }
                            Ok(_) => {}
                            Err(error) => {
                                running.execution_abort.set_failover();
                                log::error!("deadline cancellation unconfirmed: {error:#}");
                            }
                        }
                    }
                }
            }
            return false;
        }

        matches!(
            Self::record_session_error(&running.backend, &running.lease, error).await,
            Ok(true)
        )
    }
    async fn complete_session_execution(
        &self,
        running: RunningSessionRuntime,
        outcome: TurnBodyOutcome,
    ) -> Result<FinishCause> {
        let TurnBodyOutcome {
            result,
            cleanup_turn,
            failover_cause,
        } = outcome;
        let settled = *result.as_ref().unwrap_or(&true);
        if let Some(interrupted) = result.as_ref().err().and_then(cancel_that_interrupted) {
            log::info!(
                "turn append lost to a Cancel; aborting instead of recording an error: \
                 session_id={} cancel_seq={}",
                running.activation.session_id,
                interrupted.cancel_seq
            );
            running.execution_abort.set_ctrlc();
        }
        let durable_error_recorded = self.record_execution_error(&running, &result).await;
        if !running.lease.is_held() {
            log::warn!(
                "session execution ended after failover: session_id={} worker_id={} revision={}",
                running.activation.session_id,
                running.lease.worker_id(),
                running.lease.fence_token()
            );
        }
        if !running.execution_abort.aborted() {
            Self::wait_for_post_turn_maintenance(&running.per_session, &running.lease).await;
        }
        Self::stop_execution_tasks([
            running.watch_task,
            running.control_task,
            running.session_watcher,
            running.abort_relay,
            running.shutdown_relay,
        ])
        .await;
        let finished = Self::finish_execution(
            &running.execution,
            &running.backend,
            &running.lease,
            FinishExecutionInputs {
                result,
                cleanup_turn,
                settled,
                failover_cause,
                config: running.per_session,
                execution_abort: running.execution_abort,
                shutdown: running.shutdown,
            },
        )
        .await;
        if durable_error_recorded {
            finished.map_err(|error| anyhow::Error::new(DurableActivationError::new(error)))
        } else {
            finished
        }
    }

    fn spawn_execution_abort_relays(
        user_abort: crate::utils::AbortSignal,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> (crate::utils::AbortSignal, JoinHandle<()>, JoinHandle<()>) {
        let execution_abort = crate::utils::create_abort_signal();
        let abort_relay = {
            let execution_abort = execution_abort.clone();
            tokio::spawn(async move {
                crate::utils::wait_abort_signal(&user_abort).await;
                execution_abort.set_ctrlc();
            })
        };
        let shutdown_relay = {
            let execution_abort = execution_abort.clone();
            tokio::spawn(async move {
                shutdown.cancelled().await;
                execution_abort.set_failover();
            })
        };
        (execution_abort, abort_relay, shutdown_relay)
    }

    fn configure_tool_confirmation(
        &self,
        config: &crate::config::GlobalConfig,
        subject: Option<&String>,
        session_id: &str,
        abort: &crate::utils::AbortSignal,
    ) {
        let confirm: Arc<crate::ConfirmToolUseFn> = match subject {
            Some(subject) => crate::nats_tool_confirmation::nats_confirm_tool_use(
                self.client.clone(),
                subject.clone(),
                session_id.to_string(),
                abort.clone(),
            ),
            None => Arc::new(|_call, _arguments, _reason| crate::tool::ToolUseConfirmation::Defer),
        };
        config.write().set_tui_confirm_tool_use(Some(confirm));
    }

    async fn await_turn_body(
        mut turn: JoinHandle<Result<bool>>,
        abort: &crate::utils::AbortSignal,
        shutdown: &tokio_util::sync::CancellationToken,
    ) -> TurnBodyOutcome {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                abort.set_failover();
                turn.abort();
                TurnBodyOutcome {
                    result: Ok(true),
                    cleanup_turn: Some(turn),
                    failover_cause: Some(FailoverCause::Shutdown),
                }
            }
            _ = harnx_core::abort::wait_abort_signal(abort) => {
                turn.abort();
                TurnBodyOutcome {
                    result: Ok(true),
                    cleanup_turn: Some(turn),
                    failover_cause: None,
                }
            }
            result = &mut turn => TurnBodyOutcome {
                result: result.unwrap_or_else(|error| Err(error.into())),
                cleanup_turn: None,
                failover_cause: None,
            },
        }
    }

    async fn stop_execution_tasks<const N: usize>(tasks: [JoinHandle<()>; N]) {
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
    }

    async fn finish_execution(
        execution: &WorkerExecution,
        backend: &NatsSessionLogBackend,
        lease: &Arc<NatsSessionLease>,
        inputs: FinishExecutionInputs,
    ) -> Result<FinishCause> {
        let FinishExecutionInputs {
            result,
            cleanup_turn,
            settled,
            failover_cause,
            config,
            execution_abort,
            shutdown,
        } = inputs;
        let failover_cause =
            failover_cause.or_else(|| shutdown.is_cancelled().then_some(FailoverCause::Shutdown));
        let turn = match failover_cause {
            Some(cause) => FinishedTurn::for_failover(cleanup_turn, config, cause),
            None => FinishedTurn {
                task: cleanup_turn,
                settled,
                config,
                failover_cause: None,
            },
        };
        let cause = execution.finish(backend, lease, turn).await?;
        if shutdown.is_cancelled() && matches!(cause, FinishCause::Completed { .. }) {
            return Ok(FinishCause::Failover(FailoverCause::Shutdown));
        }
        if execution_abort.aborted() {
            Ok(cause)
        } else {
            result.map(|_| cause)
        }
    }

    pub(super) async fn wait_for_post_turn_maintenance(
        config: &crate::config::GlobalConfig,
        lease: &NatsSessionLease,
    ) {
        while lease.is_held() {
            if config.try_read().is_some_and(|guard| {
                guard
                    .maintenance_abort
                    .as_ref()
                    .is_some_and(|abort| abort.aborted())
            }) {
                break;
            }
            let pending = crate::config::Config::session_maintenance_pending(config, |session| {
                session.compressing() || session.titling()
            });
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
    ) -> Result<bool> {
        if interrupted_by_cancel(error) {
            log::info!(
                "turn ended in an interruption, not a failure: session_id={} error={error:#}",
                backend.session_id()
            );
            return Ok(false);
        }
        if !should_append_control_log_entry(lease) {
            return Ok(false);
        }
        let entry = harnx_core::session::SessionLogEntry::Error {
            message: format!("{error:#}"),
            fence_token: lease.fence_token(),
            timestamp: Some(chrono::Utc::now()),
        };
        match backend.append_event(&entry).await {
            Ok(_) => Ok(true),
            Err(append_error) => {
                log::warn!(
                    "failed to append Error entry: session_id={} err={append_error:#}",
                    backend.session_id()
                );
                Err(append_error)
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
                    abort_for_watch.set_failover();
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
        assert!(
            !WorkerRuntime::record_session_error(&backend, &lease, &interrupted)
                .await
                .unwrap()
        );

        // Read through the log itself: the structural guard over this file's
        // family counts the worker's leader-authoritative decision points.
        let log = crate::nats_session_log::NatsSessionLog::new(jetstream.clone(), &storage_key);
        assert!(
            !log.load_events_latest_async()
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
        assert!(WorkerRuntime::record_session_error(
            &backend,
            &lease,
            &anyhow::anyhow!("model exploded")
        )
        .await
        .unwrap());
        assert!(log.load_events_latest_async().await.unwrap().iter().any(|(_, entry)| matches!(
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
