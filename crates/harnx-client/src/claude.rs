use crate::*;

use harnx_core::text::strip_think_tag;

use anyhow::{bail, Context, Result};
use reqwest::RequestBuilder;
use serde_json::{json, Value};

const API_BASE: &str = "https://api.anthropic.com/v1";

impl ClaudeClient {
    config_get_fn!(api_key, get_api_key);
    config_get_fn!(api_base, get_api_base);

    pub const PROMPTS: [PromptAction<'static>; 1] = [("api_key", "API Key", None)];
}

impl_client_trait!(
    ClaudeClient,
    (
        prepare_chat_completions,
        claude_chat_completions,
        claude_chat_completions_streaming
    ),
    (noop_prepare_embeddings, noop_embeddings),
    (noop_prepare_rerank, noop_rerank),
);

fn prepare_chat_completions(
    self_: &ClaudeClient,
    data: ChatCompletionsData,
) -> Result<RequestData> {
    let api_key = self_.get_api_key()?;
    let api_base = self_
        .get_api_base()
        .unwrap_or_else(|_| API_BASE.to_string());

    let url = format!("{}/messages", api_base.trim_end_matches('/'));
    let body = claude_build_chat_completions_body(data, &self_.model)?;

    let mut request_data = RequestData::new(url, body);

    request_data.header("anthropic-version", "2023-06-01");
    if api_key.starts_with("sk-ant-oat") {
        request_data.bearer_auth(api_key);
    } else {
        request_data.header("x-api-key", api_key);
    }

    Ok(request_data)
}

pub async fn claude_chat_completions(
    builder: RequestBuilder,
    _model: &Model,
) -> Result<ChatCompletionsOutput> {
    let res = builder.send().await?;
    let status = res.status();
    let retry_after = parse_retry_after(res.headers());
    let data: Value = res.json().await?;
    if !status.is_success() {
        catch_error(&data, status.as_u16(), retry_after)?;
    }
    debug!("non-stream-data: {data}");
    claude_extract_chat_completions(&data)
}

/// Mutable state threaded through the Claude streaming parser. Extracted
/// from the `sse_stream` closure so the per-event logic is testable in
/// isolation.
#[derive(Default)]
struct ClaudeStreamState {
    function_name: String,
    function_arguments: String,
    function_id: String,
    reasoning_state: i32,
    /// Accumulated signature from `signature_delta` events for the current
    /// thinking block.  Passed to each tool call emitted in the same turn so
    /// the serialiser can echo it back verbatim on the next request.
    thinking_signature: String,
}

fn claude_emit_pending_tool_call(
    state: &mut ClaudeStreamState,
    handler: &mut SseHandler,
    empty_args_as_object: bool,
) -> Result<()> {
    if state.function_name.is_empty() {
        return Ok(());
    }
    let arguments: Value = if empty_args_as_object && state.function_arguments.is_empty() {
        json!({})
    } else {
        state.function_arguments.parse().with_context(|| {
            format!(
                "Tool call '{}' have non-JSON arguments '{}'",
                state.function_name, state.function_arguments
            )
        })?
    };
    let thought_signature = if state.thinking_signature.is_empty() {
        None
    } else {
        Some(state.thinking_signature.clone())
    };
    handler.tool_call(ToolCall::new(
        state.function_name.clone(),
        arguments,
        Some(state.function_id.clone()),
        thought_signature,
    ))?;
    state.function_name.clear();
    state.function_arguments.clear();
    state.function_id.clear();
    Ok(())
}

/// Transition the reasoning-block bracket state. Emits `<think>\n` when
/// opening and `\n</think>\n\n` when closing; no-op when already in the
/// target state.
fn claude_transition_reasoning(
    state: &mut ClaudeStreamState,
    handler: &mut SseHandler,
    open: bool,
) -> Result<()> {
    let target: i32 = if open { 1 } else { 0 };
    if state.reasoning_state == target {
        return Ok(());
    }
    let bracket = if open { "<think>\n" } else { "\n</think>\n\n" };
    handler.text(bracket)?;
    state.reasoning_state = target;
    Ok(())
}

fn claude_handle_content_block_start(
    state: &mut ClaudeStreamState,
    handler: &mut SseHandler,
    data: &Value,
) -> Result<()> {
    let (Some("tool_use"), Some(name), Some(id)) = (
        data["content_block"]["type"].as_str(),
        data["content_block"]["name"].as_str(),
        data["content_block"]["id"].as_str(),
    ) else {
        return Ok(());
    };
    // Fallback emit: the previous tool_use block never received a
    // content_block_stop (some providers / proxy paths skip it).
    // Normally content_block_stop clears the accumulators, so this
    // path is dormant.
    claude_emit_pending_tool_call(state, handler, false)?;
    state.function_name = name.into();
    state.function_arguments.clear();
    state.function_id = id.into();
    Ok(())
}

fn claude_handle_content_block_delta(
    state: &mut ClaudeStreamState,
    handler: &mut SseHandler,
    data: &Value,
) -> Result<()> {
    let delta = &data["delta"];
    if let Some(text) = delta["text"].as_str() {
        handler.text(text)?;
    } else if let Some(text) = delta["thinking"].as_str() {
        claude_transition_reasoning(state, handler, true)?;
        handler.text(text)?;
    } else if let Some(sig) = delta["signature"].as_str() {
        // signature_delta: accumulate the thinking-block signature so it can
        // be echoed back verbatim on the next API request (issue #328).
        state.thinking_signature.push_str(sig);
    } else if let Some(partial_json) = delta["partial_json"]
        .as_str()
        .filter(|_| !state.function_name.is_empty())
    {
        state.function_arguments.push_str(partial_json);
    }
    Ok(())
}

fn claude_handle_content_block_stop(
    state: &mut ClaudeStreamState,
    handler: &mut SseHandler,
) -> Result<()> {
    claude_transition_reasoning(state, handler, false)?;
    // Emit if a tool_use block is pending, and reset accumulators so
    // the fallback emit path in content_block_start doesn't re-fire
    // this same call when the next tool_use block begins.
    claude_emit_pending_tool_call(state, handler, true)
}

fn claude_handle_stream_event(
    state: &mut ClaudeStreamState,
    handler: &mut SseHandler,
    data: &Value,
) -> Result<()> {
    let Some(typ) = data["type"].as_str() else {
        return Ok(());
    };
    match typ {
        "message_start" => {
            handler.set_usage(
                data["message"]["usage"]["input_tokens"].as_u64(),
                None,
                data["message"]["usage"]["cache_read_input_tokens"].as_u64(),
            );
        }
        "message_delta" => {
            // message_delta usage fields are cumulative and override
            // earlier values from message_start when present
            handler.set_usage(
                data["usage"]["input_tokens"].as_u64(),
                data["usage"]["output_tokens"].as_u64(),
                data["usage"]["cache_read_input_tokens"].as_u64(),
            );
        }
        "content_block_start" => claude_handle_content_block_start(state, handler, data)?,
        "content_block_delta" => claude_handle_content_block_delta(state, handler, data)?,
        "content_block_stop" => claude_handle_content_block_stop(state, handler)?,
        _ => {}
    }
    Ok(())
}

pub async fn claude_chat_completions_streaming(
    builder: RequestBuilder,
    handler: &mut SseHandler,
    _model: &Model,
) -> Result<()> {
    let mut state = ClaudeStreamState::default();
    let handle = |message: SseMmessage| -> Result<bool> {
        if handler.aborted() {
            return Ok(true);
        }
        let data: Value = serde_json::from_str(&message.data)?;
        debug!("stream-data: {data}");
        claude_handle_stream_event(&mut state, handler, &data)?;
        Ok(false)
    };

    sse_stream(builder, handle).await
}

pub fn claude_build_chat_completions_body(
    data: ChatCompletionsData,
    model: &Model,
) -> Result<Value> {
    let ChatCompletionsData {
        mut messages,
        temperature,
        top_p,
        functions,
        stream,
    } = data;

    let system_message = extract_system_message(&mut messages);

    let mut network_image_urls = vec![];

    let messages_len = messages.len();
    let messages: Vec<Value> = messages
        .into_iter()
        .enumerate()
        .flat_map(|(i, message)| {
            let Message { role, content } = message;
            match content {
                MessageContent::Text(text) if role.is_assistant() && i != messages_len - 1 => {
                    vec![json!({ "role": role, "content": strip_think_tag(&text) })]
                }
                MessageContent::Text(text) => vec![json!({
                    "role": role,
                    "content": text,
                })],
                MessageContent::Array(list) => {
                    let content: Vec<_> = list
                        .into_iter()
                        .map(|item| match item {
                            MessageContentPart::Text { text } => {
                                json!({"type": "text", "text": text})
                            }
                            MessageContentPart::ImageUrl {
                                image_url: ImageUrl { url },
                            } => {
                                if let Some((mime_type, data)) = url
                                    .strip_prefix("data:")
                                    .and_then(|v| v.split_once(";base64,"))
                                {
                                    json!({
                                        "type": "image",
                                        "source": {
                                            "type": "base64",
                                            "media_type": mime_type,
                                            "data": data,
                                        }
                                    })
                                } else {
                                    network_image_urls.push(url.clone());
                                    json!({ "url": url })
                                }
                            }
                        })
                        .collect();
                    vec![json!({
                        "role": role,
                        "content": content,
                    })]
                }
                MessageContent::ToolCalls(MessageContentToolCalls {
                    tool_results, text, thought, ..
                }) => {
                    let mut assistant_parts = vec![];
                    let mut user_parts = vec![];
                    if let Some(thought_text) = thought {
                        // Echo the thinking block verbatim so the API knows
                        // this assistant turn included extended thinking.
                        // The signature is stored on each tool call in the turn
                        // (issue #328: omitting this caused the model to treat
                        // its own tool calls as coming from a "previous session").
                        let signature = tool_results
                            .first()
                            .and_then(|r| r.call.thought_signature.as_deref())
                            .unwrap_or("");
                        assistant_parts.push(json!({
                            "type": "thinking",
                            "thinking": thought_text,
                            "signature": signature,
                        }));
                    }
                    if !text.is_empty() {
                        assistant_parts.push(json!({
                            "type": "text",
                            "text": text,
                        }))
                    }
                    for tool_result in tool_results {
                        assistant_parts.push(json!({
                            "type": "tool_use",
                            "id": tool_result.call.id,
                            "name": tool_result.call.name,
                            "input": tool_result.call.arguments,
                        }));
                        user_parts.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_result.call.id,
                            "content": tool_result.output.to_string(),
                        }));
                    }
                    vec![
                        json!({
                            "role": "assistant",
                            "content": assistant_parts,
                        }),
                        json!({
                            "role": "user",
                            "content": user_parts,
                        }),
                    ]
                }
            }
        })
        .collect();

    if !network_image_urls.is_empty() {
        bail!(
            "The model does not support network images: {:?}",
            network_image_urls
        );
    }

    let mut body = json!({
        "model": model.real_name(),
        "messages": messages,
    });
    if let Some(parts) = system_message {
        let system_blocks: Vec<Value> = parts
            .iter()
            .map(|text| json!({"type": "text", "text": text}))
            .collect();
        body["system"] = system_blocks.into();
    }
    if let Some(v) = model.max_tokens_param() {
        body["max_tokens"] = v.into();
    }
    if let Some(v) = temperature {
        body["temperature"] = v.into();
    }
    if let Some(v) = top_p {
        body["top_p"] = v.into();
    }
    if stream {
        body["stream"] = true.into();
    }
    if let Some(functions) = functions {
        body["tools"] = functions
            .iter()
            .map(|v| {
                json!({
                    "name": v.name,
                    "description": v.description,
                    "input_schema": v.parameters,
                })
            })
            .collect();
    }
    Ok(body)
}

