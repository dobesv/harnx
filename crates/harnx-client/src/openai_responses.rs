//! OpenAI Responses API (`/v1/responses`) request body builder.
//!
//! The Responses API is an alternative to `/v1/chat/completions` for models
//! that support reasoning effort + function tools (e.g., gpt-5.6-sol). The
//! wire shape differs significantly:
//!
//! - `input` array (not `messages`) with typed items: `message`, `function_call`,
//!   `function_call_output`.
//! - Top-level `instructions` for system prompt (not embedded in `input`).
//! - `max_output_tokens` (not `max_tokens`).
//! - Flat `tools` array: `{type:"function", name, description, parameters}` —
//!   no nested `function` wrapper like chat/completions.
//! - `store: false` hard default (server defaults to `true`).
//! - `include: ["reasoning.encrypted_content"]` for stateless reasoning replay.
//! - Tool calls round-trip as `function_call` output items followed by
//!   `function_call_output` items in the next user turn.
//!
//! This module provides `openai_build_responses_body` for constructing the
//! Responses request body from `ChatCompletionsData`, plus extraction,
//! reasoning replay, and streaming support.

use crate::*;
use anyhow::{bail, Result};
use harnx_core::text::strip_think_tag;
use harnx_core::tool::ToolResult;

use serde_json::{json, Value};

/// Build an OpenAI Responses API request body from `ChatCompletionsData`.
///
/// Transforms the standard harnx `ChatCompletionsData` into the Responses
/// wire shape:
///
/// ```json
/// {
///   "model": "gpt-5.6-sol",
///   "instructions": "System prompt (extracted from first message)",
///   "input": [
///     { "type": "message", "role": "user", "content": [{"type": "input_text", "text": "..."}] },
///     { "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "..."}] },
///     { "type": "function_call", "call_id": "...", "name": "...", "arguments": "..." },
///     { "type": "function_call_output", "call_id": "...", "output": "..." }
///   ],
///   "tools": [
///     { "type": "function", "name": "...", "description": "...", "parameters": {...} }
///   ],
///   "max_output_tokens": 16384,
///   "store": false,
///   "include": ["reasoning.encrypted_content"],
///   "stream": true
/// }
/// ```
///
/// # Wire-shape notes
///
/// - `instructions`: top-level, not in `input`; extracted from leading system message.
/// - `input`: array of typed items (`message`, `function_call`, `function_call_output`).
/// - `tools`: flat array, each entry `{type:"function", name, description, parameters}`.
///   Unlike chat/completions, the function fields are NOT nested under a `function` key.
/// - `store`: hard default `false` (server default is `true`).
/// - `include`: always `["reasoning.encrypted_content"]`.
/// - `max_output_tokens`: from `model.max_tokens_param()`, clamped to min 16.
/// - `reasoning`: NOT emitted by this builder; alias patches assign the whole
///   object, e.g. `.body.reasoning = {"effort":"high"}`. jaq won't create the
///   missing parent for `.body.reasoning.effort = ...`.
/// - `temperature`/`top_p`: included only if provided; alias patches may `del(.body.temperature)`
///   for reasoning models that reject these params.
/// - `seed`: NOT supported by Responses API; never emitted.
///
/// # Tool-call history round-trip
///
/// When `MessageContent::ToolCalls` is present in history:
/// 1. Reasoning replay consumes `thought`/`thought_signature`.
/// 2. One `function_call` item per tool call (from `tool_results[].call`).
/// 3. One `function_call_output` item per tool result.
///
/// # Extension points
///
/// - Reasoning replay inserts input items BEFORE `function_call` items.
/// - Extract/stream functions handle Responses output format.
pub fn openai_build_responses_body(data: ChatCompletionsData, model: &Model) -> Value {
    let ChatCompletionsData {
        mut messages,
        temperature,
        top_p,
        functions,
        stream,
        attachments_dir: _, // Responses uses same runtime pre-pass as chat
    } = data;

    let messages_len = messages.len();

    // Extract leading system message -> top-level `instructions`
    let system_message = extract_system_message(&mut messages);
    let instructions = system_message.map(|parts| parts.join("\n\n"));

    // Build `input` array from remaining messages
    let input: Vec<Value> = messages
        .into_iter()
        .enumerate()
        .flat_map(|(i, message)| {
            let Message { role, content, .. } = message;
            match content {
                MessageContent::ToolCalls(MessageContentToolCalls {
                    tool_results,
                    text: _,
                    thought: _,
                    sequence: _,
                }) => {
                    // Build input items for tool-call history:
                    // 1. reasoning replay — encrypted content from prior turn's thought_signature
                    // 2. one function_call per tool_result.call
                    // 3. one function_call_output per tool_result

                    let mut items: Vec<Value> = Vec::new();

                    // Reasoning replay: emit a reasoning input item BEFORE function_call items
                    // when any tool_result in this turn has a thought_signature.
                    // The encrypted_content enables stateless reasoning replay across tool turns
                    // (server-side state is not preserved with store:false).
                    //
                    // Wire shape (per OpenAI Responses API multi-turn docs):
                    // { "type": "reasoning", "encrypted_content": "<blob>", "summary": [] }
                    //
                    // Source: first non-empty tool_result.call.thought_signature in this turn.
                    // Mirrors Claude/Bedrock which attach one signature per tool turn.
                    let encrypted_content = tool_results
                        .iter()
                        .filter_map(|r| r.call.thought_signature.as_ref())
                        .next();

                    if let Some(encrypted) = encrypted_content {
                        items.push(json!({
                            "type": "reasoning",
                            "encrypted_content": encrypted,
                            "summary": [],
                        }));
                    }

                    // Emit function_call + function_call_output items
                    // When `sequence` is true, each tool result represents a separate turn
                    // (tool call + tool output). We emit paired items for each.
                    for tool_result in &tool_results {
                        // The call_id comes from tool_result.call.id
                        // Responses API requires call_id at top level
                        let call_id = tool_result.call.id.clone().unwrap_or_default();
                        let name = tool_result.call.name.clone();
                        let arguments = tool_result.call.arguments.to_string();

                        items.push(json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments,
                        }));
                    }

                    // Now emit function_call_output for each tool result
                    for tool_result in tool_results {
                        let call_id = tool_result.call.id.clone().unwrap_or_default();
                        let output = responses_tool_result_output(&tool_result);

                        items.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": output,
                        }));
                    }

                    items
                }
                MessageContent::Text(text) if role.is_assistant() && i != messages_len - 1 => {
                    // Non-final assistant text: strip think tag
                    let stripped = strip_think_tag(&text);
                    vec![json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": stripped.as_ref(),
                        }]
                    })]
                }
                MessageContent::Text(text) if role.is_assistant() => {
                    // Final assistant text: emit as-is (think tag preserved for final)
                    vec![json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": text,
                        }]
                    })]
                }
                MessageContent::Text(text) if role.is_user() => {
                    vec![json!({
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": text,
                        }]
                    })]
                }
                MessageContent::Array(parts) => {
                    // Multimodal: emit input_text and input_image parts
                    #[allow(clippy::unnecessary_filter_map)]
                    let content_parts: Vec<Value> = parts
                        .into_iter()
                        .filter_map(|part| match part {
                            MessageContentPart::Text { text } => Some(json!({
                                "type": "input_text",
                                "text": text,
                            })),
                            MessageContentPart::ImageUrl { image_url } => Some(json!({
                                "type": "input_image",
                                "image_url": image_url.url,
                            })),
                        })
                        .collect();
                    vec![json!({
                        "type": "message",
                        "role": role,
                        "content": content_parts,
                    })]
                }
                _ => {
                    // Fallback: emit as text content
                    vec![json!({
                        "type": "message",
                        "role": role,
                        "content": [{
                            "type": content_type_for_role(&role),
                            "text": content.to_text(),
                        }]
                    })]
                }
            }
        })
        .collect();

    let mut body = json!({
        "model": model.real_name(),
        "input": input,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });

    // Top-level instructions (system message)
    if let Some(instructions) = instructions {
        body["instructions"] = instructions.into();
    }

    // max_output_tokens: from model.max_tokens_param(), clamped to min 16
    // Responses API rejects max_output_tokens < 16
    if let Some(v) = model.max_tokens_param() {
        let clamped = v.max(16);
        body["max_output_tokens"] = clamped.into();
    }

    // temperature/top_p: include only if provided
    // Note: alias patches may `del(.body.temperature)` for reasoning models
    // that reject temperature/top_p. We emit them if provided and let patches
    // remove them as needed.
    if let Some(v) = temperature {
        body["temperature"] = v.into();
    }
    if let Some(v) = top_p {
        body["top_p"] = v.into();
    }

    // stream flag
    if stream {
        body["stream"] = true.into();
    }

    // tools: flat array, `{type:"function", name, description, parameters}`
    // Responses flattens tool definitions — NOT nested under `function` key.
    if let Some(functions) = functions {
        body["tools"] = functions
            .into_iter()
            .map(|func| {
                // Responses wants flat structure:
                // {type: "function", name, description, parameters}
                // NOT {type: "function", function: {...}}
                json!({
                    "type": "function",
                    "name": func.name,
                    "description": func.description,
                    "parameters": func.parameters,
                })
            })
            .collect();
    }

    body
}

