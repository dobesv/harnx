use super::*;
use harnx_execution_control::{CancelDisposition, Operation, OperationState};
use std::time::Duration;

fn terminal_cancellation_covers(operation: Option<&Operation>, seq: u64) -> bool {
    operation.is_some_and(|operation| {
        operation.state == OperationState::Cancelled
            && operation.cancel_recorded
            && operation
                .admissions
                .values()
                .any(|admitted| *admitted == Some(seq))
    })
}

fn pending_prompt_is_covered(
    entries: &[(u64, SessionLogEntry)],
    previous: Option<&Operation>,
    seq: u64,
) -> Result<bool> {
    Ok(
        requested_seq_status(entries, seq)? == RequestedSeqStatus::Covered
            || terminal_cancellation_covers(previous, seq),
    )
}

pub(crate) async fn resolve_pending_execution(
    store: &ExecutionStore,
    jetstream: &jetstream::Context,
    session_id: &str,
) -> Result<Option<Operation>> {
    let previous = store.current(session_id).await?;
    let log = NatsSessionLog::new(jetstream.clone(), session_id);
    restore_selected_stop(&log, store, previous.as_ref()).await?;
    let entries = log.load_events_latest_async().await?;
    let pending = entries.iter().rev().find_map(|(seq, entry)| match entry {
        SessionLogEntry::Message { id, role, .. } if role.is_user() => Some((*seq, id.clone())),
        _ => None,
    });
    let Some((seq, message_id)) = pending else {
        // Control can target an existing owner before its first transcript entry.
        // This never creates a generation or adopts a prompt.
        return Ok(previous.filter(|operation| !operation.state.is_terminal()));
    };
    // An inherited cancellation can stop an ownerless child before a worker
    // has a fence with which to append a transcript Cancel entry. Its terminal
    // operation is nevertheless authoritative for prompts admitted to that
    // exact generation; do not replay one into a replacement execution.
    if pending_prompt_is_covered(&entries, previous.as_ref(), seq)? {
        return Ok(None);
    }
    let history = store.recovery_history(session_id).await?;
    let owner =
        crate::nats_session_log::recovery::prompt_owner(&history, message_id.as_deref(), seq)?
            .with_context(|| {
                format!(
            "unknown pending prompt generation; automatic adoption refused: session={} seq={seq}",
            session_id
        )
            })?;
    resolve_owned_prompt(store, &log, owner, previous).await
}

async fn restore_selected_stop(
    log: &NatsSessionLog,
    store: &ExecutionStore,
    previous: Option<&Operation>,
) -> Result<()> {
    if let Some(operation) = previous.filter(|op| op.gate_registration.is_some()) {
        log.recover_stop(store, &operation.reference).await?;
    }
    Ok(())
}

async fn resolve_owned_prompt(
    store: &ExecutionStore,
    log: &NatsSessionLog,
    owner: &harnx_execution_control::RecoveryHistory,
    previous: Option<Operation>,
) -> Result<Option<Operation>> {
    if pending_owner_stopped(log, store, owner).await? {
        return Ok(previous.filter(|operation| {
            operation.reference == owner.reference && !operation.state.is_terminal()
        }));
    }
    let operation = store
        .get(&owner.reference)
        .await?
        .context("pending prompt owner retired")?;
    // Physical cancellation can win before the gate marker exists, or after
    // the history snapshot above. Return only the original cleanup owner; a
    // closed record never justifies installing another generation.
    if operation.state.is_terminal() {
        return Ok(None);
    }
    Ok(Some(operation))
}

async fn pending_owner_stopped(
    log: &NatsSessionLog,
    store: &ExecutionStore,
    owner: &harnx_execution_control::RecoveryHistory,
) -> Result<bool> {
    if owner.gate_registration.is_some()
        && log.recover_stop(store, &owner.reference).await?.is_some()
    {
        return Ok(true);
    }
    store.is_stop_fenced(&owner.reference).await
}

