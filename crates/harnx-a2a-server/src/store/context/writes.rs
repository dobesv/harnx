use super::*;
use harnx_core::crypto::sha256;

impl ContextWrite {
    pub(super) fn new(
        storage_key: &str,
        predecessor: u64,
        mut document: ContextDocument,
        operation_id: &str,
    ) -> Result<Self> {
        ensure!(!operation_id.is_empty(), "missing context operation id");
        // Reusing an ID for another predecessor or payload must not be mistaken
        // for a recovered acknowledgement.
        ensure!(
            document.last_operation.id != operation_id,
            AuthorityError::OperationMismatch
        );
        document.last_operation = OperationReceipt {
            id: operation_id.into(),
            digest: String::new(),
            predecessor,
        };
        document.validate()?;
        document.last_operation.digest = document_digest(&document)?;
        Ok(Self {
            storage_key: storage_key.into(),
            predecessor,
            document,
        })
    }
}

fn document_digest(document: &ContextDocument) -> Result<String> {
    let mut value = serde_json::to_value(document)?;
    value["last_operation"]["digest"] = "".into();
    value.sort_all_objects();
    Ok(sha256(&serde_json::to_string(&value)?))
}

impl A2aStore {
    /// Owner/task/revision are checked on the document being mutated. A later
    /// takeover or competing update defeats this exact predecessor at commit.
    pub async fn prepare_context_update(
        &self,
        storage_key: &str,
        expected: &ContextVersion,
        operation_id: &str,
        mutate: impl FnOnce(&mut ContextState),
    ) -> Result<ContextWrite> {
        let current = self.owner_document(storage_key, expected).await?;
        self.verify_archive_projection(storage_key, &current.document.state)
            .await?;
        let mut next = current.document.clone();
        mutate(&mut next.state);
        self.validate_new_task(storage_key, &current.document.state, &next.state)
            .await?;
        validate_transition(&current.document.state, &mut next.state)?;
        self.verify_archive_projection(storage_key, &next.state)
            .await?;
        ContextWrite::new(storage_key, current.revision, next, operation_id)
    }

    /// Release ownership, never delete the authority or its epoch/active state.
    /// Pending active work remains recoverable by the next claimant.
    pub async fn prepare_context_release(
        &self,
        storage_key: &str,
        expected: &ContextVersion,
        operation_id: &str,
    ) -> Result<ContextWrite> {
        let mut current = self.owner_document(storage_key, expected).await?;
        current.document.owner = None;
        ContextWrite::new(
            storage_key,
            current.revision,
            current.document,
            operation_id,
        )
    }

    pub async fn commit_context(&self, write: &ContextWrite) -> Result<ContextSnapshot> {
        let payload = serde_json::to_vec(&write.document)?;
        let kv = self.store.kv_store();
        check_payload_budget(&payload, self.store.a2a_payload_limit().await?)?;
        let key = context_authority_key(&write.storage_key);
        // update(expected=0) is a strict create; unlike kv.create it cannot revive
        // a deleted epoch document by rebasing onto a tombstone revision.
        let result = kv
            .update(&key, payload.into(), write.predecessor)
            .await
            .map_err(anyhow::Error::from);
        #[cfg(feature = "fault-injection")]
        let result = self.context_ack_for_test(result).await;
        match result {
            Ok(revision) => Ok(ContextSnapshot {
                document: write.document.clone(),
                revision,
            }),
            Err(error) => self
                .resolve_context_write(write)
                .await
                .with_context(|| format!("context CAS acknowledgement unresolved: {error:#}")),
        }
    }

    /// Same receipt proves this exact write applied. If a later owner/operation
    /// superseded the receipt, report conflict instead of repeating side effects.
    pub async fn resolve_context_write(&self, write: &ContextWrite) -> Result<ContextSnapshot> {
        let current = self
            .read_context(&write.storage_key)
            .await?
            .ok_or(AuthorityError::Conflict)?;
        let receipt = &current.document.last_operation;
        ensure!(
            receipt.id == write.document.last_operation.id,
            AuthorityError::Conflict
        );
        ensure!(
            receipt == &write.document.last_operation
                && document_digest(&current.document)? == receipt.digest,
            AuthorityError::OperationMismatch
        );
        Ok(current)
    }

    #[cfg(feature = "fault-injection")]
    async fn context_ack_for_test(&self, result: Result<u64>) -> Result<u64> {
        if result.is_ok() {
            self.context_hooks
                .checkpoint(crate::fault_injection::Boundary::ContextCas)
                .await;
            if self.context_hooks.take_context_ack_loss() {
                anyhow::bail!("injected context acknowledgement loss");
            }
        }
        result
    }
}

