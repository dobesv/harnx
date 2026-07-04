//! LLM call wrappers that operate on pre-built `ChatCompletionsData`.
//! Harnx wrappers in `crates/harnx/src/client/common.rs` build
//! `ChatCompletionsData` from `Input + GlobalConfig` and delegate here.
//! Dry-run handling also stays on the harnx side because it requires
//! `Input + Config` to compute the echo text.

use anyhow::{Context, Result};
use harnx_client::{
    ChatCompletionsData, ChatCompletionsOutput, Client, ClientCallContext, CompletionTokenUsage,
    SseHandler,
};
use harnx_core::abort::wait_abort_signal;
use harnx_core::event::{AgentEvent, ModelEvent};
use harnx_core::sink::emit_agent_event;
use harnx_core::text::{extract_code_block, strip_think_tag};
use harnx_core::tool::ToolCall;
use harnx_render::pretty_error_string;

/// Orchestrate one streaming chat-completions call, honouring the caller's abort signal.
///
/// This is the lowest-level transport wrapper used by both the engine and the TUI.
/// It *only* runs the client's streaming API and turns transport-level aborts into
/// `Ok(())`; higher layers decide how to interpret partial text/tool calls.
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
            emit_agent_event(AgentEvent::Model(ModelEvent::Error(pretty_error_string(
                &err,
            ))));
            Err(err)
        }
    }
}

