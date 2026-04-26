//! `AgentConfig` — a named agent's pure configuration: model, prompts,
//! tools, hooks, retry config, variables. Data + pure methods only; no
//! file I/O, no inquire, no runtime state. Runtime fields (mcp_manager,
//! rag) live on the harnx-side `Agent` wrapper.

use crate::hooks::HooksConfig;
use crate::model::Model;
use crate::retry_config::RetryConfig;
use crate::system_vars::render_template;
use crate::tool::Tools;

use anyhow::Result;
use fancy_regex::Regex;
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use std::sync::LazyLock;

/// Agent name used for transient prompts that aren't loaded from disk.
pub const TEMP_AGENT_NAME: &str = "%%";

const CREATE_TITLE_PROMPT: &str = r#"Create a concise, 3-6 word title.

**Notes**:
- Avoid quotation marks or emojis
- RESPOND ONLY WITH TITLE SLUG TEXT

**Examples**:
stock-market-trends
perfect-chocolate-chip-recipe
remote-work-productivity-tips
video-game-development-insights"#;

/// Built-in agent name: routes to the title-creation prompt.
pub const CREATE_TITLE_AGENT_NAME: &str = "%create-title%";

pub type AgentVariables = IndexMap<String, String>;

static RE_METADATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\A-{3,}\s*(.*?)\s*-{3,}\s*(.*)").unwrap());

// --- Toolset-value serde helpers ---------------------------------------------
//
// `use_tools:` in an agent front-matter may be either a comma-separated string
// ("fs,web_search") or a YAML list. `deserialize_use_tools` normalises both
// shapes to `Option<Vec<String>>`. The helpers are public so harnx's Config
// parsing can reuse them for its own `mapping_tools:` field.

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolsetValue {
    String(String),
    Array(Vec<String>),
}

pub fn normalize_toolset_value(value: ToolsetValue) -> Vec<String> {
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

/// Split a comma-separated string of tool selectors while respecting `{…}` brace groups.
///
/// A comma inside braces (e.g. `fs_{read_file,write_file}`) is *not* treated as a separator.
pub fn split_tool_selectors(input: &str) -> Vec<&str> {
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

pub fn deserialize_use_tools<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<ToolsetValue>::deserialize(deserializer)?;
    Ok(value.map(normalize_toolset_value))
}

// --- AgentConfig --------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentConfig {
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
        deserialize_with = "deserialize_use_tools"
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_agent: Option<String>,
    #[serde(default)]
    prompt: String,

    #[serde(skip, default)]
    shared_variables: AgentVariables,
    #[serde(skip, default)]
    session_variables: Option<AgentVariables>,
    #[serde(skip, default)]
    tools: Tools,
    #[serde(skip, default)]
    model: Model,
}

impl AgentConfig {
    pub fn from_markdown(name: &str, content: &str) -> Result<Self> {
        let mut metadata = "";
        let mut prompt = content.trim();
        if let Ok(Some(caps)) = RE_METADATA.captures(content) {
            if let (Some(metadata_value), Some(prompt_value)) = (caps.get(1), caps.get(2)) {
                metadata = metadata_value.as_str().trim();
                prompt = prompt_value.as_str().trim();
            }
        }
        let prompt = prompt.to_string();
        let frontmatter = if metadata.is_empty() {
            AgentFrontMatter::default()
        } else {
            serde_yaml::from_str::<AgentFrontMatter>(metadata)
                .map_err(|e| anyhow::anyhow!("Invalid front-matter in agent '{}': {}", name, e))?
        };
        Ok(Self {
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
            compaction_agent: frontmatter.compaction_agent,
            prompt,
            ..Default::default()
        })
    }

    pub fn from_prompt(prompt: &str) -> Self {
        let prompt = prompt.to_string();
        Self {
            name: TEMP_AGENT_NAME.to_string(),
            prompt,
            ..Default::default()
        }
    }

