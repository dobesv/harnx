//! Environment-variable loading extracted from config/mod.rs for code health.
use super::*;
use std::env;

impl Config {
    pub(super) fn load_envs(&mut self) {
        if let Ok(v) = env::var(get_env_name("model")) {
            self.model_id = v;
        }
        if let Some(v) = read_env_value::<f64>(&get_env_name("temperature")) {
            self.temperature = v;
        }
        if let Some(v) = read_env_value::<f64>(&get_env_name("top_p")) {
            self.top_p = v;
        }

        if let Some(Some(v)) = read_env_bool(&get_env_name("dry_run")) {
            self.dry_run = v;
        }
        if let Some(Some(v)) = read_env_bool(&get_env_name("stream")) {
            self.stream = v;
        }
        if let Some(Some(v)) = read_env_bool(&get_env_name("save")) {
            self.save = v;
        }
        if let Ok(v) = env::var(get_env_name("keybindings")) {
            if v == "vi" {
                self.keybindings = v;
            }
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("editor")) {
            self.editor = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("wrap")) {
            self.wrap = v;
        }
        if let Some(Some(v)) = read_env_bool(&get_env_name("wrap_code")) {
            self.wrap_code = v;
        }

        if let Some(Some(v)) = read_env_bool(&get_env_name("tool_use")) {
            self.tool_use = v;
        }
        if let Ok(v) = env::var(get_env_name("toolsets")) {
            if let Ok(v) = parse_toolsets_json(&v) {
                self.toolsets = v;
            }
        }
        if let Ok(v) = env::var(get_env_name("use_tools")) {
            if v == "null" {
                self.use_tools = None;
            } else {
                self.use_tools = Some(
                    split_tool_selectors(&v)
                        .into_iter()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect(),
                );
            }
        }

        if let Some(v) = read_env_bool(&get_env_name("save_session")) {
            self.save_session = v;
        }
        if let Some(v) = read_env_value::<u64>(&get_env_name("cleanup_inactive_sessions_days")) {
            self.cleanup_inactive_sessions_days = v;
        }
        if let Some(v) = read_env_value::<u64>(&get_env_name("cleanup_remote_sessions_days")) {
            self.cleanup_remote_sessions_days = v;
        }
        if let Some(Some(v)) = read_env_value::<usize>(&get_env_name("compress_threshold")) {
            self.compress_threshold = v;
        }

        if let Some(v) = read_env_value::<String>(&get_env_name("rag_embedding_model")) {
            self.rag_embedding_model = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("rag_reranker_model")) {
            self.rag_reranker_model = v;
        }
        if let Some(Some(v)) = read_env_value::<usize>(&get_env_name("rag_top_k")) {
            self.rag_top_k = v;
        }
        if let Some(v) = read_env_value::<usize>(&get_env_name("rag_chunk_size")) {
            self.rag_chunk_size = v;
        }
        if let Some(v) = read_env_value::<usize>(&get_env_name("rag_chunk_overlap")) {
            self.rag_chunk_overlap = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("rag_template")) {
            self.rag_template = v;
        }

        if let Ok(v) = env::var(get_env_name("document_loaders")) {
            if let Ok(v) = serde_json::from_str(&v) {
                self.document_loaders = v;
            }
        }

        if let Some(Some(v)) = read_env_bool(&get_env_name("highlight")) {
            self.highlight = v;
        }
        if *NO_COLOR {
            self.highlight = false;
        }
        if self.highlight && self.theme.is_none() {
            if let Some(v) = read_env_value::<String>(&get_env_name("theme")) {
                self.theme = v;
            } else if *IS_STDOUT_TERMINAL {
                if let Ok(mode) = theme_mode(QueryOptions::default()) {
                    let theme = match mode {
                        ThemeMode::Dark => "dark",
                        ThemeMode::Light => "light",
                    };
                    self.theme = Some(theme.into());
                }
            }
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("serve_addr")) {
            self.serve_addr = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("user_agent")) {
            self.user_agent = v;
        }
        if let Some(Some(v)) = read_env_bool(&get_env_name("save_shell_history")) {
            self.save_shell_history = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("sync_models_url")) {
            self.sync_models_url = v;
        }
    }
}

