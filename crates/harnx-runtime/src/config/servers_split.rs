//! Tool server package loading extracted from config/mod.rs for code health.
use super::*;
use crate::config::patches_split::load_package_tool_server_patch;

impl Config {
    /// Load tool servers from a single package directory.
    ///
    /// Servers are stored with their bare names (the yaml stem) and tagged with
    /// `package = Some(pkg_name)`. The actual display name used with the LLM
    /// — bare or prefixed — is decided at runtime based on which agent is active.
    pub(super) fn load_package_servers(config: &mut Config, pkg_path: &Path, pkg_name: &str) {
        let pkg_tool_dir = pkg_path.join(paths::TOOL_SERVERS_DIR_NAME);
        if !pkg_tool_dir.is_dir() {
            return;
        }

        let patch = load_package_tool_server_patch(pkg_name);
        for mut server in Self::load_tool_servers_from_dir(&pkg_tool_dir).unwrap_or_default() {
            server.package = Some(pkg_name.to_string());
            if let Some(patch) = &patch {
                if let Err(e) = apply_tool_server_patch(&mut server, &patch.tool_servers) {
                    log::error!(
                        "Package patch failed for tool server '{}': {e:#}",
                        server.name
                    );
                    continue;
                }
            }
            config.tool_servers.push(server);
        }
    }
}

/// Apply a list of jq-style patch expressions to a tool server config.
pub(super) fn apply_tool_server_patch(
    server: &mut ToolServerConfig,
    patches: &[String],
) -> Result<()> {
    if patches.is_empty() {
        return Ok(());
    }

    let input = serde_json::to_value(&*server).context("Failed to serialize tool server config")?;
    let output =
        harnx_core::jaq::eval_filters_strict(patches, input).context("jq patch evaluation")?;
    *server =
        serde_json::from_value(output).context("Failed to deserialize patched tool server")?;

    // Restore the #[serde(skip)] package field that round-trips through serde_json lose.
    // It's set by the caller after this function returns.
    Ok(())
}
