use crate::*;

use harnx_core::{
    message::MessageContentPart,
    model::{normalize_cache_usage, RawCacheUsage},
    text::strip_think_tag,
    tool::ToolResult,
};

use anyhow::{bail, Context, Result};
use reqwest::RequestBuilder;
use serde::Deserialize;
use serde_json::{json, Value};

const API_BASE: &str = "https://api.openai.com/v1";


fn openai_collect_tool_result_images(tool_results: &[ToolResult]) -> Vec<Value> {
    tool_results
        .iter()
        .flat_map(|tool_result| tool_result.content.iter())
        .filter_map(|part| match part {
            MessageContentPart::ImageUrl { image_url } => Some(json!({
                "type": "image_url",
                "image_url": { "url": image_url.url },
            })),
            MessageContentPart::Text { .. } => None,
        })
        .collect()
}

fn openai_tool_message_content(tool_result: &ToolResult) -> String {
    let output = tool_result.output.to_string();
    if tool_result
        .content
        .iter()
        .any(|part| matches!(part, MessageContentPart::ImageUrl { .. }))
    {
        format!(
            "{output}\n[Tool {} returned image(s): see next message]",
            tool_result.call.name
        )
    } else {
        output
    }
}

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

    let (url, body) = match self_.model.endpoint() {
        Some("responses") => (
            format!("{}/responses", api_base.trim_end_matches('/')),
            crate::openai_responses::openai_build_responses_body(data, &self_.model),
        ),
        _ => (
            format!("{}/chat/completions", api_base.trim_end_matches('/')),
            openai_build_chat_completions_body(data, &self_.model),
        ),
    };

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
    model: &Model,
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
    match model.endpoint() {
        Some("responses") => crate::openai_responses::openai_extract_responses(&data, model),
        _ => openai_extract_chat_completions(&data, model),
    }
}

/// Mutable accumulator state for the OpenAI streaming parser. Extracted
/// so per-event handling is testable in isolation.
#[derive(Default)]
pub(crate) struct OpenAiStreamState {
    pub call_id: String,
    pub function_name: String,
    pub function_arguments: String,
    pub function_id: String,
    pub reasoning_state: i32,
}

