//! Worker-lifetime reconciliation. Gate stops, not turn tasks or wake-ups, are the queue.
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use harnx_execution_control::{
    CleanupScope, CleanupState, CleanupStatus, ExecutionContext, ExecutionStore, Operation,
    OperationKind, OperationRef,
};
use harnx_toolset::{CancelAcceptance, ControlMessage};
use harnx_toolset_server::invocation_journal::InvocationJournal;
use std::{collections::BTreeMap, time::Duration};
use tokio_util::task::AbortOnDropHandle;

#[cfg(test)]
mod tests;

const SCAN_INTERVAL: Duration = Duration::from_secs(1);
const CLEANUP_BUDGET: Duration = Duration::from_secs(5);

pub(super) struct CleanupSupervisor(#[allow(dead_code)] AbortOnDropHandle<()>);

#[derive(Clone)]
struct Reconciler {
    store: ExecutionStore,
    journal: InvocationJournal,
    client: async_nats::Client,
}

impl CleanupSupervisor {
    pub async fn start(js: &async_nats::jetstream::Context, replicas: usize) -> Result<Self> {
        let reconciler = Reconciler {
            store: ExecutionStore::ensure(js, replicas).await?,
            journal: InvocationJournal::ensure(js).await?,
            client: js.client().clone(),
        };
        Ok(Self(AbortOnDropHandle::new(tokio::spawn(reconciler.run()))))
    }
}

impl Reconciler {
    async fn run(self) {
        let mut retries = BTreeMap::<OperationRef, (tokio::time::Instant, Duration)>::new();
        let mut ticks = tokio::time::interval(SCAN_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // Reopen watches after broker restart. Startup and periodic scans
            // recover a stop committed before a crash or a lost notification.
            let mut watch = match self.store.watch().await {
                Ok(watch) => watch,
                Err(error) => {
                    log::warn!("cleanup watch unavailable: {error:#}");
                    ticks.tick().await;
                    continue;
                }
            };
            self.run_watch(&mut watch, &mut ticks, &mut retries).await;
        }
    }

    async fn run_watch(
        &self,
        watch: &mut harnx_nats_common::recovery::KvUpdates,
        ticks: &mut tokio::time::Interval,
        retries: &mut BTreeMap<OperationRef, (tokio::time::Instant, Duration)>,
    ) {
        loop {
            if let Err(error) = self.scan(retries).await {
                log::warn!("cleanup scan incomplete; retrying: {error:#}");
            }
            tokio::select! {
                _ = ticks.tick() => {},
                update = watch.next() => if !matches!(update, Some(Ok(_))) { break; },
            }
            // Coalesce busy gate watches. Never flood owner RPCs under load.
            ticks.tick().await;
        }
    }

    async fn scan(
        &self,
        retries: &mut BTreeMap<OperationRef, (tokio::time::Instant, Duration)>,
    ) -> Result<()> {
        let scopes = self.store.cleanup_scopes().await?;
        retries.retain(|reference, _| {
            scopes
                .iter()
                .any(|scope| scope.context.operation() == reference)
        });
        for scope in scopes {
            let now = tokio::time::Instant::now();
            let send = retries
                .get(scope.context.operation())
                .is_none_or(|(next, _)| *next <= now);
            let result = self.reconcile(&scope, Utc::now(), send).await;
            if let Err(error) = result {
                log::debug!("cleanup remains unresolved: {error:#}");
            }
            if send {
                let delay = retries
                    .get(scope.context.operation())
                    .map_or(Duration::from_millis(250), |(_, delay)| {
                        (*delay * 2).min(Duration::from_secs(30))
                    });
                retries.insert(scope.context.operation().clone(), (now + delay, delay));
            }
        }
        Ok(())
    }

    async fn reconcile(&self, scope: &CleanupScope, now: DateTime<Utc>, send: bool) -> Result<()> {
        let result = self.reconcile_tree(scope, send).await;
        let mut cleanup = match result {
            Ok(operation) => operation.cleanup_status(),
            Err(error) => CleanupStatus::unconfirmed(format!(
                "cleanup metadata/evidence unavailable: {error:#}"
            )),
        };
        if cleanup.state != CleanupState::Confirmed
            && now
                .signed_duration_since(scope.stop.decision.accepted_at)
                .to_std()
                .unwrap_or_default()
                >= CLEANUP_BUDGET
        {
            cleanup.state = CleanupState::Unconfirmed;
            cleanup.last_error.get_or_insert_with(|| {
                "resource owner or descendants have not confirmed cleanup within budget".into()
            });
        }
        self.store.record_cleanup(&scope.context, cleanup).await
    }

    async fn reconcile_tree(&self, scope: &CleanupScope, send: bool) -> Result<Operation> {
        let mut pending = vec![scope.context.operation().clone()];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(reference) = pending.pop() {
            anyhow::ensure!(seen.insert(reference.clone()), "cleanup graph cycle");
            let operation = self
                .store
                .get(&reference)
                .await?
                .context("physical operation metadata missing")?;
            self.request_cleanup(scope, &operation, send).await?;
            pending.extend(operation.children);
        }
        // Legacy physical status is a projection; the CleanupUpdate below is
        // durable and remains addressable after pruning or G2 replacement.
        self.store.status(scope.context.operation()).await
    }

    async fn request_cleanup(
        &self,
        scope: &CleanupScope,
        operation: &Operation,
        send: bool,
    ) -> Result<()> {
        if operation.cleanup_confirmed() {
            return Ok(());
        }
        self.store
            .cancel_operation(&operation.reference, None, false)
            .await?;
        if !send {
            return Ok(());
        }
        if let Err(error) = self
            .wake_owner(
                &scope.context,
                operation,
                &scope.stop.decision.cancellation_id,
            )
            .await
        {
            log::debug!("cleanup owner wake-up unconfirmed: {error:#}");
        }
        Ok(())
    }

    async fn wake_owner(
        &self,
        root: &ExecutionContext,
        operation: &Operation,
        id: &str,
    ) -> Result<()> {
        if operation.owner_stopped {
            return Ok(());
        }
        if operation.kind == OperationKind::Session {
            let command = super::ControlCommand::CancelExecution {
                execution_id: operation.reference.execution_id.clone(),
                cancellation_id: id.into(),
            };
            let subject = super::control_subject(&operation.reference.session_id);
            self.client
                .publish(subject, command.to_bytes()?.into())
                .await?;
            return Ok(());
        }
        let Some(record) = self
            .journal
            .recorded(
                &operation.reference.session_id,
                &operation.reference.execution_id,
            )
            .await?
        else {
            // Controlled hooks have their own durable watch. Missing tool routing
            // data cannot establish that an operation never ran.
            anyhow::bail!("cleanup route unavailable");
        };
        let context = self
            .store
            .gate_context(root.gate_root(), &operation.reference)
            .await?;
        let control = ControlMessage::cancel(context, record.server, id.into());
        let subject =
            harnx_core::instance::ServerScope::from_string(record.server_scope).control_subject();
        let ack = harnx_toolset_server::cancellation_client::request_cancellation(
            &self.client,
            subject,
            &control,
            Duration::from_millis(250),
        )
        .await;
        match ack.acceptance {
            CancelAcceptance::Accepted { .. } | CancelAcceptance::AlreadyFinished => Ok(()),
            CancelAcceptance::Rejected { reason } | CancelAcceptance::Unknown { reason } => {
                anyhow::bail!("cleanup request acceptance unresolved: {reason}")
            }
        }
        // Owners record their own evidence via CleanupUpdate. Never convert an
        // acknowledgement, missing process, or missing lease to owner_stopped.
    }
}

/// Keep shared tool servers alive for unconfirmed work, without holding a lease
/// or an active session slot. G1 only releases its own server-user identity.
pub(super) fn release_server_claim_when_clean(
    reconciler: Option<std::sync::Arc<super::server_reconciler::ServerReconciler>>,
    execution: super::execution_control::WorkerExecution,
) {
    let Some(reconciler) = reconciler else {
        return;
    };
    harnx_execution_control::CleanupTasks::process().spawn(async move {
        loop {
            let confirmed = async {
                if let Some(fence) = &execution.fence {
                    if execution.store.gate_cleanup(&fence.context).await?.state
                        == CleanupState::Confirmed
                    {
                        return Ok(true);
                    }
                }
                Ok::<_, anyhow::Error>(
                    execution
                        .store
                        .status(&execution.reference)
                        .await?
                        .cleanup_confirmed(),
                )
            }
            .await;
            if matches!(confirmed, Ok(true)) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        reconciler
            .session_ended(&format!(
                "{}/{}",
                execution.reference.session_id, execution.reference.execution_id
            ))
            .await;
    });
}
