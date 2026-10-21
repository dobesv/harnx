//! Large tool results compact inside the shared loop, not after losing a turn.
use super::*;
use harnx_core::event::{AgentEvent, AgentEventSink, SessionEvent};
use harnx_core::message::{Message, MessageContent, MessageRole};
use harnx_runtime::agent_loop::{run_agent_loop, AgentCallFn};
use harnx_runtime::test_utils::{MockClient, MockTurnBuilder};
use std::sync::Mutex;

struct LargeOutput;

#[async_trait]
impl Toolset for LargeOutput {
    fn name(&self) -> &str {
        "large"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "large_output".into(),
            description: "Return a large test result".into(),
            input_schema: json!({"type": "object"}),
            cancellation_guarantee: Default::default(),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: None,
            meta: None,
        }]
    }

    async fn invoke(
        &self,
        _: &str,
        args: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
    ) -> std::result::Result<serde_json::Value, ToolInvokeError> {
        let text = format!("round {} ", args["round"]).repeat(2500);
        Ok(json!({"content": [{"type": "text", "text": text}]}))
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<AgentEvent>>);

impl AgentEventSink for Events {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

type Requests = Arc<Mutex<Vec<Vec<Message>>>>;

fn model(requests: &Requests) -> AgentCallFn {
    let requests = requests.clone();
    Arc::new(move |input, config, _| {
        let messages = harnx_runtime::config::input::build_messages(input, config);
        let requests = requests.clone();
        Box::pin(async move {
            let mut requests = requests.lock().unwrap();
            let round = requests.len();
            requests.push(messages?);
            let calls = if round < 2 {
                vec![ToolCall::new(
                    "large_output".into(),
                    json!({"round": round}),
                    Some(format!("call-{round}")),
                    None,
                )]
            } else {
                vec![]
            };
            Ok(("Finished".into(), None, calls, Default::default()))
        })
    })
}

fn assert_compacted_request(requests: &Requests) {
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let last = requests.last().unwrap();
    assert_eq!(
        last.iter()
            .filter(|m| m.content.to_text() == "what time is it")
            .count(),
        1
    );
    let rounds: Vec<_> = last
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::ToolCalls(calls) => {
                Some(calls.tool_results[0].call.id.as_deref().unwrap())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        rounds,
        ["call-1"],
        "dropped results must not return from Input.tool_calls"
    );
}

fn assert_single_compaction(events: &Events) {
    let events = events.0.lock().unwrap();
    let lifecycle: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Session(SessionEvent::CompactingStarted { .. }) => Some("started"),
            AgentEvent::Session(SessionEvent::CompactingCompleted { .. }) => Some("completed"),
            AgentEvent::Session(SessionEvent::CompactingFailed { .. }) => Some("failed"),
            _ => None,
        })
        .collect();
    assert_eq!(lifecycle, ["started", "completed"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn large_tool_results_compact_mid_turn_and_finish_without_post_turn_compaction() -> Result<()>
{
    let Some(harness) = RoundHarness::start_with_toolset(LargeOutput, "____large").await? else {
        return Ok(());
    };
    let summarizer = Arc::new(
        MockClient::builder()
            .add_turn(MockTurnBuilder::new().add_text_chunk("Summary.").build())
            .build(),
    );
    let _mock = harnx_runtime::client::TestStateGuard::new(Some(summarizer.clone())).await;
    harness.config.write().stream = false;
    harness.config.write().compress_threshold = 11_000;
    let requests = Requests::default();
    let mut ctx = harness.loop_context();
    ctx.call_fn = Some(model(&requests));
    let events = Arc::new(Events::default());
    let result = harnx_core::sink::with_agent_event_sink(
        events.clone(),
        run_agent_loop(&ctx, harness.input()),
    )
    .await?;
    assert!(matches!(result, LoopResult::Completed));
    assert_compacted_request(&requests);
    assert_single_compaction(&events);
    assert_finished_session(&harness.config, &summarizer);
    Ok(())
}

fn assert_finished_session(config: &GlobalConfig, summarizer: &MockClient) {
    assert_eq!(
        summarizer.conversation_history().conversation_history.len(),
        1
    );
    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert!(!session.need_compress(guard.compress_threshold));
    assert_eq!(
        session.messages.last().unwrap().role,
        MessageRole::Assistant
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_overflow_after_tool_round_compacts_then_retries_without_restoring_results(
) -> Result<()> {
    let Some(harness) = RoundHarness::start_with_toolset(LargeOutput, "____large").await? else {
        return Ok(());
    };
    let mock = overflow_client();
    let _mock = harnx_runtime::client::TestStateGuard::new(Some(mock.clone())).await;
    harness.config.write().stream = false;
    harness.config.write().compress_threshold = 0;
    let mut ctx = harness.loop_context();
    ctx.call_fn = None;
    let events = Arc::new(Events::default());
    let result = harnx_core::sink::with_agent_event_sink(
        events.clone(),
        run_agent_loop(&ctx, harness.input()),
    )
    .await?;
    assert!(matches!(result, LoopResult::Completed));
    assert_single_compaction(&events);
    assert_overflow_retry(&mock);
    Ok(())
}

fn overflow_client() -> Arc<MockClient> {
    Arc::new(
        MockClient::builder()
            .add_turn(
                MockTurnBuilder::new()
                    .add_tool_call_with_id("large_output", json!({"round": 0}), "overflow-call-0")
                    .build(),
            )
            .add_turn(
                MockTurnBuilder::new()
                    .add_tool_call_with_id("large_output", json!({"round": 1}), "overflow-call-1")
                    .build(),
            )
            .add_turn(
                MockTurnBuilder::new()
                    .add_error(
                        harnx_core::error::LlmError {
                            status: 400,
                            message: "prompt is too long".into(),
                            retry_after: None,
                        }
                        .into(),
                    )
                    .build(),
            )
            .add_turn(MockTurnBuilder::new().add_text_chunk("Summary.").build())
            .add_turn(MockTurnBuilder::new().add_text_chunk("Finished").build())
            .build(),
    )
}

fn assert_overflow_retry(mock: &MockClient) {
    let history = mock.conversation_history();
    assert_eq!(history.conversation_history.len(), 5);
    let retry = &history.conversation_history[4].messages;
    assert_eq!(
        retry
            .iter()
            .filter(|message| message.content.to_text() == "what time is it")
            .count(),
        1
    );
    let rounds: Vec<_> = retry
        .iter()
        .filter_map(|message| match &message.content {
            MessageContent::ToolCalls(calls) => calls.tool_results[0].call.id.as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(
        rounds,
        ["overflow-call-1"],
        "archived round must not return from Input.tool_calls"
    );
}
