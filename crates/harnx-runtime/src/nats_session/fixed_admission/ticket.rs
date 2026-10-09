use anyhow::{ensure, Result};
use harnx_core::session::SessionLogEntry;
use serde::{Deserialize, Serialize};

/// Persist this ticket in the frontend's authority before calling append or close.
/// Restoring it must preserve every field, especially the original predecessor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixedAdmissionTicket {
    storage_key: String,
    invocation_id: String,
    prompt_id: String,
    closure_id: String,
    expected_predecessor: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedAdmissionOutcome {
    Pending,
    Admitted {
        prompt_sequence: u64,
    },
    Closed {
        closure_sequence: u64,
    },
    /// An unrelated entry won the original predecessor. The missing prompt
    /// cannot be appended later; closure does not append at the newer tail.
    Fenced {
        sequence: u64,
    },
}

impl FixedAdmissionTicket {
    /// Restore identities from durable frontend authority, never from a new tail.
    pub fn from_parts(
        storage_key: String,
        invocation_id: String,
        prompt_id: String,
        closure_id: String,
        expected_predecessor: u64,
    ) -> Result<Self> {
        let ticket = Self {
            storage_key,
            invocation_id,
            prompt_id,
            closure_id,
            expected_predecessor,
        };
        ticket.validate()?;
        Ok(ticket)
    }

    pub fn storage_key(&self) -> &str {
        &self.storage_key
    }
    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }
    pub fn prompt_id(&self) -> &str {
        &self.prompt_id
    }
    pub fn closure_id(&self) -> &str {
        &self.closure_id
    }
    pub fn expected_predecessor(&self) -> u64 {
        self.expected_predecessor
    }

    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            !self.storage_key.is_empty()
                && !self.prompt_id.is_empty()
                && !self.closure_id.is_empty(),
            "empty fixed admission identity"
        );
        ensure!(
            !self.invocation_id.is_empty()
                && self
                    .invocation_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "fixed invocation identity must be a KV-safe segment"
        );
        ensure!(
            self.expected_predecessor < u64::MAX,
            "fixed admission predecessor overflow"
        );
        Ok(())
    }

    pub(crate) fn outcome(
        &self,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<FixedAdmissionOutcome> {
        self.validate()?;
        let successor = self.expected_predecessor + 1;
        if let Some((seq, _)) = entries.iter().find(|(_, entry)| {
            matches!(entry, SessionLogEntry::Message { id: Some(id), role, .. }
                if role.is_user() && id == &self.prompt_id)
        }) {
            ensure!(
                *seq == successor,
                "fixed prompt identity at wrong predecessor"
            );
            return Ok(FixedAdmissionOutcome::Admitted {
                prompt_sequence: *seq,
            });
        }
        let Some((seq, entry)) = entries.iter().find(|(seq, _)| *seq == successor) else {
            let tail = entries.last().map_or(0, |(seq, _)| *seq);
            ensure!(
                tail == self.expected_predecessor,
                "fixed admission log gap or predecessor unavailable"
            );
            return Ok(FixedAdmissionOutcome::Pending);
        };
        if let SessionLogEntry::AdmissionClosed {
            invocation_id,
            prompt_id,
            closure_id,
            expected_predecessor,
        } = entry
        {
            if invocation_id == &self.invocation_id && prompt_id == &self.prompt_id {
                ensure!(
                    closure_id == &self.closure_id
                        && *expected_predecessor == self.expected_predecessor,
                    "fixed admission closure identity mismatch"
                );
                return Ok(FixedAdmissionOutcome::Closed {
                    closure_sequence: *seq,
                });
            }
        }
        Ok(FixedAdmissionOutcome::Fenced { sequence: *seq })
    }

    pub(super) fn close_entry(&self) -> SessionLogEntry {
        SessionLogEntry::AdmissionClosed {
            invocation_id: self.invocation_id.clone(),
            prompt_id: self.prompt_id.clone(),
            closure_id: self.closure_id.clone(),
            expected_predecessor: self.expected_predecessor,
        }
    }
}
