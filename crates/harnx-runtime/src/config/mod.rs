pub mod agent;
mod agent_ops_split;
mod attachments;
mod compaction;
mod completion_split;
mod env_split;
pub mod input;
mod loader_split;
mod macros_split;
mod nats_split;
mod patches_split;
mod paths_split;
mod persistence_split;
mod rag_split;
mod servers_split;
pub mod session;
mod session_dump;
mod session_externalize;
mod session_log_split;
pub mod session_meta;
mod session_ops_compaction;
mod session_ops_split;
mod settings_split;

pub use self::env_split::load_env_file;
pub use self::macros_split::macro_execute;
pub use self::nats_split::NatsServerConfig;
pub(crate) use self::persistence_split::collect_tool_calls;
pub use self::session_dump::render_session_dump;
pub(crate) use self::session_log_split::{
    adjust_range_for_tool_pairs, split_session_log_documents, validate_edited_session_documents,
    validate_tool_pair_integrity,
};

pub use self::agent::TEMP_AGENT_NAME;
pub use self::agent::{
    apply_package_agent_transforms, complete_agent_variables, list_agents, list_assistant_agents,
    render_agent_dump, Agent, AgentConfig, AgentVariables,
};
pub(crate) use self::attachments::attachments_dir_for;
pub use self::attachments::{write_attachment, Base64Encoder};
pub use self::input::Input;
pub use self::patches_split::acp_server_display_name;
pub use self::patches_split::mcp_server_display_name;
use self::patches_split::{
    apply_client_patch, apply_mcp_server_patch, load_package_mcp_patch, package_dir_name,
};
use self::session::Session;
pub use self::session_meta::{
    build_picker_context, find_matching_session, parse_session_meta, sort_sessions_for_picker,
    PickerContext, SessionMeta,
};
pub use harnx_core::attachments::{
    expand_passthrough_reference, read_attachment, AttachmentRefCache, CachedRef,
    ExpandedAttachment, CID_PREFIX,
};
pub use harnx_core::last_message::LastMessage;
#[allow(unused_imports)]
pub use harnx_core::macros::{Macro, MacroVariable};
pub use harnx_core::model::ModelsOverride;
pub use harnx_core::path::ensure_parent_exists;
pub use harnx_core::working_mode::WorkingMode;

use harnx_core::config_data::ConfigData;
use harnx_core::config_paths as paths;
use harnx_core::session::SessionLogEntry;

use crate::client::{
    create_client_config, list_client_types, list_models, ClientConfig, Model, ModelType,
    ProviderModels, OPENAI_COMPATIBLE_PROVIDERS,
};
use crate::commands::{run_command, split_args_text};
use crate::tool::{ToolDeclaration, ToolResult, Tools};
use crate::utils::*;
use harnx_acp::{AcpManager, AcpServerConfig};
use harnx_hooks::{AsyncHookManager, HooksConfig};
use harnx_mcp::{McpManager, McpServerConfig};
use harnx_rag::Rag;
use harnx_render::{MarkdownRender, RenderOptions};

use anyhow::{anyhow, bail, Context, Result};
use globset::GlobBuilder;
use indexmap::IndexMap;
use inquire::{list_option::ListOption, validator::Validation, Confirm, MultiSelect, Select, Text};
use parking_lot::RwLock;
use serde_json::json;
use simplelog::LevelFilter;
use std::collections::{HashMap, HashSet};
use std::{
    env,
    fs::{read_dir, read_to_string, remove_dir_all, remove_file},
    path::{Path, PathBuf},
    process,
    sync::{Arc, OnceLock},
};
use syntect::highlighting::ThemeSet;
use terminal_colorsaurus::{theme_mode, QueryOptions, ThemeMode};

pub use harnx_rag::TEMP_RAG_NAME;

const SERVE_ADDR: &str = "127.0.0.1:8000";

const SYNC_MODELS_URL: &str =
    "https://raw.githubusercontent.com/dobesv/harnx/refs/heads/main/models.yaml";

const RAG_TEMPLATE: &str = r#"Answer the query based on the context while respecting the rules. (user query, some textual context and rules, all inside xml tags)

<context>
__CONTEXT__
</context>

<rules>
- If you don't know, just say so.
- If you are not sure, ask for clarification.
- Answer in the same language as the user query.
- If the context appears unreadable or of poor quality, tell the user then answer as best as you can.
- If the answer is not in the context but you think you know the answer, explain that to the user then answer with your own knowledge.
- Answer directly and without using xml tags.
</rules>

<user_query>
__INPUT__
</user_query>"#;

static EDITOR: OnceLock<Option<String>> = OnceLock::new();

use harnx_core::agent_config::{normalize_toolset_value, split_tool_selectors, ToolsetValue};

fn parse_toolsets_json(value: &str) -> serde_json::Result<IndexMap<String, Vec<String>>> {
    let values = serde_json::from_str::<IndexMap<String, ToolsetValue>>(value)?;
    Ok(values
        .into_iter()
        .map(|(key, value)| (key, normalize_toolset_value(value)))
        .collect())
}

struct SessionSaveRequest<'a> {
    input: Input,
    output: &'a str,
    thought: Option<&'a str>,
}

impl<'a> SessionSaveRequest<'a> {
    fn new(input: &Input, output: &'a str, thought: Option<&'a str>) -> Self {
        let mut input = input.clone();
        input.clear_patch();
        Self {
            input,
            output,
            thought,
        }
    }
}

/// Check whether a glob pattern matches a tool name.
/// Returns `false` if the pattern is invalid (graceful degradation).
fn matches_tool_glob(pattern: &str, name: &str) -> bool {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .ok()
        .is_some_and(|g| g.compile_matcher().is_match(name))
}

