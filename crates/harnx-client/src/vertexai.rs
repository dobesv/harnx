use crate::access_token::*;
use crate::claude::*;
use crate::openai::*;
use crate::*;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Duration, Utc};
use reqwest::{Client as ReqwestClient, RequestBuilder};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::PathBuf, str::FromStr};

impl VertexAIClient {
    config_get_fn!(project_id, get_project_id);
    config_get_fn!(location, get_location);

    pub const PROMPTS: [PromptAction<'static>; 2] = [
        ("project_id", "Project ID", None),
        ("location", "Location", None),
    ];
}

#[async_trait::async_trait]
impl Client for VertexAIClient {
    client_common_fns!();

    async fn chat_completions_inner(
        &self,
        client: &ReqwestClient,
        data: ChatCompletionsData,
    ) -> Result<ChatCompletionsOutput> {
        prepare_gcloud_access_token(client, self.name(), &self.config.adc_file).await?;
        let model = self.model();
        let model_category = ModelCategory::from_str(model.real_name())?;
        let request_data = prepare_chat_completions(self, data, &model_category)?;
        let builder = self.request_builder(client, request_data)?;
        match model_category {
            ModelCategory::Gemini => gemini_chat_completions(builder, model).await,
            ModelCategory::Claude => claude_chat_completions(builder, model).await,
            ModelCategory::Mistral => openai_chat_completions(builder, model).await,
        }
    }

