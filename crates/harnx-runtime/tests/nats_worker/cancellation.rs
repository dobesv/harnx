use super::*;
use anyhow::Context;
use harnx_core::session::SessionLogEntry as Entry;
use harnx_runtime::nats_session::InterruptOutcome;

/// Append a `Cancel` straight to a session's log, as `interrupt_session` does
/// but without its control-subject hint. Only the worker's own stream watcher
/// can notice this one.
async fn append_cancel(js: &async_nats::jetstream::Context, session: &str) -> Result<u64> {
    harnx_runtime::nats_session_log::NatsSessionLog::new(js.clone(), session)
        .append_event_async(&Entry::cancel_request(
            uuid::Uuid::now_v7().to_string(),
            "client:test".into(),
        ))
        .await
}

/// The sequence an interrupt accepted, or a failure naming what it saw
/// instead. A turn that was already over is not what these tests set up.
fn accepted_seq(outcome: InterruptOutcome) -> Result<u64> {
    match outcome {
        InterruptOutcome::Accepted { cancel_seq } => Ok(cancel_seq),
        other => anyhow::bail!("the turn must be interruptible, got {other:?}"),
    }
}

/// Poll a session's log until it carries a `Cancel`, returning its sequence.
async fn await_cancel_entry(js: &async_nats::jetstream::Context, session: &str) -> Result<u64> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            let entries = harnx_runtime::nats_session_log::NatsSessionLog::new(js.clone(), session)
                .load_events_latest_async()
                .await?;
            if let Some(seq) = entries
                .iter()
                .find_map(|(seq, entry)| matches!(entry, Entry::Cancel { .. }).then_some(*seq))
            {
                return Ok::<_, anyhow::Error>(seq);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?
}

#[path = "cancellation/hierarchy.rs"]
mod hierarchy;

#[path = "cancellation/recovery.rs"]
mod recovery;

#[path = "cancellation/prompt_persistence.rs"]
mod prompt_persistence;

#[path = "cancellation/overlap.rs"]
mod overlap;

#[path = "cancellation/restart_ordering.rs"]
mod restart_ordering;

async fn session(url: &str, id: &str) -> Result<NatsSession> {
    session_with_route(
        url,
        id,
        harnx_runtime::SessionActivationRoute::ClusterShared,
    )
    .await
}

async fn session_with_route(
    url: &str,
    id: &str,
    activation_route: harnx_runtime::SessionActivationRoute,
) -> Result<NatsSession> {
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
            activation_route,
        },
        client.clone(),
        async_nats::jetstream::new(client),
        create_abort_signal(),
    )
    .await
}

/// Acceptance is the `Cancel` append. The wind-up activation that follows it
/// is a best-effort wake-up, so a route with nowhere to publish cannot turn an
/// interrupted turn back into a running one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_cancellation_survives_recovery_activation_failure() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let session = session_with_route(
        server.url(),
        "cancel-invalid-recovery-route",
        harnx_runtime::SessionActivationRoute::WorkerTargeted {
            session_scope: "invalid".into(),
            worker_id: "worker".into(),
        },
    )
    .await?;
    session.enqueue_text("interrupt me").await?;

    let cancel_seq = accepted_seq(session.interrupt("client cancel").await?)?;

    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    assert_eq!(
        await_cancel_entry(&js, session.storage_key()).await?,
        cancel_seq
    );
    Ok(())
}

/// A session whose log already carries a `Cancel` when a worker first sees it
/// resumes as a wind-up and nothing else: the interrupted round is answered,
/// the `Cancel` that ended it stands, and the model is never called.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_with_cancel_winds_up_and_does_not_call_model() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let session = session(server.url(), "cancel-before-activation").await?;
    session.enqueue_text("must never reach the model").await?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    // The turn had already made a tool call, so winding it up is something the
    // log can be checked for rather than a no-op.
    harnx_runtime::nats_session_log::NatsSessionLog::new(js.clone(), session.storage_key())
        .append_event_async(&Entry::ToolCalls {
            text: String::new(),
            thought: None,
            calls: vec![ToolCall::new(
                "slow_tool".into(),
                json!({}),
                Some("cut-off-call".into()),
                None,
            )],
            timestamp: None,
            fence_token: Some(1),
        })
        .await?;
    let cancel_seq = accepted_seq(session.interrupt("client cancel").await?)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "late-worker",
        fold_capture_call_fn(calls.clone(), Arc::new(AsyncMutex::new(Vec::new()))),
    )
    .await?;
    // Give the worker its activation before concluding it ran nothing.
    wait_for_worker_session_cleanup(&js, session.storage_key()).await?;
    // The late worker winds the turn up behind the interruption; it never
    // rewrites or displaces the `Cancel` that ended it.
    assert_eq!(
        await_cancel_entry(&js, session.storage_key()).await?,
        cancel_seq
    );
    let log =
        harnx_runtime::nats_session_log::NatsSessionLog::new(js.clone(), session.storage_key());
    poll_until(async || {
        Ok(log
            .load_events_latest_async()
            .await?
            .iter()
            .any(|(seq, entry)| {
                matches!(entry, Entry::ToolResults { results, .. }
                    if *seq > cancel_seq
                        && results.iter().any(|r| r.id.as_deref() == Some("cut-off-call")))
            }))
    })
    .await
    .context("the wind-up answers the round the Cancel cut off")?;
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
    let model_dropped = tokio_util::sync::CancellationToken::new();
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "watch-only-worker",
        abort_blocked_call_fn(entered.clone(), model_dropped.clone()),
    )
    .await?;
    let session = session(server.url(), "watch-only-cancel").await?;
    session
        .enqueue_text("stream until durable cancellation")
        .await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, entered.notified()).await?;
    // Deliberately bypass the Core NATS latency path: only the worker's own
    // stream watcher can see this Cancel, and it must drop the model on it.
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    append_cancel(&js, session.storage_key()).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, model_dropped.cancelled()).await?;
    assert!(
        model_dropped.is_cancelled(),
        "the durable watch requires a model drop, not another cooperative poll"
    );
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
