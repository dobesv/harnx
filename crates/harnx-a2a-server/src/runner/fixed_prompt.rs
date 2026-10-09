use super::*;
use harnx_runtime::nats_session::AppendedPrompt;

impl Publisher {
    pub(super) async fn admit_fixed_prompt(
        &mut self,
        session: &NatsSession,
        input: &Input,
    ) -> Result<AppendedPrompt> {
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::BeforeAdmission)
            .await;
        let ticket = self.authority.ticket(&self.storage_key)?;
        let outcome = session
            .append_fixed_prompt(&ticket, input.message_content())
            .await?;
        anyhow::ensure!(
            matches!(
                outcome,
                harnx_runtime::nats_session::fixed_admission::FixedAdmissionOutcome::Admitted { .. }
            ),
            "fixed admission closed or fenced before execution"
        );
        let appended = session
            .fixed_prompt_handle(&ticket)
            .await?
            .context("fixed prompt handle missing")?;
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::AfterAdmission)
            .await;
        let record = self
            .authority
            .update(&self.store, &self.storage_key, |active| {
                active.snapshot.user_msg_id = appended.user_msg_id().into();
                active.snapshot.user_msg_seq = appended.user_msg_seq();
                active.snapshot.execution_id = appended.execution_id().unwrap_or_default().into();
                active.admission.phase = crate::store::context::AdmissionPhase::Admitted;
                active.admission.prompt_sequence = Some(appended.user_msg_seq());
            })
            .await?;
        self.record = Some(Arc::new(record));
        let active = self
            .authority
            .context
            .document
            .state
            .active
            .as_ref()
            .context("admitted task missing")?;
        self.store
            .put_message_dedupe(
                &self.storage_key,
                crate::store::MessageIdentity {
                    message_id: &active.message.message_id,
                    fingerprint: &active.message.fingerprint,
                },
                &active.snapshot.task.id,
            )
            .await?;
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::MessageMapping)
            .await;
        self.publish_snapshot();
        Ok(appended)
    }
}
