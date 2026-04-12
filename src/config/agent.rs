use super::*;

use crate::{
    client::{Message, MessageContent, MessageRole, Model},
    tool::Tools,
};

use anyhow::{anyhow, Context, Result};
use fancy_regex::Regex;
use inquire::{validator::Validation, Text};
use serde::{Deserialize, Serialize};
use std::{
    fs::{read_dir, read_to_string},
    path::Path,
    sync::LazyLock,
};

pub const TEMP_AGENT_NAME: &str = "%%";

fn default_retry_attempts() -> u32 {
    3
}
fn default_initial_delay_ms() -> u64 {
    1000
}
fn default_max_delay_ms() -> u64 {
    60000
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryConfig {
    #[serde(default = "default_retry_attempts")]
    pub attempts: u32,
    #[serde(default = "default_initial_delay_ms")]
    pub initial_delay_ms: u64,
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            attempts: default_retry_attempts(),
            initial_delay_ms: default_initial_delay_ms(),
            max_delay_ms: default_max_delay_ms(),
        }
    }
}

pub const CREATE_TITLE_AGENT: &str = "%create-title%";

const CREATE_TITLE_PROMPT: &str = r#"Create a concise, 3-6 word title.

**Notes**:
- Avoid quotation marks or emojis
- RESPOND ONLY WITH TITLE SLUG TEXT

**Examples**:
stock-market-trends
perfect-chocolate-chip-recipe
remote-work-productivity-tips
video-game-development-insights"#;

const DEFAULT_AGENT_NAME: &str = "rag";
pub type AgentVariables = IndexMap<String, String>;

static RE_METADATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)-{3,}\s*(.*?)\s*-{3,}\s*(.*)").unwrap());

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Agent {
    name: String,
    #[serde(
        rename(serialize = "model", deserialize = "model"),
        skip_serializing_if = "Option::is_none"
    )]
    model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    model_fallbacks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry: Option<RetryConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::deserialize_use_tools"
    )]
    use_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    variables: Vec<AgentVariable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    conversation_starters: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    documents: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_default_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hooks: Option<HooksConfig>,
    #[serde(default)]
    prompt: String,

    #[serde(skip, default)]
    shared_variables: AgentVariables,
    #[serde(skip, default)]
    session_variables: Option<AgentVariables>,
    #[serde(skip, default)]
    tools: Tools,
    #[serde(skip, default)]
    rag: Option<Arc<Rag>>,
    #[serde(skip, default)]
    model: Model,
    #[serde(skip, default)]
    mcp_manager: Option<Arc<McpManager>>,
}

impl Agent {
    pub fn from_markdown(name: &str, content: &str) -> Self {
        let mut metadata = "";
        let mut prompt = content.trim();
        if let Ok(Some(caps)) = RE_METADATA.captures(content) {
            if let (Some(metadata_value), Some(prompt_value)) = (caps.get(1), caps.get(2)) {
                metadata = metadata_value.as_str().trim();
                prompt = prompt_value.as_str().trim();
            }
        }
        let mut prompt = prompt.to_string();
        interpolate_variables(&mut prompt);
        let frontmatter = if metadata.is_empty() {
            AgentFrontMatter::default()
        } else {
            serde_yaml::from_str::<AgentFrontMatter>(metadata).unwrap_or_default()
        };
        Self {
            name: name.to_string(),
            model_id: frontmatter.model_id,
            model_fallbacks: frontmatter.model_fallbacks,
            retry: frontmatter.retry,
            temperature: frontmatter.temperature,
            top_p: frontmatter.top_p,
            use_tools: frontmatter.use_tools,
            description: frontmatter.description,
            version: frontmatter.version,
            variables: frontmatter.variables,
            conversation_starters: frontmatter.conversation_starters,
            documents: frontmatter.documents,
            agent_default_session: frontmatter.agent_default_session,
            instructions: frontmatter.instructions,
            hooks: frontmatter.hooks,
            prompt,
            ..Default::default()
        }
    }

