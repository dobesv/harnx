use crate::*;

use harnx_core::crypto::{base64_decode, encode_uri, hex_encode, hmac_sha256, sha256};
use harnx_core::text::strip_think_tag;

use anyhow::{bail, Context, Result};
use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use aws_smithy_eventstream::smithy::parse_response_headers;
use bytes::BytesMut;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use indexmap::IndexMap;
use reqwest::{Client as ReqwestClient, Method, RequestBuilder};
use serde::Deserialize;
use serde_json::{json, Value};

impl BedrockClient {
    config_get_fn!(access_key_id, get_access_key_id);
    config_get_fn!(secret_access_key, get_secret_access_key);
    config_get_fn!(region, get_region);
    config_get_fn!(session_token, get_session_token);

    pub const PROMPTS: [PromptAction<'static>; 3] = [
        ("access_key_id", "AWS Access Key ID", None),
        ("secret_access_key", "AWS Secret Access Key", None),
        ("region", "AWS Region", None),
    ];

    fn chat_completions_builder(
        &self,
        client: &ReqwestClient,
        data: ChatCompletionsData,
    ) -> Result<RequestBuilder> {
        let access_key_id = self.get_access_key_id()?;
        let secret_access_key = self.get_secret_access_key()?;
        let region = self.get_region()?;
        let session_token = self.get_session_token().ok();
        let host = format!("bedrock-runtime.{region}.amazonaws.com");

        let model_name = &self.model.real_name();

        let uri = if data.stream {
            format!("/model/{model_name}/converse-stream")
        } else {
            format!("/model/{model_name}/converse")
        };

        let body = build_chat_completions_body(data, &self.model)?;

        let mut request_data = RequestData::new("", body);
        self.patch_request_data(&mut request_data)?;
        let RequestData {
            url: _,
            headers,
            body,
        } = request_data;

        let builder = aws_fetch(
            client,
            &AwsCredentials {
                access_key_id,
                secret_access_key,
                region,
                session_token,
            },
            AwsRequest {
                method: Method::POST,
                host,
                service: "bedrock".into(),
                uri,
                querystring: "".into(),
                headers,
                body: body.to_string(),
            },
        )?;

        Ok(builder)
    }

    fn embeddings_builder(
        &self,
        client: &ReqwestClient,
        data: &EmbeddingsData,
    ) -> Result<RequestBuilder> {
        let access_key_id = self.get_access_key_id()?;
        let secret_access_key = self.get_secret_access_key()?;
        let region = self.get_region()?;
        let session_token = self.get_session_token().ok();
        let host = format!("bedrock-runtime.{region}.amazonaws.com");

        let uri = format!("/model/{}/invoke", self.model.real_name());

        let input_type = match data.query {
            true => "search_query",
            false => "search_document",
        };

        let body = json!({
            "texts": data.texts,
            "input_type": input_type,
        });

        let mut request_data = RequestData::new("", body);
        self.patch_request_data(&mut request_data)?;
        let RequestData {
            url: _,
            headers,
            body,
        } = request_data;

        let builder = aws_fetch(
            client,
            &AwsCredentials {
                access_key_id,
                secret_access_key,
                region,
                session_token,
            },
            AwsRequest {
                method: Method::POST,
                host,
                service: "bedrock".into(),
                uri,
                querystring: "".into(),
                headers,
                body: body.to_string(),
            },
        )?;

        Ok(builder)
    }
}

#[async_trait::async_trait]
impl Client for BedrockClient {
    client_common_fns!();

    async fn chat_completions_inner(
        &self,
        client: &ReqwestClient,
        data: ChatCompletionsData,
    ) -> Result<ChatCompletionsOutput> {
        let builder = self.chat_completions_builder(client, data)?;
        chat_completions(builder).await
    }

