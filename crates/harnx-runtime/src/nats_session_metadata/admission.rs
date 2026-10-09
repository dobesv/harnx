//! Admission is separate from delivery epochs and editable conversation text.
//!
//! Origin (`External` vs `Inherited`) is supplied only by harness frontend adapters
//! or inherited runtime work — never inferred from message role, missing parent,
//! or transport. This distinction is central to nonrenewing run deadlines: only a
//! trusted external admission mints a new run boundary.
use super::{InvocationIdentity, RunIdentity, RunLimitsRecord, SessionMetadataStore};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};
use harnx_core::session::SessionLogEntry;
use serde::{Deserialize, Serialize};

/// Authority supplied only by harness frontend adapters or inherited runtime work.
#[derive(Clone, Debug)]
pub enum AdmissionAuthority {
    /// A frontend has accepted a distinct external instruction. This is not an
    /// authentication claim about HTTP, message roles or shell processes.
    External { admitted_at: DateTime<Utc> },
    Inherited {
        parent: RunLimitsRecord,
        edge: super::InvocationEdgeKind,
        admitted_at: DateTime<Utc>,
    },
}

/// Persisted origin granted by the harness, not inferred from a message role
/// or a missing parent at worker pickup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionOrigin {
    External,
    Inherited,
}

/// Original intent stored before the executable prompt. Target worker freezes
/// effective target policy in the immutable limits record before dispatch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvocationAdmission {
    pub origin: AdmissionOrigin,
    pub run_id: RunIdentity,
    pub invocation_id: InvocationIdentity,
    pub admitted_at: DateTime<Utc>,
    pub parent: Option<RunLimitsRecord>,
    pub parent_storage_key: Option<String>,
    pub edge: Option<super::InvocationEdgeKind>,
    pub timeout_secs: Option<u64>,
    pub token_budget: Option<u64>,
    pub prompt_content: Option<harnx_core::message::MessageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_ticket: Option<crate::nats_session::fixed_admission::FixedAdmissionTicket>,
}

impl InvocationAdmission {
    pub fn new(
        authority: &AdmissionAuthority,
        invocation_id: String,
        timeout_secs: Option<u64>,
        token_budget: Option<u64>,
    ) -> Self {
        let (run_id, admitted_at, parent, edge) = match authority {
            AdmissionAuthority::External { admitted_at } => {
                (RunIdentity::new(), *admitted_at, None, None)
            }
            AdmissionAuthority::Inherited {
                parent,
                edge,
                admitted_at,
            } => (
                parent.run_id.clone(),
                *admitted_at,
                Some(parent.clone()),
                Some(*edge),
            ),
        };
        let origin = match authority {
            AdmissionAuthority::External { .. } => AdmissionOrigin::External,
            AdmissionAuthority::Inherited { .. } => AdmissionOrigin::Inherited,
        };
        Self {
            origin,
            run_id,
            invocation_id: InvocationIdentity::from_string(invocation_id),
            admitted_at,
            parent,
            parent_storage_key: None,
            edge,
            timeout_secs,
            token_budget,
            prompt_content: None,
            fixed_ticket: None,
        }
    }
}

fn intent_key(storage: &str, id: &str) -> String {
    format!("sessions/{storage}/admissions/{id}")
}
fn head_key(storage: &str) -> String {
    format!("sessions/{storage}/admission_head")
}
fn prompt_key(storage: &str, id: &str) -> String {
    // Transcript IDs are opaque, not KV-safe identifiers (imports can contain
    // spaces or punctuation). The immutable binding still stores the real ID.
    let key = harnx_core::session_identity::session_key(Some(storage), id);
    format!("sessions/{storage}/prompt_admissions/{key}")
}

impl SessionMetadataStore {
    pub async fn admission(&self, storage: &str, id: &str) -> Result<Option<InvocationAdmission>> {
        let key = intent_key(storage, id);
        let bytes = self.leader_value(&key).await?;
        bytes
            .map(|bytes| {
                let admission: InvocationAdmission =
                    serde_json::from_slice(&bytes).context("invalid invocation admission")?;
                ensure!(
                    admission.invocation_id.as_str() == id,
                    "invocation admission identity mismatch"
                );
                ensure!(
                    admission.parent.is_some() == admission.edge.is_some()
                        && admission.parent.is_some()
                            == (admission.origin == AdmissionOrigin::Inherited),
                    "invalid admission origin or ancestry"
                );
                if let Some(parent) = &admission.parent {
                    ensure!(
                        parent.run_id == admission.run_id,
                        "admission changed inherited run identity"
                    );
                }
                Ok(admission)
            })
            .transpose()
    }

    pub async fn prompt_admission(
        &self,
        storage: &str,
        prompt: &str,
    ) -> Result<Option<InvocationAdmission>> {
        let key = prompt_key(storage, prompt);
        let bytes = self.leader_value(&key).await?;
        match bytes {
            Some(bytes) => self.admission(storage, std::str::from_utf8(&bytes)?).await,
            None => Ok(None),
        }
    }

