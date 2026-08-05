use super::*;

use anyhow::{anyhow, Context, Result};
use inquire::{validator::Validation, Text};
use std::path::Path;

pub use harnx_core::agent_config::{
    split_tool_selectors, AgentConfig, AgentRole, AgentVariable, AgentVariables, TEMP_AGENT_NAME,
};

const DEFAULT_AGENT_NAME: &str = "rag";

#[derive(Debug, Clone, Default)]
pub struct Agent {
    config: AgentConfig,
    rag: Option<Arc<Rag>>,
}

impl std::ops::Deref for Agent {
    type Target = AgentConfig;
    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

impl std::ops::DerefMut for Agent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.config
    }
}

impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        Self { config, rag: None }
    }

    pub fn into_config(self) -> AgentConfig {
        self.config
    }

    pub fn rag(&self) -> Option<Arc<Rag>> {
        self.rag.clone()
    }
}

pub fn builtin(name: &str) -> Result<Agent> {
    let content = AgentConfig::builtin_markdown(name)
        .ok_or_else(|| anyhow::anyhow!("Unknown built-in agent `{name}`"))?;
    Ok(Agent::new(AgentConfig::from_markdown(name, content)?))
}

pub fn load(path: &Path) -> Result<Agent> {
    let name = path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("Invalid agent file name: '{}'", path.display()))?;
    load_with_qualified_name(path, name)
}

pub fn load_with_qualified_name(path: &Path, qualified_name: &str) -> Result<Agent> {
    let contents = read_to_string(path)
        .with_context(|| format!("Failed to read agent file at '{}'", path.display()))?;
    load_with_qualified_name_from_contents(path, qualified_name, &contents)
}

fn load_with_qualified_name_from_contents(
    path: &Path,
    qualified_name: &str,
    contents: &str,
) -> Result<Agent> {
    let mut config = AgentConfig::from_markdown(qualified_name, contents)?;
    let _ = path;
    if let Some((pkg, stem)) = qualified_name.split_once('/') {
        apply_package_agent_transforms(&mut config, pkg, stem)?;
    }
    Ok(Agent::new(config))
}

/// Apply patches for package agents.
///
/// Note: `use_tools` entries are intentionally left as the author wrote them
/// (e.g. `fs_read_file`).  The MCP manager is scoped to the active agent at
/// runtime, so same-package servers are already registered under their bare
/// names — no rewriting needed here.
pub fn apply_package_agent_transforms(
    config: &mut AgentConfig,
    pkg_name: &str,
    agent_stem: &str,
) -> Result<()> {
    if let Some(patch) = load_package_patch_for(pkg_name)? {
        apply_agent_patch(config, agent_stem, &patch)?;
    }
    // Rewrite model_id: resolve the client-name part relative to this package.
    // Format is "client:model" or just "client". The leading-slash escape "/foo"
    // resolves to top-level "foo"; "other/foo" passes through unchanged.
    if let Some(model_id) = config.model_id().map(ToOwned::to_owned) {
        let resolved = match model_id.split_once(':') {
            Some((client_part, model_part)) => {
                let resolved_client = harnx_core::package_namespace::resolve_package_relative_name(
                    client_part,
                    Some(pkg_name),
                );
                format!("{resolved_client}:{model_part}")
            }
            None => harnx_core::package_namespace::resolve_package_relative_name(
                &model_id,
                Some(pkg_name),
            ),
        };
        config.set_model_id(Some(resolved));
    }
    // Rewrite model_fallbacks too
    let resolved_fallbacks: Vec<String> = config
        .model_fallbacks()
        .iter()
        .map(|fb| match fb.split_once(':') {
            Some((client_part, model_part)) => {
                let resolved_client = harnx_core::package_namespace::resolve_package_relative_name(
                    client_part,
                    Some(pkg_name),
                );
                format!("{resolved_client}:{model_part}")
            }
            None => {
                harnx_core::package_namespace::resolve_package_relative_name(fb, Some(pkg_name))
            }
        })
        .collect();
    if !resolved_fallbacks.is_empty() {
        config.set_model_fallbacks(resolved_fallbacks);
    }
    Ok(())
}