    /// Markdown body for the built-in `%create-title%` agent, or `None`
    /// if `name` is not a recognised built-in.
    pub fn builtin_markdown(name: &str) -> Option<&'static str> {
        match name {
            CREATE_TITLE_AGENT_NAME => Some(CREATE_TITLE_PROMPT),
            _ => None,
        }
    }

    pub fn export(&self) -> Result<String> {
        let metadata = AgentFrontMatter::from_config(self);
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

    pub fn set_model_id(&mut self, model_id: Option<String>) {
        self.model_id = model_id;
    }

    pub fn model_fallbacks(&self) -> &[String] {
        &self.model_fallbacks
    }

    pub fn retry_config(&self) -> RetryConfig {
        self.retry.clone().unwrap_or_default()
    }

    pub fn set_model_fallbacks(&mut self, fallbacks: Vec<String>) {
        self.model_fallbacks = fallbacks;
    }

    pub fn set_compaction_agent(&mut self, value: Option<String>) {
        self.compaction_agent = value;
    }

    pub fn use_tools(&self) -> Option<Vec<String>> {
        self.use_tools.clone()
    }

    pub fn hooks(&self) -> Option<&HooksConfig> {
        self.hooks.as_ref()
    }

    pub fn compaction_agent(&self) -> Option<&str> {
        self.compaction_agent.as_deref()
    }

    pub fn has_args(&self) -> bool {
        self.name.contains('#')
    }

    pub fn set_model(&mut self, model: Model) {
        self.model_id = Some(model.id());
        self.model = model;
    }

    /// Replace the runtime `Model` without touching the serde `model_id`.
    /// Used by `init` when falling back to the global current model.
    pub fn set_resolved_model(&mut self, model: Model) {
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

    pub fn set_tools(&mut self, tools: Tools) {
        self.tools = tools;
    }

    pub fn documents(&self) -> &[String] {
        &self.documents
    }

    pub fn variables_mut(&mut self) -> &mut Vec<AgentVariable> {
        &mut self.variables
    }

    pub fn tools(&self) -> &Tools {
        &self.tools
    }

    pub fn conversation_staters(&self) -> &[String] {
        &self.conversation_starters
    }

    pub fn interpolated_instructions(&self) -> anyhow::Result<String> {
        let template = self.instructions.as_deref().unwrap_or(&self.prompt);
        render_template(template, self)
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

    /// Compute the full system-message text (prompt + tools summary), matching
    /// the logic in `build_messages` but without producing a User message.
    /// Used by `Session::build_messages` when `inject_system_prompt` is true.
    pub fn system_text(&self) -> anyhow::Result<String> {
        let prompt = self.interpolated_instructions()?;
        let tools_text = self.tools_text();
        Ok(match (&tools_text, prompt.is_empty()) {
            (Some(tools), false) => format!("{prompt}\n\n{tools}"),
            (Some(tools), true) => tools.clone(),
            (None, false) => prompt,
            (None, true) => String::new(),
        })
    }

    pub fn tools_text(&self) -> Option<String> {
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

    /// Build the messages for a one-shot LLM call using this agent's prompt +
    /// tools and the user-side `input`. Assembles a System message (prompt +
    /// tool summary, if any) followed by a User message with the input's
    /// content, optionally appending an Assistant continuation message if
    /// `input.continue_output()` is set.
    pub fn build_messages(
        &self,
        input: &crate::input::Input,
    ) -> anyhow::Result<Vec<crate::message::Message>> {
        use crate::message::{Message, MessageContent, MessageRole};
        let prompt = self.interpolated_instructions()?;
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
        Ok(messages)
    }

    /// Render the prompt + tools-summary + input markdown for the echo-mode
    /// (`.echo`) command. Companion of `build_messages`.
    pub fn echo_messages(&self, input: &crate::input::Input) -> anyhow::Result<String> {
        let prompt = self.interpolated_instructions()?;
        let tools_text = self.tools_text();
        let input_markdown = input.render();

        if prompt.is_empty() {
            if let Some(tools) = &tools_text {
                Ok(format!("{tools}\n\n{input_markdown}"))
            } else {
                Ok(input_markdown)
            }
        } else if let Some(tools) = &tools_text {
            Ok(format!("{prompt}\n\n{tools}\n\n{input_markdown}"))
        } else {
            Ok(format!("{}\n\n{}", prompt, input_markdown))
        }
    }
}

// --- AgentFrontMatter (serialized shape of an agent.md front-matter block) ---

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
        deserialize_with = "deserialize_use_tools"
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_agent: Option<String>,
}

impl AgentFrontMatter {
    fn from_config(config: &AgentConfig) -> Self {
        Self {
            model_id: config.model_id.clone(),
            model_fallbacks: config.model_fallbacks.clone(),
            retry: config.retry.clone(),
            temperature: config.temperature,
            top_p: config.top_p,
            use_tools: config.use_tools.clone(),
            description: config.description.clone(),
            version: config.version.clone(),
            variables: config.variables.clone(),
            conversation_starters: config.conversation_starters.clone(),
            documents: config.documents.clone(),
            agent_default_session: config.agent_default_session.clone(),
            instructions: config.instructions.clone(),
            hooks: config.hooks.clone(),
            compaction_agent: config.compaction_agent.clone(),
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
            && self.compaction_agent.is_none()
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

// --- AgentVariable -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frontmatter_only_at_start_of_file() {
        // A `--- ... ---` block that is NOT at the very start of the file must
        // be treated as plain prompt text, not parsed as front-matter.
        let content = "Some preamble text.\n---\nmodel: openai:gpt-4o\n---\nMore text.";
        let agent = AgentConfig::from_markdown("test", content).unwrap();
        // No model should have been parsed from the mid-file block.
        assert!(agent.model_id().is_none());
        // The entire content (trimmed) should appear as the prompt.
        let instructions = agent.interpolated_instructions().unwrap();
        assert!(
            instructions.contains("Some preamble text."),
            "expected preamble in prompt, got: {instructions:?}"
        );
        assert!(
            instructions.contains("model: openai:gpt-4o"),
            "expected mid-file --- block to be part of prompt, got: {instructions:?}"
        );
    }

    #[test]
    fn test_malformed_frontmatter_returns_error() {
        // A file whose leading front-matter contains invalid YAML must return
        // Err rather than silently producing a default config.
        let content = "---\nmodel: [unclosed bracket\n---\nYou are an agent.";
        let result = AgentConfig::from_markdown("bad-agent", content);
        assert!(
            result.is_err(),
            "expected Err for malformed YAML front-matter, got Ok"
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("Invalid front-matter"),
            "expected error message to mention 'Invalid front-matter', got: {msg:?}"
        );
    }

    #[test]
    fn test_render_template_system_var_os() {
        let agent = AgentConfig::from_prompt("You are on {{__os__}}");
        let result = agent.interpolated_instructions().unwrap();
        assert_eq!(result, format!("You are on {}", std::env::consts::OS));
    }

    #[test]
    fn test_render_template_agent_name() {
        let agent = AgentConfig::from_markdown("my-agent", "Your name is {{agent.name}}.").unwrap();
        let result = agent.interpolated_instructions().unwrap();
        assert_eq!(result, "Your name is my-agent.");
    }

    #[test]
    fn test_render_template_user_variable() {
        let mut agent = AgentConfig::from_prompt("Hello {{project_name}}");
        let mut variables = AgentVariables::default();
        variables.insert("project_name".to_string(), "harnx".to_string());
        agent.set_shared_variables(variables);
        let result = agent.interpolated_instructions().unwrap();
        assert_eq!(result, "Hello harnx");
    }

    #[test]
    fn test_render_template_undefined_var_returns_err() {
        let agent = AgentConfig::from_prompt("Hello {{undefined_mysterious_var}}");
        let result = agent.interpolated_instructions();
        assert!(
            result.is_err(),
            "expected Err for undefined template var, got Ok"
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("Template error"),
            "expected error message to contain 'Template error', got: {msg:?}"
        );
    }
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
