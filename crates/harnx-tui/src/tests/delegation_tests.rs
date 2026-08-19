use super::test_config_with_mock_client_and_agent;
use crate::test_utils::TuiTestHarness;
use crate::types::{PendingMessage, ToolCallBody, TranscriptItem};
use harnx_runtime::client::TestStateGuard;
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::test_utils::{MockClient, MockTurnBuilder};
use std::sync::Arc;
use std::time::Duration;

fn handoff_mock_client(config: &GlobalConfig) -> Arc<MockClient> {
    Arc::new(
        MockClient::builder()
            .global_config(config.clone())
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("I'll delegate this task.")
                    .add_tool_call(
                        "specialist_session_handoff",
                        serde_json::json!({
                            "session_id": "handoff-session-1",
                            "prompt": "Please help with this task"
                        }),
                    )
                    .build(),
            )
            .build(),
    )
}

async fn drain_ready_events(harness: &mut TuiTestHarness) {
    while let Ok(event) = harness.tui().event_rx.try_recv() {
        harness.tui().handle_tui_event(event).await.unwrap();
    }
}

fn handoff_body(harness: &mut TuiTestHarness) -> Option<ToolCallBody> {
    harness
        .tui()
        .app
        .transcript
        .iter()
        .find_map(|item| match item {
            TranscriptItem::ToolCall {
                tool_name, body, ..
            } if tool_name == "specialist_session_handoff" => body.clone(),
            _ => None,
        })
}

async fn wait_for_handoff_body(harness: &mut TuiTestHarness) -> ToolCallBody {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        drain_ready_events(harness).await;
        if let Some(body) = handoff_body(harness) {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "transcript never received specialist_session_handoff: {:?}",
            harness.tui().app.transcript
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn specialist_handoff_tool_appears_in_transcript() {
    let config = test_config_with_mock_client_and_agent("coordinator", Some("delegation-test"));
    let mock_client = handoff_mock_client(&config);
    let _guard = TestStateGuard::new(Some(mock_client.clone())).await;

    let mut harness = TuiTestHarness::with_config(config).await;
    harness.tui().clear_transcript();
    harness.tui().app.transcript.push(TranscriptItem::UserText {
        timestamp: None,
        text: "Help me".to_string(),
        seq: None,
    });
    harness
        .tui()
        .start_prompt(PendingMessage {
            text: "Help me".to_string(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
        .unwrap();

    harness
        .sync()
        .wait_until_mock_exhausted(mock_client.as_ref(), Duration::from_secs(5))
        .await
        .unwrap();

    let body = wait_for_handoff_body(&mut harness).await;
    let body = match body {
        ToolCallBody::Yaml(body) | ToolCallBody::Markdown(body) => body,
    };
    assert!(body.contains("handoff-session-1"));
    harness.drain_and_settle().await.unwrap();
}