    pub fn from_prompt(prompt: &str) -> Self {
        let mut agent = Self::from_markdown(TEMP_AGENT_NAME, prompt);
        agent.name = TEMP_AGENT_NAME.to_string();
        agent
    }

    pub fn builtin(name: &str) -> Result<Self> {
        let content = match name {
            CREATE_TITLE_AGENT => CREATE_TITLE_PROMPT,
            _ => bail!("Unknown built-in agent `{name}`"),
        };
        Ok(Self::from_markdown(name, content))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents = read_to_string(path)
            .with_context(|| format!("Failed to read agent file at '{}'", path.display()))?;
        let name = path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("Invalid agent file name: '{}'", path.display()))?;
        Ok(Self::from_markdown(name, &contents))
    }

    pub async fn init(
        config: &GlobalConfig,
        name: &str,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        let agent_file_path = Config::agent_file(name);
        let mut agent = if agent_file_path.exists() {
            Self::load(&agent_file_path)?
        } else {
            Self::builtin(name)?
        };

        let mcp_manager = config.read().mcp_manager.clone();
        agent.mcp_manager = mcp_manager.clone();

        let mcp_tools = match &mcp_manager {
            Some(manager) => Some(manager.get_all_tools().await),
            None => None,
        };
        agent.tools = Tools::init_from_mcp(mcp_tools);

        let model = {
            let config = config.read();
            match agent.model_id.as_ref() {
                Some(model_id) => Model::retrieve_model(&config, model_id, ModelType::Chat)?,
                None => {
                    if agent.temperature.is_none() {
                        agent.temperature = config.temperature;
                    }
                    if agent.top_p.is_none() {
                        agent.top_p = config.top_p;
                    }
                    config.current_model().clone()
                }
            }
        };
        agent.model = model;

        let rag_path = Config::agent_rag_file(name, DEFAULT_AGENT_NAME);
        let agent_dir = agent_file_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(Config::agents_data_dir);

        for variable in &mut agent.variables {
            if let Some(path_str) = &variable.path {
                if variable.default.is_some() {
                    log::warn!(
                        "Variable '{}': both 'path' and 'default' set, using 'path'",
                        variable.name
                    );
                }

                let resolved_path = safe_join_path(&agent_dir, path_str).ok_or_else(|| {
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

        agent.rag = if rag_path.exists() {
            Some(Arc::new(Rag::load(config, DEFAULT_AGENT_NAME, &rag_path)?))
        } else if !agent.documents.is_empty() && !config.read().info_flag {
            let mut ans = false;
            if *IS_STDOUT_TERMINAL {
                ans = Confirm::new("The agent has the documents, init RAG?")
                    .with_default(true)
                    .prompt()?;
            }
            if ans {
                let mut document_paths = vec![];
                for path in &agent.documents {
                    if is_url(path) {
                        document_paths.push(path.to_string());
                    } else {
                        let new_path = safe_join_path(&agent_dir, path)
                            .ok_or_else(|| anyhow!("Invalid document path: '{path}'"))?;
                        document_paths.push(new_path.display().to_string())
                    }
                }
                let rag =
                    Rag::init(config, "rag", &rag_path, &document_paths, abort_signal).await?;
                Some(Arc::new(rag))
            } else {
                None
            }
        } else {
            None
        };

        Ok(agent)
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
            match variables.get(&key) {
                Some(value) => {
                    output.insert(key, value.clone());
                }
                None => {
                    if let Some(value) = agent_variable.default.clone() {
                        output.insert(key, value);
                        continue;
                    }
                    if no_interaction {
                        continue;
                    }
                    if *IS_STDOUT_TERMINAL {
                        if !printed {
                            println!("⚙ Init agent variables...");
                            printed = true;
                        }
                        let value = Text::new(&format!(
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
                        .prompt()?;
                        output.insert(key, value);
                    } else {
                        unset_variables.push(agent_variable)
                    }
                }
            }
        }
        if !unset_variables.is_empty() {
            bail!(
                "The following agent variables are required:\n{}",
                unset_variables
                    .iter()
                    .map(|v| format!("  - {}: {}", v.name, v.description))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
        Ok(output)
    }

    pub fn export(&self) -> Result<String> {
        let metadata = AgentFrontMatter::from_agent(self);
        if metadata.is_empty() {
            Ok(format!("{}\n", self.prompt))
        } else {
            let metadata = serialize_frontmatter(&metadata)?;
            if self.prompt.is_empty() {
                Ok(format!("---\n{}\n---\n", metadata))
            } else {
                Ok(format!("---\n{}\n---\n\n{}\n", metadata, self.prompt))
            }
        }
    }

    pub fn banner(&self) -> String {
        let starters = if self.conversation_starters.is_empty() {
            String::new()
        } else {
            let starters = self
                .conversation_starters
                .iter()
                .map(|v| format!("- {v}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                r#"

## Conversation Starters
{starters}"#
            )
        };
        format!(
            r#"# {} v{}
{}{}"#,
            self.name, self.version, self.description, starters
        )
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_name(&mut self, name: &str) {
        self.name = name.to_string();
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    pub fn temperature(&self) -> Option<f64> {
        self.temperature
    }

    pub fn top_p(&self) -> Option<f64> {
        self.top_p
    }

    pub fn model_id(&self) -> Option<&str> {
        self.model_id.as_deref()
    }

    pub fn model_fallbacks(&self) -> &[String] {
        &self.model_fallbacks
    }

    pub fn retry_config(&self) -> RetryConfig {
        self.retry.clone().unwrap_or_default()
    }

    #[cfg(test)]
    pub fn set_model_fallbacks(&mut self, fallbacks: Vec<String>) {
        self.model_fallbacks = fallbacks;
    }

    pub fn use_tools(&self) -> Option<Vec<String>> {
        self.use_tools.clone()
    }

    pub fn hooks(&self) -> Option<&HooksConfig> {
        self.hooks.as_ref()
    }

    pub fn has_args(&self) -> bool {
        self.name.contains('#')
    }

    pub fn set_model(&mut self, model: Model) {
        self.model_id = Some(model.id());
        self.model = model;
    }

    pub fn set_temperature(&mut self, value: Option<f64>) {
        self.temperature = value;
    }

    pub fn set_top_p(&mut self, value: Option<f64>) {
        self.top_p = value;
    }

    pub fn set_use_tools(&mut self, value: Option<Vec<String>>) {
        self.use_tools = value;
    }

    pub fn echo_messages(&self, input: &Input) -> String {
        let prompt = self.interpolated_instructions();
        let tools_text = self.tools_text();
        let input_markdown = input.render();

        let base = if prompt.is_empty() {
            if let Some(tools) = &tools_text {
                format!("{tools}\n\n{input_markdown}")
            } else {
                input_markdown
            }
        } else if let Some(tools) = &tools_text {
            format!("{prompt}\n\n{tools}\n\n{input_markdown}")
        } else {
            format!("{}\n\n{}", prompt, input_markdown)
        };
        base
    }

    pub fn build_messages(&self, input: &Input) -> Vec<Message> {
        let prompt = self.interpolated_instructions();
        let tools_text = self.tools_text();
        let content = input.message_content();
        let mut messages = if prompt.is_empty() {
            let mut messages = vec![];
            if let Some(tools_text) = &tools_text {
                messages.push(Message::new(
                    MessageRole::System,
                    MessageContent::Text(tools_text.clone()),
                ));
            }
            messages.push(Message::new(MessageRole::User, content));
            messages
        } else {
            let mut messages = vec![];
            let system_text = match (&tools_text, prompt.is_empty()) {
                (Some(tools), false) => format!("{prompt}\n\n{tools}"),
                (Some(tools), true) => tools.clone(),
                (None, false) => prompt.to_string(),
                (None, true) => String::new(),
            };
            if !system_text.is_empty() {
                messages.push(Message::new(
                    MessageRole::System,
                    MessageContent::Text(system_text),
                ));
            }
            messages.push(Message::new(MessageRole::User, content));
            messages
        };
        if let Some(text) = input.continue_output() {
            messages.push(Message::new(
                MessageRole::Assistant,
                MessageContent::Text(text.into()),
            ));
        }
        messages
    }

    pub fn tools(&self) -> &Tools {
        &self.tools
    }

    pub fn rag(&self) -> Option<Arc<Rag>> {
        self.rag.clone()
    }

    pub fn conversation_staters(&self) -> &[String] {
        &self.conversation_starters
    }

    pub fn interpolated_instructions(&self) -> String {
        let mut output = self
            .instructions
            .clone()
            .unwrap_or_else(|| self.prompt.clone());
        for (k, v) in self.variables() {
            output = output.replace(&format!("{{{{{k}}}}}"), v)
        }
        interpolate_variables(&mut output);
        output
    }

    pub fn agent_default_session(&self) -> Option<&str> {
        self.agent_default_session.as_deref()
    }

    pub fn variables(&self) -> &AgentVariables {
        match &self.session_variables {
            Some(variables) => variables,
            None => &self.shared_variables,
        }
    }

    pub fn shared_variables(&self) -> &AgentVariables {
        &self.shared_variables
    }

    pub fn set_shared_variables(&mut self, shared_variables: AgentVariables) {
        self.shared_variables = shared_variables;
    }

    pub fn set_session_variables(&mut self, session_variables: AgentVariables) {
        self.session_variables = Some(session_variables);
    }

    pub fn defined_variables(&self) -> &[AgentVariable] {
        &self.variables
    }

    pub fn exit_session(&mut self) {
        self.session_variables = None;
    }

    fn tools_text(&self) -> Option<String> {
        let declarations = self.tools.declarations();
        if declarations.is_empty() {
            return None;
        }
        let tools = declarations
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let description = match v.description.split_once('\n') {
                    Some((first_line, _)) => first_line,
                    None => &v.description,
                };
                format!("{}. {}: {description}", i + 1, v.name)
            })
            .collect::<Vec<String>>()
            .join("\n");
        Some(tools)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct AgentFrontMatter {
    #[serde(
        rename(serialize = "model", deserialize = "model"),
        skip_serializing_if = "Option::is_none"
    )]
    model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    model_fallbacks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry: Option<RetryConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::deserialize_use_tools"
    )]
    use_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    variables: Vec<AgentVariable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    conversation_starters: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    documents: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_default_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hooks: Option<HooksConfig>,
}

impl AgentFrontMatter {
    fn from_agent(agent: &Agent) -> Self {
        Self {
            model_id: agent.model_id.clone(),
            model_fallbacks: agent.model_fallbacks.clone(),
            retry: agent.retry.clone(),
            temperature: agent.temperature,
            top_p: agent.top_p,
            use_tools: agent.use_tools.clone(),
            description: agent.description.clone(),
            version: agent.version.clone(),
            variables: agent.variables.clone(),
            conversation_starters: agent.conversation_starters.clone(),
            documents: agent.documents.clone(),
            agent_default_session: agent.agent_default_session.clone(),
            instructions: agent.instructions.clone(),
            hooks: agent.hooks.clone(),
        }
    }

    fn is_empty(&self) -> bool {
        self.model_id.is_none()
            && self.model_fallbacks.is_empty()
            && self.retry.is_none()
            && self.temperature.is_none()
            && self.top_p.is_none()
            && self.use_tools.is_none()
            && self.description.is_empty()
            && self.version.is_empty()
            && self.variables.is_empty()
            && self.conversation_starters.is_empty()
            && self.documents.is_empty()
            && self.agent_default_session.is_none()
            && self.instructions.is_none()
            && self.hooks.is_none()
    }
}

fn serialize_frontmatter(frontmatter: &AgentFrontMatter) -> Result<String> {
    let output = serde_yaml::to_string(frontmatter)?;
    Ok(output
        .strip_prefix("---\n")
        .unwrap_or(&output)
        .trim()
        .to_string())
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AgentVariable {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[allow(dead_code)]
    #[serde(skip_serializing, skip_deserializing, default)]
    pub value: String,
}

pub fn list_agents() -> Vec<String> {
    let mut output: Vec<String> = match read_dir(Config::agents_data_dir()) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let path = entry.path();
                match path.extension().and_then(|value| value.to_str()) {
                    Some("md") => path
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .map(|value| value.to_string()),
                    _ => None,
                }
            })
            .collect(),
        Err(_) => vec![],
    };
    output.sort();
    output.dedup();
    output
}

pub fn complete_agent_variables(agent_name: &str) -> Vec<(String, Option<String>)> {
    let markdown_path = Config::agents_data_dir().join(format!("{agent_name}.md"));
    if markdown_path.exists() {
        if let Ok(agent) = Agent::load(&markdown_path) {
            return agent
                .variables
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GlobalConfig;
    use crate::utils::create_abort_signal;
    use std::{
        fs,
        path::Path,
        path::PathBuf,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEST_CONFIG_DIR_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn unique_test_config_dir() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "harnx-agent-test-{}-{timestamp}",
            std::process::id()
        ))
    }

    fn with_test_config_dir<T>(f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
        let _guard = TEST_CONFIG_DIR_LOCK.lock().unwrap();
        let config_dir = unique_test_config_dir();
        let agents_dir = config_dir.join("agents");
        fs::create_dir_all(&agents_dir)?;

        unsafe {
            std::env::set_var("HARNX_CONFIG_DIR", &config_dir);
        }
        let result = f(&config_dir);
        unsafe {
            std::env::remove_var("HARNX_CONFIG_DIR");
        }

        let cleanup_result = fs::remove_dir_all(&config_dir);
        match (result, cleanup_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(err)) => Err(err.into()),
            (Err(err), Ok(())) => Err(err),
            (Err(err), Err(cleanup_err)) => Err(err.context(format!(
                "Additionally failed to clean up test config dir '{}': {cleanup_err}",
                config_dir.display()
            ))),
        }
    }

    fn init_test_agent(agent_name: &str, content: &str, files: &[(&str, &str)]) -> Result<Agent> {
        with_test_config_dir(|config_dir| {
            let agents_dir = config_dir.join("agents");
            fs::write(agents_dir.join(format!("{agent_name}.md")), content)?;

            for (relative_path, file_content) in files {
                let path = agents_dir.join(relative_path);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, file_content)?;
            }

            let config = GlobalConfig::default();
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(Agent::init(&config, agent_name, create_abort_signal()))
        })
    }

    fn make_tool_declaration(name: &str, description: &str) -> crate::tool::ToolDeclaration {
        crate::tool::ToolDeclaration {
            name: name.to_string(),
            description: description.to_string(),
            parameters: Default::default(),
            mcp_tool_name: None,
        }
    }

    fn make_agent_with_tools(prompt: &str, tools: Vec<crate::tool::ToolDeclaration>) -> Agent {
        let mut agent = Agent::from_markdown("test", prompt);
        agent.tools =
            crate::tool::Tools::init_from_mcp(if tools.is_empty() { None } else { Some(tools) });
        agent
    }

    #[test]
    fn test_agent_from_markdown_full() {
        let content = "---\nmodel: openai:gpt-4o\ntemperature: 0.7\ntop_p: 0.9\nuse_tools: fs,web_search\ndescription: A test agent\nversion: '1.0'\n---\nYou are a helpful test agent.";
        let agent = Agent::from_markdown("test-agent", content);
        assert_eq!(agent.name(), "test-agent");
        assert_eq!(agent.model_id(), Some("openai:gpt-4o"));
        assert_eq!(agent.temperature(), Some(0.7));
        assert_eq!(agent.top_p(), Some(0.9));
        assert_eq!(
            agent.use_tools(),
            Some(vec!["fs".to_string(), "web_search".to_string()])
        );
        assert!(agent
            .interpolated_instructions()
            .contains("You are a helpful test agent"));
    }

    #[test]
    fn test_agent_from_markdown_minimal() {
        let content = "Just instructions, no front-matter.";
        let agent = Agent::from_markdown("minimal", content);
        assert_eq!(agent.name(), "minimal");
        assert!(agent.model_id().is_none());
        assert!(agent.temperature().is_none());
        assert_eq!(
            agent.interpolated_instructions(),
            "Just instructions, no front-matter."
        );
    }

    #[test]
    fn test_agent_from_markdown_empty_body() {
        let content = "---\nmodel: openai:gpt-4o\ntemperature: 0.5\n---\n";
        let agent = Agent::from_markdown("empty-body", content);
        assert_eq!(agent.name(), "empty-body");
        assert_eq!(agent.model_id(), Some("openai:gpt-4o"));
        assert!(agent.interpolated_instructions().is_empty());
    }

    #[test]
    fn test_agent_set_name() {
        let mut agent = Agent::from_prompt("You are a test agent.");
        assert_eq!(agent.name(), "%%");
        agent.set_name("new-name");
        assert_eq!(agent.name(), "new-name");
    }

    #[test]
    fn test_agent_from_prompt() {
        let agent = Agent::from_prompt("You are a pirate");
        assert_eq!(agent.name(), "%%");
        assert!(agent
            .interpolated_instructions()
            .contains("You are a pirate"));
        assert!(agent.model_id().is_none());
        assert!(agent.temperature().is_none());
    }

    #[test]
    fn test_agent_builtin_create_title() {
        let agent = Agent::builtin("%create-title%").unwrap();
        assert_eq!(agent.name(), "%create-title%");
        assert!(!agent.interpolated_instructions().is_empty());
        assert!(agent.interpolated_instructions().contains("concise"));
    }

    #[test]
    fn test_agent_builtin_unknown() {
        let result = Agent::builtin("unknown-agent");
        assert!(result.is_err());
    }

    #[test]
    fn test_agent_from_markdown_with_use_tools() {
        let content = "---\nuse_tools: fs_*,bash_exec\n---\nHelp with files.";
        let agent = Agent::from_markdown("tools-agent", content);
        assert_eq!(
            agent.use_tools(),
            Some(vec!["fs_*".to_string(), "bash_exec".to_string()])
        );
    }

    #[test]
    fn test_tools_text_with_tools() {
        let agent = make_agent_with_tools(
            "prompt",
            vec![
                make_tool_declaration("tool_a", "Description A"),
                make_tool_declaration("tool_b", "Description B"),
            ],
        );

        let text = agent.tools_text();

        assert_eq!(
            text,
            Some("1. tool_a: Description A\n2. tool_b: Description B".to_string())
        );
    }

    #[test]
    fn test_tools_text_without_tools() {
        let agent = make_agent_with_tools("prompt", vec![]);

        assert_eq!(agent.tools_text(), None);
    }

    #[test]
    fn test_tools_text_multiline_description() {
        let agent = make_agent_with_tools(
            "prompt",
            vec![make_tool_declaration(
                "tool_x",
                "First line\nSecond line\nThird line",
            )],
        );

        let text = agent.tools_text();

        assert_eq!(text, Some("1. tool_x: First line".to_string()));
    }

    #[test]
    fn test_export_does_not_contain_tool_text() {
        let agent = make_agent_with_tools(
            "You are a helpful assistant.",
            vec![make_tool_declaration("my_tool", "Tool description")],
        );

        let exported = agent.export().unwrap();

        assert!(!exported.contains("my_tool"));
        assert!(!exported.contains("Tool description"));
        assert!(exported.contains("You are a helpful assistant."));
    }

    #[test]
    fn test_build_messages_always_uses_system_and_user_format() {
        let config = GlobalConfig::default();
        let agent = Agent::from_prompt(
            "System message\n__INPUT__\n\n### INPUT:\nExample input\n### OUTPUT:\nExample output",
        );
        let input = Input::from_str(&config, "Real input", Some(agent));

        let messages = input.agent().build_messages(&input);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::System);
        assert_eq!(messages[1].role, MessageRole::User);
        assert_eq!(
            messages[0].content.to_text(),
            "System message\n__INPUT__\n\n### INPUT:\nExample input\n### OUTPUT:\nExample output"
        );
        assert_eq!(messages[1].content.to_text(), "Real input");
    }

    #[test]
    fn test_agent_variable_path_deserialization() {
        let yaml = r#"name: prompt
description: Shared prompt
path: shared/prompt.md
"#;

        let variable: AgentVariable = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(variable.name, "prompt");
        assert_eq!(variable.description, "Shared prompt");
        assert_eq!(variable.path.as_deref(), Some("shared/prompt.md"));
        assert!(variable.default.is_none());
        assert!(variable.value.is_empty());
    }

    #[test]
    fn test_agent_variable_path_serialization() {
        let variable = AgentVariable {
            name: "prompt".to_string(),
            description: "Shared prompt".to_string(),
            default: None,
            path: Some("shared/prompt.md".to_string()),
            value: "runtime-only".to_string(),
        };

        let yaml = serde_yaml::to_string(&variable).unwrap();
        let round_trip: AgentVariable = serde_yaml::from_str(&yaml).unwrap();

        assert!(yaml.contains("path: shared/prompt.md"));
        assert!(!yaml.contains("value:"));
        assert_eq!(round_trip.name, "prompt");
        assert_eq!(round_trip.description, "Shared prompt");
        assert_eq!(round_trip.path.as_deref(), Some("shared/prompt.md"));
        assert!(round_trip.default.is_none());
        assert!(round_trip.value.is_empty());
    }

    #[test]
    fn test_agent_variable_without_path() {
        let yaml = r#"name: prompt
description: Shared prompt
"#;

        let variable: AgentVariable = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(variable.name, "prompt");
        assert_eq!(variable.description, "Shared prompt");
        assert!(variable.path.is_none());
        assert!(variable.default.is_none());
        assert!(variable.value.is_empty());
    }

    #[test]
    fn test_agent_variable_with_path() {
        let agent = init_test_agent(
            "path-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    path: shared/prompt.md
---
You are a test agent.
"#,
            &[("shared/prompt.md", "Loaded prompt")],
        )
        .unwrap();

        assert_eq!(agent.variables[0].default.as_deref(), Some("Loaded prompt"));
    }

    #[test]
    fn test_agent_variable_path_missing_file() {
        let error = init_test_agent(
            "missing-path-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    path: shared/missing.md
---
You are a test agent.
"#,
            &[],
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("prompt"));
        assert!(message.contains("shared/missing.md"));
    }

    #[test]
    fn test_agent_variable_path_traversal_rejected() {
        let error = init_test_agent(
            "traversal-path-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    path: ../../../etc/passwd
---
You are a test agent.
"#,
            &[],
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("prompt"));
        assert!(message.contains("../../../etc/passwd"));
        assert!(message.contains("not allowed"));
    }

    #[test]
    fn test_agent_variable_path_absolute_rejected() {
        let error = init_test_agent(
            "absolute-path-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    path: /etc/passwd
---
You are a test agent.
"#,
            &[],
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("prompt"));
        assert!(message.contains("/etc/passwd"));
        assert!(message.contains("not allowed"));
    }

    #[test]
    fn test_agent_variable_path_empty_file() {
        let agent = init_test_agent(
            "empty-path-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    path: shared/empty.md
---
You are a test agent.
"#,
            &[("shared/empty.md", "")],
        )
        .unwrap();

        assert_eq!(agent.variables[0].default.as_deref(), Some(""));
    }

    #[test]
    fn test_agent_variable_path_and_default_uses_path() {
        let agent = init_test_agent(
            "path-and-default-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    default: Inline prompt
    path: shared/prompt.md
---
You are a test agent.
"#,
            &[("shared/prompt.md", "Loaded from file")],
        )
        .unwrap();

        assert_eq!(
            agent.variables[0].default.as_deref(),
            Some("Loaded from file")
        );
    }

    #[test]
    fn test_agent_variable_path_nested_relative_file() {
        let agent = init_test_agent(
            "nested-relative-path-variable",
            r#"---
variables:
  - name: prompt
    description: Shared prompt
    path: shared/nested/prompt.md
---
You are a test agent.
"#,
            &[("shared/nested/prompt.md", "Nested prompt")],
        )
        .unwrap();

        assert_eq!(agent.variables[0].default.as_deref(), Some("Nested prompt"));
    }
}
