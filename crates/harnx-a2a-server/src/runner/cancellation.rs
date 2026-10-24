//! Cancellation success requires terminal authority, not a local notification.
//!
//! CancelTask persists an immutable intent to the context document and polls for
//! terminal state. The 10-second wait may timeout before owner-settlement completes;
//! callers receive `"A2A cancellation unconfirmed; retry the same task"` and must
//! poll or retry the same task ID until terminal. Intent remains durable and owner
//! settlement continues in the background. There is no failover SLO and no automatic
//! replay.
use super::*;

impl Runner {
    /// Requests task cancellation, records immutable intent, and awaits terminal state.
    ///
    /// Returns the terminal task record on success. If the 10-second confirmation
    /// wait times out, returns an error requiring retry of the same `task_id`.
    /// The cancel intent persists durably and background supervision eventually
    /// settles it regardless of this call's outcome.
    pub async fn cancel_task(
        &self,
        export: &Export,
        owner: &RequestIdentity,
        task_id: &str,
    ) -> Result<TaskRecord> {
        let record = self
            .store
            .get_task_for_export(export, owner, task_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        if record.task.status.state.is_terminal() {
            return Ok(record);
        }
        let storage =
            harnx_core::session_identity::session_key(Some(&export.agent), &record.task.context_id);
        match self.store.request_task_cancel(&storage, task_id).await {
            Ok(()) => (),
            Err(error) => {
                // T1 may have retired while its intent competed with a T2 claim.
                let latest = self
                    .store
                    .get_task_for_export(export, owner, task_id)
                    .await?
                    .ok_or(StoreError::NotFound)?;
                if latest.task.status.state.is_terminal() {
                    return Ok(latest);
                }
                return Err(error);
            }
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let latest = self
                    .store
                    .get_task_for_export(export, owner, task_id)
                    .await?
                    .ok_or(StoreError::NotFound)?;
                if latest.task.status.state.is_terminal() {
                    return Ok(latest);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("A2A cancellation unconfirmed; retry the same task")?
    }
}

impl Publisher {
    pub(super) async fn observe_cancel(&self, session: &NatsSession) -> Result<()> {
        let current = self
            .store
            .read_context(&self.storage_key)
            .await?
            .context("cancel authority missing")?;
        anyhow::ensure!(
            current.document.owner == self.authority.context.document.owner,
            "A2A owner superseded"
        );
        let active = current
            .document
            .state
            .active
            .as_ref()
            .context("cancel task missing")?;
        anyhow::ensure!(
            active.snapshot.task.id == self.record().task.id,
            "A2A task superseded"
        );
        if active.cancel.is_some() {
            session
                .interrupt_admitted_invocation("A2A cancellation")
                .await?;
        }
        Ok(())
    }
}

impl Runner {
    /// Parked tasks have no publisher to observe cancellation. Claim the scoped
    /// lease and use existing fixed-ticket stop/settlement rules.
    pub(crate) async fn cancel_hitl_task(
        &self,
        export: &Export,
        owner: &RequestIdentity,
        session: &NatsSession,
        task_id: &str,
    ) -> Result<TaskRecord> {
        self.authorize_session(export, owner, session).await?;
        if let Err(error) = self
            .store
            .request_task_cancel(session.storage_key(), task_id)
            .await
        {
            let latest = self
                .store
                .get_task(session.storage_key(), task_id)
                .await?
                .context("cancel task missing")?;
            // Completion/retirement can win while CancelTask is still using T1's
            // snapshot. Never apply its intent to a successor task.
            if latest.task.status.state.is_terminal() {
                return Ok(latest);
            }
            return Err(error);
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let record = self
                    .store
                    .get_task(session.storage_key(), task_id)
                    .await?
                    .context("cancel task missing")?;
                if record.task.status.state.is_terminal() {
                    return Ok(record);
                }
                if let Some(lease) = self.candidate_lease(session.storage_key()).await? {
                    let mut authority = self.claim_context(session, lease).await?;
                    let result = async {
                        let latest = self
                            .store
                            .get_task(session.storage_key(), task_id)
                            .await?
                            .context("cancel task missing")?;
                        if latest.task.status.state.is_terminal() {
                            return Ok(latest);
                        }
                        let active = authority
                            .context
                            .document
                            .state
                            .active
                            .as_ref()
                            .context("cancel task missing")?;
                        anyhow::ensure!(active.snapshot.task.id == task_id, RunnerError::Terminal);
                        anyhow::ensure!(active.cancel.is_some(), "cancel intent missing");
                        if let Some(reply) = active
                            .hitl_checkpoint
                            .as_ref()
                            .and_then(|c| c.reply.as_ref())
                        {
                            self.store
                                .put_reply_dedupe(
                                    session.storage_key(),
                                    &reply.message_id,
                                    task_id,
                                    &reply.fingerprint,
                                )
                                .await?;
                        }
                        self.recover_admission(session, &mut authority).await?;
                        self.store
                            .get_task(session.storage_key(), task_id)
                            .await?
                            .context("canceled task missing")
                    }
                    .await;
                    authority
                        .release(&self.store, session.storage_key())
                        .await?;
                    return result;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("A2A cancellation unconfirmed; retry the same task")?
    }
}
