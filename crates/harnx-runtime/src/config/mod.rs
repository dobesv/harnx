pub mod agent;
pub mod input;
pub mod session;
pub mod session_meta;

pub use self::agent::{complete_agent_variables, list_agents, Agent, AgentConfig, AgentVariables};
pub use self::agent::{CREATE_TITLE_AGENT, TEMP_AGENT_NAME};
pub use self::input::Input;
pub use self::session_meta::{
    build_picker_context, parse_session_meta, sort_sessions_for_picker, PickerContext,
    SessionMeta,
};
use self::session::Session;
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
    create_client_config, list_client_types, list_models, ClientConfig, MessageContentToolCalls,
    Model, ModelType, ProviderModels, OPENAI_COMPATIBLE_PROVIDERS,
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
    fs::{read_dir, read_to_string, remove_dir_all, remove_file, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process,
    sync::{Arc, OnceLock},
};
use syntect::highlighting::ThemeSet;
use terminal_colorsaurus::{theme_mode, QueryOptions, ThemeMode};
use uuid::Uuid;

pub use harnx_rag::TEMP_RAG_NAME;
pub const TEMP_SESSION_NAME: &str = "temp";

const SERVE_ADDR: &str = "127.0.0.1:8000";

const SYNC_MODELS_URL: &str =
    "https://raw.githubusercontent.com/dobesv/harnx/refs/heads/main/models.yaml";

const DEFAULT_COMPACT_PROMPT: &str =
    "Summarize the discussion briefly in 200 words or less to use as a prompt for future context.";

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

fn split_session_log_documents(raw_log: &str) -> Vec<String> {
    raw_log
        .split("\n---\n")
        .filter_map(|document| {
            let document = document.trim();
            let document = document.strip_prefix("---\n").unwrap_or(document).trim();
            if document.is_empty() {
                None
            } else {
                Some(document.to_string())
            }
        })
        .collect()
}

fn validate_edited_session_documents(content: &str) -> Result<Vec<String>> {
    let documents = split_session_log_documents(content);
    for document in &documents {
        serde_yaml::from_str::<SessionLogEntry>(document).with_context(|| {
            format!(
                "Invalid session log entry YAML:
{document}"
            )
        })?;
    }
    Ok(documents)
}

/// Adjust `[from, to]` so that `ToolCalls`/`ToolResults` pairs are never
/// split across the range boundary, then return the (possibly expanded) range.
///
/// Rules:
/// - If `from` points at a `ToolResults` entry (i.e. the pair's `ToolCalls` is
///   at `from - 1`, outside the range), that is an error: we can't silently
///   expand backward because the caller's intent is unclear.
/// - If `to` points at a `ToolCalls` entry and `to + 1` is its paired
///   `ToolResults`, auto-expand `to` by one.
///
/// Returns `(adjusted_from, adjusted_to)`.
fn adjust_range_for_tool_pairs(
    from: usize,
    to: usize,
    documents: &[String],
) -> Result<(usize, usize)> {
    // Parse only the entries we need: the one just before `from` (to check if
    // `from` is a dangling ToolResults) and up through `to + 1` (to check if
    // `to` is a ToolCalls that needs its partner).
    let parse = |idx: usize| -> Option<SessionLogEntry> {
        documents
            .get(idx)
            .and_then(|raw| serde_yaml::from_str::<SessionLogEntry>(raw).ok())
    };

    // Reject: range starts on a ToolResults whose ToolCalls is outside the range.
    if matches!(parse(from), Some(SessionLogEntry::ToolResults { .. })) {
        // Check if the immediately preceding entry is a ToolCalls — if so,
        // this is definitely a dangling-results situation.
        bail!(
            "Sequence {from} is a tool-results entry; its paired tool-calls entry ({}) \
             would be outside the range. Expand your range to include it.",
            from.saturating_sub(1)
        );
    }

    // Auto-expand: range ends on a ToolCalls whose ToolResults is just outside.
    let mut adjusted_to = to;
    if matches!(parse(to), Some(SessionLogEntry::ToolCalls { .. }))
        && to + 1 < documents.len()
        && matches!(parse(to + 1), Some(SessionLogEntry::ToolResults { .. }))
    {
        adjusted_to = to + 1;
    }

    Ok((from, adjusted_to))
}

