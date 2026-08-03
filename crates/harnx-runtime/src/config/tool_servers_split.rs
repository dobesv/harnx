//! Tool server config for NATS-based tool servers.
//!
//! These configs specify which NATS tool servers to spawn as separate processes.
//! Similar in shape to `McpServerConfig` but without MCP-specific fields like
//! `rename_tools` and `tool_templates`.

use super::*;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Default value for boolean fields that should default to `true`.
fn default_true() -> bool {
    true
}

/// Configuration for a NATS-based tool server spawned as a subprocess.
///
/// Each tool server is a separate process that connects to the shared NATS
/// broker and advertises its tools via the KV registry. The runtime spawns
/// these servers on demand when a session needs their tools.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ToolServerConfig {
    /// Server name, set from the config file stem by the loader.
    #[serde(default)]
    pub name: String,

    /// Command to execute (binary name or path).
    pub command: String,

    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables to set for the server process.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Whether this server should be spawned. Defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Human-readable description of the server.
    #[serde(default)]
    pub description: Option<String>,

    /// Package this server belongs to, set at runtime by the package loader.
    ///
    /// `None` for user-provided configs; `Some(package_name)` for servers
    /// installed from packages.
    #[serde(skip)]
    pub package: Option<String>,

    /// Per-tool-server hooks configuration.
    ///
    /// Hooks defined here apply only to tools provided by this tool server.
    /// Merged with global and agent hooks at runtime.
    #[serde(default)]
    pub hooks: Option<HooksConfig>,
}

impl Config {
    /// Load tool server configs from a directory of YAML files.
    ///
    /// Each `.yaml` file in `dir` defines one tool server. The file stem
    /// (basename without extension) becomes the server's `name`.
    pub fn load_tool_servers_from_dir(dir: &Path) -> Result<Vec<ToolServerConfig>> {
        if !dir.exists() {
            return Ok(vec![]);
        }

        let mut servers = Vec::new();
        for path in Self::sorted_yaml_files(dir)? {
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(stem) if !stem.is_empty() => stem.to_string(),
                _ => continue,
            };

            let content = read_to_string(&path).with_context(|| {
                format!("Failed to read tool server config '{}'", path.display())
            })?;
            let mut server: ToolServerConfig =
                serde_yaml::from_str(&content).with_context(|| {
                    format!("Failed to parse tool server config '{}'", path.display())
                })?;
            server.name = stem;
            servers.push(server);
        }

        Ok(servers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::{env_lock, EnvGuard};
    use std::fs;

    #[test]
    fn deserializes_minimal_config_with_defaults() {
        let yaml = r#"
command: harnx-time-server
"#;
        let config: ToolServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "");
        assert_eq!(config.command, "harnx-time-server");
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
        assert!(config.enabled);
        assert!(config.description.is_none());
        assert!(config.package.is_none());
        assert!(config.hooks.is_none());
    }