    async fn chat_completions_streaming_inner(
        &self,
        client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()> {
        let builder = self.chat_completions_builder(client, data)?;
        chat_completions_streaming(builder, handler).await
    }

    async fn embeddings_inner(
        &self,
        client: &ReqwestClient,
        data: &EmbeddingsData,
    ) -> Result<EmbeddingsOutput> {
        let builder = self.embeddings_builder(client, data)?;
        embeddings(builder).await
    }
}

async fn chat_completions(builder: RequestBuilder) -> Result<ChatCompletionsOutput> {
    let res = builder.send().await?;
    let status = res.status();
    let retry_after = parse_retry_after(res.headers());
    let data: Value = res.json().await?;

    if !status.is_success() {
        catch_error(&data, status.as_u16(), retry_after)?;
    }

    debug!("non-stream-data: {data}");
    harnx_core::llm_trace::response("bedrock", &data);
    extract_chat_completions(&data)
}

/// Mutable accumulator state for the Bedrock streaming parser.
/// Extracted so per-event handling is testable in isolation.
#[derive(Default)]
struct BedrockStreamState {
    function_name: String,
    function_arguments: String,
    function_id: String,
    /// Accumulated signature from `reasoningContent.signature` deltas for
    /// the current reasoning block. Passed to each toolUse emitted in the
    /// same turn so the serialiser can echo it back verbatim on the next
    /// request — Bedrock (and the underlying Anthropic API) rejects
    /// reasoning round-trips whose signature is missing or modified.
    thinking_signature: String,
}

fn bedrock_emit_pending_tool_call(
    state: &mut BedrockStreamState,
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

fn bedrock_handle_content_block_start(
    state: &mut BedrockStreamState,
    handler: &mut SseHandler,
    data: &Value,
) -> Result<()> {
    let Some(tool_use) = data["start"]["toolUse"].as_object() else {
        return Ok(());
    };
    let (Some(id), Some(name)) = (
        json_str_from_map(tool_use, "toolUseId"),
        json_str_from_map(tool_use, "name"),
    ) else {
        return Ok(());
    };
    // Fallback emit for servers that skip contentBlockStop.
    bedrock_emit_pending_tool_call(state, handler)?;
    // Defensive clear preserved from the original code, in case args
    // were accumulated without a preceding function_name.
    state.function_arguments.clear();
    state.function_name = name.into();
    state.function_id = id.into();
    Ok(())
}

fn bedrock_handle_content_block_delta(
    state: &mut BedrockStreamState,
    handler: &mut SseHandler,
    data: &Value,
) -> Result<()> {
    if let Some(text) = data["delta"]["text"].as_str() {
        handler.text(text)?;
    } else if let Some(text) = data["delta"]["reasoningContent"]["text"].as_str() {
        // Route reasoning text to the dedicated thought buffer so the
        // serialiser can echo a `reasoningContent` block on the next
        // request. Routing to `handler.text()` instead folds reasoning
        // into the text buffer wrapped in `<think>...</think>` and
        // returns `thought = None`, which makes the next turn omit the
        // reasoningContent block entirely — the model then sees its own
        // tool calls as orphaned and produces "previous session"
        // hallucinations.
        handler.thought(text)?;
    } else if let Some(sig) = data["delta"]["reasoningContent"]["signature"].as_str() {
        // Bedrock Converse Stream delivers the reasoning-block signature
        // via a `reasoningContent.signature` delta after the text deltas.
        // Accumulate so it can be attached to every toolUse in the turn.
        state.thinking_signature.push_str(sig);
    } else if let Some(input) = data["delta"]["toolUse"]["input"].as_str() {
        state.function_arguments.push_str(input);
    }
    Ok(())
}

fn bedrock_handle_content_block_stop(
    state: &mut BedrockStreamState,
    handler: &mut SseHandler,
) -> Result<()> {
    // Emit if a toolUse block is pending, and reset accumulators so the
    // fallback emit path in contentBlockStart doesn't re-fire this same
    // call when the next toolUse block begins. Same bug that affected
    // the Claude streaming parser (both follow Anthropic's content-block
    // protocol).
    //
    // Reasoning-block close brackets are no longer emitted here — the
    // SseHandler's `thought()` / `text()` / `tool_call()` / `done()`
    // methods manage `<think>...</think>` display framing themselves now
    // that reasoning text flows through `thought_buffer`.
    bedrock_emit_pending_tool_call(state, handler)
}

fn bedrock_handle_stream_event(
    state: &mut BedrockStreamState,
    handler: &mut SseHandler,
    smithy_type: &str,
    data: &Value,
) -> Result<()> {
    match smithy_type {
        "contentBlockStart" => bedrock_handle_content_block_start(state, handler, data)?,
        "contentBlockDelta" => bedrock_handle_content_block_delta(state, handler, data)?,
        "contentBlockStop" => bedrock_handle_content_block_stop(state, handler)?,
        "metadata" => {
            handler.set_usage(
                data["usage"]["inputTokens"].as_u64(),
                data["usage"]["outputTokens"].as_u64(),
                data["usage"]["cacheReadInputTokens"].as_u64(),
            );
        }
        _ => {}
    }
    Ok(())
}

async fn chat_completions_streaming(
    builder: RequestBuilder,
    handler: &mut SseHandler,
) -> Result<()> {
    let res = builder.send().await?;
    let status = res.status();
    if !status.is_success() {
        let retry_after = parse_retry_after(res.headers());
        let data: Value = res.json().await?;
        catch_error(&data, status.as_u16(), retry_after)?;
        bail!("Invalid response data: {data}");
    }

    let mut state = BedrockStreamState::default();

    let mut stream = res.bytes_stream();
    let mut buffer = BytesMut::new();
    let mut decoder = MessageFrameDecoder::new();
    while let Some(chunk) = stream.next().await {
        if handler.aborted() {
            break;
        }
        let chunk = chunk?;
        buffer.extend_from_slice(&chunk);
        while let DecodedFrame::Complete(message) = decoder.decode_frame(&mut buffer)? {
            let response_headers = parse_response_headers(&message)?;
            let message_type = response_headers.message_type.as_str();
            let smithy_type = response_headers.smithy_type.as_str();
            match (message_type, smithy_type) {
                ("event", _) => {
                    let data: Value = serde_json::from_slice(message.payload())?;
                    debug!("stream-data: {smithy_type} {data}");
                    if harnx_core::llm_trace::is_enabled() {
                        harnx_core::llm_trace::stream_event(
                            "bedrock",
                            &serde_json::json!({"smithy_type": smithy_type, "event": data}),
                        );
                    }
                    bedrock_handle_stream_event(&mut state, handler, smithy_type, &data)?;
                }
                ("exception", _) => {
                    let payload = base64_decode(message.payload())?;
                    let data = String::from_utf8_lossy(&payload);

                    bail!("Invalid response data: {data} (smithy_type: {smithy_type})")
                }
                _ => {
                    bail!("Unrecognized message, message_type: {message_type}, smithy_type: {smithy_type}",);
                }
            }
        }
    }
    Ok(())
}

async fn embeddings(builder: RequestBuilder) -> Result<EmbeddingsOutput> {
    let res = builder.send().await?;
    let status = res.status();
    let data: Value = res.json().await?;

    if !status.is_success() {
        catch_error(&data, status.as_u16(), None)?;
    }

    let res_body: EmbeddingsResBody =
        serde_json::from_value(data).context("Invalid embeddings data")?;
    Ok(res_body.embeddings)
}

#[derive(Deserialize)]
struct EmbeddingsResBody {
    embeddings: Vec<Vec<f32>>,
}

fn build_chat_completions_body(data: ChatCompletionsData, model: &Model) -> Result<Value> {
    let ChatCompletionsData {
        mut messages,
        temperature,
        top_p,
        functions,
        stream: _,
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
                    vec![json!({ "role": role, "content": [ { "text": strip_think_tag(&text) } ] })]
                }
                MessageContent::Text(text) => vec![json!({
                    "role": role,
                    "content": [
                        {
                            "text": text,
                        }
                    ],
                })],
                MessageContent::Array(list) => {
                    let content: Vec<_> = list
                        .into_iter()
                        .map(|item| match item {
                            MessageContentPart::Text { text } => {
                                json!({"text": text})
                            }
                            MessageContentPart::ImageUrl {
                                image_url: ImageUrl { url },
                            } => {
                                if let Some((mime_type, data)) = url
                                    .strip_prefix("data:")
                                    .and_then(|v| v.split_once(";base64,"))
                                {
                                    json!({
                                        "image": {
                                            "format": mime_type.replace("image/", ""),
                                            "source": {
                                                "bytes": data,
                                            }
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
                    tool_results,
                    text,
                    thought,
                    ..
                }) => {
                    let mut assistant_parts = vec![];
                    let mut user_parts = vec![];
                    if let Some(thought_text) = thought {
                        // Echo the reasoningContent block verbatim so
                        // Bedrock knows this assistant turn included
                        // extended thinking. The signature is stored on
                        // each tool call in the turn — omitting it
                        // causes the model to treat its own tool calls
                        // as coming from a "previous session" and
                        // produce replay-confusion hallucinations.
                        let signature = tool_results
                            .first()
                            .and_then(|r| r.call.thought_signature.as_deref())
                            .unwrap_or("");
                        let mut reasoning_text = json!({ "text": thought_text });
                        if !signature.is_empty() {
                            if let Some(obj) = reasoning_text.as_object_mut() {
                                obj.insert("signature".to_string(), signature.into());
                            }
                        }
                        assistant_parts.push(json!({
                            "reasoningContent": {
                                "reasoningText": reasoning_text,
                            }
                        }));
                    }
                    if !text.is_empty() {
                        assistant_parts.push(json!({
                            "text": text,
                        }))
                    }
                    for tool_result in tool_results {
                        assistant_parts.push(json!({
                            "toolUse": {
                                "toolUseId": tool_result.call.id,
                                "name": tool_result.call.name,
                                "input": tool_result.call.arguments,
                            }
                        }));
                        user_parts.push(json!({
                            "toolResult": {
                                "toolUseId": tool_result.call.id,
                                "content": [
                                    {
                                        "json": tool_result.output,
                                    }
                                ]
                            }
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
        "inferenceConfig": {},
        "messages": messages,
    });
    if let Some(parts) = system_message {
        let system_blocks: Vec<Value> = parts
            .iter()
            .map(|text| json!({"text": text}))
            .collect();
        body["system"] = system_blocks.into();
    }

    if let Some(v) = model.max_tokens_param() {
        body["inferenceConfig"]["maxTokens"] = v.into();
    }
    if let Some(v) = temperature {
        body["inferenceConfig"]["temperature"] = v.into();
    }
    if let Some(v) = top_p {
        body["inferenceConfig"]["topP"] = v.into();
    }
    if let Some(functions) = functions {
        let tools: Vec<_> = functions
            .iter()
            .map(|v| {
                json!({
                    "toolSpec": {
                        "name": v.name,
                        "description": v.description,
                        "inputSchema": {
                            "json": v.parameters,
                        },
                    }
                })
            })
            .collect();
        body["toolConfig"] = json!({
            "tools": tools,
        })
    }
    Ok(body)
}

fn extract_chat_completions(data: &Value) -> Result<ChatCompletionsOutput> {
    let mut text = String::new();
    let mut reasoning: Option<String> = None;
    let mut reasoning_signature: Option<String> = None;
    let mut tool_calls = vec![];
    if let Some(array) = data["output"]["message"]["content"].as_array() {
        for item in array {
            if let Some(v) = item["text"].as_str() {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(v);
            } else if let Some(reasoning_text) =
                item["reasoningContent"]["reasoningText"].as_object()
            {
                if let Some(t) = json_str_from_map(reasoning_text, "text") {
                    reasoning = Some(t.to_string());
                }
                if let Some(s) = json_str_from_map(reasoning_text, "signature") {
                    reasoning_signature = Some(s.to_string());
                }
            } else if let Some(tool_use) = item["toolUse"].as_object() {
                if let (Some(id), Some(name), Some(input)) = (
                    json_str_from_map(tool_use, "toolUseId"),
                    json_str_from_map(tool_use, "name"),
                    tool_use.get("input"),
                ) {
                    tool_calls.push(ToolCall::new(
                        name.to_string(),
                        input.clone(),
                        Some(id.to_string()),
                        None, // signature attached below
                    ));
                }
            }
        }
    }

    // Attach the reasoning signature to every tool call in this turn.
    // Bedrock requires the signature echoed back verbatim alongside the
    // reasoningContent block.
    if let Some(sig) = &reasoning_signature {
        for call in &mut tool_calls {
            call.thought_signature = Some(sig.clone());
        }
    }

    // When there are tool calls, carry the thought on its dedicated field
    // so the serialiser can echo back the reasoningContent block on the
    // next request. When there are no tool calls, fold it into text for
    // display (existing behaviour for plain-text reasoning responses).
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
        id: None,
        input_tokens: data["usage"]["inputTokens"].as_u64(),
        output_tokens: data["usage"]["outputTokens"].as_u64(),
        cached_tokens: data["usage"]["cacheReadInputTokens"].as_u64(),
    };
    Ok(output)
}

#[derive(Debug)]
struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
    region: String,
    session_token: Option<String>,
}

#[derive(Debug)]
struct AwsRequest {
    method: Method,
    host: String,
    service: String,
    uri: String,
    querystring: String,
    headers: IndexMap<String, String>,
    body: String,
}

fn aws_fetch(
    client: &ReqwestClient,
    credentials: &AwsCredentials,
    request: AwsRequest,
) -> Result<RequestBuilder> {
    let AwsRequest {
        method,
        host,
        service,
        uri,
        querystring,
        mut headers,
        body,
    } = request;
    let region = &credentials.region;

    let endpoint = format!("https://{host}{uri}");

    let now: DateTime<Utc> = Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = amz_date[0..8].to_string();
    headers.insert("host".into(), host.clone());
    headers.insert("x-amz-date".into(), amz_date.clone());
    if let Some(token) = credentials.session_token.clone() {
        headers.insert("x-amz-security-token".into(), token);
    }

    let canonical_headers = headers
        .iter()
        .map(|(key, value)| format!("{key}:{value}\n"))
        .collect::<Vec<_>>()
        .join("");

    let signed_headers = headers
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let payload_hash = sha256(&body);

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method,
        encode_uri(&uri),
        querystring,
        canonical_headers,
        signed_headers,
        payload_hash
    );

    let algorithm = "AWS4-HMAC-SHA256";
    let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "{}\n{}\n{}\n{}",
        algorithm,
        amz_date,
        credential_scope,
        sha256(&canonical_request)
    );

    let signing_key = gen_signing_key(
        &credentials.secret_access_key,
        &date_stamp,
        region,
        &service,
    );
    let signature = hmac_sha256(&signing_key, &string_to_sign);
    let signature = hex_encode(&signature);

    let authorization_header = format!(
        "{} Credential={}/{}, SignedHeaders={}, Signature={}",
        algorithm, credentials.access_key_id, credential_scope, signed_headers, signature
    );

    headers.insert("authorization".into(), authorization_header);

    debug!("Request {endpoint} {body}");
    harnx_core::llm_trace::request_raw(&endpoint, &body);

    let mut request_builder = client.request(method, endpoint).body(body);

    for (key, value) in &headers {
        request_builder = request_builder.header(key, value);
    }

    Ok(request_builder)
}

fn gen_signing_key(key: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{key}").as_bytes(), date_stamp);
    let k_region = hmac_sha256(&k_date, region);
    let k_service = hmac_sha256(&k_region, service);
    hmac_sha256(&k_service, "aws4_request")
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::abort::create_abort_signal;
    use tokio::sync::mpsc::unbounded_channel;

    /// Regression test: two toolUse blocks in one Bedrock streaming
    /// response must not cause the first one to be emitted twice.
    /// Mirrors the Claude parser fix — contentBlockStop now resets
    /// accumulator state so contentBlockStart's fallback-emit path
    /// doesn't re-fire the previous call.
    #[test]
    fn two_tool_uses_in_one_response_do_not_double_emit() {
        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = BedrockStreamState::default();

        let events: Vec<(&str, Value)> = vec![
            (
                "contentBlockStart",
                json!({"start": {"toolUse": {"toolUseId": "toolu_A", "name": "Bash"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"delta": {"toolUse": {"input": "{\"command\": \"pwd\"}"}}}),
            ),
            ("contentBlockStop", json!({})),
            (
                "contentBlockStart",
                json!({"start": {"toolUse": {"toolUseId": "toolu_B", "name": "Bash"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"delta": {"toolUse": {"input": "{\"command\": \"ls\"}"}}}),
            ),
            ("contentBlockStop", json!({})),
        ];

        for (smithy_type, data) in &events {
            bedrock_handle_stream_event(&mut state, &mut handler, smithy_type, data)
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
            "each toolUse block should be emitted exactly once"
        );
    }

    /// End-to-end reasoning round-trip on the Bedrock STREAMING path.
    ///
    /// When Bedrock Converse Stream delivers a reasoningContent block
    /// (text deltas + trailing signature delta) followed by toolUse, the
    /// streaming parser must:
    ///   1. route reasoning text to `handler.thought()` (not
    ///      `handler.text()`, which would fold reasoning into the text
    ///      buffer wrapped in `<think>...</think>` and lose the thought),
    ///   2. capture the `reasoningContent.signature` delta and attach it
    ///      to every toolUse in the turn, and
    ///   3. surface the thought via `SseHandler::take()` so the
    ///      serialiser can echo a `reasoningContent` block on the next
    ///      request.
    ///
    /// If any of those break, the next turn's request has no
    /// reasoningContent block, the model sees its own tool calls as
    /// orphaned, and it produces "previous session" hallucinations.
    /// This test drives the full state machine and verifies the next
    /// request body is well-formed.
    #[test]
    fn bedrock_streaming_thought_roundtrips_into_next_request_body() {
        use harnx_core::api_types::ChatCompletionsData;
        use harnx_core::message::{Message, MessageContent, MessageContentToolCalls, MessageRole};
        use harnx_core::model::Model;
        use harnx_core::tool::ToolResult;

        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = BedrockStreamState::default();

        // Realistic Bedrock Converse Stream sequence: reasoning block
        // (multi-chunk text + trailing signature delta) then a toolUse.
        let events: Vec<(&str, Value)> = vec![
            ("contentBlockStart", json!({"start": {}})),
            (
                "contentBlockDelta",
                json!({"delta": {"reasoningContent": {"text": "Let me think "}}}),
            ),
            (
                "contentBlockDelta",
                json!({"delta": {"reasoningContent": {"text": "about this."}}}),
            ),
            (
                "contentBlockDelta",
                json!({"delta": {"reasoningContent": {"signature": "sig_bedrock_xyz"}}}),
            ),
            ("contentBlockStop", json!({})),
            (
                "contentBlockStart",
                json!({"start": {"toolUse": {"toolUseId": "toolu_S", "name": "Bash"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"delta": {"toolUse": {"input": "{\"command\":\"ls\"}"}}}),
            ),
            ("contentBlockStop", json!({})),
        ];
        for (smithy_type, data) in &events {
            bedrock_handle_stream_event(&mut state, &mut handler, smithy_type, data)
                .expect("stream event should process");
        }

        let (text, thought, tool_calls, _usage) = handler.take();

        assert_eq!(
            thought.as_deref(),
            Some("Let me think about this."),
            "Bedrock streaming reasoning text must reach the dedicated thought \
             field, not the text buffer wrapped in <think>"
        );
        assert!(
            !text.contains("<think>"),
            "text buffer must not be polluted with <think> wrappers when \
             tool calls are present. Got: {text:?}"
        );
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(
            tool_calls[0].thought_signature.as_deref(),
            Some("sig_bedrock_xyz"),
            "Bedrock streaming reasoningContent.signature must reach \
             ToolCall.thought_signature"
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
        let mut model = Model::new("bedrock", "us.anthropic.claude-sonnet-4-6");
        model.set_max_tokens(Some(4096), true);
        let body = build_chat_completions_body(
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

        let assistant_msg = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("must have an assistant turn");
        let content = assistant_msg["content"]
            .as_array()
            .expect("assistant content array");

        // Find the reasoningContent block — it must exist and precede the
        // toolUse so Bedrock can verify the reasoning matches the request.
        let reasoning_idx = content
            .iter()
            .position(|b| b["reasoningContent"].is_object())
            .expect(
                "next request body must include a reasoningContent block for the \
                 streamed assistant turn: otherwise the model receives tool \
                 results with no record of its prior reasoning",
            );
        let tool_use_idx = content
            .iter()
            .position(|b| b["toolUse"].is_object())
            .expect("must have a toolUse block");
        assert!(
            reasoning_idx < tool_use_idx,
            "reasoningContent must precede toolUse"
        );

        let reasoning_text = &content[reasoning_idx]["reasoningContent"]["reasoningText"];
        assert_eq!(reasoning_text["text"], "Let me think about this.");
        assert_eq!(
            reasoning_text["signature"], "sig_bedrock_xyz",
            "reasoning signature must be echoed verbatim from the streamed \
             reasoningContent.signature delta"
        );
    }

    /// Multiple toolUse blocks in one streamed Bedrock turn must all carry
    /// the same reasoning signature. Bedrock rejects requests where any
    /// toolUse sibling of a reasoningContent block lacks the signature
    /// when echoed back.
    #[test]
    fn bedrock_streaming_multiple_tool_calls_share_thought_signature() {
        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = BedrockStreamState::default();

        let events: Vec<(&str, Value)> = vec![
            ("contentBlockStart", json!({"start": {}})),
            (
                "contentBlockDelta",
                json!({"delta": {"reasoningContent": {"text": "Plan two calls."}}}),
            ),
            (
                "contentBlockDelta",
                json!({"delta": {"reasoningContent": {"signature": "sig_multi"}}}),
            ),
            ("contentBlockStop", json!({})),
            (
                "contentBlockStart",
                json!({"start": {"toolUse": {"toolUseId": "toolu_A", "name": "Bash"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"delta": {"toolUse": {"input": "{\"command\":\"pwd\"}"}}}),
            ),
            ("contentBlockStop", json!({})),
            (
                "contentBlockStart",
                json!({"start": {"toolUse": {"toolUseId": "toolu_B", "name": "Bash"}}}),
            ),
            (
                "contentBlockDelta",
                json!({"delta": {"toolUse": {"input": "{\"command\":\"ls\"}"}}}),
            ),
            ("contentBlockStop", json!({})),
        ];
        for (smithy_type, data) in &events {
            bedrock_handle_stream_event(&mut state, &mut handler, smithy_type, data)
                .expect("stream event should process");
        }

        let (_text, thought, tool_calls, _usage) = handler.take();
        assert_eq!(thought.as_deref(), Some("Plan two calls."));
        assert_eq!(tool_calls.len(), 2);
        for call in &tool_calls {
            assert_eq!(
                call.thought_signature.as_deref(),
                Some("sig_multi"),
                "every streamed toolUse sibling of a reasoningContent block \
                 must carry its signature so the next request body is well-formed"
            );
        }
    }

    /// Non-streaming counterpart: `extract_chat_completions` must capture
    /// the reasoningContent signature and attach it to every parsed
    /// toolUse so a multi-call thinking turn echoes correctly.
    #[test]
    fn bedrock_extract_attaches_signature_to_every_tool_call() {
        let response = json!({
            "output": {"message": {"role": "assistant", "content": [
                {
                    "reasoningContent": {
                        "reasoningText": {
                            "text": "two calls",
                            "signature": "sig_multi"
                        }
                    }
                },
                {"toolUse": {"toolUseId": "toolu_A", "name": "Bash", "input": {"command": "pwd"}}},
                {"toolUse": {"toolUseId": "toolu_B", "name": "Bash", "input": {"command": "ls"}}}
            ]}},
            "usage": {"inputTokens": 1, "outputTokens": 1}
        });

        let output = extract_chat_completions(&response).unwrap();
        assert_eq!(
            output.thought.as_deref(),
            Some("two calls"),
            "thought must be stored in ChatCompletionsOutput.thought, not folded \
             into text when tool calls are present"
        );
        assert_eq!(output.tool_calls.len(), 2);
        for call in &output.tool_calls {
            assert_eq!(
                call.thought_signature.as_deref(),
                Some("sig_multi"),
                "every parsed toolUse sibling of a reasoningContent block must \
                 carry its signature (non-streaming multi-call)"
            );
        }
    }

    /// Bedrock streaming text-only response with reasoning must populate
    /// `thought` cleanly without polluting `text`. Pins the no-tool path
    /// so a future refactor can't re-introduce `<think>` wrappers in text.
    #[test]
    fn bedrock_streaming_text_only_with_thinking_separates_buffers() {
        let (tx, _rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());
        let mut state = BedrockStreamState::default();

        let events: Vec<(&str, Value)> = vec![
            ("contentBlockStart", json!({"start": {}})),
            (
                "contentBlockDelta",
                json!({"delta": {"reasoningContent": {"text": "Considering."}}}),
            ),
            (
                "contentBlockDelta",
                json!({"delta": {"reasoningContent": {"signature": "sig_text"}}}),
            ),
            ("contentBlockStop", json!({})),
            ("contentBlockStart", json!({"start": {}})),
            (
                "contentBlockDelta",
                json!({"delta": {"text": "Final answer."}}),
            ),
            ("contentBlockStop", json!({})),
        ];
        for (smithy_type, data) in &events {
            bedrock_handle_stream_event(&mut state, &mut handler, smithy_type, data)
                .expect("stream event should process");
        }

        let (text, thought, tool_calls, _usage) = handler.take();
        assert_eq!(text, "Final answer.", "text buffer carries only the prose");
        assert_eq!(thought.as_deref(), Some("Considering."));
        assert!(
            tool_calls.is_empty(),
            "no toolUse blocks were sent; tool_calls must stay empty"
        );
    }
}
