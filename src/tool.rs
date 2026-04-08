use crate::{
    config::GlobalConfig,
    hooks::{dispatch::dispatch_hooks, HookEvent, HookResultControl},
    mcp_safety::{truncate_output, TruncateOpts},
    ui_output::emit_ui_output,
    utils::*,
};

use anyhow::{anyhow, bail, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Write as _;
use textwrap::{wrap, Options};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedReceiver;

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

        let eval_result = call.eval_mcp(config);
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

                // Emit tool result to TUI or terminal
                let mut opts = TruncateOpts::default();
                let marker = " [...] ";
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    opts.head_lines = 5.max((rows / 2) as usize);
                    opts.tail_lines = 0;
                    // "<= " prefix is 3 chars, marker is 7 chars; total overhead = 10
                    // line_head_bytes + marker.len() + prefix.len() must fit in cols
                    opts.line_head_bytes = (cols as usize).saturating_sub(3 + marker.len());
                    opts.line_tail_bytes = 0;
                    opts.marker = Some(marker.to_string());
                }
                let output_str =
                    extract_user_display_text(&result).unwrap_or_else(|| match &result {
                        Value::String(s) => s.clone(),
                        _ => result.to_string(),
                    });
                let truncated = truncate_output(&output_str, &opts);
                let text = format!("{}\n", dimmed_text(&truncated));
                if !emit_ui_output(text.clone()) && *IS_STDOUT_TERMINAL {
                    print!("{text}");
                }

                if result.is_null() {
                    result = json!("DONE");
                } else {
                    is_all_null = false;
                }
                let mut result_obj = ToolResult::new(call, result);
                if let Some(obj) = result_obj.output.as_object() {
                    if obj.get("action").and_then(|v| v.as_str()) == Some("switch_agent") {
                        if let (Some(agent), Some(prompt)) = (
                            obj.get("agent").and_then(|v| v.as_str()),
                            obj.get("prompt").and_then(|v| v.as_str()),
                        ) {
                            result_obj.switch_agent = Some(SwitchAgentData {
                                agent: agent.to_string(),
                                prompt: prompt.to_string(),
                            });
                        }
                    }
                }
                output.push(result_obj);
            }
            Err(ToolError::Recoverable(err)) => {
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

                is_all_null = false;
                let error_result = json!({
                    "is_error": true,
                    "error": err.to_string(),
                });
                output.push(ToolResult::new(call, error_result));
            }
            Err(ToolError::Fatal(err)) => return Err(err),
        }
    }
    if is_all_null {
        output = vec![];
    }
    Ok(output)
}

pub const TRIGGER_AGENT_TOOL_NAME: &str = "trigger_agent";

