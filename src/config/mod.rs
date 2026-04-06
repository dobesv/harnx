mod agent;
mod input;
mod session;

pub use self::agent::{complete_agent_variables, list_agents, Agent, AgentVariables};
pub use self::agent::{CREATE_TITLE_AGENT, TEMP_AGENT_NAME};
pub use self::input::Input;
use self::session::Session;

use crate::acp::{AcpManager, AcpServerConfig};
use crate::client::{
    create_client_config, list_client_types, list_models, ClientConfig, MessageContentToolCalls,
    Model, ModelType, ProviderModels, OPENAI_COMPATIBLE_PROVIDERS,
};
use crate::hooks::{AsyncHookManager, HooksConfig};
use crate::mcp::{McpManager, McpServerConfig};
use crate::rag::Rag;
use crate::render::{MarkdownRender, RenderOptions};
use crate::repl::{run_repl_command, split_args_text};
use crate::tool::{ToolDeclaration, ToolResult, Tools};
use crate::utils::*;

use anyhow::{anyhow, bail, Context, Result};
use globset::GlobBuilder;
use indexmap::IndexMap;
use inquire::{list_option::ListOption, validator::Validation, Confirm, MultiSelect, Select, Text};
use parking_lot::RwLock;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use simplelog::LevelFilter;
use std::collections::{HashMap, HashSet};
use std::{
    env,
    fs::{
        create_dir_all, read_dir, read_to_string, remove_dir_all, remove_file, File, OpenOptions,
    },
    io::Write,
    path::{Path, PathBuf},
    process,
    sync::{Arc, OnceLock},
};
use syntect::highlighting::ThemeSet;
use terminal_colorsaurus::{color_scheme, ColorScheme, QueryOptions};

pub const TEMP_RAG_NAME: &str = "temp";
pub const TEMP_SESSION_NAME: &str = "temp";

/// Monokai Extended
const DARK_THEME: &[u8] = include_bytes!("../../assets/monokai-extended.theme.bin");
const LIGHT_THEME: &[u8] = include_bytes!("../../assets/monokai-extended-light.theme.bin");

const CONFIG_FILE_NAME: &str = "config.yaml";
const MACROS_DIR_NAME: &str = "macros";
const ENV_FILE_NAME: &str = ".env";
const MESSAGES_FILE_NAME: &str = "messages.md";
const SESSIONS_DIR_NAME: &str = "sessions";
const RAGS_DIR_NAME: &str = "rags";
const AGENTS_DIR_NAME: &str = "agents";

const CLIENTS_FIELD: &str = "clients";

const SERVE_ADDR: &str = "127.0.0.1:8000";

const SYNC_MODELS_URL: &str =
    "https://raw.githubusercontent.com/dobesv/harnx/refs/heads/main/models.yaml";

const SUMMARIZE_PROMPT: &str =
    "Summarize the discussion briefly in 200 words or less to use as a prompt for future context.";
const SUMMARY_PROMPT: &str = "This is a summary of the chat history as a recap: ";

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

const LEFT_PROMPT: &str = "{color.cyan}>{color.reset} ";
const RIGHT_PROMPT: &str = "";

static EDITOR: OnceLock<Option<String>> = OnceLock::new();

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ToolsetValue {
    String(String),
    Array(Vec<String>),
}

fn normalize_toolset_value(value: ToolsetValue) -> Vec<String> {
    match value {
        ToolsetValue::String(value) => split_tool_selectors(&value)
            .into_iter()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect(),
        ToolsetValue::Array(values) => values,
    }
}

fn deserialize_toolsets<'de, D>(
    deserializer: D,
) -> std::result::Result<IndexMap<String, Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = IndexMap::<String, ToolsetValue>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .map(|(key, value)| (key, normalize_toolset_value(value)))
        .collect())
}

fn parse_toolsets_json(value: &str) -> serde_json::Result<IndexMap<String, Vec<String>>> {
    let values = serde_json::from_str::<IndexMap<String, ToolsetValue>>(value)?;
    Ok(values
        .into_iter()
        .map(|(key, value)| (key, normalize_toolset_value(value)))
        .collect())
}

