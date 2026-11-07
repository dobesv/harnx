//! Durable composed-prompt delivery without attaching a follower.
use super::{AppendedPrompt, NatsSession};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::{AdmissionAuthority, InvocationAdmission};
use anyhow::{ensure, Context, Result};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;

/// No activation is needed for an already-visible boundary. This is not a
/// terminal classification: approval visibility also suppresses activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptActivation {
    Published,
    NotNeeded,
}

impl NatsSession {
    /// Reserve, bind and append once. Replay uses the original admission content
    /// and raw prompt sequence, never the invocation's latest steering prompt.
    pub(crate) async fn append_dispatch_content(
        &self,
        content: MessageContent,
    ) -> Result<AppendedPrompt> {
        self.append_dispatch_content_inner(content, DispatchAttempt::default())
            .await
    }

    async fn append_dispatch_content_inner(
        &self,
        content: MessageContent,
        _attempt: DispatchAttempt,
    ) -> Result<AppendedPrompt> {
        let id = self
            .invocation_id
            .as_deref()
            .context("dispatch requires invocation identity")?;
        Self::ensure_unreserved_admission_id(id)?;
        let authority = self
            .admission_authority
            .as_ref()
            .context("dispatch requires inherited authority")?;
        ensure!(
            matches!(authority, AdmissionAuthority::Inherited { .. }),
            "dispatch requires inherited authority"
        );
        let parent = self
            .parent_session_id
            .as_deref()
            .context("dispatch requires trusted parent storage scope")?;
        let entries = self.load_durable_entries().await?;
        let mut intent = InvocationAdmission::new(
            authority,
            id.into(),
            self.admission_timeout,
            self.admission_token_budget,
        );
        intent.parent_storage_key = Some(parent.into());
        intent.prompt_content = Some(content);
        // A historical replay must not contend with a later invocation's head.
        // Validate saved ownership before repairing its create-only binding.
        if let Some(saved) = self.metadata_store.admission(&self.storage_key, id).await? {
            validate_dispatch_admission(&saved, &intent)?;
            if let Some(prompt) = original_prompt(&entries, id, &saved)? {
                self.metadata_store
                    .bind_prompt_admission(&self.storage_key, id, saved.invocation_id.as_str())
                    .await?;
                return Ok(prompt);
            }
        }
        let admission = self
            .metadata_store
            .reserve_admission(&self.storage_key, &intent, &entries)
            .await?;
        #[cfg(test)]
        fail_at(_attempt.fault, DispatchFault::AfterAdmission)?;
        validate_dispatch_admission(&admission, &intent)?;
        self.metadata_store
            .bind_prompt_admission(&self.storage_key, id, admission.invocation_id.as_str())
            .await?;
        #[cfg(test)]
        fail_at(_attempt.fault, DispatchFault::AfterBinding)?;
        if let Some(prompt) = original_prompt(&entries, id, &admission)? {
            return Ok(prompt);
        }
        self.append_new_dispatch_prompt(id, &admission, _attempt)
            .await
    }

    async fn append_new_dispatch_prompt(
        &self,
        id: &str,
        admission: &InvocationAdmission,
        _attempt: DispatchAttempt,
    ) -> Result<AppendedPrompt> {
        self.check_parent_work("child-prompt-admission").await?;
        ensure!(
            !dispatch_deadline_expired(&self.metadata_store, &self.storage_key, admission).await?,
            "dispatch admission deadline expired before append"
        );
        let log = NatsSessionLog::new_with_replicas(
            self.jetstream.clone(),
            &self.storage_key,
            self.attachment_replicas,
        );
        let entry = SessionLogEntry::Message {
            id: Some(id.into()),
            role: MessageRole::User,
            content: admission
                .prompt_content
                .clone()
                .context("dispatch admission has no original content")?,
            timestamp: None,
            fence_token: None,
        };
        #[cfg(test)]
        fail_at(_attempt.fault, DispatchFault::BeforeAppend)?;
        let appended = self.append_prompt_entry(&log, &entry, id).await;
        // A lost PubAck is not proof that append failed. Resolve the exact raw
        // identity before returning an error; never pick a later prompt.
        #[cfg(test)]
        let appended = if _attempt.fault == Some(DispatchFault::AppendAckLost) {
            appended.and_then(|_| anyhow::bail!("injected dispatch append acknowledgement loss"))
        } else {
            appended
        };
        match appended {
            Ok(seq) => Ok(dispatch_prompt(id, seq, admission)),
            Err(error) => {
                if let Some(prompt) =
                    original_prompt(&self.load_durable_entries().await?, id, admission)?
                {
                    Ok(prompt)
                } else {
                    Err(error.context("dispatch prompt not durably appended"))
                }
            }
        }
    }

