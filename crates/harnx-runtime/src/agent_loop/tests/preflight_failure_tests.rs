//! Failed heuristic compaction must not turn a viable tool continuation into a failure.
use super::*;
use crate::test_utils::{MockClient, MockTurn, MockTurnBuilder};
use harnx_core::event::SessionEvent;
use harnx_core::message::{Message, MessageContent, MessageContentToolCalls};
use tokio::sync::Notify;

fn continuation() -> (AgentLoopContext, Input) {
    let config = replay_test_config(&TempDir::new().unwrap());
    let results = vec![ToolResult::new(
        ToolCall::new(
            "read".into(),
            serde_json::json!({}),
            Some("read-1".into()),
            None,
        ),
        serde_json::json!({"text": "completed tool result"}),
    )];
    {
        let mut guard = config.write();
        guard.stream = false;
        guard.compress_threshold = 1;
        guard.session.as_mut().unwrap().messages = vec![
            Message::new(
                MessageRole::User,
                MessageContent::Text("Previous question".into()),
            ),
            Message::new(
                MessageRole::Assistant,
                MessageContent::Text("old context ".repeat(1000)),
            ),
            Message::new(
                MessageRole::User,
                MessageContent::Text("Current question".into()),
            ),
            Message::new(
                MessageRole::Tool,
                MessageContent::ToolCalls(MessageContentToolCalls::new(
                    results.clone(),
                    "round prose".into(),
                    None,
                )),
            ),
        ];
    }
    let input = crate::config::input::from_str(&config, "Current question", None)
        .merge_tool_results("round prose".into(), None, results);
    let mut ctx = make_test_context(
        config.clone(),
        Arc::new(|_, _, _| unreachable!()),
        no_op_tool_round(),
    );
    ctx.call_fn = None;
    // Isolate preflight attempts from normal post-turn maintenance.
    ctx.on_text_response = Some(Arc::new(move |output, _| {
        assert_eq!(output, "all done");
        config.write().compress_threshold = 0;
        Box::pin(async {})
    }));
    (ctx, input)
}

fn rejection(message: &str) -> MockTurn {
    MockTurnBuilder::new()
        .add_error(
            harnx_core::error::LlmError {
                status: 400,
                message: message.into(),
                retry_after: None,
            }
            .into(),
        )
        .build()
}

fn history(config: &GlobalConfig) -> Vec<Message> {
    config.read().session.as_ref().unwrap().messages.clone()
}

fn assert_uncompacted(config: &GlobalConfig, original: &[Message]) {
    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert!(!session.compressing());
    assert!(session.compaction_summary.is_none());
    assert_eq!(session.messages.len(), original.len());
    for (actual, expected) in session.messages.iter().zip(original) {
        assert_eq!(actual.role, expected.role);
        assert_eq!(actual.content, expected.content);
        assert_eq!(actual.id, expected.id);
        assert_eq!(actual.log_seq, expected.log_seq);
    }
}

fn assert_contains_history(request: &[Message], original: &[Message]) {
    for expected in original {
        assert!(request.iter().any(|message| message.role == expected.role && message.content == expected.content),
            "missing original message: {expected:?}");
    }
}

fn assert_compactions(sink: &CollectingSink, expected: &[&str]) {
    let events = sink.events.lock().unwrap();
    let lifecycle: Vec<_> = events
        .iter()
        .filter_map(|(event, _)| match event {
            AgentEvent::Session(SessionEvent::CompactingStarted { .. }) => Some("started"),
            AgentEvent::Session(SessionEvent::CompactingCompleted { .. }) => Some("completed"),
            AgentEvent::Session(SessionEvent::CompactingFailed { .. }) => Some("failed"),
            _ => None,
        })
        .collect();
    assert_eq!(lifecycle, expected);
}