pub fn trigger_agent_tool_declaration() -> ToolDeclaration {
    let mut properties = IndexMap::new();
    properties.insert(
        "agent".to_string(),
        JsonSchema {
            type_value: Some("string".to_string()),
            description: Some("The name of the agent to transfer the session to.".to_string()),
            ..Default::default()
        },
    );
    properties.insert(
        "prompt".to_string(),
        JsonSchema {
            type_value: Some("string".to_string()),
            description: Some("The new prompt to start the new agent with.".to_string()),
            ..Default::default()
        },
    );
    ToolDeclaration {
        name: TRIGGER_AGENT_TOOL_NAME.to_string(),
        description: "Transfer the session to another agent with a new prompt in an empty session."
            .to_string(),
        parameters: JsonSchema {
            type_value: Some("object".to_string()),
            properties: Some(properties),
            required: Some(vec!["agent".to_string(), "prompt".to_string()]),
            ..Default::default()
        },
        mcp_tool_name: None,
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolResult {
    pub call: ToolCall,
    pub output: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_agent: Option<SwitchAgentData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchAgentData {
    pub agent: String,
    pub prompt: String,
}

impl ToolResult {
    pub fn new(call: ToolCall, output: Value) -> Self {
        Self {
            call,
            output,
            switch_agent: None,
        }
    }
}

pub enum ToolError {
    Recoverable(anyhow::Error),
    Fatal(anyhow::Error),
}

#[derive(Debug, Clone, Default)]
pub struct Tools {
    declarations: Vec<ToolDeclaration>,
}

impl Tools {
    pub fn init_from_mcp(mcp_tools: Option<Vec<ToolDeclaration>>) -> Self {
        Self {
            declarations: mcp_tools.unwrap_or_default(),
        }
    }

    pub fn declarations(&self) -> Vec<ToolDeclaration> {
        self.declarations.clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: JsonSchema,
    #[serde(skip, default)]
    pub mcp_tool_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

fn display_wrap_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&w| w >= 40)
        .unwrap_or(88)
}

fn wrap_display_text(text: &str, initial_indent: &str, subsequent_indent: &str) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    let options = Options::new(display_wrap_width())
        .initial_indent(initial_indent)
        .subsequent_indent(subsequent_indent)
        .break_words(false)
        .word_splitter(textwrap::WordSplitter::NoHyphenation);
    wrap(text, options).join("\n")
}

fn pretty_yaml_block(value: &Value) -> String {
    serde_yaml::to_string(value)
        .map(|s| s.trim_end().to_string())
        .unwrap_or_else(|_| value.to_string())
}

fn format_tool_invocation_display(tool_name: &str, json_data: &Value) -> String {
    let header = wrap_display_text(&format!("🛠️  {tool_name}"), "", "   ");
    match json_data {
        Value::Null => format!("{}\n", dimmed_text(&header)),
        _ => {
            let body = pretty_yaml_block(json_data)
                .lines()
                .map(|line| format!("   {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{}\n{}\n", dimmed_text(&header), dimmed_text(&body))
        }
    }
}

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

    pub fn new(
        name: String,
        arguments: Value,
        id: Option<String>,
        thought_signature: Option<String>,
    ) -> Self {
        Self {
            name,
            arguments,
            id,
            thought_signature,
        }
    }

    fn eval_mcp(&self, config: &GlobalConfig) -> Result<Value, ToolError> {
        let json_data = if self.arguments.is_null() {
            Value::Null
        } else if self.arguments.is_object() {
            self.arguments.clone()
        } else if let Some(arguments) = self.arguments.as_str() {
            serde_json::from_str(arguments).map_err(|_| {
                ToolError::Recoverable(anyhow!(
                    "The call '{}' has invalid arguments: {arguments}",
                    self.name
                ))
            })?
        } else {
            return Err(ToolError::Recoverable(anyhow!(
                "The call '{}' has invalid arguments: {}",
                self.name,
                self.arguments
            )));
        };

        // Emit tool call info to TUI or terminal
        let text = format_tool_invocation_display(&self.name, &json_data);
        if !emit_ui_output(text.clone()) && *IS_STDOUT_TERMINAL {
            print!("{text}");
        }

        if self.name == TRIGGER_AGENT_TOOL_NAME {
            let agent = json_data["agent"].as_str().ok_or_else(|| {
                ToolError::Recoverable(anyhow!("Missing 'agent' argument for trigger_agent"))
            })?;
            let prompt = json_data["prompt"].as_str().ok_or_else(|| {
                ToolError::Recoverable(anyhow!("Missing 'prompt' argument for trigger_agent"))
            })?;

            return Ok(json!({
                "status": "success",
                "message": format!("Transferring session to agent '{}'...", agent),
                "action": "switch_agent",
                "agent": agent,
                "prompt": prompt
            }));
        }

        let acp_manager = config.read().acp_manager.clone();
        if let Some(manager) = acp_manager {
            if manager.find_client_for_tool(&self.name).is_some() {
                let tool_name = self.name.clone();
                // call_tool internally races session_prompt against
                // Ctrl+C and cancels the ACP session (including
                // auto-created sessions) when the user aborts.
                //
                // Subscribe to ACP chunk notifications so that tool calls,
                // agent thoughts, plan updates, and text chunks from the
                // sub-agent (and any nested sub-sub-agents) are printed to
                // stdout in real-time instead of being silently swallowed.
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        let has_ui_output = emit_ui_output("");
                        let is_terminal = *IS_STDOUT_TERMINAL && !has_ui_output;
                        let (chunk_rx, subscription_id) = manager.subscribe_chunks().await;

                        // Spawn a spinner only in non-TUI terminal mode.
                        let spinner = if is_terminal {
                            Some(spawn_spinner(&format!("  {} working…", tool_name)))
                        } else {
                            None
                        };

                        // Forward chunks either to TUI output sink or stdout.
                        let spinner_clone = spinner.clone();
                        let spinner_msg = format!("  {} working…", tool_name);
                        let forward_handle =
                            tokio::spawn(forward_acp_chunks(chunk_rx, spinner_clone, spinner_msg));

                        let call_result = manager.call_tool(&tool_name, json_data).await;

                        // Tear down: unsubscribe first (closes the
                        // channel), then await the forward task.
                        manager.unsubscribe_chunks(subscription_id).await;
                        let _ = forward_handle.await;

                        if let Some(s) = spinner {
                            s.stop();
                        }

                        call_result
                    })
                })
                .map_err(|err| {
                    // Only user-initiated aborts (Ctrl+C) are fatal and should
                    // stop the entire tool batch.  Other failures (timeouts,
                    // connection errors, bad arguments) are recoverable so the
                    // LLM receives the error message and can retry.
                    if err.to_string().contains("aborted by user") {
                        ToolError::Fatal(err)
                    } else {
                        ToolError::Recoverable(err)
                    }
                })?;

                return Ok(result);
            }
        }

        let mcp_manager = config.read().mcp_manager.clone();
        let manager = match mcp_manager {
            Some(m) => m,
            None => {
                return Err(ToolError::Recoverable(anyhow!(
                    "No tool provider configured for '{}'",
                    self.name
                )))
            }
        };

        let tool_name = self.name.clone();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                tokio::select! {
                    result = manager.call_tool(&tool_name, json_data) => result.map_err(ToolError::Recoverable),
                    _ = tokio::signal::ctrl_c() => {
                        Err(ToolError::Fatal(anyhow!("MCP tool call aborted by user")))
                    }
                }
            })
        })?;

        Ok(result)
    }
}