    /// Publish only this original prompt. A completed/blocked replay doesn't
    /// resume approvals, and an expired admission doesn't start fresh work.
    pub(crate) async fn activate_dispatch_prompt(
        &self,
        prompt: &AppendedPrompt,
    ) -> Result<PromptActivation> {
        self.activate_dispatch_prompt_inner(prompt, DispatchAttempt::default())
            .await
    }

    async fn activate_dispatch_prompt_inner(
        &self,
        prompt: &AppendedPrompt,
        _attempt: DispatchAttempt,
    ) -> Result<PromptActivation> {
        let admission = self
            .metadata_store
            .prompt_admission(&self.storage_key, prompt.user_msg_id())
            .await?
            .context("dispatch prompt has no admission binding")?;
        ensure!(
            Some(admission.invocation_id.as_str()) == prompt.execution_id(),
            "dispatch execution binding mismatch"
        );
        ensure!(
            admission.parent_storage_key == self.parent_session_id
                && self.parent_session_id.is_some(),
            "dispatch parent storage scope mismatch"
        );
        ensure!(
            admission.fixed_ticket.is_none(),
            "fixed admission requires fixed-ticket activation"
        );
        let entries = self.load_durable_entries().await?;
        let original = original_prompt(&entries, prompt.user_msg_id(), &admission)?
            .context("dispatch original prompt is missing")?;
        ensure!(
            original.user_msg_seq() == prompt.user_msg_seq(),
            "dispatch original prompt sequence mismatch"
        );
        // Visibility is used only as an activation guard, never as terminal proof.
        let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)?;
        if Self::is_turn_completion_visible(&entries, Some(&effective), prompt.user_msg_seq()) {
            return Ok(PromptActivation::NotNeeded);
        }
        ensure!(
            !dispatch_deadline_expired(&self.metadata_store, &self.storage_key, &admission).await?,
            "dispatch admission deadline expired before activation"
        );
        #[cfg(test)]
        fail_at(_attempt.fault, DispatchFault::BeforeActivation)?;
        self.publish_dispatch_activation(prompt).await?;
        #[cfg(test)]
        fail_at(_attempt.fault, DispatchFault::ActivationAckLost)?;
        Ok(PromptActivation::Published)
    }

    async fn publish_dispatch_activation(&self, prompt: &AppendedPrompt) -> Result<()> {
        use crate::nats_worker::{
            publish_session_activate, publish_targeted_session_activate, LocalWorkerTarget,
            SessionActivate, SessionActivationRoute,
        };
        self.check_parent_work("child-activation").await?;
        // Journal replay republishes the exact activation, including its dedup
        // identity. Explicit recovery attempts may use a fresh delivery epoch,
        // but must still target this original prompt, never a session tail.
        let mut activation = SessionActivate {
            session_id: self.storage_key.clone(),
            execution_id: prompt.execution_id().map(str::to_owned),
            epoch: format!(
                "dispatch:{}:{}",
                prompt.user_msg_id(),
                prompt.user_msg_seq()
            ),
            agent_name: self.config.initializer.agent_name().map(str::to_owned),
            requested_seq: Some(prompt.user_msg_seq()),
            target_worker_id: None,
            token_budget: None,
            tool_confirmation_subject: None,
        };
        match &self.config.activation_route {
            SessionActivationRoute::ClusterShared => {
                publish_session_activate(
                    &self.jetstream,
                    &self.config.cluster,
                    &activation,
                    self.attachment_replicas,
                )
                .await?;
            }
            SessionActivationRoute::WorkerTargeted {
                session_scope,
                worker_id,
            } => {
                activation.target_worker_id = Some(worker_id.clone());
                publish_targeted_session_activate(
                    &self.jetstream,
                    LocalWorkerTarget::new(session_scope, worker_id)?,
                    &activation,
                )
                .await?;
            }
        }
        Ok(())
    }
}

