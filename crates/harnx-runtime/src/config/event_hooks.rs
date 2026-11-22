//! Event-hook loading and global → tool-server/package → agent resolution.
//!
//! Mirrors `config/middleware.rs`: a later full entry with the same name
//! replaces the earlier one in place (across tool-server files too), and a
//! disable entry removes it. Duplicate names within one file are an error;
//! resolution is per event, so an agent can override its global `Stop` hook
//! without touching the global `SessionStart` one.

use anyhow::{bail, Context, Result};
use harnx_core::hooks::{EventHookDefinition, EventHookEntry, HookEventKind, HooksConfig};
use harnx_core::middleware::{
    script_language, MiddlewareOrigin, MiddlewareSource, MiddlewareSourceConfig,
};

use super::{AgentConfig, Config, ToolServerConfig};
use std::collections::HashSet;

/// A resolved event hook. Tool selectors stay unresolved until provider
/// discovery, so server startup can read them without a running provider.
#[derive(Debug, Clone)]
pub struct ResolvedEventHook {
    pub name: String,
    pub source: MiddlewareSource,
    /// Original helper tool selectors, not discovered tool names.
    pub tools: Vec<String>,
    /// Exact HTTP(S) hosts; empty denies all fetch requests.
    pub fetch: Vec<String>,
    /// Inclusive wall-clock limit for one script run.
    pub timeout: std::time::Duration,
    pub on_error: harnx_core::hooks::EventHookOnError,
}

impl ResolvedEventHook {
    /// Resolve this entry's allowlist after discovery, using the same selection
    /// and exclusions as middleware helpers. `config` must have declarations
    /// cached with names visible from the active `package`.
    pub fn allowed_tool_names(&self, config: &Config, package: Option<&str>) -> HashSet<String> {
        config
            .select_script_tools_for_package(&self.tools, package)
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    }
}

/// The resolved per-event chains, in execution order.
#[derive(Debug, Clone, Default)]
pub struct ResolvedEventHooks {
    pub session_start: Vec<ResolvedEventHook>,
    pub user_prompt_submit: Vec<ResolvedEventHook>,
    pub stop: Vec<ResolvedEventHook>,
    pub stop_failure: Vec<ResolvedEventHook>,
}

impl ResolvedEventHooks {
    pub fn list(&self, event: HookEventKind) -> &[ResolvedEventHook] {
        match event {
            HookEventKind::SessionStart => &self.session_start,
            HookEventKind::UserPromptSubmit => &self.user_prompt_submit,
            HookEventKind::Stop => &self.stop,
            HookEventKind::StopFailure => &self.stop_failure,
        }
    }

    /// Raw selectors from enabled entries across every event. Server startup
    /// must use these instead of resolving names before discovery.
    pub fn tool_selectors(&self) -> impl Iterator<Item = &str> {
        HookEventKind::ALL.into_iter().flat_map(|event| {
            self.list(event)
                .iter()
                .flat_map(|hook| hook.tools.iter().map(String::as_str))
        })
    }

    pub fn is_empty(&self) -> bool {
        HookEventKind::ALL
            .iter()
            .all(|event| self.list(*event).is_empty())
    }
}

impl Config {
    /// Resolve every event's ordered hook chain before a turn runs.
    /// Tool-server entries participate even in unmanaged workers; no
    /// supervisor is needed.
    pub fn resolved_event_hooks(&self) -> Result<ResolvedEventHooks> {
        self.resolved_event_hooks_for_agent(self.agent.as_deref())
    }

    // qual:allow(iosp) — resolution is a straight three-level fold (global, tool servers, agent); helper extraction would obscure the level precedence.
    pub(super) fn resolved_event_hooks_for_agent(
        &self,
        agent: Option<&AgentConfig>,
    ) -> Result<ResolvedEventHooks> {
        let global_origin = super::middleware::file_origin(&Self::config_file(), None)?;
        let mut resolved = ResolvedEventHooks::default();
        if let Some(global) = &self.hooks {
            for event in HookEventKind::ALL {
                apply_level(event, &mut resolved, global, &global_origin)?;
            }
        }
        apply_server_level(&mut resolved, &self.tool_servers)?;
        if let Some(agent) = agent {
            let origin = agent_hooks_origin(agent)?;
            if let Some(hooks) = agent.hooks() {
                for event in HookEventKind::ALL {
                    apply_level(event, &mut resolved, hooks, &origin)?;
                }
            }
        }
        validate_operation_name_collisions(self, &resolved)?;
        Ok(resolved)
    }
}

fn validate_operation_name_collisions(config: &Config, hooks: &ResolvedEventHooks) -> Result<()> {
    for event in HookEventKind::ALL {
        for hook in hooks.list(event) {
            for selector in &hook.tools {
                let selector = selector.trim();
                let sanitized = harnx_core::package_namespace::sanitize_for_tool_name(selector);
                let is_toolset = config.toolsets.contains_key(selector)
                    || (sanitized != selector && config.toolsets.contains_key(&sanitized));
                if !is_toolset
                    && !crate::tool_selector::is_glob(selector)
                    && crate::event_hook_operations::names(event).contains(&selector)
                {
                    bail!(
                        "hook '{}' ({}) has tools selector '{}' that collides with an event-hook operation",
                        hook.name,
                        event.key(),
                        selector
                    );
                }
            }
        }
    }
    Ok(())
}

