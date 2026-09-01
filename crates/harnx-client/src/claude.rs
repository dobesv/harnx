use crate::*;
use crate::claude_upload::{AnthropicAttachmentEncoder, ANTHROPIC_FILES_BETA_HEADER_VALUE};

use harnx_core::attachments::{collect_cid_refs, shared_attachment_cache, ExpandedAttachment, CID_PREFIX};
use harnx_core::text::strip_think_tag;

use anyhow::{bail, Context, Result};
use reqwest::{Client as ReqwestClient, RequestBuilder};
use serde_json::{json, Value};
use std::collections::HashMap;

const API_BASE: &str = "https://api.anthropic.com/v1";

impl ClaudeClient {
    config_get_fn!(api_key, get_api_key);
    config_get_fn!(api_base, get_api_base);

    pub const PROMPTS: [PromptAction<'static>; 1] = [("api_key", "API Key", None)];
}

#[async_trait::async_trait]
impl Client for ClaudeClient {
    client_common_fns!();

    fn expands_attachments_internally(&self) -> bool {
        true
    }

    async fn chat_completions_inner(
        &self,
        client: &ReqwestClient,
        data: ChatCompletionsData,
    ) -> Result<ChatCompletionsOutput> {
        let builder = self.prepare_and_build_request(client, data).await?;
        claude_chat_completions(builder, &self.model).await
    }

    async fn chat_completions_streaming_inner(
        &self,
        client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()> {
        let builder = self.prepare_and_build_request(client, data).await?;
        claude_chat_completions_streaming(builder, handler, &self.model).await
    }

    async fn embeddings_inner(
        &self,
        client: &ReqwestClient,
        data: &EmbeddingsData,
    ) -> Result<EmbeddingsOutput> {
        let request_data = noop_prepare_embeddings(self, data)?;
        let builder = self.request_builder(client, request_data)?;
        noop_embeddings(builder, self.model()).await
    }

    async fn rerank_inner(
        &self,
        client: &ReqwestClient,
        data: &RerankData,
    ) -> Result<RerankOutput> {
        let request_data = noop_prepare_rerank(self, data)?;
        let builder = self.request_builder(client, request_data)?;
        noop_rerank(builder, self.model()).await
    }
}

impl ClaudeClient {
    async fn prepare_and_build_request(
        &self,
        client: &ReqwestClient,
        data: ChatCompletionsData,
    ) -> Result<RequestBuilder> {
        let api_key = self.get_api_key()?;
        let api_base = self
            .get_api_base()
            .unwrap_or_else(|_| API_BASE.to_string());

        let expanded_attachments = if self.model.supports_vision() && data.attachments_dir.is_some() {
            let dir = data.attachments_dir.clone().unwrap();
            let cache = shared_attachment_cache(self.name());
            let encoder = AnthropicAttachmentEncoder::new_with_cache(
                client.clone(),
                api_key.clone(),
                api_base.clone(),
                cache,
            );
            let cid_refs = collect_cid_refs(&data.messages);
            let mut map = HashMap::new();
            for cid in cid_refs {
                match encoder.expand(&dir, &cid).await {
                    Ok(expanded) => {
                        map.insert(cid, expanded);
                    }
                    Err(err) => {
                        warn!("Failed to expand Claude attachment {}: {}", cid, err);
                    }
                }
            }
            map
        } else {
            HashMap::new()
        };

        let uses_file_api = expanded_attachments.values().any(|attachment| {
            matches!(attachment, ExpandedAttachment::RemoteRef { ref_id, .. } if ref_id.starts_with("file_"))
        });

        let url = format!("{}/messages", api_base.trim_end_matches('/'));
        let body = claude_build_chat_completions_body(data, &self.model, &expanded_attachments)?;

        let mut request_data = RequestData::new(url, body);
        request_data.header("anthropic-version", "2023-06-01");
        if uses_file_api {
            request_data.header("anthropic-beta", ANTHROPIC_FILES_BETA_HEADER_VALUE);
        }
        if api_key.starts_with("sk-ant-oat") {
            request_data.bearer_auth(api_key);
        } else {
            request_data.header("x-api-key", api_key);
        }

        self.request_builder(client, request_data)
    }
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
    harnx_core::llm_trace::response("claude", &data);
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
        // Route thinking deltas to the dedicated thought buffer so the
        // serialiser can echo a `{"type":"thinking",...}` block on the next
        // request. Routing to `handler.text()` instead folds thinking into
        // the text buffer wrapped in `<think>...</think>` and returns
        // `thought = None`, which makes the next turn omit the thinking
        // block entirely — the model then sees its own tool calls as
        // orphaned and produces "previous session" hallucinations.
        handler.thought(text)?;
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

/// Add two optional u64 values. If both are None, returns None.
/// If one is Some and the other None, returns the Some value.
/// Uses saturating_add to avoid overflow panics in debug builds.
fn add_opt_u64(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
    }
}

fn claude_usage(usage: &Value) -> StreamingUsage {
    let cached_tokens = usage["cache_read_input_tokens"].as_u64();
    let cache_write_tokens = usage["cache_creation_input_tokens"].as_u64();
    let input_tokens = add_opt_u64(
        add_opt_u64(usage["input_tokens"].as_u64(), cache_write_tokens),
        cached_tokens,
    );
    StreamingUsage {
        input_tokens,
        // Anthropic omits output tokens on message_start, so this stays None there.
        // message_delta supplies the real value and set_usage's or-merge replaces it,
        // keeping streaming and non-streaming usage on one safe parsing path.
        output_tokens: usage["output_tokens"].as_u64(),
        cached_tokens,
        cache_write_tokens,
    }
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
            // Anthropic reports cache buckets disjoint from input tokens.
            handler.set_usage(claude_usage(&data["message"]["usage"]));
        }
        "message_delta" => {
            // Cumulative fields override message_start values when present.
            handler.set_usage(claude_usage(&data["usage"]));
        }
        "content_block_start" => claude_handle_content_block_start(state, handler, data)?,
        "content_block_delta" => claude_handle_content_block_delta(state, handler, data)?,
        "content_block_stop" => claude_handle_content_block_stop(state, handler)?,
        "error" => {
            let _ = data;
            return crate::catch_error(data, 500, None);
        }
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
        harnx_core::llm_trace::stream_event("claude", &data);
        claude_handle_stream_event(&mut state, handler, &data)?;
        Ok(false)
    };

