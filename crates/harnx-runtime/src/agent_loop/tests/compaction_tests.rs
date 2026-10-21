use super::*;
use harnx_core::message::{Message, MessageContent};

fn config_with_history() -> GlobalConfig {
    let config = replay_test_config(&TempDir::new().unwrap());
    let mut guard = config.write();
    guard.compress_threshold = 1;
    guard.session.as_mut().unwrap().messages = vec![Message::new(
        MessageRole::User,
        MessageContent::Text("old history ".repeat(100)),
    )];
    drop(guard);
    config
}

#[tokio::test]
async fn preflight_never_compacts_fresh_user_input_or_disabled_sessions() {
    let config = config_with_history();
    let call_fn: AgentCallFn = Arc::new(|_, _, _| unreachable!("no model request expected"));
    let ctx = make_test_context(config.clone(), call_fn, no_op_tool_round());
    let mut input = crate::config::input::from_str(&config, "new user input", None);
    super::super::compaction::preflight(&ctx, &mut input)
        .await
        .unwrap();
    assert!(config
        .read()
        .session
        .as_ref()
        .unwrap()
        .compaction_summary
        .is_none());
    config.write().compress_threshold = 0;
    let mut input = input.merge_tool_results("tool prose".into(), None, vec![]);
    super::super::compaction::preflight(&ctx, &mut input)
        .await
        .unwrap();
    assert!(!config.read().session.as_ref().unwrap().compressing());
}

#[tokio::test]
async fn preflight_skips_sessionless_tool_continuations() {
    let config = config_with_history();
    let call_fn: AgentCallFn = Arc::new(|_, _, _| unreachable!("no model request expected"));
    let ctx = make_test_context(config.clone(), call_fn, no_op_tool_round());
    let mut input = crate::config::input::from_str(&config, "original prompt", None)
        .merge_tool_results("tool prose".into(), None, vec![]);
    input.with_session = false;

    super::super::compaction::preflight(&ctx, &mut input)
        .await
        .unwrap();

    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert!(input.tool_calls.is_some());
    assert!(session.compaction_summary.is_none());
    assert!(!session.compressing());
}

#[tokio::test]
async fn cancelled_mid_turn_compaction_releases_single_flight_flag() {
    let config = config_with_history();
    let call_fn: AgentCallFn = Arc::new(|_, _, _| unreachable!("no model request expected"));
    let ctx = make_test_context(config.clone(), call_fn, no_op_tool_round());
    ctx.abort_signal.set_ctrlc();
    assert!(super::super::compaction::compact_in_turn(&ctx)
        .await
        .is_err());
    assert!(!config.read().session.as_ref().unwrap().compressing());
}

#[tokio::test]
async fn preflight_compacts_at_most_once_even_when_request_remains_over_threshold() {
    use crate::test_utils::{MockClient, MockTurnBuilder};

    let config = config_with_history();
    config.write().stream = false;
    let summarizer = Arc::new(
        MockClient::builder()
            .add_turn(MockTurnBuilder::new().add_text_chunk("Summary.").build())
            .build(),
    );
    let _mock = crate::client::TestStateGuard::new(Some(summarizer.clone())).await;
    let call_fn: AgentCallFn = Arc::new(|_, _, _| unreachable!("no main model request expected"));
    let ctx = make_test_context(config.clone(), call_fn, no_op_tool_round());
    let mut input = crate::config::input::from_str(&config, "original prompt", None)
        .merge_tool_results("tool prose".into(), None, vec![]);

    super::super::compaction::preflight(&ctx, &mut input)
        .await
        .unwrap();

    assert_eq!(
        summarizer.conversation_history().conversation_history.len(),
        1
    );
    let guard = config.read();
    let session = guard.session.as_ref().unwrap();
    assert!(session.compaction_summary.is_some());
    assert!(!session.compressing());
    let threshold = guard.compress_threshold;
    drop(guard);
    let messages = crate::config::input::build_messages(&input, &config).unwrap();
    assert!(
        input
            .agent()
            .model()
            .estimate_request_tokens(&messages, &[])
            > threshold
    );
    assert!(input.session_input_start.is_none());
}
