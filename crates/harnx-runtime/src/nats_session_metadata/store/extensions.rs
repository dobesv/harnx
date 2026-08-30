use super::{extension_validation::validate_mutable_namespace, mutation::PatchGuard, *};
use anyhow::Result;
use serde_json::Value;

pub struct SessionExtensionUpdate<'a> {
    pub namespace: &'a str,
    pub value: Value,
}

impl SessionMetadataStore {
    pub async fn replace_extension(
        &self,
        session_id: &str,
        namespace: &str,
        value: Value,
    ) -> Result<MetadataRecord> {
        self.replace_extension_guarded(
            session_id,
            None,
            SessionExtensionUpdate { namespace, value },
        )
        .await
    }

    pub async fn replace_extension_for_agent(
        &self,
        session_id: &str,
        agent: &str,
        update: SessionExtensionUpdate<'_>,
    ) -> Result<MetadataRecord> {
        self.replace_extension_guarded(session_id, Some(agent), update)
            .await
    }

    async fn replace_extension_guarded(
        &self,
        session_id: &str,
        agent: Option<&str>,
        update: SessionExtensionUpdate<'_>,
    ) -> Result<MetadataRecord> {
        let SessionExtensionUpdate { namespace, value } = update;
        validate_mutable_namespace(namespace)?;
        let namespace_size = serde_json::to_vec(&value)?.len();
        anyhow::ensure!(
            namespace_size <= EXTENSION_NAMESPACE_MAX_BYTES,
            "extension namespace '{namespace}' exceeds {} bytes",
            EXTENSION_NAMESPACE_MAX_BYTES
        );
        self.patch_guarded(
            session_id,
            agent.map_or_else(PatchGuard::default, PatchGuard::for_agent),
            |metadata| {
                metadata
                    .extensions
                    .insert(namespace.to_string(), value.clone());
                Ok(())
            },
        )
        .await
    }

    pub async fn delete_extension(
        &self,
        session_id: &str,
        namespace: &str,
    ) -> Result<MetadataRecord> {
        self.delete_extension_guarded(session_id, None, namespace)
            .await
    }

    pub async fn delete_extension_for_agent(
        &self,
        session_id: &str,
        agent: &str,
        namespace: &str,
    ) -> Result<MetadataRecord> {
        self.delete_extension_guarded(session_id, Some(agent), namespace)
            .await
    }

    async fn delete_extension_guarded(
        &self,
        session_id: &str,
        agent: Option<&str>,
        namespace: &str,
    ) -> Result<MetadataRecord> {
        validate_mutable_namespace(namespace)?;
        self.patch_guarded(
            session_id,
            agent.map_or_else(PatchGuard::default, PatchGuard::for_agent),
            |metadata| {
                metadata.extensions.remove(namespace);
                Ok(())
            },
        )
        .await
    }
}
