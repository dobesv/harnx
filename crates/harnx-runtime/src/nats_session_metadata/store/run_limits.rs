use super::{invocation_limits_key, run_limits_key, SessionMetadataStore};
use crate::nats_session_metadata::RunLimitsRecord;
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::kv;

impl SessionMetadataStore {
    /// Create an immutable root snapshot. Identical retries return the original
    /// revision, including after a lost acknowledgement. Different content fails.
    pub async fn put_run_limits(&self, storage_key: &str, record: &RunLimitsRecord) -> Result<u64> {
        ensure!(
            record.parent_invocation.is_none(),
            "run limits must describe a root invocation"
        );
        self.create_limits(&run_limits_key(storage_key, record.run_id.as_str()), record)
            .await
    }

    /// Load the frozen root snapshot, never a freshly calculated deadline.
    pub async fn get_run_limits(
        &self,
        storage_key: &str,
        run_id: &str,
    ) -> Result<Option<RunLimitsRecord>> {
        let record = self
            .read_limits(&run_limits_key(storage_key, run_id))
            .await?;
        if let Some(record) = &record {
            ensure!(
                record.run_id.as_str() == run_id && record.parent_invocation.is_none(),
                "run limits identity mismatch"
            );
        }
        Ok(record)
    }

    /// Create an immutable invocation snapshot with the same retry contract as roots.
    pub async fn put_invocation_limits(
        &self,
        storage_key: &str,
        record: &RunLimitsRecord,
    ) -> Result<u64> {
        self.create_limits(
            &invocation_limits_key(storage_key, record.invocation_id.as_str()),
            record,
        )
        .await
    }

    pub async fn get_invocation_limits(
        &self,
        storage_key: &str,
        invocation_id: &str,
    ) -> Result<Option<RunLimitsRecord>> {
        let record = self
            .read_limits(&invocation_limits_key(storage_key, invocation_id))
            .await?;
        if let Some(record) = &record {
            ensure!(
                record.invocation_id.as_str() == invocation_id,
                "invocation limits identity mismatch"
            );
        }
        Ok(record)
    }

    /// First admission resolves policy; replay reuses the saved snapshot without
    /// evaluating `admit`. Concurrent admissions of the same identity adopt the
    /// first snapshot. A reused invocation ID from a different run fails closed.
    pub async fn load_or_create_invocation_limits(
        &self,
        storage_key: &str,
        run_id: &str,
        invocation_id: &str,
        admit: impl FnOnce() -> Result<RunLimitsRecord>,
    ) -> Result<RunLimitsRecord> {
        let key = invocation_limits_key(storage_key, invocation_id);
        let record = self
            .load_or_create_limits(&key, || {
                let proposed = admit()?;
                ensure!(
                    proposed.run_id.as_str() == run_id
                        && proposed.invocation_id.as_str() == invocation_id,
                    "invocation limits identity mismatch"
                );
                Ok(proposed)
            })
            .await?;
        ensure!(
            record.run_id.as_str() == run_id && record.invocation_id.as_str() == invocation_id,
            "invocation limits identity mismatch"
        );
        Ok(record)
    }

    /// Root equivalent of `load_or_create_invocation_limits`. A later external
    /// instruction must have a new run ID; it cannot update the earlier snapshot.
    pub async fn load_or_create_run_limits(
        &self,
        storage_key: &str,
        run_id: &str,
        admit: impl FnOnce() -> Result<RunLimitsRecord>,
    ) -> Result<RunLimitsRecord> {
        let key = run_limits_key(storage_key, run_id);
        let record = self
            .load_or_create_limits(&key, || {
                let proposed = admit()?;
                ensure!(
                    proposed.run_id.as_str() == run_id && proposed.parent_invocation.is_none(),
                    "run limits identity mismatch"
                );
                Ok(proposed)
            })
            .await?;
        ensure!(
            record.run_id.as_str() == run_id && record.parent_invocation.is_none(),
            "run limits identity mismatch"
        );
        Ok(record)
    }

    async fn load_or_create_limits(
        &self,
        key: &str,
        admit: impl FnOnce() -> Result<RunLimitsRecord>,
    ) -> Result<RunLimitsRecord> {
        if let Some(record) = self.read_limits(key).await? {
            return Ok(record);
        }
        let proposed = admit()?;
        match self.create_limits(key, &proposed).await {
            Ok(_) => Ok(proposed),
            Err(error) => self.read_limits(key).await?.ok_or(error),
        }
    }

    async fn create_limits(&self, key: &str, record: &RunLimitsRecord) -> Result<u64> {
        let payload = serde_json::to_vec(record)?;
        // Expected revision zero is create-only CAS. Unlike KV create(), it also
        // refuses to resurrect a tombstoned identity after session cleanup.
        match self.store.update(key, payload.into(), 0).await {
            Ok(revision) => Ok(revision),
            Err(error) => {
                // Never retry a mutation on an ambiguous ack. Read authoritative
                // state; equality confirms an identical retry, not an overwrite.
                let entry = harnx_nats_common::recovery::read(|| self.store.entry(key))
                    .await
                    .with_context(|| format!("Limits creation unconfirmed for '{key}': {error}"))?;
                if let Some(entry) = entry {
                    ensure!(
                        entry.operation == kv::Operation::Put,
                        "limits identity '{key}' was deleted"
                    );
                    let stored: RunLimitsRecord = serde_json::from_slice(&entry.value)
                        .with_context(|| format!("Invalid limits record '{key}'"))?;
                    ensure!(stored == *record, "immutable limits conflict for '{key}'");
                    return Ok(entry.revision);
                }
                Err(error.into())
            }
        }
    }

    async fn read_limits(&self, key: &str) -> Result<Option<RunLimitsRecord>> {
        let entry = harnx_nats_common::recovery::read(|| self.store.entry(key)).await?;
        match entry {
            Some(entry) if entry.operation == kv::Operation::Put => {
                serde_json::from_slice(&entry.value)
                    .with_context(|| format!("Invalid limits record '{key}'"))
                    .map(Some)
            }
            Some(_) => anyhow::bail!("limits identity '{key}' was deleted"),
            None => Ok(None),
        }
    }
}
