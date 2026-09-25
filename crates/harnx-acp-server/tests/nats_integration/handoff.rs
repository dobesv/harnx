//! Committed-handoff fallback and source-session deactivation behavior.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::StopReason;
use anyhow::{Context, Result};
use harnx_core::event::{AgentEvent, SessionEvent, TurnEvent, TurnOutcome};
use harnx_runtime::AgentCallFn;
use tokio::sync::Semaphore;

use super::support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requested_committed_ended_handoff_uses_safe_fallback() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let requested = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let order = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = spawn_worker(
        Arc::clone(&config),
        handoff_call_fn(
            Arc::clone(&requested),
            Arc::clone(&release),
            Arc::clone(&order),
            Arc::clone(&calls),
        ),
    )
    .await?;
    let agent = new_agent(&config);
    let mut client = attach_test_client(Arc::clone(&agent), PermissionReply::Reject).await?;
    let session_id = initialize_and_create_session(&agent).await?;
    let session_key = session_id.0.to_string();
    let prompt = spawn_prompt(Arc::clone(&agent), session_id.clone());

    tokio::time::timeout(TEST_TIMEOUT, requested.acquire())
        .await
        .context("requested event was not emitted")??
        .forget();
    assert!(agent.session_handoff_target(&session_key).await.is_none());
    release.add_permits(1);
    let response = tokio::time::timeout(TEST_TIMEOUT, prompt)
        .await
        .context("handoff prompt did not finish")???;
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    let notification = tokio::time::timeout(TEST_TIMEOUT, client.notifications.recv())
        .await
        .context("handoff fallback notification timed out")?
        .context("ACP notification stream closed")?;
    assert_eq!(notification.session_id, session_id);
    let fallback = notification_text(notification).context("handoff fallback was not text")?;
    assert_handoff_fallback(&fallback);
    assert_eq!(
        *order.lock().expect("order mutex poisoned"),
        ["requested", "committed", "ended"]
    );
    assert_committed_target(&agent, &session_key).await?;

    let error = agent
        .prompt(text_prompt(session_id, "must not reach source"))
        .await
        .expect_err("handed-off source must reject prompts");
    assert_post_handoff_error(&error.to_string());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    worker.abort();
    let _ = worker.await;
    Ok(())
}

fn assert_handoff_fallback(message: &str) {
    for expected in [
        "agent `atlas`",
        "local session `target-session`",
        "cluster `prod`",
        "target is running independently",
        ".session atlas@prod target-session",
        "http://127.0.0.1:8000/",
    ] {
        assert!(
            message.contains(expected),
            "missing `{expected}`: {message}"
        );
    }
}

async fn assert_committed_target(
    agent: &harnx_acp_server::HarnxAgent,
    session_id: &str,
) -> Result<()> {
    let target = agent
        .session_handoff_target(session_id)
        .await
        .context("source was not marked handed off")?;
    assert_eq!(
        (target.cluster(), target.agent(), target.local_session_id()),
        ("prod", "atlas", "target-session")
    );
    Ok(())
}

fn assert_post_handoff_error(error: &str) {
    for expected in [
        "no longer active",
        "new prompt was not sent to the source session",
        "agent `atlas`",
        "local session `target-session`",
        "cluster `prod`",
        ".session atlas@prod target-session",
    ] {
        assert!(error.contains(expected), "missing `{expected}`: {error}");
    }
}
fn handoff_call_fn(
    requested: Arc<Semaphore>,
    release: Arc<Semaphore>,
    order: Arc<Mutex<Vec<&'static str>>>,
    calls: Arc<AtomicUsize>,
) -> AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let requested = Arc::clone(&requested);
        let release = Arc::clone(&release);
        let order = Arc::clone(&order);
        let calls = Arc::clone(&calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            emit_handoff_event(
                &order,
                "requested",
                AgentEvent::Turn(TurnEvent::HandoffRequested {
                    agent: "atlas@prod".to_string(),
                    session_id: Some("tentative-target".to_string()),
                }),
            );
            requested.add_permits(1);
            release.acquire().await.expect("release closed").forget();
            emit_handoff_event(
                &order,
                "committed",
                AgentEvent::Session(SessionEvent::HandoffCommitted {
                    agent: "atlas@prod".to_string(),
                    session_id: "target-session".to_string(),
                    handoff_tool_call_id: Some("handoff-call".to_string()),
                    after_seq: Some(42),
                }),
            );
            emit_handoff_event(
                &order,
                "ended",
                AgentEvent::Turn(TurnEvent::Ended {
                    outcome: TurnOutcome::default(),
                }),
            );
            Ok((
                "source handed off".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

fn emit_handoff_event(order: &Mutex<Vec<&'static str>>, label: &'static str, event: AgentEvent) {
    order.lock().expect("order mutex poisoned").push(label);
    harnx_core::sink::emit_agent_event(event);
}
