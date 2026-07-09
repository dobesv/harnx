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
                // Standard dotenv precedence: the ambient/inherited environment
                // always wins. The .env file only FILLS IN variables that are
                // not already set in the process environment — it never clobbers
                // an inherited value. This matches python-dotenv / node dotenv /
                // dotenvy defaults (override is opt-in, not default) and is the
                // more secure choice: operator-injected env (systemd, Docker,
                // K8s secrets) takes precedence over a user-writable file.
                // Fixes #989 (previously .env unconditionally overrode inherited
                // HARNX_LOG_* vars, which required a per-key special-case guard).
                if env::var_os(key).is_some() {
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
    fn load_env_file_ambient_env_always_wins() {
        use std::io::Write;

        let _lock = crate::config::test_support::env_lock();
        let prev_log_level = std::env::var_os("HARNX_LOG_LEVEL");
        let prev_inherited = std::env::var_os("SOME_INHERITED_VAR");
        let prev_unset_var = std::env::var_os("ENV_ONLY_VAR");
        let prev_harnx_env_file = std::env::var_os("HARNX_ENV_FILE");

        // Set inherited vars: one logging var, one arbitrary non-log var. Both
        // are already present in the ambient environment.
        // SAFETY: test-only; global test lock held.
        unsafe { std::env::set_var("HARNX_LOG_LEVEL", "debug") };
        unsafe { std::env::set_var("SOME_INHERITED_VAR", "from-ambient") };
        // Ensure the .env-only var is NOT set ambiently.
        unsafe { std::env::remove_var("ENV_ONLY_VAR") };

        // .env tries to override both inherited vars AND sets a fresh var.
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let env_file = tmpdir.path().join(".env");
        let mut f = std::fs::File::create(&env_file).expect("create .env");
        f.write_all(
            b"HARNX_LOG_LEVEL=info\nSOME_INHERITED_VAR=from-dotenv\nENV_ONLY_VAR=from-dotenv\n",
        )
        .expect("write .env");
        f.sync_all().expect("sync .env");

        unsafe { std::env::set_var("HARNX_ENV_FILE", env_file.display().to_string()) };

        load_env_file().expect("load_env_file");

        // Ambient env ALWAYS wins — .env must not clobber ANY already-set var,
        // not just the special-cased logging vars.
        assert_eq!(
            std::env::var("HARNX_LOG_LEVEL").ok(),
            Some("debug".to_string()),
            "inherited HARNX_LOG_LEVEL should win over .env"
        );
        assert_eq!(
            std::env::var("SOME_INHERITED_VAR").ok(),
            Some("from-ambient".to_string()),
            "any inherited (non-log) var should win over .env"
        );

        // A var absent from the ambient env IS filled in from .env.
        assert_eq!(
            std::env::var("ENV_ONLY_VAR").ok(),
            Some("from-dotenv".to_string()),
            "unset var should be filled in from .env"
        );

        // Restore prior state
        unsafe { std::env::remove_var("HARNX_ENV_FILE") };
        unsafe { std::env::remove_var("ENV_ONLY_VAR") };
        match prev_log_level {
            Some(v) => unsafe { std::env::set_var("HARNX_LOG_LEVEL", v) },
            None => unsafe { std::env::remove_var("HARNX_LOG_LEVEL") },
        }
        match prev_inherited {
            Some(v) => unsafe { std::env::set_var("SOME_INHERITED_VAR", v) },
            None => unsafe { std::env::remove_var("SOME_INHERITED_VAR") },
        }
        if let Some(v) = prev_unset_var {
            unsafe { std::env::set_var("ENV_ONLY_VAR", v) }
        }
        if let Some(v) = prev_harnx_env_file {
            unsafe { std::env::set_var("HARNX_ENV_FILE", v) }
        }
    }
}