#[tokio::test]
async fn failed_preflight_completes_turn_with_uncompacted_request_once() {
    let (ctx, input) = continuation();
    let original = history(&ctx.config);
    let mock = Arc::new(
        MockClient::builder()
            .add_turn(rejection("invalid summarizer request"))
            .add_turn(MockTurn::with_text("all done"))
            .build(),
    );
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let sink = Arc::new(CollectingSink::default());
    let result = harnx_core::sink::with_agent_event_sink(sink.clone(), run_agent_loop(&ctx, input))
        .await
        .unwrap();
    assert!(matches!(result, LoopResult::Completed));
    let requests = mock.conversation_history();
    assert_eq!(
        requests.conversation_history.len(),
        2,
        "one failed summarizer and one main request, no preflight loop"
    );
    assert_contains_history(&requests.conversation_history[1].messages, &original);
    assert_compactions(&sink, &["started", "failed"]);
    let events = sink.events.lock().unwrap();
    assert!(events.iter().any(|(event, _)| matches!(event,
        AgentEvent::Session(SessionEvent::CompactingFailed { error, .. }) if error.contains("invalid summarizer request"))));
    drop(events);
    assert_contains_history(&history(&ctx.config), &original);
    assert!(!ctx.config.read().session.as_ref().unwrap().compressing());
    assert!(ctx
        .config
        .read()
        .session
        .as_ref()
        .unwrap()
        .compaction_summary
        .is_none());
}

#[tokio::test]
async fn failed_preflight_preserves_history_and_prepared_cursor() {
    let (ctx, mut input) = continuation();
    let original = history(&ctx.config);
    // A reused cursor must remain valid, even though ordinary preparation runs after preflight.
    input.session_input_start = Some(original.len());
    let mock = Arc::new(
        MockClient::builder()
            .add_turn(rejection("invalid summarizer request"))
            .build(),
    );
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    super::super::compaction::preflight(&ctx, &mut input)
        .await
        .unwrap();
    assert_uncompacted(&ctx.config, &original);
    assert_eq!(input.session_input_start, Some(original.len()));
    let request = crate::config::input::build_messages(&input, &ctx.config).unwrap();
    assert_contains_history(&request, &original);
    assert_eq!(mock.conversation_history().conversation_history.len(), 1);
}

fn gated_failure(reached: Arc<Notify>, release: Arc<Notify>) -> MockTurn {
    MockTurnBuilder::new()
        .add_gate(reached, release)
        .add_error(
            harnx_core::error::LlmError {
                status: 400,
                message: "invalid summarizer request".into(),
                retry_after: None,
            }
            .into(),
        )
        .build()
}

async fn wait_for_summarizer(reached: &Notify) {
    tokio::time::timeout(std::time::Duration::from_secs(30), reached.notified())
        .await
        .expect("summarizer must reach the gate");
}

#[tokio::test]
async fn interrupt_during_preflight_stops_turn_without_main_request() {
    let (ctx, input) = continuation();
    ctx.config.write().stream = true;
    let original = history(&ctx.config);
    let reached = Arc::new(Notify::new());
    let mock = Arc::new(
        MockClient::builder()
            .add_turn(gated_failure(reached.clone(), Arc::new(Notify::new())))
            .build(),
    );
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let sink = Arc::new(CollectingSink::default());
    let run = harnx_core::sink::with_agent_event_sink(sink.clone(), run_agent_loop(&ctx, input));
    let interrupt = async {
        wait_for_summarizer(&reached).await;
        assert!(ctx.config.read().session.as_ref().unwrap().compressing());
        ctx.abort_signal.set_ctrlc();
    };
    let (result, ()) = tokio::join!(run, interrupt);
    assert!(result
        .err()
        .unwrap()
        .to_string()
        .contains("interrupted at compaction-failed"));
    assert_eq!(mock.conversation_history().conversation_history.len(), 1);
    assert_uncompacted(&ctx.config, &original);
    assert_compactions(&sink, &["started", "failed"]);
}

