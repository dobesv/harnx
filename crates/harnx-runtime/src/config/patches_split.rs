//! Extracted from config/mod.rs for code health.
use crate::client::ClientConfig;
use anyhow::{Context, Result};

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

pub(super) fn load_package_patch(pkg_name: &str) -> Option<harnx_core::package::PackagePatch> {
    let patch_path = harnx_core::config_paths::package_patch_file(pkg_name);
    if !patch_path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&patch_path) {
        Ok(content) => content,
        Err(e) => {
            log::warn!(
                "Failed to read package patch at '{}': {e:#}",
                patch_path.display()
            );
            return None;
        }
    };
    let patch: harnx_core::package::PackagePatch = match serde_yaml::from_str(&content) {
        Ok(patch) => patch,
        Err(e) => {
            log::warn!(
                "Failed to parse package patch at '{}': {e:#}",
                patch_path.display()
            );
            return None;
        }
    };
    Some(patch)
}

/// Apply a list of jq-style patch expressions to a client config.
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