/// Extract a valid package name from a directory path entry.
/// Returns None for non-directories and hidden directories (starting with '.').
fn handoff_tool_declarations_for_agents(
    active_pkg: Option<&str>,
) -> (Vec<ToolDeclaration>, HashMap<String, String>) {
    let mut handoff_targets = HashMap::new();
    let declarations = crate::config::agent::list_agents()
        .into_iter()
        .map(|agent_name| {
            let display_name =
                harnx_core::package_namespace::handoff_display_name(&agent_name, active_pkg);
            handoff_targets.insert(display_name.clone(), agent_name.clone());

            let mut properties = IndexMap::new();
            properties.insert(
                "prompt".to_string(),
                crate::tool::JsonSchema {
                    type_value: Some("string".to_string()),
                    description: Some("The new prompt to start the target agent session with.".to_string()),
                    ..Default::default()
                },
            );
            properties.insert(
                "session_id".to_string(),
                crate::tool::JsonSchema {
                    type_value: Some("string".to_string()),
                    description: Some(
                        "Optional target session ID selecting which session the target agent starts under. Handoff clears conversation history, so even when a session is reused its prior messages are not visible to the target agent. Do not rely on earlier context being available — pass everything the target needs in `prompt`.".to_string(),
                    ),
                    ..Default::default()
                },
            );
            ToolDeclaration {
                name: format!("{display_name}_session_handoff"),
                description: format!(
                    "Exit the current agent session and hand off to the '{agent_name}' agent, which starts fresh. Prior conversation history is not carried over — it is intentionally cleared on handoff. Only the `prompt` argument provides context to the target agent, so include everything it needs there."
                ),
                parameters: crate::tool::JsonSchema {
                    type_value: Some("object".to_string()),
                    properties: Some(properties),
                    required: Some(vec!["prompt".to_string()]),
                    ..Default::default()
                },
                mcp_tool_name: None,
                mcp_server_name: None,
                call_template: None,
                result_template: None,
                idempotent_hint: None,
                read_only_hint: None,
            }
        })
        .collect();

    (declarations, handoff_targets)
}

pub struct Config {
    pub data: ConfigData,

    // Server-config vectors (types live in dependent crates — stay here,
    // not in ConfigData, to avoid reverse deps from harnx-core).
    pub clients: Vec<ClientConfig>,
    pub nats_servers: Vec<NatsServerConfig>,
    pub mcp_servers: Vec<McpServerConfig>,
    pub acp_servers: Vec<AcpServerConfig>,

    // Runtime state — unchanged from pre-A2:
    pub model_cooldowns: std::sync::Arc<parking_lot::Mutex<crate::client::retry::ModelCooldownMap>>,
    pub macro_flag: bool,
    pub info_flag: bool,
    pub show_sequence_numbers: bool,
    pub show_timestamps: bool,
    pub agent_variables: Option<AgentVariables>,
    pub mcp_root: Vec<String>,

    pub model: Model,
    pub tools: Tools,
    pub mcp_manager: Option<Arc<McpManager>>,
    pub acp_manager: Option<Arc<AcpManager>>,
    pub working_mode: WorkingMode,
    pub last_message: Option<LastMessage>,

    pub session: Option<Session>,
    pub rag: Option<Arc<Rag>>,
    pub agent: Option<Agent>,
    /// Remote agent metadata for NATS thin-client mode.
    /// When set, the agent runs on a remote worker and this client
    /// drives the turn via `ThinClientSession`.
    pub remote_agent: Option<(String, String)>, // (agent_name, cluster)
    pub tui_before_editor: Option<Box<dyn FnMut() + Send + Sync>>,
    pub tui_after_editor: Option<Box<dyn FnMut() + Send + Sync>>,
    /// Runtime-only override for tool-use confirmation prompts. When set (the
    /// TUI installs one), a `PreToolUse` hook returning `ask` is resolved
    /// through this callback instead of the default `inquire` terminal prompt,
    /// so confirmation renders as a native ratatui modal rather than fighting
    /// the alternate-screen TUI. `None` keeps the CLI/inquire behavior.
    pub tui_confirm_tool_use: Option<Arc<crate::tool::ConfirmToolUseFn>>,

    /// Override the sessions directory — used in tests to redirect session
    /// log writes to a temp directory without touching real user data.
    pub sessions_dir_override: Option<std::path::PathBuf>,
    /// Override the directory used for editor temp files — used in tests so
    /// the after-hook closure can find the file without scanning the global
    /// temp directory. Never set in production.
    pub temp_dir_override: Option<std::path::PathBuf>,
}

impl std::ops::Deref for Config {
    type Target = ConfigData;
    fn deref(&self) -> &ConfigData {
        &self.data
    }
}

impl std::ops::DerefMut for Config {
    fn deref_mut(&mut self) -> &mut ConfigData {
        &mut self.data
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("data", &self.data)
            .field("clients", &self.clients)
            .field("mcp_servers", &self.mcp_servers)
            .field("acp_servers", &self.acp_servers)
            .field("macro_flag", &self.macro_flag)
            .field("info_flag", &self.info_flag)
            .field("agent_variables", &self.agent_variables)
            .field("mcp_root", &self.mcp_root)
            .field("model", &self.model)
            .field("tools", &self.tools)
            .field("mcp_manager", &self.mcp_manager)
            .field("acp_manager", &self.acp_manager)
            .field("working_mode", &self.working_mode)
            .field("last_message", &self.last_message)
            .field("session", &self.session)
            .field("rag", &self.rag)
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

impl Clone for Config {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            clients: self.clients.clone(),
            nats_servers: self.nats_servers.clone(),
            mcp_servers: self.mcp_servers.clone(),
            acp_servers: self.acp_servers.clone(),
            model_cooldowns: self.model_cooldowns.clone(),
            macro_flag: self.macro_flag,
            info_flag: self.info_flag,
            show_sequence_numbers: self.show_sequence_numbers,
            show_timestamps: self.show_timestamps,
            agent_variables: self.agent_variables.clone(),
            mcp_root: self.mcp_root.clone(),
            model: self.model.clone(),
            tools: self.tools.clone(),
            mcp_manager: self.mcp_manager.clone(),
            acp_manager: self.acp_manager.clone(),
            working_mode: self.working_mode.clone(),
            last_message: self.last_message.clone(),
            session: self.session.clone(),
            rag: self.rag.clone(),
            agent: self.agent.clone(),
            remote_agent: self.remote_agent.clone(),
            tui_before_editor: None,
            tui_after_editor: None,
            tui_confirm_tool_use: None,
            sessions_dir_override: self.sessions_dir_override.clone(),
            temp_dir_override: self.temp_dir_override.clone(),
        }
    }
}