/// Return the content type for a given role.
///
/// Responses API uses `input_text` for user messages and `output_text` for
/// assistant messages. System messages are extracted to `instructions` top-level.
fn content_type_for_role(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User => "input_text",
        MessageRole::Assistant => "output_text",
        MessageRole::System => "input_text", // Shouldn't happen (extracted)
        MessageRole::Tool => "input_text",   // Tool role shouldn't appear directly
    }
}

/// Convert a tool result's output to a string for `function_call_output`.
///
/// Mirrors `openai_tool_message_content` from openai.rs but returns the
/// string directly (not wrapped for chat/completions wire shape).
fn responses_tool_result_output(tool_result: &ToolResult) -> String {
    // Use the same logic as openai.rs: stringify the output Value.
    // For complex outputs, the JSON string representation is used.
    tool_result.output.to_string()
}

/// Parse a non-streaming OpenAI Responses API JSON into `ChatCompletionsOutput`.
///
/// Mirrors `gemini_extract_chat_completions_text` but reads the Responses `output[]` array.
/// The Responses API output shape differs from chat/completions:
///
/// ```json
/// {
///   "id": "resp_abc123",
///   "output": [
///     { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "..." }], "encrypted_content": "..." },
///     { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "..." }] },
///     { "type": "function_call", "call_id": "...", "name": "...", "arguments": "{...}" }
///   ],
///   "usage": {
///     "input_tokens": 100,
///     "output_tokens": 50,
///     "input_tokens_details": { "cached_tokens": 20 }
///   }
/// }
/// ```
///
/// # Output item types
///
/// - `message`: Extract `output_text` parts into `text`.
/// - `reasoning`: Extract summary text into `thought`; capture `encrypted_content` for replay.
///   When a `function_call` follows, the encrypted content becomes the tool call's
///   `thought_signature` (mirrors Gemini `thoughtSignature` handling).
/// - `function_call`: Parse arguments JSON, capture `call_id`, and associate prior reasoning's
///   `encrypted_content` as `thought_signature`.
///
/// # Usage field names
///
/// Responses API uses `input_tokens` / `output_tokens` (not `prompt_tokens` / `completion_tokens`).
/// Cached tokens are under `usage.input_tokens_details.cached_tokens`.
///
/// # Wire-shape note
///
/// The `encrypted_content` field on reasoning items contains an opaque blob for reasoning replay.
/// This is captured into `thought_signature` on the tool call to enable stateless replay across
/// tool turns.
pub fn openai_extract_responses(data: &Value) -> Result<ChatCompletionsOutput> {
    let mut text_parts = vec![];
    let mut thought_parts = vec![];
    let mut tool_calls = vec![];
    // Track the most recent encrypted_content for associating with function_call
    // (Responses may interleave reasoning + function_call items)
    let mut pending_encrypted_content: Option<String> = None;

    if let Some(output_items) = data["output"].as_array() {
        for item in output_items {
            let item_type = item["type"].as_str().unwrap_or("");

            match item_type {
                "message" => {
                    // Extract output_text content parts
                    if let Some(content_parts) = item["content"].as_array() {
                        for content_part in content_parts {
                            if content_part["type"].as_str() == Some("output_text") {
                                if let Some(text) = content_part["text"].as_str() {
                                    text_parts.push(text);
                                }
                            }
                        }
                    }
                }
                "reasoning" => {
                    // Extract summary text into thought
                    // The summary is an array of summary_text parts
                    if let Some(summary_parts) = item["summary"].as_array() {
                        for summary_part in summary_parts {
                            if summary_part["type"].as_str() == Some("summary_text") {
                                if let Some(text) = summary_part["text"].as_str() {
                                    thought_parts.push(text);
                                }
                            }
                        }
                    }
                    // Capture encrypted_content for tool call thought_signature
                    // Wire-shape note: field name is `encrypted_content` per Responses API spec.
                    if let Some(encrypted) = item["encrypted_content"].as_str() {
                        pending_encrypted_content = Some(encrypted.to_string());
                    }
                }
                "function_call" => {
                    // Parse function call: name, arguments (JSON string), call_id
                    let name = item["name"].as_str().unwrap_or_default();
                    let call_id = item["call_id"].as_str().map(|s| s.to_string());

                    // Arguments come as JSON string; parse to Value
                    let arguments: Value = if let Some(args_str) = item["arguments"].as_str() {
                        if args_str.is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str(args_str).unwrap_or_else(|_| json!({}))
                        }
                    } else {
                        json!({})
                    };

                    // Associate pending encrypted_content as thought_signature for every
                    // function_call in this reasoning turn.
                    let thought_signature = pending_encrypted_content.as_ref().cloned();

                    tool_calls.push(ToolCall::new(
                        name.to_string(),
                        arguments,
                        call_id,
                        thought_signature,
                    ));
                }
                _ => {
                    // Ignore unknown item types (e.g., function_call_output in response)
                    trace!("Unknown output item type: {}", item_type);
                }
            }
        }
    }

    // Join text and thought parts
    let text = text_parts.join("\n\n");
    let thought = if thought_parts.is_empty() {
        None
    } else {
        Some(thought_parts.join("\n\n"))
    };

    // Validate we got something
    if text.is_empty() && tool_calls.is_empty() && thought.is_none() {
        bail!("Invalid Responses output: no text, tool_calls, or reasoning found in: {data}");
    }

    // Extract usage tokens
    // Wire-shape: Responses uses input_tokens/output_tokens, not prompt_tokens/completion_tokens
    let input_tokens = data["usage"]["input_tokens"].as_u64();
    let output_tokens = data["usage"]["output_tokens"].as_u64();
    // Cached tokens under input_tokens_details.cached_tokens
    let cached_tokens = data["usage"]["input_tokens_details"]["cached_tokens"].as_u64();

    // Extract response id
    let id = data["id"].as_str().map(|s| s.to_string());

    Ok(ChatCompletionsOutput {
        text,
        tool_calls,
        thought,
        id,
        input_tokens,
        output_tokens,
        cached_tokens,
    })
}

