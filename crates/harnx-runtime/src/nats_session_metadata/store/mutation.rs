use super::*;
use anyhow::{bail, Context, Result};
use async_nats::jetstream::kv;
use chrono::Utc;

impl SessionMetadataStore {
    pub async fn apply_patch(
        &self,
        session_id: &str,
        patch: SessionMetadataPatch,
    ) -> Result<MetadataRecord> {
        self.patch(session_id, |metadata| apply_typed_patch(metadata, &patch))
            .await
    }

    pub async fn apply_patch_for_agent(
        &self,
        session_id: &str,
        agent: &str,
        patch: SessionMetadataPatch,
    ) -> Result<MetadataRecord> {
        self.patch_guarded(session_id, PatchGuard::for_agent(agent), |metadata| {
            apply_typed_patch(metadata, &patch)
        })
        .await
    }

    pub async fn apply_override(
        &self,
        session_id: &str,
        update: SessionOverrideUpdate,
    ) -> Result<MetadataRecord> {
        self.patch(session_id, |metadata| {
            update.apply(&mut metadata.overrides);
            Ok(())
        })
        .await
    }
}

fn apply_typed_patch(metadata: &mut SessionMetadata, patch: &SessionMetadataPatch) -> Result<()> {
    if let Some(title) = &patch.title {
        metadata.title.value = title
            .value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        metadata.title.manual = metadata.title.value.is_some() && title.manual;
        if metadata.title.value.is_none() {
            metadata.title.last_updated_tokens = 0;
        }
    }
    if let Some(variables) = &patch.variables {
        metadata.variables = variables.clone();
    }
    if let Some(overrides) = &patch.overrides {
        metadata.overrides = overrides.clone();
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
pub(in crate::nats_session_metadata) struct PatchGuard<'a> {
    expected_agent: Option<&'a str>,
    worker_fence_token: Option<u64>,
}

impl<'a> PatchGuard<'a> {
    pub(super) fn for_agent(agent: &'a str) -> Self {
        Self {
            expected_agent: Some(agent),
            ..Self::default()
        }
    }

    pub(in crate::nats_session_metadata) fn for_worker(fence_token: u64) -> Self {
        Self {
            worker_fence_token: Some(fence_token),
            ..Self::default()
        }
    }

    fn validate(self, metadata: &SessionMetadata) -> Result<()> {
        if self
            .expected_agent
            .is_some_and(|agent| !metadata_belongs_to_agent(metadata, agent))
        {
            bail!("Not Found")
        }
        if let Some(fence_token) = self.worker_fence_token {
            anyhow::ensure!(
                fence_token >= metadata.worker_fence_token,
                "stale session metadata writer fence {fence_token}; latest committed fence is {}",
                metadata.worker_fence_token
            );
        }
        Ok(())
    }

    fn apply_fence(self, metadata: &mut SessionMetadata) {
        if let Some(fence_token) = self.worker_fence_token {
            metadata.worker_fence_token = fence_token;
        }
    }
}

impl SessionMetadataStore {
    pub async fn patch<F>(&self, session_id: &str, mut patch: F) -> Result<MetadataRecord>
    where
        F: FnMut(&mut SessionMetadata) -> Result<()>,
    {
        self.patch_guarded(session_id, PatchGuard::default(), &mut patch)
            .await
    }

    pub async fn patch_with_fence<F>(
        &self,
        session_id: &str,
        fence_token: u64,
        patch: F,
    ) -> Result<MetadataRecord>
    where
        F: FnMut(&mut SessionMetadata) -> Result<()>,
    {
        self.patch_guarded(session_id, PatchGuard::for_worker(fence_token), patch)
            .await
    }

    pub(super) async fn patch_guarded<F>(
        &self,
        session_id: &str,
        guard: PatchGuard<'_>,
        mut patch: F,
    ) -> Result<MetadataRecord>
    where
        F: FnMut(&mut SessionMetadata) -> Result<()>,
    {
        self.patch_guarded_if_changed(session_id, guard, |metadata| {
            patch(metadata)?;
            Ok(true)
        })
        .await
    }

    pub(in crate::nats_session_metadata) async fn patch_guarded_if_changed<F>(
        &self,
        session_id: &str,
        guard: PatchGuard<'_>,
        mut patch: F,
    ) -> Result<MetadataRecord>
    where
        F: FnMut(&mut SessionMetadata) -> Result<bool>,
    {
        for attempt in 0..CAS_RETRY_LIMIT {
            let mut record = self.record_for_patch(session_id, guard).await?;
            let immutable = immutable_identity(&record.metadata);
            let changed = patch(&mut record.metadata)?;
            let fence_advanced = guard
                .worker_fence_token
                .is_some_and(|token| token > record.metadata.worker_fence_token);
            if !changed && !fence_advanced {
                return Ok(record);
            }
            anyhow::ensure!(
                immutable == immutable_identity(&record.metadata),
                "session identity, agent source, schema version, and creation time are immutable"
            );
            guard.apply_fence(&mut record.metadata);
            record.metadata.validate(session_id)?;
            match self
                .update_metadata(&record.metadata, record.revision)
                .await
            {
                Ok(revision) => {
                    self.publish_invalidation(session_id, revision).await;
                    return Ok(MetadataRecord {
                        metadata: record.metadata,
                        revision,
                    });
                }
                Err(error) if is_wrong_last_revision(&error) && attempt + 1 < CAS_RETRY_LIMIT => {
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded CAS loop always returns")
    }

    async fn record_for_patch(
        &self,
        session_id: &str,
        guard: PatchGuard<'_>,
    ) -> Result<MetadataRecord> {
        let record = self.get(session_id).await?;
        let record = match (record, guard.expected_agent) {
            (Some(record), _) => record,
            (None, Some(_)) => bail!("Not Found"),
            (None, None) => bail!("session metadata '{session_id}' not found"),
        };
        guard.validate(&record.metadata)?;
        Ok(record)
    }

    async fn update_metadata(&self, metadata: &SessionMetadata, revision: u64) -> Result<u64> {
        let key = metadata_key(&metadata.session_id);
        let payload = serde_json::to_vec(metadata).with_context(|| {
            format!(
                "Failed to serialize session metadata '{}'",
                metadata.session_id
            )
        })?;
        self.store
            .update(&key, payload.into(), revision)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| {
                format!("Failed to CAS-update session metadata '{key}' at revision {revision}")
            })
    }

    async fn publish_invalidation(&self, session_id: &str, revision: u64) {
        let subject = invalidation_subject(session_id);
        let payload = serde_json::json!({
            "session_id": session_id,
            "revision": revision,
        });
        if let Err(error) = self
            .client
            .publish(subject, payload.to_string().into())
            .await
        {
            log::warn!(
                "failed to publish session metadata invalidation: session_id={session_id} error={error:#}"
            );
        }
    }
}

fn immutable_identity(
    metadata: &SessionMetadata,
) -> (u32, String, chrono::DateTime<Utc>, SessionAgentSource) {
    (
        metadata.schema_version,
        metadata.session_id.clone(),
        metadata.created_at,
        metadata.agent.clone(),
    )
}

pub(super) fn is_wrong_last_revision(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<kv::UpdateError>())
        .is_some_and(|error| error.kind() == kv::UpdateErrorKind::WrongLastRevision)
}
