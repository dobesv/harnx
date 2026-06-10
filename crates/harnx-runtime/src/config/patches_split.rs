//! Extracted from config/mod.rs for code health.
use crate::client::ClientConfig;
use anyhow::{Context, Result};
use harnx_acp::AcpServerConfig;
use harnx_core::package_namespace::{handoff_display_name, pkg_from_qualified, qualify_agent_name};
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

/// Reconstruct the target's qualified name (`pkg/agent`) from a server config.
///
/// Auto-registered package agents already carry a qualified `server.name`, so it
/// is returned as-is. Manual package servers carry a bare stem and are qualified
/// against their package. Top-level servers (no package) stay bare.
fn server_target_qualified(name: &str, package: Option<&str>) -> String {
    if pkg_from_qualified(name).is_some() {
        name.to_string()
    } else if let Some(pkg) = package {
        qualify_agent_name(pkg, name)
    } else {
        name.to_string()
    }
}

/// Compute display name for MCP server given active agent package.
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
    handoff_display_name(
        &server_target_qualified(&server.name, server.package.as_deref()),
        agent_package,
    )
}

#[cfg(test)]
mod tests {
    use super::{acp_server_display_name, mcp_server_display_name, server_target_qualified};
    use harnx_acp::AcpServerConfig;
    use harnx_core::package_namespace::handoff_display_name;
    use harnx_mcp::McpServerConfig;
    use std::collections::HashMap;

    fn acp_server(name: &str, package: Option<&str>) -> AcpServerConfig {
        AcpServerConfig {
            name: name.to_string(),
            command: "acp-server".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            enabled: true,
            description: None,
            idle_timeout_secs: 300,
            operation_timeout_secs: 3600,
            package: package.map(str::to_string),
        }
    }

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

    fn assert_display_name_matches_handoff(
        target_qualified: &str,
        active_pkg: Option<&str>,
        actual: &str,
    ) {
        assert_eq!(actual, handoff_display_name(target_qualified, active_pkg));
        assert!(
            !actual.contains('/'),
            "display name leaked slash for {target_qualified}: {actual}"
        );
    }

    #[test]
    fn acp_auto_registered_same_package_uses_bare_stem() {
        let server = acp_server("pantheon/atlas", Some("pantheon"));
        let actual = acp_server_display_name(&server, Some("pantheon"));

        assert_eq!(actual, "atlas");
        assert_display_name_matches_handoff(
            &server_target_qualified(&server.name, server.package.as_deref()),
            Some("pantheon"),
            &actual,
        );
    }

    #[test]
    fn acp_cross_package_uses_namespaced_display() {
        let server = acp_server("other/helper", Some("other"));
        let actual = acp_server_display_name(&server, Some("pantheon"));

        assert_eq!(actual, "other__helper");
        assert_display_name_matches_handoff(
            &server_target_qualified(&server.name, server.package.as_deref()),
            Some("pantheon"),
            &actual,
        );
    }

    #[test]
    fn acp_top_level_from_package_gets_explicit_prefix() {
        let server = acp_server("global", None);
        let actual = acp_server_display_name(&server, Some("pantheon"));

        assert_eq!(actual, "__global");
        assert_display_name_matches_handoff(
            &server_target_qualified(&server.name, server.package.as_deref()),
            Some("pantheon"),
            &actual,
        );
    }

    #[test]
    fn acp_manual_same_package_bare_name_stays_bare() {
        let server = acp_server("fs", Some("pantheon"));
        let actual = acp_server_display_name(&server, Some("pantheon"));

        assert_eq!(actual, "fs");
        assert_display_name_matches_handoff(
            &server_target_qualified(&server.name, server.package.as_deref()),
            Some("pantheon"),
            &actual,
        );
    }

    #[test]
    fn acp_manual_top_level_bare_name_stays_bare_at_top_level() {
        let server = acp_server("fs", None);
        let actual = acp_server_display_name(&server, None);

        assert_eq!(actual, "fs");
        assert_display_name_matches_handoff(
            &server_target_qualified(&server.name, server.package.as_deref()),
            None,
            &actual,
        );
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