// ================================================================
// Streaming Support (t6)
// ================================================================

use std::collections::HashMap;

use reqwest::RequestBuilder;

/// Mutable accumulator state for the Responses streaming parser.
///
/// Unlike chat/completions where tool calls arrive in delta chunks, Responses
/// sends discrete events: `output_item.added`, `function_call_arguments.delta`,
/// and `function_call_arguments.done`. This struct tracks in-progress tool calls
/// and reasoning state across events.
#[derive(Default)]
pub struct ResponsesStreamState {
    /// Pending function calls indexed by item_id: (name, args_buffer, call_id)
    /// When `function_call_arguments.done` arrives, we finalize and emit.
    pub pending_tool_calls: HashMap<String, PendingToolCall>,
    /// Most recent reasoning item's encrypted_content for attaching to tool calls
    pub encrypted_content: Option<String>,
    /// Current reasoning item_id (to track which encrypted_content belongs to which item)
    pub reasoning_item_id: Option<String>,
}

/// A pending tool call being accumulated across streaming events.
#[derive(Default)]
pub struct PendingToolCall {
    pub name: String,
    pub arguments: String,
    pub call_id: Option<String>,
}

/// Process a single Responses SSE event.
///
/// This is extracted for unit testing without a live server. The async wrapper
/// `openai_responses_streaming` calls this for each message.
///
/// # Event types handled
///
/// - `response.output_text.delta`: Text deltas → `handler.text`
/// - `response.reasoning_summary_text.delta` / `response.reasoning_text.delta`: Reasoning → `handler.thought`
/// - `response.output_item.added`: Record new item (function_call or reasoning)
/// - `response.function_call_arguments.delta`: Accumulate args for pending tool call
/// - `response.function_call_arguments.done` / `response.output_item.done`: Finalize tool call
/// - `response.completed`: Usage stats → `handler.set_usage`
/// - Error events → `catch_error`
///
/// # Wire-shape notes
///
/// - Usage is nested under `data["response"]["usage"]` on `response.completed`
/// - Reasoning `encrypted_content` is on the item under `data["encrypted_content"]`
/// - Function call `arguments` arrives as JSON string deltas, finalized via `arguments.done`
pub(crate) fn openai_handle_responses_event(
    state: &mut ResponsesStreamState,
    handler: &mut SseHandler,
    event_type: &str,
    data: &Value,
) -> Result<()> {
    match event_type {
        // Text output deltas
        "response.output_text.delta" => {
            if let Some(delta) = data["delta"].as_str() {
                handler.text(delta)?;
            }
        }

        // Reasoning summary deltas (primary source)
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(delta) = data["delta"].as_str() {
                handler.thought(delta)?;
            }
        }

        // New output item added: track in state
        "response.output_item.added" => {
            let item = &data["item"];
            let item_type = item["type"].as_str().unwrap_or("");
            let item_id = item["id"].as_str().unwrap_or_default();

            match item_type {
                "function_call" => {
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    let call_id = item["call_id"].as_str().map(|s| s.to_string());
                    state.pending_tool_calls.insert(
                        item_id.to_string(),
                        PendingToolCall {
                            name,
                            arguments: String::new(),
                            call_id,
                        },
                    );
                }
                "reasoning" => {
                    // Track reasoning item for encrypted_content capture
                    state.reasoning_item_id = Some(item_id.to_string());
                    // Clear any prior encrypted_content
                    state.encrypted_content = None;
                }
                _ => {}
            }
        }

        // Accumulate function call arguments
        "response.function_call_arguments.delta" => {
            let item_id = data["item_id"].as_str().unwrap_or_default();
            if let Some(pending) = state.pending_tool_calls.get_mut(item_id) {
                if let Some(delta) = data["delta"].as_str() {
                    pending.arguments.push_str(delta);
                }
            }
        }

        // Function call arguments done: finalize and emit
        "response.function_call_arguments.done" => {
            let item_id = data["item_id"].as_str().unwrap_or_default();
            if let Some(pending) = state.pending_tool_calls.remove(item_id) {
                // Parse accumulated arguments
                let arguments: Value = if pending.arguments.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&pending.arguments).unwrap_or_else(|_| json!({}))
                };

                // Attach encrypted_content as thought_signature if available.
                // If not available yet (reasoning arrives after tool call), response.completed
                // attaches it to all pending tool calls.
                let thought_signature = state.encrypted_content.clone();

                handler.tool_call(ToolCall::new(
                    pending.name,
                    arguments,
                    pending.call_id,
                    thought_signature,
                ))?;
            }
        }

        // Output item done: capture reasoning encrypted_content, finalize function_call
        "response.output_item.done" => {
            let item = &data["item"];
            let item_type = item["type"].as_str().unwrap_or("");
            let item_id = item["id"].as_str().unwrap_or_default();

            match item_type {
                "reasoning" => {
                    // Capture encrypted_content for subsequent tool calls. If tool calls
                    // were already emitted without this signature, keep encrypted_content
                    // until response.completed to attach it then.
                    if let Some(encrypted) = item["encrypted_content"].as_str() {
                        state.encrypted_content = Some(encrypted.to_string());
                    }
                }
                "function_call" => {
                    // Finalize if not already done via function_call_arguments.done
                    if let Some(pending) = state.pending_tool_calls.remove(item_id) {
                        let arguments: Value = if pending.arguments.is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str(&pending.arguments).unwrap_or_else(|_| json!({}))
                        };

                        let thought_signature = state.encrypted_content.clone();

                        handler.tool_call(ToolCall::new(
                            pending.name,
                            arguments,
                            pending.call_id,
                            thought_signature,
                        ))?;
                    }
                }
                _ => {}
            }
        }

        // Response completed: emit usage and attach late reasoning if needed
        "response.completed" => {
            // If we have encrypted_content from reasoning that arrived AFTER tool calls
            // were emitted, attach it now to the last pending tool call in handler.
            if let Some(encrypted) = &state.encrypted_content {
                // Note: handler.tool_call() has already been called for each tool call.
                handler.attach_thought_signature_to_pending_tool_calls(encrypted.clone());
            }
            state.encrypted_content = None;

            // Wire-shape note: usage is nested under `response.usage` (not top-level)
            let response = &data["response"];
            let usage = &response["usage"];

            let input_tokens = usage["input_tokens"].as_u64();
            let output_tokens = usage["output_tokens"].as_u64();
            let cached_tokens = usage["input_tokens_details"]["cached_tokens"].as_u64();

            handler.set_usage(input_tokens, output_tokens, cached_tokens);
        }

        // Error handling
        "response.failed" | "response.error" | "error" => {
            if let Some(err_obj) = data
                .get("error")
                .or_else(|| data.get("response").and_then(|r| r.get("error")))
            {
                return crate::catch_error(&json!({"error": err_obj}), 500, None);
            }
            // Fallback: generic error
            bail!("Responses stream error: {}", data);
        }

        // Ignore other events
        _ => {
            trace!("Unhandled Responses SSE event: {}", event_type);
        }
    }

    Ok(())
}