fn load_package_patch_for(pkg_name: &str) -> Result<Option<harnx_core::package::PackagePatch>> {
    let patch_path = harnx_core::config_paths::package_patch_file(pkg_name);
    if !patch_path.exists() {
        return Ok(None);
    }
    let content = read_to_string(&patch_path)
        .with_context(|| format!("Failed to read package patch at '{}'", patch_path.display()))?;
    let patch = serde_yaml::from_str(&content).with_context(|| {
        format!(
            "Failed to parse package patch at '{}'",
            patch_path.display()
        )
    })?;
    Ok(Some(patch))
}

fn apply_agent_patch(
    config: &mut AgentConfig,
    _agent_stem: &str,
    patch: &harnx_core::package::PackagePatch,
) -> Result<()> {
    if patch.agents.is_empty() {
        return Ok(());
    }
    let input = serde_json::to_value(&*config)
        .with_context(|| "Failed to serialize AgentConfig for jaq patch")?;
    let output = harnx_core::jaq::eval_filters_strict(&patch.agents, input)
        .with_context(|| "jq patch expression failed for agent config")?;
    *config = serde_json::from_value(output)
        .with_context(|| "Failed to deserialize AgentConfig after jaq patch")?;
    Ok(())
}

/// Load file-backed defaults for variables that have a `path:` field.
fn resolve_file_backed_variables(variables: &mut [AgentVariable], agent_dir: &Path) -> Result<()> {
    for variable in variables.iter_mut() {
        if let Some(path_str) = &variable.path {
            if variable.default.is_some() {
                log::warn!(
                    "Variable '{}': both 'path' and 'default' set, using 'path'",
                    variable.name
                );
            }

            let resolved_path = safe_join_path(agent_dir, path_str).ok_or_else(|| {
                anyhow!(
                    "Variable '{}': path '{}' is not allowed (must be relative, no '..' traversal)",
                    variable.name,
                    path_str
                )
            })?;

            let content = std::fs::read_to_string(&resolved_path).with_context(|| {
                format!(
                    "Failed to load file '{}' (resolved to '{}') for variable '{}'",
                    path_str,
                    resolved_path.display(),
                    variable.name
                )
            })?;

            variable.default = Some(content);
        }
    }
    Ok(())
}

/// Load file-backed variable defaults onto the agent's variables.
///
/// For each variable with a `path:` field, reads the file and stores its
/// content as the variable's `default`.  This is the subset of init that
/// must run before `init_agent_session_variables` so that user-provided
/// `agent_variables` can still override file defaults.
pub fn resolve_file_defaults(agent: &mut Agent) -> Result<()> {
    let agent_file_path = Config::agent_file(agent.name());
    let agent_dir = agent_file_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(Config::agents_config_dir);
    resolve_file_backed_variables(agent.config.variables_mut(), &agent_dir)
}

/// Resolve file-backed variable defaults and populate `shared_variables`.
///
/// This performs the synchronous subset of `init()` that loads variable
/// values from files (the `path:` field on agent variables) and then
/// runs `init_agent_variables` with `no_interaction: true`.  It does NOT
/// touch MCP, RAG, or model resolution — call `retrieve_agent` for the
/// model and this method for variables when you need a lightweight agent
/// suitable for non-interactive use (e.g. compaction).
pub fn resolve_variables(agent: &mut Agent) -> Result<()> {
    resolve_file_defaults(agent)?;

    let new_variables = init_agent_variables(
        agent.config.defined_variables(),
        agent.config.shared_variables(),
        true, // no_interaction
    )?;
    agent.set_shared_variables(new_variables);
    Ok(())
}

#[allow(dead_code)]
fn expand_agent_use_tool_selectors(config: &Config, use_tools: Option<Vec<String>>) -> Vec<String> {
    let Some(use_tools) = use_tools.filter(|selectors| !selectors.is_empty()) else {
        return Vec::new();
    };

    split_tool_selectors(&use_tools.join(","))
        .into_iter()
        .flat_map(|selector| {
            let selector = selector.trim();
            config
                .toolsets
                .get(selector)
                .cloned()
                .unwrap_or_else(|| vec![selector.to_string()])
        })
        .collect()
}

