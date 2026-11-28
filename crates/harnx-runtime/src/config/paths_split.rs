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
        Self::reserve_new_session_id_with_abort(config, None).await
    }

    /// Cancellable wait for managed-local readiness before reserving an ID.
    pub async fn reserve_new_session_id_with_abort(
        config: &GlobalConfig,
        abort: Option<&AbortSignal>,
    ) -> Result<String> {
        let config = config.read().clone();
        let initializer = crate::SessionInitializer::from_config(&config)?;
        config
            .reserve_new_session_id_with_timeout(initializer, abort)
            .await
    }

    /// Reserve using caller-supplied initial properties, such as a request identity.
    /// Explicit properties win over the destination cluster and global defaults.
    pub async fn reserve_new_session_id_with_initializer(
        config: &GlobalConfig,
        initializer: crate::SessionInitializer,
    ) -> Result<String> {
        let config = config.read().clone();
        config
            .reserve_new_session_id_with_timeout(initializer, None)
            .await
    }

    async fn reserve_new_session_id_with_timeout(
        &self,
        initializer: crate::SessionInitializer,
        abort: Option<&AbortSignal>,
    ) -> Result<String> {
        const RESERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
        let cluster = self.remote_agent.as_ref().map_or_else(
            || self.default_cluster_key(),
            |(_, cluster)| cluster.as_str(),
        );
        // Use the authenticated connection acquired here inside the timeout.
        // A fresh connect inside it would race another owner handoff.
        let prepared = self.prepare_managed_local(cluster, abort).await?;
        // Test-only budget seam (see `test_env_duration_ms`): shrinks the
        // reservation cutoff so e2e tests can prove it fires without
        // multi-second sleeps. The acquisition boundary above is unchanged.
        let reservation_timeout = crate::utils::test_env_duration_ms(
            "HARNX_TEST_RESERVATION_TIMEOUT_MS",
            RESERVATION_TIMEOUT,
        );
        tokio::time::timeout(
            reservation_timeout,
            self.reserve_new_session_id_inner(cluster, &initializer, prepared.as_ref()),
        )
        .await
        .context("Timed out reserving a new NATS session ID")?
    }

    async fn reserve_new_session_id_inner(
        &self,
        cluster: &str,
        initializer: &crate::SessionInitializer,
        prepared: Option<&(NatsServerConfig, async_nats::Client)>,
    ) -> Result<String> {
        const LOCAL_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);
        const LOCAL_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
        // Inside the reservation budget: only the test stall seam runs before
        // the KV round trips, so a stalled reservation proves the budget
        // enforces after readiness.
        crate::utils::test_env_sleep_ms("HARNX_TEST_RESERVATION_STALL_MS").await;
        if cluster != LOCAL_CLUSTER_KEY {
            return self
                .reserve_new_session_id_once(cluster, initializer, prepared)
                .await;
        }

        // Keep the existing bounded handoff retry for the KV operation. The
        // ready client reconnects across owner loss without repeating startup.
        loop {
            let attempt = self.reserve_new_session_id_once(cluster, initializer, prepared);
            if let Ok(Ok(session_id)) = tokio::time::timeout(LOCAL_OPERATION_TIMEOUT, attempt).await
            {
                return Ok(session_id);
            }
            tokio::time::sleep(LOCAL_RETRY_DELAY).await;
        }
    }

    async fn reserve_new_session_id_once(
        &self,
        cluster: &str,
        initializer: &crate::SessionInitializer,
        prepared: Option<&(NatsServerConfig, async_nats::Client)>,
    ) -> Result<String> {
        if let Some((server, client)) = prepared {
            return self
                .reserve_new_session_id_on_server(server, client.clone(), initializer)
                .await;
        }
        let (server, client) = self.resolve_nats_connection(cluster, None).await?;
        self.reserve_new_session_id_on_server(&server, client, initializer)
            .await
    }

    async fn reserve_new_session_id_on_server(
        &self,
        server: &NatsServerConfig,
        client: async_nats::Client,
        initializer: &crate::SessionInitializer,
    ) -> Result<String> {
        let jetstream = async_nats::jetstream::new(client);
        let store = crate::nats_session_metadata::SessionMetadataStore::ensure(
            &jetstream,
            server.replicas.unwrap_or(1),
        )
        .await?;
        let initializer = initializer
            .clone()
            .with_default_user_id(self.default_user_id(Some(server)));
        crate::utils::session_name::reserve_short_session_id(&store, &initializer).await
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
