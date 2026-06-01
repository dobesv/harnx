//! Extracted from config/mod.rs for code health.
use crate::client::ClientConfig;
use anyhow::{Context, Result};
use harnx_acp::AcpServerConfig;
use harnx_mcp::McpServerConfig;

/// Extract a valid package name from a directory path entry.
/// Returns None for non-directories and hidden directories (starting with '.').
pub(super) fn package_dir_name(path: &std::path::Path) -> Option<String> {
    if !path.is_dir() {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    if name.starts_with('.') {
        return None;
    }
    Some(name.to_string())
}

pub(super) fn load_package_mcp_patch(pkg_name: &str) -> Option<harnx_core::package::PackagePatch> {
    let patch_path = harnx_core::config_paths::package_patch_file(pkg_name);
    if !patch_path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&patch_path) {
        Ok(content) => content,
        Err(error) => {
            log::warn!(
                "Failed to read package patch file {}: {}",
                patch_path.display(),
                error
            );
            return None;
        }
    };
    match serde_yaml::from_str(&content) {
        Ok(patch) => Some(patch),
        Err(error) => {
            log::warn!(
                "Failed to parse package patch file {}: {}",
                patch_path.display(),
                error
            );
            None
        }
    }
}

pub(super) fn apply_mcp_server_patch(
    server: &mut McpServerConfig,
    patches: &[String],
) -> Result<()> {
    if patches.is_empty() {
        return Ok(());
    }
    let saved_package = server.package.clone();
    let input = serde_json::to_value(&*server)
        .with_context(|| "Failed to serialize McpServerConfig for jaq patch")?;
    let output = harnx_core::jaq::eval_filters_strict(patches, input)
        .with_context(|| "jq patch expression failed for MCP server config")?;
    *server = serde_json::from_value(output)
        .with_context(|| "Failed to deserialize McpServerConfig after jaq patch")?;
    server.package = saved_package;
    Ok(())
}

pub(super) fn apply_client_patch(client: &mut ClientConfig, patches: &[String]) -> Result<()> {
    if patches.is_empty() {
        return Ok(());
    }
    let input = serde_json::to_value(&*client)
        .with_context(|| "Failed to serialize ClientConfig for jaq patch")?;
    let output = harnx_core::jaq::eval_filters_strict(patches, input)
        .with_context(|| "jq patch expression failed for client config")?;
    *client = serde_json::from_value(output)
        .with_context(|| "Failed to deserialize ClientConfig after jaq patch")?;
    Ok(())
}

///
/// - Top-level servers (`package == None`): unchanged name.
/// - Same-package servers: bare name (the yaml stem, e.g. `fs`).
/// - Other-package servers: `<sanitized_pkg>__<bare_name>` (e.g. `otherpkg__db`).
pub fn mcp_server_display_name(server: &McpServerConfig, agent_package: Option<&str>) -> String {
    use harnx_core::package_namespace::sanitize_for_tool_name;
    match (&server.package, agent_package) {
        (None, _) => server.name.clone(),
        (Some(pkg), Some(active_package)) if pkg == active_package => server.name.clone(),
        (Some(pkg), _) => format!("{}__{}", sanitize_for_tool_name(pkg), server.name),
    }
}

/// Compute display name for ACP server given active agent package.
pub(super) fn acp_server_display_name(
    server: &AcpServerConfig,
    agent_package: Option<&str>,
) -> String {
    use harnx_core::package_namespace::sanitize_for_tool_name;
    match (&server.package, agent_package) {
        (None, _) => server.name.clone(),
        (Some(pkg), Some(active_package)) if pkg == active_package => server.name.clone(),
        (Some(pkg), _) => format!("{}__{}", sanitize_for_tool_name(pkg), server.name),
    }
}
