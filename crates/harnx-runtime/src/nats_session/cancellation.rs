use super::*;
use harnx_execution_control::{CancelDisposition, Operation};
use std::time::Duration;

pub(crate) async fn resolve_pending_execution(
    store: &ExecutionStore,
    jetstream: &jetstream::Context,
    session_id: &str,
) -> Result<Option<Operation>> {
    if let Some(operation) = store.current(session_id).await? {
        if !operation.state.is_terminal() {
            return Ok(Some(operation));
        }
    }
    let entries = NatsSessionLog::new(jetstream.clone(), session_id)
        .load_events_latest_async()
        .await?;
    let pending = entries.iter().rev().find_map(|(seq, entry)| match entry {
        SessionLogEntry::Message { id, role, .. } if role.is_user() => Some((*seq, id.clone())),
        _ => None,
    });
    let Some((seq, message_id)) = pending else {
        return Ok(None);
    };
    if requested_seq_status(&entries, seq)? == RequestedSeqStatus::Covered {
        return Ok(None);
    }
    let operation = store.session(session_id, None, None).await?;
    let message_id = message_id.unwrap_or_else(|| format!("legacy-{seq}"));
    store
        .reserve_prompt(&operation.reference, &message_id)
        .await?;
    store
        .commit_prompt(&operation.reference, &message_id, seq)
        .await?;
    Ok(Some(operation))
}

impl NatsSession {
    pub fn execution_store(&self) -> &ExecutionStore {
        &self.execution_store
    }

    pub fn with_execution_parent(mut self, parent: OperationRef, invocation_id: String) -> Self {
        self.execution_parent = Some(parent);
        self.invocation_id = Some(invocation_id);
        self
    }

    /// Acceptance is the KV CAS, bounded independently of execution shutdown.
    pub async fn request_cancel(&self, request: CancelRequest) -> Result<CancelReceipt> {
        // A generation-scoped request may come from an old child row. Only
        // root interruption can abort this frontend before checking the ID.
        if request.expected_execution_id.is_none() {
            self.abort_signal.set_ctrlc();
        }
        let receipt = tokio::time::timeout(Duration::from_secs(2), async {
            resolve_pending_execution(&self.execution_store, &self.jetstream, &self.session_id)
                .await?;
            let receipt = self
                .execution_store
                .request_cancel(&self.session_id, request)
                .await?;
            if receipt.cancelled {
                let operation = self
                    .execution_store
                    .current(&self.session_id)
                    .await?
                    .context("execution missing")?;
                let through = operation
                    .admissions
                    .values()
                    .filter_map(|seq| *seq)
                    .max()
                    .unwrap_or(0);
                self.publish_activation((&operation.reference.execution_id, through), None, None)
                    .await?;
            }
            Ok::<_, anyhow::Error>(receipt)
        })
        .await
        .context("timed out persisting cancellation request; retry to reconcile")??;
        if receipt.cancelled {
            self.abort_signal.set_ctrlc();
        }
        // The operation is already durable. Failure to publish a latency hint
        // must never turn accepted cancellation into a request-failed UI.
        if let (Some(execution_id), Some(cancellation_id)) =
            (&receipt.execution_id, &receipt.cancellation_id)
        {
            let command = ControlCommand::CancelExecution {
                execution_id: execution_id.clone(),
                cancellation_id: cancellation_id.clone(),
            };
            let client = self.client.clone();
            let session = self.session_id.clone();
            tokio::spawn(async move {
                let _ = publish_control_command(&client, &session, &command).await;
            });
        }
        Ok(receipt)
    }

    pub async fn cancel_status(&self, receipt: &CancelReceipt) -> Result<CancellationStatus> {
        let Some(execution_id) = receipt.execution_id.as_ref() else {
            return Ok(receipt.clone());
        };
        let reference = OperationRef::new(&self.session_id, execution_id);
        let operation = self
            .execution_store
            .get(&reference)
            .await?
            .context("cancelled execution record missing")?;
        self.reconcile_cancelled_owner(&operation).await?;
        let operation = self.execution_store.status(&reference).await?;
        Ok(CancelReceipt::from_operation(&operation, false))
    }

    async fn reconcile_cancelled_owner(&self, operation: &Operation) -> Result<()> {
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
        if crate::nats_lease::session_has_active_lease(&self.jetstream, &self.session_id).await? {
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

    /// Blocking compatibility wrapper. A durable request alone is not success.
    pub async fn cancel_pending_turn(&self) -> Result<bool> {
        let receipt = self.request_cancel(CancelRequest::default()).await?;
        let status = self
            .wait_for_cancel(
                &receipt,
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await?;
        match status.disposition {
            CancelDisposition::Idle => Ok(false),
            CancelDisposition::Cancelled => Ok(true),
            _ => anyhow::bail!(
                "cancellation unconfirmed for session '{}'; retry cancellation before prompting",
                self.session_id
            ),
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