pub async fn init(config: &GlobalConfig, name: &str, abort_signal: AbortSignal) -> Result<Agent> {
    let agent_file_path = Config::agent_file(name);
    let mut agent = if agent_file_path.exists() {
        load_with_qualified_name(&agent_file_path, name)?
    } else {
        builtin(name)?
    };

    // Tools are now loaded via NATS tool_servers, not direct MCP
    agent.config.set_tools(Tools::init_from_mcp(None));

    let model = {
        let config = config.read();
        match agent.model_id() {
            Some(model_id) => {
                crate::client::retrieve_model(&config.clients, model_id, ModelType::Chat)?
            }
            None => {
                if agent.temperature().is_none() {
                    agent.config.set_temperature(config.temperature);
                }
                if agent.top_p().is_none() {
                    agent.config.set_top_p(config.top_p);
                }
                config.current_model().clone()
            }
        }
    };
    agent.config.set_resolved_model(model);

    let rag_path = Config::agent_rag_file(name, DEFAULT_AGENT_NAME);
    let agent_dir = agent_file_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(Config::agents_config_dir);

    resolve_file_backed_variables(agent.config.variables_mut(), &agent_dir)?;

    agent.rag = resolve_agent_rag(config, &agent, &rag_path, &agent_dir, abort_signal).await?;

    Ok(agent)
}

/// Resolve the RAG store for an agent during [`init`]: load an existing store
/// if present, otherwise optionally build one from the agent's documents
/// (prompting in interactive mode). Returns `None` when there is no RAG.
async fn resolve_agent_rag(
    config: &GlobalConfig,
    agent: &Agent,
    rag_path: &Path,
    agent_dir: &Path,
    abort_signal: AbortSignal,
) -> Result<Option<Arc<Rag>>> {
    if rag_path.exists() {
        let rag = Rag::load(&config.read().clients, DEFAULT_AGENT_NAME, rag_path)?;
        return Ok(Some(Arc::new(rag)));
    }

    if agent.documents().is_empty() || config.read().info_flag || !confirm_init_rag()? {
        return Ok(None);
    }

    let document_paths = collect_agent_document_paths(agent, agent_dir)?;
    let rag = build_agent_rag(config, rag_path, &document_paths, abort_signal).await?;
    Ok(Some(Arc::new(rag)))
}

/// Prompt the user (only on a TTY) to confirm building a RAG store from the
/// agent's documents. Returns `false` without prompting in non-interactive use.
fn confirm_init_rag() -> Result<bool> {
    if !*IS_STDOUT_TERMINAL {
        return Ok(false);
    }
    Confirm::new("The agent has the documents, init RAG?")
        .with_default(true)
        .prompt()
        .map_err(Into::into)
}

/// Resolve the agent's document references to concrete paths/URLs, rejecting
/// any local path that escapes `agent_dir`.
fn collect_agent_document_paths(agent: &Agent, agent_dir: &Path) -> Result<Vec<String>> {
    let mut document_paths = vec![];
    for path in agent.documents() {
        if is_url(path) {
            document_paths.push(path.to_string());
        } else {
            let new_path = safe_join_path(agent_dir, path)
                .ok_or_else(|| anyhow!("Invalid document path: '{path}'"))?;
            document_paths.push(new_path.display().to_string());
        }
    }
    Ok(document_paths)
}

/// Build a fresh RAG store from `document_paths` using the current config's
/// embedding/reranker settings.
async fn build_agent_rag(
    config: &GlobalConfig,
    rag_path: &Path,
    document_paths: &[String],
    abort_signal: AbortSignal,
) -> Result<Rag> {
    let (
        clients_owned,
        loaders_owned,
        rag_embedding_model_owned,
        rag_reranker_model,
        rag_top_k,
        rag_chunk_size,
        rag_chunk_overlap,
        user_agent_owned,
        dry_run,
    ) = {
        let cfg = config.read();
        (
            cfg.clients.clone(),
            cfg.document_loaders.clone(),
            cfg.rag_embedding_model.clone(),
            cfg.rag_reranker_model.clone(),
            cfg.rag_top_k,
            cfg.rag_chunk_size,
            cfg.rag_chunk_overlap,
            cfg.user_agent.clone(),
            cfg.dry_run,
        )
    };
    let init_ctx = harnx_rag::RagInitContext {
        clients: &clients_owned,
        document_loaders: &loaders_owned,
        rag_embedding_model: rag_embedding_model_owned.as_deref(),
        rag_reranker_model,
        rag_top_k,
        rag_chunk_size,
        rag_chunk_overlap,
        user_agent: user_agent_owned.as_deref(),
        dry_run,
    };
    Rag::init(&init_ctx, "rag", rag_path, document_paths, abort_signal).await
}

