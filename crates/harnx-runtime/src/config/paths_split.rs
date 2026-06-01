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

    pub fn mcp_servers_dir() -> PathBuf {
        paths::mcp_servers_dir()
    }

    pub fn acp_servers_dir() -> PathBuf {
        paths::acp_servers_dir()
    }

    pub fn macro_file(name: &str) -> PathBuf {
        paths::macro_file(name)
    }

    pub fn env_file() -> PathBuf {
        paths::env_file()
    }

    pub fn messages_file(&self) -> PathBuf {
        paths::messages_file(self.agent.as_ref().map(|a| a.name()))
    }

    pub fn sessions_dir(&self) -> PathBuf {
        if let Some(ref override_dir) = self.sessions_dir_override {
            return override_dir.clone();
        }
        paths::sessions_dir(self.agent.as_ref().map(|a| a.name()))
    }

    pub fn rags_dir() -> PathBuf {
        paths::rags_dir()
    }

    pub fn session_file(&self, name: &str) -> PathBuf {
        match name.split_once('/') {
            Some((sub, leaf)) => self.sessions_dir().join(sub).join(format!("{leaf}.yaml")),
            None => self.sessions_dir().join(format!("{name}.yaml")),
        }
    }

    /// Atomically claim a short session ID by creating its stub file with
    /// `create_new(true)`. Returns `Ok(true)` if the claim succeeded, `Ok(false)`
    /// if another process already claimed the same ID (caller should retry with a
    /// different ID), or `Err` for unexpected I/O failures.
    fn claim_session_file(&self, id: &str) -> Result<bool> {
        let path = self.session_file(id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create sessions dir at {}", parent.display())
            })?;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e)
                .with_context(|| format!("Failed to claim session ID file at {}", path.display())),
        }
    }

    /// Generate a unique short session ID and atomically claim it on disk.
    /// Retries with the next second's timestamp if the claim loses a race.
    pub fn new_session_id(&self) -> Result<String> {
        loop {
            let candidate =
                crate::utils::session_name::generate_session_id(|c| self.session_file(c).exists());
            if self.claim_session_file(&candidate)? {
                return Ok(candidate);
            }
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
