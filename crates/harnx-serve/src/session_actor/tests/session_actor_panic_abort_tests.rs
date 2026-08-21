//! Panic cleanup for active initial and resumed turns.

use super::*;
use harnx_core::tool::ToolCall;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::{oneshot, Notify};

struct DropCanary(Option<oneshot::Sender<()>>);

impl Drop for DropCanary {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

fn blocking_drop_canary_call_fn(
    leading_interrupt_rounds: usize,
) -> (AgentCallFn, Arc<Notify>, oneshot::Receiver<()>) {
    let round = Arc::new(AtomicUsize::new(0));
    let turn_started = Arc::new(Notify::new());
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let dropped_tx = Arc::new(std::sync::Mutex::new(Some(dropped_tx)));
    let call_fn: AgentCallFn = {
        let round = Arc::clone(&round);
        let turn_started = Arc::clone(&turn_started);
        Arc::new(move |_input, _config, _abort| {
            let round = Arc::clone(&round);
            let turn_started = Arc::clone(&turn_started);
            let dropped_tx = Arc::clone(&dropped_tx);
            Box::pin(async move {
                if round.fetch_add(1, Ordering::SeqCst) < leading_interrupt_rounds {
                    return Ok((
                        "approval required".to_string(),
                        None,
                        vec![ToolCall::new(
                            "harnx_agent_session_history_read".to_string(),
                            json!({}),
                            Some("resume-call".to_string()),
                            None,
                        )],
                        harnx_runtime::client::CompletionTokenUsage::default(),
                    ));
                }

                let dropped_tx = dropped_tx
                    .lock()
                    .expect("lock drop canary sender")
                    .take()
                    .expect("blocking turn should start once");
                let _canary = DropCanary(Some(dropped_tx));
                turn_started.notify_one();
                std::future::pending().await
            })
        })
    };
    (call_fn, turn_started, dropped_rx)
}

#[tokio::test]
async fn session_actor_panic_aborts_in_flight_turn() {
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");

    let (call_fn, turn_started, dropped_rx) = blocking_drop_canary_call_fn(0);
    let registry = registry_with_call_fn(call_fn);
    let handle = registry.get_or_spawn(key("plain", "panic-abort"));
    let _sub = subscribe(&handle).await;

    let result = prompt(&handle, "run until actor stops").await;
    assert!(matches!(result, PromptResult::Accepted { .. }));
    tokio::time::timeout(Duration::from_secs(2), turn_started.notified())
        .await
        .expect("timed out waiting for turn to start");

    handle
        .tx
        .send(SessionCommand::Panic)
        .await
        .expect("send panic command");
    tokio::time::timeout(Duration::from_secs(2), dropped_rx)
        .await
        .expect("timed out waiting for in-flight turn to be aborted")
        .expect("drop canary sender closed without notification");
}

#[tokio::test]
async fn session_actor_panic_aborts_in_flight_resume_turn() {
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent_with_front_matter(
        "plain",
        "model: openai:gpt-4o\nuse_tools: harnx_agent_session_history_read\nhooks:\n  entries:\n    - command: |\n        harnx-claude-compatible-hook-server --event PreToolUse --matcher '^harnx_agent_session_history_read$' -- printf '\"'\"'{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"approval needed\"}}'\"'\"''",
        "You are plain.",
    );

    let (call_fn, resume_turn_started, dropped_rx) = blocking_drop_canary_call_fn(1);
    let registry = registry_with_call_fn(call_fn);
    let handle = registry.get_or_spawn(key("plain", "panic-abort-resume"));
    let _sub = subscribe(&handle).await;

    let result = prompt(&handle, "interrupt before resume").await;
    assert!(matches!(result, PromptResult::Accepted { .. }));
    wait_for_state(&handle, "interrupted", |state| {
        matches!(state, SessionState::Interrupted { .. })
    })
    .await;

    let resumed = prompt_with_options(
        &handle,
        "interrupt before resume",
        SessionPromptOptions {
            resume: vec![InterruptResume {
                interrupt_id: "resume-call".to_string(),
                status: InterruptResumeStatus::Approved,
                payload: InterruptResumePayload {
                    approved: true,
                    reason: None,
                },
            }],
            ..Default::default()
        },
    )
    .await;
    assert!(matches!(resumed, PromptResult::Accepted { .. }));
    tokio::time::timeout(Duration::from_secs(2), resume_turn_started.notified())
        .await
        .expect("timed out waiting for resumed turn to start");

    handle
        .tx
        .send(SessionCommand::Panic)
        .await
        .expect("send panic command");
    tokio::time::timeout(Duration::from_secs(2), dropped_rx)
        .await
        .expect("timed out waiting for in-flight resumed turn to be aborted")
        .expect("drop canary sender closed without notification");
}
