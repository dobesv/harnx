//! Fixed admission for coordinated frontends. Nothing here activates or replays work.
use super::{AppendedPrompt, NatsSession};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_session_metadata::InvocationAdmission;
use anyhow::{ensure, Context, Result};
use harnx_core::{
    message::{MessageContent, MessageRole},
    session::SessionLogEntry,
};

mod ticket;
pub use ticket::{FixedAdmissionOutcome, FixedAdmissionTicket};

pub(crate) fn admission_closed(
    entries: &[(u64, SessionLogEntry)],
    invocation: &str,
    prompt: &str,
) -> bool {
    entries.iter().any(|(_, entry)| {
        matches!(entry,
        SessionLogEntry::AdmissionClosed { invocation_id, prompt_id, .. }
            if invocation_id == invocation || prompt_id == prompt)
    })
}

impl NatsSession {
    fn fixed_log(&self) -> NatsSessionLog {
        NatsSessionLog::new(self.jetstream.clone(), self.storage_key.clone())
    }

    fn check_fixed_ticket(&self, ticket: &FixedAdmissionTicket) -> Result<()> {
        ticket.validate()?;
        ensure!(
            ticket.storage_key() == self.storage_key,
            "fixed admission session mismatch"
        );
        Ok(())
    }

    /// Capture only. Caller must win its same-document owner CAS before any
    /// runtime effects. Recovery restores that ticket instead of calling this again.
    pub async fn prepare_fixed_admission(
        &self,
        invocation_id: String,
        prompt_id: String,
        closure_id: String,
    ) -> Result<FixedAdmissionTicket> {
        let tail = self
            .fixed_log()
            .load_events_latest_required_async()
            .await?
            .last()
            .map_or(0, |(seq, _)| *seq);
        FixedAdmissionTicket::from_parts(
            self.storage_key.clone(),
            invocation_id,
            prompt_id,
            closure_id,
            tail,
        )
    }

    /// Resolve durable raw identity, including after later execution/edits. An
    /// unavailable broker or missing retained predecessor is not proof of closure.
    pub async fn resolve_fixed_admission(
        &self,
        ticket: &FixedAdmissionTicket,
    ) -> Result<FixedAdmissionOutcome> {
        self.resolve_fixed_admission_inner(ticket, None).await
    }

    async fn resolve_fixed_admission_inner(
        &self,
        ticket: &FixedAdmissionTicket,
        expected_content: Option<&MessageContent>,
    ) -> Result<FixedAdmissionOutcome> {
        self.check_fixed_ticket(ticket)?;
        let saved = self
            .metadata_store
            .admission(&self.storage_key, ticket.invocation_id())
            .await?;
        if let Some(saved) = &saved {
            ensure!(
                saved.fixed_ticket.as_ref() == Some(ticket),
                "fixed admission ticket mismatch"
            );
            if let Some(content) = expected_content {
                ensure!(
                    saved.prompt_content.as_ref() == Some(content),
                    "fixed prompt content mismatch"
                );
            }
        }
        let entries = self.fixed_log().load_events_latest_required_async().await?;
        let outcome = ticket.outcome(&entries)?;
        if let FixedAdmissionOutcome::Admitted { prompt_sequence } = outcome {
            let saved = match saved {
                Some(saved) => saved,
                None => self
                    .metadata_store
                    .admission(&self.storage_key, ticket.invocation_id())
                    .await?
                    .context("fixed prompt missing admission")?,
            };
            ensure!(
                saved.fixed_ticket.as_ref() == Some(ticket),
                "fixed admission ticket mismatch"
            );
            if let Some(content) = expected_content {
                ensure!(
                    saved.prompt_content.as_ref() == Some(content),
                    "fixed prompt content mismatch"
                );
            }
            let entry = &entries
                .iter()
                .find(|(seq, _)| *seq == prompt_sequence)
                .context("fixed prompt missing from transcript")?
                .1;
            ensure!(
                matches!(entry, SessionLogEntry::Message { content, .. }
                if Some(content) == saved.prompt_content.as_ref()),
                "fixed prompt content mismatch"
            );
        }
        Ok(outcome)
    }