pub fn load_env_file() -> Result<()> {
    let env_file_path = Config::env_file();
    let contents = match read_to_string(&env_file_path) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    debug!("Use env file '{}'", env_file_path.display());
    #[cfg(unix)]
    {
        use std::os::unix::prelude::PermissionsExt;
        let _ = std::fs::set_permissions(&env_file_path, std::fs::Permissions::from_mode(0o600));
    }
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if !key.is_empty() {
                if matches!(key, "HARNX_LOG_LEVEL" | "HARNX_LOG_PATH") && env::var_os(key).is_some()
                {
                    continue;
                }
                unsafe {
                    env::set_var(key, value.trim());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RAII guard that removes an env var on drop.
    #[test]
    fn load_envs_reads_cleanup_remote_sessions_days() {
        let _lock = crate::config::test_support::env_lock();
        let prev = std::env::var_os("HARNX_CLEANUP_REMOTE_SESSIONS_DAYS");
        // SAFETY: test-only; global test lock held.
        unsafe { std::env::set_var("HARNX_CLEANUP_REMOTE_SESSIONS_DAYS", "7") };

        let mut config = Config::default();
        config.load_envs();

        assert_eq!(config.cleanup_remote_sessions_days, Some(7));

        // Restore prior state
        match prev {
            Some(v) => unsafe { std::env::set_var("HARNX_CLEANUP_REMOTE_SESSIONS_DAYS", v) },
            None => unsafe { std::env::remove_var("HARNX_CLEANUP_REMOTE_SESSIONS_DAYS") },
        }
    }

    #[test]
    fn load_envs_unset_cleanup_remote_sessions_days_is_none() {
        let _lock = crate::config::test_support::env_lock();
        let prev = std::env::var_os("HARNX_CLEANUP_REMOTE_SESSIONS_DAYS");
        // SAFETY: test-only; global test lock held.
        unsafe { std::env::remove_var("HARNX_CLEANUP_REMOTE_SESSIONS_DAYS") };

        let mut config = Config::default();
        config.load_envs();

        assert_eq!(config.cleanup_remote_sessions_days, None);

        // Restore prior state
        if let Some(v) = prev {
            unsafe { std::env::set_var("HARNX_CLEANUP_REMOTE_SESSIONS_DAYS", v) }
        }
    }

    #[test]
    fn load_env_file_does_not_override_inherited_harnx_log_vars() {
        let _lock = crate::config::test_support::env_lock();
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            temp.path().join(".env"),
            "HARNX_LOG_LEVEL=info\nHARNX_LOG_PATH=from-dot-env.log\nOTHER_VAR=ok\n",
        )
        .unwrap();

        let prev_cwd = std::env::current_dir().unwrap();
        let prev_level = std::env::var_os("HARNX_LOG_LEVEL");
        let prev_path = std::env::var_os("HARNX_LOG_PATH");
        let prev_other = std::env::var_os("OTHER_VAR");

        let result = {
            unsafe {
                std::env::set_var("HARNX_LOG_LEVEL", "debug");
                std::env::set_var("HARNX_LOG_PATH", "/tmp/inherited.log");
                std::env::remove_var("OTHER_VAR");
            }
            std::env::set_current_dir(temp.path()).unwrap();

            load_env_file().unwrap();

            let level = std::env::var("HARNX_LOG_LEVEL").unwrap();
            let path = std::env::var("HARNX_LOG_PATH").unwrap();
            let other = std::env::var("OTHER_VAR").ok();
            (level, path, other)
        };

        std::env::set_current_dir(prev_cwd).unwrap();
        match prev_level {
            Some(value) => unsafe { std::env::set_var("HARNX_LOG_LEVEL", value) },
            None => unsafe { std::env::remove_var("HARNX_LOG_LEVEL") },
        }
        match prev_path {
            Some(value) => unsafe { std::env::set_var("HARNX_LOG_PATH", value) },
            None => unsafe { std::env::remove_var("HARNX_LOG_PATH") },
        }
        match prev_other {
            Some(value) => unsafe { std::env::set_var("OTHER_VAR", value) },
            None => unsafe { std::env::remove_var("OTHER_VAR") },
        }

        assert_eq!(result.0, "debug");
        assert_eq!(result.1, "/tmp/inherited.log");
    }
}
