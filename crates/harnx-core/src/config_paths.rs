//! Config directory path resolution for harnx.
//!
//! Owns the "where do harnx config files live" logic: the XDG / HARNX_* env
//! resolution and the file/dir name constants. See also `harnx-core::path`
//! for generic path algorithms (safe_join_path, expand_glob_paths).

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

use std::env;
use std::path::PathBuf;

/// Root config directory. Resolution order:
/// 1. `HARNX_CONFIG_DIR` env var (literal path).
/// 2. `XDG_CONFIG_HOME/harnx` (XDG override).
/// 3. OS default (`dirs::config_dir()/harnx` — `~/.config/harnx` on Linux).
///
/// Panics if the OS has no default user config dir AND no overrides.
pub fn config_dir() -> PathBuf {
    if let Ok(v) = env::var(get_env_name("config_dir")) {
        PathBuf::from(v)
    } else if let Ok(v) = env::var("XDG_CONFIG_HOME") {
        PathBuf::from(v).join(HARNX_NAME)
    } else {
        let dir = dirs::config_dir().expect("No user's config directory");
        dir.join(HARNX_NAME)
    }
}

/// Join `name` under `config_dir()`. Convenience for leaf files/dirs.
pub fn local_path(name: &str) -> PathBuf {
    config_dir().join(name)
}

/// Path to the main config file. Overridable via `HARNX_CONFIG_FILE`.
pub fn config_file() -> PathBuf {
    match env::var(get_env_name("config_file")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_path(CONFIG_FILE_NAME),
    }
}

/// Directory holding macro YAML files. Overridable via `HARNX_MACROS_DIR`.
pub fn macros_dir() -> PathBuf {
    match env::var(get_env_name("macros_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_path(MACROS_DIR_NAME),
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
        Ok(value) => PathBuf::from(value),
        Err(_) => local_path(ENV_FILE_NAME),
    }
}

/// Top-level RAG manifests dir (not per-agent). Overridable via `HARNX_RAGS_DIR`.
pub fn rags_dir() -> PathBuf {
    match env::var(get_env_name("rags_dir")) {
        Ok(value) => PathBuf::from(value),
        Err(_) => local_path(RAGS_DIR_NAME),
    }
}

/// Root dir for per-agent data subdirectories.
pub fn agents_data_dir() -> PathBuf {
    local_path(AGENTS_DIR_NAME)
}

/// Per-agent data dir. Each agent may override its own location via
/// `<AGENT_NAME>_DATA_DIR` (dashes become underscores, uppercased).
pub fn agent_data_dir(name: &str) -> PathBuf {
    match env::var(format!("{}_DATA_DIR", normalize_env_name(name))) {
        Ok(value) => PathBuf::from(value),
        Err(_) => agents_data_dir().join(name),
    }
}

/// Per-agent RAG manifest file: `<agent_data_dir>/<rag_name>.yaml`.
pub fn agent_rag_file(agent_name: &str, rag_name: &str) -> PathBuf {
    agent_data_dir(agent_name).join(format!("{rag_name}.yaml"))
}

/// Per-agent instruction file: `<agents_data_dir>/<name>.md`.
pub fn agent_file(name: &str) -> PathBuf {
    agents_data_dir().join(format!("{name}.md"))
}

/// Optional models-override YAML file; if present, overrides models.yaml entries.
pub fn models_override_file() -> PathBuf {
    local_path("models-override.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let got = models_override_file();
        assert!(got.ends_with("models-override.yaml"));
    }
}
