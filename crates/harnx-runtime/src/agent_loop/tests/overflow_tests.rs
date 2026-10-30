//! Real completion/retry engine and compactor, with one scripted client for both.
use super::*;
use crate::test_utils::{MockClient, MockTurn, MockTurnBuilder};
use harnx_core::error::LlmError;
use harnx_core::message::{Message, MessageContent};

#[path = "overflow_tests/fallback.rs"]
mod fallback;

fn config_with_old_history() -> GlobalConfig {
    let config = replay_test_config(&TempDir::new().unwrap());
    let mut guard = config.write();
    guard.stream = false;
    guard.compress_threshold = 0; // Overflow recovery ignores automatic thresholds.
    let session = guard.session.as_mut().unwrap();
    session.messages = vec![Message::new(
        MessageRole::User,
        MessageContent::Text("Previous request".into()),
    )];
    for _ in 0..8 {
        session.messages.push(Message::new(
            MessageRole::Assistant,
            MessageContent::Text("archived context ".repeat(1000)),
        ));
    }
    drop(guard);
    config
}

fn overflow_turn() -> MockTurn {
    MockTurnBuilder::new()
        .add_error(
            anyhow::Error::new(LlmError {
                status: 400,
                message: "context_length_exceeded".into(),
                retry_after: None,
            })
            .context("provider request failed"),
        )
        .build()
}

fn client(last: MockTurn) -> Arc<MockClient> {
    Arc::new(
        MockClient::builder()
            .add_turn(overflow_turn())
            .add_turn(MockTurn::with_text("Summary."))
            .add_turn(last)
            .build(),
    )
}

fn context(config: GlobalConfig) -> AgentLoopContext {
    let mut ctx = make_test_context(
        config,
        Arc::new(|_, _, _| unreachable!()),
        no_op_tool_round(),
    );
    ctx.call_fn = None;
    ctx
}

fn texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| message.content.to_text())
        .collect()
}

fn assert_recovered_requests(mock: &MockClient, prompt: &str) {
    let history = mock.conversation_history();
    assert_eq!(
        history.conversation_history.len(),
        3,
        "two main requests and one summarizer"
    );
    let retry = texts(&history.conversation_history[2].messages);
    assert_eq!(
        retry.iter().filter(|text| text.as_str() == prompt).count(),
        1
    );
    assert!(!retry.iter().any(|text| text.contains("archived context")));
}

fn assert_compaction_lifecycle(sink: &CollectingSink) {
    let events = sink.events.lock().unwrap();
    let lifecycle: Vec<_> = events
        .iter()
        .filter_map(|(event, _)| match event {
            AgentEvent::Session(harnx_core::event::SessionEvent::CompactingStarted { .. }) => {
                Some("started")
            }
            AgentEvent::Session(harnx_core::event::SessionEvent::CompactingCompleted {
                ..
            }) => Some("completed"),
            AgentEvent::Session(harnx_core::event::SessionEvent::CompactingFailed { .. }) => {
                Some("failed")
            }
            _ => None,
        })
        .collect();
    assert_eq!(lifecycle, ["started", "completed"]);
}

#[tokio::test]
async fn first_request_overflow_compacts_and_retries_once_preserving_prompt_verbatim() {
    let config = config_with_old_history();
    let mock = client(MockTurn::with_text("all done"));
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let ctx = context(config.clone());
    let input = crate::config::input::from_str(&config, "Current question", None);
    let sink = Arc::new(CollectingSink::default());
    let result = harnx_core::sink::with_agent_event_sink(sink.clone(), run_agent_loop(&ctx, input))
        .await
        .unwrap();
    assert!(matches!(result, LoopResult::Completed));
    assert_recovered_requests(&mock, "Current question");
    assert_compaction_lifecycle(&sink);
}

#[tokio::test]
async fn overflow_twice_fails_with_compaction_context_without_a_third_attempt() {
    let config = config_with_old_history();
    let mock = client(overflow_turn());
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let ctx = context(config.clone());
    let input = crate::config::input::from_str(&config, "Current question", None);
    let error = run_agent_loop(&ctx, input).await.err().unwrap();
    assert!(
        error.to_string().contains("compaction was tried"),
        "{error:#}"
    );
    assert!(harnx_engine::retry::find_llm_error(&error)
        .unwrap()
        .is_context_overflow());
    assert_recovered_requests(&mock, "Current question");
}