pub fn claude_extract_chat_completions(data: &Value) -> Result<ChatCompletionsOutput> {
    let mut text = String::new();
    let mut reasoning: Option<String> = None;
    let mut reasoning_signature: Option<String> = None;
    let mut tool_calls = vec![];
    if let Some(list) = data["content"].as_array() {
        for item in list {
            match item["type"].as_str() {
                Some("thinking") => {
                    if let Some(v) = item["thinking"].as_str() {
                        reasoning = Some(v.to_string());
                    }
                    if let Some(s) = item["signature"].as_str() {
                        reasoning_signature = Some(s.to_string());
                    }
                }
                Some("text") => {
                    if let Some(v) = item["text"].as_str() {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(v);
                    }
                }
                Some("tool_use") => {
                    if let (Some(name), Some(input), Some(id)) = (
                        item["name"].as_str(),
                        item.get("input"),
                        item["id"].as_str(),
                    ) {
                        tool_calls.push(ToolCall::new(
                            name.to_string(),
                            input.clone(),
                            Some(id.to_string()),
                            None, // signature attached below
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    // Attach the thinking signature to every tool call in this turn.
    // The API requires it echoed back verbatim alongside the thinking block.
    if let Some(sig) = &reasoning_signature {
        for call in &mut tool_calls {
            call.thought_signature = Some(sig.clone());
        }
    }

    // When there are tool calls, carry the thought on its dedicated field so
    // the serialiser can echo back the thinking block on the next request.
    // When there are no tool calls, fold it into text for display (existing
    // behaviour for plain-text reasoning responses).
    if !tool_calls.is_empty() {
        if text.is_empty() && reasoning.is_none() {
            bail!("Invalid response data: {data}");
        }
    } else {
        if let Some(r) = &reasoning {
            text = format!("<think>\n{r}\n</think>\n\n{text}");
        }
        if text.is_empty() {
            bail!("Invalid response data: {data}");
        }
    }

    let output = ChatCompletionsOutput {
        text,
        tool_calls,
        thought: reasoning,
        id: data["id"].as_str().map(|v| v.to_string()),
        input_tokens: data["usage"]["input_tokens"].as_u64(),
        output_tokens: data["usage"]["output_tokens"].as_u64(),
        cached_tokens: data["usage"]["cache_read_input_tokens"].as_u64(),
    };
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_system_prompt_prefix_array() {
        let yaml = r#"
system_prompt_prefix:
  - identity
  - extra
"#;

        let config: ClaudeConfig = serde_yaml::from_str(yaml).expect("parse claude config");

        assert_eq!(
            config.system_prompt_prefix,
            Some(vec!["identity".to_string(), "extra".to_string()])
        );
    }

    /// Regression test for a Claude streaming parser bug where two
    /// tool_use blocks in the same response caused the first one to be
    /// emitted twice. Root cause: `content_block_stop` emitted the
    /// tool_call but left `function_name` populated, so the next
    /// `content_block_start` saw non-empty state and re-emitted the
    /// same call via its "missed stop event" fallback path.
    #[test]
    fn two_tool_uses_in_one_response_do_not_double_emit() {
        use harnx_core::abort::create_abort_signal;
        use tokio::sync::mpsc::unbounded_channel;

        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = ClaudeStreamState::default();

        let events = [
            json!({
                "type": "content_block_start",
                "content_block": {"type": "tool_use", "name": "Bash", "id": "toolu_A"}
            }),
            json!({
                "type": "content_block_delta",
                "delta": {"partial_json": "{\"command\": \"pwd\"}"}
            }),
            json!({"type": "content_block_stop"}),
            // Before the fix, this content_block_start re-emitted id=A
            // because function_name was still populated.
            json!({
                "type": "content_block_start",
                "content_block": {"type": "tool_use", "name": "Bash", "id": "toolu_B"}
            }),
            json!({
                "type": "content_block_delta",
                "delta": {"partial_json": "{\"command\": \"ls\"}"}
            }),
            json!({"type": "content_block_stop"}),
        ];

        for event in &events {
            claude_handle_stream_event(&mut state, &mut handler, event)
                .expect("stream event should process");
        }

        let ids: Vec<Option<&str>> = handler
            .tool_calls()
            .iter()
            .map(|c| c.id.as_deref())
            .collect();
        assert_eq!(
            ids,
            vec![Some("toolu_A"), Some("toolu_B")],
            "each tool_use block should be emitted exactly once"
        );
    }

    /// Regression test for issue #328. When a `ToolCalls` message carries a
    /// `thought` (extended thinking block), the serialiser must include a
    /// `{"type":"thinking","thinking":...,"signature":...}` content block as
    /// the first item in the assistant turn.  Without it the Anthropic API has
    /// no record of the model's prior reasoning and the model interprets the
    /// tool results as coming from a "previous session".
    #[test]
    fn claude_body_includes_thinking_block_when_thought_present() {
        use harnx_core::message::{Message, MessageContent, MessageContentToolCalls, MessageRole};
        use harnx_core::tool::{ToolCall, ToolResult};

        let call = ToolCall::new(
            "Bash".to_string(),
            json!({"command": "ls"}),
            Some("toolu_X".to_string()),
            None,
        );
        let tool_result = ToolResult::new(call, json!({"output": "file.txt"}));
        let tool_calls_msg = Message::new(
            MessageRole::Tool,
            MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![tool_result],
                String::new(),
                Some("I reasoned carefully".to_string()),
            )),
        );

        let messages = vec![
            Message::new(
                MessageRole::User,
                MessageContent::Text("Do something".to_string()),
            ),
            tool_calls_msg,
        ];

        let mut model = Model::new("claude", "claude-3-5-sonnet");
        model.set_max_tokens(Some(4096), true);

        let data = ChatCompletionsData {
            messages,
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
        };

        let body = claude_build_chat_completions_body(data, &model).unwrap();
        let msgs = body["messages"].as_array().expect("messages array");

        // Find the assistant turn — it follows the user message in the array.
        let assistant_msg = msgs
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("serialised messages must contain an assistant turn (issue #328: ToolCalls arm must emit one)");

        let content = assistant_msg["content"].as_array()
            .expect("assistant content should be an array");

        // The thinking block must be present and come before any tool_use block.
        let thinking_idx = content.iter().position(|b| b["type"] == "thinking")
            .expect("assistant content must contain a thinking block (issue #328: thought is dropped)");
        let tool_use_idx = content.iter().position(|b| b["type"] == "tool_use")
            .expect("assistant content must contain a tool_use block");

        assert!(
            thinking_idx < tool_use_idx,
            "thinking block must precede tool_use block"
        );
        assert_eq!(
            content[thinking_idx]["thinking"], "I reasoned carefully",
            "thinking block must carry the thought text verbatim"
        );
    }

    /// Regression test for issue #328 (parser side).  `claude_extract_chat_completions`
    /// must store the thinking block's text in `ChatCompletionsOutput.thought` and
    /// its `signature` in `ToolCall.thought_signature` so the serialiser can echo
    /// them back on the next turn.
    #[test]
    fn claude_extract_preserves_thought_and_signature() {
        let response = json!({
            "id": "msg_test",
            "content": [
                {
                    "type": "thinking",
                    "thinking": "Let me think...",
                    "signature": "sig_abc123"
                },
                {
                    "type": "tool_use",
                    "id": "toolu_X",
                    "name": "Bash",
                    "input": {"command": "ls"}
                }
            ],
            "usage": {"input_tokens": 10, "output_tokens": 20}
        });

        let output = claude_extract_chat_completions(&response)
            .expect("extraction should succeed");

        assert_eq!(
            output.thought,
            Some("Let me think...".to_string()),
            "thought must be stored in ChatCompletionsOutput.thought (issue #328: currently always None)"
        );
        assert_eq!(
            output.tool_calls[0].thought_signature,
            Some("sig_abc123".to_string()),
            "thinking signature must be stored in ToolCall.thought_signature (issue #328: currently always None)"
        );
    }

    #[test]
    fn claude_body_has_array_system_blocks() {
        use harnx_core::message::{Message, MessageContent, MessageContentPart, MessageRole};

        let messages = vec![
            Message::new(
                MessageRole::System,
                MessageContent::Array(vec![
                    MessageContentPart::Text {
                        text: "identity".to_string(),
                    },
                    MessageContentPart::Text {
                        text: "extra".to_string(),
                    },
                    MessageContentPart::Text {
                        text: "Be helpful".to_string(),
                    },
                ]),
            ),
            Message::new(
                MessageRole::User,
                MessageContent::Text("Hello".to_string()),
            ),
        ];

        let mut model = Model::new("claude", "claude-3-5-sonnet");
        model.set_max_tokens(Some(4096), true);

        let data = ChatCompletionsData {
            messages,
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
        };

        let body = claude_build_chat_completions_body(data, &model).unwrap();

        let system = body["system"].as_array().expect("system should be an array");
        assert_eq!(system.len(), 3);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "identity");
        assert_eq!(system[1]["type"], "text");
        assert_eq!(system[1]["text"], "extra");
        assert_eq!(system[2]["type"], "text");
        assert_eq!(system[2]["text"], "Be helpful");
    }
}
