use crate::*;

use harnx_core::text::strip_think_tag;

use anyhow::{bail, Context, Result};
use reqwest::RequestBuilder;
use serde::Deserialize;
use serde_json::{json, Value};

const API_BASE: &str = "https://api.openai.com/v1";

impl OpenAIClient {
    config_get_fn!(api_key, get_api_key);
    config_get_fn!(api_base, get_api_base);

    pub const PROMPTS: [PromptAction<'static>; 1] = [("api_key", "API Key", None)];
}

impl_client_trait!(
    OpenAIClient,
    (
        prepare_chat_completions,
        openai_chat_completions,
        openai_chat_completions_streaming
    ),
    (prepare_embeddings, openai_embeddings),
    (noop_prepare_rerank, noop_rerank),
);

fn prepare_chat_completions(
    self_: &OpenAIClient,
    data: ChatCompletionsData,
) -> Result<RequestData> {
    let api_key = self_.get_api_key()?;
    let api_base = self_
        .get_api_base()
        .unwrap_or_else(|_| API_BASE.to_string());

    let url = format!("{}/chat/completions", api_base.trim_end_matches('/'));

    let body = openai_build_chat_completions_body(data, &self_.model);

    let mut request_data = RequestData::new(url, body);

    request_data.bearer_auth(api_key);
    if let Some(organization_id) = &self_.config.organization_id {
        request_data.header("OpenAI-Organization", organization_id);
    }

    Ok(request_data)
}

fn prepare_embeddings(self_: &OpenAIClient, data: &EmbeddingsData) -> Result<RequestData> {
    let api_key = self_.get_api_key()?;
    let api_base = self_
        .get_api_base()
        .unwrap_or_else(|_| API_BASE.to_string());

    let url = format!("{api_base}/embeddings");

    let body = openai_build_embeddings_body(data, &self_.model);

    let mut request_data = RequestData::new(url, body);

    request_data.bearer_auth(api_key);
    if let Some(organization_id) = &self_.config.organization_id {
        request_data.header("OpenAI-Organization", organization_id);
    }

    Ok(request_data)
}

pub async fn openai_chat_completions(
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
    harnx_core::llm_trace::response("openai", &data);
    openai_extract_chat_completions(&data)
}

/// Mutable accumulator state for the OpenAI streaming parser. Extracted
/// so per-event handling is testable in isolation.
#[derive(Default)]
struct OpenAiStreamState {
    call_id: String,
    function_name: String,
    function_arguments: String,
    function_id: String,
    reasoning_state: i32,
}

fn openai_emit_pending_tool_call(
    state: &mut OpenAiStreamState,
    handler: &mut SseHandler,
) -> Result<()> {
    if state.function_name.is_empty() {
        return Ok(());
    }
    if state.function_arguments.is_empty() {
        state.function_arguments = String::from("{}");
    }
    let arguments: Value = state.function_arguments.parse().with_context(|| {
        format!(
            "Tool call '{}' have non-JSON arguments '{}'",
            state.function_name, state.function_arguments
        )
    })?;
    handler.tool_call(ToolCall::new(
        state.function_name.clone(),
        arguments,
        normalize_function_id(&state.function_id),
        None,
    ))?;
    state.function_name.clear();
    state.function_arguments.clear();
    state.function_id.clear();
    Ok(())
}

/// Transition the reasoning-block bracket state. Emits `<think>\n` when
/// opening and `\n</think>\n\n` when closing; no-op when already in the
/// target state.
fn openai_transition_reasoning(
    state: &mut OpenAiStreamState,
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

fn openai_handle_text_delta(
    state: &mut OpenAiStreamState,
    handler: &mut SseHandler,
    data: &Value,
) -> Result<()> {
    let delta = &data["choices"][0]["delta"];
    if let Some(text) = delta["content"].as_str().filter(|v| !v.is_empty()) {
        openai_transition_reasoning(state, handler, false)?;
        handler.text(text)?;
    } else if let Some(text) = delta["reasoning_content"]
        .as_str()
        .or_else(|| delta["reasoning"].as_str())
        .filter(|v| !v.is_empty())
    {
        openai_transition_reasoning(state, handler, true)?;
        handler.text(text)?;
    }
    Ok(())
}

fn openai_accumulate_tool_call_delta(
    state: &mut OpenAiStreamState,
    function: &serde_json::Map<String, Value>,
    id: Option<&str>,
) {
    if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
        if name.starts_with(&state.function_name) {
            state.function_name = name.to_string();
        } else {
            state.function_name.push_str(name);
        }
    }
    if let Some(arguments) = function.get("arguments").and_then(|v| v.as_str()) {
        state.function_arguments.push_str(arguments);
    }
    if let Some(id) = id {
        state.function_id = id.to_string();
    }
}

fn openai_handle_tool_call_delta(
    state: &mut OpenAiStreamState,
    handler: &mut SseHandler,
    data: &Value,
) -> Result<()> {
    let delta = &data["choices"][0]["delta"];
    let (Some(function), index, id) = (
        delta["tool_calls"][0]["function"].as_object(),
        delta["tool_calls"][0]["index"].as_u64(),
        delta["tool_calls"][0]["id"]
            .as_str()
            .filter(|v| !v.is_empty()),
    ) else {
        return Ok(());
    };
    openai_transition_reasoning(state, handler, false)?;
    let maybe_call_id = format!("{}/{}", id.unwrap_or_default(), index.unwrap_or_default());
    if maybe_call_id != state.call_id && maybe_call_id.len() >= state.call_id.len() {
        openai_emit_pending_tool_call(state, handler)?;
        state.call_id = maybe_call_id;
    }
    openai_accumulate_tool_call_delta(state, function, id);
    Ok(())
}

fn openai_handle_final_usage(handler: &mut SseHandler, data: &Value) {
    // Only capture usage from the final usage-only chunk (where choices is empty/absent).
    // Some providers send partial usage in intermediate chunks which would give wrong values.
    let Some(usage) = data.get("usage") else {
        return;
    };
    let choices_empty = data["choices"].as_array().is_none_or(|c| c.is_empty());
    if !choices_empty {
        return;
    }
    handler.set_usage(
        usage["prompt_tokens"].as_u64(),
        usage["completion_tokens"].as_u64(),
        usage["prompt_tokens_details"]["cached_tokens"].as_u64(),
    );
}

fn openai_handle_stream_event(
    state: &mut OpenAiStreamState,
    handler: &mut SseHandler,
    data: &Value,
) -> Result<()> {
    openai_handle_text_delta(state, handler, data)?;
    openai_handle_tool_call_delta(state, handler, data)?;
    openai_handle_final_usage(handler, data);
    Ok(())
}

pub async fn openai_chat_completions_streaming(
    builder: RequestBuilder,
    handler: &mut SseHandler,
    _model: &Model,
) -> Result<()> {
    let mut state = OpenAiStreamState::default();
    let handle = |message: SseMmessage| -> Result<bool> {
        if handler.aborted() {
            return Ok(true);
        }
        if message.data == "[DONE]" {
            openai_emit_pending_tool_call(&mut state, handler)?;
            return Ok(true);
        }
        let data: Value = serde_json::from_str(&message.data)?;
        debug!("stream-data: {data}");
        harnx_core::llm_trace::stream_event("openai", &data);
        openai_handle_stream_event(&mut state, handler, &data)?;
        Ok(false)
    };

    sse_stream(builder, handle).await
}

pub async fn openai_embeddings(
    builder: RequestBuilder,
    _model: &Model,
) -> Result<EmbeddingsOutput> {
    let res = builder.send().await?;
    let status = res.status();
    let data: Value = res.json().await?;
    if !status.is_success() {
        catch_error(&data, status.as_u16(), None)?;
    }
    let res_body: EmbeddingsResBody =
        serde_json::from_value(data).context("Invalid embeddings data")?;
    let output = res_body.data.into_iter().map(|v| v.embedding).collect();
    Ok(output)
}

#[derive(Deserialize)]
struct EmbeddingsResBody {
    data: Vec<EmbeddingsResBodyEmbedding>,
}

#[derive(Deserialize)]
struct EmbeddingsResBodyEmbedding {
    embedding: Vec<f32>,
}

pub fn openai_build_chat_completions_body(data: ChatCompletionsData, model: &Model) -> Value {
    let ChatCompletionsData {
        messages,
        temperature,
        top_p,
        functions,
        stream,
    } = data;

    let messages_len = messages.len();
    let messages: Vec<Value> = messages
        .into_iter()
        .enumerate()
        .flat_map(|(i, message)| {
            let Message { role, content, .. } = message;
            match content {
                MessageContent::ToolCalls(MessageContentToolCalls {
                    tool_results,
                    text: _,
                    thought: _,
                    sequence,
                }) => {
                    if !sequence {
                        let tool_calls: Vec<_> = tool_results
                            .iter()
                            .map(|tool_result| {
                                json!({
                                    "id": tool_result.call.id,
                                    "type": "function",
                                    "function": {
                                        "name": tool_result.call.name,
                                        "arguments": tool_result.call.arguments.to_string(),
                                    },
                                })
                            })
                            .collect();
                        let mut messages = vec![
                            json!({ "role": MessageRole::Assistant, "tool_calls": tool_calls }),
                        ];
                        for tool_result in tool_results {
                            messages.push(json!({
                                "role": "tool",
                                "content": tool_result.output.to_string(),
                                "tool_call_id": tool_result.call.id,
                            }));
                        }
                        messages
                    } else {
                        tool_results.into_iter().flat_map(|tool_result| {
                            vec![
                                json!({
                                    "role": MessageRole::Assistant,
                                    "tool_calls": [
                                        {
                                            "id": tool_result.call.id,
                                            "type": "function",
                                            "function": {
                                                "name": tool_result.call.name,
                                                "arguments": tool_result.call.arguments.to_string(),
                                            },
                                        }
                                    ]
                                }),
                                json!({
                                    "role": "tool",
                                    "content": tool_result.output.to_string(),
                                    "tool_call_id": tool_result.call.id,
                                })
                            ]

                        }).collect()
                    }
                }
                MessageContent::Text(text) if role.is_assistant() && i != messages_len - 1 => {
                    vec![json!({ "role": role, "content": strip_think_tag(&text) }
                    )]
                }
                _ => vec![json!({ "role": role, "content": content })],
            }
        })
        .collect();

    let mut body = json!({
        "model": &model.real_name(),
        "messages": messages,
    });

    if let Some(v) = model.max_tokens_param() {
        if model
            .patch()
            .and_then(|v| v.get("body").and_then(|v| v.get("max_tokens")))
            == Some(&Value::Null)
        {
            body["max_completion_tokens"] = v.into();
        } else {
            body["max_tokens"] = v.into();
        }
    }
    if let Some(v) = temperature {
        body["temperature"] = v.into();
    }
    if let Some(v) = top_p {
        body["top_p"] = v.into();
    }
    if stream {
        body["stream"] = true.into();
        body["stream_options"] = json!({"include_usage": true});
    }
    if let Some(functions) = functions {
        body["tools"] = functions
            .iter()
            .map(|v| {
                json!({
                    "type": "function",
                    "function": v,
                })
            })
            .collect();
    }
    body
}

pub fn openai_build_embeddings_body(data: &EmbeddingsData, model: &Model) -> Value {
    json!({
        "input": data.texts,
        "model": model.real_name()
    })
}

pub fn openai_extract_chat_completions(data: &Value) -> Result<ChatCompletionsOutput> {
    let text = data["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default();

    let reasoning = data["choices"][0]["message"]["reasoning_content"]
        .as_str()
        .or_else(|| data["choices"][0]["message"]["reasoning"].as_str())
        .unwrap_or_default()
        .trim();

    let mut tool_calls = vec![];
    if let Some(calls) = data["choices"][0]["message"]["tool_calls"].as_array() {
        for call in calls {
            if let (Some(name), Some(arguments), Some(id)) = (
                call["function"]["name"].as_str(),
                call["function"]["arguments"].as_str(),
                call["id"].as_str(),
            ) {
                let arguments: Value = arguments.parse().with_context(|| {
                    format!("Tool call '{name}' have non-JSON arguments '{arguments}'")
                })?;
                tool_calls.push(ToolCall::new(
                    name.to_string(),
                    arguments,
                    Some(id.to_string()),
                    None,
                ));
            }
        }
    };

    if text.is_empty() && tool_calls.is_empty() {
        bail!("Invalid response data: {data}");
    }
    let text = if !reasoning.is_empty() {
        format!("<think>\n{reasoning}\n</think>\n\n{text}")
    } else {
        text.to_string()
    };
    let output = ChatCompletionsOutput {
        text,
        tool_calls,
        thought: None,
        id: data["id"].as_str().map(|v| v.to_string()),
        input_tokens: data["usage"]["prompt_tokens"].as_u64(),
        output_tokens: data["usage"]["completion_tokens"].as_u64(),
        cached_tokens: data["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64(),
    };
    Ok(output)
}

fn normalize_function_id(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::abort::create_abort_signal;
    use tokio::sync::mpsc::unbounded_channel;

    /// Regression guard: two tool_calls in one OpenAI streaming response
    /// must emit each call exactly once. OpenAI splits calls by a
    /// `{id}/{index}` transition marker plus a final `[DONE]` flush;
    /// the test covers both the transition emit and the final flush.
    #[test]
    fn two_tool_calls_in_one_response_do_not_double_emit() {
        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = OpenAiStreamState::default();

        let chunks = [
            json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_A",
                    "function": {"name": "Bash", "arguments": "{\"cmd"}
                }]}}]
            }),
            json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_A",
                    "function": {"name": "", "arguments": "\": \"pwd\"}"}
                }]}}]
            }),
            json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 1,
                    "id": "call_B",
                    "function": {"name": "Bash", "arguments": "{\"cmd"}
                }]}}]
            }),
            json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 1,
                    "id": "call_B",
                    "function": {"name": "", "arguments": "\": \"ls\"}"}
                }]}}]
            }),
        ];

        for chunk in &chunks {
            openai_handle_stream_event(&mut state, &mut handler, chunk)
                .expect("stream event should process");
        }
        // [DONE] flushes the still-pending final tool call.
        openai_emit_pending_tool_call(&mut state, &mut handler).expect("finalize should succeed");

        let ids: Vec<Option<&str>> = handler
            .tool_calls()
            .iter()
            .map(|c| c.id.as_deref())
            .collect();
        assert_eq!(
            ids,
            vec![Some("call_A"), Some("call_B")],
            "each tool_call must be emitted exactly once"
        );
    }
}
