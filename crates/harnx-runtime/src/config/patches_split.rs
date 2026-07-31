//! Extracted from config/mod.rs for code health.
use crate::client::ClientConfig;
use anyhow::{Context, Result};
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

    let saved_name = match client {
        ClientConfig::Unknown => None,
        _ => Some(client.effective_name().to_string()),
    };
    let saved_package = match client {
        ClientConfig::OpenAIConfig(c) => c.package.clone(),
        ClientConfig::OpenAICompatibleConfig(c) => c.package.clone(),
        ClientConfig::GeminiConfig(c) => c.package.clone(),
        ClientConfig::ClaudeConfig(c) => c.package.clone(),
        ClientConfig::CohereConfig(c) => c.package.clone(),
        ClientConfig::AzureOpenAIConfig(c) => c.package.clone(),
        ClientConfig::VertexAIConfig(c) => c.package.clone(),
        ClientConfig::BedrockConfig(c) => c.package.clone(),
        ClientConfig::LlamaServerConfig(c) => c.package.clone(),
        ClientConfig::Unknown => None,
    };

    let mut input = serde_json::to_value(&*client)
        .with_context(|| "Failed to serialize ClientConfig for jaq patch")?;
    if let (Some(name), serde_json::Value::Object(obj)) = (&saved_name, &mut input) {
        obj.insert("name".to_string(), serde_json::Value::String(name.clone()));
    }

    let output = harnx_core::jaq::eval_filters_strict(patches, input)
        .with_context(|| "jq patch expression failed for client config")?;

    let patched_name = output
        .as_object()
        .and_then(|obj| obj.get("name"))
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string);

    *client = serde_json::from_value(output)
        .with_context(|| "Failed to deserialize ClientConfig after jaq patch")?;

    if let Some(name) = patched_name.or(saved_name) {
        client.set_name(name);
    }
    client.set_package(saved_package);

    Ok(())
}
/// Compute display name for MCP server given active agent package.
///
/// - Top-level servers (`package == None`): unchanged name.
/// - Same-package servers: bare name (the yaml stem, e.g. `fs`).
/// - Other-package servers: `<sanitized_pkg>__<bare_name>` (e.g. `otherpkg__db`).
pub fn mcp_server_display_name(server: &McpServerConfig, agent_package: Option<&str>) -> String {
    server_display_name(&server.name, server.package.as_deref(), agent_package)
}

/// Compute a tool-facing server name from its name and package scope.
pub(crate) fn server_display_name(
    name: &str,
    package: Option<&str>,
    agent_package: Option<&str>,
) -> String {
    use harnx_core::package_namespace::sanitize_for_tool_name;
    match (package, agent_package) {
        (None, _) => name.to_string(),
        (Some(pkg), Some(active_package)) if pkg == active_package => name.to_string(),
        (Some(pkg), _) => format!("{}__{name}", sanitize_for_tool_name(pkg)),
    }
}
#[cfg(test)]
mod tests {
    use super::mcp_server_display_name;
    use harnx_mcp::McpServerConfig;
    use std::collections::HashMap;
    fn mcp_server(name: &str, package: Option<&str>) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: "mcp-server".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            roots: Vec::new(),
            enabled: true,
            description: None,
            rename_tools: HashMap::new(),
            tool_templates: HashMap::new(),
            hooks: None,
            package: package.map(str::to_string),
        }
    }
    #[test]
    fn mcp_same_package_uses_bare_name() {
        let server = mcp_server("fs", Some("pantheon"));
        let actual = mcp_server_display_name(&server, Some("pantheon"));

        assert_eq!(actual, "fs");
    }

    #[test]
    fn mcp_cross_package_uses_namespaced_display() {
        let server = mcp_server("db", Some("otherpkg"));
        let actual = mcp_server_display_name(&server, Some("pantheon"));

        assert_eq!(actual, "otherpkg__db");
    }

    #[test]
    fn mcp_top_level_from_package_stays_bare() {
        let server = mcp_server("bash", None);
        let actual = mcp_server_display_name(&server, Some("pantheon"));

        assert_eq!(actual, "bash");
    }

    #[test]
    fn mcp_same_package_with_no_active_package_uses_namespaced_display() {
        let server = mcp_server("fs", Some("mypkg"));
        let actual = mcp_server_display_name(&server, None);

        assert_eq!(actual, "mypkg__fs");
    }
}
