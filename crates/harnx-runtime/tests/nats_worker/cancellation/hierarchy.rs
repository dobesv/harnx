use super::*;
use anyhow::Context;
use harnx_execution_control::{OperationRef, OperationState, Owner};
use harnx_runtime::AgentCallFn;

async fn enqueue_child(parent: &NatsSession, url: &str, id: &str) -> Result<NatsSession> {
    let store = parent.execution_store();
    let parent_operation = store.current(parent.session_id()).await?.unwrap();
    let invocation = OperationRef::new(parent.session_id(), format!("invoke-{id}"));
    store
        .child(invocation.clone(), parent_operation.reference)
        .await?;
    let owner = Owner {
        instance_id: "completed-tool-handler".into(),
        fence: 1,
    };
    store.claim(&invocation, owner.clone()).await?;
    let child = session(url, id)
        .await?
        .with_execution_parent(invocation.clone(), format!("invoke-{id}"));
    child.enqueue_text("wait until cancelled").await?;
    // The tool handler has returned, but its registered child still owns work.
    // No intermediate handler remains to explicitly forward cancellation.
    store.owner_stopped(&invocation, &owner).await?;
    Ok(child)
}

fn waiting_model(entered: Arc<AtomicUsize>, stopped: Arc<AtomicUsize>) -> AgentCallFn {
    Arc::new(move |_, _, abort| {
        let entered = entered.clone();
        let stopped = stopped.clone();
        Box::pin(async move {
            entered.fetch_add(1, Ordering::SeqCst);
            harnx_core::abort::wait_abort_signal(&abort).await;
            stopped.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("model cancelled")
        })
    })
}

async fn confirmed_cancel(session: &NatsSession) -> Result<()> {
    // KV only: neither the requester nor a tool handler traverses descendants.
    let receipt = session
        .execution_store()
        .request_cancel(session.session_id(), CancelRequest::default())
        .await?;
    let status = session
        .wait_for_cancel(&receipt, tokio::time::Instant::now() + CI_SAFE_TIMEOUT)
        .await?;
    assert_eq!(status.disposition, CancelDisposition::Cancelled);
    Ok(())
}

async fn exercise_hierarchy(cancel_root: bool) -> Result<()> {
    let server = require_nats_server()
        .await?
        .context("nats-server required")?;
    let root = session(server.url(), "hierarchy-root").await?;
    root.enqueue_text("wait until cancelled").await?;
    let middle = enqueue_child(&root, server.url(), "hierarchy-middle").await?;
    let leaf = enqueue_child(&middle, server.url(), "hierarchy-leaf").await?;
    let entered = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "hierarchical-worker",
        waiting_model(entered.clone(), stopped.clone()),
    )
    .await;
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        while entered.load(Ordering::SeqCst) != 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    confirmed_cancel(if cancel_root { &root } else { &middle }).await?;
    assert_eq!(
        stopped.load(Ordering::SeqCst),
        if cancel_root { 3 } else { 2 }
    );
    for child in [&middle, &leaf] {
        assert_eq!(
            child
                .execution_store()
                .current(child.session_id())
                .await?
                .unwrap()
                .state,
            OperationState::Cancelled
        );
    }
    if !cancel_root {
        assert_eq!(
            root.execution_store()
                .current(root.session_id())
                .await?
                .unwrap()
                .state,
            OperationState::Running
        );
        root.enqueue_text("parent can continue after its child stops")
            .await?;
        confirmed_cancel(&root).await?;
    }
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn durable_cancellation_reaches_three_worker_levels_without_forwarding_handlers() -> Result<()>
{
    exercise_hierarchy(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_child_cancellation_stops_only_its_worker_subtree() -> Result<()> {
    exercise_hierarchy(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_ownerless_child_prompt_is_not_replayed_when_reopened() -> Result<()> {
    let server = require_nats_server()
        .await?
        .context("nats-server required")?;
    let root = session(server.url(), "ownerless-cancel-root").await?;
    let store = root.execution_store();
    let root_operation = store.session(root.session_id(), None, None).await?;
    let child = enqueue_child(&root, server.url(), "ownerless-cancel-child").await?;
    let child_execution_id = store
        .current(child.session_id())
        .await?
        .unwrap()
        .reference
        .execution_id;

    store
        .request_cancel(root.session_id(), CancelRequest::default())
        .await?;
    store.status(&root_operation.reference).await?;
    let cancelled_child = store.current(child.session_id()).await?.unwrap();
    assert_eq!(cancelled_child.state, OperationState::Cancelled);
    assert!(cancelled_child.cancel_recorded);

    let reopened = session(server.url(), child.session_id()).await?;
    assert_eq!(reopened.activate_pending_turn().await?, None);
    assert_eq!(
        store
            .current(child.session_id())
            .await?
            .unwrap()
            .reference
            .execution_id,
        child_execution_id
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "ownerless-cancel-worker",
        fold_capture_call_fn(calls.clone(), Arc::new(AsyncMutex::new(Vec::new()))),
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
