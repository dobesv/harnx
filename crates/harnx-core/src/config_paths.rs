//! Config directory path resolution for harnx.
//!
//! Owns the "where do harnx config files live" logic: the XDG / HARNX_* env
//! resolution and the file/dir name constants. See also `harnx-core::path`
//! for generic path algorithms (safe_join_path, expand_glob_paths).

use std::env;
use std::path::PathBuf;

/// Primary config file in the config dir (overridable via `HARNX_CONFIG_FILE`).
pub const CONFIG_FILE_NAME: &str = "config.yaml";

/// Subdirectory holding user-authored macros.
pub const MACROS_DIR_NAME: &str = "macros";

/// Shell env file loaded by `load_env_file`, relative to the config dir.
pub const ENV_FILE_NAME: &str = ".env";

/// Default name for the persisted "last messages" file.
pub const MESSAGES_FILE_NAME: &str = "messages.md";

/// Subdirectory holding session YAML files.
pub const SESSIONS_DIR_NAME: &str = "sessions";

/// Subdirectory holding RAG manifest files.
pub const RAGS_DIR_NAME: &str = "rags";

/// Subdirectory holding per-agent data (instructions, messages, sessions).
pub const AGENTS_DIR_NAME: &str = "agents";

/// Subdirectory holding per-client YAML files.
pub const CLIENTS_DIR_NAME: &str = "clients";

/// Subdirectory holding per-MCP-server YAML files.
pub const MCP_SERVERS_DIR_NAME: &str = "mcp_servers";

/// Subdirectory holding per-ACP-server YAML files.
pub const ACP_SERVERS_DIR_NAME: &str = "acp_servers";

/// Canonical crate-name prefix for HARNX_* env variables and the
/// `~/.config/harnx` subdirectory. Hardcoded as a literal because this
/// module lives in `harnx-core` but resolves paths for the `harnx` app.
const HARNX_NAME: &str = "harnx";

/// Translate a logical key into the matching `HARNX_<KEY>` env var name.
///
/// Example: `get_env_name("config_dir")` → `"HARNX_CONFIG_DIR"`.
pub fn get_env_name(key: &str) -> String {
    format!("{HARNX_NAME}_{key}").to_ascii_uppercase()
}

/// Normalize an identifier into an uppercase env-name fragment.
///
/// Replaces `-` with `_` and uppercases the result. Used to derive
/// `<AGENT>_DATA_DIR`-style env vars from agent names.
pub fn normalize_env_name(value: &str) -> String {
    value.replace('-', "_").to_ascii_uppercase()
}

/// Root config directory. Resolution order:
/// 1. `HARNX_CONFIG_DIR` env var (literal path).
/// 2. `XDG_CONFIG_HOME/harnx` (XDG override).
/// 3. OS default (`dirs::config_dir()/harnx` — `~/.config/harnx` on Linux).
///
/// Panics if the OS has no default user config dir AND no overrides.
pub fn config_dir() -> PathBuf {
    if let Ok(v) = env::var(get_env_name("config_dir")) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Ok(v) = env::var("XDG_CONFIG_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v).join(HARNX_NAME);
        }
    }
    let dir = dirs::config_dir().expect("No user's config directory");
    dir.join(HARNX_NAME)
}

/// Join `name` under `config_dir()`. Convenience for leaf files/dirs.
pub fn local_path(name: &str) -> PathBuf {
    config_dir().join(name)
}

/// Root data directory. Resolution order:
/// 1. `HARNX_DATA_DIR` env var (literal path).
/// 2. `XDG_DATA_HOME/harnx` (XDG override).
/// 3. OS default (`dirs::data_dir()/harnx`).
///
/// Panics if the OS has no default user data dir AND no overrides.
pub fn data_dir() -> PathBuf {
    if let Ok(v) = env::var(get_env_name("data_dir")) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Ok(v) = env::var("XDG_DATA_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v).join(HARNX_NAME);
        }
    }
    let dir = dirs::data_dir().expect("No user data dir");
    dir.join(HARNX_NAME)
}

/// Join `name` under `data_dir()`. Convenience for leaf files/dirs.
pub fn data_path(name: &str) -> PathBuf {
    data_dir().join(name)
}