pub fn init_agent_variables(
    agent_variables: &[AgentVariable],
    variables: &AgentVariables,
    no_interaction: bool,
) -> Result<AgentVariables> {
    let mut output = IndexMap::new();
    if agent_variables.is_empty() {
        return Ok(output);
    }
    let mut printed = false;
    let mut unset_variables = vec![];
    for agent_variable in agent_variables {
        let key = agent_variable.name.clone();
        if let Some(value) =
            resolve_agent_variable(agent_variable, variables, no_interaction, &mut printed)?
        {
            output.insert(key, value);
        } else if !no_interaction && !*IS_STDOUT_TERMINAL {
            unset_variables.push(agent_variable);
        }
    }
    ensure_no_unset_variables(&unset_variables)?;
    Ok(output)
}

/// Resolve a single agent variable's value: explicit override, then default,
/// then an interactive prompt (TTY only). Returns `None` when the value is
/// still unresolved (no value available in a non-interactive context).
fn resolve_agent_variable(
    agent_variable: &AgentVariable,
    variables: &AgentVariables,
    no_interaction: bool,
    printed: &mut bool,
) -> Result<Option<String>> {
    if let Some(value) = variables.get(&agent_variable.name) {
        return Ok(Some(value.clone()));
    }
    if let Some(value) = agent_variable.default.clone() {
        return Ok(Some(value));
    }
    if no_interaction || !*IS_STDOUT_TERMINAL {
        return Ok(None);
    }
    Ok(Some(prompt_agent_variable(agent_variable, printed)?))
}

/// Interactively prompt for a required agent variable, emitting the
/// "Init agent variables" banner exactly once.
fn prompt_agent_variable(agent_variable: &AgentVariable, printed: &mut bool) -> Result<String> {
    if !*printed {
        crate::utils::emit_info("⚙ Init agent variables...".to_string());
        *printed = true;
    }
    Text::new(&format!(
        "{} ({}):",
        agent_variable.name, agent_variable.description
    ))
    .with_validator(|input: &str| {
        if input.trim().is_empty() {
            Ok(Validation::Invalid("This field is required".into()))
        } else {
            Ok(Validation::Valid)
        }
    })
    .prompt()
    .map_err(Into::into)
}

/// Resolve an agent's variables without ever prompting, failing when a declared
/// variable has no value.
///
/// [`init_agent_variables`] tolerates an unset variable when it is told not to
/// interact, so you can inspect or list an agent without supplying its inputs.
/// A worker cannot afford that: it is about to render the prompt, and a missing
/// value surfaces as an opaque "undefined value" from the template engine
/// instead of naming what the operator has to supply.
pub fn require_agent_variables(
    agent_variables: &[AgentVariable],
    variables: &AgentVariables,
) -> Result<AgentVariables> {
    let mut output = IndexMap::new();
    let mut unset_variables = vec![];
    let mut printed = false;
    for agent_variable in agent_variables {
        match resolve_agent_variable(agent_variable, variables, true, &mut printed)? {
            Some(value) => {
                output.insert(agent_variable.name.clone(), value);
            }
            None => unset_variables.push(agent_variable),
        }
    }
    ensure_no_unset_variables(&unset_variables)?;
    Ok(output)
}

