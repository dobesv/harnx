//! Session binding and authorization against the resolved export.
use super::{assert_local_id_no_dot, A2aStore, StoreError};
use crate::{exports::Export, identity::Principal};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};
use harnx_core::access_rules::AccessRules;
use harnx_runtime::nats_session_metadata::{is_cas_conflict, TaskIndex};
use serde::{Deserialize, Serialize};

/// Extension namespace for the A2A session binding in metadata.extensions.
pub const A2A_BINDING_NAMESPACE: &str = "dev.harnx.a2a";

/// Version of the A2A binding schema.
pub const A2A_BINDING_VERSION: u32 = 1;

/// Session binding stored in `dev.harnx.a2a` extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aBinding {
    pub version: u32,
    pub export: String,
    pub agent: String,
    pub cluster: String,
    pub owner: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Check the session binding against the resolved export and owner.
/// Mismatched exports and unauthorized owners look like missing contexts.
pub fn validate_binding(
    binding: &A2aBinding,
    export: &Export,
    owner: &Principal,
    access_rules: Option<&AccessRules>,
) -> bool {
    binding_matches_export(binding, export)
        && match access_rules {
            Some(rules) => rules.can_access_session(
                &export.agent_ref(),
                &owner.user_id().into_iter().collect::<Vec<_>>(),
                binding.owner.as_deref(),
            ),
            None => binding.owner.as_deref() == owner.user_id(),
        }
}

fn binding_matches_export(binding: &A2aBinding, export: &Export) -> bool {
    let expected = (
        A2A_BINDING_VERSION,
        export.public_name.as_str(),
        export.agent.as_str(),
        export.cluster.as_deref().unwrap_or("__local__"),
    );
    let actual = (
        binding.version,
        binding.export.as_str(),
        binding.agent.as_str(),
        binding.cluster.as_str(),
    );
    actual == expected
}

impl A2aStore {
    /// Resolve and authorize before resuming a runtime session. Never creates metadata.
    pub async fn resolve_context(
        &self,
        export: &Export,
        owner: &Principal,
        local_id: &str,
    ) -> Result<Option<String>> {
        let key = harnx_core::session_identity::session_key(Some(&export.agent), local_id);
        Ok(self
            .get_binding(&key)
            .await?
            .filter(|binding| validate_binding(binding, export, owner, self.access_rules()))
            .map(|_| key))
    }

    /// Get the session binding from metadata extensions.
    pub async fn get_binding(&self, storage_key: &str) -> Result<Option<A2aBinding>> {
        let Some(record) = self.store.get(storage_key).await? else {
            return Ok(None);
        };
        record
            .metadata
            .extensions
            .get(A2A_BINDING_NAMESPACE)
            .map(|value| {
                serde_json::from_value(value.clone())
                    .with_context(|| format!("Invalid A2A binding in {}", storage_key))
            })
            .transpose()
    }

    /// Write the A2A binding to a new session's extensions.
    pub async fn bind_context(
        &self,
        storage_key: &str,
        export: &Export,
        owner: &Principal,
    ) -> Result<()> {
        let mut record = self
            .store
            .get(storage_key)
            .await?
            .ok_or(StoreError::NotFound)?;
        let binding = A2aBinding {
            version: A2A_BINDING_VERSION,
            export: export.public_name.clone(),
            agent: export.agent.clone(),
            cluster: export.cluster.as_deref().unwrap_or("__local__").into(),
            owner: owner.user_id().map(str::to_owned),
            created_at: Utc::now(),
        };
        self.write_binding(storage_key, &mut record, &binding)
            .await?;
        // New contexts have no legacy tasks. Never overwrite a concurrently created index.
        if let Err(error) = self
            .store
            .put_a2a_task_index(storage_key, &TaskIndex::new(), None)
            .await
        {
            if !is_cas_conflict(&error) {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Low-level write-once binding patch; only call during session creation.
    pub async fn write_binding(
        &self,
        storage_key: &str,
        record: &mut harnx_runtime::nats_session_metadata::MetadataRecord,
        binding: &A2aBinding,
    ) -> Result<()> {
        ensure!(
            storage_key == record.metadata.storage_key(),
            "binding storage key mismatch"
        );
        ensure!(
            binding.agent == record.metadata.agent.name().unwrap_or_default(),
            "binding agent mismatch"
        );
        ensure!(
            binding.version == A2A_BINDING_VERSION,
            "unsupported binding version"
        );
        assert_local_id_no_dot(&record.metadata.session_id)?;
        let binding_json = serde_json::to_value(binding)?;
        // The runtime creates session metadata before the frontend binds it.
        // Check absence inside the CAS closure so a stale caller cannot rebind it.
        *record = self
            .store
            .patch(storage_key, |metadata| {
                ensure!(
                    !metadata.extensions.contains_key(A2A_BINDING_NAMESPACE),
                    "A2A binding already exists"
                );
                metadata
                    .extensions
                    .insert(A2A_BINDING_NAMESPACE.into(), binding_json.clone());
                Ok(())
            })
            .await?;

        Ok(())
    }
}