#[tokio::test]
async fn overflow_rebases_prepared_cursor_without_repersisting_or_losing_request_patch() {
    let config = config_with_old_history();
    let mock = client(MockTurn::with_text("all done"));
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let ctx = context(config.clone());
    let mut input = crate::config::input::from_str(&config, "Current question", None);
    input.set_patched_text(Some("Request-only patch".into()));
    prepare_round_input(&ctx, &mut input, false).await.unwrap();
    let old_start = input.session_input_start.unwrap();
    super::super::overflow::call(&ctx, &mut input)
        .await
        .unwrap();
    assert!(input.session_input_start.unwrap() < old_start);
    assert_recovered_requests(&mock, "Request-only patch");
    assert_eq!(
        texts(&config.read().session.as_ref().unwrap().messages),
        ["Current question"]
    );
}

#[tokio::test]
async fn overflow_keeps_already_durable_worker_prompt_even_above_keep_budget() {
    let config = config_with_old_history();
    let prompt = "Keep this request verbatim. ".repeat(2500);
    let mut selected = vec![
        Message::new(
            MessageRole::User,
            MessageContent::Text("First admitted question".into()),
        ),
        Message::new(MessageRole::User, MessageContent::Text(prompt.clone())),
    ];
    selected[0].id = Some("first-admission".into());
    selected[1].id = Some("second-admission".into());
    config
        .write()
        .session
        .as_mut()
        .unwrap()
        .messages
        .extend(selected.clone());
    let mock = client(MockTurn::with_text("all done"));
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let ctx = context(config.clone());
    let mut input = crate::config::input::from_str(&config, &prompt, None);
    input.skip_user_log_append = true;
    input.durable_user_messages = selected;
    let result = run_agent_loop(&ctx, input).await.unwrap();
    assert!(matches!(result, LoopResult::Completed));
    assert_recovered_requests(&mock, &prompt);
    assert_recovered_requests(&mock, "First admitted question");
    let active = texts(&config.read().session.as_ref().unwrap().messages);
    assert_eq!(
        active
            .iter()
            .filter(|text| **text == prompt || text.as_str() == "First admitted question")
            .cloned()
            .collect::<Vec<_>>(),
        ["First admitted question".to_owned(), prompt]
    );
}

#[tokio::test]
async fn streaming_first_request_overflow_compacts_then_continues_streaming() {
    let config = config_with_old_history();
    config.write().stream = true;
    let mock = client(MockTurn::with_text("all done"));
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let ctx = context(config.clone());
    let input = crate::config::input::from_str(&config, "Current question", None);
    let result = run_agent_loop(&ctx, input).await.unwrap();
    assert!(matches!(result, LoopResult::Completed));
    assert_recovered_requests(&mock, "Current question");
}

#[tokio::test]
async fn overflow_preserves_queued_input_and_timing_identity_once() {
    let config = config_with_old_history();
    let mock = client(MockTurn::with_text("all done"));
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let ctx = context(config.clone());
    let input = crate::config::input::from_str(&config, "Original input", None).merge_tool_results(
        "round prose".into(),
        None,
        vec![],
    );
    let mut input = begin_agent_loop(&ctx, input);
    input.set_injected_user_text("Queued question".into());
    prepare_round_input(&ctx, &mut input, false).await.unwrap();
    let timing = input.timing_context.clone().expect("initial timing notice");
    super::super::overflow::call(&ctx, &mut input)
        .await
        .unwrap();
    assert_recovered_requests(&mock, "Queued question");
    let retry = texts(&mock.conversation_history().conversation_history[2].messages);
    assert_eq!(
        retry
            .iter()
            .filter(|text| **text == timing.message.content.to_text())
            .count(),
        1
    );
    assert!(!retry.iter().any(|text| text == "Original input"));
}