/// Bail with a descriptive error listing all required variables that were left
/// unset in a non-interactive context.
fn ensure_no_unset_variables(unset_variables: &[&AgentVariable]) -> Result<()> {
    if unset_variables.is_empty() {
        return Ok(());
    }
    bail!(
        "The following agent variables are required:\n{}",
        unset_variables
            .iter()
            .map(|v| format!("  - {}: {}", v.name, v.description))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

pub fn list_agents() -> Vec<String> {
    let mut output = list_local_agent_names();
    output.extend(list_package_agent_names());
    output.extend(list_remote_agent_names(None));
    output.sort();
    output.dedup();
    output
}

/// Markdown agent stems in the top-level agents config dir.
fn list_local_agent_names() -> Vec<String> {
    let Ok(entries) = read_dir(Config::agents_config_dir()) else {
        return vec![];
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| markdown_stem(&entry.path()))
        .collect()
}

/// Package agents discovered under `packages/<pkg>/agents`, returned as
/// qualified `pkg/stem` names.
fn list_package_agent_names() -> Vec<String> {
    let packages_dir = harnx_core::config_paths::packages_dir();
    let Ok(pkg_entries) = read_dir(&packages_dir) else {
        return vec![];
    };
    let mut names = vec![];
    for pkg_entry in pkg_entries.filter_map(|e| e.ok()) {
        let pkg_path = pkg_entry.path();
        let Some(pkg_name) = package_name(&pkg_path) else {
            continue;
        };
        let agents_dir = pkg_path.join(harnx_core::config_paths::AGENTS_DIR_NAME);
        let Ok(agent_entries) = read_dir(&agents_dir) else {
            continue;
        };
        for agent_entry in agent_entries.filter_map(|e| e.ok()) {
            if let Some(stem) = markdown_stem(&agent_entry.path()) {
                names.push(format!("{pkg_name}/{stem}"));
            }
        }
    }
    names
}

fn list_remote_agent_names(role_filter: Option<AgentRole>) -> Vec<String> {
    let nats_servers_dir = Config::config_dir().join(paths::NATS_SERVERS_DIR_NAME);
    let Ok(servers) = Config::load_nats_servers_from_dir(&nats_servers_dir) else {
        return vec![];
    };

    let mut names = vec![];
    for server in servers {
        let cluster_name = server.name;
        for agent in server.agents {
            if role_filter.as_ref().is_some_and(|role| agent.role != *role) {
                continue;
            }
            names.push(format!("{}@{}", agent.name, cluster_name));
        }
    }
    names
}

/// File stem of a markdown (`.md`) file, or `None` for other files.
fn markdown_stem(path: &Path) -> Option<String> {
    if path.extension().and_then(|x| x.to_str()) != Some("md") {
        return None;
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
}

/// Package directory name, skipping non-directories and hidden entries.
fn package_name(pkg_path: &Path) -> Option<String> {
    if !pkg_path.is_dir() {
        return None;
    }
    match pkg_path.file_name().and_then(|n| n.to_str()) {
        Some(n) if !n.starts_with('.') => Some(n.to_string()),
        _ => None,
    }
}

/// If `path` is a markdown agent file whose role matches [`AgentRole::Assistant`],
/// returns the parsed agent's stem plus its content. Returns `None` for files
/// that aren't markdown, fail to read, fail to parse, or aren't assistants.
async fn read_assistant_agent(path: &Path, name_for_parse: &str) -> Option<String> {
    if path.extension().and_then(|x| x.to_str()) != Some("md") {
        return None;
    }
    let stem = path.file_stem().and_then(|s| s.to_str())?.to_string();
    let contents = tokio::fs::read_to_string(path).await.ok()?;
    let config = AgentConfig::from_markdown(name_for_parse, &contents).ok()?;
    (config.role == AgentRole::Assistant).then_some(stem)
}

/// Collects assistant agent names from a directory of agent markdown files.
/// Each entry uses `name_for(stem)` to compute the display name and the name
/// passed to [`AgentConfig::from_markdown`].
async fn collect_assistant_agents_in_dir<F>(dir: &Path, name_for: F) -> Vec<String>
where
    F: Fn(&str) -> String,
{
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let display_name = name_for(stem);
        if read_assistant_agent(&path, &display_name).await.is_some() {
            out.push(display_name);
        }
    }
    out
}

/// Returns names of agents whose role is [`AgentRole::Assistant`].
/// Unlike [`list_agents`], this reads and parses each agent file.
/// Silently skips files that fail to parse.
///
/// Includes agents from the top-level `agents/` directory (bare names) and
/// from `packages/<pkg>/agents/` directories (as `pkg/stem` qualified names).
pub async fn list_assistant_agents() -> Vec<String> {
    let mut output =
        collect_assistant_agents_in_dir(&Config::agents_config_dir(), |stem| stem.to_string())
            .await;
    output.extend(list_remote_agent_names(Some(AgentRole::Assistant)));

    let packages_dir = harnx_core::config_paths::packages_dir();
    if let Ok(mut pkg_dir) = tokio::fs::read_dir(&packages_dir).await {
        while let Ok(Some(pkg_entry)) = pkg_dir.next_entry().await {
            let pkg_path = pkg_entry.path();
            if !pkg_path.is_dir() {
                continue;
            }
            let Some(pkg_name) = pkg_path
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| !n.starts_with('.'))
                .map(|s| s.to_string())
            else {
                continue;
            };
            let pkg_agents_dir = pkg_path.join(harnx_core::config_paths::AGENTS_DIR_NAME);
            let qualified = collect_assistant_agents_in_dir(&pkg_agents_dir, |stem| {
                format!("{pkg_name}/{stem}")
            })
            .await;
            output.extend(qualified);
        }
    }

    output.sort();
    output.dedup();
    output
}

pub fn complete_agent_variables(agent_name: &str) -> Vec<(String, Option<String>)> {
    let markdown_path = Config::agent_file(agent_name);
    if markdown_path.exists() {
        if let Ok(agent) = load_with_qualified_name(&markdown_path, agent_name) {
            return agent
                .defined_variables()
                .iter()
                .map(|v| {
                    let description = match &v.default {
                        Some(default) => format!("{} [default: {default}]", v.description),
                        None => v.description.clone(),
                    };
                    (format!("{}=", v.name), Some(description))
                })
                .collect();
        }
    }
    vec![]
}

/// Render a fully-rendered agent dump (agent-md format) for an arbitrary agent.
///
/// This is the orchestration function reused by both CLI and TUI to ensure
/// consistent behavior. The pipeline is:
/// 1. Load agent config by name (from file or builtin)
/// 2. Apply package patches via `apply_package_agent_transforms`
/// 3. Expand `use_tools` via `Config::expand_use_tools`
/// 4. Assemble via `AgentConfig::export_rendered`
///
/// Tool declarations come from NATS tool providers configured for the runtime,
/// so CLI and TUI callers can use the same function without requiring a live
/// session.
///
/// # Errors
///
/// Returns a clear error if the agent is not found.
///
/// # Example
///
/// ```ignore
/// let config = Config::load_from_file(&Config::config_file())?;
/// let rendered = render_agent_dump(&config, "my-agent")?;
/// println!("{}", rendered);
/// ```
pub fn render_agent_dump(config: &Config, agent_name: &str) -> Result<String> {
    // Step 1: Load agent config by name
    let agent_file_path = Config::agent_file(agent_name);
    let mut agent_config = if agent_file_path.exists() {
        let contents = read_to_string(&agent_file_path).with_context(|| {
            format!(
                "Failed to read agent file at '{}'",
                agent_file_path.display()
            )
        })?;
        AgentConfig::from_markdown(agent_name, &contents)?
    } else {
        // Try builtin agent
        AgentConfig::builtin_markdown(agent_name)
            .map(|content| AgentConfig::from_markdown(agent_name, content))
            .ok_or_else(|| anyhow!("agent '{}' not found", agent_name))??
    };

    // Step 2: Apply package patches (must happen BEFORE interpolation)
    if let Some((pkg, stem)) = agent_name.split_once('/') {
        apply_package_agent_transforms(&mut agent_config, pkg, stem)?;
    }

    // Step 3: Resolve file-backed variable defaults (like resolve_file_defaults)
    let agent_dir = agent_file_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(Config::agents_config_dir);

    // Load file-backed variable defaults
    resolve_file_backed_variables(agent_config.variables_mut(), &agent_dir)?;

    // Step 4: Initialize shared_variables for template interpolation
    // Start from defined-variable defaults, then overlay CLI-provided agent_variables
    // (CLI values win; unspecified vars keep their defaults). This ensures a single
    // CLI override doesn't wipe out other defaults (which would make MiniJinja
    // Strict-undefined interpolation fail).
    let mut shared_variables = AgentVariables::default();
    for v in agent_config.defined_variables() {
        if let Some(default) = &v.default {
            shared_variables.insert(v.name.clone(), default.clone());
        }
    }
    if let Some(variables) = &config.agent_variables {
        // Overlay CLI-provided values on top of defaults (CLI wins)
        shared_variables.extend(variables.clone());
    }
    agent_config.set_shared_variables(shared_variables);

    // Step 5: Expand use_tools via Config::expand_use_tools
    let active_pkg = harnx_core::package_namespace::pkg_from_qualified(agent_name);
    let expanded_tools = config.expand_use_tools(agent_config.use_tools().as_deref(), active_pkg);

    // Step 6: Export rendered (interpolates body internally)
    agent_config.export_rendered(&expanded_tools)
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod agent_tests;

#[cfg(test)]
#[path = "agent_dump_tests.rs"]
mod agent_dump_tests;
