//! Filesystem path/dir accessors extracted from config/mod.rs for code health.
use super::*;

/// Agent/session identity used to resolve one local attachment-cache directory.
pub struct SessionAttachmentPath<'a> {
    pub agent_name: &'a str,
    pub session_id: &'a str,
}

impl SessionAttachmentPath<'_> {
    fn has_safe_session_id(&self) -> bool {
        if self.session_id.is_empty() {
            return false;
        }
        if self.session_id.contains(['/', '\\']) {
            return false;
        }
        Path::new(self.session_id)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    }
}

impl Config {
    pub fn config_dir() -> PathBuf {
        paths::config_dir()
    }

    pub fn local_path(name: &str) -> PathBuf {
        paths::local_path(name)
    }

    pub fn config_file() -> PathBuf {
        paths::config_file()
    }

    pub fn macros_dir() -> PathBuf {
        paths::macros_dir()
    }

    pub fn clients_dir() -> PathBuf {
        paths::clients_dir()
    }

    pub fn tool_servers_dir() -> PathBuf {
        paths::tool_servers_dir()
    }

    pub fn macro_file(name: &str) -> PathBuf {
        paths::macro_file(name)
    }

    pub fn nats_server_file(name: &str) -> PathBuf {
        paths::nats_server_file(name)
    }

    pub fn env_file() -> PathBuf {
        paths::env_file()
    }

    pub fn messages_file(&self) -> PathBuf {
        paths::messages_file(self.agent.as_ref().map(|a| a.name()))
    }

    pub fn rags_dir() -> PathBuf {
        paths::rags_dir()
    }

    /// Atomically reserve a short session ID by creating its complete canonical
    /// metadata record without holding the configuration lock across network
    /// I/O. The reserved session may remain transcript-empty until its first
    /// turn.
    pub async fn reserve_new_session_id(config: &GlobalConfig) -> Result<String> {
        const RESERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
        let config = config.read().clone();
        tokio::time::timeout(RESERVATION_TIMEOUT, config.reserve_new_session_id_inner())
            .await
            .context("Timed out reserving a new NATS session ID")?
    }

    async fn reserve_new_session_id_inner(&self) -> Result<String> {
        const LOCAL_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

        // Validate and snapshot the initializer before entering the local
        // broker handoff retry loop. Configuration errors are deterministic;
        // retrying them would turn an actionable failure into a timeout.
        let initializer = crate::nats_session_metadata::SessionInitializer::from_config(self)?;

        let cluster = self
            .remote_agent
            .as_ref()
            .map(|(_, cluster)| cluster.as_str())
            .unwrap_or(LOCAL_CLUSTER_KEY);
        if cluster != LOCAL_CLUSTER_KEY {
            return self
                .reserve_new_session_id_once(cluster, &initializer)
                .await;
        }

        // The embedded broker is owned by one of the connected processes. It
        // can legitimately disappear between discovery and the KV operation
        // when that process exits, so rediscover and retry during that narrow
        // handoff rather than surfacing a transient no-responders error.
        loop {
            match self
                .reserve_new_session_id_once(cluster, &initializer)
                .await
            {
                Ok(session_id) => return Ok(session_id),
                Err(_) => tokio::time::sleep(LOCAL_RETRY_DELAY).await,
            }
        }
    }

    async fn reserve_new_session_id_once(
        &self,
        cluster: &str,
        initializer: &crate::nats_session_metadata::SessionInitializer,
    ) -> Result<String> {
        const LOCAL_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

        let server = self.resolve_nats_server(cluster).await?;
        if cluster == LOCAL_CLUSTER_KEY {
            tokio::time::timeout(
                LOCAL_OPERATION_TIMEOUT,
                self.reserve_new_session_id_on_server(&server, initializer),
            )
            .await
            .context("Timed out reserving an ID on the shared local NATS server")?
        } else {
            self.reserve_new_session_id_on_server(&server, initializer)
                .await
        }
    }

    async fn reserve_new_session_id_on_server(
        &self,
        server: &NatsServerConfig,
        initializer: &crate::nats_session_metadata::SessionInitializer,
    ) -> Result<String> {
        let client = Self::connect_nats_server(server).await?;
        let jetstream = async_nats::jetstream::new(client);
        let store = crate::nats_session_metadata::SessionMetadataStore::ensure(
            &jetstream,
            server.replicas.unwrap_or(1),
        )
        .await?;
        let mut seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        loop {
            let candidate = crate::utils::session_name::encode_timestamp_session_id(seconds);
            let metadata =
                crate::nats_session_metadata::SessionMetadata::new(&candidate, initializer.clone());
            if store.create(&metadata).await?.is_some() {
                return Ok(candidate);
            }
            seconds = seconds.saturating_add(1);
        }
    }

    pub fn rag_file(&self, name: &str) -> PathBuf {
        paths::rag_file(self.agent.as_ref().map(|a| a.name()), name)
    }

    pub fn agents_data_dir() -> PathBuf {
        paths::agents_data_dir()
    }

    /// Root dir for per-agent instruction files (.md) — lives in config dir.
    pub fn agents_config_dir() -> PathBuf {
        paths::agents_config_dir()
    }

    pub fn agent_data_dir(name: &str) -> PathBuf {
        paths::agent_data_dir(name)
    }

    /// Resolve a session attachment directory only for a safe session ID.
    pub fn session_attachments_dir(location: SessionAttachmentPath<'_>) -> Option<PathBuf> {
        if !location.has_safe_session_id() {
            return None;
        }
        Some(
            Self::agent_data_dir(location.agent_name)
                .join("attachments")
                .join(location.session_id),
        )
    }

    pub fn agent_rag_file(agent_name: &str, rag_name: &str) -> PathBuf {
        paths::agent_rag_file(agent_name, rag_name)
    }

    pub fn agent_file(name: &str) -> PathBuf {
        if let Some((pkg, stem)) = name.split_once('/') {
            paths::package_dir(pkg)
                .join(paths::AGENTS_DIR_NAME)
                .join(format!("{stem}.md"))
        } else {
            paths::agents_config_dir().join(format!("{name}.md"))
        }
    }

    pub fn models_override_file() -> PathBuf {
        paths::models_override_file()
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, SessionAttachmentPath};

    #[test]
    fn session_attachment_paths_reject_unsafe_ids() {
        let path = |session_id| SessionAttachmentPath {
            agent_name: "agent",
            session_id,
        };
        assert!(Config::session_attachments_dir(path("session-1")).is_some());
        for session_id in ["", ".", "..", "../escape", "/tmp/escape", "a/b", "a\\b"] {
            assert!(
                Config::session_attachments_dir(path(session_id)).is_none(),
                "unsafe session ID should be rejected: {session_id}"
            );
        }
    }
}
