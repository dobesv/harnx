use super::*;
use anyhow::{Context, Result};
use harnx_execution_control::{ExecutionContext, ExecutionStore, OperationRef, Owner};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_level_subagent_stop_returns_while_owned_child_turn_is_still_held() -> Result<()> {
    let nats = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(nats.url()).await?);
    let store = ExecutionStore::ensure(&js, 1).await?;
    let root = session_execution(&store, "root", "g1", None).await?;
    let tool = tool_execution(&store, &root, "parent-tool").await?;
    let tree = child_tree(&store, &tool).await?;
    let unrelated = session_execution(&store, "unrelated", "g1", None).await?;
    let session = targeted_session(&js, &tool).await?;
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let (started, entered) = tokio::sync::oneshot::channel();
    let finished = CancellationToken::new();
    let turn = tokio::spawn({
        let finished = finished.clone();
        async move {
            started.send(()).unwrap();
            held.await?;
            finished.cancel();
            anyhow::bail!("late child result");
        }
    });
    entered.await?;
    let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        await_owned_turn(
            &session,
            turn,
            cancel_tx,
            AwaitTurnParams {
                message: "original",
                timeout: None,
                token_budget: None,
                cancel,
            },
        ),
    )
    .await?;
    assert!(matches!(result, PromptTurn::Aborted(_)));
    assert_eq!(cancel_rx.recv().await, Some(()));
    assert!(
        !finished.is_cancelled(),
        "acceptance must not await the child turn"
    );
    let stop = store
        .gate_stop(tree.child.gate_root(), tree.child.operation())
        .await?
        .context("target stop")?;
    assert_eq!(
        store
            .gate_stop(tree.grandchild.gate_root(), tree.grandchild.operation())
            .await?,
        Some(stop)
    );
    for context in [&root, &tree.sibling, &unrelated] {
        assert!(store
            .gate_stop(context.gate_root(), context.operation())
            .await?
            .is_none());
    }
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), finished.cancelled()).await?;
    Ok(())
}

struct ChildTree {
    child: ExecutionContext,
    grandchild: ExecutionContext,
    sibling: ExecutionContext,
}

async fn child_tree(store: &ExecutionStore, tool: &ExecutionContext) -> Result<ChildTree> {
    let child = session_execution(
        store,
        "targeted",
        "targeted-call",
        Some(tool.operation().clone()),
    )
    .await?;
    let nested = tool_execution(store, &child, "nested-tool").await?;
    let grandchild = session_execution(
        store,
        "grandchild",
        "grandchild-call",
        Some(nested.operation().clone()),
    )
    .await?;
    let sibling = session_execution(
        store,
        "sibling",
        "sibling-call",
        Some(tool.operation().clone()),
    )
    .await?;
    Ok(ChildTree {
        child,
        grandchild,
        sibling,
    })
}

async fn targeted_session(
    js: &async_nats::jetstream::Context,
    tool: &ExecutionContext,
) -> Result<NatsSession> {
    Ok(NatsSession::new(
        crate::NatsSessionConfig {
            cluster: "local".into(),
            initializer: crate::SessionInitializer::inline(
                "",
                Default::default(),
                Default::default(),
            ),
            session_id: Some("targeted".into()),
            activation_route: crate::SessionActivationRoute::ClusterShared,
        },
        js.client().clone(),
        js.clone(),
        crate::utils::create_abort_signal(),
    )
    .await?
    .with_execution_parent(tool.operation().clone(), "targeted-call".into()))
}

async fn session_execution(
    store: &ExecutionStore,
    name: &str,
    id: &str,
    parent: Option<OperationRef>,
) -> Result<ExecutionContext> {
    let key = harnx_core::session_identity::session_key(None, name);
    let operation = store.session(&key, parent, Some(id)).await?;
    store
        .claim(&operation.reference, Owner::invocation("worker"))
        .await?;
    store.activate_gate(&operation.reference).await
}

async fn tool_execution(
    store: &ExecutionStore,
    parent: &ExecutionContext,
    id: &str,
) -> Result<ExecutionContext> {
    let operation = store
        .child(
            OperationRef::new(&parent.generation().session_id, id),
            parent.operation().clone(),
        )
        .await?;
    store
        .claim(&operation.reference, Owner::invocation("tool-server"))
        .await?;
    store.activate_gate(&operation.reference).await
}