    #[test]
    fn deserializes_config_with_hooks() {
        let yaml = r#"
command: harnx-time-server
hooks:
  max_resume: 3
  entries:
    - command: "harnx-claude-compatible-hook-server --event PreToolUse --matcher time --timeout 30 --command /path/to/hook.sh"
      status_message: "Checking time tool"
"#;
        let config: ToolServerConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.command, "harnx-time-server");
        let hooks = config.hooks.as_ref().expect("hooks should be parsed");
        assert_eq!(hooks.max_resume, Some(3));
        assert_eq!(hooks.entries.len(), 1);
        let entry = &hooks.entries[0];
        assert_eq!(
            entry.command,
            "harnx-claude-compatible-hook-server --event PreToolUse --matcher time --timeout 30 --command /path/to/hook.sh"
        );
        assert_eq!(entry.status_message.as_deref(), Some("Checking time tool"));
        assert!(entry.async_hook.is_none());
    }

    #[test]
    fn shipped_bash_config_launches_proxy_auth_as_tool_hook() {
        let coding = include_str!("../../../../packages/coding/tool_servers/bash.yaml");
        let pantheon = include_str!("../../../../packages/pantheon/tool_servers/bash.yaml");
        assert_eq!(coding, pantheon);

        let config: ToolServerConfig = serde_yaml::from_str(coding).expect("parse bash config");
        let hooks = config.hooks.expect("bash hooks");
        assert_eq!(hooks.entries.len(), 1);
        let hook = &hooks.entries[0];
        assert!(hook.command.starts_with("harnx-proxy-auth --hook "));
        assert!(hook
            .command
            .contains("$HARNX_PACKAGE_DIR/hooks/jira-auth-hook.py"));
        assert!(hook.command.contains("$temp_file_root"));
    }

    #[test]
    fn loads_from_user_config_dir() {
        let _guard = env_lock();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_dir = temp_dir.path();

        // Write a tool_servers/time.yaml
        let tool_servers_dir = config_dir.join(paths::TOOL_SERVERS_DIR_NAME);
        fs::create_dir_all(&tool_servers_dir).expect("create tool_servers dir");
        fs::write(
            tool_servers_dir.join("time.yaml"),
            "command: harnx-time-server\n",
        )
        .expect("write time.yaml");

        // Set HARNX_CONFIG_DIR to temp dir
        let _env_guard = EnvGuard::new("HARNX_CONFIG_DIR", config_dir);

        // Load config via load_from_file (which calls loader_split logic)
        let config_file = config_dir.join(paths::CONFIG_FILE_NAME);
        fs::write(&config_file, "model: openai\n").expect("write config.yaml");

        let config = Config::load_from_file(&config_file).expect("load config");

        // Verify tool_servers loaded with name = "time"
        assert_eq!(config.tool_servers.len(), 1);
        assert_eq!(config.tool_servers[0].name, "time");
        assert_eq!(config.tool_servers[0].command, "harnx-time-server");
        assert!(config.tool_servers[0].package.is_none());
    }

    #[test]
    fn loads_tool_servers_from_package_dir() {
        let _guard = env_lock();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_dir = temp_dir.path();
        let package_tool_servers = config_dir
            .join(paths::PACKAGES_DIR_NAME)
            .join("coding")
            .join(paths::TOOL_SERVERS_DIR_NAME);
        fs::create_dir_all(&package_tool_servers).expect("create package tool_servers dir");
        fs::write(
            package_tool_servers.join("time.yaml"),
            "command: harnx-time-server\n",
        )
        .expect("write package time.yaml");
        let _env_guard = EnvGuard::new("HARNX_CONFIG_DIR", config_dir);
        let config_file = config_dir.join(paths::CONFIG_FILE_NAME);
        fs::write(&config_file, "model: openai\n").expect("write config.yaml");

        let config = Config::load_from_file(&config_file).expect("load config");

        assert_eq!(config.tool_servers.len(), 1);
        assert_eq!(config.tool_servers[0].name, "time");
        assert_eq!(config.tool_servers[0].package.as_deref(), Some("coding"));
    }

    #[test]
    fn loads_package_tool_servers_in_name_order() {
        let _guard = env_lock();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_dir = temp_dir.path();
        for (package, command) in [("zzz", "from-zzz"), ("aaa", "from-aaa")] {
            let server_dir = config_dir
                .join(paths::PACKAGES_DIR_NAME)
                .join(package)
                .join(paths::TOOL_SERVERS_DIR_NAME);
            fs::create_dir_all(&server_dir).expect("create package tool_servers dir");
            fs::write(server_dir.join("dup.yaml"), format!("command: {command}\n"))
                .expect("write duplicate tool server");
        }
        let _env_guard = EnvGuard::new("HARNX_CONFIG_DIR", config_dir);
        let config_file = config_dir.join(paths::CONFIG_FILE_NAME);
        fs::write(&config_file, "model: openai\n").expect("write config.yaml");

        let config = Config::load_from_file(&config_file).expect("load config");

        assert_eq!(config.tool_servers.len(), 2);
        assert_eq!(config.tool_servers[0].package.as_deref(), Some("aaa"));
        assert_eq!(config.tool_servers[0].command, "from-aaa");
        assert_eq!(config.tool_servers[1].package.as_deref(), Some("zzz"));
    }
}
