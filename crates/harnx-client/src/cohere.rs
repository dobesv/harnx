use crate::openai::*;
use crate::openai_compatible::*;
use crate::*;

use anyhow::{bail, Context, Result};
use reqwest::RequestBuilder;
use serde::Deserialize;
use serde_json::{json, Value};

const API_BASE: &str = "https://api.cohere.ai/v2";

impl CohereClient {
    config_get_fn!(api_key, get_api_key);
    config_get_fn!(api_base, get_api_base);

    pub const PROMPTS: [PromptAction<'static>; 1] = [("api_key", "API Key", None)];
}

impl_client_trait!(
    CohereClient,
    (
        prepare_chat_completions,
        chat_completions,
        chat_completions_streaming
    ),
    (prepare_embeddings, embeddings),
    (prepare_rerank, generic_rerank),
);

fn prepare_chat_completions(
    self_: &CohereClient,
    data: ChatCompletionsData,
) -> Result<RequestData> {
    let api_key = self_.get_api_key()?;
    let api_base = self_
        .get_api_base()
        .unwrap_or_else(|_| API_BASE.to_string());

    let url = format!("{}/chat", api_base.trim_end_matches('/'));
    let mut body = openai_build_chat_completions_body(data, &self_.model);
    if let Some(obj) = body.as_object_mut() {
        if let Some(top_p) = obj.remove("top_p") {
            obj.insert("p".to_string(), top_p);
        }
    }

    let mut request_data = RequestData::new(url, body);

    request_data.bearer_auth(api_key);

    Ok(request_data)
}

fn prepare_embeddings(self_: &CohereClient, data: &EmbeddingsData) -> Result<RequestData> {
    let api_key = self_.get_api_key()?;
    let api_base = self_
        .get_api_base()
        .unwrap_or_else(|_| API_BASE.to_string());

    let url = format!("{}/embed", api_base.trim_end_matches('/'));

    let input_type = match data.query {
        true => "search_query",
        false => "search_document",
    };

    let body = json!({
        "model": self_.model.real_name(),
        "texts": data.texts,
        "input_type": input_type,
        "embedding_types": ["float"],
    });

    let mut request_data = RequestData::new(url, body);

    request_data.bearer_auth(api_key);

    Ok(request_data)
}

fn prepare_rerank(self_: &CohereClient, data: &RerankData) -> Result<RequestData> {
    let api_key = self_.get_api_key()?;
    let api_base = self_
        .get_api_base()
        .unwrap_or_else(|_| API_BASE.to_string());

    let url = format!("{}/rerank", api_base.trim_end_matches('/'));
    let body = generic_build_rerank_body(data, &self_.model);

    let mut request_data = RequestData::new(url, body);

    request_data.bearer_auth(api_key);

    Ok(request_data)
}

async fn chat_completions(
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
    harnx_core::llm_trace::response("cohere", &data);
    extract_chat_completions(&data)
}

/// Mutable accumulator state for the Cohere streaming parser. Extracted
/// so per-event handling is testable in isolation.
#[derive(Default)]
struct CohereStreamState {
    function_name: String,
    function_arguments: String,
    function_id: String,
}

fn cohere_handle_tool_call_start(state: &mut CohereStreamState, data: &Value) {
    let (Some(function), Some(id)) = (
        data["delta"]["message"]["tool_calls"]["function"].as_object(),
        data["delta"]["message"]["tool_calls"]["id"].as_str(),
    ) else {
        return;
    };
    if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
        state.function_name = name.to_string();
    }
    state.function_id = id.to_string();
}

fn cohere_handle_tool_call_end(
    state: &mut CohereStreamState,
    handler: &mut SseHandler,
) -> Result<()> {
    if !state.function_name.is_empty() {
        let arguments: Value = state.function_arguments.parse().with_context(|| {
            format!(
                "Tool call '{}' have non-JSON arguments '{}'",
                state.function_name, state.function_arguments
            )
        })?;
        handler.tool_call(ToolCall::new(
            state.function_name.clone(),
            arguments,
            Some(state.function_id.clone()),
            None,
        ))?;
    }
    state.function_name.clear();
    state.function_arguments.clear();
    state.function_id.clear();
    Ok(())
}

