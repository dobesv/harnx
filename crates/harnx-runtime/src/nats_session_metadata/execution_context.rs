use super::{store::PatchGuard, MetadataRecord, SessionMetadata, SessionMetadataStore};
use anyhow::{Context, Result};
use harnx_core::execution_context::{
    ExecutionContextExtension, ExecutionContextObservation, EXECUTION_CONTEXT_NAMESPACE,
};

pub fn execution_contexts(metadata: &SessionMetadata) -> Result<Vec<ExecutionContextObservation>> {
    let Some(value) = metadata.extensions.get(EXECUTION_CONTEXT_NAMESPACE) else {
        return Ok(Vec::new());
    };
    Ok(ExecutionContextExtension::from_value(value.clone())?.contexts)
}

impl SessionMetadataStore {
    pub async fn merge_execution_contexts(
        &self,
        session_id: &str,
        observations: &[ExecutionContextObservation],
    ) -> Result<MetadataRecord> {
        self.merge_execution_contexts_guarded(session_id, None, observations)
            .await
    }

    pub async fn merge_execution_contexts_with_fence(
        &self,
        session_id: &str,
        fence_token: u64,
        observations: &[ExecutionContextObservation],
    ) -> Result<MetadataRecord> {
        self.merge_execution_contexts_guarded(session_id, Some(fence_token), observations)
            .await
    }

    async fn merge_execution_contexts_guarded(
        &self,
        session_id: &str,
        fence_token: Option<u64>,
        observations: &[ExecutionContextObservation],
    ) -> Result<MetadataRecord> {
        for observation in observations {
            observation.validate()?;
        }
        let observations = observations.to_vec();
        let guard = fence_token.map_or_else(PatchGuard::default, PatchGuard::for_worker);
        self.patch_guarded_if_changed(session_id, guard, move |metadata| {
            let mut extension = match metadata.extensions.get(EXECUTION_CONTEXT_NAMESPACE) {
                Some(value) => ExecutionContextExtension::from_value(value.clone())
                    .context("read retained execution contexts")?,
                None => ExecutionContextExtension::default(),
            };
            let mut changed = false;
            for observation in &observations {
                changed |= extension.merge(observation.clone());
            }
            if changed {
                metadata.extensions.insert(
                    EXECUTION_CONTEXT_NAMESPACE.to_string(),
                    serde_json::to_value(extension)?,
                );
            }
            Ok(changed)
        })
        .await
    }
}