impl Config {
    /// Build an isolated copy of this config for running a single prompt in
    /// its own session, without disturbing the original.
    ///
    /// SHARED (cheap `Arc`/value clones — the fork sees the same underlying
    /// runtime resources): `mcp_manager`, `acp_manager`, `rag`,
    /// `model_cooldowns`, plus config data, clients, model, tools, agent, and
    /// all flags/overrides.
    ///
    /// ISOLATED / RESET: `session` is `None` so the caller can attach its own
    /// session (via `use_session`) without racing the source config's active
    /// session. The two `tui_*_editor` hooks are dropped to `None` — they are
    /// non-`Clone` `FnMut` trait objects and the ACP server prompt path never
    /// invokes interactive editor hooks, so dropping them is both safe and
    /// required.
    pub fn fork_session_scope(&self) -> Config {
        Config {
            data: self.data.clone(),
            clients: self.clients.clone(),
            nats_servers: self.nats_servers.clone(),
            mcp_servers: self.mcp_servers.clone(),
            acp_servers: self.acp_servers.clone(),
            model_cooldowns: self.model_cooldowns.clone(),
            macro_flag: self.macro_flag,
            info_flag: self.info_flag,
            show_sequence_numbers: self.show_sequence_numbers,
            show_timestamps: self.show_timestamps,
            agent_variables: self.agent_variables.clone(),
            mcp_root: self.mcp_root.clone(),
            model: self.model.clone(),
            tools: self.tools.clone(),
            mcp_manager: self.mcp_manager.clone(),
            acp_manager: self.acp_manager.clone(),
            working_mode: self.working_mode.clone(),
            last_message: self.last_message.clone(),
            session: None,
            rag: self.rag.clone(),
            agent: self.agent.clone(),
            remote_agent: self.remote_agent.clone(),
            // ACP server prompt path never invokes editor hooks. Drop them so
            // forked prompt configs can own isolated session state without
            // trying to clone `FnMut` trait objects.
            tui_before_editor: None,
            tui_after_editor: None,
            tui_confirm_tool_use: None,
            sessions_dir_override: self.sessions_dir_override.clone(),
            temp_dir_override: self.temp_dir_override.clone(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data: ConfigData::default(),

            clients: vec![],
            nats_servers: vec![],
            mcp_servers: vec![],
            acp_servers: vec![],

            model_cooldowns: std::sync::Arc::new(parking_lot::Mutex::new(Default::default())),
            macro_flag: false,
            info_flag: false,
            show_sequence_numbers: ConfigData::default().show_sequence_numbers,
            show_timestamps: ConfigData::default().show_timestamps,
            agent_variables: None,
            mcp_root: vec![],

            model: Default::default(),
            tools: Default::default(),
            mcp_manager: None,
            acp_manager: None,
            working_mode: WorkingMode::Cmd,
            last_message: None,

            session: None,
            rag: None,
            agent: None,
            remote_agent: None,
            tui_before_editor: None,
            tui_after_editor: None,
            tui_confirm_tool_use: None,
            sessions_dir_override: None,
            temp_dir_override: None,
        }
    }
}

pub type GlobalConfig = Arc<RwLock<Config>>;

/// Returns `true` if `path` equals `$HOME` or is an ancestor of `$HOME`
/// (e.g. `/home` or `/`). Used to prevent over-broad paths from becoming MCP
/// roots. Returns `false` when `$HOME` is unset.
#[cfg(unix)]
pub(super) fn path_is_home_or_ancestor(path: &Path) -> bool {
    let home_os = match std::env::var_os("HOME") {
        Some(h) => h,
        None => return false,
    };
    let home = std::fs::canonicalize(&home_os).unwrap_or_else(|_| PathBuf::from(&home_os));
    let candidate = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    home.starts_with(&candidate)
}

impl Config {
    /// Set remote agent metadata for NATS thin-client mode.
    pub fn set_remote_agent(&mut self, agent: String, cluster: String) {
        self.remote_agent = Some((agent, cluster));
    }

    /// Check if this config is running in remote-agent mode.
    pub fn is_remote_agent(&self) -> bool {
        self.remote_agent.is_some()
    }

    pub fn state(&self) -> StateFlags {
        let mut flags = StateFlags::empty();
        if let Some(session) = &self.session {
            if session.is_empty() {
                flags |= StateFlags::SESSION_EMPTY;
            } else {
                flags |= StateFlags::SESSION;
            }
            if session.agent_name().is_some() {
                flags |= StateFlags::AGENT;
            }
        }
        if self.agent.is_some() {
            flags |= StateFlags::AGENT;
        }
        if self.rag.is_some() {
            flags |= StateFlags::RAG;
        }
        flags
    }

    pub fn serve_addr(&self) -> String {
        self.serve_addr.clone().unwrap_or_else(|| SERVE_ADDR.into())
    }

    pub fn log_config(is_serve: bool) -> Result<(LevelFilter, Option<PathBuf>)> {
        let log_level = env::var(get_env_name("log_level"))
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| Self::default_log_level(is_serve));
        if log_level == LevelFilter::Off {
            return Ok((log_level, None));
        }
        Ok((log_level, Self::resolve_log_path(is_serve)))
    }

