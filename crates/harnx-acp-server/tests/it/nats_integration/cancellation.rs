//! Mid-turn cancellation behavior and durable worker observation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{CancelNotification, StopReason};
use anyhow::{Context, Result};
use harnx_runtime::AgentCallFn;
use tokio::sync::Semaphore;

use super::support::*;

struct AbortObserver {
    observed: Arc<AtomicBool>,
    abort: harnx_core::abort::AbortSignal,
}

impl Drop for AbortObserver {
    fn drop(&mut self) {
        if self.abort.aborted() {
            self.observed.store(true, Ordering::SeqCst);
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mid_turn_cancel_stops_turn() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = test_config(&server.url);
    let started = Arc::new(Semaphore::new(0));
    let worker_observed_cancel = Arc::new(AtomicBool::new(false));
    let call_fn: AgentCallFn = {
        let started = Arc::clone(&started);
        let worker_observed_cancel = Arc::clone(&worker_observed_cancel);
        Arc::new(move |_input, _config, abort| {
            let started = Arc::clone(&started);
            let worker_observed_cancel = Arc::clone(&worker_observed_cancel);
            Box::pin(async move {
                let _guard = AbortObserver {
                    observed: Arc::clone(&worker_observed_cancel),
                    abort: abort.clone(),
                };
                started.add_permits(1);
                harnx_core::abort::wait_abort_signal(&abort).await;
                worker_observed_cancel.store(true, Ordering::SeqCst);
                anyhow::bail!("cancelled test model call")
            })
        })
    };
    let worker = spawn_worker(Arc::clone(&config), call_fn).await?;
    let agent = new_agent(&config);
    let session_id = initialize_and_create_session(&agent).await?;

    let mut prompt = tokio::spawn({
        let agent = Arc::clone(&agent);
        let session_id = session_id.clone();
        async move { agent.prompt(text_prompt(session_id, "wait")).await }
    });
    tokio::select! {
        permit = started.acquire() => permit.context("started semaphore closed")?.forget(),
        result = &mut prompt => anyhow::bail!("prompt ended before model call started: {result:?}"),
        _ = tokio::time::sleep(TEST_TIMEOUT) => anyhow::bail!("worker model call did not start"),
    }

    agent
        .cancel(CancelNotification::new(session_id))
        .await
        .context("cancel prompt")?;
    let response = tokio::time::timeout(TEST_TIMEOUT, prompt)
        .await
        .context("cancelled prompt did not return")???;
    assert_eq!(response.stop_reason, StopReason::Cancelled);
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !worker_observed_cancel.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("worker did not observe durable cancellation")?;

    worker.abort();
    let _ = worker.await;
    Ok(())
}
