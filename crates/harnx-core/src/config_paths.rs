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
}