    /// Latest prompt bound to an invocation, including interactive steering.
    pub async fn invocation_prompt_seq(
        &self,
        storage: &str,
        id: &str,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<Option<u64>> {
        let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(entries)?;
        for (seq, entry) in effective.iter().rev() {
            let prompt = match entry {
                SessionLogEntry::Message {
                    id: Some(prompt),
                    role,
                    ..
                } if role.is_user() => Some(prompt),
                SessionLogEntry::CompactRequest { compaction_id, .. } => Some(compaction_id),
                _ => None,
            };
            if let Some(prompt) = prompt {
                if prompt == id
                    || self
                        .prompt_admission(storage, prompt)
                        .await?
                        .is_some_and(|admission| admission.invocation_id.as_str() == id)
                {
                    return Ok(Some(*seq));
                }
            }
        }
        // Removing or editing the root prompt does not remove its admission or
        // mint a fresh run. Keep the immutable raw root binding as a fallback.
        let fixed = self
            .admission(storage, id)
            .await?
            .and_then(|a| a.fixed_ticket);
        Ok(entries.iter().find_map(|(seq, entry)| match entry {
            SessionLogEntry::Message {
                id: Some(prompt),
                role,
                ..
            } if role.is_user()
                && (prompt == id || fixed.as_ref().is_some_and(|t| t.prompt_id() == prompt)) =>
            {
                Some(*seq)
            }
            SessionLogEntry::CompactRequest { compaction_id, .. } if compaction_id == id => {
                Some(*seq)
            }
            _ => None,
        }))
    }

    /// Reserve one execution invocation. Its log terminal permits replacement.
    /// Ordinary unbound reservations stay busy. Fixed reservations can instead
    /// prove non-executable closure at their immutable predecessor. An uncertain
    /// append never permits minting a replacement invocation.
    pub async fn reserve_admission(
        &self,
        storage: &str,
        intent: &InvocationAdmission,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<InvocationAdmission> {
        let id = intent.invocation_id.as_str();
        ensure!(
            intent.fixed_ticket.is_some()
                || !crate::nats_session::fixed_admission::admission_closed(entries, id, id),
            "fixed admission closed without prompt; cannot reserve ordinary replay"
        );
        if let Some(ticket) = &intent.fixed_ticket {
            ticket.validate()?;
            ensure!(
                ticket.storage_key() == storage
                    && ticket.invocation_id() == id
                    && intent.prompt_content.is_some(),
                "invalid fixed admission reservation"
            );
        }
        let frozen = match self.admission(storage, id).await? {
            Some(saved) => saved,
            None => {
                let payload = serde_json::to_vec(intent)?;
                if self
                    .kv_store()
                    .update(intent_key(storage, id), payload.into(), 0)
                    .await
                    .is_err()
                {
                    self.admission(storage, id)
                        .await?
                        .context("admission create unconfirmed")?
                } else {
                    intent.clone()
                }
            }
        };
        if intent.fixed_ticket.is_some() || frozen.fixed_ticket.is_some() {
            ensure!(
                intent.fixed_ticket == frozen.fixed_ticket
                    && intent.prompt_content == frozen.prompt_content,
                "fixed admission identity or content mismatch"
            );
        }
        let head = head_key(storage);
        for _ in 0..16 {
            let current = self.leader_entry(&head).await?;
            let revision = current.as_ref().map_or(0, |entry| entry.revision);
            if let Some(entry) = &current {
                let current_id = std::str::from_utf8(&entry.value)?;
                if current_id == id {
                    return Ok(frozen);
                }
                ensure!(
                    self.admission_is_terminal(storage, current_id, entries)
                        .await?,
                    "session busy: invocation {current_id} is active or admission is unconfirmed"
                );
            }
            if harnx_nats_common::cas::update(
                self.kv_store(),
                head.clone(),
                id.to_owned().into(),
                revision,
            )
            .await
            .is_ok()
            {
                return Ok(frozen);
            }
        }
        anyhow::bail!("session busy: admission reservation kept changing")
    }

    pub async fn active_admission(
        &self,
        storage: &str,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<Option<InvocationAdmission>> {
        let head = head_key(storage);
        let bytes = self.leader_value(&head).await?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let id = std::str::from_utf8(&bytes)?;
        if self.admission_is_terminal(storage, id, entries).await? {
            return Ok(None);
        }
        Ok(Some(
            self.admission(storage, id)
                .await?
                .context("active admission missing")?,
        ))
    }

    async fn admission_is_terminal(
        &self,
        storage: &str,
        id: &str,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<bool> {
        if let Some(ticket) = self
            .admission(storage, id)
            .await?
            .and_then(|a| a.fixed_ticket)
        {
            use crate::nats_session::fixed_admission::FixedAdmissionOutcome;
            ensure!(
                ticket.storage_key() == storage && ticket.invocation_id() == id,
                "fixed admission identity mismatch"
            );
            match ticket.outcome(entries)? {
                FixedAdmissionOutcome::Closed { .. } | FixedAdmissionOutcome::Fenced { .. } => {
                    return Ok(true)
                }
                _ => {}
            }
        }
        let prompt = self.invocation_prompt_seq(storage, id, entries).await?;
        Ok(prompt.is_some_and(|seq| {
            crate::nats_session::invocation_terminal_seq(entries, seq).is_some()
        }))
    }

    pub async fn bind_prompt_admission(
        &self,
        storage: &str,
        prompt: &str,
        invocation: &str,
    ) -> Result<()> {
        let key = prompt_key(storage, prompt);
        if self
            .kv_store()
            .update(&key, invocation.to_owned().into(), 0)
            .await
            .is_err()
        {
            let stored = self
                .leader_value(&key)
                .await?
                .context("prompt admission binding unconfirmed")?;
            ensure!(
                stored.as_ref() == invocation.as_bytes(),
                "prompt admission binding conflict"
            );
        }
        Ok(())
    }
}