impl NatsSession {
    pub(super) async fn reconcile_previous_stop(&self) -> Result<()> {
        let Some(previous) = self
            .execution_store
            .current(&self.storage_key)
            .await?
            .filter(|operation| operation.gate_registration.is_some())
        else {
            return Ok(());
        };
        NatsSessionLog::new(self.jetstream.clone(), &self.storage_key)
            .recover_stop(&self.execution_store, &previous.reference)
            .await?;
        Ok(())
    }

    pub fn execution_store(&self) -> &ExecutionStore {
        &self.execution_store
    }

    pub fn with_execution_parent(mut self, parent: OperationRef, invocation_id: String) -> Self {
        self.execution_parent = Some(parent);
        self.invocation_id = Some(invocation_id);
        self
    }

    /// A sub-agent follower can only interrupt the invocation it was created for.
    /// Never turn delayed G1 cleanup into a request against a session's current G2.
    pub(crate) async fn request_invocation_cancel(&self) -> Result<CancelReceipt> {
        if let Some(cancel) = &self.parent_cancel {
            cancel.cancel();
        }
        self.abort_signal.set_ctrlc();
        self.request_cancel(CancelRequest {
            expected_execution_id: self.invocation_id.clone(),
            retry: false,
        })
        .await
    }

    /// Acceptance is the KV CAS, bounded independently of execution shutdown.
    pub async fn request_cancel(&self, request: CancelRequest) -> Result<CancelReceipt> {
        // A generation-scoped request may come from an old child row. Only
        // root interruption can abort this frontend before checking the ID.
        if request.expected_execution_id.is_none() {
            self.abort_signal.set_ctrlc();
        }
        // Prompt admission already binds the selected execution. Recovery and
        // transcript projection must not delay (or invalidate) a root receipt.
        let receipt = tokio::time::timeout(
            Duration::from_secs(2),
            self.execution_store
                .request_cancel(&self.storage_key, request),
        )
        .await
        .context("timed out persisting cancellation request; retry to reconcile")??;
        if receipt.cancelled {
            self.abort_signal.set_ctrlc();
            self.wake_cancelled_generation(receipt.clone());
        }
        Ok(receipt)
    }

    fn wake_cancelled_generation(&self, receipt: CancelReceipt) {
        let session = self.clone();
        harnx_execution_control::CleanupTasks::process().spawn(async move {
            let Some(execution_id) = receipt.execution_id else {
                return;
            };
            let reference = OperationRef::new(&session.storage_key, &execution_id);
            let wake = async {
                let operation = session
                    .execution_store
                    .get(&reference)
                    .await?
                    .context("cancelled execution missing")?;
                let through = operation
                    .admissions
                    .values()
                    .filter_map(|seq| *seq)
                    .max()
                    .unwrap_or(0);
                session
                    .publish_control_activation((&execution_id, through), None, None)
                    .await
            };
            let notify = async {
                if let Some(cancellation_id) = receipt.cancellation_id {
                    let command = ControlCommand::CancelExecution {
                        execution_id: execution_id.clone(),
                        cancellation_id,
                    };
                    let _ =
                        publish_control_command(&session.client, &session.storage_key, &command)
                            .await;
                }
            };
            // A slow recovery activation must not hold the live owner's stop
            // hint behind it. The durable watch recovers either lost wakeup.
            let (activation, ()) =
                tokio::join!(tokio::time::timeout(Duration::from_secs(2), wake), notify);
            if let Err(error) = activation {
                log::warn!("cancellation recovery activation timed out: {error}");
            }
        });
    }

    pub async fn cancel_status(&self, receipt: &CancelReceipt) -> Result<CancellationStatus> {
        let Some(execution_id) = receipt.execution_id.as_ref() else {
            return Ok(receipt.clone());
        };
        let reference = OperationRef::new(&self.storage_key, execution_id);
        let operation = self
            .execution_store
            .get(&reference)
            .await?
            .context("cancelled execution record missing")?;
        self.reconcile_cancelled_owner(&operation).await?;
        let operation = self.execution_store.status(&reference).await?;
        Ok(CancelReceipt::from_operation(&operation, false))
    }

