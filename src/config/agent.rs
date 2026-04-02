use super::*;

use crate::{
    client::{Message, MessageContent, MessageRole, Model},
    tool::{run_llm_tool, Tools},
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
const TOOLS_PLACEHOLDER: &str = "{{__tools__}}";

pub type AgentVariables = IndexMap<String, String>;

pub const INPUT_PLACEHOLDER: &str = "__INPUT__";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    use_tools: Option<String>,
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
    #[serde(default, skip_serializing_if = "is_false")]
    dynamic_instructions: bool,

    #[serde(skip, default)]
    config_variables: AgentVariables,
    #[serde(skip, default)]
    shared_variables: AgentVariables,
    #[serde(skip, default)]
    session_variables: Option<AgentVariables>,
    #[serde(skip, default)]
    shared_dynamic_instructions: Option<String>,
    #[serde(skip, default)]
    session_dynamic_instructions: Option<String>,
    #[serde(skip, default)]
    tools: Tools,
    #[serde(skip, default)]
    rag: Option<Arc<Rag>>,
    #[serde(skip, default)]
    model: Model,
    #[serde(skip, default)]
    mcp_manager: Option<Arc<McpManager>>,
    #[serde(skip, default)]
    compat_config: AgentConfig,
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
        let mut agent = Self {
            name: name.to_string(),
            model_id: frontmatter.model_id,
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
            dynamic_instructions: frontmatter.dynamic_instructions,
            ..Default::default()
        };
        agent.sync_compat_config();
        agent
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
        let agent_file_path = Config::agents_data_dir().join(format!("{name}.md"));
        let mut agent = if agent_file_path.exists() {
            Self::load(&agent_file_path)?
        } else {
            let functions_dir = Config::agent_tools_dir(name);
            let definition_file_path = functions_dir.join("index.yaml");
            if !definition_file_path.exists() {
                bail!("Unknown agent `{name}`");
            }
            let definition = AgentDefinition::load(&definition_file_path)?;
            let mut prompt = definition.instructions;
            interpolate_variables(&mut prompt);
            let mut agent = Self {
                name: name.to_string(),
                description: definition.description,
                version: definition.version,
                variables: definition.variables,
                conversation_starters: definition.conversation_starters,
                documents: definition.documents,
                prompt,
                dynamic_instructions: definition.dynamic_instructions,
                ..Default::default()
            };
            agent.sync_compat_config();
            agent
        };

        let config_path = Config::agent_config_file(name);
        let mut compat_config = if config_path.exists() {
            AgentConfig::load(&config_path)?
        } else {
            AgentConfig::new(&config.read())
        };
        compat_config.load_envs(name);
        agent.apply_compat_config(compat_config);

        let mcp_manager = config.read().mcp_manager.clone();
        agent.mcp_manager = mcp_manager.clone();

        let mcp_tools = if agent.contains_tools_placeholder() {
            match &mcp_manager {
                Some(manager) => Some(manager.get_all_tools().await),
                None => None,
            }
        } else {
            None
        };
        agent.tools = Tools::init_from_mcp(mcp_tools.clone());
        agent.replace_tools_placeholder();

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
        agent.sync_compat_config();

        let rag_path = Config::agent_rag_file(name, DEFAULT_AGENT_NAME);
        let agent_dir = agent_file_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(Config::agents_data_dir);
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
            r#"# {} {}
{}{}"#,
            self.name, self.version, self.description, starters
        )
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
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

    pub fn use_tools(&self) -> Option<String> {
        self.use_tools.clone()
    }

    pub fn is_empty_prompt(&self) -> bool {
        self.instructions_or_prompt().is_empty()
    }

    pub fn is_embedded_prompt(&self) -> bool {
        self.instructions_or_prompt().contains(INPUT_PLACEHOLDER)
    }

    pub fn has_args(&self) -> bool {
        self.name.contains('#')
    }

    pub fn set_model(&mut self, model: Model) {
        self.model_id = Some(model.id());
        self.model = model;
        self.sync_compat_config();
    }

    pub fn set_temperature(&mut self, value: Option<f64>) {
        self.temperature = value;
        self.sync_compat_config();
    }

    pub fn set_top_p(&mut self, value: Option<f64>) {
        self.top_p = value;
        self.sync_compat_config();
    }

    pub fn set_use_tools(&mut self, value: Option<String>) {
        self.use_tools = value;
        self.sync_compat_config();
    }

    pub fn echo_messages(&self, input: &Input) -> String {
        let prompt = self.interpolated_instructions();
        let input_markdown = input.render();
        if prompt.is_empty() {
            input_markdown
        } else if prompt.contains(INPUT_PLACEHOLDER) {
            prompt.replace(INPUT_PLACEHOLDER, &input_markdown)
        } else {
            format!("{}\n\n{}", prompt, input_markdown)
        }
    }

    pub fn build_messages(&self, input: &Input) -> Vec<Message> {
        let prompt = self.interpolated_instructions();
        let mut content = input.message_content();
        let mut messages = if prompt.is_empty() {
            vec![Message::new(MessageRole::User, content)]
        } else if prompt.contains(INPUT_PLACEHOLDER) {
            content.merge_prompt(|v: &str| prompt.replace(INPUT_PLACEHOLDER, v));
            vec![Message::new(MessageRole::User, content)]
        } else {
            let mut messages = vec![];
            let (system, cases) = parse_structure_prompt(&prompt);
            if !system.is_empty() {
                messages.push(Message::new(
                    MessageRole::System,
                    MessageContent::Text(system.to_string()),
                ));
            }
            if !cases.is_empty() {
                messages.extend(cases.into_iter().flat_map(|(i, o)| {
                    vec![
                        Message::new(MessageRole::User, MessageContent::Text(i.to_string())),
                        Message::new(MessageRole::Assistant, MessageContent::Text(o.to_string())),
                    ]
                }));
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

    pub fn config(&self) -> &AgentConfig {
        &self.compat_config
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
            .session_dynamic_instructions
            .clone()
            .or_else(|| self.shared_dynamic_instructions.clone())
            .or_else(|| self.instructions.clone())
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

    pub fn variable_envs(&self) -> HashMap<String, String> {
        self.variables()
            .iter()
            .map(|(k, v)| {
                (
                    format!("LLM_AGENT_VAR_{}", normalize_env_name(k)),
                    v.clone(),
                )
            })
            .collect()
    }

    pub fn config_variables(&self) -> &AgentVariables {
        &self.config_variables
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
        self.session_dynamic_instructions = None;
    }

    pub fn is_dynamic_instructions(&self) -> bool {
        self.dynamic_instructions
    }

    pub fn update_shared_dynamic_instructions(&mut self, force: bool) -> Result<()> {
        if self.is_dynamic_instructions() && (force || self.shared_dynamic_instructions.is_none()) {
            self.shared_dynamic_instructions = Some(self.run_instructions_fn()?);
        }
        Ok(())
    }

    pub fn update_session_dynamic_instructions(&mut self, value: Option<String>) -> Result<()> {
        if self.is_dynamic_instructions() {
            let value = match value {
                Some(v) => v,
                None => self.run_instructions_fn()?,
            };
            self.session_dynamic_instructions = Some(value);
        }
        Ok(())
    }

    fn run_instructions_fn(&self) -> Result<String> {
        let value = run_llm_tool(
            self.name().to_string(),
            vec!["_instructions".into(), "{}".into()],
            self.variable_envs(),
        )?;
        match value {
            Some(v) => Ok(v),
            _ => bail!("No return value from '_instructions' function"),
        }
    }

    fn apply_compat_config(&mut self, compat_config: AgentConfig) {
        if compat_config.model_id.is_some() {
            self.model_id = compat_config.model_id.clone();
        }
        if compat_config.temperature.is_some() {
            self.temperature = compat_config.temperature;
        }
        if compat_config.top_p.is_some() {
            self.top_p = compat_config.top_p;
        }
        if compat_config.use_tools.is_some() {
            self.use_tools = compat_config.use_tools.clone();
        }
        if compat_config.agent_default_session.is_some() {
            self.agent_default_session = compat_config.agent_default_session.clone();
        }
        if compat_config.instructions.is_some() {
            self.instructions = compat_config.instructions.clone();
        }
        if compat_config.hooks.is_some() {
            self.hooks = compat_config.hooks.clone();
        }
        self.config_variables = compat_config.variables.clone();
        self.compat_config = compat_config;
        self.sync_compat_config();
    }

    fn sync_compat_config(&mut self) {
        self.compat_config = AgentConfig {
            model_id: self.model_id.clone(),
            temperature: self.temperature,
            top_p: self.top_p,
            use_tools: self.use_tools.clone(),
            agent_default_session: self.agent_default_session.clone(),
            instructions: self.instructions.clone(),
            variables: self.config_variables.clone(),
            hooks: self.hooks.clone(),
        };
    }

    fn instructions_or_prompt(&self) -> &str {
        self.instructions.as_deref().unwrap_or(&self.prompt)
    }

    fn contains_tools_placeholder(&self) -> bool {
        self.prompt.contains(TOOLS_PLACEHOLDER)
            || self
                .instructions
                .as_deref()
                .is_some_and(|value| value.contains(TOOLS_PLACEHOLDER))
    }

    fn replace_tools_placeholder(&mut self) {
        if !self.contains_tools_placeholder() {
            return;
        }
        let tools = self
            .tools
            .declarations()
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let description = match v.description.split_once('\n') {
                    Some((v, _)) => v,
                    None => &v.description,
                };
                format!("{}. {}: {description}", i + 1, v.name)
            })
            .collect::<Vec<String>>()
            .join("\n");
        if self.prompt.contains(TOOLS_PLACEHOLDER) {
            self.prompt = self.prompt.replace(TOOLS_PLACEHOLDER, &tools);
        }
        if let Some(instructions) = self.instructions.as_mut() {
            if instructions.contains(TOOLS_PLACEHOLDER) {
                *instructions = instructions.replace(TOOLS_PLACEHOLDER, &tools);
            }
        }
        self.sync_compat_config();
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AgentConfig {
    #[serde(rename(serialize = "model", deserialize = "model"))]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_tools: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_default_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub variables: AgentVariables,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HooksConfig>,
}

impl AgentConfig {
    pub fn new(config: &Config) -> Self {
        Self {
            use_tools: config.use_tools.clone(),
            agent_default_session: config.agent_default_session.clone(),
            ..Default::default()
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents = read_to_string(path)
            .with_context(|| format!("Failed to read agent config file at '{}'", path.display()))?;
        let config: Self = serde_yaml::from_str(&contents)
            .with_context(|| format!("Failed to load agent config at '{}'", path.display()))?;
        Ok(config)
    }

    fn load_envs(&mut self, name: &str) {
        let with_prefix = |v: &str| normalize_env_name(&format!("{name}_{v}"));

        if let Some(v) = read_env_value::<String>(&with_prefix("model")) {
            self.model_id = v;
        }
        if let Some(v) = read_env_value::<f64>(&with_prefix("temperature")) {
            self.temperature = v;
        }
        if let Some(v) = read_env_value::<f64>(&with_prefix("top_p")) {
            self.top_p = v;
        }
        if let Some(v) = read_env_value::<String>(&with_prefix("use_tools")) {
            self.use_tools = v;
        }
        if let Some(v) = read_env_value::<String>(&with_prefix("agent_default_session")) {
            self.agent_default_session = v;
        }
        if let Some(v) = read_env_value::<String>(&with_prefix("instructions")) {
            self.instructions = v;
        }
        if let Ok(v) = env::var(with_prefix("variables")) {
            if let Ok(v) = serde_json::from_str(&v) {
                self.variables = v;
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AgentDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub dynamic_instructions: bool,
    #[serde(default)]
    pub variables: Vec<AgentVariable>,
    #[serde(default)]
    pub conversation_starters: Vec<String>,
    #[serde(default)]
    pub documents: Vec<String>,
}

impl AgentDefinition {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = read_to_string(path)
            .with_context(|| format!("Failed to read agent index file at '{}'", path.display()))?;
        let definition: Self = serde_yaml::from_str(&contents)
            .with_context(|| format!("Failed to load agent index at '{}'", path.display()))?;
        Ok(definition)
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
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    use_tools: Option<String>,
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
    #[serde(default, skip_serializing_if = "is_false")]
    dynamic_instructions: bool,
}

impl AgentFrontMatter {
    fn from_agent(agent: &Agent) -> Self {
        Self {
            model_id: agent.model_id.clone(),
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
            dynamic_instructions: agent.dynamic_instructions,
        }
    }

    fn is_empty(&self) -> bool {
        self.model_id.is_none()
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
            && !self.dynamic_instructions
    }
}

fn is_false(value: &bool) -> bool {
    !*value
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
    #[serde(skip_deserializing, default)]
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
    if !output.is_empty() {
        output.sort();
        output.dedup();
        return output;
    }
    let agents_file = Config::tools_dir().join("agents.txt");
    let contents = match read_to_string(agents_file) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    contents
        .split('\n')
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                None
            } else {
                Some(line.to_string())
            }
        })
        .collect()
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

    let index_path = Config::agent_tools_dir(agent_name).join("index.yaml");
    if !index_path.exists() {
        return vec![];
    }
    let Ok(definition) = AgentDefinition::load(&index_path) else {
        return vec![];
    };
    definition
        .variables
        .iter()
        .map(|v| {
            let description = match &v.default {
                Some(default) => format!("{} [default: {default}]", v.description),
                None => v.description.clone(),
            };
            (format!("{}=", v.name), Some(description))
        })
        .collect()
}

fn parse_structure_prompt(prompt: &str) -> (&str, Vec<(&str, &str)>) {
    let mut text = prompt;
    let mut search_input = true;
    let mut system = None;
    let mut parts = vec![];
    loop {
        let search = if search_input {
            "### INPUT:"
        } else {
            "### OUTPUT:"
        };
        match text.find(search) {
            Some(idx) => {
                if system.is_none() {
                    system = Some(&text[..idx])
                } else {
                    parts.push(&text[..idx])
                }
                search_input = !search_input;
                text = &text[(idx + search.len())..];
            }
            None => {
                if !text.is_empty() {
                    if system.is_none() {
                        system = Some(text)
                    } else {
                        parts.push(text)
                    }
                }
                break;
            }
        }
    }
    let parts_len = parts.len();
    if parts_len > 0 && parts_len % 2 == 0 {
        let cases: Vec<(&str, &str)> = parts
            .iter()
            .step_by(2)
            .zip(parts.iter().skip(1).step_by(2))
            .map(|(i, o)| (i.trim(), o.trim()))
            .collect();
        let system = system.map(|v| v.trim()).unwrap_or_default();
        return (system, cases);
    }

    (prompt, vec![])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_structure_prompt1() {
        let prompt = r#"
System message
### INPUT:
Input 1
### OUTPUT:
Output 1
"#;
        assert_eq!(
            parse_structure_prompt(prompt),
            ("System message", vec![("Input 1", "Output 1")])
        );
    }

    #[test]
    fn test_parse_structure_prompt2() {
        let prompt = r#"
### INPUT:
Input 1
### OUTPUT:
Output 1
"#;
        assert_eq!(
            parse_structure_prompt(prompt),
            ("", vec![("Input 1", "Output 1")])
        );
    }

    #[test]
    fn test_parse_structure_prompt3() {
        let prompt = r#"
System message
### INPUT:
Input 1
"#;
        assert_eq!(parse_structure_prompt(prompt), (prompt, vec![]));
    }
}