pub(crate) fn openai_emit_pending_tool_call(
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
pub(crate) fn openai_transition_reasoning(
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

pub(crate) fn openai_handle_text_delta(
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

pub(crate) fn openai_accumulate_tool_call_delta(
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

pub(crate) fn openai_handle_tool_call_delta(
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

pub(crate) fn openai_handle_final_usage(
    handler: &mut SseHandler,
    data: &Value,
    model: &Model,
) {
    // Only capture usage from the final usage-only chunk (where choices is empty/absent).
    // Some providers send partial usage in intermediate chunks which would give wrong values.
    let Some(usage) = data.get("usage").filter(|value| value.is_object()) else {
        return;
    };
    let choices_empty = data["choices"].as_array().is_none_or(|c| c.is_empty());
    if !choices_empty {
        return;
    }
    let usage = openai_normalize_chat_usage(usage, model);
    handler.set_usage(StreamingUsage {
        input_tokens: Some(usage.input_tokens),
        output_tokens: Some(usage.output_tokens),
        cached_tokens: Some(usage.cached_tokens),
        cache_write_tokens: Some(usage.cache_write_tokens),
    });
}

pub(crate) fn openai_handle_stream_event(
    state: &mut OpenAiStreamState,
    handler: &mut SseHandler,
    data: &Value,
    model: &Model,
) -> Result<()> {
    if let Some(_err) = data["error"].as_object() {
        return crate::catch_error(data, 500, None);
    }
    openai_handle_text_delta(state, handler, data)?;
    openai_handle_tool_call_delta(state, handler, data)?;
    openai_handle_final_usage(handler, data, model);
    Ok(())
}

pub async fn openai_chat_completions_streaming(
    builder: RequestBuilder,
    handler: &mut SseHandler,
    model: &Model,
) -> Result<()> {
    if let Some("responses") = model.endpoint() {
        return crate::openai_responses::openai_responses_streaming(builder, handler, model).await;
    }

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
        openai_handle_stream_event(&mut state, handler, &data, model)?;
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
        attachments_dir: _,  // OpenAI uses the runtime base64 pre-pass
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
                    let trailing_image_parts = openai_collect_tool_result_images(&tool_results);
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
                        for tool_result in &tool_results {
                            messages.push(json!({
                                "role": "tool",
                                "content": openai_tool_message_content(tool_result),
                                "tool_call_id": tool_result.call.id,
                            }));
                        }
                        if !trailing_image_parts.is_empty() {
                            messages.push(json!({
                                "role": MessageRole::User,
                                "content": trailing_image_parts,
                            }));
                        }
                        messages
                    } else {
                        let mut messages: Vec<_> = tool_results
                            .into_iter()
                            .flat_map(|tool_result| {
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
                                        "content": openai_tool_message_content(&tool_result),
                                        "tool_call_id": tool_result.call.id,
                                    })
                                ]
                            })
                            .collect();
                        if !trailing_image_parts.is_empty() {
                            messages.push(json!({
                                "role": MessageRole::User,
                                "content": trailing_image_parts,
                            }));
                        }
                        messages
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
        // Check if patches specify max_tokens: null (should use max_completion_tokens instead)
        let patches_have_null_max_tokens = model
            .patches()
            .map(|p| {
                p.iter().any(|expr| {
                    // jq patches use field-path syntax: `.body.max_tokens = null`
                    // (not quoted JSON key form like `"max_tokens"`)
                    expr.contains(".max_tokens") && expr.contains("null")
                })
            })
            .unwrap_or(false);
        if patches_have_null_max_tokens {
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

/// Normalizes chat/completions usage to the OTel-subset convention.
///
/// The OpenAI chat/completions API is an OpenAI-family endpoint and uses
/// `normalize_cache_usage` with the per-model `cache_accounting` flag. This handles
/// proxies like LiteLLM/agentgateway that may front a disjoint backend (Anthropic/Bedrock)
/// while exposing an OpenAI-compatible wire format. The default `subset` is correct
/// for direct OpenAI and well-behaved proxies that normalize themselves.
fn openai_normalize_chat_usage(
    usage: &Value,
    model: &Model,
) -> harnx_core::api_types::CompletionTokenUsage {
    let details = &usage["prompt_tokens_details"];
    let read = details["cached_tokens"]
        .as_u64()
        .or_else(|| usage["cache_read_input_tokens"].as_u64())
        .unwrap_or_default();
    let write = details["cache_write_tokens"]
        .as_u64()
        .or_else(|| details["cache_creation_tokens"].as_u64())
        .or_else(|| usage["cache_creation_input_tokens"].as_u64())
        .unwrap_or_default();
    normalize_cache_usage(
        RawCacheUsage {
            input: usage["prompt_tokens"].as_u64().unwrap_or_default(),
            output: usage["completion_tokens"].as_u64().unwrap_or_default(),
            read,
            write,
        },
        model.cache_accounting(),
        model.name(),
    )
}

pub fn openai_extract_chat_completions(
    data: &Value,
    model: &Model,
) -> Result<ChatCompletionsOutput> {
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
    let usage = data
        .get("usage")
        .filter(|value| value.is_object())
        .map(|usage| openai_normalize_chat_usage(usage, model));
    let output = ChatCompletionsOutput {
        text,
        tool_calls,
        thought: None,
        id: data["id"].as_str().map(|v| v.to_string()),
        input_tokens: usage.as_ref().map(|usage| usage.input_tokens),
        output_tokens: usage.as_ref().map(|usage| usage.output_tokens),
        cached_tokens: usage.as_ref().map(|usage| usage.cached_tokens),
        cache_write_tokens: usage.as_ref().map(|usage| usage.cache_write_tokens),
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

    use harnx_core::{
        message::{ImageUrl, Message, MessageContent, MessageContentPart, MessageContentToolCalls, MessageRole},
        model::CacheAccounting,
        tool::{ToolCall, ToolResult},
    };

    fn chat_response(usage: Value) -> Value {
        json!({
            "id": "chatcmpl_test",
            "choices": [{"message": {"content": "Hello"}}],
            "usage": usage,
        })
    }

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

    fn assert_usage(actual: &CompletionTokenUsage, expected: CompletionTokenUsage) {
        assert_eq!(actual, &expected);
        assert!(actual.input_tokens >= actual.cached_tokens + actual.cache_write_tokens);
    }

    #[test]
    fn openai_chat_without_usage_leaves_token_fields_absent() {
        let model = Model::new("openai", "gpt-test");
        let response = json!({
            "id": "chatcmpl_test",
            "choices": [{"message": {"content": "Hello"}}],
        });

        let output = openai_extract_chat_completions(&response, &model)
            .expect("usage-less response should parse");

        assert_eq!(output.input_tokens, None);
        assert_eq!(output.output_tokens, None);
        assert_eq!(output.cached_tokens, None);
        assert_eq!(output.cache_write_tokens, None);
    }

    #[test]
    fn openai_chat_null_final_usage_does_not_replace_stream_usage() {
        let model = Model::new("openai", "gpt-test");
        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        handler.set_usage(StreamingUsage {
            input_tokens: Some(11),
            output_tokens: Some(7),
            cached_tokens: Some(3),
            cache_write_tokens: Some(2),
        });

        openai_handle_final_usage(
            &mut handler,
            &json!({"choices": [], "usage": null}),
            &model,
        );

        let (_, _, _, usage) = handler.take();
        assert_usage(
            &usage,
            CompletionTokenUsage {
                input_tokens: 11,
                output_tokens: 7,
                cached_tokens: 3,
                cache_write_tokens: 2,
            },
        );
    }

    #[test]
    fn openai_chat_parses_subset_cache_write_on_both_paths() {
        let model = Model::new("openai", "gpt-test");
        let usage = json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": {
                "cached_tokens": 30,
                "cache_write_tokens": 10
            }
        });
        let output = openai_extract_chat_completions(&chat_response(usage.clone()), &model)
            .expect("non-streaming usage should parse");
        let expected = CompletionTokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            cached_tokens: 30,
            cache_write_tokens: 10,
        };
        assert_output_usage(&output, &expected);

        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = OpenAiStreamState::default();
        let event = json!({"choices": [], "usage": usage});
        openai_handle_stream_event(&mut state, &mut handler, &event, &model)
            .expect("final usage event should parse");
        let (_, _, _, usage) = handler.take();
        assert_usage(&usage, expected);
    }

    #[test]
    fn openai_chat_parses_litellm_aliases() {
        let model = Model::new("openai", "proxy-model");
        let response = chat_response(json!({
            "prompt_tokens": 120,
            "completion_tokens": 5,
            "cache_read_input_tokens": 40,
            "prompt_tokens_details": {"cache_creation_tokens": 15}
        }));
        let output = openai_extract_chat_completions(&response, &model)
            .expect("LiteLLM aliases should parse");
        assert_output_usage(
            &output,
            &CompletionTokenUsage {
                input_tokens: 120,
                output_tokens: 5,
                cached_tokens: 40,
                cache_write_tokens: 15,
            },
        );
    }

    #[test]
    fn openai_chat_cache_accounting_controls_disjoint_normalization() {
        let response = chat_response(json!({
            "prompt_tokens": 60,
            "completion_tokens": 5,
            "cache_read_input_tokens": 20,
            "cache_creation_input_tokens": 10
        }));
        let subset = Model::new("openai", "subset-proxy");
        let subset_output = openai_extract_chat_completions(&response, &subset)
            .expect("subset usage should parse");
        assert_eq!(subset_output.input_tokens, Some(60));

        let mut disjoint = Model::new("openai", "disjoint-proxy");
        disjoint.data_mut().cache_accounting = Some(CacheAccounting::Disjoint);
        let disjoint_output = openai_extract_chat_completions(&response, &disjoint)
            .expect("disjoint usage should parse");
        assert_output_usage(
            &disjoint_output,
            &CompletionTokenUsage {
                input_tokens: 90,
                output_tokens: 5,
                cached_tokens: 20,
                cache_write_tokens: 10,
            },
        );
    }

    fn test_tool_result(id: &str, name: &str, output: Value) -> ToolResult {
        ToolResult::new(
            ToolCall::new(name.to_string(), json!({"arg": id}), Some(id.to_string()), None),
            output,
        )
    }

    fn image_part(url: &str) -> MessageContentPart {
        MessageContentPart::ImageUrl {
            image_url: ImageUrl {
                url: url.to_string(),
            },
        }
    }

    fn build_tool_calls_body(tool_results: Vec<ToolResult>, sequence: bool) -> Value {
        let mut tool_calls = MessageContentToolCalls::new(tool_results, String::new(), None);
        tool_calls.sequence = sequence;
        let data = ChatCompletionsData {
            messages: vec![Message {
                role: MessageRole::Assistant,
                content: MessageContent::ToolCalls(tool_calls),
                ..Default::default()
            }],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };
        let model = harnx_core::model::Model::new("openai", "gpt-4o");
        openai_build_chat_completions_body(data, &model)
    }

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
            openai_handle_stream_event(
                &mut state,
                &mut handler,
                chunk,
                &Model::new("openai", "test-model"),
            )
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

    fn make_model_with_patches(patches: Vec<&str>) -> harnx_core::model::Model {
        let mut model = harnx_core::model::Model::new("openai", "gpt-4o");
        model.set_max_tokens(Some(1024), true);
        model.data_mut().patches = Some(patches.into_iter().map(String::from).collect());
        model
    }


    #[test]
    fn tool_calls_with_images_emit_single_trailing_user_message_in_normal_mode() {
        let mut first = test_tool_result("call_1", "read_file", json!({"ok": true, "idx": 1}));
        first.content.push(image_part("data:image/png;base64,AAAA"));
        let mut second = test_tool_result("call_2", "inspect_file", json!({"ok": true, "idx": 2}));
        second.content.push(image_part("data:image/jpeg;base64,BBBB"));

        let body = build_tool_calls_body(vec![first.clone(), second.clone()], false);
        let messages = body["messages"].as_array().expect("messages array");

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], json!("assistant"));
        assert_eq!(messages[0]["tool_calls"][0]["id"], json!("call_1"));
        assert_eq!(messages[0]["tool_calls"][1]["id"], json!("call_2"));

        assert_eq!(messages[1]["role"], json!("tool"));
        assert_eq!(messages[1]["tool_call_id"], json!("call_1"));
        assert_eq!(
            messages[1]["content"],
            json!(format!(
                "{}\n[Tool {} returned image(s): see next message]",
                first.output, first.call.name
            ))
        );

        assert_eq!(messages[2]["role"], json!("tool"));
        assert_eq!(messages[2]["tool_call_id"], json!("call_2"));
        assert_eq!(
            messages[2]["content"],
            json!(format!(
                "{}\n[Tool {} returned image(s): see next message]",
                second.output, second.call.name
            ))
        );

        assert_eq!(messages[3]["role"], json!("user"));
        assert_eq!(
            messages[3]["content"],
            json!([
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,BBBB"}},
            ])
        );
    }

    #[test]
    fn tool_calls_with_single_image_emit_trailing_user_message_in_sequence_mode() {
        let mut first = test_tool_result("call_1", "read_file", json!("preview"));
        first.content.push(image_part("data:image/png;base64,AAAA"));
        let second = test_tool_result("call_2", "list_dir", json!("no image"));

        let body = build_tool_calls_body(vec![first.clone(), second.clone()], true);
        let messages = body["messages"].as_array().expect("messages array");

        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["role"], json!("assistant"));
        assert_eq!(messages[0]["tool_calls"][0]["id"], json!("call_1"));
        assert_eq!(messages[1]["role"], json!("tool"));
        assert_eq!(messages[1]["tool_call_id"], json!("call_1"));
        assert_eq!(
            messages[1]["content"],
            json!(format!(
                "{}\n[Tool {} returned image(s): see next message]",
                first.output, first.call.name
            ))
        );
        assert_eq!(messages[2]["role"], json!("assistant"));
        assert_eq!(messages[2]["tool_calls"][0]["id"], json!("call_2"));
        assert_eq!(messages[3]["role"], json!("tool"));
        assert_eq!(messages[3]["tool_call_id"], json!("call_2"));
        assert_eq!(messages[3]["content"], json!(second.output.to_string()));
        assert_eq!(messages[4]["role"], json!("user"));
        assert_eq!(
            messages[4]["content"],
            json!([
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
            ])
        );
    }

    #[test]
    fn tool_calls_without_images_match_legacy_output() {
        let first = test_tool_result("call_1", "read_file", json!("first"));
        let second = test_tool_result("call_2", "list_dir", json!({"ok": true}));

        let normal_body = build_tool_calls_body(vec![first.clone(), second.clone()], false);
        assert_eq!(
            normal_body["messages"],
            json!([
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "read_file", "arguments": "{\"arg\":\"call_1\"}"}
                        },
                        {
                            "id": "call_2",
                            "type": "function",
                            "function": {"name": "list_dir", "arguments": "{\"arg\":\"call_2\"}"}
                        }
                    ]
                },
                {"role": "tool", "content": "\"first\"", "tool_call_id": "call_1"},
                {"role": "tool", "content": "{\"ok\":true}", "tool_call_id": "call_2"}
            ])
        );

        let sequence_body = build_tool_calls_body(vec![first, second], true);
        assert_eq!(
            sequence_body["messages"],
            json!([
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "read_file", "arguments": "{\"arg\":\"call_1\"}"}
                        }
                    ]
                },
                {"role": "tool", "content": "\"first\"", "tool_call_id": "call_1"},
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call_2",
                            "type": "function",
                            "function": {"name": "list_dir", "arguments": "{\"arg\":\"call_2\"}"}
                        }
                    ]
                },
                {"role": "tool", "content": "{\"ok\":true}", "tool_call_id": "call_2"}
            ])
        );
    }

    #[test]
    fn null_max_tokens_patch_uses_max_completion_tokens_key() {
        let model = make_model_with_patches(vec![
            "del(.body.max_tokens) | .body.max_completion_tokens = null",
            ".body.max_tokens = null",
        ]);
        let data = ChatCompletionsData {
            messages: vec![],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };
        let body = openai_build_chat_completions_body(data, &model);
        assert!(
            body.get("max_completion_tokens").is_some(),
            "should use max_completion_tokens when patch nulls max_tokens"
        );
        assert!(
            body.get("max_tokens").is_none(),
            "should not set max_tokens when patch nulls it"
        );
    }

    #[test]
    fn no_null_max_tokens_patch_uses_max_tokens_key() {
        let model = make_model_with_patches(vec![".body.temperature = 0"]);
        let data = ChatCompletionsData {
            messages: vec![],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };
        let body = openai_build_chat_completions_body(data, &model);
        assert!(
            body.get("max_tokens").is_some(),
            "should use max_tokens when no patch nulls it"
        );
        assert!(
            body.get("max_completion_tokens").is_none(),
            "should not set max_completion_tokens without a null-max_tokens patch"
        );
    }

    #[test]
    fn openai_remains_base64_only_for_images() {
        let client = OpenAIClient {
            config: harnx_core::provider_config::openai::OpenAIConfig {
                name: "test-openai".into(),
                api_key: Some("test-key".into()),
                ..Default::default()
            },
            model: harnx_core::model::Model::new("openai", "gpt-4o"),
        };
        assert!(
            !client.expands_attachments_internally(),
            "OpenAIClient.expands_attachments_internally() must remain false"
        );

        let data_url = "data:image/png;base64,QUJDRA==".to_string();
        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Array(vec![MessageContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: data_url.clone(),
                    },
                }]),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
            attachments_dir: None,
        };

        let request = prepare_chat_completions(&client, data).expect("request should build");
        assert!(
            request.url.ends_with("/chat/completions"),
            "OpenAI request should target chat/completions"
        );
        assert_eq!(
            request.body["messages"],
            json!([
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "image_url",
                            "image_url": { "url": data_url }
                        }
                    ]
                }
            ])
        );
    }

    #[test]
    fn openai_streaming_error_event_fails() {
        use harnx_core::abort::create_abort_signal;
        use tokio::sync::mpsc::unbounded_channel;
        use harnx_core::error::LlmError;

        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = OpenAiStreamState::default();

        let event = json!({
            "error": {
                "message": "Rate limit reached",
                "type": "requests",
                "param": null,
                "code": "rate_limit_exceeded"
            }
        });

        let result = openai_handle_stream_event(
            &mut state,
            &mut handler,
            &event,
            &Model::new("openai", "test-model"),
        );
        assert!(result.is_err(), "Stream error event should return an error");
        let err = result.unwrap_err();
        let llm_err = err.downcast_ref::<LlmError>().expect("Should be an LlmError");
        assert_eq!(llm_err.status, 500); // We map internal stream errors to 500
        assert!(llm_err.is_retryable());
        assert!(llm_err.message.contains("Rate limit reached"));
    }
}
