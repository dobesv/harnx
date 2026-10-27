//! Reply retries consult durable mappings and then inline intent before grammar.
use super::*;

impl HarnxHandler {
    async fn retry_initial_message(
        &self,
        owner: &RequestIdentity,
        request: &MessageAdmission,
    ) -> Result<Option<Admission>, A2AError> {
        let Some(record) = self.dedupe(owner, request).await? else {
            return Ok(None);
        };
        let reconciled = self.reconcile(owner, record).await?;
        Ok(Some(Admission::retry(reconciled)))
    }

    pub(super) async fn lookup_reply_retry(
        &self,
        owner: &RequestIdentity,
        request: &MessageAdmission,
        lookup: ReplyLookup<'_>,
    ) -> Result<Option<Admission>, A2AError> {
        let mapping = self
            .backend
            .store
            .get_reply_dedupe(lookup.storage, &request.message.message_id)
            .await
            .map_err(map_error)?;
        let inline = inline_mapping(lookup.active, &request.message.message_id);
        let Some(mapping) = mapping.or(inline) else {
            // Initial-message retries also win over reply parsing.
            return self.retry_initial_message(owner, request).await;
        };
        validate_mapping(&mapping, request)?;
        if unresolved_intent(lookup.active, &mapping.task_id, &request.message.message_id) {
            return Ok(None);
        }
        let record = self.task(owner, &mapping.task_id).await?;
        Ok(Some(Admission::reply_snapshot(record)))
    }
}

fn inline_mapping(active: Option<&ActiveTask>, message_id: &str) -> Option<DedupeEntry> {
    let active = active?;
    active
        .hitl_checkpoint
        .as_ref()?
        .reply
        .as_ref()
        .filter(|r| r.message_id == message_id)
        .map(|r| DedupeEntry::new(&active.snapshot.task.id, &r.fingerprint))
}

fn validate_mapping(mapping: &DedupeEntry, request: &MessageAdmission) -> Result<(), A2AError> {
    if mapping.fingerprint != request.fingerprint {
        return Err(map_error(StoreError::FingerprintMismatch.into()));
    }
    if request
        .message
        .task_id
        .as_ref()
        .is_some_and(|target| target != &mapping.task_id)
    {
        return Err(A2AError::invalid_params(
            "reply messageId belongs to a different task",
        ));
    }
    Ok(())
}

fn unresolved_intent(active: Option<&ActiveTask>, task_id: &str, message_id: &str) -> bool {
    let Some(active) = active else {
        return false;
    };
    if active.snapshot.task.id != task_id || active.snapshot.task.status.state.is_terminal() {
        return false;
    }
    let Some(checkpoint) = &active.hitl_checkpoint else {
        return false;
    };
    checkpoint.phase == HitlPhase::DecisionIntent
        && checkpoint
            .reply
            .as_ref()
            .is_some_and(|r| r.message_id == message_id)
}