fn cohere_handle_stream_event(
    state: &mut CohereStreamState,
    handler: &mut SseHandler,
    data: &Value,
) -> Result<()> {
    let Some(typ) = data["type"].as_str() else {
        return Ok(());
    };
    match typ {
        "content-delta" => {
            if let Some(text) = data["delta"]["message"]["content"]["text"].as_str() {
                handler.text(text)?;
            }
        }
        "tool-plan-delta" => {
            if let Some(text) = data["delta"]["message"]["tool_plan"].as_str() {
                handler.text(text)?;
            }
        }
        "tool-call-start" => cohere_handle_tool_call_start(state, data),
        "tool-call-delta" => {
            if let Some(text) =
                data["delta"]["message"]["tool_calls"]["function"]["arguments"].as_str()
            {
                state.function_arguments.push_str(text);
            }
        }
        "tool-call-end" => cohere_handle_tool_call_end(state, handler)?,
        _ => {}
    }
    Ok(())
}

async fn chat_completions_streaming(
    builder: RequestBuilder,
    handler: &mut SseHandler,
    _model: &Model,
) -> Result<()> {
    let mut state = CohereStreamState::default();
    let handle = |message: SseMmessage| -> Result<bool> {
        if handler.aborted() {
            return Ok(true);
        }
        if message.data == "[DONE]" {
            return Ok(true);
        }
        let data: Value = serde_json::from_str(&message.data)?;
        debug!("stream-data: {data}");
        harnx_core::llm_trace::stream_event("cohere", &data);
        cohere_handle_stream_event(&mut state, handler, &data)?;
        Ok(false)
    };

    sse_stream(builder, handle).await
}

async fn embeddings(builder: RequestBuilder, _model: &Model) -> Result<EmbeddingsOutput> {
    let res = builder.send().await?;
    let status = res.status();
    let data: Value = res.json().await?;
    if !status.is_success() {
        catch_error(&data, status.as_u16(), None)?;
    }
    let res_body: EmbeddingsResBody =
        serde_json::from_value(data).context("Invalid embeddings data")?;
    Ok(res_body.embeddings.float)
}

#[derive(Deserialize)]
struct EmbeddingsResBody {
    embeddings: EmbeddingsResBodyEmbeddings,
}

#[derive(Deserialize)]
struct EmbeddingsResBodyEmbeddings {
    float: Vec<Vec<f32>>,
}

fn extract_chat_completions(data: &Value) -> Result<ChatCompletionsOutput> {
    let mut text = data["message"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let mut tool_calls = vec![];
    if let Some(calls) = data["message"]["tool_calls"].as_array() {
        if text.is_empty() {
            if let Some(tool_plain) = data["message"]["tool_plan"].as_str() {
                text = tool_plain.to_string();
            }
        }
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
    }

    if text.is_empty() && tool_calls.is_empty() {
        bail!("Invalid response data: {data}");
    }
    let output = ChatCompletionsOutput {
        text,
        tool_calls,
        thought: None,
        id: data["id"].as_str().map(|v| v.to_string()),
        input_tokens: data["usage"]["billed_units"]["input_tokens"].as_u64(),
        output_tokens: data["usage"]["billed_units"]["output_tokens"].as_u64(),
        cached_tokens: None,
    };
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::abort::create_abort_signal;
    use tokio::sync::mpsc::unbounded_channel;

    /// Regression guard: two sequential tool-call blocks in one Cohere
    /// streaming response must emit each exactly once.
    #[test]
    fn two_tool_calls_in_one_response_do_not_double_emit() {
        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = CohereStreamState::default();

        let events = [
            json!({
                "type": "tool-call-start",
                "delta": {"message": {"tool_calls": {
                    "id": "call_A",
                    "function": {"name": "Bash"}
                }}}
            }),
            json!({
                "type": "tool-call-delta",
                "delta": {"message": {"tool_calls": {
                    "function": {"arguments": "{\"cmd\": \"pwd\"}"}
                }}}
            }),
            json!({"type": "tool-call-end"}),
            json!({
                "type": "tool-call-start",
                "delta": {"message": {"tool_calls": {
                    "id": "call_B",
                    "function": {"name": "Bash"}
                }}}
            }),
            json!({
                "type": "tool-call-delta",
                "delta": {"message": {"tool_calls": {
                    "function": {"arguments": "{\"cmd\": \"ls\"}"}
                }}}
            }),
            json!({"type": "tool-call-end"}),
        ];

        for event in &events {
            cohere_handle_stream_event(&mut state, &mut handler, event)
                .expect("stream event should process");
        }

        let ids: Vec<Option<&str>> = handler
            .tool_calls()
            .iter()
            .map(|c| c.id.as_deref())
            .collect();
        assert_eq!(
            ids,
            vec![Some("call_A"), Some("call_B")],
            "each tool-call block must be emitted exactly once"
        );
    }
}