    /// Default log level when `log_level` is unset: `Debug` in debug builds,
    /// otherwise `Info` for serve mode and `Off` for interactive use.
    fn default_log_level(is_serve: bool) -> LevelFilter {
        if cfg!(debug_assertions) {
            LevelFilter::Debug
        } else if is_serve {
            LevelFilter::Info
        } else {
            LevelFilter::Off
        }
    }

    /// Resolve the log file path: an explicit `log_path` env value wins;
    /// otherwise default to the state-dir log file (or none in serve mode).
    fn resolve_log_path(is_serve: bool) -> Option<PathBuf> {
        if let Ok(v) = env::var(get_env_name("log_path")) {
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
        if is_serve {
            return None;
        }
        Some(paths::state_path(&format!(
            "{}.log",
            env!("CARGO_CRATE_NAME")
        )))
    }

    pub fn edit_config(&mut self) -> Result<()> {
        let config_path = Self::config_file();
        self.edit_with_tui_hooks(|this| {
            let editor = this.editor()?;
            edit_file(&editor, &config_path)
        })?;
        crate::utils::emit_info(format!(
            "NOTE: Remember to restart harnx if there are changes made.\nConfig files:\n  {}\n  {}/\n  {}/\n  {}/",
            config_path.display(),
            Self::clients_dir().display(),
            Self::mcp_servers_dir().display(),
            Self::acp_servers_dir().display(),
        ));
        Ok(())
    }

    pub fn current_model(&self) -> &Model {
        if let Some(session) = self.session.as_ref() {
            session.model()
        } else if let Some(agent) = self.agent.as_ref() {
            agent.model()
        } else {
            &self.model
        }
    }

    pub fn current_model_id(&self) -> Option<String> {
        let id = self.current_model().id();
        if id.is_empty() {
            None
        } else {
            Some(id)
        }
    }

    pub fn extract_agent(&self) -> Agent {
        // When an explicit agent is active, prefer it over the session-derived
        // agent. The in-memory agent has the full configuration from the agent
        // file (including retry settings, hooks, etc.) that may not be stored
        // in the session log.  The session-derived agent is used only when
        // loading a standalone session from disk with no agent in context.
        if let Some(agent) = self.agent.as_ref() {
            agent.clone()
        } else if let Some(session) = self.session.as_ref() {
            self::session::to_agent(session)
        } else {
            let mut agent = Agent::new(AgentConfig::from_prompt(""));
            agent.set_model(self.model.clone());
            agent.set_temperature(self.temperature);
            agent.set_top_p(self.top_p);
            agent.set_use_tools(self.use_tools.clone());
            agent
        }
    }

    /// Package of the currently active agent (e.g. `Some("pantheon")` for
    /// `pantheon/daedalus`, `None` for a top-level agent). Used to spell
    /// package-aware handoff tool declarations consistently across the engine
    /// allow-list, the tool list sent to the LLM, CLI listings, and shell
    /// completion (#709).
    pub fn active_package(&self) -> Option<String> {
        harnx_core::package_namespace::pkg_from_qualified(self.extract_agent().name())
            .map(str::to_string)
    }

    pub fn resolved_hooks(&self) -> HooksConfig {
        let global = self.hooks.clone().unwrap_or_default();
        if let Some(agent) = &self.agent {
            if let Some(agent_hooks) = agent.hooks() {
                return HooksConfig::merge(&global, agent_hooks);
            }
        }
        global
    }

    pub fn info(&self) -> Result<String> {
        if let Some(agent) = &self.agent {
            let output = agent.export()?;
            if let Some(session) = &self.session {
                let session = session
                    .export()?
                    .split('\n')
                    .map(|v| format!("  {v}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(format!("{output}session:\n{session}"))
            } else {
                Ok(output)
            }
        } else if let Some(session) = &self.session {
            session.export()
        } else if let Some(rag) = &self.rag {
            rag.export()
        } else {
            self.sysinfo()
        }
    }

    pub fn sysinfo(&self) -> Result<String> {
        let display_path = |path: &Path| path.display().to_string();
        let wrap = self
            .wrap
            .clone()
            .map_or_else(|| String::from("no"), |v| v.to_string());
        let (rag_reranker_model, rag_top_k) = match &self.rag {
            Some(rag) => rag.get_config(),
            None => (self.rag_reranker_model.clone(), self.rag_top_k),
        };
        let agent = self.extract_agent();
        let mut items = vec![
            ("model", agent.model().id()),
            ("temperature", format_option_value(&agent.temperature())),
            ("top_p", format_option_value(&agent.top_p())),
            (
                "use_tools",
                agent
                    .use_tools()
                    .map(|v| v.join(","))
                    .unwrap_or_else(|| "null".into()),
            ),
            (
                "max_output_tokens",
                agent
                    .model()
                    .max_tokens_param()
                    .map(|v| format!("{v} (current model)"))
                    .unwrap_or_else(|| "null".into()),
            ),
            ("save_session", format_option_value(&self.save_session)),
            ("compress_threshold", self.compress_threshold.to_string()),
            (
                "rag_reranker_model",
                format_option_value(&rag_reranker_model),
            ),
            ("rag_top_k", rag_top_k.to_string()),
            ("dry_run", self.dry_run.to_string()),
            ("tool_use", self.tool_use.to_string()),
            ("stream", self.stream.to_string()),
            ("save", self.save.to_string()),
            ("keybindings", self.keybindings.clone()),
            ("wrap", wrap),
            ("wrap_code", self.wrap_code.to_string()),
            ("highlight", self.highlight.to_string()),
            ("theme", format_option_value(&self.theme)),
            ("config_file", display_path(&Self::config_file())),
            ("env_file", display_path(&Self::env_file())),
            ("sessions_dir", display_path(&self.sessions_dir())),
            ("rags_dir", display_path(&Self::rags_dir())),
            ("macros_dir", display_path(&Self::macros_dir())),
            ("messages_file", display_path(&self.messages_file())),
        ];
        if let Some(hooks) = &self.hooks {
            items.push(("hooks", hooks.entries.len().to_string()));
        }
        if let Ok((_, Some(log_path))) =
            Self::log_config(self.working_mode.is_serve() || self.working_mode.is_acp())
        {
            items.push(("log_path", display_path(&log_path)));
        }
        let output = items
            .iter()
            .map(|(name, value)| format!("{name:<24}{value}\n"))
            .collect::<Vec<String>>()
            .join("");
        Ok(output)
    }

    pub fn delete(config: &GlobalConfig, kind: &str) -> Result<()> {
        let (dir, file_ext) = match kind {
            "agent" => (Self::agents_config_dir(), Some(".md")),
            "session" => (config.read().sessions_dir(), Some(".yaml")),
            "rag" => (Self::rags_dir(), Some(".yaml")),
            "macro" => (Self::macros_dir(), Some(".yaml")),
            "agent-data" => (Self::agents_data_dir(), None),
            _ => bail!("Unknown kind '{kind}'"),
        };

        let names = Self::deletable_names(&dir, file_ext);
        if names.is_empty() {
            bail!("No {kind} to delete")
        }

        let select_names = Self::prompt_delete_selection(kind, names)?;
        for name in select_names {
            Self::delete_entry(&dir, file_ext, kind, &name)?;
        }
        crate::utils::emit_info(format!("✓ Successfully deleted {kind}."));
        Ok(())
    }

    /// List the deletable item names in `dir`. With `Some(ext)`, returns file
    /// stems whose extension matches; with `None`, returns subdirectory names.
    fn deletable_names(dir: &Path, file_ext: Option<&str>) -> Vec<String> {
        let Ok(entries) = read_dir(dir) else {
            return vec![];
        };
        let mut names = vec![];
        for entry in entries.flatten() {
            let name = entry.file_name();
            match file_ext {
                Some(file_ext) => {
                    if let Some(stem) = name.to_string_lossy().strip_suffix(file_ext) {
                        names.push(stem.to_string());
                    }
                }
                None => {
                    if entry.path().is_dir() {
                        names.push(name.to_string_lossy().to_string());
                    }
                }
            }
        }
        names.sort_unstable();
        names
    }

    /// Prompt the user to multi-select which `kind` items to delete, requiring
    /// at least one selection.
    fn prompt_delete_selection(kind: &str, names: Vec<String>) -> Result<Vec<String>> {
        MultiSelect::new(&format!("Select {kind} to delete:"), names)
            .with_validator(|list: &[ListOption<&String>]| {
                if list.is_empty() {
                    Ok(Validation::Invalid(
                        "At least one item must be selected".into(),
                    ))
                } else {
                    Ok(Validation::Valid)
                }
            })
            .prompt()
            .map_err(Into::into)
    }

    /// Remove a single named entry — a file (`Some(ext)`) or a directory (`None`).
    fn delete_entry(dir: &Path, file_ext: Option<&str>, kind: &str, name: &str) -> Result<()> {
        let fail = |path: &Path| format!("Failed to delete {kind} at '{}'", path.display());
        match file_ext {
            Some(ext) => {
                let path = dir.join(format!("{name}{ext}"));
                if kind == "session" {
                    if let Err(err) = crate::config::attachments::remove_attachments_dir(&path) {
                        log::warn!("failed to remove attachments for session '{name}': {err}");
                    }
                }
                remove_file(&path).with_context(|| fail(&path))
            }
            None => {
                let path = dir.join(name);
                remove_dir_all(&path).with_context(|| fail(&path))
            }
        }
    }

    pub(super) fn edit_with_tui_hooks<T, F>(&mut self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Self) -> Result<T>,
    {
        if let Some(before) = self.tui_before_editor.as_mut() {
            before();
        }
        let result = f(self);
        if let Some(after) = self.tui_after_editor.as_mut() {
            after();
        }
        result
    }

    pub fn active_tool_names(&self) -> HashSet<String> {
        let agent = self.extract_agent();
        let use_tools = match agent.use_tools() {
            Some(v) => v,
            None => return HashSet::new(),
        };
        let use_tools_str = use_tools.join(",");
        // Generate handoff declarations relative to the active agent's package
        // so their names match the package-aware spelling exposed to the agent
        // and decoded by the engine (#709).
        let active_pkg =
            harnx_core::package_namespace::pkg_from_qualified(agent.name()).map(str::to_string);
        let (declarations, _) =
            self.tool_declarations_for_use_tools(Some(&use_tools_str), active_pkg.as_deref());
        let declaration_names: HashSet<String> =
            declarations.iter().map(|d| d.name.clone()).collect();
        let mut names = HashSet::new();
        for item in use_tools.iter().map(|s| s.trim()) {
            if let Some(values) = self.toolsets.get(item) {
                names.extend(
                    values
                        .iter()
                        .filter(|v| declaration_names.contains(v.as_str()))
                        .cloned(),
                );
            } else {
                names.extend(
                    declaration_names
                        .iter()
                        .filter(|n| matches_tool_glob(item, n))
                        .cloned(),
                );
            }
        }
        names
    }

    pub fn exit_agent_session(&mut self) -> Result<()> {
        self.exit_session()?;
        if let Some(agent) = self.agent.as_mut() {
            agent.exit_session();
            if self.working_mode.is_tui() {
                self.init_agent_shared_variables()?;
            }
        }
        Ok(())
    }

    pub fn select_tools(&self, agent: &AgentConfig) -> Option<Vec<ToolDeclaration>> {
        if !self.tool_use {
            return None;
        }
        let use_tools = agent.use_tools()?;
        let use_tools_str = use_tools.join(",");
        // Handoff declarations must be spelled relative to this agent's package
        // so the tool names sent to the LLM match what the engine can decode
        // and what `build_tool_eval_context` allow-lists (#709).
        let active_pkg =
            harnx_core::package_namespace::pkg_from_qualified(agent.name()).map(str::to_string);
        let (declarations, _) =
            self.tool_declarations_for_use_tools(Some(&use_tools_str), active_pkg.as_deref());
        let tool_names = self.collect_selected_tool_names(&use_tools, &declarations);

        let mut functions: Vec<ToolDeclaration> = declarations
            .iter()
            .filter(|v| tool_names.contains(&v.name))
            .cloned()
            .collect();
        self.merge_agent_owned_tools(&mut functions, &tool_names);

        if functions.is_empty() {
            None
        } else {
            Some(functions)
        }
    }

    /// Resolve the concrete set of tool names selected by `use_tools`,
    /// expanding toolset names and glob selectors against the available
    /// `declarations`.
    fn collect_selected_tool_names(
        &self,
        use_tools: &[String],
        declarations: &[ToolDeclaration],
    ) -> HashSet<String> {
        let declaration_names: HashSet<String> =
            declarations.iter().map(|v| v.name.to_string()).collect();
        let mut tool_names: HashSet<String> = HashSet::new();
        for item in use_tools.iter().map(|s| s.trim()) {
            if let Some(values) = self.toolsets.get(item) {
                tool_names.extend(
                    values
                        .iter()
                        .filter(|v| declaration_names.contains(v.as_str()))
                        .cloned(),
                );
            } else {
                tool_names.extend(
                    declaration_names
                        .iter()
                        .filter(|name| matches_tool_glob(item, name))
                        .cloned(),
                );
            }
        }
        tool_names
    }

    /// Merge in any agent-owned tool declarations (e.g. handoff tools, builtins)
    /// that are permitted by `tool_names` but not already present in `functions`.
    /// The `tool_names` whitelist ensures `agent.tools()` cannot smuggle in tools
    /// that `use_tools` did not request.
    fn merge_agent_owned_tools(
        &self,
        functions: &mut Vec<ToolDeclaration>,
        tool_names: &HashSet<String>,
    ) {
        let Some(active_agent) = &self.agent else {
            return;
        };
        let existing_names: HashSet<String> =
            functions.iter().map(|v| v.name.to_string()).collect();
        functions.extend(
            active_agent
                .tools()
                .declarations()
                .into_iter()
                .filter(|v| tool_names.contains(&v.name) && !existing_names.contains(&v.name)),
        );
    }

    pub fn editor(&self) -> Result<String> {
        EDITOR.get_or_init(move || {
            let editor = self.editor.clone()
                .or_else(|| env::var("VISUAL").ok().or_else(|| env::var("EDITOR").ok()))
                .unwrap_or_else(|| {
                    if cfg!(windows) {
                        "notepad".to_string()
                    } else {
                        "nano".to_string()
                    }
                });
            which::which(&editor).ok().map(|_| editor)
        })
        .clone()
        .ok_or_else(|| anyhow!("Editor not found. Please add the `editor` configuration or set the $EDITOR or $VISUAL environment variable."))
    }

    pub fn sync_models_url(&self) -> String {
        self.sync_models_url
            .clone()
            .unwrap_or_else(|| SYNC_MODELS_URL.into())
    }

    pub async fn sync_models(url: &str, abort_signal: AbortSignal) -> Result<()> {
        let content = abortable_run_with_spinner(fetch(url), "Fetching models.yaml", abort_signal)
            .await
            .with_context(|| format!("Failed to fetch '{url}'"))?;
        crate::utils::emit_info(format!("✓ Fetched '{url}'"));
        let list = serde_yaml::from_str::<Vec<ProviderModels>>(&content)
            .with_context(|| "Failed to parse models.yaml")?;
        let models_override = ModelsOverride {
            version: env!("CARGO_PKG_VERSION").to_string(),
            list,
        };
        let models_override_data =
            serde_yaml::to_string(&models_override).with_context(|| "Failed to serde {}")?;

        let model_override_path = Self::models_override_file();
        ensure_parent_exists(&model_override_path)?;
        std::fs::write(&model_override_path, models_override_data)
            .with_context(|| format!("Failed to write to '{}'", model_override_path.display()))?;
        crate::utils::emit_info(format!("✓ Updated '{}'", model_override_path.display()));
        Ok(())
    }

    pub fn loal_models_override() -> Result<Vec<ProviderModels>> {
        let model_override_path = Self::models_override_file();
        let err = || {
            format!(
                "Failed to load models at '{}'",
                model_override_path.display()
            )
        };
        let content = read_to_string(&model_override_path).with_context(err)?;
        let models_override: ModelsOverride = serde_yaml::from_str(&content).with_context(err)?;
        if models_override.version != env!("CARGO_PKG_VERSION") {
            bail!("Incompatible version")
        }
        Ok(models_override.list)
    }

    pub fn light_theme(&self) -> bool {
        matches!(self.theme.as_deref(), Some("light"))
    }

    pub fn render_options(&self) -> Result<RenderOptions> {
        let theme = if self.highlight {
            let theme_mode = if self.light_theme() { "light" } else { "dark" };
            let theme_filename = format!("{theme_mode}.tmTheme");
            let theme_path = Self::local_path(&theme_filename);
            if theme_path.exists() {
                let theme = ThemeSet::get_theme(&theme_path)
                    .with_context(|| format!("Invalid theme at '{}'", theme_path.display()))?;
                Some(theme)
            } else {
                Some(harnx_render::load_builtin_theme(self.light_theme())?)
            }
        } else {
            None
        };
        let wrap = if *IS_STDOUT_TERMINAL {
            self.wrap.clone()
        } else {
            None
        };
        let truecolor = matches!(
            env::var("COLORTERM").as_ref().map(|v| v.as_str()),
            Ok("truecolor")
        );
        Ok(RenderOptions::new(theme, wrap, self.wrap_code, truecolor))
    }

    /// Render a status line showing agent name and session ID.
    ///
    /// When `use_icons` is true, an appropriate icon leads the line:
    /// - `🤖 <agent> ▸ <session>` when an agent is active
    /// - `💬 <session>` when only a session is active (no robot icon)
    ///
    /// When `use_icons` is false, icons are omitted (used for spinner where
    /// the braille animation frame serves as the leading character).
    pub fn render_status_line(&self, use_icons: bool) -> String {
        let agent_name = if let Some(agent) = &self.agent {
            Some(agent.name().to_string())
        } else {
            let agent = self.extract_agent();
            if agent.name() != TEMP_AGENT_NAME {
                Some(agent.name().to_string())
            } else {
                None
            }
        };
        let session_name = self.session.as_ref().map(|s| s.id().to_string());
        let model_id = self.current_model_id();

        match (agent_name, model_id, session_name, use_icons) {
            (Some(agent), Some(model), Some(session), true) => {
                format!("🤖 {} ▸ {} ▸ {}", agent, model, session)
            }
            (Some(agent), Some(model), Some(session), false) => {
                format!("{} ▸ {} ▸ {}", agent, model, session)
            }
            (Some(agent), Some(model), None, true) => format!("🤖 {} ▸ {}", agent, model),
            (Some(agent), Some(model), None, false) => format!("{} ▸ {}", agent, model),
            (Some(agent), None, Some(session), true) => format!("🤖 {} ▸ {}", agent, session),
            (Some(agent), None, Some(session), false) => format!("{} ▸ {}", agent, session),
            (Some(agent), None, None, true) => format!("🤖 {}", agent),
            (Some(agent), None, None, false) => agent,
            (None, _, Some(session), true) => format!("💬 {}", session),
            (None, _, Some(session), false) => session,
            (None, _, None, _) => String::new(),
        }
    }

    pub fn print_markdown(&self, text: &str) -> Result<()> {
        let rendered = if *IS_STDOUT_TERMINAL {
            let render_options = self.render_options()?;
            let mut markdown_render = MarkdownRender::init(render_options)?;
            markdown_render.render(text)
        } else {
            text.to_string()
        };
        crate::utils::emit_info(rendered);
        Ok(())
    }

    pub fn before_chat_completion(&mut self, input: &Input) -> Result<()> {
        self.last_message = Some(LastMessage::new(input.clone(), String::new()));
        Ok(())
    }

    /// Record token usage without saving any new message — the
    /// round's transcript entries are being written separately by the
    /// split [`save_session_tool_calls`] / [`save_session_tool_results`]
    /// pair.  Callers use this to keep `completion_usage` current on
    /// the session while driving the two-phase save directly.
    pub fn record_completion_usage(&mut self, usage: &crate::client::CompletionTokenUsage) {
        if let Some(session) = &mut self.session {
            session.add_completion_usage(usage);
        }
    }

    /// Finalize the tool round opened by [`save_session_tool_calls`].
    /// Writes a `ToolResults` entry to the session log and fills in
    /// the pending outputs on the last in-memory message.
    pub fn save_session_tool_results(&mut self, results: &[ToolResult]) -> Result<()> {
        let sessions_dir = self.sessions_dir();
        let Some(session) = self.session.as_mut() else {
            return Ok(());
        };
        session.set_sessions_dir(sessions_dir);
        crate::config::session::add_tool_results(session, results)
    }

    fn discontinuous_last_message(&mut self) {
        if let Some(last_message) = self.last_message.as_mut() {
            last_message.continuous = false;
        }
    }

    fn session_for_save<'a>(&'a mut self, request: &SessionSaveRequest) -> Option<&'a mut Session> {
        if !request.input.with_session() {
            return None;
        }

        let sessions_dir = self.sessions_dir();
        let session = self.session.as_mut()?;
        session.set_sessions_dir(sessions_dir);
        Some(session)
    }

    fn save_message_with_tool_results(
        session: &mut Session,
        request: &SessionSaveRequest,
        tool_results: &[crate::tool::ToolResult],
    ) -> Result<()> {
        let calls = collect_tool_calls(tool_results);
        crate::config::session::add_tool_calls(
            session,
            &request.input,
            request.output,
            request.thought,
            &calls,
        )?;
        crate::config::session::add_tool_results(session, tool_results)
    }

    fn init_agent_shared_variables(&mut self) -> Result<()> {
        let agent = match self.agent.as_mut() {
            Some(v) => v,
            None => return Ok(()),
        };
        if !agent.defined_variables().is_empty() && agent.shared_variables().is_empty() {
            let mut config_variables = AgentVariables::default();
            if let Some(v) = &self.agent_variables {
                config_variables.extend(v.clone());
            }
            let new_variables = self::agent::init_agent_variables(
                agent.defined_variables(),
                &config_variables,
                self.info_flag,
            )?;
            agent.set_shared_variables(new_variables);
        }
        Ok(())
    }

    fn init_agent_session_variables(&mut self, new_session: bool) -> Result<()> {
        let (agent, session) = match (self.agent.as_mut(), self.session.as_mut()) {
            (Some(agent), Some(session)) => (agent, session),
            _ => return Ok(()),
        };
        if new_session {
            let shared_variables = agent.shared_variables().clone();
            let session_variables =
                if !agent.defined_variables().is_empty() && shared_variables.is_empty() {
                    let mut config_variables = AgentVariables::default();
                    if let Some(v) = &self.agent_variables {
                        config_variables.extend(v.clone());
                    }
                    let new_variables = self::agent::init_agent_variables(
                        agent.defined_variables(),
                        &config_variables,
                        self.info_flag,
                    )?;
                    agent.set_shared_variables(new_variables.clone());
                    new_variables
                } else {
                    shared_variables
                };
            agent.set_session_variables(session_variables);
            session.sync_agent(agent)?;
        } else {
            let variables = session.agent_variables();
            agent.set_session_variables(variables.clone());
        }
        Ok(())
    }

    pub fn expand_use_tools(
        &self,
        use_tools: Option<&[String]>,
        active_pkg: Option<&str>,
    ) -> Vec<String> {
        // Handle None or empty selectors → empty list (no tools)
        let use_tools = match use_tools {
            Some(selectors) if !selectors.is_empty() => selectors,
            _ => return Vec::new(),
        };

        let selectors_str = use_tools.join(",");
        let expanded_selectors = split_tool_selectors(&selectors_str)
            .into_iter()
            .flat_map(|selector| {
                let selector = selector.trim();
                self.toolsets
                    .get(selector)
                    .cloned()
                    .unwrap_or_else(|| vec![selector.to_string()])
            })
            .collect::<Vec<String>>();

        // Wildcard "*" → all tools (use tool_declarations_for_use_tools unchanged)
        if expanded_selectors.iter().any(|s| s.trim() == "*") {
            let (declarations, _) =
                self.tool_declarations_for_use_tools(Some(&selectors_str), active_pkg);
            return declarations
                .into_iter()
                .map(|declaration| declaration.name)
                .collect();
        }

        // Explicit selectors → filter declarations to matched names only
        let selected_names: HashSet<&str> = expanded_selectors
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s as &str)
            .collect();

        let (declarations, _) =
            self.tool_declarations_for_use_tools(Some(&selectors_str), active_pkg);

        // Return only tools whose names are in the selected set (plus runtime-injected tools)
        declarations
            .into_iter()
            .filter(|declaration| {
                // Accept if explicitly selected by name
                selected_names.contains(declaration.name.as_str()) ||
                // Accept runtime-injected tools (MCP/ACP tools not in original builtin list)
                declaration.mcp_server_name.is_some() ||
                declaration.mcp_tool_name.is_some()
            })
            .map(|declaration| declaration.name)
            .collect()
    }

