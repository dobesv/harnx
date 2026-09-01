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
use harnx_core::event::{AgentEvent, ModelEvent, NoticeEvent};
use harnx_core::sink::emit_agent_event;
use harnx_core::text::{extract_code_block, strip_think_tag};
use harnx_core::tool::ToolCall;
use harnx_render::pretty_error_string;
use tracing::Instrument;

fn llm_request_span(client: &dyn Client) -> tracing::Span {
    tracing::info_span!(
        "llm_request",
        otel.kind = "client",
        gen_ai.system = client.name(),
        gen_ai.request.model = client.model().name(),
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
        gen_ai.usage.cache_write.input_tokens = tracing::field::Empty,
        harnx.gen_ai.usage.cached_tokens = tracing::field::Empty,
        harnx.gen_ai.cost.usd = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
        otel.status_description = tracing::field::Empty,
    )
}

fn record_token_count(span: &tracing::Span, field: &'static str, value: Option<u64>) {
    if let Some(value) = value {
        span.record(field, i64::try_from(value).unwrap_or(i64::MAX));
    }
}

fn record_cost(span: &tracing::Span, cost: Option<f64>) {
    if let Some(cost) = cost {
        span.record("harnx.gen_ai.cost.usd", cost);
    }
}

fn record_llm_error(span: &tracing::Span, error: &anyhow::Error) {
    span.record("otel.status_code", "ERROR");
    span.record("otel.status_description", error.to_string());
}

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
    let span = llm_request_span(client);
    // `SseHandler` exposes usage only through `take(self)`, which the caller
    // invokes after this borrowed handler returns and the span has closed.
    let result = async {
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
    .instrument(span.clone())
    .await;

    if let Err(error) = &result {
        record_llm_error(&span, error);
    }

    result
}

/// Orchestrate one non-streaming LLM call. Wraps `chat_completions_with_data`
/// with: optional code-block extraction, `AgentEvent::Model` event emission,
/// and extraction of the response fields the caller needs (text, thought,
/// tool_calls, usage). Tool-call evaluation stays on the caller side so the
/// caller can control whether a spinner covers that work.
///
/// Terminal model events are deliberately not emitted here. One transport
/// invocation can be followed by tool execution, stop-hook continuation, or
/// another pending message; the agent loop owns the actual `ModelEvent::Final`
/// / `ModelEvent::Error` boundary.
pub async fn run_chat_completion(
    client: &dyn Client,
    data: ChatCompletionsData,
    ctx: &ClientCallContext<'_>,
    extract_code: bool,
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
                cache_write_tokens,
                ..
            } = output;
            let mut usage = CompletionTokenUsage::new(input_tokens, output_tokens, cached_tokens);
            usage.cache_write_tokens = cache_write_tokens.unwrap_or_default();

            if !text.is_empty() && extract_code {
                text = extract_code_block(&strip_think_tag(&text)).to_string();
            }

            if !usage.is_empty() {
                emit_agent_event(AgentEvent::Model(ModelEvent::Usage {
                    input: usage.input_tokens,
                    output: usage.output_tokens,
                    cached: usage.cached_tokens,
                    cache_write: usage.cache_write_tokens,
                    session_label: None,
                }));
            }

            Ok((text, thought, tool_calls, usage))
        }
        Err(err) => Err(err),
    }
}

