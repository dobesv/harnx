//! `AgentConfig` — a named agent's pure configuration: model, prompts,
//! tools, hooks, retry config, variables. Data + pure methods only; no
//! file I/O, no inquire, no runtime state. Runtime fields (rag) live on the harnx-side `Agent` wrapper.

use crate::config_data::{LoopDetectionOverride, RunLimitsOverride};
use crate::hooks::HooksConfig;
use crate::middleware::MiddlewareConfig;
use crate::model::Model;
use crate::retry_config::RetryConfig;
use crate::system_vars::render_template;
use crate::tool::{ToolDeclaration, Tools};
use crate::user_principals::UserPrincipals;

use anyhow::{Context, Result};
use fancy_regex::Regex;
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use std::sync::LazyLock;

/// Agent name used for transient prompts that aren't loaded from disk.
pub const TEMP_AGENT_NAME: &str = "%%";

pub type AgentVariables = IndexMap<String, String>;

/// Collect flat `[KEY, VALUE, KEY, VALUE, ...]` CLI pairs (as produced by the
/// `--agent-variable` / `-x` flag) into `AgentVariables`. Returns `None` for an
/// empty input so callers can leave `config.agent_variables` unset. Errors when
/// the values don't form complete key/value pairs.
pub fn collect_agent_variables(values: &[String]) -> Result<Option<AgentVariables>> {
    if values.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        values.len().is_multiple_of(2),
        "agent_variable values must be key/value pairs"
    );
    Ok(Some(
        values
            .chunks(2)
            .map(|v| (v[0].to_string(), v[1].to_string()))
            .collect(),
    ))
}

static RE_METADATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\A-{3,}\s*(.*?)\s*-{3,}\s*(.*)").unwrap());

/// An agent file split at its front matter.
struct AgentMarkdownParts<'a> {
    front_matter: &'a str,
    /// Byte offset of `front_matter` in the file.
    offset: usize,
    prompt: &'a str,
}

/// Split an agent file into its front matter and prompt, both trimmed. `None`
/// when the file doesn't start with a `---` block.
fn split_agent_markdown(content: &str) -> Option<AgentMarkdownParts<'_>> {
    let caps = RE_METADATA.captures(content).ok().flatten()?;
    let (metadata, prompt) = (caps.get(1)?, caps.get(2)?);
    let untrimmed = metadata.as_str();
    let front_matter = untrimmed.trim();
    Some(AgentMarkdownParts {
        front_matter,
        offset: metadata.start() + (untrimmed.len() - untrimmed.trim_start().len()),
        prompt: prompt.as_str().trim(),
    })
}

/// The front matter of an agent file, as [`AgentConfig::from_markdown`] reads
/// it, with the byte offset it starts at. `None` when there is none or it is
/// empty; such a file is all prompt.
pub fn agent_front_matter(content: &str) -> Option<(&str, usize)> {
    split_agent_markdown(content)
        .filter(|parts| !parts.front_matter.is_empty())
        .map(|parts| (parts.front_matter, parts.offset))
}

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

/// `patches:` accepts one jaq filter string or a list of them, mirroring the
/// package patch `agents:` shape. A whitespace-only string and `null` read as
/// no patches.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum PatchesValue {
    One(String),
    Many(Vec<String>),
}

// qual:api — referenced only through serde's `deserialize_with` string attribute, which rustqual cannot see as a call site
pub fn deserialize_patches<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<PatchesValue>::deserialize(deserializer)?;
    Ok(match value {
        None => Vec::new(),
        Some(PatchesValue::One(filter)) => {
            if filter.trim().is_empty() {
                Vec::new()
            } else {
                vec![filter]
            }
        }
        Some(PatchesValue::Many(filters)) => filters,
    })
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_use_tools"
    )]
    bash_tools: Option<Vec<String>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    // Boxed: `AgentConfig` is cloned into `Input` and the turn dispatch path's
    // async state machines, and `HooksConfig` carries four per-event lists plus
    // a file origin, so an inline copy measurably inflates every future frame
    // holding an agent (CI stack overflow in harnx-serve dispatch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hooks: Option<Box<HooksConfig>>,
    #[serde(default, skip_serializing_if = "MiddlewareConfig::is_empty")]
    pub middleware: MiddlewareConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_keep_recent_turns: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_keep_recent_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_tool_output_max_chars: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    loop_detection: Option<LoopDetectionOverride>,
    /// Per-agent run limits override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_limits: Option<RunLimitsOverride>,
    /// jaq filters that rewrite this agent's own config at session activation,
    /// with the session's caller-principals snapshot bound as `$user`. One
    /// filter string or a list — the same shape as a package patch `agents:`
    /// entry, but scoped to the declaring agent (no `.name` filtering needed).
    /// Fail-closed at activation; see [`AgentConfig::apply_user_patches`].
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_patches"
    )]
    patches: Vec<String>,
    #[serde(default)]
    pub role: AgentRole,
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
    /// The caller principals this activation was tailored for, stamped onto
    /// the effective agent a worker installs. Read by template rendering (the
    /// `__user__` variable). Never serialized: it is per-activation runtime
    /// state, not part of the agent file.
    #[serde(skip, default)]
    user_principals: Option<UserPrincipals>,
}

/// The jaq global names (with their `$` prefix) available to `patches`
/// filters, paired with values in [`AgentConfig::patch_globals`]. Adding a
/// global means adding its name here, its value in `patch_globals`, and
/// keeping the declaration list passed at validation sites in sync.
pub const PATCH_GLOBAL_NAMES: &[&str] = &["$user"];

/// Serde-skipped runtime state carried across a `patches` round trip
/// (invariant I3); see [`AgentConfig::take_runtime_state`].
struct PatchRuntimeState {
    before_model_id: Option<String>,
    shared_variables: AgentVariables,
    session_variables: Option<AgentVariables>,
    tools: Tools,
    resolved_model: Model,
    user_principals: Option<UserPrincipals>,
    middleware_origin: Option<crate::middleware::MiddlewareOrigin>,
    hook_origin: Option<crate::middleware::MiddlewareOrigin>,
}

