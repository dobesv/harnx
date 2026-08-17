//! Filesystem path/dir accessors extracted from config/mod.rs for code health.
use super::*;

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

    /// Atomically reserve a short session ID in NATS without holding the
    /// configuration lock across network I/O.
    pub async fn reserve_new_session_id(config: &GlobalConfig) -> Result<String> {
        const RESERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
        let config = config.read().clone();
        tokio::time::timeout(RESERVATION_TIMEOUT, config.reserve_new_session_id_inner())
            .await
            .context("Timed out reserving a new NATS session ID")?
    }

    async fn reserve_new_session_id_inner(&self) -> Result<String> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let cluster = self
            .remote_agent
            .as_ref()
            .map(|(_, cluster)| cluster.as_str())
            .unwrap_or(LOCAL_CLUSTER_KEY);
        let server = self.resolve_nats_server(cluster).await?;
        let client = Self::connect_nats_server(&server).await?;
        let store = crate::nats_session_index::ensure_index_bucket(
            &async_nats::jetstream::new(client),
            server.replicas.unwrap_or(1),
        )
        .await?;
        let activity_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut seconds = activity_seconds;

        loop {
            let candidate = crate::utils::session_name::encode_timestamp_session_id(seconds);
            let record = crate::nats_session_index::SessionIndexRecord {
                session_id: candidate.clone(),
                agent_name: self
                    .agent
                    .as_ref()
                    .map(|agent| agent.name().to_string())
                    .unwrap_or_default(),
                working_dir: std::env::current_dir()
                    .ok()
                    .map(|path| path.to_string_lossy().into_owned()),
                git_branch: Some(crate::utils::session_name::git_branch())
                    .filter(|branch| !branch.is_empty()),
                git_remote: crate::utils::session_name::git_remote(),
                title: None,
                last_activity: activity_seconds,
            };
            if crate::nats_session_index::try_create_record(&store, &record).await? {
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