/// Orchestrate one streaming LLM call. Runs `chat_completions_streaming_with_data`
/// (which consumes the caller-supplied `SseHandler`), then after completion
/// extracts the response tuple from the handler and emits usage events via
/// `harnx_core::sink`. Terminal model events belong to the agent loop rather
/// than an individual transport invocation.
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
        return Ok((text, thought, vec![], usage, true));
    }

    match send_ret {
        Ok(_) => {
            if !usage.is_empty() {
                emit_agent_event(AgentEvent::Model(ModelEvent::Usage {
                    input: usage.input_tokens,
                    output: usage.output_tokens,
                    cached: usage.cached_tokens,
                    cache_write: usage.cache_write_tokens,
                    session_label: None,
                }));
            }
            Ok((text, thought, tool_calls, usage, false))
        }
        Err(err) => {
            if text.trim().is_empty() {
                Err(err)
            } else {
                emit_agent_event(AgentEvent::Notice(NoticeEvent::Warning(
                    pretty_error_string(&err),
                )));
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
    let span = llm_request_span(client);
    let result = async {
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
    .instrument(span.clone())
    .await;

    match &result {
        Ok(output) => {
            record_token_count(&span, "gen_ai.usage.input_tokens", output.input_tokens);
            record_token_count(&span, "gen_ai.usage.output_tokens", output.output_tokens);
            record_token_count(
                &span,
                "gen_ai.usage.cache_read.input_tokens",
                output.cached_tokens,
            );
            record_token_count(
                &span,
                "gen_ai.usage.cache_write.input_tokens",
                output.cache_write_tokens,
            );
            record_token_count(
                &span,
                "harnx.gen_ai.usage.cached_tokens",
                output.cached_tokens,
            );
            let mut usage = CompletionTokenUsage::new(
                output.input_tokens,
                output.output_tokens,
                output.cached_tokens,
            );
            usage.cache_write_tokens = output.cache_write_tokens.unwrap_or_default();
            record_cost(&span, client.model().cost_usd(&usage));
        }
        Err(error) => record_llm_error(&span, error),
    }

    result
}

#[cfg(test)]
mod tests {
    use super::{chat_completions_with_data, run_chat_completion_streaming};
    use harnx_client::{
        ChatCompletionsData, ChatCompletionsOutput, ClientCallContext, CompletionTokenUsage, Model,
        ModelData, SseHandler,
    };
    use harnx_core::abort::create_abort_signal;
    use harnx_core::event::{AgentEvent, AgentEventSink, ModelEvent, NoticeEvent};
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

    #[test]
    fn llm_error_status_description_omits_wrapped_cause() {
        harnx_core::require_nextest();
        let error = anyhow::anyhow!("SECRET_TOKEN_BODY").context("chat completions request failed");

        // `record_llm_error` records Display output. Unlike Debug or the full
        // error chain, Display includes only this safe outer context.
        let status_description = error.to_string();
        assert!(status_description.contains("chat completions request failed"));
        assert!(!status_description.contains("SECRET_TOKEN_BODY"));
    }

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
                    AgentEvent::Model(ModelEvent::Error(message))
                    | AgentEvent::Notice(NoticeEvent::Warning(message)) => Some(message.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    impl AgentEventSink for CollectingSink {
        fn emit(&self, event: AgentEvent) {
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

    fn priced_span_model(name: &str, cache_write_price: Option<f64>) -> Model {
        let mut data = ModelData::new(name);
        data.input_price = Some(3.0);
        data.output_price = Some(15.0);
        data.cache_read_price = Some(0.3);
        data.cache_write_price = cache_write_price;
        Model::from_config("mock", &[data])
            .into_iter()
            .next()
            .expect("span test model should exist")
    }

    fn span_test_output() -> ChatCompletionsOutput {
        ChatCompletionsOutput {
            text: "done".to_owned(),
            input_tokens: Some(1_000_000),
            output_tokens: Some(100_000),
            cached_tokens: Some(200_000),
            cache_write_tokens: Some(100_000),
            ..Default::default()
        }
    }

    fn span_attribute_f64(attributes: &[opentelemetry::KeyValue], key: &str) -> Option<f64> {
        attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .and_then(|attribute| match &attribute.value {
                opentelemetry::Value::F64(value) => Some(*value),
                _ => None,
            })
    }

    fn collect_llm_request_span(
        name: &str,
        cache_write_price: Option<f64>,
    ) -> opentelemetry_sdk::trace::SpanData {
        let spans = harnx_telemetry::collect_test_spans(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime should build")
                .block_on(async {
                    let client = MockClient::builder()
                        .model(priced_span_model(name, cache_write_price))
                        .add_turn(MockTurnBuilder::new().output(span_test_output()).build())
                        .build();
                    chat_completions_with_data(
                        &client,
                        ChatCompletionsData {
                            stream: false,
                            ..streaming_data()
                        },
                        &ClientCallContext {
                            user_agent: None,
                            dry_run: false,
                        },
                    )
                    .await
                    .expect("mock completion should succeed");
                });
        });

        assert_eq!(spans.len(), 1);
        spans.into_iter().next().expect("span should be exported")
    }

    fn assert_cache_usage_attributes(span: &opentelemetry_sdk::trace::SpanData) {
        for (key, value) in [
            ("gen_ai.usage.cache_read.input_tokens", 200_000_i64),
            ("gen_ai.usage.cache_write.input_tokens", 100_000_i64),
            ("harnx.gen_ai.usage.cached_tokens", 200_000_i64),
        ] {
            assert!(
                span.attributes
                    .contains(&opentelemetry::KeyValue::new(key, value)),
                "missing {key}={value}"
            );
        }
    }

    #[test]
    fn llm_request_span_records_cache_usage_and_cost_when_priced() {
        harnx_core::require_nextest();
        let span = collect_llm_request_span("priced-model", Some(3.75));

        assert_cache_usage_attributes(&span);
        let cost = span_attribute_f64(&span.attributes, "harnx.gen_ai.cost.usd")
            .expect("priced span should contain cost");
        assert!((cost - 4.035).abs() < 1e-12, "unexpected span cost: {cost}");
    }

    #[test]
    fn llm_request_span_omits_cost_when_cache_price_is_missing() {
        harnx_core::require_nextest();
        let span = collect_llm_request_span("unpriced-model", None);

        assert_cache_usage_attributes(&span);
        assert!(span
            .attributes
            .iter()
            .all(|attribute| attribute.key.as_str() != "harnx.gen_ai.cost.usd"));
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
    /// call result plus terminal/warning messages emitted to the agent-event
    /// sink so each test can assert its own expectations.
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
        assert!(messages.is_empty());
    }

    #[test]
    fn streaming_error_with_whitespace_only_text_returns_err() {
        let (result, messages) = run_streaming_error_case_serial(Some("\n"));
        assert!(result.is_err());
        assert!(messages.is_empty());
    }

    #[test]
    fn streaming_error_with_partial_text_returns_ok_and_emits_warning() {
        let (result, messages) = run_streaming_error_case_serial(Some("partial"));
        let output = result.expect("partial text should be returned");
        assert_eq!(output.0, "partial");
        assert!(output.2.is_empty());
        assert_eq!(messages.len(), 1);
        assert!(!messages[0].is_empty());
    }
}