    /// Reserve immutable admission and append once at the fixed predecessor.
    /// A closed/fenced ticket returns its durable outcome, never a new prompt.
    pub async fn append_fixed_prompt(
        &self,
        ticket: &FixedAdmissionTicket,
        content: MessageContent,
    ) -> Result<FixedAdmissionOutcome> {
        self.check_fixed_ticket(ticket)?;
        self.check_parent_work("fixed-prompt-admission").await?;
        let outcome = self
            .resolve_fixed_admission_inner(ticket, Some(&content))
            .await?;
        if outcome != FixedAdmissionOutcome::Pending {
            return Ok(outcome);
        }
        let authority = self
            .admission_authority
            .as_ref()
            .context("fixed admission requires explicit authority")?;
        let mut intent = InvocationAdmission::new(
            authority,
            ticket.invocation_id().to_owned(),
            self.admission_timeout,
            self.admission_token_budget,
        );
        intent.parent_storage_key = self.parent_session_id.clone();
        intent.prompt_content = Some(content.clone());
        intent.fixed_ticket = Some(ticket.clone());
        let entries = self.fixed_log().load_events_latest_required_async().await?;
        self.metadata_store
            .reserve_admission(&self.storage_key, &intent, &entries)
            .await?;
        self.metadata_store
            .bind_prompt_admission(
                &self.storage_key,
                ticket.prompt_id(),
                ticket.invocation_id(),
            )
            .await?;
        let entry = SessionLogEntry::Message {
            id: Some(ticket.prompt_id().to_owned()),
            role: MessageRole::User,
            content,
            timestamp: None,
            fence_token: None,
        };
        self.commit_fixed_entry(ticket, &entry, "prompt").await
    }

    /// Close competes with prompt at the SAME predecessor. It never reserves an
    /// invocation, interrupts another turn, activates work, or reconstructs text.
    pub async fn close_fixed_admission(
        &self,
        ticket: &FixedAdmissionTicket,
    ) -> Result<FixedAdmissionOutcome> {
        self.check_fixed_ticket(ticket)?;
        let outcome = self.resolve_fixed_admission(ticket).await?;
        if outcome != FixedAdmissionOutcome::Pending {
            return Ok(outcome);
        }
        self.commit_fixed_entry(ticket, &ticket.close_entry(), "close")
            .await
    }

    async fn commit_fixed_entry(
        &self,
        ticket: &FixedAdmissionTicket,
        entry: &SessionLogEntry,
        kind: &str,
    ) -> Result<FixedAdmissionOutcome> {
        #[cfg(feature = "fault-injection")]
        if let Some(faults) = &self.fixed_admission_faults {
            faults.before_append().await;
        }
        let message_id = format!(
            "fixed-{kind}:{}:{}",
            ticket.storage_key(),
            if kind == "prompt" {
                ticket.prompt_id()
            } else {
                ticket.closure_id()
            }
        );
        let result = self
            .fixed_log()
            .append_fenced(entry, ticket.expected_predecessor(), &message_id)
            .await;
        #[cfg(feature = "fault-injection")]
        let result = if self
            .fixed_admission_faults
            .as_ref()
            .is_some_and(|f| f.drop_ack())
            && result.is_ok()
        {
            Err(anyhow::anyhow!(
                "injected lost fixed admission acknowledgement"
            ))
        } else {
            result
        };
        // Resolve both conflicts and ambiguous acknowledgements by original raw
        // identity. Even a duplicate PubAck is not proof of this exact payload.
        let outcome = self.resolve_fixed_admission(ticket).await?;
        if outcome == FixedAdmissionOutcome::Pending {
            result.context("fixed admission append unconfirmed")?;
            anyhow::bail!("fixed admission append acknowledged without durable identity");
        }
        Ok(outcome)
    }

    /// Obtain the existing runtime handle for observation/activation by the
    /// caller. Closing an admitted ticket returns this same execution identity.
    pub async fn fixed_prompt_handle(
        &self,
        ticket: &FixedAdmissionTicket,
    ) -> Result<Option<AppendedPrompt>> {
        match self.resolve_fixed_admission(ticket).await? {
            FixedAdmissionOutcome::Admitted { prompt_sequence } => Ok(Some(AppendedPrompt {
                user_msg_id: ticket.prompt_id().to_owned(),
                user_msg_seq: prompt_sequence,
                execution_id: Some(ticket.invocation_id().to_owned()),
                events: None,
                live: None,
            })),
            _ => Ok(None),
        }
    }
}

impl NatsSession {
    /// Observe the original fixed invocation's durable completion without
    /// activation or append. Recovery must check this before deciding to stop it.
    pub async fn fixed_prompt_completion(
        &self,
        ticket: &FixedAdmissionTicket,
    ) -> Result<Option<super::NatsTurnResult>> {
        let FixedAdmissionOutcome::Admitted { prompt_sequence } =
            self.resolve_fixed_admission(ticket).await?
        else {
            return Ok(None);
        };
        let entries = self.fixed_log().load_events_latest_required_async().await?;
        let Some(terminal) = super::invocation_terminal_seq(&entries, prompt_sequence) else {
            return Ok(None);
        };
        let (response, error) = Self::extract_turn_outcome(&entries, prompt_sequence);
        Ok(Some(super::NatsTurnResult {
            response,
            error,
            session_id: self.session_id.clone(),
            user_msg_id: ticket.prompt_id().into(),
            user_msg_seq: prompt_sequence,
            was_cancelled: entries.iter().any(|(seq, entry)| {
                *seq == terminal && matches!(entry, SessionLogEntry::Cancel { .. })
            }),
        }))
    }
}