#[tokio::test]
async fn overflow_with_only_a_new_prompt_fails_without_summarizing_that_prompt() {
    let config = config_with_old_history();
    config.write().session.as_mut().unwrap().messages.clear();
    let mock = Arc::new(MockClient::builder().add_turn(overflow_turn()).build());
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let ctx = context(config.clone());
    let input = crate::config::input::from_str(&config, "Only prompt", None);
    let error = run_agent_loop(&ctx, input).await.err().unwrap();
    assert!(format!("{error:#}").contains("Nothing to compact"));
    assert_eq!(mock.conversation_history().conversation_history.len(), 1);
    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    let prompts: Vec<_> = session
        .messages
        .iter()
        .filter(|message| crate::config::is_turn_prompt(message))
        .map(|message| message.content.to_text())
        .collect();
    assert_eq!(
        (prompts, session.compaction_summary.as_ref()),
        (vec!["Only prompt".to_owned()], None)
    );
}

#[tokio::test]
async fn non_overflow_bad_request_does_not_compact() {
    let config = config_with_old_history();
    let turn = MockTurnBuilder::new()
        .add_error(
            LlmError {
                status: 400,
                message: "invalid tool schema".into(),
                retry_after: None,
            }
            .into(),
        )
        .build();
    let mock = Arc::new(MockClient::builder().add_turn(turn).build());
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let ctx = context(config.clone());
    let input = crate::config::input::from_str(&config, "Current question", None);
    assert!(run_agent_loop(&ctx, input).await.is_err());
    assert_eq!(mock.conversation_history().conversation_history.len(), 1);
    assert!(config
        .read()
        .session
        .as_ref()
        .unwrap()
        .compaction_summary
        .is_none());
}

#[tokio::test]
async fn overflow_resummarizes_prior_summary_when_new_prompt_is_only_active_input() {
    let config = config_with_old_history();
    {
        let mut guard = config.write();
        let session = guard.session.as_mut().unwrap();
        session.messages.clear();
        session.compaction_summary = Some("Prior conversation context ".repeat(1000));
    }
    let mock = client(MockTurn::with_text("all done"));
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let ctx = context(config.clone());
    let input = crate::config::input::from_str(&config, "Current question", None);
    let result = run_agent_loop(&ctx, input).await.unwrap();
    assert!(matches!(result, LoopResult::Completed));
    assert_recovered_requests(&mock, "Current question");
    let requests = mock.conversation_history();
    assert!(texts(&requests.conversation_history[1].messages)
        .iter()
        .any(|text| text.contains("Prior conversation context")));
}

#[tokio::test]
async fn sessionless_overflow_returns_original_error_without_compaction_or_retry() {
    let config = config_with_old_history();
    let original = texts(&config.read().session.as_ref().unwrap().messages);
    let mock = Arc::new(MockClient::builder().add_turn(overflow_turn()).build());
    let _mock = crate::client::TestStateGuard::new(Some(mock.clone())).await;
    let ctx = context(config.clone());
    let mut input = crate::config::input::from_str(&config, "Detached request", None);
    input.with_session = false;

    let error = super::super::overflow::call(&ctx, &mut input)
        .await
        .err()
        .unwrap();

    let diagnostic = format!("{error:#}");
    assert!(
        diagnostic.contains("provider request failed"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("compaction"), "{diagnostic}");
    let provider_error = harnx_engine::retry::find_llm_error(&error).unwrap();
    assert_eq!(provider_error.status, 400);
    assert_eq!(provider_error.message, "context_length_exceeded");
    assert_eq!(mock.conversation_history().conversation_history.len(), 1);
    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert_eq!(texts(&session.messages), original);
    assert!(session.compaction_summary.is_none());
    assert!(!session.compressing());
}

#[tokio::test]
async fn overflow_recovery_rejects_compaction_already_in_progress() {
    let config = config_with_old_history();
    config
        .write()
        .session
        .as_mut()
        .unwrap()
        .set_compressing(true);
    let original = texts(&config.read().session.as_ref().unwrap().messages);
    let ctx = context(config.clone());

    let error = super::super::compaction::compact_preserving(&ctx, Some(0))
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "session compaction already in progress during overflow recovery"
    );
    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert_eq!(texts(&session.messages), original);
    assert!(session.compaction_summary.is_none());
    assert!(
        session.compressing(),
        "the existing compactor still owns the flag"
    );
}