/// Split a comma-separated string of tool selectors while respecting `{…}` brace groups.
///
/// A comma inside braces (e.g. `fs_{read_file,write_file}`) is *not* treated as a separator.
fn split_tool_selectors(input: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut start = 0;
    let mut depth: usize = 0;
    for (i, ch) in input.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(&input[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(&input[start..]);
    items
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

/// Deserializes `use_tools` accepting both a YAML list and a comma-separated string.
fn deserialize_use_tools<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<ToolsetValue>::deserialize(deserializer)?;
    Ok(value.map(normalize_toolset_value))
}

#[derive(Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(rename(serialize = "model", deserialize = "model"))]
    #[serde(default)]
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,

    pub dry_run: bool,
    pub stream: bool,
    pub save: bool,
    pub keybindings: String,
    pub editor: Option<String>,
    pub wrap: Option<String>,
    pub wrap_code: bool,

    pub tool_use: bool,
    #[serde(default)]
    #[serde(alias = "mapping_tools")]
    #[serde(deserialize_with = "deserialize_toolsets")]
    pub toolsets: IndexMap<String, Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_use_tools")]
    pub use_tools: Option<Vec<String>>,

    pub repl_default_session: Option<String>,
    pub cmd_default_session: Option<String>,
    pub agent_default_session: Option<String>,

    pub save_session: Option<bool>,
    pub compress_threshold: usize,
    pub summarize_prompt: Option<String>,
    pub summary_prompt: Option<String>,

    pub rag_embedding_model: Option<String>,
    pub rag_reranker_model: Option<String>,
    pub rag_top_k: usize,
    pub rag_chunk_size: Option<usize>,
    pub rag_chunk_overlap: Option<usize>,
    pub rag_template: Option<String>,

    #[serde(default)]
    pub document_loaders: HashMap<String, String>,

    pub highlight: bool,
    pub theme: Option<String>,
    pub left_prompt: Option<String>,
    pub right_prompt: Option<String>,

    pub serve_addr: Option<String>,
    pub user_agent: Option<String>,
    pub save_shell_history: bool,
    pub sync_models_url: Option<String>,

    pub clients: Vec<ClientConfig>,

    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,

    #[serde(default)]
    pub acp_servers: Vec<AcpServerConfig>,

    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HooksConfig>,

    #[serde(skip)]
    pub macro_flag: bool,
    #[serde(skip)]
    pub info_flag: bool,
    #[serde(skip)]
    pub agent_variables: Option<AgentVariables>,
    #[serde(skip)]
    pub mcp_root: Vec<String>,

    #[serde(skip)]
    pub model: Model,
    #[serde(skip)]
    pub tools: Tools,
    #[serde(skip)]
    pub mcp_manager: Option<Arc<McpManager>>,
    #[serde(skip)]
    pub acp_manager: Option<Arc<AcpManager>>,
    #[serde(skip)]
    pub working_mode: WorkingMode,
    #[serde(skip)]
    pub last_message: Option<LastMessage>,

    #[serde(skip)]
    pub session: Option<Session>,
    #[serde(skip)]
    pub rag: Option<Arc<Rag>>,
    #[serde(skip)]
    pub agent: Option<Agent>,
    #[serde(skip)]
    pub tui_before_editor: Option<Box<dyn FnMut() + Send + Sync>>,
    #[serde(skip)]
    pub tui_after_editor: Option<Box<dyn FnMut() + Send + Sync>>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("model_id", &self.model_id)
            .field("temperature", &self.temperature)
            .field("top_p", &self.top_p)
            .field("dry_run", &self.dry_run)
            .field("stream", &self.stream)
            .field("save", &self.save)
            .field("keybindings", &self.keybindings)
            .field("editor", &self.editor)
            .field("wrap", &self.wrap)
            .field("wrap_code", &self.wrap_code)
            .field("tool_use", &self.tool_use)
            .field("toolsets", &self.toolsets)
            .field("use_tools", &self.use_tools)
            .field("repl_default_session", &self.repl_default_session)
            .field("cmd_default_session", &self.cmd_default_session)
            .field("agent_default_session", &self.agent_default_session)
            .field("save_session", &self.save_session)
            .field("compress_threshold", &self.compress_threshold)
            .field("summarize_prompt", &self.summarize_prompt)
            .field("summary_prompt", &self.summary_prompt)
            .field("rag_embedding_model", &self.rag_embedding_model)
            .field("rag_reranker_model", &self.rag_reranker_model)
            .field("rag_top_k", &self.rag_top_k)
            .field("rag_chunk_size", &self.rag_chunk_size)
            .field("rag_chunk_overlap", &self.rag_chunk_overlap)
            .field("rag_template", &self.rag_template)
            .field("document_loaders", &self.document_loaders)
            .field("highlight", &self.highlight)
            .field("theme", &self.theme)
            .field("left_prompt", &self.left_prompt)
            .field("right_prompt", &self.right_prompt)
            .field("serve_addr", &self.serve_addr)
            .field("user_agent", &self.user_agent)
            .field("save_shell_history", &self.save_shell_history)
            .field("sync_models_url", &self.sync_models_url)
            .field("clients", &self.clients)
            .field("mcp_servers", &self.mcp_servers)
            .field("acp_servers", &self.acp_servers)
            .field("hooks", &self.hooks)
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
            model_id: self.model_id.clone(),
            temperature: self.temperature,
            top_p: self.top_p,
            dry_run: self.dry_run,
            stream: self.stream,
            save: self.save,
            keybindings: self.keybindings.clone(),
            editor: self.editor.clone(),
            wrap: self.wrap.clone(),
            wrap_code: self.wrap_code,
            tool_use: self.tool_use,
            toolsets: self.toolsets.clone(),
            use_tools: self.use_tools.clone(),
            repl_default_session: self.repl_default_session.clone(),
            cmd_default_session: self.cmd_default_session.clone(),
            agent_default_session: self.agent_default_session.clone(),
            save_session: self.save_session,
            compress_threshold: self.compress_threshold,
            summarize_prompt: self.summarize_prompt.clone(),
            summary_prompt: self.summary_prompt.clone(),
            rag_embedding_model: self.rag_embedding_model.clone(),
            rag_reranker_model: self.rag_reranker_model.clone(),
            rag_top_k: self.rag_top_k,
            rag_chunk_size: self.rag_chunk_size,
            rag_chunk_overlap: self.rag_chunk_overlap,
            rag_template: self.rag_template.clone(),
            document_loaders: self.document_loaders.clone(),
            highlight: self.highlight,
            theme: self.theme.clone(),
            left_prompt: self.left_prompt.clone(),
            right_prompt: self.right_prompt.clone(),
            serve_addr: self.serve_addr.clone(),
            user_agent: self.user_agent.clone(),
            save_shell_history: self.save_shell_history,
            sync_models_url: self.sync_models_url.clone(),
            clients: self.clients.clone(),
            mcp_servers: self.mcp_servers.clone(),
            acp_servers: self.acp_servers.clone(),
            hooks: self.hooks.clone(),
            macro_flag: self.macro_flag,
            info_flag: self.info_flag,
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
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model_id: Default::default(),
            temperature: None,
            top_p: None,

            dry_run: false,
            stream: true,
            save: false,
            keybindings: "emacs".into(),
            editor: None,
            wrap: None,
            wrap_code: false,

            tool_use: true,
            toolsets: Default::default(),
            use_tools: None,

            repl_default_session: None,
            cmd_default_session: None,
            agent_default_session: None,

            save_session: None,
            compress_threshold: 180000,
            summarize_prompt: None,
            summary_prompt: None,

            rag_embedding_model: None,
            rag_reranker_model: None,
            rag_top_k: 5,
            rag_chunk_size: None,
            rag_chunk_overlap: None,
            rag_template: None,

            document_loaders: Default::default(),

            highlight: true,
            theme: None,
            left_prompt: None,
            right_prompt: None,

            serve_addr: None,
            user_agent: None,
            save_shell_history: true,
            sync_models_url: None,

            clients: vec![],
            mcp_servers: vec![],
            acp_servers: vec![],

            hooks: None,

            macro_flag: false,
            info_flag: false,
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
        if let Ok(v) = env::var(get_env_name("config_dir")) {
            PathBuf::from(v)
        } else if let Ok(v) = env::var("XDG_CONFIG_HOME") {
            PathBuf::from(v).join(env!("CARGO_CRATE_NAME"))
        } else {
            let dir = dirs::config_dir().expect("No user's config directory");
            dir.join(env!("CARGO_CRATE_NAME"))
        }
    }

    pub fn local_path(name: &str) -> PathBuf {
        Self::config_dir().join(name)
    }

    pub fn config_file() -> PathBuf {
        match env::var(get_env_name("config_file")) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::local_path(CONFIG_FILE_NAME),
        }
    }

    pub fn macros_dir() -> PathBuf {
        match env::var(get_env_name("macros_dir")) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::local_path(MACROS_DIR_NAME),
        }
    }

    pub fn macro_file(name: &str) -> PathBuf {
        Self::macros_dir().join(format!("{name}.yaml"))
    }

    pub fn env_file() -> PathBuf {
        match env::var(get_env_name("env_file")) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::local_path(ENV_FILE_NAME),
        }
    }

    pub fn messages_file(&self) -> PathBuf {
        match &self.agent {
            None => match env::var(get_env_name("messages_file")) {
                Ok(value) => PathBuf::from(value),
                Err(_) => Self::local_path(MESSAGES_FILE_NAME),
            },
            Some(agent) => Self::agent_data_dir(agent.name()).join(MESSAGES_FILE_NAME),
        }
    }

    pub fn sessions_dir(&self) -> PathBuf {
        match &self.agent {
            None => match env::var(get_env_name("sessions_dir")) {
                Ok(value) => PathBuf::from(value),
                Err(_) => Self::local_path(SESSIONS_DIR_NAME),
            },
            Some(agent) => Self::agent_data_dir(agent.name()).join(SESSIONS_DIR_NAME),
        }
    }

    pub fn rags_dir() -> PathBuf {
        match env::var(get_env_name("rags_dir")) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::local_path(RAGS_DIR_NAME),
        }
    }

    pub fn session_file(&self, name: &str) -> PathBuf {
        match name.split_once("/") {
            Some((dir, name)) => self.sessions_dir().join(dir).join(format!("{name}.yaml")),
            None => self.sessions_dir().join(format!("{name}.yaml")),
        }
    }

    pub fn rag_file(&self, name: &str) -> PathBuf {
        match &self.agent {
            Some(agent) => Self::agent_rag_file(agent.name(), name),
            None => Self::rags_dir().join(format!("{name}.yaml")),
        }
    }

    pub fn agents_data_dir() -> PathBuf {
        Self::local_path(AGENTS_DIR_NAME)
    }

    pub fn agent_data_dir(name: &str) -> PathBuf {
        match env::var(format!("{}_DATA_DIR", normalize_env_name(name))) {
            Ok(value) => PathBuf::from(value),
            Err(_) => Self::agents_data_dir().join(name),
        }
    }

    pub fn agent_rag_file(agent_name: &str, rag_name: &str) -> PathBuf {
        Self::agent_data_dir(agent_name).join(format!("{rag_name}.yaml"))
    }

    pub fn agent_file(name: &str) -> PathBuf {
        Self::agents_data_dir().join(format!("{name}.md"))
    }

    pub fn models_override_file() -> PathBuf {
        Self::local_path("models-override.yaml")
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

    pub fn edit_config(&self) -> Result<()> {
        let config_path = Self::config_file();
        let editor = self.editor()?;
        edit_file(&editor, &config_path)?;
        println!(
            "NOTE: Remember to restart {} if there are changes made to '{}",
            env!("CARGO_CRATE_NAME"),
            config_path.display(),
        );
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
            session.to_agent()
        } else if let Some(agent) = self.agent.as_ref() {
            agent.clone()
        } else {
            let mut agent = Agent::from_prompt("");
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
        println!("✓ Successfully deleted {kind}.");
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
            Model::retrieve_model(&config.read(), id, ModelType::Reranker)?;
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
        let model = Model::retrieve_model(self, model_id, ModelType::Chat)?;
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
        let mut agent = Agent::from_prompt(prompt);
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
            Agent::load(&path)?
        } else {
            Agent::builtin(name)?
        };
        let current_model = self.current_model().clone();
        match agent.model_id() {
            Some(model_id) => {
                if current_model.id() != model_id {
                    let model = Model::retrieve_model(self, model_id, ModelType::Chat)?;
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
        let agent = self.retrieve_agent(name)?;
        self.use_agent_obj(agent)
    }

    pub fn use_agent_obj(&mut self, agent: Agent) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            session.guard_empty()?;
            session.set_agent(&agent);
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
        let editor = self.editor()?;
        edit_file(&editor, &agent_path)?;
        if self.working_mode.is_repl() {
            println!("✓ Saved the agent to '{}'.", agent_path.display());
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
            if self.working_mode.is_repl() {
                println!("✓ Saved the agent to '{}'.", agent_path.display());
            }
        }

        Ok(())
    }

    pub fn all_agents() -> Vec<Agent> {
        let mut agents: HashMap<String, Agent> = HashMap::new();
        for name in list_agents() {
            let path = Self::agent_file(&name);
            if let Ok(agent) = Agent::load(&path) {
                agents.insert(name, agent);
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
            None | Some(TEMP_SESSION_NAME) => {
                let session_file = self.session_file(TEMP_SESSION_NAME);
                if session_file.exists() {
                    remove_file(session_file).with_context(|| {
                        format!("Failed to cleanup previous '{TEMP_SESSION_NAME}' session")
                    })?;
                }
                session = Some(Session::new(self, TEMP_SESSION_NAME));
            }
            Some(name) => {
                let session_path = self.session_file(name);
                if !session_path.exists() {
                    session = Some(Session::new(self, name));
                } else {
                    session = Some(Session::load(self, name, &session_path)?);
                }
            }
        }
        let mut new_session = false;
        if let Some(session) = session.as_mut() {
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
                            session.add_message(input, output, None)?;
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
            session.render(&mut markdown_render, &agent_info)
        } else {
            bail!("No session")
        }
    }

    pub fn exit_session(&mut self) -> Result<()> {
        if let Some(mut session) = self.session.take() {
            let sessions_dir = self.sessions_dir();
            session.exit(&sessions_dir, self.working_mode.is_repl())?;
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
            session.save(&session_name, &session_path, self.working_mode.is_repl())?;
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

    pub fn edit_session(&mut self) -> Result<()> {
        let name = match &self.session {
            Some(session) => session.name().to_string(),
            None => bail!("No session"),
        };
        let session_path = self.session_file(&name);
        self.save_session(Some(&name))?;
        let editor = self.editor()?;
        if let Some(before) = self.tui_before_editor.as_mut() {
            before();
        }
        let edit_result = edit_file(&editor, &session_path).with_context(|| {
            format!(
                "Failed to edit '{}' with '{editor}'",
                session_path.display()
            )
        });
        if let Some(after) = self.tui_after_editor.as_mut() {
            after();
        }
        edit_result?;
        self.session = Some(Session::load(self, &name, &session_path)?);
        self.discontinuous_last_message();
        Ok(())
    }

    pub fn empty_session(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_mut() {
            if let Some(agent) = self.agent.as_ref() {
                session.sync_agent(agent);
            }
            session.clear_messages();
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
                WorkingMode::Repl => self.repl_default_session.as_ref(),
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

    pub fn list_autoname_sessions(&self) -> Vec<String> {
        list_file_names(self.sessions_dir().join("_"), ".yaml")
    }

    pub fn maybe_compress_session(config: GlobalConfig) {
        let mut need_compress = false;
        {
            let mut config = config.write();
            let compress_threshold = config.compress_threshold;
            if let Some(session) = config.session.as_mut() {
                if session.need_compress(compress_threshold) {
                    session.set_compressing(true);
                    need_compress = true;
                }
            }
        };
        if !need_compress {
            return;
        }
        let color = if config.read().light_theme() {
            nu_ansi_term::Color::LightGray
        } else {
            nu_ansi_term::Color::DarkGray
        };
        print!(
            "\n📢 {}\n",
            color.italic().paint("Compressing the session."),
        );
        tokio::spawn(async move {
            if let Err(err) = Config::compress_session(&config).await {
                warn!("Failed to compress the session: {err}");
            }
            if let Some(session) = config.write().session.as_mut() {
                session.set_compressing(false);
            }
        });
    }

    pub async fn compress_session(config: &GlobalConfig) -> Result<()> {
        match config.read().session.as_ref() {
            Some(session) => {
                if !session.has_user_messages() {
                    bail!("No need to compress since there are no messages in the session")
                }
            }
            None => bail!("No session"),
        }

        let prompt = config
            .read()
            .summarize_prompt
            .clone()
            .unwrap_or_else(|| SUMMARIZE_PROMPT.into());
        let input = Input::from_str(config, &prompt, None);
        let summary = input.fetch_chat_text().await?;
        let summary_prompt = config
            .read()
            .summary_prompt
            .clone()
            .unwrap_or_else(|| SUMMARY_PROMPT.into());
        if let Some(session) = config.write().session.as_mut() {
            session.compress(format!("{summary_prompt}{summary}"));
        }
        config.write().discontinuous_last_message();
        Ok(())
    }

    pub fn is_compressing_session(&self) -> bool {
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
        print!("\n📢 {}\n", color.italic().paint("Autonaming the session."),);
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
        let input = Input::from_str(config, &text, Some(agent));
        let text = input.fetch_chat_text().await?;
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
                Rag::init(config, TEMP_RAG_NAME, &rag_path, &[], abort_signal).await?
            }
            Some(name) => {
                let rag_path = config.read().rag_file(name);
                if !rag_path.exists() {
                    if config.read().working_mode.is_cmd() {
                        bail!("Unknown RAG '{name}'")
                    }
                    Rag::init(config, name, &rag_path, &[], abort_signal).await?
                } else {
                    Rag::load(config, name, &rag_path)?
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
        let editor = config.read().editor()?;
        edit_file(&editor, &temp_file)?;
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
        rag.refresh_document_paths(&new_document_paths, false, config, abort_signal)
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
        rag.refresh_document_paths(&document_paths, true, config, abort_signal)
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
        let (embeddings, ids) = rag
            .search(text, top_k, reranker_model.as_deref(), abort_signal)
            .await?;
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
        let agent = Agent::init(config, agent_name, abort_signal).await?;
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
        if let Some(session) = session {
            // Exit any existing session (e.g. from repl_default_session) before
            // switching to the agent's session.
            config.write().exit_session()?;
            config.write().use_session(Some(&session))?;
        } else {
            config.write().init_agent_shared_variables()?;
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
            if self.working_mode.is_repl() {
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
            let editor = self.editor()?;
            edit_file(&editor, &macro_path)?;
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
            WorkingMode::Repl => self.repl_default_session.as_ref(),
            WorkingMode::Cmd => self.cmd_default_session.as_ref(),
            WorkingMode::Serve => return Ok(()),
            WorkingMode::Acp(_) => return Ok(()),
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

    pub fn select_tools(&self, agent: &Agent) -> Option<Vec<ToolDeclaration>> {
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

    pub fn repl_complete(
        &self,
        cmd: &str,
        args: &[&str],
        _line: &str,
    ) -> Vec<(String, Option<String>)> {
        let mut values: Vec<(String, Option<String>)> = vec![];
        let filter = args.last().unwrap_or(&"");
        if args.len() == 1 {
            values = match cmd {
                ".model" => list_models(self, ModelType::Chat)
                    .into_iter()
                    .map(|v| (v.id(), Some(v.description())))
                    .collect(),
                ".session" => {
                    if args[0].starts_with("_/") {
                        map_completion_values(
                            self.list_autoname_sessions()
                                .iter()
                                .rev()
                                .map(|v| format!("_/{v}"))
                                .collect::<Vec<String>>(),
                        )
                    } else {
                        map_completion_values(self.list_sessions())
                    }
                }
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
                "rag_reranker_model" => list_models(self, ModelType::Reranker)
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
                let dir = Self::agent_data_dir(args[0]).join(SESSIONS_DIR_NAME);
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
        println!("✓ Fetched '{url}'");
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
        println!("✓ Updated '{}'", model_override_path.display());
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
                let theme = if self.light_theme() {
                    decode_bin(LIGHT_THEME).context("Invalid builtin light theme")?
                } else {
                    decode_bin(DARK_THEME).context("Invalid builtin dark theme")?
                };
                Some(theme)
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

    pub fn render_prompt_left(&self) -> String {
        let variables = self.generate_prompt_context();
        let left_prompt = self.left_prompt.as_deref().unwrap_or(LEFT_PROMPT);
        render_prompt(left_prompt, &variables)
    }

    pub fn render_prompt_right(&self) -> String {
        let variables = self.generate_prompt_context();
        let right_prompt = self.right_prompt.as_deref().unwrap_or(RIGHT_PROMPT);
        render_prompt(right_prompt, &variables)
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
        if *IS_STDOUT_TERMINAL {
            let render_options = self.render_options()?;
            let mut markdown_render = MarkdownRender::init(render_options)?;
            println!("{}", markdown_render.render(text));
        } else {
            println!("{text}");
        }
        Ok(())
    }

    fn generate_prompt_context(&self) -> HashMap<&str, String> {
        let mut output = HashMap::new();
        let agent = self.extract_agent();
        output.insert("model", agent.model().id());
        output.insert("client_name", agent.model().client_name().to_string());
        output.insert("model_name", agent.model().name().to_string());
        output.insert(
            "max_input_tokens",
            agent
                .model()
                .max_input_tokens()
                .unwrap_or_default()
                .to_string(),
        );
        if let Some(temperature) = agent.temperature() {
            if temperature != 0.0 {
                output.insert("temperature", temperature.to_string());
            }
        }
        if let Some(top_p) = agent.top_p() {
            if top_p != 0.0 {
                output.insert("top_p", top_p.to_string());
            }
        }
        if self.dry_run {
            output.insert("dry_run", "true".to_string());
        }
        if self.stream {
            output.insert("stream", "true".to_string());
        }
        if self.save {
            output.insert("save", "true".to_string());
        }
        if let Some(wrap) = &self.wrap {
            if wrap != "no" {
                output.insert("wrap", wrap.clone());
            }
        }
        if agent.name() != TEMP_AGENT_NAME {
            output.insert("agent", agent.name().to_string());
        }
        if let Some(session) = &self.session {
            output.insert("session", session.name().to_string());
            if let Some(autoname) = session.autoname() {
                output.insert("session_autoname", autoname.to_string());
            }
            output.insert("dirty", session.dirty().to_string());
            let (tokens, percent) = session.tokens_usage();
            output.insert("consume_tokens", tokens.to_string());
            output.insert("consume_percent", percent.to_string());
            output.insert("user_messages_len", session.user_messages_len().to_string());
        }
        if let Some(rag) = &self.rag {
            output.insert("rag", rag.name().to_string());
        }
        if let Some(agent) = &self.agent {
            output.insert("agent", agent.name().to_string());
        }

        if self.highlight {
            output.insert("color.reset", "\u{1b}[0m".to_string());
            output.insert("color.black", "\u{1b}[30m".to_string());
            output.insert("color.dark_gray", "\u{1b}[90m".to_string());
            output.insert("color.red", "\u{1b}[31m".to_string());
            output.insert("color.light_red", "\u{1b}[91m".to_string());
            output.insert("color.green", "\u{1b}[32m".to_string());
            output.insert("color.light_green", "\u{1b}[92m".to_string());
            output.insert("color.yellow", "\u{1b}[33m".to_string());
            output.insert("color.light_yellow", "\u{1b}[93m".to_string());
            output.insert("color.blue", "\u{1b}[34m".to_string());
            output.insert("color.light_blue", "\u{1b}[94m".to_string());
            output.insert("color.purple", "\u{1b}[35m".to_string());
            output.insert("color.light_purple", "\u{1b}[95m".to_string());
            output.insert("color.magenta", "\u{1b}[35m".to_string());
            output.insert("color.light_magenta", "\u{1b}[95m".to_string());
            output.insert("color.cyan", "\u{1b}[36m".to_string());
            output.insert("color.light_cyan", "\u{1b}[96m".to_string());
            output.insert("color.white", "\u{1b}[37m".to_string());
            output.insert("color.light_gray", "\u{1b}[97m".to_string());
        }

        output
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
        if !tool_results.is_empty() {
            return Ok(());
        }
        self.last_message = Some(LastMessage::new(input.clone(), output.to_string()));
        if !self.dry_run {
            self.save_message(input, output, thought)?;
        }
        Ok(())
    }

    fn discontinuous_last_message(&mut self) {
        if let Some(last_message) = self.last_message.as_mut() {
            last_message.continuous = false;
        }
    }

    fn save_message(&mut self, input: &Input, output: &str, thought: Option<&str>) -> Result<()> {
        let mut input = input.clone();
        input.clear_patch();
        if let Some(session) = input.session_mut(&mut self.session) {
            session.add_message(&input, output, thought)?;
            return Ok(());
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
            let new_variables = Agent::init_agent_variables(
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
                    let new_variables = Agent::init_agent_variables(
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
            session.sync_agent(agent);
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
        let content = read_to_string(config_path).with_context(err)?;
        let config: Self = serde_yaml::from_str(&content)
            .map_err(|err| {
                let err_msg = err.to_string();
                let err_msg = if err_msg.starts_with(&format!("{CLIENTS_FIELD}: ")) {
                    // location is incorrect, get rid of it
                    err_msg
                        .split_once(" at line")
                        .map(|(v, _)| {
                            format!("{v} (Sorry for being unable to provide an exact location)")
                        })
                        .unwrap_or_else(|| "clients: invalid value".into())
                } else {
                    err_msg
                };
                anyhow!("{err_msg}")
            })
            .with_context(err)?;

        Ok(config)
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
        let config = json!({
            "model": model_id.to_string(),
            "save": false,
            "clients": vec![client],
        });
        let config =
            serde_json::from_value(config).with_context(|| "Failed to load config from env")?;
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

        if let Some(v) = read_env_value::<String>(&get_env_name("repl_default_session")) {
            self.repl_default_session = v;
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
        if let Some(v) = read_env_value::<String>(&get_env_name("summarize_prompt")) {
            self.summarize_prompt = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("summary_prompt")) {
            self.summary_prompt = v;
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
                if let Ok(color_scheme) = color_scheme(QueryOptions::default()) {
                    let theme = match color_scheme {
                        ColorScheme::Dark => "dark",
                        ColorScheme::Light => "light",
                    };
                    self.theme = Some(theme.into());
                }
            }
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("left_prompt")) {
            self.left_prompt = v;
        }
        if let Some(v) = read_env_value::<String>(&get_env_name("right_prompt")) {
            self.right_prompt = v;
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
            if self.acp_manager.is_some() {
                if let Some(manager) = &self.acp_manager {
                    declarations.extend(manager.get_all_tools_blocking());
                }
            }
            if split_tool_selectors(use_tools).into_iter().any(|v| {
                let v = v.trim();
                v == crate::tool::TRIGGER_AGENT_TOOL_NAME
                    || matches_tool_glob(v, crate::tool::TRIGGER_AGENT_TOOL_NAME)
            }) {
                declarations.push(crate::tool::trigger_agent_tool_declaration());
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
            let models = list_models(self, ModelType::Chat);
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkingMode {
    Cmd,
    Repl,
    Serve,
    Acp(String),
}

impl WorkingMode {
    pub fn is_cmd(&self) -> bool {
        matches!(self, WorkingMode::Cmd)
    }
    pub fn is_repl(&self) -> bool {
        matches!(self, WorkingMode::Repl)
    }
    pub fn is_serve(&self) -> bool {
        matches!(self, WorkingMode::Serve)
    }
    pub fn is_acp(&self) -> bool {
        matches!(self, WorkingMode::Acp(_))
    }
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
        crate::hooks::PersistentHookManager::new(),
    ));
    let mut pending_async_context = None;
    for step in &macro_value.steps {
        let command = Macro::interpolate_command(step, &variables);
        println!(">> {}", multiline_text(&command));
        run_repl_command(
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

#[derive(Debug, Clone, Deserialize)]
pub struct Macro {
    #[serde(default)]
    pub variables: Vec<MacroVariable>,
    pub steps: Vec<String>,
}

impl Macro {
    pub fn resolve_variables(&self, args: &[String]) -> Result<IndexMap<String, String>> {
        let mut output = IndexMap::new();
        for (i, variable) in self.variables.iter().enumerate() {
            let value = if variable.rest && i == self.variables.len() - 1 {
                if args.len() > i {
                    Some(args[i..].join(" "))
                } else {
                    variable.default.clone()
                }
            } else {
                args.get(i)
                    .map(|v| v.to_string())
                    .or_else(|| variable.default.clone())
            };
            let value =
                value.ok_or_else(|| anyhow!("Missing value for variable '{}'", variable.name))?;
            output.insert(variable.name.clone(), value);
        }
        Ok(output)
    }

    pub fn usage(&self, name: &str) -> String {
        let mut parts = vec![name.to_string()];
        for (i, variable) in self.variables.iter().enumerate() {
            let part = match (
                variable.rest && i == self.variables.len() - 1,
                variable.default.is_some(),
            ) {
                (true, true) => format!("[{}]...", variable.name),
                (true, false) => format!("<{}>...", variable.name),
                (false, true) => format!("[{}]", variable.name),
                (false, false) => format!("<{}>", variable.name),
            };
            parts.push(part);
        }
        parts.join(" ")
    }

    pub fn interpolate_command(command: &str, variables: &IndexMap<String, String>) -> String {
        let mut output = command.to_string();
        for (key, value) in variables {
            output = output.replace(&format!("{{{{{key}}}}}"), value);
        }
        output
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MacroVariable {
    pub name: String,
    #[serde(default)]
    pub rest: bool,
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsOverride {
    pub version: String,
    pub list: Vec<ProviderModels>,
}

#[derive(Debug, Clone)]
pub struct LastMessage {
    pub input: Input,
    pub output: String,
    pub continuous: bool,
}

impl LastMessage {
    pub fn new(input: Input, output: String) -> Self {
        Self {
            input,
            output,
            continuous: true,
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssertState {
    True(StateFlags),
    False(StateFlags),
    TrueFalse(StateFlags, StateFlags),
    Equal(StateFlags),
}

impl AssertState {
    pub fn pass() -> Self {
        AssertState::False(StateFlags::empty())
    }

    pub fn bare() -> Self {
        AssertState::Equal(StateFlags::empty())
    }

    pub fn assert(self, flags: StateFlags) -> bool {
        match self {
            AssertState::True(true_flags) => true_flags & flags != StateFlags::empty(),
            AssertState::False(false_flags) => false_flags & flags == StateFlags::empty(),
            AssertState::TrueFalse(true_flags, false_flags) => {
                (true_flags & flags != StateFlags::empty())
                    && (false_flags & flags == StateFlags::empty())
            }
            AssertState::Equal(check_flags) => check_flags == flags,
        }
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

    let mut config = serde_json::json!({});
    let (model, clients_config) = create_client_config(client).await?;
    config["model"] = model.into();
    config[CLIENTS_FIELD] = clients_config;

    let config_data = serde_yaml::to_string(&config).with_context(|| "Failed to create config")?;
    let config_data = format!(
        "# see https://github.com/dobesv/harnx/blob/main/config.example.yaml\n\n{config_data}"
    );

    ensure_parent_exists(config_path)?;
    std::fs::write(config_path, config_data)
        .with_context(|| format!("Failed to write to '{}'", config_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::prelude::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(config_path, perms)?;
    }

    println!("✓ Saved the config file to '{}'.\n", config_path.display());

    Ok(())
}

pub(crate) fn ensure_parent_exists(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Failed to write to '{}', No parent path", path.display()))?;
    if !parent.exists() {
        create_dir_all(parent).with_context(|| {
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
}
