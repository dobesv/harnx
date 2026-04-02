use crate::{
    config::{Agent, Config, GlobalConfig},
    hooks::{dispatch::dispatch_hooks, HookEvent, HookResultControl},
    utils::*,
};

use anyhow::{anyhow, bail, Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};
use tokio::runtime::Handle;

#[cfg(windows)]
const PATH_SEP: &str = ";";
#[cfg(not(windows))]
const PATH_SEP: &str = ":";

pub fn eval_tool_calls(config: &GlobalConfig, mut calls: Vec<ToolCall>) -> Result<Vec<ToolResult>> {
    let mut output = vec![];
    if calls.is_empty() {
        return Ok(output);
    }
    calls = ToolCall::dedup(calls);
    if calls.is_empty() {
        bail!("The request was aborted because an infinite loop of function calls was detected.")
    }

    let hooks = config.read().resolved_hooks();
    let cwd = std::env::current_dir().unwrap_or_default();
    let session_id = "cmd".to_string();

    let mut is_all_null = true;
    for call in calls {
        let tool_input = call.arguments.clone();
        let tool_use_id = call.id.clone().unwrap_or_default();

        let pre_event = HookEvent::PreToolUse {
            tool_name: call.name.clone(),
            tool_input: tool_input.clone(),
            tool_use_id: tool_use_id.clone(),
        };
        let pre_outcome = tokio::task::block_in_place(|| {
            Handle::current().block_on(dispatch_hooks(
                &pre_event,
                &hooks.entries,
                &session_id,
                &cwd,
            ))
        });
        if let HookResultControl::Block { reason } = pre_outcome.control {
            let blocked_result = json!({"error": reason, "blocked_by_hook": true});
            output.push(ToolResult::new(call, blocked_result));
            is_all_null = false;
            continue;
        }
        if let HookResultControl::Ask { reason } = pre_outcome.control {
            if !crate::hooks::prompt::confirm_tool_use(
                &call.name,
                &call.arguments,
                reason.as_deref(),
            ) {
                let deny_reason = reason.unwrap_or_else(|| "Denied by user".to_string());
                let blocked_result = json!({"error": deny_reason, "blocked_by_hook": true});
                output.push(ToolResult::new(call, blocked_result));
                is_all_null = false;
                continue;
            }
        }

        let is_mcp = config
            .read()
            .mcp_manager
            .as_ref()
            .is_some_and(|m| m.has_tool(&call.name));
        let eval_result = if is_mcp {
            call.eval_mcp(config)
        } else {
            call.eval(config)
        };
        match eval_result {
            Ok(mut result) => {
                let post_event = HookEvent::PostToolUse {
                    tool_name: call.name.clone(),
                    tool_input: tool_input.clone(),
                    tool_response: result.clone(),
                    tool_use_id: tool_use_id.clone(),
                };
                let _ = tokio::task::block_in_place(|| {
                    Handle::current().block_on(dispatch_hooks(
                        &post_event,
                        &hooks.entries,
                        &session_id,
                        &cwd,
                    ))
                });

                if result.is_null() {
                    result = json!("DONE");
                } else {
                    is_all_null = false;
                }
                output.push(ToolResult::new(call, result));
            }
            Err(err) => {
                let fail_event = HookEvent::PostToolUseFailure {
                    tool_name: call.name.clone(),
                    tool_input: tool_input.clone(),
                    tool_use_id: tool_use_id.clone(),
                    error: err.to_string(),
                };
                let _ = tokio::task::block_in_place(|| {
                    Handle::current().block_on(dispatch_hooks(
                        &fail_event,
                        &hooks.entries,
                        &session_id,
                        &cwd,
                    ))
                });

                return Err(err);
            }
        }
    }
    if is_all_null {
        output = vec![];
    }
    Ok(output)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolResult {
    pub call: ToolCall,
    pub output: Value,
}

impl ToolResult {
    pub fn new(call: ToolCall, output: Value) -> Self {
        Self { call, output }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Tools {
    declarations: Vec<ToolDeclaration>,
    mcp_declarations: Vec<ToolDeclaration>,
}

impl Tools {
    pub fn init(declarations_path: &Path, mcp_tools: Option<Vec<ToolDeclaration>>) -> Result<Self> {
        let declarations: Vec<ToolDeclaration> = if declarations_path.exists() {
            let ctx = || {
                format!(
                    "Failed to load functions at {}",
                    declarations_path.display()
                )
            };
            let content = fs::read_to_string(declarations_path).with_context(ctx)?;
            serde_json::from_str(&content).with_context(ctx)?
        } else {
            vec![]
        };

        Ok(Self {
            declarations,
            mcp_declarations: mcp_tools.unwrap_or_default(),
        })
    }

    pub fn init_from_mcp(mcp_tools: Option<Vec<ToolDeclaration>>) -> Self {
        Self {
            declarations: vec![],
            mcp_declarations: mcp_tools.unwrap_or_default(),
        }
    }

    pub fn find(&self, name: &str) -> Option<&ToolDeclaration> {
        self.declarations
            .iter()
            .chain(self.mcp_declarations.iter())
            .find(|v| v.name == name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.declarations.iter().any(|v| v.name == name)
            || self.mcp_declarations.iter().any(|v| v.name == name)
    }

    pub fn declarations(&self) -> Vec<ToolDeclaration> {
        self.declarations
            .iter()
            .chain(self.mcp_declarations.iter())
            .cloned()
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.declarations.is_empty() && self.mcp_declarations.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: JsonSchema,
    #[serde(skip_serializing, default)]
    pub agent: bool,
    #[serde(skip, default)]
    pub mcp_tool_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonSchema {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<IndexMap<String, JsonSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<JsonSchema>>,
    #[serde(rename = "anyOf", skip_serializing_if = "Option::is_none")]
    pub any_of: Option<Vec<JsonSchema>>,
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_value: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
}

impl JsonSchema {
    pub fn is_empty_properties(&self) -> bool {
        match &self.properties {
            Some(v) => v.is_empty(),
            None => true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    pub id: Option<String>,
}

type CallConfig = (String, String, Vec<String>, HashMap<String, String>);

impl ToolCall {
    pub fn dedup(calls: Vec<Self>) -> Vec<Self> {
        let mut new_calls = vec![];
        let mut seen_ids = HashSet::new();

        for call in calls.into_iter().rev() {
            if let Some(id) = &call.id {
                if !seen_ids.contains(id) {
                    seen_ids.insert(id.clone());
                    new_calls.push(call);
                }
            } else {
                new_calls.push(call);
            }
        }

        new_calls.reverse();
        new_calls
    }

    pub fn new(name: String, arguments: Value, id: Option<String>) -> Self {
        Self {
            name,
            arguments,
            id,
        }
    }

    pub fn eval(&self, config: &GlobalConfig) -> Result<Value> {
        let (call_name, cmd_name, mut cmd_args, envs) = match &config.read().agent {
            Some(agent) => self.extract_call_config_from_agent(config, agent)?,
            None => self.extract_call_config_from_config(config)?,
        };

        let json_data = if self.arguments.is_object() {
            self.arguments.clone()
        } else if let Some(arguments) = self.arguments.as_str() {
            let arguments: Value = serde_json::from_str(arguments).map_err(|_| {
                anyhow!("The call '{call_name}' has invalid arguments: {arguments}")
            })?;
            arguments
        } else {
            bail!(
                "The call '{call_name}' has invalid arguments: {}",
                self.arguments
            );
        };

        cmd_args.push(json_data.to_string());

        let output = match run_llm_tool(cmd_name, cmd_args, envs)? {
            Some(contents) => serde_json::from_str(&contents)
                .ok()
                .unwrap_or_else(|| json!({"output": contents})),
            None => Value::Null,
        };

        Ok(output)
    }

    fn eval_mcp(&self, config: &GlobalConfig) -> Result<Value> {
        let mcp_manager = config.read().mcp_manager.clone();
        let manager = match mcp_manager {
            Some(m) => m,
            None => bail!("MCP is not configured"),
        };

        let json_data = if self.arguments.is_object() {
            self.arguments.clone()
        } else if let Some(arguments) = self.arguments.as_str() {
            serde_json::from_str(arguments).map_err(|_| {
                anyhow!(
                    "The call '{}' has invalid arguments: {arguments}",
                    self.name
                )
            })?
        } else {
            bail!(
                "The call '{}' has invalid arguments: {}",
                self.name,
                self.arguments
            );
        };

        if *IS_STDOUT_TERMINAL {
            let prompt = format!("Call {} {}", self.name, json_data);
            println!("{}", dimmed_text(&prompt));
        }

        let tool_name = self.name.clone();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                tokio::select! {
                    result = manager.call_tool(&tool_name, json_data) => result,
                    _ = tokio::signal::ctrl_c() => {
                        bail!("MCP tool call aborted by user")
                    }
                }
            })
        })?;

        Ok(result)
    }

    fn extract_call_config_from_agent(
        &self,
        config: &GlobalConfig,
        agent: &Agent,
    ) -> Result<CallConfig> {
        let function_name = self.name.clone();
        match agent.tools().find(&function_name) {
            Some(function) => {
                let agent_name = agent.name().to_string();
                if function.agent {
                    Ok((
                        format!("{agent_name}-{function_name}"),
                        agent_name,
                        vec![function_name],
                        agent.variable_envs(),
                    ))
                } else {
                    Ok((
                        function_name.clone(),
                        function_name,
                        vec![],
                        Default::default(),
                    ))
                }
            }
            None => self.extract_call_config_from_config(config),
        }
    }

    fn extract_call_config_from_config(&self, config: &GlobalConfig) -> Result<CallConfig> {
        let function_name = self.name.clone();
        match config.read().tools.contains(&function_name) {
            true => Ok((
                function_name.clone(),
                function_name,
                vec![],
                Default::default(),
            )),
            false => bail!("Unexpected call: {function_name} {}", self.arguments),
        }
    }
}

pub fn run_llm_tool(
    cmd_name: String,
    cmd_args: Vec<String>,
    mut envs: HashMap<String, String>,
) -> Result<Option<String>> {
    let prompt = format!("Call {cmd_name} {}", cmd_args.join(" "));

    let mut bin_dirs: Vec<PathBuf> = vec![];
    if cmd_args.len() > 1 {
        let dir = Config::agent_tools_dir(&cmd_name).join("bin");
        if dir.exists() {
            bin_dirs.push(dir);
        }
    }
    bin_dirs.push(Config::tools_bin_dir());
    let current_path = std::env::var("PATH").context("No PATH environment variable")?;
    let prepend_path = bin_dirs
        .iter()
        .map(|v| format!("{}{PATH_SEP}", v.display()))
        .collect::<Vec<_>>()
        .join("");
    envs.insert("PATH".into(), format!("{prepend_path}{current_path}"));

    let temp_file = temp_file("-eval-", "");
    envs.insert("LLM_OUTPUT".into(), temp_file.display().to_string());

    #[cfg(windows)]
    let cmd_name = polyfill_cmd_name(&cmd_name, &bin_dirs);
    if *IS_STDOUT_TERMINAL {
        println!("{}", dimmed_text(&prompt));
    }
    let exit_code = run_command(&cmd_name, &cmd_args, Some(envs))
        .map_err(|err| anyhow!("Unable to run {cmd_name}, {err}"))?;
    if exit_code != 0 {
        bail!("Tool call exit with {exit_code}");
    }
    let mut output = None;
    if temp_file.exists() {
        let contents =
            fs::read_to_string(temp_file).context("Failed to retrieve tool call output")?;
        if !contents.is_empty() {
            output = Some(contents);
        }
    };
    Ok(output)
}

#[cfg(windows)]
fn polyfill_cmd_name<T: AsRef<Path>>(cmd_name: &str, bin_dir: &[T]) -> String {
    let cmd_name = cmd_name.to_string();
    if let Ok(exts) = std::env::var("PATHEXT") {
        for name in exts.split(';').map(|ext| format!("{cmd_name}{ext}")) {
            for dir in bin_dir {
                let path = dir.as_ref().join(&name);
                if path.exists() {
                    return name.to_string();
                }
            }
        }
    }
    cmd_name
}