fn validate_tool_pair_integrity(start_seq: usize, documents: &[String]) -> Result<()> {
    let entries = documents
        .iter()
        .map(|document| {
            serde_yaml::from_str::<SessionLogEntry>(document).with_context(|| {
                format!(
                    "Invalid session log entry YAML:
{document}"
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;

    for (index, entry) in entries.iter().enumerate() {
        let SessionLogEntry::ToolCalls { calls, .. } = entry else {
            continue;
        };

        let Some(SessionLogEntry::ToolResults { results, .. }) = entries.get(index + 1) else {
            let call_seq = start_seq + index;
            bail!(
                "Edited tool call entry at {call_seq} must be followed immediately by matching tool results"
            );
        };

        let call_ids: HashSet<_> = calls.iter().filter_map(|call| call.id.as_deref()).collect();
        let result_seq = start_seq + index + 1;
        let missing_result_ids = results
            .iter()
            .filter(|result| result.id.as_deref().is_none_or(str::is_empty))
            .count();

        if missing_result_ids == results.len() {
            if results.len() != calls.len() {
                bail!(
                    "Edited tool result at {result_seq} is missing tool_call_id for positional matching and count {} does not match tool calls count {}",
                    results.len(),
                    calls.len()
                );
            }
            continue;
        }

        if missing_result_ids > 0 {
            bail!(
                "Edited tool result at {result_seq} mixes tool_call_id values with missing tool_call_id entries"
            );
        }

        // All results have IDs: enforce strict 1:1 mapping.
        // Collect in order so duplicate detection and count check work together.
        let result_ids: Vec<&str> = results
            .iter()
            .map(|r| r.id.as_deref().filter(|id| !id.is_empty()).unwrap())
            .collect();

        if result_ids.len() != calls.len() {
            let expected_ids = call_ids.iter().copied().collect::<Vec<_>>().join(", ");
            bail!(
                "Edited tool result at {result_seq} has {} result(s) but {} call(s) (expected ids: {expected_ids})",
                result_ids.len(),
                calls.len()
            );
        }

        let result_id_set: HashSet<&str> = result_ids.iter().copied().collect();
        if result_id_set.len() != result_ids.len() {
            bail!("Edited tool result at {result_seq} contains duplicate tool_call_id values");
        }

        for call_id in &result_ids {
            if !call_ids.contains(call_id) {
                let expected_ids = call_ids.iter().copied().collect::<Vec<_>>().join(", ");
                bail!(
                    "Edited tool result at {result_seq} references unknown tool_call_id '{call_id}' (expected one of: {expected_ids})"
                );
            }
        }
    }

    Ok(())
}

fn parse_toolsets_json(value: &str) -> serde_json::Result<IndexMap<String, Vec<String>>> {
    let values = serde_json::from_str::<IndexMap<String, ToolsetValue>>(value)?;
    Ok(values
        .into_iter()
        .map(|(key, value)| (key, normalize_toolset_value(value)))
        .collect())
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

fn handoff_tool_declarations_for_agents() -> Vec<ToolDeclaration> {
    crate::config::agent::list_agents()
        .into_iter()
        .map(|agent_name| {
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
                        "Optional target session ID. If provided, the handoff reuses that session; if omitted, the current interactive session is reused.".to_string(),
                    ),
                    ..Default::default()
                },
            );
            ToolDeclaration {
                name: format!("{agent_name}_session_handoff"),
                description: format!(
                    "Exit the current interactive agent session and hand off to the '{agent_name}' agent. Resolves the target session internally (reusing session_id when provided, otherwise the current session), then continues interaction in that agent session with the supplied prompt."
                ),
                parameters: crate::tool::JsonSchema {
                    type_value: Some("object".to_string()),
                    properties: Some(properties),
                    required: Some(vec!["prompt".to_string()]),
                    ..Default::default()
                },
                mcp_tool_name: None,
                call_template: None,
                result_template: None,
            }
        })
        .collect()
}

pub struct Config {
    pub data: ConfigData,

    // Server-config vectors (types live in dependent crates — stay here,
    // not in ConfigData, to avoid reverse deps from harnx-core).
    pub clients: Vec<ClientConfig>,
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
    pub tui_before_editor: Option<Box<dyn FnMut() + Send + Sync>>,
    pub tui_after_editor: Option<Box<dyn FnMut() + Send + Sync>>,

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
            tui_before_editor: None,
            tui_after_editor: None,
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
            mcp_servers: vec![],
            acp_servers: vec![],

            model_cooldowns: std::sync::Arc::new(parking_lot::Mutex::new(Default::default())),
            macro_flag: false,
            info_flag: false,
            show_sequence_numbers: false,
            show_timestamps: false,
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
            tui_before_editor: None,
            tui_after_editor: None,
            sessions_dir_override: None,
            temp_dir_override: None,
        }
    }
}

pub type GlobalConfig = Arc<RwLock<Config>>;

impl Config {
    pub async fn init(
        working_mode: WorkingMode,
        info_flag: bool,
        mut mcp_root: Vec<String>,
    ) -> Result<Self> {
        // Install any user-supplied models-override list before the
        // harnx-client `ALL_PROVIDER_MODELS` lazy-lock is first accessed.
        crate::client::install_models_override();

        let config_path = Self::config_file();
        let mut config = if !config_path.exists() {
            match env::var(get_env_name("provider"))
                .ok()
                .or_else(|| env::var(get_env_name("platform")).ok())
            {
                Some(v) => Self::load_dynamic(&v)?,
                None => {
                    if *IS_STDOUT_TERMINAL {
                        create_config_file(&config_path).await?;
                    }
                    Self::load_from_file(&config_path)?
                }
            }
        } else {
            Self::load_from_file(&config_path)?
        };

        if let Ok(v) = env::var("HARNX_MCP_ROOTS") {
            for root in v.split(',') {
                let root = root.trim();
                if !root.is_empty() && !mcp_root.contains(&root.to_string()) {
                    mcp_root.push(root.to_string());
                }
            }
        }

        config.working_mode = working_mode;
        config.info_flag = info_flag;
        config.mcp_root = mcp_root;

        let setup = |config: &mut Self| -> Result<()> {
            config.load_envs();

            if let Some(wrap) = config.wrap.clone() {
                config.set_wrap(&wrap)?;
            }

            config.init_mcp_manager();
            config.init_acp_manager();
            config.tools = Tools::init_from_mcp(None);

            config.setup_model()?;
            config.setup_document_loaders();
            config.setup_user_agent();
            Ok(())
        };
        let ret = setup(&mut config);
        if !info_flag {
            ret?;
        }
        Ok(config)
    }

    pub fn config_dir() -> PathBuf {
        paths::config_dir()
    }

    pub fn local_path(name: &str) -> PathBuf {
        paths::local_path(name)
    }

    pub fn config_file() -> PathBuf {
        paths::config_file()
    }

    pub fn macros_dir() -> PathBuf {
        paths::macros_dir()
    }

    pub fn clients_dir() -> PathBuf {
        paths::clients_dir()
    }

    pub fn mcp_servers_dir() -> PathBuf {
        paths::mcp_servers_dir()
    }

    pub fn acp_servers_dir() -> PathBuf {
        paths::acp_servers_dir()
    }

    pub fn macro_file(name: &str) -> PathBuf {
        paths::macro_file(name)
    }

    pub fn env_file() -> PathBuf {
        paths::env_file()
    }

    pub fn messages_file(&self) -> PathBuf {
        paths::messages_file(self.agent.as_ref().map(|a| a.name()))
    }

    pub fn sessions_dir(&self) -> PathBuf {
        if let Some(ref override_dir) = self.sessions_dir_override {
            return override_dir.clone();
        }
        paths::sessions_dir(self.agent.as_ref().map(|a| a.name()))
    }

    pub fn rags_dir() -> PathBuf {
        paths::rags_dir()
    }

    pub fn session_file(&self, name: &str) -> PathBuf {
        match name.split_once('/') {
            Some((sub, leaf)) => self.sessions_dir().join(sub).join(format!("{leaf}.yaml")),
            None => self.sessions_dir().join(format!("{name}.yaml")),
        }
    }

    pub fn rag_file(&self, name: &str) -> PathBuf {
        paths::rag_file(self.agent.as_ref().map(|a| a.name()), name)
    }

    pub fn agents_data_dir() -> PathBuf {
        paths::agents_data_dir()
    }

    pub fn agent_data_dir(name: &str) -> PathBuf {
        paths::agent_data_dir(name)
    }

    pub fn agent_rag_file(agent_name: &str, rag_name: &str) -> PathBuf {
        paths::agent_rag_file(agent_name, rag_name)
    }

    pub fn agent_file(name: &str) -> PathBuf {
        paths::agent_file(name)
    }

    pub fn models_override_file() -> PathBuf {
        paths::models_override_file()
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
            .unwrap_or(match cfg!(debug_assertions) {
                true => LevelFilter::Debug,
                false => {
                    if is_serve {
                        LevelFilter::Info
                    } else {
                        LevelFilter::Off
                    }
                }
            });
        if log_level == LevelFilter::Off {
            return Ok((log_level, None));
        }
        let log_path = match env::var(get_env_name("log_path")) {
            Ok(v) => Some(PathBuf::from(v)),
            Err(_) => match is_serve {
                true => None,
                false => Some(Config::local_path(&format!(
                    "{}.log",
                    env!("CARGO_CRATE_NAME")
                ))),
            },
        };
        Ok((log_level, log_path))
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

    pub fn extract_agent(&self) -> Agent {
        if let Some(session) = self.session.as_ref() {
            self::session::to_agent(session)
        } else if let Some(agent) = self.agent.as_ref() {
            agent.clone()
        } else {
            let mut agent = Agent::new(AgentConfig::from_prompt(""));
            agent.set_model(self.model.clone());
            agent.set_temperature(self.temperature);
            agent.set_top_p(self.top_p);
            agent.set_use_tools(self.use_tools.clone());
            agent
        }
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

    pub fn update(config: &GlobalConfig, data: &str) -> Result<()> {
        let parts: Vec<&str> = data.split_whitespace().collect();
        if parts.len() != 2 {
            bail!("Usage: .set <key> <value>. If value is null, unset key.");
        }
        let key = parts[0];
        let value = parts[1];
        match key {
            "temperature" => {
                let value = parse_value(value)?;
                config.write().set_temperature(value);
            }
            "top_p" => {
                let value = parse_value(value)?;
                config.write().set_top_p(value);
            }
            "use_tools" => {
                let value: Option<Vec<String>> = if value == "null" {
                    None
                } else {
                    Some(
                        split_tool_selectors(value)
                            .into_iter()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .collect(),
                    )
                };
                config.write().set_use_tools(value);
            }
            "model_fallbacks" => {
                let value: Vec<String> = if value == "null" {
                    vec![]
                } else {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                };
                config.write().set_model_fallbacks(value);
            }
            "compaction_agent" => {
                let value = parse_value(value)?;
                config.write().set_compaction_agent(value);
            }
            "max_output_tokens" => {
                let value = parse_value(value)?;
                config.write().set_max_output_tokens(value);
            }
            "save_session" => {
                let value = parse_value(value)?;
                config.write().set_save_session(value);
            }
            "compress_threshold" => {
                let value = parse_value(value)?;
                config.write().set_compress_threshold(value);
            }
            "rag_reranker_model" => {
                let value = parse_value(value)?;
                Self::set_rag_reranker_model(config, value)?;
            }
            "rag_top_k" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                Self::set_rag_top_k(config, value)?;
            }
            "dry_run" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                config.write().dry_run = value;
            }
            "show_sequence_numbers" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                config.write().show_sequence_numbers = value;
            }
            "show_timestamps" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                config.write().show_timestamps = value;
            }
            "tool_use" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                if value
                    && config
                        .read()
                        .tool_declarations_for_use_tools(Some("*"))
                        .is_empty()
                {
                    bail!("Tool use cannot be enabled because no tools are installed.")
                }
                config.write().tool_use = value;
            }
            "stream" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                config.write().stream = value;
            }
            "save" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                config.write().save = value;
            }
            "highlight" => {
                let value = value.parse().with_context(|| "Invalid value")?;
                config.write().highlight = value;
            }
            _ => bail!("Unknown key '{key}'"),
        }
        Ok(())
    }

    pub fn delete(config: &GlobalConfig, kind: &str) -> Result<()> {
        let (dir, file_ext) = match kind {
            "agent" => (Self::agents_data_dir(), Some(".md")),
            "session" => (config.read().sessions_dir(), Some(".yaml")),
            "rag" => (Self::rags_dir(), Some(".yaml")),
            "macro" => (Self::macros_dir(), Some(".yaml")),
            "agent-data" => (Self::agents_data_dir(), None),
            _ => bail!("Unknown kind '{kind}'"),
        };
        let names = match read_dir(&dir) {
            Ok(rd) => {
                let mut names = vec![];
                for entry in rd.flatten() {
                    let name = entry.file_name();
                    match file_ext {
                        Some(file_ext) => {
                            if let Some(name) = name.to_string_lossy().strip_suffix(file_ext) {
                                names.push(name.to_string());
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
            Err(_) => vec![],
        };

        if names.is_empty() {
            bail!("No {kind} to delete")
        }

        let select_names = MultiSelect::new(&format!("Select {kind} to delete:"), names)
            .with_validator(|list: &[ListOption<&String>]| {
                if list.is_empty() {
                    Ok(Validation::Invalid(
                        "At least one item must be selected".into(),
                    ))
                } else {
                    Ok(Validation::Valid)
                }
            })
            .prompt()?;

        for name in select_names {
            match file_ext {
                Some(ext) => {
                    let path = dir.join(format!("{name}{ext}"));
                    remove_file(&path).with_context(|| {
                        format!("Failed to delete {kind} at '{}'", path.display())
                    })?;
                }
                None => {
                    let path = dir.join(name);
                    remove_dir_all(&path).with_context(|| {
                        format!("Failed to delete {kind} at '{}'", path.display())
                    })?;
                }
            }
        }
        crate::utils::emit_info(format!("✓ Successfully deleted {kind}."));
        Ok(())
    }

    pub fn set_temperature(&mut self, value: Option<f64>) {
        if let Some(session) = self.session.as_mut() {
            session.set_temperature(value);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_temperature(value);
        } else {
            self.temperature = value;
        }
    }

    pub fn set_top_p(&mut self, value: Option<f64>) {
        if let Some(session) = self.session.as_mut() {
            session.set_top_p(value);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_top_p(value);
        } else {
            self.top_p = value;
        }
    }

    pub fn set_use_tools(&mut self, value: Option<Vec<String>>) {
        if let Some(session) = self.session.as_mut() {
            session.set_use_tools(value);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_use_tools(value);
        } else {
            self.use_tools = value;
        }
    }

    pub fn set_model_fallbacks(&mut self, value: Vec<String>) {
        if let Some(session) = self.session.as_mut() {
            session.set_model_fallbacks(value);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_model_fallbacks(value);
        }
    }

    pub fn set_compaction_agent(&mut self, value: Option<String>) {
        if let Some(session) = self.session.as_mut() {
            session.set_compaction_agent(value);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_compaction_agent(value);
        }
    }

    pub fn active_tool_names(&self) -> HashSet<String> {
        let agent = self.extract_agent();
        let use_tools = match agent.use_tools() {
            Some(v) => v,
            None => return HashSet::new(),
        };
        let use_tools_str = use_tools.join(",");
        let declarations = self.tool_declarations_for_use_tools(Some(&use_tools_str));
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

    pub fn set_save_session(&mut self, value: Option<bool>) {
        if let Some(session) = self.session.as_mut() {
            session.set_save_session(value);
        } else {
            self.save_session = value;
        }
    }

    pub fn set_compress_threshold(&mut self, value: Option<usize>) {
        if let Some(session) = self.session.as_mut() {
            session.set_compress_threshold(value);
        } else {
            self.compress_threshold = value.unwrap_or_default();
        }
    }

    pub fn set_rag_reranker_model(config: &GlobalConfig, value: Option<String>) -> Result<()> {
        if let Some(id) = &value {
            crate::client::retrieve_model(&config.read().clients, id, ModelType::Reranker)?;
        }
        let has_rag = config.read().rag.is_some();
        match has_rag {
            true => update_rag(config, |rag| {
                rag.set_reranker_model(value)?;
                Ok(())
            })?,
            false => config.write().rag_reranker_model = value,
        }
        Ok(())
    }

    pub fn set_rag_top_k(config: &GlobalConfig, value: usize) -> Result<()> {
        let has_rag = config.read().rag.is_some();
        match has_rag {
            true => update_rag(config, |rag| {
                rag.set_top_k(value)?;
                Ok(())
            })?,
            false => config.write().rag_top_k = value,
        }
        Ok(())
    }

    pub fn set_wrap(&mut self, value: &str) -> Result<()> {
        if value == "no" {
            self.wrap = None;
        } else if value == "auto" {
            self.wrap = Some(value.into());
        } else {
            value
                .parse::<u16>()
                .map_err(|_| anyhow!("Invalid wrap value"))?;
            self.wrap = Some(value.into())
        }
        Ok(())
    }

    pub fn set_max_output_tokens(&mut self, value: Option<isize>) {
        if let Some(session) = self.session.as_mut() {
            let mut model = session.model().clone();
            model.set_max_tokens(value, true);
            session.set_model(model);
        } else if let Some(agent) = self.agent.as_mut() {
            let mut model = agent.model().clone();
            model.set_max_tokens(value, true);
            agent.set_model(model);
        } else {
            self.model.set_max_tokens(value, true);
        };
    }

    pub fn set_model(&mut self, model_id: &str) -> Result<()> {
        let model = crate::client::retrieve_model(&self.clients, model_id, ModelType::Chat)?;
        if let Some(session) = self.session.as_mut() {
            session.set_model(model);
        } else if let Some(agent) = self.agent.as_mut() {
            agent.set_model(model);
        } else {
            self.model = model;
        }
        Ok(())
    }

    pub fn use_prompt(&mut self, prompt: &str) -> Result<()> {
        let mut agent = Agent::new(AgentConfig::from_prompt(prompt));
        agent.set_model(self.current_model().clone());
        if agent.temperature().is_none() {
            agent.set_temperature(self.temperature);
        }
        if agent.top_p().is_none() {
            agent.set_top_p(self.top_p);
        }
        if agent.use_tools().is_none() {
            agent.set_use_tools(self.use_tools.clone());
        }
        self.use_agent_obj(agent)
    }

    pub fn retrieve_agent(&self, name: &str) -> Result<Agent> {
        let path = Self::agent_file(name);
        let mut agent = if path.exists() {
            self::agent::load(&path)?
        } else {
            self::agent::builtin(name)?
        };
        let current_model = self.current_model().clone();
        match agent.model_id() {
            Some(model_id) => {
                if current_model.id() != model_id {
                    let model =
                        crate::client::retrieve_model(&self.clients, model_id, ModelType::Chat)?;
                    agent.set_model(model);
                } else {
                    agent.set_model(current_model);
                }
            }
            None => {
                agent.set_model(current_model);
                if agent.temperature().is_none() {
                    agent.set_temperature(self.temperature);
                }
                if agent.top_p().is_none() {
                    agent.set_top_p(self.top_p);
                }
                if agent.use_tools().is_none() {
                    agent.set_use_tools(self.use_tools.clone());
                }
            }
        }
        Ok(agent)
    }

    pub fn use_agent_by_name(&mut self, name: &str) -> Result<()> {
        let mut agent = self.retrieve_agent(name)?;
        // Mirror the async `use_agent` flow: `init()` resolves file-backed
        // variable defaults (the `path:` field) before the agent becomes
        // active.  Without this, a follow-up `use_session` would call
        // `init_agent_session_variables`, find unresolved required variables,
        // and bail with "agent variables are required".
        self::agent::resolve_file_defaults(&mut agent)?;
        // Populate shared_variables from the resolved defaults so that
        // `session::new()` -> `set_agent()` -> `render_template()` can access
        // user-defined variables immediately (before `init_agent_session_variables`
        // runs). This mirrors the variable-initialization step in
        // `init_agent_session_variables` for the no-session-yet case.
        if !agent.defined_variables().is_empty() && agent.shared_variables().is_empty() {
            let mut config_variables = AgentVariables::default();
            if let Some(v) = &self.agent_variables {
                config_variables.extend(v.clone());
            }
            let shared = self::agent::init_agent_variables(
                agent.defined_variables(),
                &config_variables,
                self.info_flag,
            )?;
            agent.set_shared_variables(shared);
        }
        self.use_agent_obj(agent)
    }

    pub fn use_agent_obj(&mut self, agent: Agent) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            session.guard_empty()?;
            session.set_agent(&agent)?;
        } else {
            self.agent = Some(agent);
        }
        Ok(())
    }

    pub fn edit_agent_prompt(&mut self) -> Result<()> {
        let agent_name;
        if let Some(session) = self.session.as_ref() {
            if let Some(name) = session.agent_name().map(|v| v.to_string()) {
                if session.is_empty() {
                    agent_name = Some(name);
                } else {
                    bail!("Cannot perform this operation because you are in a non-empty session")
                }
            } else {
                bail!("No agent")
            }
        } else {
            agent_name = self.agent.as_ref().map(|v| v.name().to_string());
        }
        let name = agent_name.ok_or_else(|| anyhow!("No agent"))?;
        self.upsert_agent(&name)?;
        self.use_agent_by_name(&name)
    }

    pub fn upsert_agent(&mut self, name: &str) -> Result<()> {
        let agent_path = Self::agent_file(name);
        ensure_parent_exists(&agent_path)?;
        self.edit_with_tui_hooks(|this| {
            let editor = this.editor()?;
            edit_file(&editor, &agent_path)
        })?;
        if self.working_mode.is_tui() {
            crate::utils::emit_info(format!("✓ Saved the agent to '{}'.", agent_path.display()));
        }
        Ok(())
    }

    pub fn save_agent(&mut self, name: Option<&str>) -> Result<()> {
        let mut agent_name = match &self.agent {
            Some(agent) => {
                if agent.has_args() {
                    bail!("Unable to save the agent with arguments (whose name contains '#')")
                }
                match name {
                    Some(v) => v.to_string(),
                    None => agent.name().to_string(),
                }
            }
            None => bail!("No agent"),
        };
        if agent_name == TEMP_AGENT_NAME {
            agent_name = Text::new("Agent name:")
                .with_validator(|input: &str| {
                    let input = input.trim();
                    if input.is_empty() {
                        Ok(Validation::Invalid("This name is required".into()))
                    } else if input == TEMP_AGENT_NAME {
                        Ok(Validation::Invalid("This name is reserved".into()))
                    } else {
                        Ok(Validation::Valid)
                    }
                })
                .prompt()?;
        }
        let agent_path = Self::agent_file(&agent_name);
        if let Some(agent) = self.agent.as_mut() {
            let content = agent.export()?;
            ensure_parent_exists(&agent_path)?;
            std::fs::write(&agent_path, content).with_context(|| {
                format!(
                    "Failed to write agent '{}' to '{}'",
                    agent.name(),
                    agent_path.display()
                )
            })?;
            agent.set_name(&agent_name);
            if self.working_mode.is_tui() {
                crate::utils::emit_info(format!(
                    "✓ Saved the agent to '{}'.",
                    agent_path.display()
                ));
            }
        }

        Ok(())
    }

    pub fn all_agents() -> Vec<AgentConfig> {
        let mut agents: HashMap<String, AgentConfig> = HashMap::new();
        for name in list_agents() {
            let path = Self::agent_file(&name);
            if let Ok(agent) = self::agent::load(&path) {
                agents.insert(name, agent.into_config());
            }
        }
        let mut agents: Vec<_> = agents.into_values().collect();
        agents.sort_unstable_by(|a, b| a.name().cmp(b.name()));
        agents
    }

    pub fn use_session(&mut self, session_name: Option<&str>) -> Result<()> {
        if self.session.is_some() {
            bail!(
                "Already in a session, please run '.exit session' first to exit the current session."
            );
        }
        let mut session;
        match session_name {
            None => {
                let uuid_name = Uuid::now_v7().to_string();
                session = Some(self::session::new(self, &uuid_name)?);
            }
            Some(TEMP_SESSION_NAME) => {
                let session_file = self.session_file(TEMP_SESSION_NAME);
                if session_file.exists() {
                    remove_file(session_file).with_context(|| {
                        format!("Failed to cleanup previous '{TEMP_SESSION_NAME}' session")
                    })?;
                }
                session = Some(self::session::new(self, TEMP_SESSION_NAME)?);
            }
            Some(name) => {
                let session_path = self.session_file(name);
                if !session_path.exists() {
                    session = Some(self::session::new(self, name)?);
                } else {
                    session = Some(self::session::load(self, name, &session_path)?);
                }
            }
        }
        let mut new_session = false;
        let sessions_dir = self.sessions_dir();
        if let Some(session) = session.as_mut() {
            // Store sessions_dir so the log file can be lazily initialized
            // on the first event (avoids creating empty files in tests).
            // Must be set before any add_message() call that triggers logging.
            session.set_sessions_dir(sessions_dir);
            if session.is_empty() {
                new_session = true;
                if let Some(LastMessage {
                    input,
                    output,
                    continuous,
                }) = &self.last_message
                {
                    if (*continuous && !output.is_empty())
                        && self.agent.is_some() == input.with_agent()
                    {
                        let ans = Confirm::new(
                            "Start a session that incorporates the last question and answer?",
                        )
                        .with_default(false)
                        .prompt()?;
                        if ans {
                            crate::config::session::add_assistant_text(
                                session, input, output, None,
                            )?;
                        }
                    }
                }
            }
        }
        self.session = session;
        self.init_agent_session_variables(new_session)?;
        Ok(())
    }

    pub fn session_info(&self) -> Result<String> {
        if let Some(session) = &self.session {
            let render_options = self.render_options()?;
            let mut markdown_render = MarkdownRender::init(render_options)?;
            let agent_info: Option<(String, Vec<String>)> = self.agent.as_ref().map(|agent| {
                let functions = agent
                    .tools()
                    .declarations()
                    .iter()
                    .map(|v| v.name.clone())
                    .collect();
                (agent.name().to_string(), functions)
            });
            self::session::render(session, &mut markdown_render, &agent_info)
        } else {
            bail!("No session")
        }
    }

    pub fn exit_session(&mut self) -> Result<()> {
        if let Some(mut session) = self.session.take() {
            let sessions_dir = self.sessions_dir();
            self::session::exit(&mut session, &sessions_dir, self.working_mode.is_tui())?;
            self.discontinuous_last_message();
        }
        Ok(())
    }

    pub fn save_session(&mut self, name: Option<&str>) -> Result<()> {
        let session_name = match &self.session {
            Some(session) => match name {
                Some(v) => v.to_string(),
                None => session
                    .autoname()
                    .unwrap_or_else(|| session.name())
                    .to_string(),
            },
            None => bail!("No session"),
        };
        let session_path = self.session_file(&session_name);
        if let Some(session) = self.session.as_mut() {
            self::session::save(
                session,
                &session_name,
                &session_path,
                self.working_mode.is_tui(),
            )?;
        }
        Ok(())
    }

    pub fn set_tui_editor_hooks(
        &mut self,
        before: Option<Box<dyn FnMut() + Send + Sync>>,
        after: Option<Box<dyn FnMut() + Send + Sync>>,
    ) {
        self.tui_before_editor = before;
        self.tui_after_editor = after;
    }

    fn edit_with_tui_hooks<T, F>(&mut self, f: F) -> Result<T>
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

    pub fn edit_message_range(&mut self, from: usize, to: usize) -> Result<()> {
        let name = match &self.session {
            Some(session) => session.name().to_string(),
            None => bail!("No session"),
        };
        let session_path = self.session_file(&name);

        let raw_log = std::fs::read_to_string(&session_path)
            .with_context(|| format!("Failed to read '{}'", session_path.display()))?;
        let documents = split_session_log_documents(&raw_log);
        if from == 0 {
            bail!("Cannot edit or delete the session header (sequence 0)");
        }
        if to >= documents.len() {
            bail!("Sequence numbers out of range");
        }
        let (from, to) = adjust_range_for_tool_pairs(from, to, &documents)?;
        if from > to || to >= documents.len() {
            bail!("Sequence numbers out of range");
        }

        // Replacement list order becomes new order for edited range. Reordering
        // plain message entries in editor is supported as long as edited YAML still
        // passes structural validation (including tool-call/result pairing).
        let selected_documents = documents[from..=to].to_vec();
        let temp_file = if let Some(ref dir) = self.temp_dir_override {
            dir.join(format!("message-edit-{}.yaml", uuid::Uuid::new_v4()))
        } else {
            temp_file("message-edit", ".yaml")
        };

        std::fs::write(&temp_file, selected_documents.join("\n---\n"))
            .with_context(|| format!("Failed to write to '{}'", temp_file.display()))?;

        let edit_result = self.edit_with_tui_hooks(|this| {
            let editor = this.editor()?;
            edit_file(&editor, &temp_file).with_context(|| {
                format!("Failed to edit '{}' with '{}'", temp_file.display(), editor)
            })
        });
        let edited_content = std::fs::read_to_string(&temp_file)
            .with_context(|| format!("Failed to read '{}'", temp_file.display()));
        edit_result?;
        let edited_content = edited_content?;

        let edited_documents = validate_edited_session_documents(&edited_content)?;
        validate_tool_pair_integrity(from, &edited_documents)?;

        let _ = std::fs::remove_file(&temp_file);

        let edit_entry = SessionLogEntry::EditEntries {
            from,
            to,
            replacements: edited_documents,
        };
        let session = self.session.as_mut().context("No session")?;
        if !crate::config::session::append_event(session, &edit_entry) {
            bail!("Failed to append session edit entry")
        }
        self.session = Some(self::session::load(self, &name, &session_path)?);
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn delete_message_range(&mut self, from: usize, to: usize) -> Result<()> {
        let name = match &self.session {
            Some(session) => session.name().to_string(),
            None => bail!("No session"),
        };
        let session_path = self.session_file(&name);

        let raw_log = std::fs::read_to_string(&session_path)
            .with_context(|| format!("Failed to read '{}'", session_path.display()))?;
        let documents = split_session_log_documents(&raw_log);
        if from == 0 {
            bail!("Cannot edit or delete the session header (sequence 0)");
        }
        if to >= documents.len() {
            bail!("Sequence numbers out of range");
        }
        let (from, to) = adjust_range_for_tool_pairs(from, to, &documents)?;
        if from > to || to >= documents.len() {
            bail!("Sequence numbers out of range");
        }

        let edit_entry = SessionLogEntry::EditEntries {
            from,
            to,
            replacements: vec![],
        };
        let session = self.session.as_mut().context("No session")?;
        if !crate::config::session::append_event(session, &edit_entry) {
            bail!("Failed to append session delete entry")
        }
        self.session = Some(self::session::load(self, &name, &session_path)?);
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn rewind_session(&mut self, after_seq: usize) -> Result<()> {
        let name = match &self.session {
            Some(session) => session.name().to_string(),
            None => bail!("No session"),
        };
        let session_path = self.session_file(&name);

        let session = self.session.as_ref().context("No session")?;
        if after_seq >= session.log_entry_count {
            bail!(
                "Sequence number {} is out of range (log has {} entries)",
                after_seq,
                session.log_entry_count
            );
        }

        // Reject a cut point that splits a ToolCalls/ToolResults pair.
        let raw_log = std::fs::read_to_string(&session_path)
            .with_context(|| format!("Failed to read '{}'", session_path.display()))?;
        let documents = split_session_log_documents(&raw_log);
        let parse = |idx: usize| -> Option<SessionLogEntry> {
            documents
                .get(idx)
                .and_then(|raw| serde_yaml::from_str::<SessionLogEntry>(raw).ok())
        };
        if matches!(parse(after_seq), Some(SessionLogEntry::ToolCalls { .. }))
            && matches!(
                parse(after_seq + 1),
                Some(SessionLogEntry::ToolResults { .. })
            )
        {
            bail!(
                "Sequence {after_seq} is a tool-calls entry paired with tool-results at {}; \
                 rewinding here would orphan the tool calls. \
                 Use {} to keep the pair or {} to exclude it.",
                after_seq + 1,
                after_seq + 1,
                after_seq.saturating_sub(1),
            );
        }

        let rewind_entry = SessionLogEntry::Rewind { after_seq };
        let session = self.session.as_mut().context("No session")?;
        if !crate::config::session::append_event(session, &rewind_entry) {
            bail!("Failed to append session rewind entry")
        }
        self.session = Some(self::session::load(self, &name, &session_path)?);
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn edit_session(&mut self) -> Result<()> {
        let name = match &self.session {
            Some(session) => session.name().to_string(),
            None => bail!("No session"),
        };
        let session_path = self.session_file(&name);
        self.save_session(Some(&name))?;
        self.edit_with_tui_hooks(|this| {
            let editor = this.editor()?;
            edit_file(&editor, &session_path).with_context(|| {
                format!(
                    "Failed to edit '{}' with '{editor}'",
                    session_path.display()
                )
            })
        })?;
        self.session = Some(self::session::load(self, &name, &session_path)?);
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn empty_session(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            if let Some(agent) = self.agent.as_ref() {
                session.sync_agent(agent)?;
                // Persist the updated agent name/prompt/variables to disk
                // before clearing messages, so the header reflects the
                // current agent state if the session file is reloaded.
                crate::config::session::append_event(session, &session.build_header_entry());
            }
            crate::config::session::clear_messages(session);
        } else {
            bail!("No session")
        }
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn reset_session(&mut self) -> Result<()> {
        // Capture current session name before exiting
        let old_session_name = self.session.as_ref().map(|s| s.name().to_string());

        // Discard the current session without saving
        if let Some(session) = self.session.take() {
            drop(session);
            self.discontinuous_last_message();
        }
        if let Some(agent) = self.agent.as_mut() {
            agent.exit_session();
        }

        // Re-create a session with freshly-expanded variables
        let new_session_name = if let Some(agent) = &self.agent {
            let extra_vars = std::collections::HashMap::from([("AGENT_NAME", agent.name())]);
            // Per-agent front-matter first, then global config fallback
            let template = agent
                .agent_default_session()
                .map(|s| s.to_string())
                .or_else(|| self.agent_default_session.clone());
            template
                .map(|v| {
                    session_name::sanitize_session_name(
                        &session_name::expand_session_variables_with(&v, &extra_vars),
                    )
                })
                .filter(|v| !v.is_empty())
        } else {
            let default_session = match self.working_mode {
                WorkingMode::Tui => self.tui_default_session.as_ref(),
                WorkingMode::Cmd => self.cmd_default_session.as_ref(),
                WorkingMode::Serve | WorkingMode::Acp(_) => None,
            };
            default_session
                .filter(|v| !v.is_empty())
                .map(|v| {
                    session_name::sanitize_session_name(&session_name::expand_session_variables(v))
                })
                .filter(|v| !v.is_empty())
        };

        let session_name = new_session_name.or(old_session_name);
        if let Some(name) = session_name {
            self.use_session(Some(&name))?;
        }
        Ok(())
    }

    pub fn set_save_session_this_time(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            session.set_save_session_this_time();
        } else {
            bail!("No session")
        }
        Ok(())
    }

    pub fn list_sessions(&self) -> Vec<String> {
        list_file_names(self.sessions_dir(), ".yaml")
    }

    pub fn list_sessions_with_meta(&self) -> Vec<SessionMeta> {
        let Ok(entries) = std::fs::read_dir(self.sessions_dir()) else {
            return Vec::new();
        };

        let mut sessions = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let name = path.file_stem()?.to_str()?;
                (path.extension().and_then(|ext| ext.to_str()) == Some("yaml"))
                    .then(|| parse_session_meta(name, &path))
                    .flatten()
            })
            .collect::<Vec<_>>();

        sessions.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        sessions
    }

    pub fn maybe_compact_session(config: GlobalConfig) {
        let mut need_compact = false;
        {
            let mut config = config.write();
            let compress_threshold = config.compress_threshold;
            if let Some(session) = config.session.as_mut() {
                if session.need_compress(compress_threshold) {
                    session.set_compressing(true);
                    need_compact = true;
                }
            }
        };
        if !need_compact {
            return;
        }
        let color = if config.read().light_theme() {
            nu_ansi_term::Color::LightGray
        } else {
            nu_ansi_term::Color::DarkGray
        };
        crate::utils::emit_info(format!(
            "📢 {}",
            color.italic().paint("Compacting the session.")
        ));
        tokio::spawn(async move {
            if let Err(err) = Config::compact_session(&config).await {
                warn!("Failed to compact the session: {err}");
            }
            if let Some(session) = config.write().session.as_mut() {
                session.set_compressing(false);
            }
        });
    }

    pub async fn compact_session(config: &GlobalConfig) -> Result<()> {
        match config.read().session.as_ref() {
            Some(session) => {
                if !session.has_user_messages() {
                    bail!("No need to compact since there are no messages in the session")
                }
            }
            None => bail!("No session"),
        }

        // Check if the current agent has a compaction_agent configured
        let compaction_agent_name = config
            .read()
            .extract_agent()
            .compaction_agent()
            .map(str::to_owned);

        let (prompt, agent_override) = if let Some(name) = compaction_agent_name {
            match config.read().retrieve_agent(&name) {
                Ok(mut compaction_agent) => {
                    if let Err(e) = self::agent::resolve_variables(&mut compaction_agent) {
                        warn!("Failed to resolve variables for compaction_agent '{name}': {e}");
                    }
                    // Keep the normal compaction prompt text so session-aware
                    // message building still has non-empty user content, while
                    // the agent override provides the specialized system prompt.
                    (DEFAULT_COMPACT_PROMPT.to_string(), Some(compaction_agent))
                }
                Err(e) => {
                    warn!(
                        "Failed to load compaction_agent '{name}': {e}; falling back to default compaction"
                    );
                    (DEFAULT_COMPACT_PROMPT.to_string(), None)
                }
            }
        } else {
            (DEFAULT_COMPACT_PROMPT.to_string(), None)
        };

        // Build the Input without an agent override so that with_session=true is
        // preserved — the compaction LLM must see the full session history to
        // summarise it.  Then swap the agent (model/params/prompt) in place
        // without disturbing the with_session flag.
        let mut input = crate::config::input::from_str(config, &prompt, None);
        if let Some(compaction_agent) = agent_override {
            crate::config::input::set_agent(&mut input, config, compaction_agent.into_config());
        }
        let summary = crate::config::input::fetch_chat_text(&input, config).await?;
        if let Some(session) = config.write().session.as_mut() {
            crate::config::session::compress(session, summary);
        }
        config.write().discontinuous_last_message();
        Ok(())
    }

    pub fn is_compacting_session(&self) -> bool {
        self.session
            .as_ref()
            .map(|v| v.compressing())
            .unwrap_or_default()
    }

    pub fn maybe_autoname_session(config: GlobalConfig) {
        let mut need_autoname = false;
        if let Some(session) = config.write().session.as_mut() {
            if session.need_autoname() {
                session.set_autonaming(true);
                need_autoname = true;
            }
        }
        if !need_autoname {
            return;
        }
        let color = if config.read().light_theme() {
            nu_ansi_term::Color::LightGray
        } else {
            nu_ansi_term::Color::DarkGray
        };
        crate::utils::emit_info(format!(
            "📢 {}",
            color.italic().paint("Autonaming the session.")
        ));
        tokio::spawn(async move {
            if let Err(err) = Config::autoname_session(&config).await {
                warn!("Failed to autonaming the session: {err}");
            }
            if let Some(session) = config.write().session.as_mut() {
                session.set_autonaming(false);
            }
        });
    }

    pub async fn autoname_session(config: &GlobalConfig) -> Result<()> {
        let text = match config
            .read()
            .session
            .as_ref()
            .and_then(|v| v.chat_history_for_autonaming())
        {
            Some(v) => v,
            None => bail!("No chat history"),
        };
        let agent = config.read().retrieve_agent(CREATE_TITLE_AGENT)?;
        let input = crate::config::input::from_str(config, &text, Some(agent));
        let text = crate::config::input::fetch_chat_text(&input, config).await?;
        if let Some(session) = config.write().session.as_mut() {
            session.set_autoname(&text);
        }
        Ok(())
    }

    pub async fn use_rag(
        config: &GlobalConfig,
        rag: Option<&str>,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        if config.read().agent.is_some() {
            bail!("Cannot perform this operation because you are using a agent")
        }
        let rag = match rag {
            None => {
                let rag_path = config.read().rag_file(TEMP_RAG_NAME);
                if rag_path.exists() {
                    remove_file(&rag_path).with_context(|| {
                        format!("Failed to cleanup previous '{TEMP_RAG_NAME}' rag")
                    })?;
                }
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
                Rag::init(&init_ctx, TEMP_RAG_NAME, &rag_path, &[], abort_signal).await?
            }
            Some(name) => {
                let rag_path = config.read().rag_file(name);
                if !rag_path.exists() {
                    if config.read().working_mode.is_cmd() {
                        bail!("Unknown RAG '{name}'")
                    }
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
                    Rag::init(&init_ctx, name, &rag_path, &[], abort_signal).await?
                } else {
                    Rag::load(&config.read().clients, name, &rag_path)?
                }
            }
        };
        config.write().rag = Some(Arc::new(rag));
        Ok(())
    }

    pub async fn edit_rag_docs(config: &GlobalConfig, abort_signal: AbortSignal) -> Result<()> {
        let mut rag = match config.read().rag.clone() {
            Some(v) => v.as_ref().clone(),
            None => bail!("No RAG"),
        };

        let document_paths = rag.document_paths();
        let temp_file = temp_file(&format!("-rag-{}", rag.name()), ".txt");
        tokio::fs::write(&temp_file, &document_paths.join("\n"))
            .await
            .with_context(|| format!("Failed to write to '{}'", temp_file.display()))?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut config_write = config.write();
            let editor = config_write.editor()?;
            let temp_file_path = temp_file.clone();
            config_write.edit_with_tui_hooks(|_| {
                let result = edit_file(&editor, &temp_file_path);
                let _ = tx.send(result);
                Ok(())
            })?;
        }
        rx.await
            .map_err(|_| anyhow!("Editor hook completion channel unexpectedly closed"))??;
        let new_document_paths = tokio::fs::read_to_string(&temp_file)
            .await
            .with_context(|| format!("Failed to read '{}'", temp_file.display()))?;
        let new_document_paths = new_document_paths
            .split('\n')
            .filter_map(|v| {
                let v = v.trim();
                if v.is_empty() {
                    None
                } else {
                    Some(v.to_string())
                }
            })
            .collect::<Vec<_>>();
        if new_document_paths.is_empty() || new_document_paths == document_paths {
            bail!("No changes")
        }
        let (document_loaders, user_agent_owned, dry_run) = {
            let cfg = config.read();
            (
                cfg.document_loaders.clone(),
                cfg.user_agent.clone(),
                cfg.dry_run,
            )
        };
        let call_ctx = harnx_rag::RagCallContext {
            user_agent: user_agent_owned.as_deref(),
            dry_run,
        };
        rag.refresh_document_paths(
            &new_document_paths,
            false,
            &document_loaders,
            &call_ctx,
            abort_signal,
        )
        .await?;
        config.write().rag = Some(Arc::new(rag));
        Ok(())
    }

    pub async fn rebuild_rag(config: &GlobalConfig, abort_signal: AbortSignal) -> Result<()> {
        let mut rag = match config.read().rag.clone() {
            Some(v) => v.as_ref().clone(),
            None => bail!("No RAG"),
        };
        let document_paths = rag.document_paths().to_vec();
        let (document_loaders, user_agent_owned, dry_run) = {
            let cfg = config.read();
            (
                cfg.document_loaders.clone(),
                cfg.user_agent.clone(),
                cfg.dry_run,
            )
        };
        let call_ctx = harnx_rag::RagCallContext {
            user_agent: user_agent_owned.as_deref(),
            dry_run,
        };
        rag.refresh_document_paths(
            &document_paths,
            true,
            &document_loaders,
            &call_ctx,
            abort_signal,
        )
        .await?;
        config.write().rag = Some(Arc::new(rag));
        Ok(())
    }

    pub fn rag_sources(config: &GlobalConfig) -> Result<String> {
        match config.read().rag.as_ref() {
            Some(rag) => match rag.get_last_sources() {
                Some(v) => Ok(v),
                None => bail!("No sources"),
            },
            None => bail!("No RAG"),
        }
    }

    pub fn rag_info(&self) -> Result<String> {
        if let Some(rag) = &self.rag {
            rag.export()
        } else {
            bail!("No RAG")
        }
    }

    pub fn exit_rag(&mut self) -> Result<()> {
        self.rag.take();
        Ok(())
    }

    pub async fn search_rag(
        config: &GlobalConfig,
        rag: &Rag,
        text: &str,
        abort_signal: AbortSignal,
    ) -> Result<String> {
        let (reranker_model, top_k) = rag.get_config();
        let (embeddings, ids) = {
            let (user_agent_owned, dry_run) = {
                let cfg = config.read();
                (cfg.user_agent.clone(), cfg.dry_run)
            };
            let call_ctx = harnx_rag::RagCallContext {
                user_agent: user_agent_owned.as_deref(),
                dry_run,
            };
            rag.search(
                &call_ctx,
                text,
                top_k,
                reranker_model.as_deref(),
                abort_signal,
            )
            .await?
        };
        let text = config.read().rag_template(&embeddings, text);
        rag.set_last_sources(&ids);
        Ok(text)
    }

    pub fn list_rags() -> Vec<String> {
        match read_dir(Self::rags_dir()) {
            Ok(rd) => {
                let mut names = vec![];
                for entry in rd.flatten() {
                    let name = entry.file_name();
                    if let Some(name) = name.to_string_lossy().strip_suffix(".yaml") {
                        names.push(name.to_string());
                    }
                }
                names.sort_unstable();
                names
            }
            Err(_) => vec![],
        }
    }

    pub fn rag_template(&self, embeddings: &str, text: &str) -> String {
        if embeddings.is_empty() {
            return text.to_string();
        }
        self.rag_template
            .as_deref()
            .unwrap_or(RAG_TEMPLATE)
            .replace("__CONTEXT__", embeddings)
            .replace("__INPUT__", text)
    }

    pub async fn use_agent(
        config: &GlobalConfig,
        agent_name: &str,
        session_name: Option<&str>,
        abort_signal: AbortSignal,
    ) -> Result<()> {
        if !config.read().tool_use {
            bail!("Please enable tool use before using the agent.");
        }
        if config.read().agent.is_some() {
            bail!("Already in a agent, please run '.exit agent' first to exit the current agent.");
        }
        let agent = self::agent::init(config, agent_name, abort_signal).await?;
        let extra_vars = std::collections::HashMap::from([("AGENT_NAME", agent.name())]);
        let global_agent_session = config.read().agent_default_session.clone();
        let session = session_name.map(|v| v.to_string()).or_else(|| {
            if config.read().macro_flag {
                None
            } else {
                // Per-agent front-matter first, then global config fallback
                let template = agent
                    .agent_default_session()
                    .map(|s| s.to_string())
                    .or_else(|| global_agent_session.clone());
                template
                    .map(|v| {
                        session_name::sanitize_session_name(
                            &session_name::expand_session_variables_with(&v, &extra_vars),
                        )
                    })
                    .filter(|v| !v.is_empty())
            }
        });
        config.write().rag = agent.rag();
        config.write().agent = Some(agent);
        // Populate shared_variables from resolved file-backed defaults and
        // any --agent-variable overrides before any code path that renders
        // the template. session::new() -> set_agent() runs the template
        // immediately, so this must happen before use_session().
        config.write().init_agent_shared_variables()?;
        if let Some(session) = session {
            // Exit any existing session (e.g. from tui_default_session) before
            // switching to the agent's session.
            config.write().exit_session()?;
            config.write().use_session(Some(&session))?;
        }
        Ok(())
    }

    pub fn agent_info(&self) -> Result<String> {
        if let Some(agent) = &self.agent {
            agent.export()
        } else {
            bail!("No agent")
        }
    }

    pub fn agent_banner(&self) -> Result<String> {
        if let Some(agent) = &self.agent {
            Ok(agent.banner())
        } else {
            bail!("No agent")
        }
    }

    pub fn exit_agent(&mut self) -> Result<()> {
        self.exit_session()?;
        if self.agent.take().is_some() {
            self.rag.take();
            self.discontinuous_last_message();
        }
        Ok(())
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

    pub fn list_macros() -> Vec<String> {
        list_file_names(Self::macros_dir(), ".yaml")
    }

    pub fn load_macro(name: &str) -> Result<Macro> {
        let path = Self::macro_file(name);
        let err = || format!("Failed to load macro '{name}' at '{}'", path.display());
        let content = read_to_string(&path).with_context(err)?;
        let value: Macro = serde_yaml::from_str(&content).with_context(err)?;
        Ok(value)
    }

    pub fn has_macro(name: &str) -> bool {
        let names = Self::list_macros();
        names.contains(&name.to_string())
    }

    pub fn new_macro(&mut self, name: &str) -> Result<()> {
        if self.macro_flag {
            bail!("No macro");
        }
        let ans = Confirm::new("Create a new macro?")
            .with_default(true)
            .prompt()?;
        if ans {
            let macro_path = Self::macro_file(name);
            ensure_parent_exists(&macro_path)?;
            self.edit_with_tui_hooks(|this| {
                let editor = this.editor()?;
                edit_file(&editor, &macro_path)
            })?;
        } else {
            bail!("No macro");
        }
        Ok(())
    }

    pub fn apply_default_session(&mut self) -> Result<()> {
        if self.macro_flag || !self.state().is_empty() {
            return Ok(());
        }
        let default_session = match self.working_mode {
            WorkingMode::Tui => self.tui_default_session.as_ref(),
            WorkingMode::Cmd => self.cmd_default_session.as_ref(),
            WorkingMode::Serve => return Ok(()),
            WorkingMode::Acp(_) => self.agent_default_session.as_ref(),
        };
        let default_session = match default_session {
            Some(v) => {
                if v.is_empty() {
                    return Ok(());
                }
                v.to_string()
            }
            None => return Ok(()),
        };
        let session_name = session_name::sanitize_session_name(
            &session_name::expand_session_variables(&default_session),
        );
        if !session_name.is_empty() {
            self.use_session(Some(&session_name))?;
        }
        Ok(())
    }

    pub fn select_tools(&self, agent: &AgentConfig) -> Option<Vec<ToolDeclaration>> {
        let mut functions = vec![];
        if self.tool_use {
            if let Some(use_tools) = agent.use_tools() {
                let use_tools_str = use_tools.join(",");
                let declarations = self.tool_declarations_for_use_tools(Some(&use_tools_str));
                let mut tool_names: HashSet<String> = Default::default();
                let declaration_names: HashSet<String> =
                    declarations.iter().map(|v| v.name.to_string()).collect();
                for item in use_tools.iter().map(|s| s.trim()) {
                    if let Some(values) = self.toolsets.get(item) {
                        tool_names.extend(
                            values
                                .iter()
                                .filter(|v| declaration_names.contains(v.as_str()))
                                .cloned(),
                        )
                    } else {
                        tool_names.extend(
                            declaration_names
                                .iter()
                                .filter(|name| matches_tool_glob(item, name))
                                .cloned(),
                        );
                    }
                }
                functions = declarations
                    .iter()
                    .filter_map(|v| {
                        if tool_names.contains(&v.name) {
                            Some(v.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
            }

            if let Some(agent) = &self.agent {
                let mut agent_functions = agent.tools().declarations();
                let tool_names: HashSet<String> =
                    agent_functions.iter().map(|v| v.name.to_string()).collect();
                agent_functions.extend(
                    functions
                        .into_iter()
                        .filter(|v| !tool_names.contains(&v.name)),
                );
                functions = agent_functions;
            }
        };
        if functions.is_empty() {
            None
        } else {
            Some(functions)
        }
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

    pub fn command_complete(
        &self,
        cmd: &str,
        args: &[&str],
        _line: &str,
    ) -> Vec<(String, Option<String>)> {
        let mut values: Vec<(String, Option<String>)> = vec![];
        let filter = args.last().unwrap_or(&"");
        if args.len() == 1 {
            values = match cmd {
                ".model" => list_models(&self.clients, ModelType::Chat)
                    .into_iter()
                    .map(|v| (v.id(), Some(v.description())))
                    .collect(),
                ".session" => map_completion_values(self.list_sessions()),
                ".rag" => map_completion_values(Self::list_rags()),
                ".agent" => map_completion_values(list_agents()),
                ".macro" => map_completion_values(Self::list_macros()),
                ".info" => map_completion_values(vec!["session", "agent", "rag", "tools"]),
                ".mcp" => map_completion_values(vec![
                    "list",
                    "connect",
                    "disconnect",
                    "tools",
                    "roots",
                    "add-root",
                    "remove-root",
                ]),
                ".use" => map_completion_values(vec!["tool"]),
                ".drop" => map_completion_values(vec!["tool"]),
                ".starter" => match &self.agent {
                    Some(agent) => agent
                        .conversation_staters()
                        .iter()
                        .enumerate()
                        .map(|(i, v)| ((i + 1).to_string(), Some(v.to_string())))
                        .collect(),
                    None => vec![],
                },
                ".set" => {
                    let mut values = vec![
                        "temperature",
                        "top_p",
                        "use_tools",
                        "save_session",
                        "compress_threshold",
                        "compaction_agent",
                        "model_fallbacks",
                        "rag_reranker_model",
                        "rag_top_k",
                        "max_output_tokens",
                        "dry_run",
                        "tool_use",
                        "stream",
                        "save",
                        "highlight",
                    ];
                    values.sort_unstable();
                    values
                        .into_iter()
                        .map(|v| (format!("{v} "), None))
                        .collect()
                }
                ".delete" => {
                    map_completion_values(vec!["agent", "session", "rag", "macro", "agent-data"])
                }
                _ => vec![],
            };
        } else if cmd == ".set" && args.len() == 2 {
            let candidates = match args[0] {
                "max_output_tokens" => match self.current_model().max_output_tokens() {
                    Some(v) => vec![v.to_string()],
                    None => vec![],
                },
                "dry_run" => complete_bool(self.dry_run),
                "stream" => complete_bool(self.stream),
                "save" => complete_bool(self.save),
                "tool_use" => complete_bool(self.tool_use),
                "use_tools" => {
                    let mut prefix = String::new();
                    let mut ignores = HashSet::new();
                    if let Some((v, _)) = args[1].rsplit_once(',') {
                        ignores = v.split(',').collect();
                        prefix = format!("{v},");
                    }
                    let mut values = vec![];
                    if prefix.is_empty() {
                        values.push("*".to_string());
                    }
                    values.extend(
                        self.tool_declarations_for_use_tools(Some("*"))
                            .iter()
                            .map(|v| v.name.clone()),
                    );
                    values.extend(self.toolsets.keys().map(|v| v.to_string()));
                    values
                        .into_iter()
                        .filter(|v| !ignores.contains(v.as_str()))
                        .map(|v| format!("{prefix}{v}"))
                        .collect()
                }
                "save_session" => {
                    let save_session = if let Some(session) = &self.session {
                        session.save_session()
                    } else {
                        self.save_session
                    };
                    complete_option_bool(save_session)
                }
                "rag_reranker_model" => list_models(&self.clients, ModelType::Reranker)
                    .iter()
                    .map(|v| v.id())
                    .collect(),
                "highlight" => complete_bool(self.highlight),
                _ => vec![],
            };
            values = candidates.into_iter().map(|v| (v, None)).collect();
        } else if cmd == ".use" && args.len() == 2 && args[0] == "tool" {
            let mut candidates: Vec<String> = self
                .tool_declarations_for_use_tools(Some("*"))
                .iter()
                .map(|v| v.name.clone())
                .collect();
            candidates.extend(self.toolsets.keys().map(|v| v.to_string()));
            if let Some(manager) = &self.mcp_manager {
                for name in manager.list_servers() {
                    candidates.push(format!("{name}_*"));
                }
            }
            let active = self.active_tool_names();
            values = candidates
                .into_iter()
                .filter(|v| !active.contains(v))
                .map(|v| (v, None))
                .collect();
        } else if cmd == ".drop" && args.len() == 2 && args[0] == "tool" {
            let agent = self.extract_agent();
            let current = agent.use_tools().unwrap_or_default();
            values = current.into_iter().map(|s| (s, None)).collect();
        } else if cmd == ".mcp" && args.len() == 2 {
            let subcmd = args[0];
            if matches!(
                subcmd,
                "connect" | "disconnect" | "tools" | "roots" | "add-root" | "remove-root"
            ) {
                let servers = Self::mcp_list_servers_from_config(self);
                values = servers.into_iter().map(|v| (v, None)).collect();
            }
        } else if cmd == ".agent" {
            if args.len() == 2 {
                let dir = Self::agent_data_dir(args[0]).join(paths::SESSIONS_DIR_NAME);
                values = list_file_names(dir, ".yaml")
                    .into_iter()
                    .map(|v| (v, None))
                    .collect();
            }
            values.extend(complete_agent_variables(args[0]));
        };
        fuzzy_filter(values, |v| v.0.as_str(), filter)
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
        let session_name = self.session.as_ref().map(|s| s.name().to_string());
        match (agent_name, session_name, use_icons) {
            (Some(agent), Some(session), true) => format!("🤖 {} ▸ {}", agent, session),
            (Some(agent), Some(session), false) => format!("{} ▸ {}", agent, session),
            (Some(agent), None, true) => format!("🤖 {}", agent),
            (Some(agent), None, false) => agent,
            (None, Some(session), true) => format!("💬 {}", session),
            (None, Some(session), false) => session,
            (None, None, _) => String::new(),
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

    pub fn after_chat_completion(
        &mut self,
        input: &Input,
        output: &str,
        thought: Option<&str>,
        tool_results: &[ToolResult],
        usage: &crate::client::CompletionTokenUsage,
    ) -> Result<()> {
        if let Some(session) = &mut self.session {
            session.add_completion_usage(usage);
        }
        if tool_results.is_empty() {
            self.last_message = Some(LastMessage::new(input.clone(), output.to_string()));
        }
        if !self.dry_run {
            self.save_message(input, output, thought, tool_results)?;
        }
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

    /// Record an assistant tool-call request BEFORE the tools execute.
    /// Writes a `ToolCalls` entry to the session log and pushes a
    /// pending Tool message in-memory.  Must be paired with a
    /// [`save_session_tool_results`] call once outputs are available.
    ///
    /// Errors if no session is active or persistence fails.
    pub fn save_session_tool_calls(
        &mut self,
        input: &Input,
        output: &str,
        thought: Option<&str>,
        calls: &[crate::tool::ToolCall],
    ) -> Result<()> {
        let mut input = input.clone();
        input.clear_patch();
        let sessions_dir = self.sessions_dir();
        if !input.with_session() {
            return Ok(());
        }
        let Some(session) = self.session.as_mut() else {
            return Ok(());
        };
        session.set_sessions_dir(sessions_dir);
        crate::config::session::add_tool_calls(session, &input, output, thought, calls)
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

    pub fn save_message(
        &mut self,
        input: &Input,
        output: &str,
        thought: Option<&str>,
        tool_results: &[crate::tool::ToolResult],
    ) -> Result<()> {
        let mut input = input.clone();
        input.clear_patch();
        let sessions_dir = self.sessions_dir();
        if input.with_session() {
            if let Some(session) = self.session.as_mut() {
                session.set_sessions_dir(sessions_dir);
                if tool_results.is_empty() {
                    crate::config::session::add_assistant_text(session, &input, output, thought)?;
                } else {
                    // Split the combined save into its two natural
                    // events: the assistant's tool-call request, then
                    // the tool results.  Callers on the split flow
                    // drive these directly via
                    // `save_assistant_tool_calls` /
                    // `save_tool_results`; this path exists for
                    // legacy all-at-once callers until they migrate.
                    let calls: Vec<_> = tool_results.iter().map(|r| r.call.clone()).collect();
                    crate::config::session::add_tool_calls(
                        session, &input, output, thought, &calls,
                    )?;
                    crate::config::session::add_tool_results(session, tool_results)?;
                }
                return Ok(());
            }
        }

        if !self.save {
            return Ok(());
        }
        let mut file = self.open_message_file()?;
        if output.is_empty() && input.tool_calls().is_none() {
            return Ok(());
        }
        let now = now();
        let summary = input.summary();
        let raw_input = input.raw();
        let scope = if self.agent.is_none() {
            let agent_name = match input.agent().name() {
                TEMP_AGENT_NAME => None,
                "" => None,
                name => Some(name),
            };
            match (agent_name, input.rag_name()) {
                (Some(agent), Some(rag_name)) => format!(" ({agent}#{rag_name})"),
                (Some(agent), _) => format!(" ({agent})"),
                (None, Some(rag_name)) => format!(" (#{rag_name})"),
                _ => String::new(),
            }
        } else {
            String::new()
        };
        let tool_calls = match input.tool_calls() {
            Some(MessageContentToolCalls {
                tool_results, text, ..
            }) => {
                let mut lines = vec!["<tool_calls>".to_string()];
                if !text.is_empty() {
                    lines.push(text.clone());
                }
                lines.push(serde_json::to_string(&tool_results).unwrap_or_default());
                lines.push("</tool_calls>\n".to_string());
                lines.join("\n")
            }
            None => String::new(),
        };
        let thought = match thought {
            Some(v) => format!("<think>\n{v}\n</think>\n"),
            None => String::new(),
        };
        let output = format!(
            "# CHAT: {summary} [{now}]{scope}\n{raw_input}\n--------\n{thought}{tool_calls}{output}\n--------\n\n",
        );
        file.write_all(output.as_bytes())
            .with_context(|| "Failed to save message")
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

    fn open_message_file(&self) -> Result<File> {
        let path = self.messages_file();
        ensure_parent_exists(&path)?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to create/append {}", path.display()))
    }

    fn load_from_file(config_path: &Path) -> Result<Self> {
        let err = || format!("Failed to load config at '{}'", config_path.display());
        let data: ConfigData = if config_path.exists() {
            let content = read_to_string(config_path).with_context(err)?;
            serde_yaml::from_str(&content)
                .map_err(|err| anyhow!(err.to_string()))
                .with_context(err)?
        } else {
            ConfigData::default()
        };
        let mut config = Self {
            data,
            ..Self::default()
        };
        let config_dir = config_path.parent().unwrap_or(config_path);
        config.clients = Self::load_clients_from_dir(&config_dir.join(paths::CLIENTS_DIR_NAME))?;
        config.mcp_servers =
            Self::load_mcp_servers_from_dir(&config_dir.join(paths::MCP_SERVERS_DIR_NAME))?;
        config.acp_servers =
            Self::load_acp_servers_from_dir(&config_dir.join(paths::ACP_SERVERS_DIR_NAME))?;
        Self::auto_register_agents(&mut config.acp_servers)?;
        Ok(config)
    }

    fn load_clients_from_dir(dir: &Path) -> Result<Vec<ClientConfig>> {
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut clients = Vec::new();
        for path in Self::sorted_yaml_files(dir)? {
            let content = read_to_string(&path)
                .with_context(|| format!("Failed to read client config '{}'", path.display()))?;
            let client: ClientConfig = serde_yaml::from_str(&content)
                .with_context(|| format!("Failed to parse client config '{}'", path.display()))?;
            clients.push(client);
        }
        Ok(clients)
    }

    fn load_mcp_servers_from_dir(dir: &Path) -> Result<Vec<McpServerConfig>> {
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut servers = Vec::new();
        for path in Self::sorted_yaml_files(dir)? {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let content = read_to_string(&path).with_context(|| {
                format!("Failed to read MCP server config '{}'", path.display())
            })?;
            let mut server: McpServerConfig =
                serde_yaml::from_str(&content).with_context(|| {
                    format!("Failed to parse MCP server config '{}'", path.display())
                })?;
            server.name = stem;
            servers.push(server);
        }
        Ok(servers)
    }

    fn load_acp_servers_from_dir(dir: &Path) -> Result<Vec<AcpServerConfig>> {
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut servers = Vec::new();
        for path in Self::sorted_yaml_files(dir)? {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let content = read_to_string(&path).with_context(|| {
                format!("Failed to read ACP server config '{}'", path.display())
            })?;
            let mut server: AcpServerConfig =
                serde_yaml::from_str(&content).with_context(|| {
                    format!("Failed to parse ACP server config '{}'", path.display())
                })?;
            server.name = stem;
            servers.push(server);
        }
        Ok(servers)
    }

    fn auto_register_agents(acp_servers: &mut Vec<AcpServerConfig>) -> Result<()> {
        let existing_names: HashSet<String> = acp_servers
            .iter()
            .map(|server| server.name.clone())
            .collect();
        let current_exe = std::env::current_exe()?.to_string_lossy().to_string();
        for agent_name in list_agents() {
            if !existing_names.contains(&agent_name) {
                acp_servers.push(AcpServerConfig {
                    name: agent_name.clone(),
                    command: current_exe.clone(),
                    args: vec!["--acp".to_string(), agent_name],
                    env: Default::default(),
                    enabled: true,
                    description: None,
                    idle_timeout_secs: 300,
                    operation_timeout_secs: 3600,
                });
            }
        }
        Ok(())
    }

    fn sorted_yaml_files(dir: &Path) -> Result<Vec<PathBuf>> {
        let entries = read_dir(dir)
            .with_context(|| format!("Failed to read directory '{}'", dir.display()))?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry =
                entry.with_context(|| format!("Failed to read entry in '{}'", dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("yaml") {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }

    fn load_dynamic(model_id: &str) -> Result<Self> {
        let provider = match model_id.split_once(':') {
            Some((v, _)) => v,
            _ => model_id,
        };
        let is_openai_compatible = OPENAI_COMPATIBLE_PROVIDERS
            .into_iter()
            .any(|(name, _)| provider == name);
        let client = if is_openai_compatible {
            json!({ "type": "openai-compatible", "name": provider })
        } else {
            json!({ "type": provider })
        };
        let data_value = json!({
            "model": model_id.to_string(),
            "save": false,
        });
        let data: ConfigData =
            serde_json::from_value(data_value).with_context(|| "Failed to load config from env")?;

        let mut config = Self {
            data,
            ..Self::default()
        };

        config.clients =
            vec![serde_json::from_value(client).context("Failed to parse client config")?];

        let config_dir = Self::config_dir();
        config.mcp_servers =
            Self::load_mcp_servers_from_dir(&config_dir.join(paths::MCP_SERVERS_DIR_NAME))?;
        config.acp_servers =
            Self::load_acp_servers_from_dir(&config_dir.join(paths::ACP_SERVERS_DIR_NAME))?;
        Self::auto_register_agents(&mut config.acp_servers)?;
        Ok(config)
    }

    fn load_envs(&mut self) {
        if let Ok(v) = env::var(get_env_name("model")) {
            self.model_id = v;
        }
        if let Some(v) = read_env_value::<f64>(&get_env_name("temperature")) {
            self.temperature = v;
        }
        if let Some(v) = read_env_value::<f64>(&get_env_name("top_p")) {
            self.top_p = v;
        }

        if let Some(Some(v)) = read_env_bool(&get_env_name("dry_run")) {
            self.dry_run = v;
        }
        if let Some(Some(v)) = read_env_bool(&get_env_name("stream")) {
            self.stream = v;
        }
        if let Some(Some(v)) = read_env_bool(&get_env_name("save")) {
            self.save = v;
        }
        if let Ok(v) = env::var(get_env_name("keybindings")) {
            if v == "vi" {
                self.keybindings = v;
            }
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("editor")) {
            self.editor = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("wrap")) {
            self.wrap = v;
        }
        if let Some(Some(v)) = read_env_bool(&get_env_name("wrap_code")) {
            self.wrap_code = v;
        }

        if let Some(Some(v)) = read_env_bool(&get_env_name("tool_use")) {
            self.tool_use = v;
        }
        if let Ok(v) = env::var(get_env_name("toolsets")) {
            if let Ok(v) = parse_toolsets_json(&v) {
                self.toolsets = v;
            }
        }
        if let Ok(v) = env::var(get_env_name("use_tools")) {
            if v == "null" {
                self.use_tools = None;
            } else {
                self.use_tools = Some(
                    split_tool_selectors(&v)
                        .into_iter()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect(),
                );
            }
        }

        if let Some(v) = read_env_value::<String>(&get_env_name("tui_default_session"))
            .or_else(|| read_env_value::<String>(&get_env_name("repl_default_session")))
        {
            self.tui_default_session = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("cmd_default_session")) {
            self.cmd_default_session = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("agent_default_session")) {
            self.agent_default_session = v;
        }

        if let Some(v) = read_env_bool(&get_env_name("save_session")) {
            self.save_session = v;
        }
        if let Some(Some(v)) = read_env_value::<usize>(&get_env_name("compress_threshold")) {
            self.compress_threshold = v;
        }

        if let Some(v) = read_env_value::<String>(&get_env_name("rag_embedding_model")) {
            self.rag_embedding_model = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("rag_reranker_model")) {
            self.rag_reranker_model = v;
        }
        if let Some(Some(v)) = read_env_value::<usize>(&get_env_name("rag_top_k")) {
            self.rag_top_k = v;
        }
        if let Some(v) = read_env_value::<usize>(&get_env_name("rag_chunk_size")) {
            self.rag_chunk_size = v;
        }
        if let Some(v) = read_env_value::<usize>(&get_env_name("rag_chunk_overlap")) {
            self.rag_chunk_overlap = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("rag_template")) {
            self.rag_template = v;
        }

        if let Ok(v) = env::var(get_env_name("document_loaders")) {
            if let Ok(v) = serde_json::from_str(&v) {
                self.document_loaders = v;
            }
        }

        if let Some(Some(v)) = read_env_bool(&get_env_name("highlight")) {
            self.highlight = v;
        }
        if *NO_COLOR {
            self.highlight = false;
        }
        if self.highlight && self.theme.is_none() {
            if let Some(v) = read_env_value::<String>(&get_env_name("theme")) {
                self.theme = v;
            } else if *IS_STDOUT_TERMINAL {
                if let Ok(mode) = theme_mode(QueryOptions::default()) {
                    let theme = match mode {
                        ThemeMode::Dark => "dark",
                        ThemeMode::Light => "light",
                    };
                    self.theme = Some(theme.into());
                }
            }
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("serve_addr")) {
            self.serve_addr = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("user_agent")) {
            self.user_agent = v;
        }
        if let Some(Some(v)) = read_env_bool(&get_env_name("save_shell_history")) {
            self.save_shell_history = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("sync_models_url")) {
            self.sync_models_url = v;
        }
    }

    fn needs_mcp_tools(&self) -> bool {
        self.mcp_manager.is_some()
    }

    pub fn tool_declarations_for_use_tools(&self, use_tools: Option<&str>) -> Vec<ToolDeclaration> {
        let mut declarations = self.tools.declarations();
        if let Some(use_tools) = use_tools {
            if self.needs_mcp_tools() {
                if let Some(manager) = &self.mcp_manager {
                    declarations.extend(manager.get_all_tools_blocking());
                }
            }
            if let Some(manager) = &self.acp_manager {
                declarations.extend(manager.get_all_tools_blocking());
            }
            // Only generate handoff tool declarations when the agent's use_tools
            // actually requests a *_session_handoff tool. Generating them
            // unconditionally would inject extra tool declarations into agents
            // that don't need them, changing LLM request payloads (#303).
            if split_tool_selectors(use_tools).into_iter().any(|v| {
                let v = v.trim();
                v.ends_with("_session_handoff") || v == "*"
            }) {
                declarations.extend(handoff_tool_declarations_for_agents());
            }
        }

        let mut seen = HashSet::new();
        declarations.retain(|declaration| seen.insert(declaration.name.clone()));
        declarations
    }

    fn init_mcp_manager(&mut self) {
        if self.mcp_servers.is_empty() {
            return;
        }
        let mut mcp_servers = self.mcp_servers.clone();
        let mut extra_roots = self.mcp_root.clone();
        if let Ok(cwd) = env::current_dir() {
            if let Ok(cwd_str) = cwd.into_os_string().into_string() {
                if !extra_roots.contains(&cwd_str) {
                    extra_roots.insert(0, cwd_str);
                }
            }
        }
        if !extra_roots.is_empty() {
            for server in mcp_servers.iter_mut() {
                for root in extra_roots.iter().rev() {
                    if !server.roots.contains(root) {
                        server.roots.insert(0, root.clone());
                    }
                }
            }
        }
        let manager = McpManager::new();
        manager.initialize(mcp_servers);
        self.mcp_manager = Some(Arc::new(manager));
    }

    fn init_acp_manager(&mut self) {
        if self.acp_servers.is_empty() {
            return;
        }
        let manager = AcpManager::new();
        manager.initialize(self.acp_servers.clone());
        self.acp_manager = Some(Arc::new(manager));
    }

    pub fn mcp_list_servers(config: &GlobalConfig) -> Vec<String> {
        let mcp_manager = config.read().mcp_manager.clone();
        match mcp_manager {
            Some(manager) => manager.list_servers(),
            None => vec![],
        }
    }

    fn mcp_list_servers_from_config(&self) -> Vec<String> {
        match &self.mcp_manager {
            Some(manager) => manager.list_servers(),
            None => vec![],
        }
    }

    pub async fn mcp_connect_server(config: &GlobalConfig, server_name: &str) -> Result<()> {
        let mcp_manager = config.read().mcp_manager.clone();
        match mcp_manager {
            Some(manager) => manager.connect(server_name).await,
            None => bail!("MCP is not configured"),
        }
    }

    pub async fn mcp_disconnect_server(config: &GlobalConfig, server_name: &str) -> Result<()> {
        let mcp_manager = config.read().mcp_manager.clone();
        match mcp_manager {
            Some(manager) => manager.disconnect(server_name).await,
            None => bail!("MCP is not configured"),
        }
    }

    pub fn mcp_get_roots(config: &GlobalConfig, server_name: &str) -> Result<Vec<String>> {
        let mcp_manager = config.read().mcp_manager.clone();
        match mcp_manager {
            Some(manager) => {
                let client = manager
                    .get_client(server_name)
                    .ok_or_else(|| anyhow!("MCP server '{}' not found", server_name))?;
                Ok(client.get_roots())
            }
            None => bail!("MCP is not configured"),
        }
    }

    pub async fn mcp_add_root(config: &GlobalConfig, server_name: &str, root: &str) -> Result<()> {
        let mcp_manager = config.read().mcp_manager.clone();
        match mcp_manager {
            Some(manager) => {
                let client = manager
                    .get_client(server_name)
                    .ok_or_else(|| anyhow!("MCP server '{}' not found", server_name))?;
                client.add_root(root).await
            }
            None => bail!("MCP is not configured"),
        }
    }

    pub async fn mcp_remove_root(
        config: &GlobalConfig,
        server_name: &str,
        root: &str,
    ) -> Result<()> {
        let mcp_manager = config.read().mcp_manager.clone();
        match mcp_manager {
            Some(manager) => {
                let client = manager
                    .get_client(server_name)
                    .ok_or_else(|| anyhow!("MCP server '{}' not found", server_name))?;
                client.remove_root(root).await
            }
            None => bail!("MCP is not configured"),
        }
    }

    fn setup_model(&mut self) -> Result<()> {
        let mut model_id = self.model_id.clone();
        if model_id.is_empty() {
            let models = list_models(&self.clients, ModelType::Chat);
            if models.is_empty() {
                bail!("No available model");
            }
            model_id = models[0].id()
        };
        self.set_model(&model_id)?;
        self.model_id = model_id;
        Ok(())
    }

    fn setup_document_loaders(&mut self) {
        [("pdf", "pdftotext $1 -"), ("docx", "pandoc --to plain $1")]
            .into_iter()
            .for_each(|(k, v)| {
                let (k, v) = (k.to_string(), v.to_string());
                self.document_loaders.entry(k).or_insert(v);
            });
    }

    fn setup_user_agent(&mut self) {
        if let Some("auto") = self.user_agent.as_deref() {
            self.user_agent = Some(format!(
                "{}/{}",
                env!("CARGO_CRATE_NAME"),
                env!("CARGO_PKG_VERSION")
            ));
        }
    }
}

pub fn load_env_file() -> Result<()> {
    let env_file_path = Config::env_file();
    let contents = match read_to_string(&env_file_path) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    debug!("Use env file '{}'", env_file_path.display());
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            unsafe {
                env::set_var(key.trim(), value.trim());
            }
        }
    }
    Ok(())
}

#[async_recursion::async_recursion]
pub async fn macro_execute(
    config: &GlobalConfig,
    name: &str,
    args: Option<&str>,
    abort_signal: AbortSignal,
) -> Result<()> {
    let macro_value = Config::load_macro(name)?;
    let (mut new_args, text) = split_args_text(args.unwrap_or_default(), cfg!(windows));
    if !text.is_empty() {
        new_args.push(text.to_string());
    }
    let variables = macro_value
        .resolve_variables(&new_args)
        .map_err(|err| anyhow!("{err}. Usage: {}", macro_value.usage(name)))?;
    let agent = config.read().extract_agent();
    let mut config = config.read().clone();
    config.temperature = agent.temperature();
    config.top_p = agent.top_p();
    config.use_tools = agent.use_tools();
    config.macro_flag = true;
    config.model = agent.model().clone();
    config.session = None;
    config.rag = None;
    config.agent = None;
    config.discontinuous_last_message();
    let config = Arc::new(RwLock::new(config));
    config.write().macro_flag = true;
    let mut async_manager = AsyncHookManager::new();
    let persistent_manager = std::sync::Arc::new(tokio::sync::Mutex::new(
        harnx_hooks::PersistentHookManager::new(),
    ));
    let mut pending_async_context = None;
    for step in &macro_value.steps {
        let command = Macro::interpolate_command(step, &variables);
        crate::utils::emit_info(format!(">> {}", multiline_text(&command)));
        run_command(
            &config,
            abort_signal.clone(),
            &command,
            &mut async_manager,
            &persistent_manager,
            &mut pending_async_context,
        )
        .await?;
    }
    persistent_manager.lock().await.shutdown();
    Ok(())
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

async fn create_config_file(config_path: &Path) -> Result<()> {
    let ans = Confirm::new("No config file, create a new one?")
        .with_default(true)
        .prompt()?;
    if !ans {
        process::exit(0);
    }

    let client = Select::new("API Provider (required):", list_client_types()).prompt()?;

    let (model, clients_config) = create_client_config(client).await?;
    let config = serde_json::json!({ "model": model });
    let config_data = serde_yaml::to_string(&config).with_context(|| "Failed to create config")?;
    let config_data =
        format!("# see https://github.com/dobesv/harnx/blob/main/example_config\n\n{config_data}");

    ensure_parent_exists_async(config_path).await?;
    tokio::fs::write(config_path, config_data)
        .await
        .with_context(|| format!("Failed to write to '{}'", config_path.display()))?;

    let clients_dir = config_path
        .parent()
        .unwrap_or(config_path)
        .join(paths::CLIENTS_DIR_NAME);
    tokio::fs::create_dir_all(&clients_dir)
        .await
        .with_context(|| format!("Failed to create '{}'", clients_dir.display()))?;
    let client_filename = clients_config
        .get("name")
        .or_else(|| clients_config.get("type"))
        .and_then(|value| value.as_str())
        .unwrap_or("default");
    let client_path = clients_dir.join(format!("{client_filename}.yaml"));
    let client_data =
        serde_yaml::to_string(&clients_config).with_context(|| "Failed to create client config")?;
    tokio::fs::write(&client_path, client_data)
        .await
        .with_context(|| format!("Failed to write to '{}'", client_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::prelude::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        tokio::fs::set_permissions(config_path, perms.clone()).await?;
        tokio::fs::set_permissions(&client_path, perms).await?;
    }

    crate::utils::emit_info(format!(
        "✓ Saved the config file to '{}'.",
        config_path.display()
    ));
    crate::utils::emit_info(format!(
        "✓ Saved the client config to '{}'.",
        client_path.display()
    ));

    Ok(())
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

fn complete_bool(value: bool) -> Vec<String> {
    vec![(!value).to_string()]
}

fn complete_option_bool(value: Option<bool>) -> Vec<String> {
    match value {
        Some(true) => vec!["false".to_string(), "null".to_string()],
        Some(false) => vec!["true".to_string(), "null".to_string()],
        None => vec!["true".to_string(), "false".to_string()],
    }
}

fn map_completion_values<T: ToString>(value: Vec<T>) -> Vec<(String, Option<String>)> {
    value.into_iter().map(|v| (v.to_string(), None)).collect()
}

fn update_rag<F>(config: &GlobalConfig, f: F) -> Result<()>
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
mod tests {
    use super::*;
    use harnx_core::{
        message::{MessageContent, MessageRole},
        session::ToolOutput,
        tool::ToolCall,
    };

    fn tool_calls_yaml(call_ids: &[&str]) -> String {
        serde_yaml::to_string(&SessionLogEntry::ToolCalls {
            timestamp: None,
            text: String::new(),
            thought: None,
            calls: call_ids
                .iter()
                .map(|id| ToolCall {
                    name: "bash_exec".to_string(),
                    arguments: serde_json::json!({"cmd": "echo hi"}),
                    id: Some((*id).to_string()),
                    thought_signature: None,
                })
                .collect(),
        })
        .unwrap()
    }

    fn tool_results_yaml(result_ids: &[&str]) -> String {
        tool_results_yaml_with_optional_ids(
            &result_ids
                .iter()
                .map(|id| Some((*id).to_string()))
                .collect::<Vec<_>>(),
        )
    }

    fn tool_results_yaml_with_optional_ids(result_ids: &[Option<String>]) -> String {
        serde_yaml::to_string(&SessionLogEntry::ToolResults {
            timestamp: None,
            results: result_ids
                .iter()
                .map(|id| ToolOutput {
                    id: id.clone(),
                    name: "bash_exec".to_string(),
                    output: serde_json::json!({"ok": true}),
                    switch_agent: None,
                })
                .collect(),
        })
        .unwrap()
    }

    fn user_yaml(text: &str) -> String {
        serde_yaml::to_string(&SessionLogEntry::Message {
            timestamp: None,
            role: MessageRole::User,
            content: MessageContent::Text(text.to_string()),
        })
        .unwrap()
    }

    fn assistant_yaml(text: &str) -> String {
        serde_yaml::to_string(&SessionLogEntry::Message {
            timestamp: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text(text.to_string()),
        })
        .unwrap()
    }

    #[test]
    fn validate_edited_session_documents_accepts_valid_yaml_documents() {
        let content = format!("{}---\n{}", user_yaml("hi"), user_yaml("there"));

        let documents = validate_edited_session_documents(&content).unwrap();

        assert_eq!(documents.len(), 2);
    }

    #[test]
    fn validate_edited_session_documents_rejects_invalid_yaml_documents() {
        let err = validate_edited_session_documents("---\ntype: user\ntext: [")
            .expect_err("invalid yaml should fail");

        assert!(err.to_string().contains("Invalid session log entry YAML"));
    }

    #[test]
    fn validate_tool_pair_integrity_accepts_matching_ids() {
        let documents = vec![
            tool_calls_yaml(&["call-1", "call-2"]),
            tool_results_yaml(&["call-1", "call-2"]),
        ];

        validate_tool_pair_integrity(5, &documents).unwrap();
    }

    #[test]
    fn validate_tool_pair_integrity_rejects_mismatched_ids() {
        let documents = vec![tool_calls_yaml(&["call-1"]), tool_results_yaml(&["call-2"])];

        let err =
            validate_tool_pair_integrity(7, &documents).expect_err("mismatched ids should fail");

        assert_eq!(
            err.to_string(),
            "Edited tool result at 8 references unknown tool_call_id 'call-2' (expected one of: call-1)"
        );
    }

    #[test]
    fn validate_tool_pair_integrity_rejects_missing_immediate_tool_results() {
        let documents = vec![
            tool_calls_yaml(&["call-1"]),
            user_yaml("intervening message"),
        ];

        let err = validate_tool_pair_integrity(3, &documents)
            .expect_err("tool calls without immediate tool results should fail");

        assert_eq!(
            err.to_string(),
            "Edited tool call entry at 3 must be followed immediately by matching tool results"
        );
    }

    #[test]
    fn validate_tool_pair_integrity_accepts_positional_tool_results_without_ids() {
        let documents = vec![
            tool_calls_yaml(&["call-1", "call-2"]),
            tool_results_yaml_with_optional_ids(&[None, None]),
        ];

        validate_tool_pair_integrity(4, &documents).unwrap();
    }

    #[test]
    fn validate_tool_pair_integrity_rejects_positional_tool_results_when_counts_differ() {
        let documents = vec![
            tool_calls_yaml(&["call-1", "call-2"]),
            tool_results_yaml_with_optional_ids(&[None]),
        ];

        let err = validate_tool_pair_integrity(10, &documents)
            .expect_err("count mismatch should fail positional matching");

        assert_eq!(
            err.to_string(),
            "Edited tool result at 11 is missing tool_call_id for positional matching and count 1 does not match tool calls count 2"
        );
    }

    #[test]
    fn validate_tool_pair_integrity_rejects_mixed_present_and_missing_result_ids() {
        let documents = vec![
            tool_calls_yaml(&["call-1", "call-2"]),
            tool_results_yaml_with_optional_ids(&[Some("call-1".to_string()), None]),
        ];

        let err = validate_tool_pair_integrity(12, &documents)
            .expect_err("mixed id presence should fail");

        assert_eq!(
            err.to_string(),
            "Edited tool result at 13 mixes tool_call_id values with missing tool_call_id entries"
        );
    }

    #[test]
    fn validate_tool_pair_integrity_ignores_single_non_tool_document() {
        let documents = vec![user_yaml("plain message")];

        validate_tool_pair_integrity(2, &documents).unwrap();
    }

    #[test]
    fn validate_tool_pair_integrity_allows_reordered_non_tool_documents() {
        let documents = vec![assistant_yaml("second"), user_yaml("first")];

        validate_tool_pair_integrity(1, &documents).unwrap();
    }

    #[test]
    fn edit_message_range_supports_reordering_plain_messages() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let original_editor = std::env::var_os("EDITOR");
        std::env::set_var("EDITOR", "true");

        let result = (|| -> Result<()> {
            let mut config = Config {
                sessions_dir_override: Some(tmp.path().to_path_buf()),
                working_mode: WorkingMode::Cmd,
                ..Config::default()
            };
            config
                .clients
                .push(harnx_client::ClientConfig::OpenAICompatibleConfig(
                    harnx_core::provider_config::openai_compatible::OpenAICompatibleConfig {
                        name: Some("test".to_string()),
                        api_base: None,
                        api_key: None,
                        models: vec![],
                        patch: None,
                        extra: None,
                        system_prompt_prefix: None,
                    },
                ));
            config.model = harnx_client::Model::new("test", "model");
            config.model_id = "test:model".to_string();
            config.use_session(Some("reorder"))?;

            let session = config.session.as_mut().context("No session")?;
            assert!(crate::config::session::append_event(
                session,
                &SessionLogEntry::Message {
                    timestamp: None,
                    role: MessageRole::User,
                    content: MessageContent::Text("first".to_string()),
                },
            ));
            assert!(crate::config::session::append_event(
                session,
                &SessionLogEntry::Message {
                    timestamp: None,
                    role: MessageRole::Assistant,
                    content: MessageContent::Text("second".to_string()),
                },
            ));

            let replacement_yaml = assistant_yaml("second") + "\n---\n" + &user_yaml("first");

            // Use an isolated temp dir so the after-hook can find the single
            // .yaml file without scanning the global temp directory.
            let editor_tmp = TempDir::new().unwrap();
            let editor_tmp_path = editor_tmp.path().to_path_buf();
            config.temp_dir_override = Some(editor_tmp_path.clone());

            config.set_tui_editor_hooks(
                None,
                Some(Box::new(move || {
                    let temp_path = std::fs::read_dir(&editor_tmp_path)
                        .unwrap()
                        .filter_map(|e| e.ok().map(|e| e.path()))
                        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"))
                        .expect("message edit temp file");
                    std::fs::write(&temp_path, &replacement_yaml).unwrap();
                })),
            );

            config.edit_message_range(1, 2)?;

            let reloaded = config.session.as_ref().context("No session after reload")?;
            let texts: Vec<_> = reloaded
                .messages
                .iter()
                .map(|msg| msg.content.to_text())
                .collect();
            assert_eq!(texts, vec!["second", "first"]);

            Ok(())
        })();

        match original_editor {
            Some(value) => std::env::set_var("EDITOR", value),
            None => std::env::remove_var("EDITOR"),
        }

        result.unwrap();
    }

    // --- adjust_range_for_tool_pairs ---

    fn make_docs(entries: &[SessionLogEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| serde_yaml::to_string(e).unwrap().trim().to_string())
            .collect()
    }

    fn tool_calls_entry(id: &str) -> SessionLogEntry {
        SessionLogEntry::ToolCalls {
            timestamp: None,
            text: String::new(),
            thought: None,
            calls: vec![ToolCall {
                name: "bash_exec".to_string(),
                arguments: serde_json::json!({}),
                id: Some(id.to_string()),
                thought_signature: None,
            }],
        }
    }

    fn tool_results_entry(id: &str) -> SessionLogEntry {
        SessionLogEntry::ToolResults {
            timestamp: None,
            results: vec![ToolOutput {
                id: Some(id.to_string()),
                name: "bash_exec".to_string(),
                output: serde_json::json!("ok"),
                switch_agent: None,
            }],
        }
    }

    fn user_entry(text: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            timestamp: None,
            role: MessageRole::User,
            content: MessageContent::Text(text.to_string()),
        }
    }

    #[test]
    fn adjust_range_no_tool_pairs_unchanged() {
        // [0:header, 1:user, 2:user] — no pairs, range stays as-is
        let docs = make_docs(&[user_entry("a"), user_entry("b"), user_entry("c")]);
        assert_eq!(adjust_range_for_tool_pairs(1, 2, &docs).unwrap(), (1, 2));
    }

    #[test]
    fn adjust_range_expands_to_include_paired_results() {
        // [0:user, 1:tool_calls, 2:tool_results, 3:user]
        // Requesting range [1,1] (calls only) → auto-expands to [1,2]
        let docs = make_docs(&[
            user_entry("before"),
            tool_calls_entry("c1"),
            tool_results_entry("c1"),
            user_entry("after"),
        ]);
        assert_eq!(adjust_range_for_tool_pairs(1, 1, &docs).unwrap(), (1, 2));
    }

    #[test]
    fn adjust_range_pair_already_fully_included_unchanged() {
        // [0:user, 1:tool_calls, 2:tool_results, 3:user]
        // Range [1,2] already covers both — no change
        let docs = make_docs(&[
            user_entry("before"),
            tool_calls_entry("c1"),
            tool_results_entry("c1"),
            user_entry("after"),
        ]);
        assert_eq!(adjust_range_for_tool_pairs(1, 2, &docs).unwrap(), (1, 2));
    }

    #[test]
    fn adjust_range_rejects_range_starting_on_tool_results() {
        // [0:user, 1:tool_calls, 2:tool_results, 3:user]
        // Range starting at 2 (results only) → error
        let docs = make_docs(&[
            user_entry("before"),
            tool_calls_entry("c1"),
            tool_results_entry("c1"),
            user_entry("after"),
        ]);
        let err = adjust_range_for_tool_pairs(2, 2, &docs)
            .expect_err("starting on tool-results should fail");
        assert!(err.to_string().contains("tool-results entry"));
    }

    #[test]
    fn adjust_range_tool_calls_at_end_of_log_no_expansion() {
        // [0:user, 1:tool_calls] — ToolCalls is the last doc, no results follow
        // → no expansion (orphan ToolCalls is the user's problem after editing)
        let docs = make_docs(&[user_entry("before"), tool_calls_entry("c1")]);
        assert_eq!(adjust_range_for_tool_pairs(1, 1, &docs).unwrap(), (1, 1));
    }

    #[test]
    fn adjust_range_rewind_orphan_rejected() {
        // Simulate rewind check: after_seq=1 lands on ToolCalls paired with results at 2
        // [0:user, 1:tool_calls, 2:tool_results, 3:user]
        let docs = make_docs(&[
            user_entry("before"),
            tool_calls_entry("c1"),
            tool_results_entry("c1"),
            user_entry("after"),
        ]);
        // after_seq=1 means entries 0..=1 kept, entry 2 (results) excluded → orphan calls
        // Verify the guard logic that rewind_session uses
        let parse = |idx: usize| -> Option<SessionLogEntry> {
            docs.get(idx)
                .and_then(|raw| serde_yaml::from_str::<SessionLogEntry>(raw).ok())
        };
        assert!(matches!(parse(1), Some(SessionLogEntry::ToolCalls { .. })));
        assert!(matches!(
            parse(2),
            Some(SessionLogEntry::ToolResults { .. })
        ));
        // The condition that rewind_session checks:
        let would_orphan = matches!(parse(1), Some(SessionLogEntry::ToolCalls { .. }))
            && matches!(parse(2), Some(SessionLogEntry::ToolResults { .. }));
        assert!(would_orphan);
    }

    #[test]
    fn test_split_tool_selectors_simple() {
        assert_eq!(split_tool_selectors("a,b,c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_tool_selectors_braces() {
        assert_eq!(
            split_tool_selectors("fs_{read_file,write_file},bash_exec"),
            vec!["fs_{read_file,write_file}", "bash_exec"]
        );
    }

    #[test]
    fn test_split_tool_selectors_single() {
        assert_eq!(split_tool_selectors("*"), vec!["*"]);
    }

    #[test]
    fn test_split_tool_selectors_nested_braces() {
        assert_eq!(
            split_tool_selectors("a_{b_{c,d},e},f"),
            vec!["a_{b_{c,d},e}", "f"]
        );
    }

    #[test]
    fn test_split_tool_selectors_empty() {
        assert_eq!(split_tool_selectors(""), vec![""]);
    }

    #[test]
    fn test_init_mcp_manager_with_roots() {
        let mut config = Config::default();
        let server = McpServerConfig {
            name: "test".to_string(),
            command: "ls".to_string(),
            args: vec![],
            env: HashMap::new(),
            roots: vec!["/existing".to_string()],
            enabled: true,
            description: None,
            rename_tools: HashMap::new(),
            tool_templates: HashMap::new(),
        };
        config.mcp_servers = vec![server];
        config.mcp_root = vec!["/extra".to_string()];

        config.init_mcp_manager();

        let manager = config.mcp_manager.expect("Manager should be initialized");
        let client = manager.get_client("test").expect("Client should exist");
        let roots = client.get_roots();

        // Roots should be: [cwd, /extra, /existing]
        assert_eq!(roots.len(), 3);
        let cwd = env::current_dir()
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap();
        assert_eq!(roots[0], cwd);
        assert_eq!(roots[1], "/extra");
        assert_eq!(roots[2], "/existing");
    }

    // ── handoff session emptying tests ─────────────────────────────────────

    /// Verify that empty_session clears messages from a session that was loaded
    /// with an existing name (simulating the handoff path with session_id).
    /// This is the unit-level guarantee behind the #291 fix: after handoff the
    /// new agent starts with a blank session even when a session_id was provided.
    #[test]
    fn test_new_session_has_session_id() {
        let config = Config::default();
        let session = self::session::new(&config, "metadata-check").unwrap();

        assert!(session.session_id.is_some());
    }

    #[test]
    fn test_new_session_has_uuid7_filename() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config {
            sessions_dir_override: Some(tmp.path().to_path_buf()),
            ..Config::default()
        };

        config.use_session(None).unwrap();

        let session = config.session.as_ref().unwrap();
        let parsed = Uuid::parse_str(&session.name).expect("session name should be valid UUID");
        assert_eq!(parsed.get_version_num(), 7);
        assert_eq!(
            session.sessions_dir.as_ref().unwrap().join(format!("{}.yaml", session.name)),
            tmp.path().join(format!("{}.yaml", session.name))
        );
    }

    #[test]
    fn empty_session_clears_named_session_with_messages() {
        let mut config = Config::default();
        let mut session = self::session::new(&config, "handoff-target").unwrap();
        session.push_message_for_test(MessageRole::System, "You are agent A.".to_string());
        session.push_message_for_test(MessageRole::User, "Hello from old session".to_string());
        session.push_message_for_test(MessageRole::Assistant, "Response from agent A".to_string());
        assert!(!session.is_empty());
        config.session = Some(session);

        config.empty_session().unwrap();

        let session = config.session.as_ref().unwrap();
        assert!(
            session.is_empty(),
            "session should be empty after empty_session"
        );
    }

    // ── after_chat_completion incremental persistence tests ─────────────────

    /// Verify that after_chat_completion persists intermediate rounds
    /// (non-empty tool_results) to the session, not just the final round.
    #[test]
    fn after_chat_completion_saves_intermediate_tool_rounds() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;

        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config {
            data: ConfigData {
                stream: false,
                save_session: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut session = self::session::new(&config, "test-intermediate").unwrap();
        session.set_sessions_dir(tmp.path().to_path_buf());
        config.session = Some(session);

        let _agent = config.extract_agent();
        let global_config: GlobalConfig = Arc::new(RwLock::new(config));
        let input = crate::config::input::from_str(&global_config, "do something", None);

        let tool_results = vec![ToolResult::new(
            ToolCall {
                name: "my_tool".to_string(),
                arguments: json!({"key": "val"}),
                id: Some("tc1".to_string()),
                thought_signature: None,
            },
            json!("tool output"),
        )];

        // Call after_chat_completion with non-empty tool_results.
        // Previously this returned early without saving; now it should persist.
        global_config
            .write()
            .after_chat_completion(
                &input,
                "intermediate output",
                None,
                &tool_results,
                &Default::default(),
            )
            .unwrap();

        let config_guard = global_config.read();
        let session = config_guard.session.as_ref().unwrap();
        assert!(
            !session.is_empty(),
            "session should have messages after intermediate round"
        );
        // Verify content via the session's export (which serializes messages).
        let export = session.export().unwrap();
        assert!(
            export.contains("intermediate output"),
            "session export should contain assistant output; got:\n{export}"
        );
        assert!(
            export.contains("my_tool"),
            "session export should contain tool call info; got:\n{export}"
        );
    }

    // ── compact_session tests ────────────────────────────────────────────────

    /// Helper: create a GlobalConfig with a session that already has one user
    /// message in it, suitable for compaction tests.
    fn make_config_with_session() -> GlobalConfig {
        let mut config = Config {
            data: ConfigData {
                stream: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut session = self::session::new(&config, "test-session").unwrap();
        session.push_message_for_test(
            MessageRole::User,
            "Tell me about the Rust ownership model.".to_string(),
        );
        config.session = Some(session);
        Arc::new(RwLock::new(config))
    }

    /// compact_session (no compaction_agent) must send the session history to
    /// the LLM — i.e. the user message from the conversation must appear in
    /// the ChatCompletionsData that the mock receives.
    #[tokio::test]
    async fn test_compact_session_default_includes_session_history() {
        use crate::client::TestStateGuard;
        use crate::test_utils::{MockClient, MockTurnBuilder};

        let mock = Arc::new(
            MockClient::builder()
                .add_turn(MockTurnBuilder::new().add_text_chunk("Summary.").build())
                .build(),
        );
        let _guard = TestStateGuard::new(Some(mock.clone())).await;
        let config = make_config_with_session();

        Config::compact_session(&config).await.unwrap();

        let history = mock.conversation_history();
        assert_eq!(
            history.conversation_history.len(),
            1,
            "expected exactly one LLM call"
        );
        let messages = &history.conversation_history[0].messages;
        let has_history = messages.iter().any(|m| {
            if let MessageContent::Text(t) = &m.content {
                t.contains("Rust ownership model")
            } else {
                false
            }
        });
        assert!(
            has_history,
            "session history must be forwarded to the compaction LLM; messages: {messages:?}"
        );
    }

    /// compact_session with a compaction_agent must also send the session
    /// history — `set_agent` must not drop `with_session`.
    #[tokio::test]
    async fn test_compact_session_with_compaction_agent_includes_session_history() {
        use crate::client::TestStateGuard;
        use crate::test_utils::{MockClient, MockTurnBuilder};
        use std::io::Write as _;

        // Write a minimal compaction agent file to a temp dir and point the
        // config's agents directory at it via HARNX_CONFIG_DIR.
        let temp = tempfile::TempDir::new().unwrap();
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        let agent_content = "---\nmodel: gemini:gemini-3.1-flash-lite-preview\n---\nYou are a specialized compaction agent. Produce a concise summary.\n";
        let mut f = std::fs::File::create(agents_dir.join("my-compactor.md")).unwrap();
        f.write_all(agent_content.as_bytes()).unwrap();

        // Build a config where the current (non-session) agent has
        // compaction_agent = "my-compactor".
        let mut main_agent = Agent::new(AgentConfig::from_markdown(
            "main",
            "---\nmodel: gemini:gemini-3.1-flash-lite-preview\ncompaction_agent: my-compactor\n---\nYou are the main agent.",
        ).unwrap());
        main_agent.set_model(crate::client::Model::new(
            "gemini",
            "gemini-3.1-flash-lite-preview",
        ));

        let mut config = Config {
            data: ConfigData {
                stream: false,
                ..Default::default()
            },
            ..Default::default()
        };
        // Point Config::agent_file() at the temp dir via HARNX_CONFIG_DIR.
        // Use an RAII guard so the env var is restored even on panic.  The
        // guard is created *after* `TestStateGuard` acquires the global test
        // lock so concurrent tests cannot race on the env var.
        struct EnvGuard {
            key: &'static str,
            prev: Option<std::ffi::OsString>,
        }
        impl EnvGuard {
            fn new(key: &'static str, value: &std::path::Path) -> Self {
                let prev = std::env::var_os(key);
                // SAFETY: test-only; concurrent env mutation is prevented by
                // holding `TEST_CLIENT_LOCK` while the guard is alive.
                unsafe { std::env::set_var(key, value) };
                Self { key, prev }
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.prev {
                    Some(v) => unsafe { std::env::set_var(self.key, v) },
                    None => unsafe { std::env::remove_var(self.key) },
                }
            }
        }

        config.agent = Some(main_agent);

        let mut session = self::session::new(&config, "test-session").unwrap();
        session.push_message_for_test(
            MessageRole::User,
            "Tell me about the Rust ownership model.".to_string(),
        );
        config.session = Some(session);
        let config = Arc::new(RwLock::new(config));

        let mock = Arc::new(
            MockClient::builder()
                .add_turn(MockTurnBuilder::new().add_text_chunk("Compacted.").build())
                .build(),
        );
        let _guard = TestStateGuard::new(Some(mock.clone())).await;
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());

        Config::compact_session(&config).await.unwrap();

        let history = mock.conversation_history();
        assert_eq!(
            history.conversation_history.len(),
            1,
            "expected exactly one LLM call"
        );
        let messages = &history.conversation_history[0].messages;

        // The session history must be present.
        let has_history = messages.iter().any(|m| {
            if let MessageContent::Text(t) = &m.content {
                t.contains("Rust ownership model")
            } else {
                false
            }
        });
        assert!(
            has_history,
            "session history must be forwarded even when a compaction_agent is configured; messages: {messages:?}"
        );

        // The compaction agent's system prompt must also be present.
        let has_system = messages.iter().any(|m| {
            m.role == MessageRole::System
                && if let MessageContent::Text(t) = &m.content {
                    t.contains("specialized compaction agent")
                } else {
                    false
                }
        });
        assert!(
            has_system,
            "compaction agent's system prompt must be in the messages; messages: {messages:?}"
        );
    }

    /// Regression test for the ACP-server failure where `use_agent_by_name`
    /// followed by `use_session` bailed with "agent variables are required"
    /// for an agent whose variables use `path:` (file-backed defaults).  The
    /// async `agent::init` resolves these defaults, but the synchronous
    /// `retrieve_agent` does not — `use_agent_by_name` must do so itself,
    /// otherwise `init_agent_session_variables` (called from `use_session`)
    /// finds no defaults and bails in non-interactive contexts like ACP.
    #[tokio::test]
    async fn test_use_agent_by_name_resolves_file_backed_variable_defaults() {
        use crate::client::TestStateGuard;

        let temp = tempfile::TempDir::new().unwrap();
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(agents_dir.join("shared")).unwrap();
        std::fs::write(
            agents_dir.join("file-backed-vars.md"),
            "---\nvariables:\n  - name: prompt_body\n    description: Shared prompt\n    path: shared/prompt.md\n---\n{{prompt_body}}\n",
        )
        .unwrap();
        std::fs::write(agents_dir.join("shared/prompt.md"), "Loaded body").unwrap();

        struct EnvGuard {
            key: &'static str,
            prev: Option<std::ffi::OsString>,
        }
        impl EnvGuard {
            fn new(key: &'static str, value: &std::path::Path) -> Self {
                let prev = std::env::var_os(key);
                unsafe { std::env::set_var(key, value) };
                Self { key, prev }
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.prev {
                    Some(v) => unsafe { std::env::set_var(self.key, v) },
                    None => unsafe { std::env::remove_var(self.key) },
                }
            }
        }

        // Hold the global test lock so concurrent tests can't race on the
        // shared HARNX_CONFIG_DIR env var.
        let _guard = TestStateGuard::new(None).await;
        let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());

        // Drive use_session in non-interactive mode so the inquire prompt
        // that would otherwise hang in CI is suppressed.  The fix must still
        // produce populated shared_variables under no_interaction.
        let mut config = Config {
            info_flag: true,
            ..Default::default()
        };
        config
            .use_agent_by_name("file-backed-vars")
            .expect("use_agent_by_name must resolve path-backed variable defaults");
        config
            .use_session(Some("file-backed-vars-session"))
            .expect("use_session must succeed once defaults are resolved");

        let agent = config.agent.as_ref().expect("agent should be set");
        assert_eq!(
            agent
                .shared_variables()
                .get("prompt_body")
                .map(String::as_str),
            Some("Loaded body"),
            "shared_variables should be populated from the file-backed default"
        );
    }
}
