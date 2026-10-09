use super::outbox::stage_event;
use super::*;
use harnx_runtime::nats_session::fixed_admission::FixedAdmissionOutcome;

impl Publisher {
    pub(super) async fn persist_event(
        &mut self,
        changes: TaskChanges,
        response: StreamResponse,
    ) -> Result<()> {
        let terminal = changes
            .status
            .as_ref()
            .is_some_and(|status| status.state.is_terminal());
        anyhow::ensure!(
            !terminal || self.stop_observed,
            "task has no durable stop proof"
        );
        self.authority
            .flush_pending(&self.store, &self.storage_key)
            .await?;
        let commit_id = if terminal {
            format!("{}:terminal", self.record().task.id)
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        let record = self
            .authority
            .update(&self.store, &self.storage_key, |active| {
                // A superseded stage acknowledgement is resolved by its stable
                // envelope, not by applying the delta or allocating another event.
                if active
                    .publication
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.commit_id == commit_id)
                {
                    return;
                }
                if let Some(status) = &changes.status {
                    active.snapshot.task.status = status.clone();
                }
                if let Some(history) = &changes.history {
                    active.snapshot.task.history = Some(history.clone());
                }
                if let Some(artifacts) = &changes.artifacts {
                    active.snapshot.task.artifacts = Some(artifacts.clone());
                }
                if terminal {
                    active.stop_confirmed = true;
                }
                stage_event(active, response.clone(), &commit_id);
            })
            .await?;
        self.record = Some(Arc::new(record));
        self.publish_snapshot();
        Ok(())
    }
    pub(super) async fn confirm_stop(&mut self, session: &NatsSession) -> Result<()> {
        let ticket = self.authority.ticket(&self.storage_key)?;
        let stopped = match session.resolve_fixed_admission(&ticket).await? {
            FixedAdmissionOutcome::Closed { .. } | FixedAdmissionOutcome::Fenced { .. } => true,
            FixedAdmissionOutcome::Admitted { .. } => {
                session.fixed_prompt_completion(&ticket).await?.is_some()
            }
            FixedAdmissionOutcome::Pending => false,
        };
        anyhow::ensure!(stopped, "runtime invocation stop unconfirmed");
        self.stop_observed = true;
        Ok(())
    }
    pub(super) async fn stop_failed_admission(&mut self, session: &NatsSession) -> Result<()> {
        let ticket = self.authority.ticket(&self.storage_key)?;
        match session.close_fixed_admission(&ticket).await? {
            FixedAdmissionOutcome::Admitted { prompt_sequence } => {
                session
                    .interrupt_prompt(prompt_sequence, "A2A turn failed")
                    .await?;
            }
            FixedAdmissionOutcome::Closed { .. } | FixedAdmissionOutcome::Fenced { .. } => {}
            FixedAdmissionOutcome::Pending => anyhow::bail!("runtime closure unconfirmed"),
        }
        self.confirm_stop(session).await
    }
}