fn apply_server_level(
    resolved: &mut ResolvedEventHooks,
    servers: &[ToolServerConfig],
) -> Result<()> {
    // Server files fold in deterministic load order, so a same-name hook in a
    // later server replaces an earlier one in place — the same rule levels use
    // (D7). The resolved chain stays unique per event across servers.
    for server in servers.iter().filter(|server| server.enabled) {
        let Some(hooks) = &server.hooks else {
            continue;
        };
        let origin = server_hooks_origin(server)?;
        for event in HookEventKind::ALL {
            apply_level(event, resolved, hooks, &origin)?;
        }
    }
    Ok(())
}

fn server_hooks_origin(server: &ToolServerConfig) -> Result<MiddlewareOrigin> {
    if let Some(origin) = server
        .hooks
        .as_ref()
        .and_then(|hooks| hooks.origin.as_ref())
    {
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
    super::middleware::file_origin(&file, base.as_deref())
}

fn agent_hooks_origin(agent: &AgentConfig) -> Result<MiddlewareOrigin> {
    if let Some(hooks) = agent.hooks() {
        if let Some(origin) = &hooks.origin {
            return Ok(origin.clone());
        }
    }
    let file = Config::agent_file(agent.name());
    let base = agent
        .name()
        .split_once('/')
        .map(|(package, _)| harnx_core::config_paths::package_dir(package));
    super::middleware::file_origin(&file, base.as_deref())
}

/// Bind paths at load time, not at hook execution, so unmanaged/cloud workers
/// validate exactly the same declarations. Disable entries never read a script.
pub(super) fn bind_event_hooks(config: &mut HooksConfig, origin: MiddlewareOrigin) -> Result<()> {
    for event in HookEventKind::ALL {
        for entry in &config.list(event).entries {
            if let EventHookEntry::Full(definition) = entry {
                resolve_definition(definition, &origin).with_context(|| {
                    format!(
                        "hook '{}' in event hooks of '{}'",
                        definition.name,
                        origin.file.display()
                    )
                })?;
            }
        }
    }
    config.origin = Some(origin);
    Ok(())
}

fn apply_level(
    event: HookEventKind,
    resolved: &mut ResolvedEventHooks,
    config: &HooksConfig,
    fallback_origin: &MiddlewareOrigin,
) -> Result<()> {
    let origin = config.origin.as_ref().unwrap_or(fallback_origin);
    let chain = resolved_list_mut(resolved, event);
    for entry in &config.list(event).entries {
        let index = chain
            .iter()
            .position(|current| current.name == entry.name());
        match entry {
            EventHookEntry::Disable { name, .. } => {
                let Some(index) = index else {
                    bail!(
                        "hook '{name}' ({}) in '{}': cannot disable an unknown earlier entry",
                        event.key(),
                        origin.file.display()
                    );
                };
                chain.remove(index);
            }
            EventHookEntry::Full(definition) => {
                let hook = resolve_definition(definition, origin).with_context(|| {
                    format!(
                        "hook '{}' ({}) in '{}'",
                        definition.name,
                        event.key(),
                        origin.file.display()
                    )
                })?;
                if let Some(index) = index {
                    chain[index] = hook;
                } else {
                    chain.push(hook);
                }
            }
        }
    }
    Ok(())
}

fn resolved_list_mut(
    resolved: &mut ResolvedEventHooks,
    event: HookEventKind,
) -> &mut Vec<ResolvedEventHook> {
    match event {
        HookEventKind::SessionStart => &mut resolved.session_start,
        HookEventKind::UserPromptSubmit => &mut resolved.user_prompt_submit,
        HookEventKind::Stop => &mut resolved.stop,
        HookEventKind::StopFailure => &mut resolved.stop_failure,
    }
}

fn resolve_definition(
    definition: &EventHookDefinition,
    origin: &MiddlewareOrigin,
) -> Result<ResolvedEventHook> {
    let source = match &definition.source {
        MiddlewareSourceConfig::Bash { bash } => MiddlewareSource::Bash(bash.clone()),
        MiddlewareSourceConfig::Python { python } => MiddlewareSource::Python(python.clone()),
        MiddlewareSourceConfig::Script { script } => {
            let path = origin.base_dir.join(script);
            script_language(&path).map_err(anyhow::Error::msg)?;
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read hook script '{}'", path.display()))?;
            if script.extension().and_then(|ext| ext.to_str()) == Some("py") {
                MiddlewareSource::Python(text)
            } else {
                MiddlewareSource::Bash(text)
            }
        }
    };
    Ok(ResolvedEventHook {
        name: definition.name.clone(),
        source,
        tools: definition.tools.clone(),
        fetch: definition.fetch.clone(),
        timeout: definition.timeout,
        on_error: definition.on_error,
    })
}

#[cfg(test)]
mod tests;
