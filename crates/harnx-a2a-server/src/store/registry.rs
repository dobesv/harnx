//! Immutable discovery records precede admitted work; scans are leader-backed.
use super::{A2aStore, FirstMessageReservation, TaskAllocation};
use crate::{exports::Export, identity::Principal};
use anyhow::{ensure, Result};
use harnx_core::crypto::sha256;
use serde::{Deserialize, Serialize};
mod identity;
mod prune;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryRegistration {
    pub agent: String,
    pub export: String,
    pub owner: Option<String>,
    pub allocation: TaskAllocation,
    pub first: Option<FirstMessageReservation>,
}
impl A2aStore {
    pub async fn register_recovery(
        &self,
        export: &Export,
        owner: &Principal,
        allocation: &TaskAllocation,
        first: Option<FirstMessageReservation>,
    ) -> Result<()> {
        // An admin may admit a follow-up, but recovery must resume as the original
        // owner, not as that request's caller. Memberships never enter this record.
        let owner = self.recovery_owner(export, owner, allocation).await?;
        let record = RecoveryRegistration {
            agent: export.agent.clone(),
            export: export.public_name.clone(),
            owner,
            allocation: allocation.clone(),
            first,
        };
        record.validate_export(export)?;
        let key = format!("a2a.registry.{}", sha256(&allocation.storage_key));
        let kv = self.metadata().kv_store();
        if kv
            .update(&key, serde_json::to_vec(&record)?.into(), 0)
            .await
            .is_err()
        {
            let bytes = harnx_nats_common::leader_reads::get(kv, &key)
                .await?
                .ok_or_else(|| anyhow::anyhow!("recovery registry write unconfirmed"))?;
            let saved: RecoveryRegistration = serde_json::from_slice(&bytes)?;
            ensure!(
                saved.identity_matches(&record),
                "recovery registry identity mismatch"
            );
        }
        Ok(())
    }

    /// At most one record per call; cursor lives only in scanner, durable records
    /// survive restart. A full pass restarts at zero, including earlier failures.
    pub async fn next_recovery_registration(
        &self,
        cursor: &mut u64,
    ) -> Result<Option<RecoveryRegistration>> {
        let kv = self.metadata().kv_store();
        let result = kv
            .stream
            .raw_message_builder()
            .sequence(*cursor)
            .next_by_subject(format!("$KV.{}.a2a.registry.*", kv.name))
            .send()
            .await;
        match result {
            Ok(message) => {
                *cursor = message.sequence + 1;
                if message.payload.is_empty() {
                    return Ok(None);
                }
                Ok(Some(serde_json::from_slice(&message.payload)?))
            }
            Err(error)
                if error.kind()
                    == async_nats::jetstream::stream::RawMessageErrorKind::NoMessageFound =>
            {
                *cursor = 0;
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }
}
