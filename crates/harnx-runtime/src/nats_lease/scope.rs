//! Explicit coordination scopes leave worker lease routing unchanged.
use super::*;

impl NatsLeaseConfig {
    /// Separate coordination leases must never contend with worker execution.
    pub fn key_for_scope(&self, session_id: &str, scope: &str) -> Result<String> {
        anyhow::ensure!(
            !scope.is_empty()
                && scope
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "lease scope must be one nonempty alphanumeric, underscore or hyphen segment"
        );
        Ok(format!("{LEASE_KEY_PREFIX}/{session_id}/{scope}/lock"))
    }
}

impl NatsSessionLease {
    /// Acquire a coordinator lease under sessions/{storage_key}/{scope}/lock.
    /// It uses the same renewal/revision-checked release machinery, but does not
    /// bind an execution or refresh worker session activity.
    pub async fn acquire_scoped(
        mut params: NatsLeaseAcquireParams<'_>,
        scope: &str,
    ) -> Result<Option<Self>> {
        let key = params.config.key_for_scope(params.session_id, scope)?;
        params.session_metadata = None;
        Self::acquire_key(params, None, key).await
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn bucket_name(&self) -> &str {
        &self.bucket.name
    }

    /// Stable claim ordering, unlike fence_token which advances on every renewal.
    pub fn acquisition_revision(&self) -> u64 {
        self.acquisition_revision
    }
}
