//! File-backed middleware loading and global → tool-server/package → agent resolution.

use std::{collections::HashSet, path::Path};

use anyhow::{bail, Context, Result};
use fancy_regex::Regex;
use harnx_core::middleware::{
    script_language, MiddlewareConfig, MiddlewareDefinition, MiddlewareEntry, MiddlewareOrigin,
    MiddlewareSource, MiddlewareSourceConfig, ResolvedMiddleware, ResolvedMiddlewareChain,
};

use super::{AgentConfig, Config, ToolServerConfig};

impl Config {
    /// Resolve the complete ordered chain before filtering by the called tool's name.
    /// Tool-server entries participate even in unmanaged workers; no supervisor is needed.
    pub fn resolved_middleware(&self) -> Result<ResolvedMiddlewareChain> {
        self.resolved_middleware_for_agent(self.agent.as_deref())
    }

    pub(super) fn resolved_middleware_for_agent(
        &self,
        agent: Option<&AgentConfig>,
    ) -> Result<ResolvedMiddlewareChain> {
        let mut entries = Vec::new();
        apply_level(
            &mut entries,
            &self.middleware,
            &file_origin(&Self::config_file(), None)?,
        )?;
        apply_server_level(&mut entries, &self.tool_servers)?;
        apply_agent_level(&mut entries, agent)?;
        Ok(ResolvedMiddlewareChain::new(entries))
    }
}

fn apply_server_level(
    entries: &mut Vec<ResolvedMiddleware>,
    servers: &[ToolServerConfig],
) -> Result<()> {
    let mut names = HashSet::new();
    servers
        .iter()
        .filter(|server| server.enabled)
        .try_for_each(|server| {
            let origin = server_origin(server)?;
            validate_server_names(&server.middleware, &origin, &mut names)?;
            apply_level(entries, &server.middleware, &origin)
        })
}

fn validate_server_names(
    config: &MiddlewareConfig,
    origin: &MiddlewareOrigin,
    names: &mut HashSet<String>,
) -> Result<()> {
    for entry in &config.entries {
        if !names.insert(entry.name().to_owned()) {
            bail!(
                "middleware '{}': duplicate name at tool-server/package level in '{}'",
                entry.name(),
                origin.file.display()
            );
        }
    }
    Ok(())
}

fn server_origin(server: &ToolServerConfig) -> Result<MiddlewareOrigin> {
    if let Some(origin) = &server.middleware.origin {
        return Ok(origin.clone());
    }
    let base = server
        .package
        .as_deref()
        .map(harnx_core::config_paths::package_dir);
    let file = base
        .clone()
        .unwrap_or_else(harnx_core::config_paths::config_dir)
        .join("tool_servers")
        .join(format!("{}.yaml", server.name));
    file_origin(&file, base.as_deref())
}

fn apply_agent_level(
    entries: &mut Vec<ResolvedMiddleware>,
    agent: Option<&AgentConfig>,
) -> Result<()> {
    let Some(agent) = agent else {
        return Ok(());
    };
    let origin = agent_origin(agent)?;
    apply_level(entries, &agent.middleware, &origin)
}

fn agent_origin(agent: &AgentConfig) -> Result<MiddlewareOrigin> {
    if let Some(origin) = &agent.middleware.origin {
        return Ok(origin.clone());
    }
    let file = Config::agent_file(agent.name());
    let base = agent
        .name()
        .split_once('/')
        .map(|(package, _)| harnx_core::config_paths::package_dir(package));
    file_origin(&file, base.as_deref())
}

pub(super) fn file_origin(file: &Path, package_dir: Option<&Path>) -> Result<MiddlewareOrigin> {
    // Freeze relative declarations against load-time cwd. Later resolution must
    // not change when a frontend changes its working directory.
    let file = std::path::absolute(file).context("Failed to resolve middleware declaring file")?;
    let base_dir = match package_dir {
        Some(directory) => std::path::absolute(directory)
            .context("Failed to resolve middleware package directory")?,
        None => file.parent().unwrap_or(Path::new(".")).to_owned(),
    };
    Ok(MiddlewareOrigin { file, base_dir })
}

/// Bind paths at load time, not supervisor start, so unmanaged/cloud workers
/// validate exactly the same declarations. Disable entries never read a script.
pub(super) fn bind_middleware(
    config: &mut MiddlewareConfig,
    origin: MiddlewareOrigin,
) -> Result<()> {
    for entry in &config.entries {
        if let MiddlewareEntry::Full(definition) = entry {
            resolve_definition(definition, &origin)?;
        }
    }
    config.origin = Some(origin);
    Ok(())
}

fn apply_level(
    entries: &mut Vec<ResolvedMiddleware>,
    config: &MiddlewareConfig,
    fallback_origin: &MiddlewareOrigin,
) -> Result<()> {
    let origin = config.origin.as_ref().unwrap_or(fallback_origin);
    let mut names = HashSet::new();
    for entry in &config.entries {
        if !names.insert(entry.name()) {
            bail!(
                "middleware '{}': duplicate name in '{}'",
                entry.name(),
                origin.file.display()
            );
        }
        let index = entries
            .iter()
            .position(|current| current.name == entry.name());
        match entry {
            MiddlewareEntry::Disable { name, .. } => {
                let Some(index) = index else {
                    bail!(
                        "middleware '{name}' in '{}': cannot disable an unknown earlier entry",
                        origin.file.display()
                    );
                };
                entries.remove(index);
            }
            MiddlewareEntry::Full(definition) => {
                let resolved = resolve_definition(definition, origin)?;
                if let Some(index) = index {
                    entries[index] = resolved;
                } else {
                    entries.push(resolved);
                }
            }
        }
    }
    Ok(())
}

fn resolve_definition(
    definition: &MiddlewareDefinition,
    origin: &MiddlewareOrigin,
) -> Result<ResolvedMiddleware> {
    let source = match &definition.source {
        MiddlewareSourceConfig::Bash { bash } => MiddlewareSource::Bash(bash.clone()),
        MiddlewareSourceConfig::Python { python } => MiddlewareSource::Python(python.clone()),
        MiddlewareSourceConfig::Script { script } => {
            let path = origin.base_dir.join(script);
            let python = script_language(&path)
                .map_err(anyhow::Error::msg)
                .with_context(|| {
                    format!(
                        "middleware '{}' in '{}'",
                        definition.name,
                        origin.file.display()
                    )
                })?;
            let text = std::fs::read_to_string(&path).with_context(|| {
                format!(
                    "middleware '{}' in '{}': failed to read script '{}'",
                    definition.name,
                    origin.file.display(),
                    path.display()
                )
            })?;
            if python {
                MiddlewareSource::Python(text)
            } else {
                MiddlewareSource::Bash(text)
            }
        }
    };
    let matcher = Regex::new(&definition.matcher).with_context(|| {
        format!(
            "middleware '{}' in '{}': invalid match regex",
            definition.name,
            origin.file.display()
        )
    })?;
    Ok(ResolvedMiddleware {
        name: definition.name.clone(),
        matcher,
        source,
        tools: definition.tools.clone(),
        fetch: definition.fetch.clone(),
        timeout: definition.timeout,
        on_error: definition.on_error,
        origin: origin.clone(),
    })
}

#[cfg(test)]
mod tests;
