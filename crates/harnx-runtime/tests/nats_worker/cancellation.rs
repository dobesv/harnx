use super::*;
use harnx_execution_control::{CancelDisposition, CancelRequest};

#[path = "cancellation/hierarchy.rs"]
mod hierarchy;

#[path = "cancellation/recovery.rs"]
mod recovery;

async fn session(url: &str, id: &str) -> Result<NatsSession> {
    let client = async_nats::connect(url).await?;
    NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            initializer: SessionInitializer::inline(
                "",
                Default::default(),
                SessionOverrides::default(),
            ),
            session_id: Some(id.into()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        client.clone(),
        async_nats::jetstream::new(client),
        create_abort_signal(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_before_worker_start_is_durable_and_never_calls_model() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let session = session(server.url(), "cancel-before-activation").await?;
    session.enqueue_text("must never reach the model").await?;
    let receipt = session.request_cancel(CancelRequest::default()).await?;
    assert_eq!(receipt.disposition, CancelDisposition::Requested);
    assert!(session
        .enqueue_text("rejected before log append")
        .await
        .is_err());
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "late-worker",
        fold_capture_call_fn(calls.clone(), Arc::new(AsyncMutex::new(Vec::new()))),
    )
    .await;
    let status = session
        .wait_for_cancel(&receipt, tokio::time::Instant::now() + CI_SAFE_TIMEOUT)
        .await?;
    assert_eq!(status.disposition, CancelDisposition::Cancelled);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn durable_watch_cancels_streaming_without_a_core_command() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let entered = Arc::new(Notify::new());
    let aborted = Arc::new(AtomicBool::new(false));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "watch-only-worker",
        Arc::new({
            let entered = entered.clone();
            let aborted = aborted.clone();
            move |_, _, abort| {
                let entered = entered.clone();
                let aborted = aborted.clone();
                Box::pin(async move {
                    entered.notify_one();
                    harnx_core::abort::wait_abort_signal(&abort).await;
                    aborted.store(true, Ordering::SeqCst);
                    anyhow::bail!("model stopped after cancellation")
                })
            }
        }),
    )
    .await;
    let session = session(server.url(), "watch-only-cancel").await?;
    session
        .enqueue_text("stream until durable cancellation")
        .await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, entered.notified()).await?;
    // Deliberately bypass the Core NATS latency path.
    let receipt = session
        .execution_store()
        .request_cancel(session.session_id(), CancelRequest::default())
        .await?;
    let status = session
        .wait_for_cancel(&receipt, tokio::time::Instant::now() + CI_SAFE_TIMEOUT)
        .await?;
    assert_eq!(status.disposition, CancelDisposition::Cancelled);
    assert!(aborted.load(Ordering::SeqCst));
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