/// Process a streaming Responses SSE event (wrapper for testing).
///
/// Parses `message.data` as JSON and delegates to `openai_handle_responses_event`.
fn handle_responses_sse_message(
    state: &mut ResponsesStreamState,
    handler: &mut SseHandler,
    message: &crate::SseMmessage,
) -> Result<bool> {
    if handler.aborted() {
        return Ok(true);
    }

    // Parse JSON data
    let data: Value = match serde_json::from_str(&message.data) {
        Ok(d) => d,
        Err(e) => {
            // If data is not valid JSON, log and continue
            trace!("Failed to parse SSE data as JSON: {}", e);
            return Ok(false);
        }
    };

    debug!("Responses SSE event: {} data={}", message.event, data);
    harnx_core::llm_trace::stream_event("openai_responses", &data);

    // Dispatch based on event type (message.event) or data["type"]
    let event_type = message.event.as_str();

    // Also check data["type"] as some events may use that instead
    let effective_event_type = if event_type.is_empty() {
        data["type"].as_str().unwrap_or("")
    } else {
        event_type
    };

    openai_handle_responses_event(state, handler, effective_event_type, &data)?;

    Ok(false)
}

/// Stream OpenAI Responses API output.
///
/// Uses `sse_stream` to process typed SSE events. The Responses API sends
/// discrete event types (`response.output_text.delta`, `response.function_call_arguments.delta`,
/// etc.) rather than monolithic data chunks like chat/completions.
pub async fn openai_responses_streaming(
    builder: RequestBuilder,
    handler: &mut SseHandler,
    _model: &Model,
) -> Result<()> {
    let mut state = ResponsesStreamState::default();

    let handle = |message: crate::SseMmessage| -> Result<bool> {
        handle_responses_sse_message(&mut state, handler, &message)
    };

    crate::sse_stream(builder, handle).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::message::{Message, MessageContent, MessageContentPart, MessageRole};
    use harnx_core::model::Model;
    use harnx_core::tool::{ToolCall, ToolResult};

    /// Helper to create a simple model for tests.
    fn test_model() -> Model {
        Model::new("openai", "gpt-5.6-sol")
    }

    /// Helper to create a model with max_tokens_param.
    fn test_model_with_max_tokens(tokens: isize) -> Model {
        let mut model = Model::new("openai", "gpt-5.6-sol");
        model.set_max_tokens(Some(tokens), true);
        model
    }

    #[test]
    fn test_system_message_extracted_to_instructions() {
        let data = ChatCompletionsData {
            messages: vec![
                Message::new(
                    MessageRole::System,
                    MessageContent::Text("You are helpful.".into()),
                ),
                Message::new(MessageRole::User, MessageContent::Text("Hello".into())),
            ],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        assert_eq!(body["instructions"], "You are helpful.");
        assert!(
            body.get("messages").is_none(),
            "Should not have messages key"
        );
        let input = body["input"].as_array().expect(".Should have input array");
        assert_eq!(input.len(), 1, "Should have only user message in input");
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn test_system_message_parts_joined_with_newlines() {
        let data = ChatCompletionsData {
            messages: vec![
                Message::new(
                    MessageRole::System,
                    MessageContent::Array(vec![
                        MessageContentPart::Text {
                            text: "Part one.".into(),
                        },
                        MessageContentPart::Text {
                            text: "Part two.".into(),
                        },
                    ]),
                ),
                Message::new(MessageRole::User, MessageContent::Text("Hi".into())),
            ],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        assert_eq!(body["instructions"], "Part one.\n\nPart two.");
    }

    #[test]
    fn test_tools_flat_array() {
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("Hi".into()),
            )],
            temperature: None,
            top_p: None,
            functions: Some(vec![harnx_core::tool::ToolDeclaration {
                name: "get_weather".into(),
                description: "Get weather".into(),
                parameters: harnx_core::tool::JsonSchema::default(),
                mcp_tool_name: None,
                mcp_server_name: None,
                call_template: None,
                result_template: None,
                idempotent_hint: None,
                read_only_hint: None,
            }]),
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        let tools = body["tools"].as_array().expect("Should have tools array");
        assert_eq!(tools.len(), 1);
        // Verify flat structure (not nested under `function`)
        let tool = &tools[0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "get_weather");
        assert_eq!(tool["description"], "Get weather");
        assert!(
            tool.get("function").is_none(),
            "Tool should NOT have nested 'function' key"
        );
    }

    #[test]
    fn test_store_false_and_include_always_present() {
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("Hi".into()),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        assert_eq!(body["store"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn test_no_messages_key_no_seed() {
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("Hi".into()),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        assert!(
            body.get("messages").is_none(),
            "Should not have 'messages' key"
        );
        assert!(body.get("seed").is_none(), "Should not have 'seed' key");
    }

    #[test]
    fn test_tool_call_history_round_trips() {
        let tool_call = ToolCall::new(
            "get_weather".into(),
            serde_json::json!({"location": "SF"}),
            Some("call_123".into()),
            None,
        );
        let tool_result = ToolResult::new(tool_call, serde_json::json!({"temp": 72}));

        let data = ChatCompletionsData {
            messages: vec![
                Message::new(
                    MessageRole::User,
                    MessageContent::Text("What's the weather?".into()),
                ),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::ToolCalls(MessageContentToolCalls::new(
                        vec![tool_result],
                        "".into(),
                        None,
                    )),
                ),
            ],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        let input = body["input"].as_array().expect("Should have input array");
        assert!(
            input.len() >= 3,
            "Should have user message + function_call + function_call_output"
        );

        // Find the function_call item
        let fc = input
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("Should have function_call item");
        assert_eq!(fc["call_id"], "call_123");
        assert_eq!(fc["name"], "get_weather");
        assert_eq!(fc["arguments"], r#"{"location":"SF"}"#);

        // Find the function_call_output item
        let fco = input
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .expect("Should have function_call_output item");
        assert_eq!(fco["call_id"], "call_123");
        assert_eq!(fco["output"], r#"{"temp":72}"#);
    }

    #[test]
    fn test_max_output_tokens_clamped_to_min_16() {
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("Hi".into()),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        // Model with max_tokens below min
        let model = test_model_with_max_tokens(10);
        let body = openai_build_responses_body(data, &model);

        assert_eq!(body["max_output_tokens"], 16, "Should clamp to min 16");
    }

    #[test]
    fn test_stream_flag() {
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("Hi".into()),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: true,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        assert_eq!(body["stream"], true);
    }

    #[test]
    fn test_temperature_and_top_p_included_if_provided() {
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("Hi".into()),
            )],
            temperature: Some(0.7),
            top_p: Some(0.9),
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["top_p"], 0.9);
    }

    #[test]
    fn test_temperature_top_p_omitted_if_none() {
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("Hi".into()),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
    }

    #[test]
    fn test_multimodal_input_image() {
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Array(vec![
                    MessageContentPart::Text {
                        text: "What's in this image?".into(),
                    },
                    MessageContentPart::ImageUrl {
                        image_url: harnx_core::message::ImageUrl {
                            url: "https://example.com/image.png".into(),
                        },
                    },
                ]),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        let input = body["input"].as_array().expect("Should have input array");
        assert_eq!(input.len(), 1);
        let msg = &input[0];
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["role"], "user");
        let content = msg["content"]
            .as_array()
            .expect("Should have content array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "What's in this image?");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "https://example.com/image.png");
    }

    #[test]
    fn test_assistant_strips_think_tag_for_non_final() {
        let data = ChatCompletionsData {
            messages: vec![
                Message::new(MessageRole::User, MessageContent::Text("Hi".into())),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::Text(
                        "<think>some reasoning</think>\n\nHere's my answer.".into(),
                    ),
                ),
                Message::new(MessageRole::User, MessageContent::Text("Thanks".into())),
            ],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        let input = body["input"].as_array().expect("Should have input array");
        assert_eq!(input.len(), 3);
        let assistant_msg = &input[1];
        assert_eq!(assistant_msg["type"], "message");
        assert_eq!(assistant_msg["role"], "assistant");
        let content = assistant_msg["content"]
            .as_array()
            .expect("Should have content array");
        assert_eq!(content[0]["type"], "output_text");
        assert_eq!(content[0]["text"], "Here's my answer.");
    }

    #[test]
    fn test_model_name_from_real_name() {
        let model = Model::new("openai", "custom-model-name");
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("Hi".into()),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &model);

        assert_eq!(body["model"], "custom-model-name");
    }

    // ================================================================
    // Tests for openai_extract_responses
    // ================================================================

    #[test]
    fn test_extract_text_only_response() {
        let responses_json = json!({
            "id": "resp_test123",
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        { "type": "output_text", "text": "Hello, how can I help?" }
                    ]
                }
            ],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "input_tokens_details": { "cached_tokens": 5 }
            }
        });

        let output = openai_extract_responses(&responses_json).expect("Should parse");

        assert_eq!(output.text, "Hello, how can I help?");
        assert!(output.tool_calls.is_empty());
        assert!(output.thought.is_none());
        assert_eq!(output.id, Some("resp_test123".to_string()));
        assert_eq!(output.input_tokens, Some(10));
        assert_eq!(output.output_tokens, Some(20));
        assert_eq!(output.cached_tokens, Some(5));
    }

    #[test]
    fn test_extract_tool_call_response() {
        let responses_json = json!({
            "id": "resp_tool123",
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call_abc",
                    "name": "get_weather",
                    "arguments": "{\"location\": \"SF\"}"
                }
            ],
            "usage": {
                "input_tokens": 15,
                "output_tokens": 10
            }
        });

        let output = openai_extract_responses(&responses_json).expect("Should parse");

        assert!(output.text.is_empty());
        assert_eq!(output.tool_calls.len(), 1);
        let call = &output.tool_calls[0];
        assert_eq!(call.name, "get_weather");
        assert_eq!(call.id, Some("call_abc".to_string()));
        assert_eq!(call.arguments, json!({"location": "SF"}));
        assert!(call.thought_signature.is_none());
    }

    #[test]
    fn test_extract_reasoning_and_tool_call() {
        // Reasoning followed by tool call: encrypted_content -> thought_signature
        let responses_json = json!({
            "id": "resp_reasoning_tool",
            "output": [
                {
                    "type": "reasoning",
                    "summary": [
                        { "type": "summary_text", "text": "Thinking about weather..." }
                    ],
                    "encrypted_content": "ENCRYPTED_BLOB_123"
                },
                {
                    "type": "function_call",
                    "call_id": "call_xyz",
                    "name": "get_weather",
                    "arguments": "{\"city\": \"NYC\"}"
                }
            ],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 200
            }
        });

        let output = openai_extract_responses(&responses_json).expect("Should parse");

        // Thought contains reasoning summary
        assert_eq!(
            output.thought,
            Some("Thinking about weather...".to_string())
        );
        // Tool call gets encrypted_content as thought_signature
        assert_eq!(output.tool_calls.len(), 1);
        let call = &output.tool_calls[0];
        assert_eq!(call.name, "get_weather");
        assert_eq!(
            call.thought_signature,
            Some("ENCRYPTED_BLOB_123".to_string())
        );
    }

    #[test]
    fn test_extract_multiple_text_parts() {
        let responses_json = json!({
            "id": "resp_multi",
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        { "type": "output_text", "text": "Part one." },
                        { "type": "output_text", "text": "Part two." }
                    ]
                }
            ],
            "usage": {}
        });

        let output = openai_extract_responses(&responses_json).expect("Should parse");

        assert_eq!(output.text, "Part one.\n\nPart two.");
    }

    #[test]
    fn test_extract_multi_tool_call_response_shares_thought_signature() {
        let encrypted_content = "ENCRYPTED_BLOB_SHARED";
        let responses_json = json!({
            "id": "resp_multi_tool_reasoning",
            "output": [
                {
                    "type": "reasoning",
                    "summary": [
                        { "type": "summary_text", "text": "Need two tool calls." }
                    ],
                    "encrypted_content": encrypted_content
                },
                {
                    "type": "function_call",
                    "call_id": "call_one",
                    "name": "get_weather",
                    "arguments": "{\"city\": \"NYC\"}"
                },
                {
                    "type": "function_call",
                    "call_id": "call_two",
                    "name": "get_time",
                    "arguments": "{\"timezone\": \"UTC\"}"
                }
            ],
            "usage": {}
        });

        let output = openai_extract_responses(&responses_json).expect("Should parse");

        assert_eq!(output.tool_calls.len(), 2);
        for call in &output.tool_calls {
            assert_eq!(call.thought_signature.as_deref(), Some(encrypted_content));
        }
        assert_eq!(output.tool_calls[0].name, "get_weather");
        assert_eq!(output.tool_calls[1].name, "get_time");
    }

    #[test]
    fn test_extract_empty_arguments() {
        // Empty or missing arguments should parse to empty object
        let responses_json = json!({
            "id": "resp_empty_args",
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call_empty",
                    "name": "no_args_tool",
                    "arguments": ""
                }
            ],
            "usage": {}
        });

        let output = openai_extract_responses(&responses_json).expect("Should parse");

        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.tool_calls[0].arguments, json!({}));
    }

    #[test]
    fn test_extract_reasoning_without_tool_call() {
        // Reasoning without tool call: summary -> thought, encrypted_content preserved but not attached
        let responses_json = json!({
            "id": "resp_reasoning_only",
            "output": [
                {
                    "type": "reasoning",
                    "summary": [
                        { "type": "summary_text", "text": "Analyzing the question..." }
                    ],
                    "encrypted_content": "ENCRYPTED_BLOB_ALONE"
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        { "type": "output_text", "text": "Here's my response." }
                    ]
                }
            ],
            "usage": {}
        });

        let output = openai_extract_responses(&responses_json).expect("Should parse");

        assert_eq!(
            output.thought,
            Some("Analyzing the question...".to_string())
        );
        assert_eq!(output.text, "Here's my response.");
        // No tool call, so no thought_signature attached
        assert!(output.tool_calls.is_empty());
    }

    #[test]
    fn test_extract_text_and_reasoning() {
        // Message with both text and reasoning output
        let responses_json = json!({
            "id": "resp_text_reasoning",
            "output": [
                {
                    "type": "reasoning",
                    "summary": [
                        { "type": "summary_text", "text": "Let me think..." }
                    ]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        { "type": "output_text", "text": "Final answer." }
                    ]
                }
            ],
            "usage": {}
        });

        let output = openai_extract_responses(&responses_json).expect("Should parse");

        assert_eq!(output.thought, Some("Let me think...".to_string()));
        assert_eq!(output.text, "Final answer.");
    }

    // ================================================================
    // Tests for streaming (t6)
    // ================================================================

    use harnx_core::abort::create_abort_signal;
    use tokio::sync::mpsc::unbounded_channel;

    /// Helper to create a test handler without needing a live server.
    fn test_handler() -> (
        crate::SseHandler,
        tokio::sync::mpsc::UnboundedReceiver<crate::SseEvent>,
    ) {
        let (tx, rx) = unbounded_channel();
        let handler = crate::SseHandler::new(tx, create_abort_signal());
        (handler, rx)
    }

    #[test]
    fn test_streaming_text_delta() {
        let (mut handler, _rx) = test_handler();
        let mut state = ResponsesStreamState::default();

        // Simulate text delta event
        let data = json!({"delta": "Hello"});
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.output_text.delta",
            &data,
        )
        .expect("Should handle");

        let (text, thought, calls, _usage) = handler.take();
        assert_eq!(text, "Hello");
        assert!(thought.is_none());
        assert!(calls.is_empty());
    }

    #[test]
    fn test_streaming_reasoning_delta() {
        let (mut handler, _rx) = test_handler();
        let mut state = ResponsesStreamState::default();

        // Simulate reasoning delta event
        let data = json!({"delta": "Thinking..."});
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.reasoning_summary_text.delta",
            &data,
        )
        .expect("Should handle");

        let (text, thought, calls, _usage) = handler.take();
        assert!(text.is_empty()); // Reasoning goes to thought buffer
        assert_eq!(thought, Some("Thinking...".to_string()));
        assert!(calls.is_empty());
    }

    #[test]
    fn test_streaming_function_call_full_sequence() {
        let (mut handler, _rx) = test_handler();
        let mut state = ResponsesStreamState::default();

        // 1. Item added
        let data = json!({
            "item": {
                "type": "function_call",
                "id": "fc_123",
                "name": "get_weather",
                "call_id": "call_abc"
            }
        });
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.output_item.added",
            &data,
        )
        .expect("Should handle");

        // 2. Arguments delta
        let data = json!({
            "item_id": "fc_123",
            "delta": "{\"city\":"
        });
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.function_call_arguments.delta",
            &data,
        )
        .expect("Should handle");

        let data = json!({
            "item_id": "fc_123",
            "delta": " \"NYC\"}"
        });
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.function_call_arguments.delta",
            &data,
        )
        .expect("Should handle");

        // 3. Arguments done
        let data = json!({"item_id": "fc_123"});
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.function_call_arguments.done",
            &data,
        )
        .expect("Should handle");

        let (text, thought, calls, _usage) = handler.take();
        assert!(text.is_empty());
        assert!(thought.is_none());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].id, Some("call_abc".to_string()));
        assert_eq!(calls[0].arguments, json!({"city": "NYC"}));
    }

    #[test]
    fn test_streaming_response_error_surfaces_via_catch_error() {
        let (mut handler, _rx) = test_handler();
        let mut state = ResponsesStreamState::default();
        let data = json!({
            "type": "response.failed",
            "response": {
                "error": {
                    "type": "server_error",
                    "message": "Responses failed"
                }
            }
        });

        let err = openai_handle_responses_event(&mut state, &mut handler, "response.failed", &data)
            .expect_err("response.failed should surface an LLM error");

        assert_eq!(
            err.to_string(),
            "Responses failed (type: server_error) (status: 500)"
        );
    }

    #[test]
    fn test_streaming_reasoning_and_tool_call() {
        let (mut handler, _rx) = test_handler();
        let mut state = ResponsesStreamState::default();

        // 1. Reasoning item added
        let data = json!({
            "item": {
                "type": "reasoning",
                "id": "rs_1"
            }
        });
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.output_item.added",
            &data,
        )
        .expect("Should handle");

        // 2. Reasoning delta
        let data = json!({"delta": "Analyzing..."});
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.reasoning_summary_text.delta",
            &data,
        )
        .expect("Should handle");

        // 3. Reasoning done with encrypted_content
        let data = json!({
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "ENCRYPTED_BLOB"
            }
        });
        openai_handle_responses_event(&mut state, &mut handler, "response.output_item.done", &data)
            .expect("Should handle");

        // 4. Function call added
        let data = json!({
            "item": {
                "type": "function_call",
                "id": "fc_456",
                "name": "search",
                "call_id": "call_xyz"
            }
        });
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.output_item.added",
            &data,
        )
        .expect("Should handle");

        // 5. Arguments delta + done
        let data = json!({
            "item_id": "fc_456",
            "delta": "{\"q\":\"test\"}"
        });
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.function_call_arguments.delta",
            &data,
        )
        .expect("Should handle");

        let data = json!({"item_id": "fc_456"});
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.function_call_arguments.done",
            &data,
        )
        .expect("Should handle");

        let (text, thought, calls, _usage) = handler.take();
        assert!(text.is_empty());
        assert_eq!(thought, Some("Analyzing...".to_string()));
        assert_eq!(calls.len(), 1);
        // Tool call should have encrypted_content as thought_signature
        assert_eq!(
            calls[0].thought_signature,
            Some("ENCRYPTED_BLOB".to_string())
        );
    }

    #[test]
    fn test_streaming_usage_on_completed() {
        let (mut handler, _rx) = test_handler();
        let mut state = ResponsesStreamState::default();

        // Response completed with usage
        let data = json!({
            "response": {
                "id": "resp_123",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 50,
                    "input_tokens_details": {
                        "cached_tokens": 20
                    }
                }
            }
        });
        openai_handle_responses_event(&mut state, &mut handler, "response.completed", &data)
            .expect("Should handle");

        let (_text, _thought, _calls, usage) = handler.take();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cached_tokens, 20);
    }

    #[test]
    fn test_streaming_full_response_sequence() {
        // Test a complete streaming sequence: text + tool call + usage
        let (mut handler, _rx) = test_handler();
        let mut state = ResponsesStreamState::default();

        // 1. Text delta
        let data = json!({"delta": "Let me "});
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.output_text.delta",
            &data,
        )
        .expect("Should handle");

        let data = json!({"delta": "help you."});
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.output_text.delta",
            &data,
        )
        .expect("Should handle");

        // 2. Tool call added
        let data = json!({
            "item": {
                "type": "function_call",
                "id": "fc_789",
                "name": "calculate",
                "call_id": "call_calc"
            }
        });
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.output_item.added",
            &data,
        )
        .expect("Should handle");

        // 3. Arguments
        let data = json!({"item_id": "fc_789", "delta": "{\"x\":1,\"y\":2}"});
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.function_call_arguments.delta",
            &data,
        )
        .expect("Should handle");

        let data = json!({"item_id": "fc_789"});
        openai_handle_responses_event(
            &mut state,
            &mut handler,
            "response.function_call_arguments.done",
            &data,
        )
        .expect("Should handle");

        // 4. Completed
        let data = json!({
            "response": {
                "id": "resp_final",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 20
                }
            }
        });
        openai_handle_responses_event(&mut state, &mut handler, "response.completed", &data)
            .expect("Should handle");

        let (text, thought, calls, usage) = handler.take();
        assert_eq!(text, "Let me help you.");
        assert!(thought.is_none());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "calculate");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 20);
    }

    // ================================================================
    // Tests for reasoning replay (t5)
    // ================================================================

    #[test]
    fn test_reasoning_replay_with_signature() {
        // Tool-call history with thought_signature -> reasoning input item emitted
        let tool_call = ToolCall::new(
            "get_weather".into(),
            serde_json::json!({"location": "SF"}),
            Some("call_123".into()),
            Some("ENCRYPTED_BLOB_456".into()),
        );
        let tool_result = ToolResult::new(tool_call, serde_json::json!({"temp": 72}));

        let data = ChatCompletionsData {
            messages: vec![
                Message::new(
                    MessageRole::User,
                    MessageContent::Text("What's the weather?".into()),
                ),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::ToolCalls(MessageContentToolCalls::new(
                        vec![tool_result],
                        "".into(),
                        None,
                    )),
                ),
            ],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        let input = body["input"].as_array().expect("Should have input array");
        assert!(
            input.len() >= 3,
            "Should have user message + reasoning + function_call + function_call_output"
        );

        // First item should be user message
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");

        // Second item should be reasoning input (BEFORE function_call)
        let reasoning_item = &input[1];
        assert_eq!(reasoning_item["type"], "reasoning");
        assert_eq!(reasoning_item["encrypted_content"], "ENCRYPTED_BLOB_456");
        assert_eq!(reasoning_item["summary"], json!([]));

        // Third item should be function_call
        let fc = &input[2];
        assert_eq!(fc["type"], "function_call");
        assert_eq!(fc["call_id"], "call_123");
        assert_eq!(fc["name"], "get_weather");

        // Fourth item should be function_call_output
        let fco = &input[3];
        assert_eq!(fco["type"], "function_call_output");
        assert_eq!(fco["call_id"], "call_123");
    }

    #[test]
    fn test_reasoning_replay_without_signature() {
        // Tool-call history WITHOUT thought_signature -> NO reasoning input item
        let tool_call = ToolCall::new(
            "get_weather".into(),
            serde_json::json!({"location": "SF"}),
            Some("call_789".into()),
            None, // No thought_signature
        );
        let tool_result = ToolResult::new(tool_call, serde_json::json!({"temp": 72}));

        let data = ChatCompletionsData {
            messages: vec![
                Message::new(
                    MessageRole::User,
                    MessageContent::Text("What's the weather?".into()),
                ),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::ToolCalls(MessageContentToolCalls::new(
                        vec![tool_result],
                        "".into(),
                        None,
                    )),
                ),
            ],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        let input = body["input"].as_array().expect("Should have input array");

        // Should have user message + function_call + function_call_output (NO reasoning item)
        assert_eq!(
            input.len(),
            3,
            "Should have user message + function_call + function_call_output, NO reasoning item"
        );

        // First item should be user message
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");

        // Second item should be function_call (NO reasoning item)
        assert_eq!(input[1]["type"], "function_call");

        // Third item should be function_call_output
        assert_eq!(input[2]["type"], "function_call_output");
    }

    #[test]
    fn test_reasoning_replay_first_signature_used() {
        // Multiple tool_results: first non-empty signature is used
        let tool_call1 = ToolCall::new(
            "tool1".into(),
            serde_json::json!({}),
            Some("call_1".into()),
            Some("FIRST_ENCRYPTED_BLOB".into()),
        );
        let tool_call2 = ToolCall::new(
            "tool2".into(),
            serde_json::json!({}),
            Some("call_2".into()),
            Some("SECOND_ENCRYPTED_BLOB".into()),
        );
        let tool_result1 = ToolResult::new(tool_call1, serde_json::json!({}));
        let tool_result2 = ToolResult::new(tool_call2, serde_json::json!({}));

        let data = ChatCompletionsData {
            messages: vec![
                Message::new(MessageRole::User, MessageContent::Text("Hi".into())),
                Message::new(
                    MessageRole::Assistant,
                    MessageContent::ToolCalls(MessageContentToolCalls::new(
                        vec![tool_result1, tool_result2],
                        "".into(),
                        None,
                    )),
                ),
            ],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        let input = body["input"].as_array().expect("Should have input array");

        // Find reasoning item
        let reasoning_item = input.iter().find(|item| item["type"] == "reasoning");
        assert!(reasoning_item.is_some(), "Should have reasoning item");
        assert_eq!(
            reasoning_item.unwrap()["encrypted_content"],
            "FIRST_ENCRYPTED_BLOB",
            "Should use first non-empty signature"
        );
    }

    // STORE-OVERRIDE TESTS (t9 requirement)
    // Note: Model patches are applied BEFORE the body builder by the caller
    // (in openai.rs via client.rs patch_request_data). The body builder
    // always emits store:false; patches can override it. This unit test
    // verifies that the caller-level patch integration works correctly.
    //
    // The integration is tested in harnx-core/src/model.rs via
    // extract_patches_for_selects_chat_or_responses_by_endpoint, which
    // proves that patches.responses is selected for responses endpoint.
    //
    // For a full end-to-end test, see Task A in openai_responses_e2e.rs
    // which verifies that the body sent to /v1/responses has store:false.

    // STORE-OVERRIDE UNIT TEST: Apply jaq patch to override store default
    #[test]
    fn test_store_override_via_jaq_patch() {
        use harnx_core::jaq;

        // Build default body
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("Hi".into()),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());

        // Verify store:false default
        assert_eq!(body["store"], false, "store should default to false");

        // Apply the patch that model aliases use
        let patches = vec![".body.store = true".to_string()];
        let body_json = json!({ "body": body });
        let patched = jaq::eval_filters_strict(&patches, body_json).expect("patch should apply");

        // Verify store:true after patch
        assert_eq!(
            patched
                .get("body")
                .and_then(|b| b.get("store"))
                .and_then(|s| s.as_bool()),
            Some(true),
            "store should be true after applying patch"
        );
    }

    // STORE-OVERRIDE UNIT TEST: Verify responses-specific patches are selected
    #[test]
    fn test_responses_patch_selection() {
        use harnx_core::model::{ModelType, RequestPatches};

        // The endpoint selection logic is tested in harnx-core/src/model.rs
        // via extract_patches_for_selects_chat_or_responses_by_endpoint.
        // Here we verify the jaq evaluation for responses-specific patches.

        let patches_config = RequestPatches {
            chat_completions: Some(vec![".body.chat = true".to_string()]),
            responses: Some(vec![".body.responses = true".to_string()]),
            embeddings: None,
            rerank: None,
        };

        // Select patches for responses endpoint
        let responses_patches =
            ModelType::Chat.extract_patches_for(&patches_config, Some("responses"));

        // Verify responses patches are selected
        assert!(responses_patches.is_some(), "Should have responses patches");
        let patches = responses_patches.unwrap();
        assert_eq!(patches.len(), 1, "Should have one responses patch");

        // Apply patch
        let body = json!({ "store": false });
        let body_json = json!({ "body": body });
        let patched = harnx_core::jaq::eval_filters_strict(patches, body_json)
            .expect("responses patch should apply");

        assert_eq!(
            patched.get("body").and_then(|b| b.get("responses")),
            Some(&json!(true)),
            "responses patch should take effect"
        );

        // Verify chat_completions patches are NOT selected
        let chat_patches = ModelType::Chat.extract_patches_for(&patches_config, None);
        assert!(
            chat_patches.is_some(),
            "Should have chat_completions patches"
        );
        let chat_p = chat_patches.unwrap();
        assert_eq!(chat_p.len(), 1, "Should have one chat_completions patch");
        assert!(
            chat_p[0].contains("chat"),
            "Should use chat_completions patch"
        );
    }

    #[test]
    fn test_responses_patch_field_in_body() {
        // Verify store:false default and include array
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("Hi".into()),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let body = openai_build_responses_body(data, &test_model());
        // Verify store:false default
        assert_eq!(body["store"], false, "store should default to false");
        // Verify include contains reasoning.encrypted_content
        let include = body["include"]
            .as_array()
            .expect("Should have include array");
        assert!(include.contains(&json!("reasoning.encrypted_content")));
    }

    // STREAMING ORDERING UNIT TESTS: Verify encrypted_content attaches regardless of event order

    /// Test: Normal order - reasoning.output_item.done arrives BEFORE function_call_arguments.done.
    /// The encrypted_content should be available and attached immediately.
    #[test]
    fn test_streaming_normal_order_reasoning_before_tool() {
        use super::*;
        use crate::SseMmessage;
        use harnx_core::abort::create_abort_signal;
        use tokio::sync::mpsc;

        let (tx, _rx) = mpsc::unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = ResponsesStreamState::default();

        // 1. Reasoning item added
        let event1 = SseMmessage {
            event: "response.output_item.added".to_string(),
            data: r#"{"item":{"type":"reasoning","id":"rs_123"}}"#.to_string(),
        };
        handle_responses_sse_message(&mut state, &mut handler, &event1).unwrap();

        // 2. Reasoning done with encrypted_content
        let event2 = SseMmessage {
            event: "response.output_item.done".to_string(),
            data: r#"{"item":{"type":"reasoning","id":"rs_123","encrypted_content":"ENCRYPTED_BLOB_ABC"}}"#.to_string(),
        };
        handle_responses_sse_message(&mut state, &mut handler, &event2).unwrap();
        assert_eq!(
            state.encrypted_content,
            Some("ENCRYPTED_BLOB_ABC".to_string()),
            "encrypted_content should be captured after reasoning.output_item.done"
        );

        // 3. Function call added
        let event3 = SseMmessage {
            event: "response.output_item.added".to_string(),
            data: r#"{"item":{"type":"function_call","id":"fc_456","name":"test_tool","call_id":"call_789"}}"#.to_string(),
        };
        handle_responses_sse_message(&mut state, &mut handler, &event3).unwrap();

        // 4. Function call arguments delta
        let event4 = SseMmessage {
            event: "response.function_call_arguments.delta".to_string(),
            data: r#"{"item_id":"fc_456","delta":"{\"arg\":1}"}"#.to_string(),
        };
        handle_responses_sse_message(&mut state, &mut handler, &event4).unwrap();

        // 5. Function call arguments done - should attach encrypted_content
        let event5 = SseMmessage {
            event: "response.function_call_arguments.done".to_string(),
            data: r#"{"item_id":"fc_456"}"#.to_string(),
        };
        handle_responses_sse_message(&mut state, &mut handler, &event5).unwrap();

        // Verify tool call has thought_signature
        let calls = handler.tool_calls();
        assert_eq!(calls.len(), 1, "Should have one tool call");
        assert_eq!(
            calls[0].thought_signature,
            Some("ENCRYPTED_BLOB_ABC".to_string()),
            "Tool call should have thought_signature from reasoning"
        );
    }

    /// Test: Reversed order - function_call_arguments.done arrives BEFORE reasoning.output_item.done.
    /// This is the bug case: when tool call is finalized before reasoning arrives,
    /// the encrypted_content should be attached at response.completed.
    #[test]
    fn test_streaming_reversed_order_tool_before_reasoning() {
        use super::*;
        use crate::SseMmessage;
        use harnx_core::abort::create_abort_signal;
        use tokio::sync::mpsc;

        let (tx, _rx) = mpsc::unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = ResponsesStreamState::default();

        // 1. Reasoning item added
        let event1 = SseMmessage {
            event: "response.output_item.added".to_string(),
            data: r#"{"item":{"type":"reasoning","id":"rs_123"}}"#.to_string(),
        };
        handle_responses_sse_message(&mut state, &mut handler, &event1).unwrap();

        // 2. Function call added
        let event2 = SseMmessage {
            event: "response.output_item.added".to_string(),
            data: r#"{"item":{"type":"function_call","id":"fc_456","name":"test_tool","call_id":"call_789"}}"#.to_string(),
        };
        handle_responses_sse_message(&mut state, &mut handler, &event2).unwrap();

        // 3. Function call arguments delta
        let event3 = SseMmessage {
            event: "response.function_call_arguments.delta".to_string(),
            data: r#"{"item_id":"fc_456","delta":"{\"arg\":1}"}"#.to_string(),
        };
        handle_responses_sse_message(&mut state, &mut handler, &event3).unwrap();

        // 4. Function call arguments done - encrypted_content NOT yet available
        let event4 = SseMmessage {
            event: "response.function_call_arguments.done".to_string(),
            data: r#"{"item_id":"fc_456"}"#.to_string(),
        };
        handle_responses_sse_message(&mut state, &mut handler, &event4).unwrap();

        // At this point, tool call should have NO thought_signature
        let calls = handler.tool_calls();
        assert_eq!(calls.len(), 1, "Should have one tool call");
        assert!(
            calls[0].thought_signature.is_none(),
            "Tool call should NOT have thought_signature yet (reasoning not arrived)"
        );

        // 5. Reasoning done with encrypted_content - arrives AFTER tool call finalized
        let event5 = SseMmessage {
            event: "response.output_item.done".to_string(),
            data: r#"{"item":{"type":"reasoning","id":"rs_123","encrypted_content":"ENCRYPTED_BLOB_XYZ"}}"#.to_string(),
        };
        handle_responses_sse_message(&mut state, &mut handler, &event5).unwrap();
        assert_eq!(
            state.encrypted_content,
            Some("ENCRYPTED_BLOB_XYZ".to_string()),
            "encrypted_content should be captured from late reasoning"
        );

        // 6. Response completed - should attach late encrypted_content
        let event6 = SseMmessage {
            event: "response.completed".to_string(),
            data: r#"{"response":{"usage":{"input_tokens":10,"output_tokens":20,"input_tokens_details":{"cached_tokens":5}}}}"#.to_string(),
        };
        handle_responses_sse_message(&mut state, &mut handler, &event6).unwrap();

        // Verify tool call now has thought_signature attached at response.completed
        let calls = handler.tool_calls();
        assert_eq!(calls.len(), 1, "Should still have one tool call");
        assert_eq!(
            calls[0].thought_signature,
            Some("ENCRYPTED_BLOB_XYZ".to_string()),
            "Tool call should have thought_signature attached at response.completed"
        );
    }
}
