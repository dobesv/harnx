//! Parked tasks need scoped ownership before applying their saved cancel intent.
use super::*;
use crate::store::context::ActiveTask;
use harnx_runtime::nats_lease::NatsSessionLease;

impl Runner {
    pub(crate) async fn cancel_hitl_task(
        &self,
        access: TaskAccess<'_>,
        session: &NatsSession,
    ) -> Result<TaskRecord> {
        self.authorize_session(access.export, access.owner, session)
            .await?;
        if let Some(terminal) = self.request_hitl_cancel(session, access.task_id).await? {
            return Ok(terminal);
        }
        tokio::time::timeout(
            Duration::from_secs(10),
            self.await_hitl_cancellation(session, access.task_id),
        )
        .await
        .context("A2A cancellation unconfirmed; retry the same task")?
    }

    async fn request_hitl_cancel(
        &self,
        session: &NatsSession,
        task_id: &str,
    ) -> Result<Option<TaskRecord>> {
        let Err(error) = self
            .store
            .request_task_cancel(session.storage_key(), task_id)
            .await
        else {
            return Ok(None);
        };
        let latest = self
            .store
            .get_task(session.storage_key(), task_id)
            .await?
            .context("cancel task missing")?;
        // Completion/retirement can win while CancelTask still uses T1's
        // snapshot. Never apply its intent to a successor task.
        if latest.task.status.state.is_terminal() {
            return Ok(Some(latest));
        }
        Err(error)
    }

    async fn try_claim_and_cancel_hitl(
        &self,
        session: &NatsSession,
        lease: Arc<NatsSessionLease>,
        task_id: &str,
    ) -> Result<TaskRecord> {
        let mut authority = self.claim_context(session, lease).await?;
        let result = self
            .cancel_owned_hitl(session, &mut authority, task_id)
            .await;
        authority
            .release(&self.store, session.storage_key())
            .await?;
        result
    }

    async fn await_hitl_cancellation(
        &self,
        session: &NatsSession,
        task_id: &str,
    ) -> Result<TaskRecord> {
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
                return self
                    .try_claim_and_cancel_hitl(session, lease, task_id)
                    .await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn preserve_canceled_reply_dedupe(
        &self,
        session: &NatsSession,
        task_id: &str,
        active: &ActiveTask,
    ) -> Result<()> {
        if let Some(reply) = active
            .hitl_checkpoint
            .as_ref()
            .and_then(|c| c.reply.as_ref())
        {
            self.store
                .put_dedupe_mapping(
                    crate::store::DedupeMappingKind::Reply,
                    session.storage_key(),
                    crate::store::DedupeMappingTarget::new(reply.message_identity(), task_id),
                )
                .await?;
        }
        Ok(())
    }

    async fn cancel_owned_hitl(
        &self,
        session: &NatsSession,
        authority: &mut OwnedTask,
        task_id: &str,
    ) -> Result<TaskRecord> {
        let latest = self
            .store
            .get_task(session.storage_key(), task_id)
            .await?
            .context("cancel task missing")?;
        if latest.task.status.state.is_terminal() {
            return Ok(latest);
        }
        let active = validate_active_for_cancel(authority, task_id)?;
        self.preserve_canceled_reply_dedupe(session, task_id, active)
            .await?;
        self.recover_admission(session, authority).await?;
        self.store
            .get_task(session.storage_key(), task_id)
            .await?
            .context("canceled task missing")
    }
}

fn validate_active_for_cancel<'a>(
    authority: &'a OwnedTask,
    task_id: &str,
) -> Result<&'a ActiveTask> {
    let active = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .context("cancel task missing")?;
    anyhow::ensure!(active.snapshot.task.id == task_id, RunnerError::Terminal);
    anyhow::ensure!(active.cancel.is_some(), "cancel intent missing");
    Ok(active)
}