#[tokio::test]
async fn deadline_expiring_during_failed_preflight_stops_turn_without_abort_signal() {
    use crate::nats_session_metadata::{
        InvocationIdentity, RunIdentity, RunLimitsPolicySource, RunLimitsRecord,
    };
    let (ctx, input) = continuation();
    let now = chrono::Utc::now();
    {
        let mut guard = ctx.config.write();
        guard.stream = true;
        guard.run_context = Some(RunLimitsRecord {
            run_id: RunIdentity::new(),
            invocation_id: InvocationIdentity::new(),
            admitted_at: now,
            deadline: Some(now + chrono::Duration::hours(1)),
            policy_source: RunLimitsPolicySource::GlobalDefault,
            parent_invocation: None,
        });
    }
    let original = history(&ctx.config);
    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mock = Arc::new(
        MockClient::builder()
            .add_turn(gated_failure(reached.clone(), release.clone()))
            .build(),
    );
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let sink = Arc::new(CollectingSink::default());
    let run = harnx_core::sink::with_agent_event_sink(sink.clone(), run_agent_loop(&ctx, input));
    let expire = async {
        wait_for_summarizer(&reached).await;
        ctx.config.write().run_context.as_mut().unwrap().deadline =
            Some(now - chrono::Duration::seconds(1));
        release.notify_one();
    };
    let (result, ()) = tokio::join!(run, expire);
    let error = result.err().unwrap();
    assert!(error
        .downcast_ref::<crate::nats_session_metadata::run_limits::DeadlineExpired>()
        .is_some());
    assert!(!ctx.abort_signal.aborted());
    assert_eq!(mock.conversation_history().conversation_history.len(), 1);
    assert_uncompacted(&ctx.config, &original);
    assert_compactions(&sink, &["started", "failed"]);
}

#[tokio::test]
async fn overflow_after_failed_preflight_forces_compaction_and_retries_once() {
    let (ctx, input) = continuation();
    let original = history(&ctx.config);
    let mock = Arc::new(
        MockClient::builder()
            .add_turn(rejection("invalid summarizer request"))
            .add_turn(rejection("context_length_exceeded"))
            .add_turn(MockTurn::with_text("Summary."))
            .add_turn(MockTurn::with_text("all done"))
            .build(),
    );
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let sink = Arc::new(CollectingSink::default());
    let result = harnx_core::sink::with_agent_event_sink(sink.clone(), run_agent_loop(&ctx, input))
        .await
        .unwrap();
    assert!(matches!(result, LoopResult::Completed));
    let requests = mock.conversation_history();
    assert_eq!(requests.conversation_history.len(), 4);
    assert_contains_history(&requests.conversation_history[1].messages, &original);
    let retry = &requests.conversation_history[3].messages;
    assert!(!retry
        .iter()
        .any(|message| message.content.to_text().contains("old context")));
    assert_eq!(
        retry
            .iter()
            .filter(|message| message.content.to_text() == "Current question")
            .count(),
        1
    );
    assert_compactions(&sink, &["started", "failed", "started", "completed"]);
    assert!(!ctx.config.read().session.as_ref().unwrap().compressing());
}

#[tokio::test]
async fn failed_forced_compaction_after_failed_preflight_still_fails_clearly() {
    let (ctx, input) = continuation();
    let mock = Arc::new(
        MockClient::builder()
            .add_turn(rejection("invalid summarizer request"))
            .add_turn(rejection("context_length_exceeded"))
            .add_turn(rejection("forced summarizer failure"))
            .add_turn(MockTurn::with_text("must not retry the main request"))
            .build(),
    );
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let sink = Arc::new(CollectingSink::default());
    let error = harnx_core::sink::with_agent_event_sink(sink.clone(), run_agent_loop(&ctx, input))
        .await
        .err()
        .unwrap();
    let diagnostic = format!("{error:#}");
    assert!(
        diagnostic.contains("compaction was tried but failed"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("forced summarizer failure"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("context_length_exceeded"),
        "{diagnostic}"
    );
    assert_eq!(mock.conversation_history().conversation_history.len(), 3);
    assert_compactions(&sink, &["started", "failed", "started", "failed"]);
    assert!(!ctx.config.read().session.as_ref().unwrap().compressing());
}
