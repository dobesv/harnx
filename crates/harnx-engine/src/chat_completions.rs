//! LLM call wrappers that operate on pre-built `ChatCompletionsData`.
//! Harnx wrappers in `crates/harnx/src/client/common.rs` build
//! `ChatCompletionsData` from `Input + GlobalConfig` and delegate here.
//! Dry-run handling also stays on the harnx side because it requires
//! `Input + Config` to compute the echo text.

use anyhow::{Context, Result};
use harnx_client::{Client, ClientCallContext, SseHandler};
use harnx_core::abort::wait_abort_signal;
use harnx_core::api_types::{ChatCompletionsData, ChatCompletionsOutput, CompletionTokenUsage};
use harnx_core::event::{AgentEvent, ModelEvent};
use harnx_core::sink::emit_agent_event;
use harnx_core::text::{extract_code_block, strip_think_tag};
use harnx_core::tool::ToolCall;

/// Non-streaming LLM call. Builds the `reqwest::Client`, invokes
/// `Client::chat_completions_inner`, and wraps the error with a useful
/// context string. Mirrors the original harnx `chat_completions_with_input`
/// body minus the dry-run branch and the `ChatCompletionsData`
/// construction — both of which stay on the harnx caller.
pub async fn chat_completions_with_data(
    client: &dyn Client,
    data: ChatCompletionsData,
    ctx: &ClientCallContext<'_>,
) -> Result<ChatCompletionsOutput> {
    let reqwest_client = client.build_client(ctx)?;
    client
        .chat_completions_inner(&reqwest_client, data)
        .await
        .with_context(|| {
            format!(
                "Failed to call chat-completions api (client: {}, model: {})",
                client.name(),
                client.model().id()
            )
        })
}

/// Streaming LLM call. Races the underlying streaming inner method
/// against `wait_abort_signal`. On abort, notifies the handler via
/// `handler.done()` and returns Ok — the abort-signal mechanism is
/// how the caller communicates cancellation, so the streaming-side
/// doesn't need to surface an error.
pub async fn chat_completions_streaming_with_data(
    client: &dyn Client,
    data: ChatCompletionsData,
    handler: &mut SseHandler,
    ctx: &ClientCallContext<'_>,
) -> Result<()> {
    let abort_signal = handler.abort();
    tokio::select! {
        ret = async {
            let reqwest_client = client.build_client(ctx)?;
            client
                .chat_completions_streaming_inner(&reqwest_client, handler, data)
                .await
        } => {
            handler.done();
            ret.with_context(|| {
                format!(
                    "Failed to call chat-completions api (client: {}, model: {})",
                    client.name(),
                    client.model().id()
                )
            })
        }
        _ = wait_abort_signal(&abort_signal) => {
            handler.done();
            Ok(())
        }
    }
}

/// Orchestrate one non-streaming LLM call. Wraps `chat_completions_with_data`
/// with: optional code-block extraction, `AgentEvent::Model` event emission,
/// and extraction of the response fields the caller needs (text, thought,
/// tool_calls, usage). Tool-call evaluation stays on the caller side so the
/// caller can control whether a spinner covers that work.
///
/// `suppress_final_output`: when true, `ModelEvent::Final` fires with an
/// empty `output` string (signalling that the caller will display the text
/// via another path, e.g. `print_markdown`). When false, `Final` carries the
/// full text so any `AgentEventSink` consumer that renders Final sees the
/// output.
pub async fn run_chat_completion(
    client: &dyn Client,
    data: ChatCompletionsData,
    ctx: &ClientCallContext<'_>,
    extract_code: bool,
    suppress_final_output: bool,
    _abort_signal: harnx_core::abort::AbortSignal,
) -> Result<(String, Option<String>, Vec<ToolCall>, CompletionTokenUsage)> {
    let ret = chat_completions_with_data(client, data, ctx).await;

    match ret {
        Ok(output) => {
            let ChatCompletionsOutput {
                mut text,
                tool_calls,
                thought,
                input_tokens,
                output_tokens,
                cached_tokens,
                ..
            } = output;
            let usage = CompletionTokenUsage::new(input_tokens, output_tokens, cached_tokens);

            if !text.is_empty() && extract_code {
                text = extract_code_block(&strip_think_tag(&text)).to_string();
            }

            let final_output = if suppress_final_output {
                String::new()
            } else {
                text.clone()
            };
            emit_agent_event(AgentEvent::Model(ModelEvent::Final {
                output: final_output,
                usage: usage.clone(),
            }));
            if !usage.is_empty() {
                emit_agent_event(AgentEvent::Model(ModelEvent::Usage {
                    input: usage.input_tokens,
                    output: usage.output_tokens,
                    cached: usage.cached_tokens,
                    session_label: None,
                }));
            }

            Ok((text, thought, tool_calls, usage))
        }
        Err(err) => {
            emit_agent_event(AgentEvent::Model(ModelEvent::Error(err.to_string())));
            Err(err)
        }
    }
}