/// Orchestrate one streaming LLM call. Runs `chat_completions_streaming_with_data`
/// (which consumes the caller-supplied `SseHandler`), then after completion
/// extracts the response tuple from the handler and emits `AgentEvent::Model`
/// events via `harnx_core::sink`.
///
/// Returns `(text, thought, tool_calls, usage, aborted)`. Caller is responsible
/// for: tool_call evaluation, stdout newline cleanup, and final Ok/Err shaping.
/// On abort or partial-response error, `tool_calls` is returned empty.
pub async fn run_chat_completion_streaming(
    client: &dyn Client,
    data: ChatCompletionsData,
    ctx: &ClientCallContext<'_>,
    mut handler: SseHandler,
    _abort_signal: harnx_core::abort::AbortSignal,
) -> Result<(
    String,
    Option<String>,
    Vec<harnx_core::tool::ToolCall>,
    CompletionTokenUsage,
    bool,
)> {
    use harnx_core::event::{AgentEvent, ModelEvent};
    use harnx_core::sink::emit_agent_event;

    let send_ret = chat_completions_streaming_with_data(client, data, &mut handler, ctx).await;

    let aborted = handler.abort().aborted();
    let (text, thought, tool_calls, usage) = handler.take();

    if aborted {
        emit_agent_event(AgentEvent::Model(ModelEvent::Error("aborted".to_string())));
        return Ok((text, thought, vec![], usage, true));
    }

    match send_ret {
        Ok(_) => {
            emit_agent_event(AgentEvent::Model(ModelEvent::Final {
                output: String::new(),
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
            Ok((text, thought, tool_calls, usage, false))
        }
        Err(err) => {
            emit_agent_event(AgentEvent::Model(ModelEvent::Error(pretty_error_string(
                &err,
            ))));
            if text.trim().is_empty() {
                Err(err)
            } else {
                Ok((text, thought, vec![], usage, false))
            }
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::run_chat_completion_streaming;
    use harnx_client::{ChatCompletionsData, ClientCallContext, CompletionTokenUsage, SseHandler};
    use harnx_core::abort::create_abort_signal;
    use harnx_core::event::{AgentEvent, AgentEventSink, AgentSource, ModelEvent};
    use harnx_core::sink::{clear_agent_event_sink, install_agent_event_sink};
    use harnx_runtime::test_utils::{MockClient, MockTurnBuilder};
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tokio::sync::mpsc::unbounded_channel;

    /// The agent event sink is process-global state. These tests
    /// `clear_agent_event_sink` / `install_agent_event_sink`, so running two
    /// of them in the same process in parallel would let them swap collectors
    /// mid-flight. Acquire this guard for the full setup/call/cleanup window.
    /// Mirrors `harnx-core`'s `sink` test module.
    static SINK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Ignore `PoisonError` so a panic in one test doesn't cascade-fail the
    /// rest of the sink tests in this module.
    fn lock_sink_tests() -> std::sync::MutexGuard<'static, ()> {
        SINK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct CollectingSink {
        events: Mutex<Vec<AgentEvent>>,
    }

    impl CollectingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(Vec::new()),
            })
        }

        fn event_messages(&self) -> Vec<String> {
            self.events
                .lock()
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::Model(ModelEvent::Error(message)) => Some(message.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    impl AgentEventSink for CollectingSink {
        fn emit(&self, event: AgentEvent, _source: Option<AgentSource>) {
            self.events.lock().push(event);
        }
    }

    fn install_collecting_sink() -> Arc<CollectingSink> {
        clear_agent_event_sink();
        let sink = CollectingSink::new();
        install_agent_event_sink(sink.clone());
        sink
    }

    fn streaming_data() -> ChatCompletionsData {
        ChatCompletionsData {
            messages: Vec::new(),
            temperature: None,
            top_p: None,
            functions: None,
            stream: true,
            attachments_dir: None,
        }
    }

    fn sse_handler() -> SseHandler {
        let (tx, _rx) = unbounded_channel();
        SseHandler::new(tx, create_abort_signal())
    }

    fn stream_error() -> anyhow::Error {
        anyhow::anyhow!("Internal server error (type: api_error)")
    }

    type StreamingResult = anyhow::Result<(
        String,
        Option<String>,
        Vec<harnx_core::tool::ToolCall>,
        CompletionTokenUsage,
        bool,
    )>;

    /// Run `run_chat_completion_streaming` against a mock turn that streams
    /// `text_chunk` (when `Some`) followed by a mid-stream error. Returns the
    /// call result plus the error messages emitted to the agent-event sink so
    /// each test can assert its own expectations.
    async fn run_streaming_error_case(text_chunk: Option<&str>) -> (StreamingResult, Vec<String>) {
        let sink = install_collecting_sink();
        let mut turn = MockTurnBuilder::new();
        if let Some(chunk) = text_chunk {
            turn = turn.add_text_chunk(chunk);
        }
        let client = MockClient::builder()
            .add_turn(turn.add_error(stream_error()).build())
            .build();

        let result = run_chat_completion_streaming(
            &client,
            streaming_data(),
            &ClientCallContext {
                user_agent: None,
                dry_run: false,
            },
            sse_handler(),
            create_abort_signal(),
        )
        .await;

        let messages = sink.event_messages();
        clear_agent_event_sink();
        (result, messages)
    }

    /// Serialized, synchronous wrapper around `run_streaming_error_case`.
    /// Holds the sink guard in sync context (never across an `.await`, which
    /// would trip `clippy::await_holding_lock`) and drives the async body on a
    /// fresh current-thread runtime.
    fn run_streaming_error_case_serial(text_chunk: Option<&str>) -> (StreamingResult, Vec<String>) {
        let _sink_guard = lock_sink_tests();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
            .block_on(run_streaming_error_case(text_chunk))
    }

    #[test]
    fn streaming_error_with_empty_text_returns_err() {
        let (result, messages) = run_streaming_error_case_serial(None);
        assert!(result.is_err());
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn streaming_error_with_whitespace_only_text_returns_err() {
        let (result, messages) = run_streaming_error_case_serial(Some("\n"));
        assert!(result.is_err());
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn streaming_error_with_partial_text_returns_ok_and_emits_error() {
        let (result, messages) = run_streaming_error_case_serial(Some("partial"));
        let output = result.expect("partial text should be returned");
        assert_eq!(output.0, "partial");
        assert!(output.2.is_empty());
        assert_eq!(messages.len(), 1);
        assert!(!messages[0].is_empty());
    }
}
