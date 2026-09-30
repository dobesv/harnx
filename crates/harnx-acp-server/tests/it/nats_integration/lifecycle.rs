//! Turn admission and idle-touch lifecycle behavior.

use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{CancelNotification, PromptResponse, StopReason};
use anyhow::{Context, Result};
use harnx_runtime::AgentCallFn;
use tokio::sync::Semaphore;

use super::support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_prompt_on_same_session_is_rejected() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let worker = spawn_worker(
        Arc::clone(&config),
        gated_call_fn(Arc::clone(&started), Arc::clone(&release)),
    )
    .await?;
    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;

    let first_prompt = tokio::spawn({
        let agent = Arc::clone(&agent);
        let session_id = session_id.clone();
        async move { agent.prompt(text_prompt(session_id, "first")).await }
    });
    tokio::time::timeout(TEST_TIMEOUT, started.acquire())
        .await
        .context("first model call did not start")??
        .forget();

    let overlap_error = agent
        .prompt(text_prompt(session_id, "overlap"))
        .await
        .expect_err("overlapping turn must be rejected");
    assert!(
        overlap_error
            .to_string()
            .contains("already has an in-flight turn"),
        "unexpected overlap error: {overlap_error}"
    );

    release.add_permits(1);
    let response = tokio::time::timeout(TEST_TIMEOUT, first_prompt)
        .await
        .context("first prompt did not complete")???;
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    worker.abort();
    let _ = worker.await;
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn touch_updates_on_prompt_lifecycle() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let started = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let worker = spawn_worker(
        Arc::clone(&config),
        gated_call_fn(Arc::clone(&started), Arc::clone(&release)),
    )
    .await?;
    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;
    let session_key = session_id.0.to_string();
    let created_touch =
        session_touch(&agent, &session_key, "missing created session touch").await?;

    tokio::time::sleep(Duration::from_millis(5)).await;
    let mut prompt = spawn_prompt(Arc::clone(&agent), session_id.clone());
    wait_for_model_start(&started, &mut prompt).await?;
    let started_touch = session_touch(&agent, &session_key, "missing prompt start touch").await?;
    assert!(started_touch > created_touch);

    tokio::time::sleep(Duration::from_millis(5)).await;
    release.add_permits(1);
    let response = tokio::time::timeout(TEST_TIMEOUT, prompt)
        .await
        .context("prompt did not complete")???;
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    let completed_touch =
        session_touch(&agent, &session_key, "missing prompt completion touch").await?;
    assert!(completed_touch > started_touch);

    tokio::time::sleep(Duration::from_millis(5)).await;
    agent
        .cancel(CancelNotification::new(session_id))
        .await
        .context("cancel idle session")?;
    let cancelled_touch = session_touch(&agent, &session_key, "missing cancel touch").await?;
    assert!(cancelled_touch > completed_touch);

    worker.abort();
    let _ = worker.await;
    Ok(())
}
fn gated_call_fn(started: Arc<Semaphore>, release: Arc<Semaphore>) -> AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        Box::pin(async move {
            started.add_permits(1);
            release
                .acquire()
                .await
                .expect("release semaphore closed")
                .forget();
            Ok((
                "done".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}
async fn wait_for_model_start(
    started: &Semaphore,
    prompt: &mut tokio::task::JoinHandle<acp::Result<PromptResponse>>,
) -> Result<()> {
    tokio::select! {
        permit = started.acquire() => permit.context("started semaphore closed")?.forget(),
        result = prompt => anyhow::bail!("prompt ended before model call started: {result:?}"),
        _ = tokio::time::sleep(TEST_TIMEOUT) => anyhow::bail!("worker model call did not start"),
    }
    Ok(())
}

async fn session_touch(
    agent: &harnx_acp_server::HarnxAgent,
    session_id: &str,
    missing_context: &'static str,
) -> Result<Duration> {
    agent
        .session_last_touched(session_id)
        .await
        .context(missing_context)
}