    pub fn tool_declarations_for_use_tools(
        &self,
        use_tools: Option<&str>,
        active_pkg: Option<&str>,
    ) -> (Vec<ToolDeclaration>, HashMap<String, String>) {
        let mut declarations = self.tools.declarations();
        let mut handoff_targets = HashMap::new();
        if let Some(use_tools) = use_tools {
            let selectors = split_tool_selectors(use_tools)
                .into_iter()
                .flat_map(|selector| {
                    let selector = selector.trim();
                    self.toolsets
                        .get(selector)
                        .cloned()
                        .unwrap_or_else(|| vec![selector.to_string()])
                })
                .collect::<Vec<String>>();
            if self.needs_mcp_tools() {
                if let Some(manager) = &self.mcp_manager {
                    if selectors.iter().any(|selector| selector == "*") {
                        declarations.extend(manager.get_all_tools_blocking());
                    } else {
                        declarations.extend(manager.get_tools_for_selectors_blocking(&selectors));
                    }
                }
            }
            if let Some(manager) = &self.acp_manager {
                declarations.extend(manager.get_all_tools_blocking());
            }
            // Only generate handoff tool declarations when the agent's use_tools
            // actually requests a *_session_handoff tool. Generating them
            // unconditionally would inject extra tool declarations into agents
            // that don't need them, changing LLM request payloads (#303).
            //
            // Use the toolset-expanded `selectors` (not the raw `use_tools`)
            // so a handoff selector or `*` reached via a toolset is honored,
            // matching how MCP tools are selected above.
            if selectors.iter().any(|v| {
                let v = v.trim();
                v.ends_with("_session_handoff") || v == "*"
            }) {
                let (handoff_declarations, targets) =
                    handoff_tool_declarations_for_agents(active_pkg);
                declarations.extend(handoff_declarations);
                handoff_targets.extend(targets);
            }
            if selectors.iter().any(|v| {
                let v = v.trim();
                v == crate::session_history::TOOL_NAME || v == "*"
            }) {
                declarations.push(crate::session_history::tool_declaration());
            }
        }

        let mut seen = HashSet::new();
        declarations.retain(|declaration| seen.insert(declaration.name.clone()));
        (declarations, handoff_targets)
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct StateFlags: u32 {
        const SESSION_EMPTY = 1 << 0;
        const SESSION = 1 << 1;
        const RAG = 1 << 2;
        const AGENT = 1 << 3;
    }
}

pub(crate) async fn ensure_parent_exists_async(path: &Path) -> Result<()> {
    if tokio::fs::metadata(path).await.is_ok() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Failed to write to '{}', No parent path", path.display()))?;
    if tokio::fs::metadata(parent).await.is_err() {
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!(
                "Failed to write to '{}', Cannot create parent directory",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn read_env_value<T>(key: &str) -> Option<Option<T>>
where
    T: std::str::FromStr,
{
    let value = env::var(key).ok()?;
    let value = parse_value(&value).ok()?;
    Some(value)
}

fn parse_value<T>(value: &str) -> Result<Option<T>>
where
    T: std::str::FromStr,
{
    let value = if value == "null" {
        None
    } else {
        let value = match value.parse() {
            Ok(value) => value,
            Err(_) => bail!("Invalid value '{}'", value),
        };
        Some(value)
    };
    Ok(value)
}

fn read_env_bool(key: &str) -> Option<Option<bool>> {
    let value = env::var(key).ok()?;
    Some(parse_bool(&value))
}

fn map_completion_values<T: ToString>(value: Vec<T>) -> Vec<(String, Option<String>)> {
    value.into_iter().map(|v| (v.to_string(), None)).collect()
}

pub(super) fn update_rag<F>(config: &GlobalConfig, f: F) -> Result<()>
where
    F: FnOnce(&mut Rag) -> Result<()>,
{
    let mut rag = match config.read().rag.clone() {
        Some(v) => v.as_ref().clone(),
        None => bail!("No RAG"),
    };
    f(&mut rag)?;
    config.write().rag = Some(Arc::new(rag));
    Ok(())
}

fn format_option_value<T>(value: &Option<T>) -> String
where
    T: std::fmt::Display,
{
    match value {
        Some(value) => value.to_string(),
        None => "null".to_string(),
    }
}

#[cfg(test)]
mod compaction_tests;
#[cfg(test)]
mod session_edit_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_extra;