fn check_payload_budget(payload: &[u8], limit: usize) -> Result<()> {
    // NATS max_payload includes publish headers. Leave room for KV CAS headers.
    let limit = limit.saturating_sub(1024);
    ensure!(
        payload.len() <= limit,
        "context authority exceeds NATS payload budget ({limit} bytes)"
    );
    Ok(())
}

fn validate_transition(old: &ContextState, next: &mut ContextState) -> Result<()> {
    let Some(previous) = &old.active else {
        return Ok(());
    };
    let same_task = next
        .active
        .as_ref()
        .is_some_and(|active| active.snapshot.task.id == previous.snapshot.task.id);
    if !same_task {
        ensure!(
            previous.ready_to_retire(),
            "active task is not durably settled/projected"
        );
        return Ok(());
    }
    let active = next.active.as_mut().expect("same task");
    ensure!(
        active.message == previous.message,
        "retained message identity is immutable"
    );
    ensure!(
        active.admission.invocation_id == previous.admission.invocation_id
            && active.admission.prompt_id == previous.admission.prompt_id
            && active.admission.fixed_predecessor == previous.admission.fixed_predecessor
            && active.admission.closure_id == previous.admission.closure_id,
        "admission ticket is immutable"
    );
    if let Some(cancel) = &previous.cancel {
        ensure!(
            active.cancel.as_ref() == Some(cancel),
            "cancel intent is immutable"
        );
    }
    validate_outbox(previous, active)?;
    validate_snapshot(previous, active)
}

fn validate_snapshot(previous: &ActiveTask, active: &mut ActiveTask) -> Result<()> {
    ensure!(
        active.snapshot.revision == previous.snapshot.revision
            && active.snapshot.created_at == previous.snapshot.created_at,
        "snapshot revision/creation time managed by authority"
    );
    if previous.snapshot.task.status.state.is_terminal() {
        ensure!(
            serde_json::to_value(&active.snapshot)? == serde_json::to_value(&previous.snapshot)?,
            "terminal snapshot is immutable"
        );
    } else if serde_json::to_value(&active.snapshot)? != serde_json::to_value(&previous.snapshot)? {
        active.snapshot.revision = previous
            .snapshot
            .revision
            .checked_add(1)
            .context("task revision overflow")?;
        active.snapshot.updated_at = chrono::Utc::now();
    }
    ensure!(
        active.publication.stream_seq >= previous.publication.stream_seq,
        "publication cursor moved backwards"
    );
    Ok(())
}

fn validate_outbox(previous: &ActiveTask, active: &ActiveTask) -> Result<()> {
    ensure!(
        active.publication.confirmed_history == checkpoint_history(previous, active),
        "event cleanup history changed without publication checkpoint"
    );
    if let Some(pending) = &previous.publication.pending {
        ensure!(
            serde_json::to_value(&active.snapshot)? == serde_json::to_value(&previous.snapshot)?,
            "snapshot changed before pending publication resolved"
        );
        ensure!(
            active.snapshot.stream_seq == previous.snapshot.stream_seq,
            "next event before pending publication resolved"
        );
        if active.publication.pending.is_none() {
            ensure!(
                active.publication.subject_sequence > pending.expected_subject_sequence,
                "pending publication cleared without a subject checkpoint"
            );
        }
        if let Some(next) = &active.publication.pending {
            ensure!(
                serde_json::to_value(next)? == serde_json::to_value(pending)?,
                "pending event is immutable"
            );
        }
    }
    if let Some(pending) = &active.publication.pending {
        ensure!(
            pending.task_sequence == active.publication.stream_seq,
            "pending event cursor mismatch"
        );
        ensure!(
            pending.expected_subject_sequence == active.publication.subject_sequence,
            "pending predecessor mismatch"
        );
    }
    Ok(())
}

fn checkpoint_history(previous: &ActiveTask, active: &ActiveTask) -> Vec<u64> {
    let mut history = previous.publication.confirmed_history.clone();
    if previous.publication.pending.is_some() && active.publication.pending.is_none() {
        history.push(active.publication.subject_sequence);
    }
    let discard = history
        .len()
        .saturating_sub(harnx_runtime::a2a_events::CHECKPOINT_HISTORY);
    drop(history.drain(..discard));
    history
}