/// Forwards ACP chunk notifications to stdout with spinner management.
///
/// Each incoming chunk temporarily clears the spinner, prints the chunk,
/// then restores the spinner.  This gives the user live visibility into
/// sub-agent (and sub-sub-agent) tool calls, thoughts, and plan updates.
async fn forward_acp_chunks(
    mut chunk_rx: UnboundedReceiver<String>,
    spinner: Option<Spinner>,
    spinner_msg: String,
) {
    while let Some(chunk) = chunk_rx.recv().await {
        // Pause the spinner (clear display but keep task alive) before
        // printing so output is clean, then resume it afterwards.
        if let Some(ref s) = spinner {
            s.pause();
        }
        if !emit_ui_output(chunk.clone()) {
            print!("{chunk}");
            let _ = std::io::stdout().flush();
        }
        // Re-enable the spinner after printing.
        if let Some(ref s) = spinner {
            let _ = s.set_message(spinner_msg.clone());
        }
    }
}

/// Extracts user-visible text from an MCP `CallToolResult` value.
///
/// The result value has the shape:
/// ```json
/// { "content": [{ "type": "text", "text": "...", "annotations": { "audience": ["user"] } }] }
/// ```
///
/// Content parts whose `annotations.audience` exists but does NOT contain `"user"` are skipped.
/// Parts with no annotations or with audience containing `"user"` are included.
/// Returns `Some(joined_text)` if any text was extracted, `None` otherwise.
fn extract_user_display_text(result: &Value) -> Option<String> {
    let content = result.get("content")?.as_array()?;
    let mut parts: Vec<&str> = Vec::new();
    for item in content {
        // Check audience annotation: if present and does NOT contain "user", skip
        if let Some(annotations) = item.get("annotations") {
            if let Some(audience) = annotations.get("audience") {
                if let Some(audience_arr) = audience.as_array() {
                    if !audience_arr.iter().any(|v| v.as_str() == Some("user")) {
                        continue;
                    }
                }
            }
        }
        // Extract text from this content part
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use parking_lot::RwLock;
    use std::sync::Arc;

    #[test]
    fn test_format_tool_invocation_display_multiline_yaml() {
        let rendered = format_tool_invocation_display(
            "argus_session_prompt",
            &json!({
                "message": "Goal — Improve display\nAcceptance criteria — Wrap nicely",
                "session_id": "session-1"
            }),
        );

        assert!(rendered.contains("argus_session_prompt"));
        assert!(rendered.contains("message:"));
        assert!(rendered.contains("Acceptance criteria"));
        assert!(rendered.contains("session_id:"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_eval_tool_calls_error_handling() {
        let _guard = crate::client::TestStateGuard::new(None).await;
        let config = Arc::new(RwLock::new(Config::default()));
        let call = ToolCall::new(
            "unknown_tool".to_string(),
            json!({}),
            Some("1".to_string()),
            None,
        );
        let calls = vec![call];

        let result = eval_tool_calls(&config, calls).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].call.name, "unknown_tool");
        assert!(result[0].output.is_object());
        assert_eq!(result[0].output["is_error"], true);
        assert!(result[0].output["error"]
            .as_str()
            .unwrap()
            .contains("No tool provider configured"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_eval_tool_calls_partial_error_handling() {
        let _guard = crate::client::TestStateGuard::new(None).await;
        let config = Arc::new(RwLock::new(Config::default()));
        // trigger_agent is handled internally and should succeed
        let call1 = ToolCall::new(
            TRIGGER_AGENT_TOOL_NAME.to_string(),
            json!({"agent": "test", "prompt": "test"}),
            Some("1".to_string()),
            None,
        );
        let call2 = ToolCall::new(
            "unknown_tool".to_string(),
            json!({}),
            Some("2".to_string()),
            None,
        );
        let calls = vec![call1, call2];

        let result = eval_tool_calls(&config, calls).unwrap();
        assert_eq!(result.len(), 2);

        assert_eq!(result[0].call.name, TRIGGER_AGENT_TOOL_NAME);
        assert_eq!(result[0].output["action"], "switch_agent");

        assert_eq!(result[1].call.name, "unknown_tool");
        assert_eq!(result[1].output["is_error"], true);
    }

    #[test]
    fn test_extract_user_display_text_basic() {
        let result = json!({
            "content": [
                {"type": "text", "text": "hello world"}
            ],
            "isError": false
        });
        assert_eq!(
            extract_user_display_text(&result),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn test_extract_user_display_text_multiple_parts() {
        let result = json!({
            "content": [
                {"type": "text", "text": "line one"},
                {"type": "text", "text": "line two"}
            ]
        });
        assert_eq!(
            extract_user_display_text(&result),
            Some("line one\nline two".to_string())
        );
    }

    #[test]
    fn test_extract_user_display_text_user_audience() {
        let result = json!({
            "content": [
                {
                    "type": "text",
                    "text": "visible",
                    "annotations": {"audience": ["user"]}
                }
            ]
        });
        assert_eq!(
            extract_user_display_text(&result),
            Some("visible".to_string())
        );
    }

    #[test]
    fn test_extract_user_display_text_assistant_only_audience() {
        let result = json!({
            "content": [
                {
                    "type": "text",
                    "text": "hidden from display",
                    "annotations": {"audience": ["assistant"]}
                }
            ]
        });
        assert_eq!(extract_user_display_text(&result), None);
    }

    #[test]
    fn test_extract_user_display_text_mixed_audiences() {
        let result = json!({
            "content": [
                {
                    "type": "text",
                    "text": "user sees this",
                    "annotations": {"audience": ["user"]}
                },
                {
                    "type": "text",
                    "text": "assistant only",
                    "annotations": {"audience": ["assistant"]}
                },
                {
                    "type": "text",
                    "text": "no annotations"
                }
            ]
        });
        assert_eq!(
            extract_user_display_text(&result),
            Some("user sees this\nno annotations".to_string())
        );
    }

    #[test]
    fn test_extract_user_display_text_both_audiences() {
        let result = json!({
            "content": [
                {
                    "type": "text",
                    "text": "for both",
                    "annotations": {"audience": ["user", "assistant"]}
                }
            ]
        });
        assert_eq!(
            extract_user_display_text(&result),
            Some("for both".to_string())
        );
    }

    #[test]
    fn test_extract_user_display_text_no_content() {
        let result = json!({"isError": false});
        assert_eq!(extract_user_display_text(&result), None);
    }

    #[test]
    fn test_extract_user_display_text_empty_content() {
        let result = json!({"content": []});
        assert_eq!(extract_user_display_text(&result), None);
    }

    #[test]
    fn test_extract_user_display_text_non_mcp_value() {
        // Plain string value — not MCP format
        let result = json!("just a string");
        assert_eq!(extract_user_display_text(&result), None);
    }

    #[test]
    fn test_extract_user_display_text_content_without_text() {
        let result = json!({
            "content": [
                {"type": "image", "data": "base64..."}
            ]
        });
        assert_eq!(extract_user_display_text(&result), None);
    }

    #[test]
    fn test_extract_user_display_text_annotations_without_audience() {
        let result = json!({
            "content": [
                {
                    "type": "text",
                    "text": "has annotations but no audience",
                    "annotations": {"priority": 0.5}
                }
            ]
        });
        assert_eq!(
            extract_user_display_text(&result),
            Some("has annotations but no audience".to_string())
        );
    }
}
