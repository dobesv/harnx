use super::test_config_with_mock_client_and_agent;
use crate::test_utils::{TestEnvironment, TuiTestHarness, ENV_LOCK};
use crate::types::PendingMessage;
use harnx_runtime::client::TestStateGuard;
use harnx_runtime::test_utils::{MockClient, MockTurnBuilder};
use std::sync::Arc;
use std::time::Duration;

/// Unit-test configs use `Default` routing with no broker handoff, so a
/// `__local__` lookup would start or join the developer's own shared broker
/// and wait for it to recover its store. A TUI with a session that runs a
/// tool call must not make one.
#[tokio::test(flavor = "multi_thread")]
async fn tool_call_turn_starts_no_shared_broker() {
    let _lock = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let _env = TestEnvironment::set(root.path());
    let config = test_config_with_mock_client_and_agent("test-agent", Some("no-broker-session"));
    let mock_client = Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_tool_call("search", serde_json::json!({"query": "test"}))
                    .build(),
            )
            .add_turn(MockTurnBuilder::new().add_text_chunk("Done.").build())
            .build(),
    );
    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;

    let mut harness = TuiTestHarness::with_config(config.clone()).await;
    harness
        .tui()
        .start_prompt(PendingMessage {
            text: "Search for it".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();
    harness
        .wait_until_screen_contains("Done.", Duration::from_secs(30))
        .await
        .unwrap();
    harness.drain_and_settle().await.unwrap();

    assert!(
        !harnx_core::config_paths::nats_runtime_dir().exists(),
        "the TUI started or joined the shared local broker"
    );
}