/// Root state directory. Resolution order:
/// 1. `HARNX_STATE_DIR` env var (literal path).
/// 2. `XDG_STATE_HOME/harnx` (XDG override).
/// 3. OS default (`dirs::state_dir().unwrap_or_else(|| dirs::data_dir().expect("No user data dir"))/harnx`).
///
/// Panics if the OS has no default user state dir, no default user data dir, AND no overrides.
pub fn state_dir() -> PathBuf {
    if let Ok(v) = env::var(get_env_name("state_dir")) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Ok(v) = env::var("XDG_STATE_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v).join(HARNX_NAME);
        }
    }
    let dir = dirs::state_dir().unwrap_or_else(|| dirs::data_dir().expect("No user data dir"));
    dir.join(HARNX_NAME)
}

/// Join `name` under `state_dir()`. Convenience for leaf files/dirs.
pub fn state_path(name: &str) -> PathBuf {
    state_dir().join(name)
}

/// Path to the main config file. Overridable via `HARNX_CONFIG_FILE`.
pub fn config_file() -> PathBuf {
    match env::var(get_env_name("config_file")) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => local_path(CONFIG_FILE_NAME),
    }
}

/// Directory holding macro YAML files. Overridable via `HARNX_MACROS_DIR`.
pub fn macros_dir() -> PathBuf {
    match env::var(get_env_name("macros_dir")) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => local_path(MACROS_DIR_NAME),
    }
}

/// Parent directory of the config file. If `HARNX_CONFIG_FILE` points
/// elsewhere, subdirectories (clients/, mcp_servers/, acp_servers/) are
/// resolved relative to that file's parent rather than `config_dir()`.
pub fn config_dir_path() -> PathBuf {
    config_file()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(config_dir)
}

/// Subdirectory holding per-client YAML files (one file per client).
pub fn clients_dir() -> PathBuf {
    config_dir_path().join(CLIENTS_DIR_NAME)
}

/// Subdirectory holding per-MCP-server YAML files.
pub fn mcp_servers_dir() -> PathBuf {
    config_dir_path().join(MCP_SERVERS_DIR_NAME)
}

/// Subdirectory holding per-ACP-server YAML files.
pub fn acp_servers_dir() -> PathBuf {
    config_dir_path().join(ACP_SERVERS_DIR_NAME)
}

/// Path to a specific macro file by name (extension `.yaml`).
pub fn macro_file(name: &str) -> PathBuf {
    macros_dir().join(format!("{name}.yaml"))
}

/// Path to the `.env` file loaded at startup. Overridable via `HARNX_ENV_FILE`.
pub fn env_file() -> PathBuf {
    match env::var(get_env_name("env_file")) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => local_path(ENV_FILE_NAME),
    }
}

/// Top-level RAG manifests dir (not per-agent). Overridable via `HARNX_RAGS_DIR`.
pub fn rags_dir() -> PathBuf {
    match env::var(get_env_name("rags_dir")) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => data_path(RAGS_DIR_NAME),
    }
}

/// Root dir for per-agent data subdirectories.
pub fn agents_data_dir() -> PathBuf {
    data_path(AGENTS_DIR_NAME)
}

/// Per-agent data dir. Each agent may override its own location via
/// `<AGENT_NAME>_DATA_DIR` (dashes become underscores, uppercased).
pub fn agent_data_dir(name: &str) -> PathBuf {
    match env::var(format!("{}_DATA_DIR", normalize_env_name(name))) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => agents_data_dir().join(name),
    }
}

/// Per-agent RAG manifest file: `<agent_data_dir>/<rag_name>.yaml`.
pub fn agent_rag_file(agent_name: &str, rag_name: &str) -> PathBuf {
    agent_data_dir(agent_name).join(format!("{rag_name}.yaml"))
}

/// Root dir for per-agent instruction files (.md) — lives in config dir.
///
/// Resolves relative to `config_dir_path()` (i.e. the parent of the active
/// config file) so that `HARNX_CONFIG_FILE` redirections are honoured, exactly
/// as `clients_dir()`, `mcp_servers_dir()`, and `acp_servers_dir()` do.
pub fn agents_config_dir() -> PathBuf {
    config_dir_path().join(AGENTS_DIR_NAME)
}

/// Per-agent instruction file: `<config_dir>/agents/<name>.md`.
pub fn agent_file(name: &str) -> PathBuf {
    agents_config_dir().join(format!("{name}.md"))
}

/// Optional models-override YAML file; if present, overrides models.yaml entries.
pub fn models_override_file() -> PathBuf {
    local_path("models-override.yaml")
}