fn validate_dispatch_admission(
    saved: &InvocationAdmission,
    intent: &InvocationAdmission,
) -> Result<()> {
    ensure!(
        saved.fixed_ticket.is_none(),
        "fixed admission requires fixed-ticket append"
    );
    ensure!(
        saved.origin == intent.origin
            && saved.parent_storage_key == intent.parent_storage_key
            && saved.run_id == intent.run_id
            && saved.parent.as_ref().map(|p| &p.invocation_id)
                == intent.parent.as_ref().map(|p| &p.invocation_id),
        "dispatch admission parent scope mismatch"
    );
    Ok(())
}

fn dispatch_prompt(id: &str, seq: u64, admission: &InvocationAdmission) -> AppendedPrompt {
    AppendedPrompt {
        user_msg_id: id.into(),
        user_msg_seq: seq,
        execution_id: Some(admission.invocation_id.as_str().into()),
        events: None,
        live: None,
    }
}

fn original_prompt(
    entries: &[(u64, SessionLogEntry)],
    id: &str,
    admission: &InvocationAdmission,
) -> Result<Option<AppendedPrompt>> {
    for (seq, entry) in entries {
        if let SessionLogEntry::Message {
            id: Some(found),
            role,
            content,
            ..
        } = entry
        {
            if role.is_user() && found == id {
                ensure!(
                    admission.prompt_content.as_ref() == Some(content),
                    "dispatch original prompt content mismatch"
                );
                return Ok(Some(dispatch_prompt(id, *seq, admission)));
            }
        }
    }
    Ok(None)
}

pub(crate) async fn dispatch_deadline_expired(
    store: &crate::nats_session_metadata::SessionMetadataStore,
    storage: &str,
    admission: &InvocationAdmission,
) -> Result<bool> {
    let limits = store
        .get_invocation_limits(storage, admission.invocation_id.as_str())
        .await?;
    // The lookup can cross the hard deadline. Check time after broker I/O,
    // including during fresh observer repair, not at request start.
    let now = chrono::Utc::now();
    if let Some(limits) = limits {
        return Ok(limits.is_expired_at(now));
    }
    // Target policy is resolved only by the worker. Before that, enforce the
    // immutable ancestor clamp and any explicit local allowance, not a guessed
    // fallback that could shorten a configured target policy.
    if admission
        .parent
        .as_ref()
        .is_some_and(|parent| parent.is_expired_at(now))
    {
        return Ok(true);
    }
    if let Some(seconds) = admission.timeout_secs.filter(|seconds| *seconds > 0) {
        let duration = chrono::Duration::try_seconds(i64::try_from(seconds)?)
            .context("dispatch deadline overflow")?;
        let deadline = admission
            .admitted_at
            .checked_add_signed(duration)
            .context("dispatch deadline overflow")?;
        return Ok(now >= deadline);
    }
    Ok(false)
}

// Fault injection belongs to one delivery attempt, never session state.
#[derive(Default)]
struct DispatchAttempt {
    #[cfg(test)]
    fault: Option<DispatchFault>,
}

#[cfg(test)]
impl From<DispatchFault> for DispatchAttempt {
    fn from(fault: DispatchFault) -> Self {
        Self { fault: Some(fault) }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchFault {
    AfterAdmission,
    AfterBinding,
    BeforeAppend,
    AppendAckLost,
    BeforeActivation,
    ActivationAckLost,
}

#[cfg(test)]
fn fail_at(actual: Option<DispatchFault>, boundary: DispatchFault) -> Result<()> {
    ensure!(
        actual != Some(boundary),
        "injected dispatch failure at {boundary:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