    /// Explicitly abandon an unconfirmed cancellation. This permits a new
    /// execution even though an owner that disappeared without acknowledging
    /// cleanup may still be running. Late control-plane updates are rejected by
    /// the now-terminal operation records.
    pub async fn abandon_unconfirmed_cancellation(
        &self,
        expected_execution_id: &str,
    ) -> Result<CancelReceipt> {
        self.execution_store
            .abandon_unconfirmed(&self.storage_key, expected_execution_id)
            .await
    }

    async fn reconcile_cancelled_owner(&self, operation: &Operation) -> Result<()> {
        // Gated executions require owner evidence from the cleanup supervisor.
        if operation.gate_registration.is_some() {
            return Ok(());
        }
        if !operation.state.cancelling() || operation.owner.is_none() {
            return Ok(());
        }
        let entries = self.load_durable_entries().await?;
        reconcile_admissions(&self.execution_store, operation, &entries).await?;
        let Some(cancel_seq) = entries.iter().rev().find_map(|(seq, entry)| match entry {
            SessionLogEntry::Cancel { fence_token }
                if *fence_token >= operation.owner.as_ref().unwrap().fence =>
            {
                Some(*seq)
            }
            _ => None,
        }) else {
            return Ok(());
        };
        let operation = self
            .execution_store
            .get(&operation.reference)
            .await?
            .context("execution missing")?;
        if !operation.admissions_covered(cancel_seq) {
            return Ok(());
        }
        if crate::nats_lease::session_has_active_lease(&self.jetstream, &self.storage_key).await? {
            return Ok(());
        }
        self.execution_store
            .mutate(&operation.reference, |latest| {
                anyhow::ensure!(
                    latest.owner == operation.owner,
                    "execution owner changed during reconciliation"
                );
                latest.owner_stopped = true;
                latest.covered_through = latest.covered_through.max(cancel_seq);
                latest.cancel_recorded = true;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub async fn wait_for_cancel(
        &self,
        receipt: &CancelReceipt,
        deadline: tokio::time::Instant,
    ) -> Result<CancellationStatus> {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;
            let status = self.cancel_status(receipt).await?;
            if matches!(
                status.disposition,
                CancelDisposition::Idle
                    | CancelDisposition::Cancelled
                    | CancelDisposition::Unconfirmed
            ) {
                return Ok(status);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(status);
            }
        }
    }

    /// Return on durable acceptance. Physical cleanup is reported separately by
    /// `cancel_status` / `wait_for_cancel` and never delays a caller's return.
    pub async fn cancel_pending_turn(&self) -> Result<bool> {
        Ok(self
            .request_cancel(CancelRequest::default())
            .await?
            .cancelled)
    }

    /// Observe the admitted generation, not whichever generation is current when
    /// an update arrives. Retained gate stops survive pruning and replacement.
    pub(super) async fn wait_for_root_stop(&self, execution_id: &str) -> Result<()> {
        let reference = OperationRef::new(&self.storage_key, execution_id);
        let mut updates = self.execution_store.watch().await?;
        loop {
            if let Some(stop) = self.execution_store.accepted_stop(&reference).await? {
                log::info!("nats session: root interruption accepted: execution={} cancellation={} reason={}",
                    execution_id, stop.decision.cancellation_id, stop.decision.reason);
                return Ok(());
            }
            updates.next().await.context("root stop watch closed")??;
        }
    }
}

pub(crate) async fn reconcile_admissions(
    store: &ExecutionStore,
    operation: &Operation,
    entries: &[(u64, SessionLogEntry)],
) -> Result<()> {
    for (seq, entry) in entries {
        if let SessionLogEntry::Message { id: Some(id), .. } = entry {
            if operation.admissions.get(id).is_some_and(Option::is_none) {
                store.commit_prompt(&operation.reference, id, *seq).await?;
            }
        }
    }
    Ok(())
}