/// Persisted "last messages" file. Agent-scoped when `agent_name` is `Some`,
/// else the top-level `messages.md` (overridable via `HARNX_MESSAGES_FILE`).
pub fn messages_file(agent_name: Option<&str>) -> PathBuf {
    match agent_name {
        None => match env::var(get_env_name("messages_file")) {
            Ok(value) if !value.is_empty() => PathBuf::from(value),
            _ => state_path(MESSAGES_FILE_NAME),
        },
        Some(agent) => agent_data_dir(agent).join(MESSAGES_FILE_NAME),
    }
}

/// Sessions directory. Agent-scoped when `agent_name` is `Some`, else the
/// top-level sessions dir (overridable via `HARNX_SESSIONS_DIR`).
pub fn sessions_dir(agent_name: Option<&str>) -> PathBuf {
    match agent_name {
        None => match env::var(get_env_name("sessions_dir")) {
            Ok(value) if !value.is_empty() => PathBuf::from(value),
            _ => state_path(SESSIONS_DIR_NAME),
        },
        Some(agent) => agent_data_dir(agent).join(SESSIONS_DIR_NAME),
    }
}

/// Resolve a session file by name. If `name` contains a `/`, splits on the
/// first one: the prefix becomes a subdir under `sessions_dir(agent_name)` and
/// the suffix becomes the `.yaml` filename. Otherwise the file sits directly
/// in the sessions dir.
pub fn session_file(agent_name: Option<&str>, name: &str) -> PathBuf {
    let dir = sessions_dir(agent_name);
    match name.split_once('/') {
        Some((sub, leaf)) => dir.join(sub).join(format!("{leaf}.yaml")),
        None => dir.join(format!("{name}.yaml")),
    }
}