    async fn chat_completions_streaming_inner(
        &self,
        client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()> {
        prepare_gcloud_access_token(client, self.name(), &self.config.adc_file).await?;
        let model = self.model();
        let model_category = ModelCategory::from_str(model.real_name())?;
        let request_data = prepare_chat_completions(self, data, &model_category)?;
        let builder = self.request_builder(client, request_data)?;
        match model_category {
            ModelCategory::Gemini => {
                gemini_chat_completions_streaming(builder, handler, model).await
            }
            ModelCategory::Claude => {
                claude_chat_completions_streaming(builder, handler, model).await
            }
            ModelCategory::Mistral => {
                openai_chat_completions_streaming(builder, handler, model).await
            }
        }
    }

    async fn embeddings_inner(
        &self,
        client: &ReqwestClient,
        data: &EmbeddingsData,
    ) -> Result<Vec<Vec<f32>>> {
        prepare_gcloud_access_token(client, self.name(), &self.config.adc_file).await?;
        let request_data = prepare_embeddings(self, data)?;
        let builder = self.request_builder(client, request_data)?;
        embeddings(builder, self.model()).await
    }
}

fn prepare_chat_completions(
    self_: &VertexAIClient,
    data: ChatCompletionsData,
    model_category: &ModelCategory,
) -> Result<RequestData> {
    let project_id = self_.get_project_id()?;
    let location = self_.get_location()?;
    let access_token = get_access_token(self_.name())?;

    let base_url = if location == "global" {
        format!("https://aiplatform.googleapis.com/v1/projects/{project_id}/locations/global/publishers")
    } else {
        format!("https://{location}-aiplatform.googleapis.com/v1/projects/{project_id}/locations/{location}/publishers")
    };

    let model_name = self_.model.real_name();

    let url = match model_category {
        ModelCategory::Gemini => {
            let func = match data.stream {
                true => "streamGenerateContent",
                false => "generateContent",
            };
            format!("{base_url}/google/models/{model_name}:{func}")
        }
        ModelCategory::Claude => {
            format!("{base_url}/anthropic/models/{model_name}:streamRawPredict")
        }
        ModelCategory::Mistral => {
            let func = match data.stream {
                true => "streamRawPredict",
                false => "rawPredict",
            };
            format!("{base_url}/mistralai/models/{model_name}:{func}")
        }
    };

    let body = match model_category {
        ModelCategory::Gemini => gemini_build_chat_completions_body(data, &self_.model)?,
        ModelCategory::Claude => {
            let mut body = claude_build_chat_completions_body(data, &self_.model)?;
            if let Some(body_obj) = body.as_object_mut() {
                body_obj.remove("model");
            }
            body["anthropic_version"] = "vertex-2023-10-16".into();
            body
        }
        ModelCategory::Mistral => {
            let mut body = openai_build_chat_completions_body(data, &self_.model);
            if let Some(body_obj) = body.as_object_mut() {
                body_obj["model"] = strip_model_version(self_.model.real_name()).into();
            }
            body
        }
    };

    let mut request_data = RequestData::new(url, body);

    request_data.bearer_auth(access_token);

    Ok(request_data)
}

fn prepare_embeddings(self_: &VertexAIClient, data: &EmbeddingsData) -> Result<RequestData> {
    let project_id = self_.get_project_id()?;
    let location = self_.get_location()?;
    let access_token = get_access_token(self_.name())?;

    let base_url = if location == "global" {
        format!("https://aiplatform.googleapis.com/v1/projects/{project_id}/locations/global/publishers")
    } else {
        format!("https://{location}-aiplatform.googleapis.com/v1/projects/{project_id}/locations/{location}/publishers")
    };
    let url = format!(
        "{base_url}/google/models/{}:predict",
        self_.model.real_name()
    );

    let instances: Vec<_> = data.texts.iter().map(|v| json!({"content": v})).collect();

    let body = json!({
        "instances": instances,
    });

    let mut request_data = RequestData::new(url, body);

    request_data.bearer_auth(access_token);

    Ok(request_data)
}

pub async fn gemini_chat_completions(
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
    harnx_core::llm_trace::response("vertexai", &data);
    gemini_extract_chat_completions_text(&data)
}

fn gemini_handle_part(handler: &mut SseHandler, part: &Value, index: usize) -> Result<()> {
    if let Some(text) = part["text"].as_str() {
        if index > 0 {
            handler.text("\n\n")?;
        }
        handler.text(text)?;
    } else if let Some(thought) = part["thought"].as_str() {
        handler.thought(thought)?;
    } else if let (Some(name), Some(args)) = (
        part["functionCall"]["name"].as_str(),
        part["functionCall"]["args"].as_object(),
    ) {
        let thought_signature = part["thoughtSignature"]
            .as_str()
            .or_else(|| part["thought_signature"].as_str())
            .map(|v| v.to_string());
        handler.tool_call(ToolCall::new(
            name.to_string(),
            json!(args),
            None,
            thought_signature,
        ))?;
    }
    Ok(())
}

/// Process one parsed chunk from a Gemini streaming response. Extracted
/// so per-chunk handling is testable in isolation. Unlike Claude/Bedrock
/// /OpenAI, Gemini has no accumulator state — each chunk carries complete
/// parts — so no state struct is needed.
fn gemini_handle_stream_chunk(handler: &mut SseHandler, data: &Value) -> Result<()> {
    if let Some(parts) = data["candidates"][0]["content"]["parts"].as_array() {
        for (i, part) in parts.iter().enumerate() {
            gemini_handle_part(handler, part, i)?;
        }
    } else if let Some("SAFETY") = data["promptFeedback"]["blockReason"]
        .as_str()
        .or_else(|| data["candidates"][0]["finishReason"].as_str())
    {
        bail!("Blocked due to safety")
    }
    handler.set_usage(
        data["usageMetadata"]["promptTokenCount"].as_u64(),
        data["usageMetadata"]["candidatesTokenCount"].as_u64(),
        data["usageMetadata"]["cachedContentTokenCount"].as_u64(),
    );

    Ok(())
}

pub async fn gemini_chat_completions_streaming(
    builder: RequestBuilder,
    handler: &mut SseHandler,
    _model: &Model,
) -> Result<()> {
    let res = builder.send().await?;
    let status = res.status();
    if !status.is_success() {
        let retry_after = parse_retry_after(res.headers());
        let data: Value = res.json().await?;
        catch_error(&data, status.as_u16(), retry_after)?;
    } else {
        let handle = |value: &str| -> Result<bool> {
            if handler.aborted() {
                return Ok(true);
            }
            let data: Value = serde_json::from_str(value)?;
            debug!("stream-data: {data}");
            harnx_core::llm_trace::stream_event("vertexai", &data);
            gemini_handle_stream_chunk(handler, &data)?;
            Ok(false)
        };
        json_stream(res.bytes_stream(), handle).await?;
    }
    Ok(())
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
    let output = res_body
        .predictions
        .into_iter()
        .map(|v| v.embeddings.values)
        .collect();
    Ok(output)
}

#[derive(Deserialize)]
struct EmbeddingsResBody {
    predictions: Vec<EmbeddingsResBodyPrediction>,
}

#[derive(Deserialize)]
struct EmbeddingsResBodyPrediction {
    embeddings: EmbeddingsResBodyPredictionEmbeddings,
}

#[derive(Deserialize)]
struct EmbeddingsResBodyPredictionEmbeddings {
    values: Vec<f32>,
}

fn gemini_extract_chat_completions_text(data: &Value) -> Result<ChatCompletionsOutput> {
    let mut text_parts = vec![];
    let mut thought_parts = vec![];
    let mut tool_calls = vec![];
    if let Some(parts) = data["candidates"][0]["content"]["parts"].as_array() {
        for part in parts {
            if let Some(text) = part["text"].as_str() {
                text_parts.push(text);
            }
            if let Some(thought) = part["thought"].as_str() {
                thought_parts.push(thought);
            }
            if let (Some(name), Some(args)) = (
                part["functionCall"]["name"].as_str(),
                part["functionCall"]["args"].as_object(),
            ) {
                let thought_signature = part["thoughtSignature"]
                    .as_str()
                    .or_else(|| part["thought_signature"].as_str())
                    .map(|v| v.to_string());
                tool_calls.push(ToolCall::new(
                    name.to_string(),
                    json!(args),
                    None,
                    thought_signature,
                ));
            }
        }
    }

    let text = text_parts.join("\n\n");
    let thought = if thought_parts.is_empty() {
        None
    } else {
        Some(thought_parts.join("\n\n"))
    };
    if text.is_empty() && tool_calls.is_empty() && thought.is_none() {
        if let Some("SAFETY") = data["promptFeedback"]["blockReason"]
            .as_str()
            .or_else(|| data["candidates"][0]["finishReason"].as_str())
        {
            bail!("Blocked due to safety")
        } else {
            bail!("Invalid response data: {data}");
        }
    }
    let output = ChatCompletionsOutput {
        text,
        tool_calls,
        thought,
        id: None,
        input_tokens: data["usageMetadata"]["promptTokenCount"].as_u64(),
        output_tokens: data["usageMetadata"]["candidatesTokenCount"].as_u64(),
        cached_tokens: data["usageMetadata"]["cachedContentTokenCount"].as_u64(),
    };
    Ok(output)
}

pub fn gemini_build_chat_completions_body(
    data: ChatCompletionsData,
    model: &Model,
) -> Result<Value> {
    let ChatCompletionsData {
        mut messages,
        temperature,
        top_p,
        functions,
        stream: _,
    } = data;

    let system_message = extract_system_message(&mut messages);

    let mut network_image_urls = vec![];
    let contents: Vec<Value> = messages
        .into_iter()
        .flat_map(|message| {
            let Message { role, content } = message;
            let role = match role {
                MessageRole::User => "user",
                _ => "model",
            };
               match content {
                    MessageContent::Text(text) => vec![json!({
                        "role": role,
                        "parts": [{ "text": text }]
                    })],
                    MessageContent::Array(list) => {
                        let parts: Vec<Value> = list
                            .into_iter()
                            .map(|item| match item {
                                MessageContentPart::Text { text } => json!({"text": text}),
                                MessageContentPart::ImageUrl { image_url: ImageUrl { url } } => {
                                    if let Some((mime_type, data)) = url.strip_prefix("data:").and_then(|v| v.split_once(";base64,")) {
                                        json!({ "inline_data": { "mime_type": mime_type, "data": data } })
                                    } else {
                                        network_image_urls.push(url.clone());
                                        json!({ "url": url })
                                    }
                                },
                            })
                            .collect();
                        vec![json!({ "role": role, "parts": parts })]
                    },
                    MessageContent::ToolCalls(MessageContentToolCalls {
                        tool_results,
                        text,
                        thought,
                        ..
                    }) => {
                        let mut model_parts = vec![];
                        if let Some(thought) = thought {
                            model_parts.push(json!({ "thought": thought }));
                        }
                        if !text.is_empty() {
                            model_parts.push(json!({ "text": text }));
                        }
                        for tool_result in tool_results.iter() {
                            let call_obj = json!({
                                "name": tool_result.call.name,
                                "args": tool_result.call.arguments,
                            });
                            let mut part_obj = json!({ "functionCall": call_obj });
                            if let Some(signature) = &tool_result.call.thought_signature {
                                if let Some(obj) = part_obj.as_object_mut() {
                                    obj.insert(
                                        "thoughtSignature".to_string(),
                                        signature.clone().into(),
                                    );
                                }
                            }
                            model_parts.push(part_obj);
                        }
                        let function_parts: Vec<Value> = tool_results.into_iter().map(|tool_result| {
                            json!({
                                "functionResponse": {
                                    "name": tool_result.call.name,
                                    "response": {
                                        "name": tool_result.call.name,
                                        "content": tool_result.output,
                                    }
                                }
                            })
                        }).collect();
                        vec![
                            json!({ "role": "model", "parts": model_parts }),
                            json!({ "role": "function", "parts": function_parts }),
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

    let mut body = json!({ "contents": contents, "generationConfig": {} }); 

    if let Some(parts) = system_message {
        let gemini_parts: Vec<Value> = parts
            .iter()
            .map(|text| json!({"text": text}))
            .collect();
        body["systemInstruction"] = json!({"parts": gemini_parts});
    }

    if let Some(v) = model.max_tokens_param() {
        body["generationConfig"]["maxOutputTokens"] = v.into();
    }
    if let Some(v) = temperature {
        body["generationConfig"]["temperature"] = v.into();
    }
    if let Some(v) = top_p {
        body["generationConfig"]["topP"] = v.into();
    }

    if let Some(functions) = functions {
        // Gemini doesn't support functions with parameters that have empty properties, so we need to patch it.
        // It also doesn't support `anyOf`, so we flatten nullable wrappers (e.g. Option<Vec<String>>).
        let function_declarations: Vec<_> = functions
            .into_iter()
            .map(|function| {
                if function.parameters.is_empty_properties() {
                    json!({
                        "name": function.name,
                        "description": function.description,
                    })
                } else {
                    let mut func = function;
                    func.parameters = func.parameters.flatten_any_of();
                    json!(func)
                }
            })
            .collect();
        body["tools"] = json!([{ "functionDeclarations": function_declarations }]);
    }

    Ok(body)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelCategory {
    Gemini,
    Claude,
    Mistral,
}

impl FromStr for ModelCategory {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.starts_with("gemini") {
            Ok(ModelCategory::Gemini)
        } else if s.starts_with("claude") {
            Ok(ModelCategory::Claude)
        } else if s.starts_with("mistral") || s.starts_with("codestral") {
            Ok(ModelCategory::Mistral)
        } else {
            unsupported_model!(s)
        }
    }
}

pub async fn prepare_gcloud_access_token(
    client: &reqwest::Client,
    client_name: &str,
    adc_file: &Option<String>,
) -> Result<()> {
    if !is_valid_access_token(client_name) {
        let (token, expires_in) = fetch_access_token(client, adc_file)
            .await
            .with_context(|| "Failed to fetch access token")?;
        let expires_at = Utc::now()
            + Duration::try_seconds(expires_in)
                .ok_or_else(|| anyhow!("Failed to parse expires_in of access_token"))?;
        set_access_token(client_name, token, expires_at.timestamp())
    }
    Ok(())
}

async fn fetch_access_token(
    client: &reqwest::Client,
    file: &Option<String>,
) -> Result<(String, i64)> {
    let credentials = load_adc(file).await?;
    let value: Value = client
        .post("https://oauth2.googleapis.com/token")
        .json(&credentials)
        .send()
        .await?
        .json()
        .await?;

    if let (Some(access_token), Some(expires_in)) =
        (value["access_token"].as_str(), value["expires_in"].as_i64())
    {
        Ok((access_token.to_string(), expires_in))
    } else if let Some(err_msg) = value["error_description"].as_str() {
        bail!("{err_msg}")
    } else {
        bail!("Invalid response data: {value}")
    }
}

async fn load_adc(file: &Option<String>) -> Result<Value> {
    let adc_file = file
        .as_ref()
        .map(PathBuf::from)
        .or_else(default_adc_file)
        .ok_or_else(|| anyhow!("No application_default_credentials.json"))?;
    let data = tokio::fs::read_to_string(adc_file).await?;
    let data: Value = serde_json::from_str(&data)?;
    if let (Some(client_id), Some(client_secret), Some(refresh_token)) = (
        data["client_id"].as_str(),
        data["client_secret"].as_str(),
        data["refresh_token"].as_str(),
    ) {
        Ok(json!({
            "client_id": client_id,
            "client_secret": client_secret,
            "refresh_token": refresh_token,
            "grant_type": "refresh_token",
        }))
    } else {
        bail!("Invalid application_default_credentials.json")
    }
}

#[cfg(not(windows))]
fn default_adc_file() -> Option<PathBuf> {
    let mut path = dirs::home_dir()?;
    path.push(".config");
    path.push("gcloud");
    path.push("application_default_credentials.json");
    Some(path)
}

#[cfg(windows)]
fn default_adc_file() -> Option<PathBuf> {
    let mut path = dirs::config_dir()?;
    path.push("gcloud");
    path.push("application_default_credentials.json");
    Some(path)
}

fn strip_model_version(name: &str) -> &str {
    match name.split_once('@') {
        Some((v, _)) => v,
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::abort::create_abort_signal;
    use tokio::sync::mpsc::unbounded_channel;

    /// Regression guard: two functionCalls in one Gemini streaming
    /// response (arriving as two separate chunks) must emit each
    /// exactly once. Unlike Claude/Bedrock, Gemini chunks are
    /// self-contained so there's no accumulator to leak between
    /// calls — this test locks that invariant in.
    #[test]
    fn two_function_calls_in_one_response_do_not_double_emit() {
        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());

        let chunks = [
            json!({
                "candidates": [{"content": {"parts": [{
                    "functionCall": {"name": "Bash", "args": {"cmd": "pwd"}}
                }]}}]
            }),
            json!({
                "candidates": [{"content": {"parts": [{
                    "functionCall": {"name": "Bash", "args": {"cmd": "ls"}}
                }]}}]
            }),
        ];

        for chunk in &chunks {
            gemini_handle_stream_chunk(&mut handler, chunk)
                .expect("stream chunk should process");
        }

        let calls = handler.tool_calls();
        assert_eq!(
            calls.len(),
            2,
            "each functionCall chunk must be emitted exactly once"
        );
        assert_eq!(calls[0].arguments, json!({"cmd": "pwd"}));
        assert_eq!(calls[1].arguments, json!({"cmd": "ls"}));
    }

    /// Regression guard: two functionCalls delivered in the SAME chunk
    /// (Gemini sometimes batches parts) must still emit as two distinct
    /// calls.
    #[test]
    fn two_function_calls_in_one_chunk_do_not_double_emit() {
        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());

        let chunk = json!({
            "candidates": [{"content": {"parts": [
                {"functionCall": {"name": "Bash", "args": {"cmd": "pwd"}}},
                {"functionCall": {"name": "Bash", "args": {"cmd": "ls"}}}
            ]}}]
        });

        gemini_handle_stream_chunk(&mut handler, &chunk).expect("stream chunk should process");

        let calls = handler.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments, json!({"cmd": "pwd"}));
        assert_eq!(calls[1].arguments, json!({"cmd": "ls"}));
    }

    /// End-to-end thought + thoughtSignature round-trip for Gemini/Vertex AI.
    ///
    /// Gemini's protocol carries `thought: <text>` parts and a
    /// `thoughtSignature` on functionCall parts. Dropping either on the
    /// round-trip leaves the model's tool calls orphaned on the next turn.
    /// The streaming code routes `part["thought"]` to `handler.thought()`
    /// and captures `thoughtSignature` on tool_call emission; this test
    /// pins both behaviours AND verifies the serialiser echoes them back.
    #[test]
    fn gemini_streaming_thought_roundtrips_into_next_request_body() {
        use harnx_core::api_types::ChatCompletionsData;
        use harnx_core::message::{Message, MessageContent, MessageContentToolCalls, MessageRole};
        use harnx_core::model::Model;
        use harnx_core::tool::ToolResult;

        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());

        // Realistic Gemini chunk: a thought part, a text part, and a
        // functionCall part with thoughtSignature, all in one candidate.
        let chunk = json!({
            "candidates": [{"content": {"parts": [
                {"thought": "Plan the call."},
                {"text": "Running ls."},
                {
                    "functionCall": {"name": "Bash", "args": {"cmd": "ls"}},
                    "thoughtSignature": "sig_gemini_xyz"
                }
            ]}}]
        });
        gemini_handle_stream_chunk(&mut handler, &chunk).expect("stream chunk should process");

        let (text, thought, tool_calls, _usage) = handler.take();
        // Gemini prepends "\n\n" for non-first parts (gemini_handle_part).
        assert!(
            text.contains("Running ls."),
            "text part flows to text buffer; got {text:?}"
        );
        assert_eq!(
            thought.as_deref(),
            Some("Plan the call."),
            "thought part must reach the dedicated thought buffer (not text)"
        );
        assert!(
            !text.contains("Plan the call."),
            "thought must not leak into text buffer; got {text:?}"
        );
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(
            tool_calls[0].thought_signature.as_deref(),
            Some("sig_gemini_xyz"),
            "thoughtSignature on functionCall must reach ToolCall.thought_signature"
        );

        // Now feed it back through the serialiser and confirm the next
        // request body carries thought + thoughtSignature on the model turn.
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
        let mut model = Model::new("gemini", "gemini-2.5-pro");
        model.set_max_tokens(Some(4096), true);
        let body = gemini_build_chat_completions_body(
            ChatCompletionsData {
                messages,
                temperature: None,
                top_p: None,
                functions: None,
                stream: true,
            },
            &model,
        )
        .unwrap();

        let contents = body["contents"].as_array().unwrap();
        let model_turn = contents
            .iter()
            .find(|c| c["role"] == "model")
            .expect("must have a model role turn after the user message");
        let parts = model_turn["parts"].as_array().unwrap();
        let thought_part = parts
            .iter()
            .find(|p| p["thought"].is_string())
            .expect("model turn must include a thought part on the round-trip");
        assert_eq!(thought_part["thought"], "Plan the call.");
        let fcall_part = parts
            .iter()
            .find(|p| p["functionCall"].is_object())
            .expect("model turn must include a functionCall part");
        assert_eq!(
            fcall_part["thoughtSignature"], "sig_gemini_xyz",
            "thoughtSignature must be echoed verbatim alongside the functionCall"
        );
    }
}