    sse_stream(builder, handle).await
}

fn claude_attachment_placeholder() -> Value {
    json!({
        "type": "text",
        "text": "[attachment unavailable: missing expanded attachment]",
    })
}

fn claude_attachment_part(
    url: &str,
    expanded_attachments: &HashMap<String, ExpandedAttachment>,
    network_image_urls: &mut Vec<String>,
) -> Value {
    if url.starts_with(CID_PREFIX) {
        return match expanded_attachments.get(url) {
            Some(ExpandedAttachment::RemoteRef { ref_id, .. }) => json!({
                "type": "image",
                "source": {
                    "type": "file",
                    "file_id": ref_id,
                }
            }),
            Some(ExpandedAttachment::DataUri { data, mime_type }) => json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime_type,
                    "data": data,
                }
            }),
            None => claude_attachment_placeholder(),
        };
    }

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
        network_image_urls.push(url.to_string());
        json!({ "url": url })
    }
}

pub fn claude_build_chat_completions_body(
    data: ChatCompletionsData,
    model: &Model,
    expanded_attachments: &HashMap<String, ExpandedAttachment>,
) -> Result<Value> {
    let ChatCompletionsData {
        mut messages,
        temperature,
        top_p,
        functions,
        stream,
        attachments_dir: _,  // Claude uses the runtime base64 pre-pass
    } = data;

    let system_message = extract_system_message(&mut messages);

    let mut network_image_urls = vec![];

    let messages_len = messages.len();
    let messages: Vec<Value> = messages
        .into_iter()
        .enumerate()
        .flat_map(|(i, message)| {
            let Message { role, content, .. } = message;
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
                            } => claude_attachment_part(
                                &url,
                                expanded_attachments,
                                &mut network_image_urls,
                            )
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
                        let tr_content = if tool_result.content.is_empty() {
                            json!(tool_result.output.to_string())
                        } else {
                            let mut blocks = vec![json!({
                                "type": "text",
                                "text": tool_result.output.to_string()
                            })];
                            for part in &tool_result.content {
                                if let MessageContentPart::ImageUrl {
                                    image_url: crate::ImageUrl { url },
                                } = part
                                {
                                    blocks.push(claude_attachment_part(
                                        url,
                                        expanded_attachments,
                                        &mut network_image_urls,
                                    ));
                                }
                            }
                            json!(blocks)
                        };
                        user_parts.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_result.call.id,
                            "content": tr_content,
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

    let usage = claude_usage(&data["usage"]);
    let output = ChatCompletionsOutput {
        text,
        tool_calls,
        thought: reasoning,
        id: data["id"].as_str().map(|v| v.to_string()),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_tokens: usage.cached_tokens,
        cache_write_tokens: usage.cache_write_tokens,
    };
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_output_usage(actual: &ChatCompletionsOutput, expected: &CompletionTokenUsage) {
        assert_eq!(
            (
                actual.input_tokens,
                actual.output_tokens,
                actual.cached_tokens,
                actual.cache_write_tokens,
            ),
            (
                Some(expected.input_tokens),
                Some(expected.output_tokens),
                Some(expected.cached_tokens),
                Some(expected.cache_write_tokens),
            )
        );
        assert!(expected.input_tokens >= expected.cached_tokens + expected.cache_write_tokens);
    }

    fn assert_usage(actual: &CompletionTokenUsage, expected: &CompletionTokenUsage) {
        assert_eq!(actual, expected);
        assert!(actual.input_tokens >= actual.cached_tokens + actual.cache_write_tokens);
    }

    #[test]
    fn claude_array_attachment_uses_file_source_for_remote_ref() {
        use harnx_core::message::ImageUrl;
        let model = Model::new("claude", "claude-3-5-sonnet");
        let messages = vec![Message::new(
            MessageRole::User,
            MessageContent::Array(vec![MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "cid:top".into(),
                },
            }]),
        )];
        let mut expanded = HashMap::new();
        expanded.insert(
            "cid:top".into(),
            ExpandedAttachment::RemoteRef {
                ref_id: "file_123".into(),
                mime_type: "image/png".into(),
                expires_at: None,
            },
        );

        let body = claude_build_chat_completions_body(
            ChatCompletionsData {
                messages,
                temperature: None,
                top_p: None,
                functions: None,
                stream: false,
                attachments_dir: None,
            },
            &model,
            &expanded,
        )
        .unwrap();

        assert_eq!(body["messages"][0]["content"][0]["source"]["file_id"], "file_123");
    }

    #[test]
    fn claude_tool_result_attachment_uses_file_source_for_remote_ref() {
        use harnx_core::message::{ImageUrl, MessageContentToolCalls};
        use harnx_core::tool::ToolResult;
        let model = Model::new("claude", "claude-3-5-sonnet");
        let tool_call = ToolCall::new("show_image".to_string(), json!({}), Some("toolu_1".into()), None);
        let mut tool_result = ToolResult::new(tool_call, json!("output text"));
        tool_result.content = vec![MessageContentPart::ImageUrl {
            image_url: ImageUrl {
                url: "cid:tool".into(),
            },
        }];
        let messages = vec![
            Message::new(MessageRole::User, MessageContent::Text("show".into())),
            Message::new(
                MessageRole::Tool,
                MessageContent::ToolCalls(MessageContentToolCalls::new(vec![tool_result], String::new(), None)),
            ),
        ];
        let mut expanded = HashMap::new();
        expanded.insert(
            "cid:tool".into(),
            ExpandedAttachment::RemoteRef {
                ref_id: "file_abc".into(),
                mime_type: "image/png".into(),
                expires_at: None,
            },
        );
        let body = claude_build_chat_completions_body(ChatCompletionsData { messages, temperature: None, top_p: None, functions: None, stream: false, attachments_dir: None }, &model, &expanded).unwrap();
        let user_msg = body["messages"].as_array().unwrap().iter().find(|m| m["role"] == "user" && m["content"].as_array().is_some_and(|c| !c.is_empty() && c[0].get("type").is_some_and(|t| t == "tool_result"))).unwrap();
        assert_eq!(user_msg["content"][0]["content"][1]["source"]["file_id"], "file_abc");
    }

    #[test]
    fn claude_attachment_uses_base64_source_for_data_uri() {
        use harnx_core::message::ImageUrl;
        let model = Model::new("claude", "claude-3-5-sonnet");
        let messages = vec![Message::new(
            MessageRole::User,
            MessageContent::Array(vec![MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "cid:data".into(),
                },
            }]),
        )];
        let mut expanded = HashMap::new();
        expanded.insert("cid:data".into(), ExpandedAttachment::DataUri { data: "QUJD".into(), mime_type: "image/png".into() });
        let body = claude_build_chat_completions_body(ChatCompletionsData { messages, temperature: None, top_p: None, functions: None, stream: false, attachments_dir: None }, &model, &expanded).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["source"]["type"], "base64");
    }

    #[test]
    fn claude_missing_cid_degrades_to_placeholder() {
        use harnx_core::message::ImageUrl;
        let model = Model::new("claude", "claude-3-5-sonnet");
        let messages = vec![Message::new(
            MessageRole::User,
            MessageContent::Array(vec![MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "cid:missing".into(),
                },
            }]),
        )];
        let body = claude_build_chat_completions_body(ChatCompletionsData { messages, temperature: None, top_p: None, functions: None, stream: false, attachments_dir: None }, &model, &HashMap::new()).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["text"], "[attachment unavailable: missing expanded attachment]");
    }

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
            attachments_dir: None,
        };

        let body = claude_build_chat_completions_body(data, &model, &HashMap::new()).unwrap();
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

    /// End-to-end thinking + signature round-trip on the STREAMING path.
    ///
    /// The non-streaming round-trip is covered by
    /// `claude_extract_preserves_thought_and_signature` plus
    /// `claude_body_includes_thinking_block_when_thought_present`. The
    /// streaming path can regress the same "previous session" symptom
    /// independently when thinking text is delivered as
    /// `content_block_delta` events with a trailing `signature_delta`.
    ///
    /// This test drives `claude_handle_stream_event` with a realistic
    /// event sequence (thinking deltas → signature_delta → tool_use),
    /// takes the `SseHandler` output the same way
    /// `run_chat_completion_streaming` does, then feeds it back into
    /// `claude_build_chat_completions_body` to verify the next request
    /// includes the thinking block + signature. If thinking deltas land
    /// in the text buffer instead of the thought buffer the serialiser
    /// emits an assistant turn with no thinking block and the model sees
    /// its tool calls as orphaned.
    #[test]
    fn claude_streaming_thought_roundtrips_into_next_request_body() {
        use harnx_core::abort::create_abort_signal;
        use harnx_core::message::{Message, MessageContent, MessageContentToolCalls, MessageRole};
        use harnx_core::tool::ToolResult;
        use tokio::sync::mpsc::unbounded_channel;

        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = ClaudeStreamState::default();

        // Realistic Anthropic streaming sequence: thinking block (with
        // multi-chunk text and a signature_delta), then a tool_use block.
        let events = [
            json!({
                "type": "message_start",
                "message": {"usage": {"input_tokens": 100, "cache_read_input_tokens": 0}}
            }),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": ""}
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "Let me think "}
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "about this."}
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": "sig_stream_xyz"}
            }),
            json!({"type": "content_block_stop", "index": 0}),
            json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "tool_use", "name": "Bash", "id": "toolu_S"}
            }),
            json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "{\"command\":\"ls\"}"}
            }),
            json!({"type": "content_block_stop", "index": 1}),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 42}
            }),
        ];

        for event in &events {
            claude_handle_stream_event(&mut state, &mut handler, event)
                .expect("stream event should process");
        }

        // Drain the handler the same way run_chat_completion_streaming does.
        let (text, thought, tool_calls, _usage) = handler.take();

        // The thinking content must end up on the dedicated `thought` field,
        // NOT folded into the text buffer with <think>...</think> wrappers.
        // If it lands in `text`, the next turn's request body has no thinking
        // block to echo back and the model treats the tool results as
        // orphaned.
        assert_eq!(
            thought.as_deref(),
            Some("Let me think about this."),
            "streaming thought must be captured in the dedicated thought field, \
             not the text buffer"
        );
        assert!(
            !text.contains("<think>"),
            "streaming text must not be polluted with <think> wrappers when \
             tool calls are present — the wrapper is meant for plain-text \
             reasoning responses; tool-call turns echo the raw thinking block. \
             Got text: {text:?}"
        );
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(
            tool_calls[0].thought_signature.as_deref(),
            Some("sig_stream_xyz"),
            "streaming signature must reach the tool_call"
        );

        // Now simulate what the agent loop does: build a ToolCalls message
        // from (text, thought, tool_calls), then build the next request body.
        let tool_result = ToolResult::new(tool_calls.into_iter().next().unwrap(), json!("ok"));
        let messages = vec![
            Message::new(
                MessageRole::User,
                MessageContent::Text("Run a command".to_string()),
            ),
            Message::new(
                MessageRole::Tool,
                MessageContent::ToolCalls(MessageContentToolCalls::new(
                    vec![tool_result],
                    text,
                    thought,
                )),
            ),
        ];

        let mut model = Model::new("claude", "claude-3-5-sonnet");
        model.set_max_tokens(Some(4096), true);

        let body = claude_build_chat_completions_body(
            ChatCompletionsData {
                messages,
                temperature: None,
                top_p: None,
                functions: None,
                stream: true,
                attachments_dir: None,
            },
            &model,
            &HashMap::new(),
        )
        .unwrap();

        let assistant_msg = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("must have an assistant turn");
        let content = assistant_msg["content"]
            .as_array()
            .expect("assistant content array");

        let thinking_block = content
            .iter()
            .find(|b| b["type"] == "thinking")
            .expect(
                "next request body must include a thinking block for the \
                 streamed assistant turn: otherwise the model receives tool \
                 results with no record of its prior reasoning and infers a \
                 session boundary",
            );
        assert_eq!(thinking_block["thinking"], "Let me think about this.");
        assert_eq!(
            thinking_block["signature"], "sig_stream_xyz",
            "thinking block signature must be echoed verbatim from the \
             streamed signature_delta"
        );
    }

    /// Multiple tool_use blocks in one streamed turn must all carry the same
    /// thought signature. The Anthropic API rejects requests where any
    /// tool_use sibling of a thinking block lacks its signature when echoed
    /// back, and the signature is shared across all tool calls in the turn.
    /// Without this, a 2-tool turn would round-trip with one valid call and
    /// one orphan call on the next request.
    #[test]
    fn claude_streaming_multiple_tool_calls_share_thought_signature() {
        use harnx_core::abort::create_abort_signal;
        use tokio::sync::mpsc::unbounded_channel;

        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = ClaudeStreamState::default();

        let events = [
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": ""}
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "Plan two calls."}
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": "sig_multi"}
            }),
            json!({"type": "content_block_stop", "index": 0}),
            json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "tool_use", "name": "Bash", "id": "toolu_A"}
            }),
            json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "{\"command\":\"pwd\"}"}
            }),
            json!({"type": "content_block_stop", "index": 1}),
            json!({
                "type": "content_block_start",
                "index": 2,
                "content_block": {"type": "tool_use", "name": "Bash", "id": "toolu_B"}
            }),
            json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {"type": "input_json_delta", "partial_json": "{\"command\":\"ls\"}"}
            }),
            json!({"type": "content_block_stop", "index": 2}),
        ];
        for event in &events {
            claude_handle_stream_event(&mut state, &mut handler, event)
                .expect("stream event should process");
        }

        let (_text, thought, tool_calls, _usage) = handler.take();
        assert_eq!(thought.as_deref(), Some("Plan two calls."));
        assert_eq!(tool_calls.len(), 2);
        for call in &tool_calls {
            assert_eq!(
                call.thought_signature.as_deref(),
                Some("sig_multi"),
                "every streamed tool_use sibling of a thinking block must carry \
                 its signature so the next request body is well-formed"
            );
        }
    }

    /// Mirror of `claude_streaming_multiple_tool_calls_share_thought_signature`
    /// for the non-streaming path. `claude_extract_chat_completions` already
    /// loops over all tool_calls and assigns the captured signature to each;
    /// this test pins that behavior so a future refactor can't drop the loop
    /// and silently break multi-call thinking turns.
    #[test]
    fn claude_extract_attaches_signature_to_every_tool_call() {
        let response = json!({
            "id": "msg_multi",
            "content": [
                {"type": "thinking", "thinking": "two calls", "signature": "sig_multi"},
                {"type": "tool_use", "id": "toolu_A", "name": "Bash", "input": {"command": "pwd"}},
                {"type": "tool_use", "id": "toolu_B", "name": "Bash", "input": {"command": "ls"}}
            ],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });

        let output = claude_extract_chat_completions(&response).unwrap();
        assert_eq!(output.tool_calls.len(), 2);
        for call in &output.tool_calls {
            assert_eq!(
                call.thought_signature.as_deref(),
                Some("sig_multi"),
                "every parsed tool_use sibling of a thinking block must carry \
                 its signature (non-streaming multi-call)"
            );
        }
    }

    /// Streaming text-only response with extended thinking must populate
    /// `thought` cleanly without polluting `text`. This is the dual of the
    /// thinking+tool_use roundtrip — it pins the behavior the streaming-side
    /// fix relies on for the no-tool-calls path so a future refactor of
    /// `handler.thought()` can't silently re-introduce `<think>` wrappers
    /// into the text buffer.
    #[test]
    fn claude_streaming_text_only_with_thinking_separates_buffers() {
        use harnx_core::abort::create_abort_signal;
        use tokio::sync::mpsc::unbounded_channel;

        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = ClaudeStreamState::default();

        let events = [
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": ""}
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "Considering."}
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": "sig_text"}
            }),
            json!({"type": "content_block_stop", "index": 0}),
            json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "text", "text": ""}
            }),
            json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "text_delta", "text": "Final answer."}
            }),
            json!({"type": "content_block_stop", "index": 1}),
        ];
        for event in &events {
            claude_handle_stream_event(&mut state, &mut handler, event)
                .expect("stream event should process");
        }

        let (text, thought, tool_calls, _usage) = handler.take();
        assert_eq!(text, "Final answer.", "text buffer carries only the prose");
        assert_eq!(thought.as_deref(), Some("Considering."));
        assert!(
            tool_calls.is_empty(),
            "no tool_use blocks were sent; tool_calls must stay empty"
        );
    }

    #[test]
    fn claude_streaming_error_event_fails() {
        use harnx_core::abort::create_abort_signal;
        use tokio::sync::mpsc::unbounded_channel;
        use harnx_core::error::LlmError;

        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = ClaudeStreamState::default();

        let event = json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": "Internal server error"
            }
        });

        let result = claude_handle_stream_event(&mut state, &mut handler, &event);
        assert!(result.is_err(), "Stream error event should return an error");
        let err = result.unwrap_err();
        let llm_err = err.downcast_ref::<LlmError>().expect("Should be an LlmError");
        assert_eq!(llm_err.status, 500);
        assert!(llm_err.is_retryable());
        assert!(llm_err.message.contains("Internal server error"));
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
            attachments_dir: None,
        };

        let body = claude_build_chat_completions_body(data, &model, &HashMap::new()).unwrap();

        let system = body["system"].as_array().expect("system should be an array");
        assert_eq!(system.len(), 3);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "identity");
        assert_eq!(system[1]["type"], "text");
        assert_eq!(system[1]["text"], "extra");
        assert_eq!(system[2]["type"], "text");
        assert_eq!(system[2]["text"], "Be helpful");
    }

    /// Regression test for issue #159.
    /// `claude_extract_chat_completions` must add Anthropic's disjoint cache
    /// buckets to `input_tokens` so the "In" count reflects all input tokens.
    #[test]
    fn claude_extract_includes_cache_creation_in_input_tokens() {
        let response = json!({
            "id": "msg_test",
            "content": [{"type": "text", "text": "Hello"}],
            "usage": {
                "input_tokens": 5,
                "cache_creation_input_tokens": 1000,
                "cache_read_input_tokens": 0,
                "output_tokens": 10
            }
        });

        let output = claude_extract_chat_completions(&response)
            .expect("extraction should succeed");

        assert_output_usage(
            &output,
            &CompletionTokenUsage {
                input_tokens: 1005,
                output_tokens: 10,
                cached_tokens: 0,
                cache_write_tokens: 1000,
            },
        );
    }

    /// Regression test for issue #159 — non-zero cache_read alongside cache_creation.
    #[test]
    fn claude_extract_all_three_token_buckets() {
        let response = json!({
            "id": "msg_test",
            "content": [{"type": "text", "text": "Hello"}],
            "usage": {
                "input_tokens": 5,
                "cache_creation_input_tokens": 1000,
                "cache_read_input_tokens": 500,
                "output_tokens": 10
            }
        });

        let output = claude_extract_chat_completions(&response)
            .expect("extraction should succeed");

        assert_output_usage(
            &output,
            &CompletionTokenUsage {
                input_tokens: 1505,
                output_tokens: 10,
                cached_tokens: 500,
                cache_write_tokens: 1000,
            },
        );
    }

    #[test]
    fn claude_streaming_normalizes_all_cache_buckets() {
        use harnx_core::abort::create_abort_signal;
        use tokio::sync::mpsc::unbounded_channel;

        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = ClaudeStreamState::default();
        let start = json!({
            "type": "message_start",
            "message": {"usage": {
                "input_tokens": 5,
                "cache_creation_input_tokens": 1000,
                "cache_read_input_tokens": 500
            }}
        });
        let delta = json!({
            "type": "message_delta",
            "usage": {"output_tokens": 10}
        });
        claude_handle_stream_event(&mut state, &mut handler, &start)
            .expect("message_start usage should parse");
        claude_handle_stream_event(&mut state, &mut handler, &delta)
            .expect("message_delta usage should parse");

        let (_, _, _, usage) = handler.take();
        assert_usage(
            &usage,
            &CompletionTokenUsage {
                input_tokens: 1505,
                output_tokens: 10,
                cached_tokens: 500,
                cache_write_tokens: 1000,
            },
        );
    }

    /// Regression test for issue #159 — no cache_creation field present.
    #[test]
    fn claude_extract_input_tokens_without_cache_creation() {
        let response = json!({
            "id": "msg_test",
            "content": [{"type": "text", "text": "Hello"}],
            "usage": {
                "input_tokens": 42,
                "output_tokens": 7
            }
        });

        let output = claude_extract_chat_completions(&response)
            .expect("extraction should succeed");

        assert_eq!(output.input_tokens, Some(42));
    }

    /// Regression test for issue #159 — only cache_creation, no input_tokens field.
    #[test]
    fn claude_extract_only_cache_creation_tokens() {
        let response = json!({
            "id": "msg_test",
            "content": [{"type": "text", "text": "Hello"}],
            "usage": {
                "cache_creation_input_tokens": 2000,
                "output_tokens": 5
            }
        });

        let output = claude_extract_chat_completions(&response)
            .expect("extraction should succeed");

        assert_eq!(output.input_tokens, Some(2000), "cache_creation alone becomes input_tokens");
    }

    /// add_opt_u64 helper edge cases including saturating behaviour.
    #[test]
    fn add_opt_u64_edge_cases() {
        assert_eq!(add_opt_u64(None, None), None);
        assert_eq!(add_opt_u64(Some(5), None), Some(5));
        assert_eq!(add_opt_u64(None, Some(10)), Some(10));
        assert_eq!(add_opt_u64(Some(5), Some(10)), Some(15));
        // saturating_add: overflow saturates to u64::MAX rather than panicking
        assert_eq!(add_opt_u64(Some(u64::MAX), Some(1)), Some(u64::MAX));
    }

    /// Package-namespaced clients (e.g. "pantheon/claude") must look up
    /// CLAUDE_API_KEY, not the invalid PANTHEON/CLAUDE_API_KEY.
    #[test]
    fn package_client_uses_bare_name_for_env_var() {
        let mut config: ClaudeConfig =
            serde_yaml::from_str("type: claude").expect("parse config");
        config.name = "pantheon/claude".to_string();
        let client = ClaudeClient {
            config,
            model: Model::new("pantheon/claude", "claude-3-5-sonnet"),
        };

        // Set CLAUDE_API_KEY (bare name, no package prefix).
        unsafe { std::env::set_var("CLAUDE_API_KEY", "test-key-bare") };
        let result = client.get_api_key();
        unsafe { std::env::remove_var("CLAUDE_API_KEY") };

        assert_eq!(result.unwrap(), "test-key-bare");
    }

    /// When a ToolResult has image content, tool_result content should be an array
    /// containing a text block and an image block with base64 data.
    #[test]
    fn claude_tool_result_with_image_emits_array_with_image_block() {
        use harnx_core::tool::ToolResult;
        let model = Model::new("claude", "claude-3-5-sonnet");
        let tool_call = ToolCall {
            id: Some("toolu_XYZ".to_string()),
            name: "fs_read".to_string(),
            arguments: json!({"path": "foo.png"}),
            thought_signature: None,
        };
        let mut tool_result = ToolResult::new(tool_call, json!("output text"));
        tool_result.content.push(MessageContentPart::ImageUrl {
            image_url: crate::ImageUrl {
                url: "data:image/png;base64,iVBORw0KGgo".to_string(),
            },
        });

        let messages = vec![
            Message::new(
                MessageRole::User,
                MessageContent::Text("Do something".to_string()),
            ),
            Message::new(
                MessageRole::Tool,
                MessageContent::ToolCalls(MessageContentToolCalls::new(
                    vec![tool_result],
                    "".to_string(),
                    None,
                )),
            ),
        ];

        let body = claude_build_chat_completions_body(
            ChatCompletionsData {
                messages,
                temperature: None,
                top_p: None,
                functions: None,
                stream: false,
            attachments_dir: None,
            },
            &model,
            &HashMap::new(),
        )
        .unwrap();

        let user_msg = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "user" && m["content"].as_array().is_some_and(|c| !c.is_empty() && c[0].get("type").is_some_and(|t| t == "tool_result")))
            .expect("must have a user tool_result turn");

        let content = user_msg["content"]
            .as_array()
            .expect("user content array")[0]["content"]
            .as_array()
            .expect("tool_result content should be an array when images present");

        assert_eq!(content.len(), 2, "must have 2 blocks (text and image)");
        // tool_result.output.to_string() JSON-encodes the Value, so "output text" becomes "\"output text\""
        assert_eq!(content[0], json!({"type": "text", "text": "\"output text\""}));
        assert_eq!(
            content[1],
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "iVBORw0KGgo"
                }
            })
        );
    }

    /// When a ToolResult has no content, tool_result content should be a string
    /// (unchanged from prior behavior).
    #[test]
    fn claude_tool_result_without_image_emits_string() {
        use harnx_core::tool::ToolResult;
        let model = Model::new("claude", "claude-3-5-sonnet");
        let tool_call = ToolCall {
            id: Some("toolu_ABC".to_string()),
            name: "fs_read".to_string(),
            arguments: json!({"path": "foo.txt"}),
            thought_signature: None,
        };
        let tool_result = ToolResult::new(tool_call, json!({"status": "ok"}));

        let messages = vec![
            Message::new(
                MessageRole::User,
                MessageContent::Text("Do something".to_string()),
            ),
            Message::new(
                MessageRole::Tool,
                MessageContent::ToolCalls(MessageContentToolCalls::new(
                    vec![tool_result],
                    "".to_string(),
                    None,
                )),
            ),
        ];

        let body = claude_build_chat_completions_body(
            ChatCompletionsData {
                messages,
                temperature: None,
                top_p: None,
                functions: None,
                stream: false,
            attachments_dir: None,
            },
            &model,
            &HashMap::new(),
        )
        .unwrap();

        let user_msg = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "user" && m["content"].as_array().is_some_and(|c| !c.is_empty() && c[0].get("type").is_some_and(|t| t == "tool_result")))
            .expect("must have a user tool_result turn");

        let content = &user_msg["content"]
            .as_array()
            .expect("user content array")[0]["content"];

        assert!(content.is_string(), "tool_result content should be a string when no images");
        assert_eq!(content.as_str().unwrap(), "{\"status\":\"ok\"}");
    }
}