/// Resolve a RAG manifest file by name. Agent-scoped when `agent_name` is
/// `Some` (routes through `agent_rag_file`, which honors `<AGENT>_DATA_DIR`),
/// else under the top-level `rags_dir()` (overridable via `HARNX_RAGS_DIR`).
pub fn rag_file(agent_name: Option<&str>, name: &str) -> PathBuf {
    match agent_name {
        Some(agent) => agent_rag_file(agent, name),
        None => rags_dir().join(format!("{name}.yaml")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutex that serialises all tests which mutate process-global env vars.
    ///
    /// `cargo nextest` runs tests in separate processes by default, but
    /// `cargo test` runs them in parallel threads within the same process.
    /// Any test that calls `env::set_var` / `env::remove_var` must hold this
    /// lock for the duration of the set → call → restore sequence.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Temporarily set an env var, invoke `f`, then restore the previous value
    /// (or remove the var if it was absent before).  Holds `ENV_MUTEX` for the
    /// entire duration so env-sensitive tests cannot race.
    fn with_env<F, R>(key: &str, value: &str, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prior = env::var_os(key);
        env::set_var(key, value);
        let result = f();
        match prior {
            Some(v) => env::set_var(key, v),
            None => env::remove_var(key),
        }
        result
    }

    /// Apply multiple env-var overrides (`Some(value)` = set, `None` = remove),
    /// invoke `f`, then restore every var to its prior state.  Holds `ENV_MUTEX`
    /// for the whole sequence so the entire multi-var mutation is atomic w.r.t.
    /// other env-sensitive tests.
    fn with_envs<F, R>(pairs: &[(&str, Option<&str>)], f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        // Save prior values and apply requested overrides.
        let priors: Vec<_> = pairs
            .iter()
            .map(|(key, new_val)| {
                let prior = env::var_os(key);
                match new_val {
                    Some(v) => env::set_var(key, v),
                    None => env::remove_var(key),
                }
                (key, prior)
            })
            .collect();
        let result = f();
        // Restore in reverse order for cleanliness.
        for (key, prior) in priors.into_iter().rev() {
            match prior {
                Some(v) => env::set_var(key, v),
                None => env::remove_var(key),
            }
        }
        result
    }

    #[test]
    fn get_env_name_produces_harnx_prefix() {
        assert_eq!(get_env_name("config_dir"), "HARNX_CONFIG_DIR");
        assert_eq!(get_env_name("log_filter"), "HARNX_LOG_FILTER");
    }

    #[test]
    fn get_env_name_uppercases_mixed_case_input() {
        assert_eq!(get_env_name("Config_File"), "HARNX_CONFIG_FILE");
    }

    #[test]
    fn normalize_env_name_uppercases_and_dashes() {
        assert_eq!(normalize_env_name("my-agent"), "MY_AGENT");
        assert_eq!(normalize_env_name("Demo_Agent"), "DEMO_AGENT");
    }

    #[test]
    fn normalize_env_name_leaves_already_normalized_identifier() {
        assert_eq!(normalize_env_name("AGENT_X"), "AGENT_X");
    }

    #[test]
    fn agent_data_dir_falls_back_to_agents_subdir() {
        // With no <AGENT>_DATA_DIR env override, the per-agent dir is
        // agents_data_dir().join(name).
        // We can't easily clear arbitrary env vars safely in parallel tests,
        // so pick a name that's very unlikely to have an override set.
        let name = "ztest_agent_for_fallback_check_4f9c";
        let got = agent_data_dir(name);
        assert!(got.ends_with(format!("{AGENTS_DIR_NAME}/{name}")));
    }

    #[test]
    fn macro_file_adds_yaml_extension() {
        let got = macro_file("some_macro");
        assert_eq!(
            got.file_name().and_then(|s| s.to_str()),
            Some("some_macro.yaml")
        );
    }

    #[test]
    fn agent_file_adds_md_extension() {
        let got = agent_file("demo");
        assert_eq!(got.file_name().and_then(|s| s.to_str()), Some("demo.md"));
    }

    #[test]
    fn models_override_file_is_under_config_dir() {
        // Assert both the file name AND the parent — tighter than
        // `ends_with("models-override.yaml")` alone would be, which
        // would pass for a bare relative `"models-override.yaml"`.
        let got = models_override_file();
        assert_eq!(
            got.file_name().and_then(|s| s.to_str()),
            Some("models-override.yaml")
        );
        assert_eq!(got.parent().unwrap(), config_dir());
    }

    #[test]
    fn session_file_without_agent_places_yaml_under_sessions_dir() {
        let got = session_file(None, "my_session");
        let tail: Vec<_> = got
            .components()
            .rev()
            .take(2)
            .map(|c| c.as_os_str().to_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(tail, vec!["my_session.yaml", SESSIONS_DIR_NAME]);
    }

    #[test]
    fn session_file_with_slash_splits_into_subdir() {
        let got = session_file(None, "group/leaf");
        let tail: Vec<_> = got
            .components()
            .rev()
            .take(3)
            .map(|c| c.as_os_str().to_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(tail, vec!["leaf.yaml", "group", SESSIONS_DIR_NAME]);
    }

    #[test]
    fn rag_file_without_agent_uses_top_level_rags_dir() {
        let got = rag_file(None, "code");
        let tail: Vec<_> = got
            .components()
            .rev()
            .take(2)
            .map(|c| c.as_os_str().to_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(tail, vec!["code.yaml", RAGS_DIR_NAME]);
    }

    #[test]
    fn rag_file_with_agent_routes_through_agent_rag_file() {
        // Use a random-looking name to avoid <AGENT>_DATA_DIR env collisions
        // (see agent_data_dir_falls_back_to_agents_subdir for the same trick).
        let agent = "ztest_agent_for_rag_2e1d";
        let got = rag_file(Some(agent), "index");
        let tail: Vec<_> = got
            .components()
            .rev()
            .take(3)
            .map(|c| c.as_os_str().to_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            tail,
            vec![
                "index.yaml".to_string(),
                agent.to_string(),
                AGENTS_DIR_NAME.to_string()
            ]
        );
    }

    #[test]
    fn messages_file_with_agent_uses_agent_data_dir() {
        let agent = "ztest_agent_for_msgs_b83c";
        let got = messages_file(Some(agent));
        let tail: Vec<_> = got
            .components()
            .rev()
            .take(3)
            .map(|c| c.as_os_str().to_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            tail,
            vec![
                MESSAGES_FILE_NAME.to_string(),
                agent.to_string(),
                AGENTS_DIR_NAME.to_string()
            ]
        );
    }

    #[test]
    fn session_file_with_agent_nests_under_agent_data_dir() {
        let agent = "ztest_agent_for_session_9a4f";
        let got = session_file(Some(agent), "my_session");
        let tail: Vec<_> = got
            .components()
            .rev()
            .take(3)
            .map(|c| c.as_os_str().to_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(
            tail,
            vec![
                "my_session.yaml".to_string(),
                SESSIONS_DIR_NAME.to_string(),
                agent.to_string(),
            ]
        );
    }

    #[test]
    fn data_dir_uses_harnx_data_dir_env_override() {
        let test_path = "/tmp/harnx_data_test_8a3f";
        let got = with_env("HARNX_DATA_DIR", test_path, data_dir);
        assert_eq!(got, PathBuf::from(test_path));
    }

    #[test]
    fn state_dir_uses_harnx_state_dir_env_override() {
        let test_path = "/tmp/harnx_state_test_7b2e";
        let got = with_env("HARNX_STATE_DIR", test_path, state_dir);
        assert_eq!(got, PathBuf::from(test_path));
    }

    #[test]
    fn rags_dir_is_under_data_dir() {
        let test_data_dir = "/tmp/harnx_data_rags_3c9b";
        // Hold both HARNX_DATA_DIR and HARNX_RAGS_DIR under one lock acquire so
        // another test cannot slip in between and see a half-set state.
        let got = with_envs(
            &[
                ("HARNX_DATA_DIR", Some(test_data_dir)),
                ("HARNX_RAGS_DIR", None), // ensure fallback path fires
            ],
            rags_dir,
        );
        assert!(
            got.starts_with(test_data_dir),
            "rags_dir should be under data_dir"
        );
        assert!(
            got.ends_with(RAGS_DIR_NAME),
            "rags_dir should end with 'rags'"
        );
    }

    #[test]
    fn sessions_dir_none_is_under_state_dir() {
        let test_state_dir = "/tmp/harnx_state_sessions_5d1a";
        let got = with_envs(
            &[
                ("HARNX_STATE_DIR", Some(test_state_dir)),
                ("HARNX_SESSIONS_DIR", None), // ensure fallback path fires
            ],
            || sessions_dir(None),
        );
        assert!(
            got.starts_with(test_state_dir),
            "sessions_dir(None) should be under state_dir"
        );
        assert!(
            got.ends_with(SESSIONS_DIR_NAME),
            "sessions_dir(None) should end with 'sessions'"
        );
    }

    #[test]
    fn sessions_dir_with_agent_is_under_data_dir() {
        let test_data_dir = "/tmp/harnx_data_agent_sessions_9e2f";
        // Use an unlikely agent name to avoid <AGENT>_DATA_DIR env collisions.
        let agent = "ztest_agent_for_sdir_5e1c";
        let got = with_env("HARNX_DATA_DIR", test_data_dir, || {
            sessions_dir(Some(agent))
        });
        assert!(
            got.starts_with(test_data_dir),
            "sessions_dir(Some(agent)) should be under data_dir"
        );
        assert!(
            got.ends_with(SESSIONS_DIR_NAME),
            "sessions_dir(Some(agent)) should end with 'sessions'"
        );
    }

    #[test]
    fn messages_file_none_is_under_state_dir() {
        let test_state_dir = "/tmp/harnx_state_messages_4f8b";
        let got = with_envs(
            &[
                ("HARNX_STATE_DIR", Some(test_state_dir)),
                ("HARNX_MESSAGES_FILE", None), // ensure fallback path fires
            ],
            || messages_file(None),
        );
        assert!(
            got.starts_with(test_state_dir),
            "messages_file(None) should be under state_dir"
        );
        assert!(
            got.ends_with(MESSAGES_FILE_NAME),
            "messages_file(None) should end with 'messages.md'"
        );
    }

    #[test]
    fn data_dir_uses_xdg_data_home_env() {
        let test_path = "/tmp/xdg_data_test_9b2c";
        // Clear HARNX_DATA_DIR so the XDG_DATA_HOME branch fires, and restore
        // both vars afterwards.
        let got = with_envs(
            &[("HARNX_DATA_DIR", None), ("XDG_DATA_HOME", Some(test_path))],
            data_dir,
        );
        assert_eq!(got, PathBuf::from(test_path).join(HARNX_NAME));
    }

    #[test]
    fn state_dir_uses_xdg_state_home_env() {
        let test_path = "/tmp/xdg_state_test_3d7e";
        // Clear HARNX_STATE_DIR so the XDG_STATE_HOME branch fires, and restore
        // both vars afterwards.
        let got = with_envs(
            &[
                ("HARNX_STATE_DIR", None),
                ("XDG_STATE_HOME", Some(test_path)),
            ],
            state_dir,
        );
        assert_eq!(got, PathBuf::from(test_path).join(HARNX_NAME));
    }
}