impl AgentConfig {
    pub fn from_markdown(name: &str, content: &str) -> Result<Self> {
        let (metadata, prompt) = match split_agent_markdown(content) {
            Some(parts) => (parts.front_matter, parts.prompt),
            None => ("", content.trim()),
        };
        let prompt = prompt.to_string();
        let frontmatter = if metadata.is_empty() {
            AgentFrontMatter::default()
        } else {
            serde_yaml::from_str::<AgentFrontMatter>(metadata)
                .map_err(|e| anyhow::anyhow!("Invalid front-matter in agent '{}': {}", name, e))?
        };
        if !frontmatter.patches.is_empty() {
            crate::jaq::validate_filters_with_globals(&frontmatter.patches, PATCH_GLOBAL_NAMES)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Invalid front-matter in agent '{name}': invalid `patches` filters: {e}"
                    )
                })?;
        }
        Ok(Self {
            name: name.to_string(),
            model_id: frontmatter.model_id,
            model_fallbacks: frontmatter.model_fallbacks,
            retry: frontmatter.retry,
            temperature: frontmatter.temperature,
            top_p: frontmatter.top_p,
            use_tools: frontmatter.use_tools,
            bash_tools: frontmatter.bash_tools,
            description: frontmatter.description,
            version: frontmatter.version,
            variables: frontmatter.variables,
            conversation_starters: frontmatter.conversation_starters,
            documents: frontmatter.documents,
            instructions: frontmatter.instructions,
            hooks: frontmatter.hooks.map(Box::new),
            middleware: frontmatter.middleware,
            compaction_agent: frontmatter.compaction_agent,
            compaction_keep_recent_turns: frontmatter.compaction_keep_recent_turns,
            compaction_keep_recent_tokens: frontmatter.compaction_keep_recent_tokens,
            compaction_tool_output_max_chars: frontmatter.compaction_tool_output_max_chars,
            title_agent: frontmatter.title_agent,
            loop_detection: frontmatter.loop_detection,
            run_limits: frontmatter.run_limits,
            patches: frontmatter.patches,
            role: frontmatter.role,
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

    pub fn from_remote_catalog(
        name: impl Into<String>,
        description: Option<String>,
        role: AgentRole,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.unwrap_or_default(),
            role,
            ..Default::default()
        }
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn builtin_markdown(_name: &str) -> Option<&'static str> {
        None
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

    pub fn export_rendered(&self, expanded_tools: &[String]) -> Result<String> {
        let mut metadata = AgentFrontMatter::from_config(self);
        metadata.use_tools = Some(expanded_tools.to_vec());
        // Clear instructions so the rendered body is the single source of the prompt.
        // Without this, interpolated_instructions() would prefer the raw `instructions`
        // field over the body on reparse via from_markdown, reintroducing template vars.
        metadata.instructions = None;
        let body = self.interpolated_instructions()?;
        if metadata.is_empty() {
            Ok(format!("{}\n", body))
        } else {
            let metadata = serialize_frontmatter(&metadata)?;
            if body.is_empty() {
                Ok(format!("---\n{}\n---\n", metadata))
            } else {
                Ok(format!("---\n{}\n---\n\n{}\n", metadata, body))
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
        // Most agents don't set a version; omit the " v…" suffix entirely so the
        // header reads "# name" rather than a dangling "# name v".
        let version = if self.version.is_empty() {
            String::new()
        } else {
            format!(" v{}", self.version)
        };
        format!(
            r#"# {}{}
{}{}"#,
            self.name, version, self.description, starters
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

    pub fn set_title_agent(&mut self, value: Option<String>) {
        self.title_agent = value;
    }

    pub fn use_tools(&self) -> Option<Vec<String>> {
        self.use_tools.clone()
    }

    pub fn bash_tools(&self) -> Option<Vec<String>> {
        self.bash_tools.clone()
    }

    pub fn hooks(&self) -> Option<&HooksConfig> {
        self.hooks.as_deref()
    }

    /// Mutable hooks access for load-time origin binding.
    pub fn hooks_mut(&mut self) -> Option<&mut HooksConfig> {
        self.hooks.as_deref_mut()
    }

    pub fn loop_detection(&self) -> Option<&LoopDetectionOverride> {
        self.loop_detection.as_ref()
    }

    /// Returns this agent's run limits override.
    pub fn run_limits(&self) -> Option<&RunLimitsOverride> {
        self.run_limits.as_ref()
    }

    /// This agent's `patches` filters, applied at worker activation with the
    /// session's caller principals bound as `$user`.
    // qual:allow(dry, boilerplate) reason: "accessor mirrors the surrounding AgentConfig getter style; the field stays private so serialized front-matter cannot diverge from parsing"
    pub fn patches(&self) -> &[String] {
        &self.patches
    }

    /// The caller principals this activation was tailored for, stamped by the
    /// worker when it installs the session's effective agent. `None` outside a
    /// session (CLI, tests, frontends).
    pub fn user_principals(&self) -> Option<&UserPrincipals> {
        self.user_principals.as_ref()
    }

    /// Stamp the caller principals onto this effective agent.
    // qual:allow(dry, boilerplate) reason: "setter mirrors the surrounding AgentConfig setter style; the field stays private to keep stamping a worker-side decision"
    pub fn set_user_principals(&mut self, user_principals: Option<UserPrincipals>) {
        self.user_principals = user_principals;
    }

    /// Values for [`PATCH_GLOBAL_NAMES`], bound while `patches` run.
    fn patch_globals(user: &UserPrincipals) -> Vec<(&'static str, serde_json::Value)> {
        vec![(
            "$user",
            serde_json::to_value(user).expect("UserPrincipals are JSON-serializable"),
        )]
    }

    /// Apply this agent's `patches` filters with `$user` bound to `user`,
    /// turning this config into the session's effective agent config.
    ///
    /// Fail-closed: a compile or runtime error, empty or multiple filter
    /// outputs, a non-object output, a changed agent `name`, or a config that
    /// no longer deserializes all fail with an error naming this agent, and
    /// the caller must fail the activation rather than run the unpatched
    /// config.
    ///
    /// Runtime state that serde does not carry is restored from the
    /// pre-patch config via [`PatchRuntimeState`], so the effective agent
    /// behaves as if the patched values had been in the agent file
    /// (invariant I3). A changed `model` id is left unresolved for the
    /// caller, which owns the client catalog.
    // qual:allow(iosp) — one fail-closed pipeline: serialize, filter, validate, restore; each step's error names the agent, and splitting it would spread that context across helpers
    pub fn apply_user_patches(&mut self, user: &UserPrincipals) -> Result<()> {
        if self.patches.is_empty() {
            return Ok(());
        }
        let agent_name = self.name.clone();
        let mut output = serde_json::to_value(&*self)
            .with_context(|| format!("serialize agent '{agent_name}' for `patches`"))?;
        let globals = Self::patch_globals(user);
        for (index, filter) in self.patches.iter().enumerate() {
            output = crate::jaq::eval_filters_single_with_globals(
                std::slice::from_ref(filter),
                output,
                &globals,
            )
            .with_context(|| {
                format!(
                    "agent '{agent_name}': `patches` filter {} failed",
                    index + 1
                )
            })?;
            anyhow::ensure!(
                output.is_object(),
                "agent '{agent_name}': `patches` filter {} {filter:?} must output a JSON object, got {}",
                index + 1,
                json_kind(&output)
            );
        }
        let patched = serde_json::from_value::<AgentConfig>(output).with_context(|| {
            format!("agent '{agent_name}': `patches` produced an invalid agent config")
        })?;
        anyhow::ensure!(
            patched.name == agent_name,
            "agent '{agent_name}': `patches` must not change the agent `name` (saw '{}')",
            patched.name
        );
        let runtime = self.take_runtime_state();
        *self = patched;
        self.restore_runtime_state(runtime);
        Ok(())
    }

    /// Capture the serde-skipped runtime state a `patches` round trip must
    /// preserve (invariant I3): variables, agent-owned tools, the resolved
    /// model while its id is unchanged, middleware and lifecycle hook origins,
    /// and the stamped principals.
    fn take_runtime_state(&mut self) -> PatchRuntimeState {
        let hook_origin = self.hooks.as_mut().and_then(|hooks| hooks.origin.take());
        PatchRuntimeState {
            before_model_id: self.model_id.clone(),
            shared_variables: std::mem::take(&mut self.shared_variables),
            session_variables: self.session_variables.take(),
            tools: std::mem::take(&mut self.tools),
            resolved_model: std::mem::take(&mut self.model),
            user_principals: self.user_principals.take(),
            middleware_origin: self.middleware.origin.take(),
            hook_origin,
        }
    }

    /// Restore the state [`AgentConfig::take_runtime_state`] captured onto
    /// the patched config. A `model` id the patch changed stays unresolved;
    /// the worker resolves it from its client catalog.
    fn restore_runtime_state(&mut self, runtime: PatchRuntimeState) {
        let PatchRuntimeState {
            before_model_id,
            shared_variables,
            session_variables,
            tools,
            resolved_model,
            user_principals,
            middleware_origin,
            hook_origin,
        } = runtime;
        self.shared_variables = shared_variables;
        self.session_variables = session_variables;
        self.tools = tools;
        if self.model_id == before_model_id {
            self.model = resolved_model;
        }
        self.user_principals = user_principals;
        self.middleware.origin = middleware_origin;
        if let Some(hooks) = &mut self.hooks {
            hooks.origin = hook_origin;
        }
    }

    pub fn compaction_agent(&self) -> Option<&str> {
        self.compaction_agent.as_deref()
    }

    pub fn title_agent(&self) -> Option<&str> {
        self.title_agent.as_deref()
    }

    pub fn compaction_keep_recent_turns(&self) -> Option<usize> {
        self.compaction_keep_recent_turns
    }

    pub fn compaction_keep_recent_tokens(&self) -> Option<usize> {
        self.compaction_keep_recent_tokens
    }

    pub fn compaction_tool_output_max_chars(&self) -> Option<usize> {
        self.compaction_tool_output_max_chars
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

    pub fn set_bash_tools(&mut self, value: Option<Vec<String>>) {
        self.bash_tools = value;
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
        self.system_text()
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

    /// Compute the system-message text from the agent's interpolated prompt.
    /// Used by `Session::build_messages` when `inject_system_prompt` is true.
    ///
    /// The agent's available tools are NOT summarized here: the model already
    /// receives every available tool as a structured definition via the API
    /// `tools` field (built, filtered by `use_tools`, in
    /// `Config::tool_declarations_for_use_tools`). Appending an unfiltered
    /// text list duplicated those definitions and leaked tools from other
    /// packages into the prompt.
    pub fn instructions_template(&self) -> &str {
        self.instructions.as_deref().unwrap_or(&self.prompt)
    }

    pub fn system_text(&self) -> anyhow::Result<String> {
        render_template(self.instructions_template(), self, None)
    }

    pub fn system_text_with_tools(&self, tools: &[ToolDeclaration]) -> anyhow::Result<String> {
        render_template(self.instructions_template(), self, Some(tools))
    }

    /// Build the messages for a one-shot LLM call using this agent's prompt and
    /// the user-side `input`. Assembles a System message (the prompt) followed
    /// by a User message with the input's content, optionally appending an
    /// Assistant continuation message if `input.continue_output()` is set.
    pub fn build_messages(
        &self,
        input: &crate::input::Input,
    ) -> anyhow::Result<Vec<crate::message::Message>> {
        use crate::message::{Message, MessageContent, MessageRole};
        let prompt =
            self.system_text_with_tools(input.resolved_tools.as_deref().unwrap_or_default())?;
        let content = input.message_content();
        let mut messages = vec![];
        if !prompt.is_empty() {
            messages.push(Message::new(
                MessageRole::System,
                MessageContent::Text(prompt),
            ));
        }
        messages.push(Message::new(MessageRole::User, content));
        if let Some(text) = input.continue_output() {
            messages.push(Message::new(
                MessageRole::Assistant,
                MessageContent::Text(text.into()),
            ));
        }
        Ok(messages)
    }

    /// Render the prompt + input markdown for the echo-mode (`.echo`) command.
    /// Companion of `build_messages`.
    pub fn echo_messages(&self, input: &crate::input::Input) -> anyhow::Result<String> {
        let prompt =
            self.system_text_with_tools(input.resolved_tools.as_deref().unwrap_or_default())?;
        let input_markdown = input.render();

        if prompt.is_empty() {
            Ok(input_markdown)
        } else {
            Ok(format!("{prompt}\n\n{input_markdown}"))
        }
    }
}

/// Role of an agent — controls visibility in the agent picker and completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    #[default]
    Assistant,
    Subagent,
    Compaction,
}

// --- AgentFrontMatter (serialized shape of an agent.md front-matter block) ---

/// The front matter of an agent file as it is written: what
/// [`AgentConfig::from_markdown`] deserializes and `export` writes back.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentFrontMatter {
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_use_tools"
    )]
    bash_tools: Option<Vec<String>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hooks: Option<HooksConfig>,
    #[serde(default, skip_serializing_if = "MiddlewareConfig::is_empty")]
    middleware: MiddlewareConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_keep_recent_turns: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_keep_recent_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_tool_output_max_chars: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    loop_detection: Option<LoopDetectionOverride>,
    /// Per-agent run limits override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_limits: Option<RunLimitsOverride>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_patches"
    )]
    patches: Vec<String>,
    #[serde(default)]
    role: AgentRole,
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
            bash_tools: config.bash_tools.clone(),
            description: config.description.clone(),
            version: config.version.clone(),
            variables: config.variables.clone(),
            conversation_starters: config.conversation_starters.clone(),
            documents: config.documents.clone(),
            instructions: config.instructions.clone(),
            hooks: config.hooks.as_deref().cloned(),
            middleware: config.middleware.clone(),
            compaction_agent: config.compaction_agent.clone(),
            compaction_keep_recent_turns: config.compaction_keep_recent_turns,
            compaction_keep_recent_tokens: config.compaction_keep_recent_tokens,
            compaction_tool_output_max_chars: config.compaction_tool_output_max_chars,
            title_agent: config.title_agent.clone(),
            loop_detection: config.loop_detection,
            run_limits: config.run_limits,
            patches: config.patches.clone(),
            role: config.role,
        }
    }

    fn model_is_empty(&self) -> bool {
        self.model_id.is_none()
            && self.model_fallbacks.is_empty()
            && self.retry.is_none()
            && self.temperature.is_none()
            && self.top_p.is_none()
    }

    /// `use_tools` and `bash_tools` are the two tool-selector settings; they
    /// group together to keep `content_is_empty`'s complexity down.
    fn tool_selection_is_empty(&self) -> bool {
        self.use_tools.is_none() && self.bash_tools.is_none()
    }

    fn content_is_empty(&self) -> bool {
        self.tool_selection_is_empty()
            && self.description.is_empty()
            && self.version.is_empty()
            && self.variables.is_empty()
            && self.conversation_starters.is_empty()
            && self.documents.is_empty()
            && self.instructions.is_none()
            && self.hooks.is_none()
            && self.middleware.is_empty()
            && self.patches.is_empty()
    }

    fn compaction_is_empty(&self) -> bool {
        self.compaction_agent.is_none()
            && self.compaction_keep_recent_turns.is_none()
            && self.compaction_keep_recent_tokens.is_none()
            && self.compaction_tool_output_max_chars.is_none()
    }

    fn title_is_empty(&self) -> bool {
        self.title_agent.is_none()
    }

    fn loop_detection_is_empty(&self) -> bool {
        self.loop_detection.is_none()
    }

    fn run_limits_is_empty(&self) -> bool {
        self.run_limits.is_none()
    }

    fn is_empty(&self) -> bool {
        self.model_is_empty()
            && self.content_is_empty()
            && self.compaction_is_empty()
            && self.title_is_empty()
            && self.loop_detection_is_empty()
            && self.run_limits_is_empty()
            && self.role == AgentRole::Assistant
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

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

// --- AgentVariable -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_tools_parse_from_frontmatter_string_and_list() {
        let string_form =
            AgentConfig::from_markdown("string-tools", "---\nbash_tools: \"fs_*,grep_*\"\n---\n")
                .unwrap();
        let expected = Some(vec!["fs_*".to_string(), "grep_*".to_string()]);
        assert_eq!(string_form.bash_tools(), expected);

        let list_form = AgentConfig::from_markdown(
            "list-tools",
            "---\nbash_tools:\n  - fs_*\n  - grep_*\n---\n",
        )
        .unwrap();
        assert_eq!(list_form.bash_tools(), expected);

        let absent =
            AgentConfig::from_markdown("plain", "---\nmodel: openai:gpt-4o\n---\nhi\n").unwrap();
        assert_eq!(absent.bash_tools(), None);

        let exported = string_form.export().unwrap();
        let reparsed = AgentConfig::from_markdown("string-tools", &exported).unwrap();
        assert_eq!(reparsed.bash_tools(), expected);
    }

    #[test]
    fn collect_agent_variables_empty_is_none() {
        assert!(collect_agent_variables(&[]).unwrap().is_none());
    }

    #[test]
    fn collect_agent_variables_builds_map() {
        let values = vec![
            "cloud_env".to_string(),
            "true".to_string(),
            "debug".to_string(),
            "false".to_string(),
        ];
        let vars = collect_agent_variables(&values).unwrap().unwrap();
        assert_eq!(vars.get("cloud_env").map(String::as_str), Some("true"));
        assert_eq!(vars.get("debug").map(String::as_str), Some("false"));
        assert_eq!(vars.len(), 2);
    }

    #[test]
    fn collect_agent_variables_rejects_odd_pairs() {
        let values = vec!["lonely_key".to_string()];
        assert!(collect_agent_variables(&values).is_err());
    }

    #[test]
    fn compaction_knobs_parse_from_frontmatter() {
        let md = "---\n\
model: openai:gpt-4o\n\
compaction_keep_recent_turns: 5\n\
compaction_keep_recent_tokens: 12000\n\
compaction_tool_output_max_chars: 500\n\
---\n\
You are a compaction agent.\n";
        let agent = AgentConfig::from_markdown("compactor", md).unwrap();
        assert_eq!(agent.compaction_keep_recent_turns(), Some(5));
        assert_eq!(agent.compaction_keep_recent_tokens(), Some(12000));
        assert_eq!(agent.compaction_tool_output_max_chars(), Some(500));

        // Absent → None.
        let bare =
            AgentConfig::from_markdown("plain", "---\nmodel: openai:gpt-4o\n---\nhi\n").unwrap();
        assert_eq!(bare.compaction_keep_recent_turns(), None);
        assert_eq!(bare.compaction_keep_recent_tokens(), None);
        assert_eq!(bare.compaction_tool_output_max_chars(), None);

        // Serialize back to front-matter and reparse, exercising the
        // from_config / skip_serializing_if / is_empty path for the knobs.
        let exported = agent.export().unwrap();
        let reparsed = AgentConfig::from_markdown("compactor", &exported).unwrap();
        assert_eq!(reparsed.compaction_keep_recent_turns(), Some(5));
        assert_eq!(reparsed.compaction_keep_recent_tokens(), Some(12000));
        assert_eq!(reparsed.compaction_tool_output_max_chars(), Some(500));
    }

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
    fn banner_omits_version_suffix_when_unset() {
        let agent =
            AgentConfig::from_markdown("demo-agent", "---\ndescription: A demo.\n---\nPrompt")
                .unwrap();
        let banner = agent.banner();
        assert!(
            banner.starts_with("# demo-agent\n"),
            "expected no dangling version, got: {banner:?}"
        );
    }

    #[test]
    fn banner_includes_version_suffix_when_set() {
        let agent = AgentConfig::from_markdown(
            "demo-agent",
            "---\ndescription: A demo.\nversion: 1.2.3\n---\nPrompt",
        )
        .unwrap();
        assert!(
            agent.banner().starts_with("# demo-agent v1.2.3\n"),
            "got: {:?}",
            agent.banner()
        );
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
    fn export_rendered_roundtrips_with_interpolated_body_and_expanded_tools() {
        let content = "---\nmodel: openai:gpt-4o\nuse_tools:\n  - fs:*\nvariables:\n  - name: project_name\n    description: Project name\nconversation_starters:\n  - Say hi\n---\nHello {{project_name}} from {{agent.name}}.";
        let mut agent = AgentConfig::from_markdown("render-agent", content).unwrap();
        let mut variables = AgentVariables::default();
        variables.insert("project_name".to_string(), "harnx".to_string());
        agent.set_shared_variables(variables);
        let expanded_tools = vec!["fs_read".to_string(), "fs_write".to_string()];

        let exported = agent.export_rendered(&expanded_tools).unwrap();

        assert!(exported.contains("use_tools:\n- fs_read\n- fs_write\n"));
        assert!(exported.contains("\n\nHello harnx from render-agent.\n"));
        assert!(!exported.contains("{{project_name}}"));
        let reparsed = AgentConfig::from_markdown("render-agent", &exported).unwrap();
        assert_eq!(reparsed.use_tools, Some(expanded_tools));
        assert_eq!(reparsed.prompt, "Hello harnx from render-agent.");
        assert_eq!(
            reparsed.interpolated_instructions().unwrap(),
            "Hello harnx from render-agent."
        );
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

    #[test]
    fn system_text_with_tools_renders_tool_names_in_jinja() {
        use crate::tool::ToolDeclaration;

        let agent = AgentConfig::from_prompt(
            "Tools: {% for t in tools %}{{ t.name }}{% if not loop.last %}, {% endif %}{% endfor %}",
        );
        let tools = vec![
            ToolDeclaration {
                name: "fs_read".to_string(),
                description: "Read files".to_string(),
                parameters: Default::default(),
                mcp_tool_name: None,
                mcp_server_name: None,
                call_template: None,
                result_template: None,
                idempotent_hint: None,
                read_only_hint: None,
                kind: None,
            },
            ToolDeclaration {
                name: "bash_exec".to_string(),
                description: "Run shell commands".to_string(),
                parameters: Default::default(),
                mcp_tool_name: None,
                mcp_server_name: None,
                call_template: None,
                result_template: None,
                idempotent_hint: None,
                read_only_hint: None,
                kind: None,
            },
        ];

        let rendered = agent.system_text_with_tools(&tools).unwrap();

        assert!(rendered.contains("fs_read"), "rendered prompt: {rendered}");
        assert!(
            rendered.contains("bash_exec"),
            "rendered prompt: {rendered}"
        );
    }

    #[test]
    fn interpolated_instructions_renders_current_model_id_after_model_change() {
        let mut agent = AgentConfig::from_prompt("Model={{ agent.model }}");
        agent.set_model_id(Some("openai:gpt-4o-mini".to_string()));

        let first = agent.interpolated_instructions().unwrap();
        assert_eq!(first, "Model=openai:gpt-4o-mini");

        agent.set_model_id(Some("anthropic:claude-3-7-sonnet".to_string()));
        let second = agent.interpolated_instructions().unwrap();
        assert_eq!(second, "Model=anthropic:claude-3-7-sonnet");
    }

    #[test]
    fn test_agent_role_defaults_to_assistant() {
        let content = "---\nmodel: openai:gpt-4o\n---\nYou are an agent.";
        let agent = AgentConfig::from_markdown("my-agent", content).unwrap();
        assert_eq!(agent.role, AgentRole::Assistant);
    }

    #[test]
    fn test_agent_role_subagent_parsed() {
        let content = "---\nrole: subagent\nmodel: openai:gpt-4o\n---\nYou are a sub-agent.";
        let agent = AgentConfig::from_markdown("my-subagent", content).unwrap();
        assert_eq!(agent.role, AgentRole::Subagent);
    }

    #[test]
    fn test_agent_role_compaction_parsed() {
        let content = "---\nrole: compaction\nmodel: openai:gpt-4o\n---\nSummarize.";
        let agent = AgentConfig::from_markdown("my-compaction", content).unwrap();
        assert_eq!(agent.role, AgentRole::Compaction);
    }

    #[test]
    fn test_agent_role_subagent_roundtrip() {
        let content = "---\nrole: subagent\nmodel: openai:gpt-4o\n---\nYou are a sub-agent.";
        let agent = AgentConfig::from_markdown("my-subagent", content).unwrap();
        let exported = agent.export().unwrap();
        let reparsed = AgentConfig::from_markdown("my-subagent", &exported).unwrap();
        assert_eq!(reparsed.role, AgentRole::Subagent);
    }

    #[test]
    fn test_agent_no_role_field_is_assistant() {
        // Completely empty frontmatter — AgentRole::Assistant is the serde default
        let agent = AgentConfig::from_markdown("plain", "You are plain.").unwrap();
        assert_eq!(agent.role, AgentRole::Assistant);
    }

    #[test]
    fn title_agent_roundtrip() {
        // Round-trip an AgentConfig with title_agent set and assert it survives serialize→deserialize
        let mut agent = AgentConfig::from_markdown(
            "title-test",
            "---\nmodel: openai:gpt-4o\ntitle_agent: claude-sonnet-4-6\n---\nYou are an agent.",
        )
        .unwrap();
        assert_eq!(agent.title_agent(), Some("claude-sonnet-4-6"));

        // Clear and set via setter
        agent.set_title_agent(None);
        assert!(agent.title_agent().is_none());

        // Set it back
        agent.set_title_agent(Some("title-generator-v2".to_string()));
        assert_eq!(agent.title_agent(), Some("title-generator-v2"));

        // Export and reparse
        let exported = agent.export().unwrap();
        let reparsed = AgentConfig::from_markdown("title-test", &exported).unwrap();
        assert_eq!(reparsed.title_agent(), Some("title-generator-v2"));
    }

    #[test]
    fn loop_detection_override_parses_from_frontmatter() {
        let md = "---\nloop_detection:\n  tool_calls: false\n---\nYou poll CI.\n";
        let agent = AgentConfig::from_markdown("poller", md).unwrap();
        assert_eq!(
            agent.loop_detection(),
            Some(&crate::config_data::LoopDetectionOverride {
                tool_calls: Some(false),
                output: None,
            })
        );
        assert_eq!(
            AgentConfig::from_markdown("plain", "Hi")
                .unwrap()
                .loop_detection(),
            None
        );
    }

    #[test]
    fn loop_detection_override_survives_export_and_reparse() {
        // The override is this agent's only front-matter. If `export` judged
        // the front-matter empty, re-saving the agent would silently drop it.
        let md = "---\nloop_detection:\n  tool_calls: false\n---\nYou poll CI.\n";
        let agent = AgentConfig::from_markdown("poller", md).unwrap();

        let exported = agent.export().unwrap();
        let reparsed = AgentConfig::from_markdown("poller", &exported).unwrap();

        assert!(
            exported.starts_with("---\n"),
            "expected a front-matter block, got: {exported:?}"
        );
        assert_eq!(
            reparsed.loop_detection(),
            Some(&crate::config_data::LoopDetectionOverride {
                tool_calls: Some(false),
                output: None,
            })
        );
    }

    #[test]
    fn test_docs_run_limits_frontmatter_examples() {
        use crate::config_data::RunLimitsConfig;
        for (value, expected) in [
            (None, 86400),
            (Some("null"), 86400),
            (Some("0"), 86400),
            (Some("-1"), 86400),
            (Some("-9223372036854775808"), 86400),
            (Some("7200"), 7200),
            (Some("604800"), 604800),
            (Some("2592000"), 2592000),
        ] {
            let frontmatter = value
                .map(|value| format!("run_limits:\n  timeout_secs: {value}\n"))
                .unwrap_or_default();
            let markdown = format!("---\n{frontmatter}---\nTarget agent\n");
            let agent = AgentConfig::from_markdown("target", &markdown).unwrap();
            let limits = agent.run_limits().and_then(|limits| limits.timeout_secs);
            assert_eq!(
                RunLimitsConfig::default()
                    .resolve(limits.as_ref())
                    .timeout_secs
                    .get(),
                expected,
                "{value:?}"
            );
        }
        for value in [
            "unlimited",
            "\"7200\"",
            "1.5",
            "true",
            "18446744073709551615",
        ] {
            assert!(
                AgentConfig::from_markdown(
                    "target",
                    &format!("---\nrun_limits:\n  timeout_secs: {value}\n---\nTarget\n")
                )
                .is_err(),
                "{value}"
            );
        }
    }

    // --- `patches` ----------------------------------------------------------------

    fn developers_group_principals() -> UserPrincipals {
        UserPrincipals {
            user_id: Some("alice".to_string()),
            users: vec!["alice".to_string()],
            groups: vec!["developers".to_string()],
            roles: vec![],
        }
    }

    /// Render the full anyhow error chain; `patches` failures assert on the
    /// agent-naming context, not just the top-level message.
    fn anyhow_chain(error: anyhow::Error) -> String {
        format!("{error:#}")
    }

    fn non_developer_principals() -> UserPrincipals {
        UserPrincipals {
            user_id: Some("mallory".to_string()),
            users: vec!["mallory".to_string()],
            groups: vec!["finance".to_string()],
            roles: vec![],
        }
    }

    /// The example agent from the plan: coding tools only for developers.
    fn patchable_agent_config() -> AgentConfig {
        AgentConfig::from_markdown(
            "jira-assistant",
            "---\n\
             model: openai:gpt-4o\n\
             use_tools: [jira, confluence, coder, fs]\n\
             patches:\n\
             - 'if ($user.groups | index(\"developers\")) then . else .use_tools |= map(select(. != \"coder\" and . != \"fs\")) end'\n\
             ---\n\
             You are a Jira assistant.\n",
        )
        .unwrap()
    }

    #[test]
    fn patches_parse_from_frontmatter_string_and_list() {
        let single =
            AgentConfig::from_markdown("single", "---\npatches: '.use_tools = []'\n---\nBody\n")
                .unwrap();
        assert_eq!(single.patches(), &[".use_tools = []".to_string()]);

        let list = AgentConfig::from_markdown(
            "list",
            "---\npatches:\n  - '.use_tools = []'\n  - '.model = \"openai:gpt-4o\"'\n---\nBody\n",
        )
        .unwrap();
        assert_eq!(
            list.patches(),
            &[
                ".use_tools = []".to_string(),
                ".model = \"openai:gpt-4o\"".to_string()
            ]
        );

        // Absent, null, and whitespace-only all read as no patches.
        for (name, frontmatter) in [
            ("absent", "model: openai:gpt-4o\n"),
            ("null", "model: openai:gpt-4o\npatches: null\n"),
            ("blank", "model: openai:gpt-4o\npatches: \"  \"\n"),
        ] {
            let agent = AgentConfig::from_markdown(name, &format!("---\n{frontmatter}---\nBody\n"))
                .unwrap();
            assert!(agent.patches().is_empty(), "{name}");
        }
    }

    #[test]
    fn patches_survive_export_and_reparse() {
        let agent = patchable_agent_config();
        let exported = agent.export().unwrap();
        let reparsed = AgentConfig::from_markdown("jira-assistant", &exported).unwrap();
        assert_eq!(reparsed.patches(), agent.patches());
    }

    #[test]
    fn broken_patches_filter_fails_agent_load() {
        // A compile error must surface at load time, not at the first session.
        let result = AgentConfig::from_markdown(
            "broken",
            "---\npatches:\n  - '$missing as $x | .'\n---\nBody\n",
        );
        let error = anyhow_chain(result.unwrap_err());
        assert!(
            error.contains("broken") && error.contains("patches"),
            "load error must name the agent and the `patches` field: {error}"
        );

        // A filter that is fine syntactically but references an undeclared
        // global also fails compile.
        assert!(AgentConfig::from_markdown(
            "undeclared",
            "---\npatches: '.x = $undeclared'\n---\nBody\n"
        )
        .is_err());
    }

    #[test]
    fn apply_user_patches_trims_use_tools_for_non_developers() {
        // Developer keeps everything.
        let mut developer = patchable_agent_config();
        developer
            .apply_user_patches(&developers_group_principals())
            .unwrap();
        assert_eq!(
            developer.use_tools(),
            Some(vec![
                "jira".to_string(),
                "confluence".to_string(),
                "coder".to_string(),
                "fs".to_string(),
            ])
        );

        // Non-developer loses the coding tools; everything else stays.
        let mut outsider = patchable_agent_config();
        outsider
            .apply_user_patches(&non_developer_principals())
            .unwrap();
        assert_eq!(
            outsider.use_tools(),
            Some(vec!["jira".to_string(), "confluence".to_string()])
        );
        assert_eq!(outsider.model_id(), Some("openai:gpt-4o"));
        assert_eq!(outsider.name(), "jira-assistant");
    }

    #[test]
    fn apply_user_patches_is_noop_without_patches() {
        let mut agent =
            AgentConfig::from_markdown("plain", "---\nmodel: openai:gpt-4o\n---\nBody\n").unwrap();
        agent
            .apply_user_patches(&non_developer_principals())
            .unwrap();
        assert_eq!(agent.model_id(), Some("openai:gpt-4o"));
        assert_eq!(agent.name(), "plain");
    }

    #[test]
    fn apply_user_patches_preserves_runtime_state() {
        use crate::middleware::MiddlewareOrigin;
        use crate::model::Model;
        use crate::tool::{ToolDeclaration, Tools};
        use std::path::PathBuf;

        let mut agent = AgentConfig::from_markdown(
            "stateful",
            "---\n\
             model: openai:gpt-4o\n\
             use_tools: [fs]\n\
             patches: '.use_tools = []'\n\
             hooks:\n\
             \x20 max_resume: 2\n\
             \x20 SessionStart: [{name: greet, script: hooks/start.sh}]\n\
             \x20 UserPromptSubmit: [{name: gate, python: pass}]\n\
             \x20 Stop: [{name: resume, bash: next}]\n\
             \x20 StopFailure: [{name: audit, script: hooks/failure.py}]\n\
             ---\n\
             Body\n",
        )
        .unwrap();
        let mut shared = AgentVariables::new();
        shared.insert("shared_key".to_string(), "shared".to_string());
        let mut session = AgentVariables::new();
        session.insert("session_key".to_string(), "session".to_string());
        agent.set_shared_variables(shared);
        agent.set_session_variables(session);
        let tools = Tools::init_from_mcp(Some(vec![ToolDeclaration {
            name: "agent_owned".to_string(),
            description: "Owned by this agent".to_string(),
            parameters: Default::default(),
            mcp_tool_name: None,
            mcp_server_name: None,
            call_template: None,
            result_template: None,
            idempotent_hint: None,
            read_only_hint: None,
            kind: None,
        }]));
        agent.set_tools(tools.clone());
        agent.set_model(Model::new("openai", "gpt-4o"));
        agent.middleware.origin = Some(MiddlewareOrigin {
            file: PathBuf::from("/etc/harnx/agents/stateful.md"),
            base_dir: PathBuf::from("/etc/harnx"),
        });
        agent.hooks.as_mut().unwrap().origin = Some(MiddlewareOrigin {
            file: PathBuf::from("/packages/demo/agents/stateful.md"),
            base_dir: PathBuf::from("/packages/demo"),
        });
        let hooks_before = agent.hooks.clone();
        agent.set_user_principals(Some(developers_group_principals()));

        agent
            .apply_user_patches(&non_developer_principals())
            .expect("identity filter with $user binds");

        // Patched value took effect.
        assert_eq!(agent.use_tools(), Some(Vec::new()));
        // Serde-skipped runtime state survived the round trip.
        assert_eq!(
            agent
                .shared_variables()
                .get("shared_key")
                .map(String::as_str),
            Some("shared")
        );
        assert_eq!(
            agent.variables().get("session_key").map(String::as_str),
            Some("session"),
            "session variables must win over shared after restore"
        );
        assert_eq!(
            agent.tools().declarations()[0].name,
            "agent_owned",
            "agent-owned tool declarations must survive"
        );
        assert_eq!(
            agent.model().id(),
            "openai:gpt-4o",
            "unchanged model id keeps the resolved model"
        );
        assert_eq!(agent.model_id(), Some("openai:gpt-4o"));
        assert_eq!(
            agent.middleware.origin.as_ref().map(|origin| &origin.file),
            Some(&PathBuf::from("/etc/harnx/agents/stateful.md")),
            "middleware origin must survive"
        );
        assert_eq!(
            agent.hooks, hooks_before,
            "all lifecycle hook entries and their shared file/base-dir origin must survive"
        );
        assert_eq!(
            agent.user_principals(),
            Some(&developers_group_principals()),
            "the stamped snapshot must survive"
        );
    }

    #[test]
    fn apply_user_patches_replaced_lifecycle_hooks_keep_their_origin() {
        use crate::hooks::HookEventKind;
        use crate::middleware::{MiddlewareOrigin, MiddlewareSourceConfig};

        let origin = MiddlewareOrigin {
            file: "/packages/demo/agents/stateful.md".into(),
            base_dir: "/packages/demo".into(),
        };
        for event in HookEventKind::ALL {
            let mut agent =
                AgentConfig::from_markdown("stateful", "---\nhooks: {max_resume: 1}\n---\nBody\n")
                    .unwrap();
            agent.hooks.as_mut().unwrap().origin = Some(origin.clone());
            agent.patches = vec![format!(
                r#".hooks = {{"{}": [{{"name": "patched", "script": "hooks/patched.py"}}], "max_resume": 4}}"#,
                event.key()
            )];

            agent
                .apply_user_patches(&non_developer_principals())
                .unwrap();

            let hooks = agent.hooks().unwrap();
            assert_eq!(hooks.origin.as_ref(), Some(&origin), "{}", event.key());
            assert_eq!(hooks.max_resume, Some(4));
            let crate::hooks::EventHookEntry::Full(entry) = &hooks.list(event).entries[0] else {
                panic!("expected patched hook for {}", event.key());
            };
            assert_eq!(entry.name, "patched");
            assert_eq!(
                entry.source,
                MiddlewareSourceConfig::Script {
                    script: "hooks/patched.py".into()
                }
            );
        }
    }

    #[test]
    fn apply_user_patches_removed_hooks_are_not_restored_with_their_origin() {
        for filter in ["del(.hooks)", ".hooks = null"] {
            let mut agent = AgentConfig::from_markdown(
                "stateful",
                "---\nhooks:\n  Stop: [{name: audit, bash: event_input}]\n---\nBody\n",
            )
            .unwrap();
            agent.hooks.as_mut().unwrap().origin = Some(crate::middleware::MiddlewareOrigin {
                file: "/packages/demo/agents/stateful.md".into(),
                base_dir: "/packages/demo".into(),
            });
            agent.patches = vec![filter.into()];

            agent
                .apply_user_patches(&non_developer_principals())
                .unwrap();

            assert!(agent.hooks().is_none(), "{filter}");
        }
    }

    #[test]
    fn apply_user_patches_changed_model_id_leaves_the_model_for_the_caller() {
        use crate::model::Model;

        let mut agent = AgentConfig::from_markdown(
            "remodeled",
            "---\n\
             model: openai:gpt-4o\n\
             patches: '.model = \"openai:gpt-4o-mini\"'\n\
             ---\n\
             Body\n",
        )
        .unwrap();
        agent.set_model(Model::new("openai", "gpt-4o"));

        agent
            .apply_user_patches(&non_developer_principals())
            .unwrap();

        assert_eq!(agent.model_id(), Some("openai:gpt-4o-mini"));
        assert_eq!(
            agent.model().id(),
            Model::default().id(),
            "a changed model id must reset the resolved model"
        );
        assert_eq!(
            serde_json::to_value(agent.model().data()).unwrap(),
            serde_json::to_value(Model::default().data()).unwrap(),
            "resolved model metadata must reset too"
        );
    }

    #[test]
    fn apply_user_patches_late_errors_preserve_runtime_state() {
        use crate::middleware::MiddlewareOrigin;
        use crate::model::Model;
        use std::path::PathBuf;

        for filter in [".use_tools = 5", ".name = \"other\""] {
            let mut agent = AgentConfig::from_markdown(
                "stateful",
                "---\nhooks:\n  StopFailure:\n    - name: audit\n      python: pass\n---\nBody\n",
            )
            .unwrap();
            agent.patches = vec![filter.to_string()];
            agent.set_shared_variables(AgentVariables::from_iter([(
                "shared".into(),
                "value".into(),
            )]));
            agent.set_session_variables(AgentVariables::from_iter([(
                "session".into(),
                "value".into(),
            )]));
            agent.set_model(Model::new("openai", "gpt-4o"));
            agent.set_user_principals(Some(developers_group_principals()));
            agent.middleware.origin = Some(MiddlewareOrigin {
                file: PathBuf::from("/packages/demo/agents/stateful.md"),
                base_dir: PathBuf::from("/packages/demo"),
            });
            agent.hooks.as_mut().unwrap().origin = agent.middleware.origin.clone();
            let before = format!("{agent:?}");

            agent
                .apply_user_patches(&non_developer_principals())
                .unwrap_err();

            assert_eq!(
                format!("{agent:?}"),
                before,
                "late error from {filter:?} must preserve serde-skipped state too"
            );
        }
    }

    #[test]
    fn apply_user_patches_rejects_non_object_intermediate_outputs() {
        for (projection, kind) in [
            (".use_tools", "array"),
            (".name", "string"),
            ("42", "number"),
            ("false", "bool"),
            ("null", "null"),
        ] {
            let mut agent =
                AgentConfig::from_markdown("tailored", "---\nuse_tools: [fs_read]\n---\nBody\n")
                    .unwrap();
            agent.patches = vec![
                ".".into(),
                projection.into(),
                r#"{"name":"tailored","instructions":"reconstructed"}"#.into(),
            ];
            let before = format!("{agent:?}");

            let error = anyhow_chain(
                agent
                    .apply_user_patches(&non_developer_principals())
                    .unwrap_err(),
            );

            assert!(
                error.contains("tailored") && error.contains("patches"),
                "{error}"
            );
            assert!(
                error.contains("filter 2") && error.contains(projection),
                "{error}"
            );
            assert!(
                error.contains("JSON object") && error.contains(kind),
                "{error}"
            );
            assert_eq!(format!("{agent:?}"), before);
        }
    }

    #[test]
    fn apply_user_patches_chains_object_outputs() {
        let mut agent = patchable_agent_config();
        agent.patches = vec![
            ".use_tools = []".into(),
            ".instructions = $user.user_id".into(),
        ];

        agent
            .apply_user_patches(&non_developer_principals())
            .unwrap();

        assert_eq!(agent.use_tools(), Some(Vec::new()));
        assert_eq!(agent.interpolated_instructions().unwrap(), "mallory");
    }

    #[test]
    fn apply_user_patches_fails_closed() {
        fn filter_case(filter: &str) -> String {
            let mut agent = AgentConfig::from_markdown(
                "failing",
                &format!("---\npatches: {filter}\n---\nBody\n"),
            )
            .unwrap();
            let error = anyhow_chain(
                agent
                    .apply_user_patches(&non_developer_principals())
                    .unwrap_err(),
            );
            assert!(
                error.contains("failing"),
                "filter {filter:?}: error must name the agent: {error}"
            );
            error
        }

        // Runtime error inside the filter.
        let error = filter_case("'.use_tools |= map(.missing)'");
        assert!(
            error.contains("patches"),
            "runtime error must name `patches`: {error}"
        );
        // No output.
        let error = filter_case("'select(false)'");
        assert!(error.contains("no output"), "{error}");
        // More than one output.
        let error = filter_case("'. , .'");
        assert!(error.contains("multiple outputs"), "{error}");
        // Output that is not an object.
        let error = filter_case("'.name'");
        assert!(error.contains("JSON object"), "{error}");
        // Renaming the agent.
        let error = filter_case("'.name = \"other\"'");
        assert!(
            error.contains("must not change the agent `name`"),
            "{error}"
        );
        // Output that no longer deserializes as an agent config.
        let error = filter_case("'.use_tools = 5'");
        assert!(
            error.contains("invalid agent config"),
            "deserialize failure must be reported: {error}"
        );

        // The same filters never mutated the config they failed on.
        let mut agent =
            AgentConfig::from_markdown("failing", "---\npatches: '.name = \"other\"'\n---\nBody\n")
                .unwrap();
        agent
            .apply_user_patches(&non_developer_principals())
            .unwrap_err();
        assert_eq!(agent.name(), "failing");
        assert_eq!(
            agent.use_tools(),
            None,
            "a failed patch must leave the config unchanged"
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
