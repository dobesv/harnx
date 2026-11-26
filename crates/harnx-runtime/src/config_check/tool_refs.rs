//! Tool selectors agents and config write, collected with where each one is
//! written and the package view it resolves in.

use std::path::{Path, PathBuf};

use harnx_core::agent_config::split_tool_selectors;
use harnx_core::config_paths::{PACKAGES_DIR_NAME, TOOL_SERVERS_DIR_NAME};
use harnx_core::middleware::{MiddlewareConfig, MiddlewareEntry};

use super::references::tool_server_file;
use super::{ConfigLayout, LoadedAgent};
use crate::config::{Config, ToolServerConfig};
use crate::nats_worker::tool_servers_for_view;

/// One tool selector as written in a config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRef {
    pub selector: String,
    pub source: PathBuf,
    /// `use_tools`, `bash_tools`, `toolsets`, `middleware.tools` or
    /// `script_tools.tools`.
    pub key: &'static str,
    /// The package whose view of tool names the selector is resolved in.
    pub agent_package: Option<String>,
}

/// Every tool selector in the checked files: agents' `use_tools` and
/// `bash_tools`, `config.yaml`'s `use_tools` and `toolsets:` members,
/// middleware `tools:` lists in `config.yaml`, tool servers and agents, and
/// script tools' `tools:`. A selector naming a toolset stands for its members.
pub fn collect_tool_refs(
    layout: &ConfigLayout,
    config: &Config,
    agents: &[LoadedAgent],
) -> Vec<ToolRef> {
    let mut refs = RefCollector {
        config,
        refs: Vec::new(),
    };
    // Without a config file, config-level selectors come only from the
    // environment; findings still name the file that would hold them.
    let config_file = layout
        .config_file
        .clone()
        .unwrap_or_else(|| layout.root.join("config.yaml"));
    refs.add(
        &config_file,
        "use_tools",
        None,
        config.use_tools.iter().flatten(),
    );
    for members in config.toolsets.values() {
        refs.add_members(&config_file, members);
    }
    refs.add_middleware(&config_file, None, &config.middleware);
    for server in config.tool_servers.iter().filter(|server| server.enabled) {
        let file = tool_server_file(layout, server)
            .unwrap_or_else(|| tool_servers_dir(layout, server.package.as_deref()));
        refs.add_middleware(&file, server.package.as_deref(), &server.middleware);
    }
    for agent in agents {
        let package = agent.package.as_deref();
        let use_tools = agent.config.use_tools().unwrap_or_default();
        let bash_tools = agent.config.bash_tools().unwrap_or_default();
        refs.add(&agent.file, "use_tools", package, &use_tools);
        refs.add(&agent.file, "bash_tools", package, &bash_tools);
        refs.add_middleware(&agent.file, package, &agent.config.middleware);
    }
    for tool in config.script_tool_catalog().iter() {
        let location = tool.location();
        let package = location.package.as_deref();
        let tools = &tool.definition().tools;
        refs.add(&location.path, "script_tools.tools", package, tools);
    }
    refs.refs
}

/// The `tool_servers/` directory a server's file belongs in, for a server
/// the layout lists no file for, so its findings still say where it lives.
fn tool_servers_dir(layout: &ConfigLayout, package: Option<&str>) -> PathBuf {
    let base = match package {
        Some(name) => layout
            .packages
            .iter()
            .find(|candidate| candidate.name == name)
            .map(|candidate| candidate.dir.clone())
            .unwrap_or_else(|| layout.root.join(PACKAGES_DIR_NAME).join(name)),
        None => layout.root.clone(),
    };
    base.join(TOOL_SERVERS_DIR_NAME)
}

struct RefCollector<'a> {
    config: &'a Config,
    refs: Vec<ToolRef>,
}

impl RefCollector<'_> {
    /// Comma-split, trimmed selectors, with toolset names expanded. In the
    /// top-level view a toolset's members are already checked where
    /// `toolsets:` declares them, so only package views repeat them.
    fn add<'s>(
        &mut self,
        source: &Path,
        key: &'static str,
        package: Option<&str>,
        entries: impl IntoIterator<Item = &'s String>,
    ) {
        for selector in selectors(entries) {
            match self.config.toolset_members(&selector) {
                Some(_) if package.is_none() => {}
                Some(members) => {
                    let members = members.clone();
                    self.push_all(source, key, package, selectors(&members));
                }
                None => self.push_all(source, key, package, [selector]),
            }
        }
    }

    fn add_members(&mut self, source: &Path, members: &[String]) {
        self.push_all(source, "toolsets", None, selectors(members));
    }

    fn add_middleware(
        &mut self,
        source: &Path,
        package: Option<&str>,
        middleware: &MiddlewareConfig,
    ) {
        for entry in &middleware.entries {
            if let MiddlewareEntry::Full(definition) = entry {
                self.add(source, "middleware.tools", package, &definition.tools);
            }
        }
    }

    fn push_all(
        &mut self,
        source: &Path,
        key: &'static str,
        package: Option<&str>,
        selectors: impl IntoIterator<Item = String>,
    ) {
        self.refs
            .extend(selectors.into_iter().map(|selector| ToolRef {
                selector,
                source: source.to_path_buf(),
                key,
                agent_package: package.map(str::to_string),
            }));
    }
}

/// Entries split at top-level commas the way `use_tools` strings are, trimmed,
/// without empty ones.
fn selectors<'s>(entries: impl IntoIterator<Item = &'s String>) -> Vec<String> {
    entries
        .into_iter()
        .flat_map(|entry| split_tool_selectors(entry))
        .map(str::trim)
        .filter(|selector| !selector.is_empty())
        .map(str::to_string)
        .collect()
}

/// The enabled servers the worker would start for this reference.
pub(super) fn reach(config: &Config, tool_ref: &ToolRef) -> Vec<ToolServerConfig> {
    tool_servers_for_view(
        config,
        tool_ref.agent_package.as_deref(),
        std::slice::from_ref(&tool_ref.selector),
    )
}
