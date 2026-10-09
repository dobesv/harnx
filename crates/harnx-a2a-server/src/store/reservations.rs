//! First-message identity is retained with work, never expired independently.
use super::{new_task_id, A2aStore, DedupeKey, StoreError};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};
use harnx_core::{crypto::sha256, session_identity::session_key};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAllocation {
    pub local_id: String,
    pub storage_key: String,
    pub task_id: String,
    pub invocation_id: String,
    pub prompt_id: String,
    pub closure_id: String,
    pub created_at: DateTime<Utc>,
}
impl TaskAllocation {
    pub fn new(agent: &str, local_id: String) -> Self {
        Self {
            storage_key: session_key(Some(agent), &local_id),
            task_id: new_task_id(&local_id),
            local_id,
            invocation_id: uuid::Uuid::new_v4().to_string(),
            prompt_id: uuid::Uuid::new_v4().to_string(),
            closure_id: uuid::Uuid::new_v4().to_string(),
            created_at: Utc::now(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FirstMessageReservation {
    pub identity: DedupeKey,
    pub fingerprint: String,
    pub message: a2a_lf::Message,
    pub allocation: TaskAllocation,
}
fn key(identity: &DedupeKey) -> Result<String> {
    // Hash one structured identity, not ambiguous concatenated user input.
    Ok(format!(
        "a2a/first-messages/{}",
        sha256(&serde_json::to_string(identity)?)
    ))
}
impl A2aStore {
    pub async fn first_message_reservation(
        &self,
        identity: &DedupeKey,
        fingerprint: &str,
    ) -> Result<Option<FirstMessageReservation>> {
        let key = key(identity)?;
        let Some(bytes) = harnx_nats_common::leader_reads::retry_transient(|| {
            harnx_nats_common::leader_reads::get(self.store.kv_store(), &key)
        })
        .await?
        else {
            return Ok(None);
        };
        let saved: FirstMessageReservation = serde_json::from_slice(&bytes)?;
        ensure!(
            &saved.identity == identity,
            "first-message reservation identity mismatch"
        );
        ensure!(
            saved.fingerprint == fingerprint,
            StoreError::FingerprintMismatch
        );
        ensure!(
            saved.message.message_id == identity.message_id
                && super::message_fingerprint(&saved.message.parts) == saved.fingerprint,
            "reserved message content identity mismatch"
        );
        ensure!(
            super::parse_task_id(&saved.allocation.task_id)?.0 == saved.allocation.local_id,
            "reserved task context identity mismatch"
        );
        Ok(Some(saved))
    }

    /// Caller already holds this candidate's scoped lease. Losing candidates
    /// release their unused lease and follow the exact winner, including IDs.
    pub async fn reserve_first_message(
        &self,
        proposed: &FirstMessageReservation,
        lease: &harnx_runtime::nats_lease::NatsSessionLease,
    ) -> Result<(FirstMessageReservation, bool)> {
        ensure!(
            lease.key() == format!("sessions/{}/a2a/lock", proposed.allocation.storage_key)
                && lease.revalidate_ownership().await?,
            "first-message reservation requires held candidate lease"
        );
        let result = self
            .store
            .kv_store()
            .update(
                key(&proposed.identity)?,
                serde_json::to_vec(proposed)?.into(),
                0,
            )
            .await
            .map_err(anyhow::Error::from);
        #[cfg(feature = "fault-injection")]
        let result = if self.context_hooks.take_first_ack_loss() && result.is_ok() {
            Err(anyhow::anyhow!(
                "injected first-message acknowledgement loss"
            ))
        } else {
            result
        };
        match result {
            Ok(_) => Ok((proposed.clone(), true)),
            Err(error) => {
                let saved = self
                    .first_message_reservation(&proposed.identity, &proposed.fingerprint)
                    .await?
                    .with_context(|| format!("first-message reservation unconfirmed: {error}"))?;
                let own = saved == *proposed;
                Ok((saved, own))
            }
        }
    }
}
