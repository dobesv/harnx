use super::*;
use anyhow::{Context, Result};
use harnx_execution_control::{
    CommitAction, ExecutionStore, GateAction, InterruptScope, OperationKind, OperationRef, Owner,
    WorkRegistration,
};
use std::sync::Arc;
use tokio::sync::Barrier;

use crate::nats_test_common as common;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_vs_nested_child_registration_creates_and_announces_nothing() -> Result<()> {
    let server = common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let js = jetstream::new(async_nats::connect(server.url()).await?);
    let store = ExecutionStore::ensure(&js, 1).await?;
    let root = store
        .open_gate(
            OperationRef::new("root", "g1"),
            Owner::invocation("root-worker"),
        )
        .await?;
    let nested = register(&store, &root, "nested", OperationKind::Session).await?;
    let tool = register(&store, &nested, "tool", OperationKind::Tool).await?;
    let metadata = crate::nats_session_metadata::SessionMetadataStore::ensure(&js, 1).await?;
    let mut toolset = SubagentToolset::new(
        "helper",
        SubagentSessionRoute::new("local", crate::SessionActivationRoute::ClusterShared),
        SubagentNats::new(js.client().clone(), js.clone(), metadata.clone()),
    );
    let barrier = Arc::new(AdmissionBarrier {
        entered: Barrier::new(2),
        release: Barrier::new(2),
    });
    toolset.admission_barrier = Some(barrier.clone());
    let task = tokio::spawn(async move {
        toolset
            .invoke_with_context(ToolInvocation {
                tool: SUBAGENT_SESSION_PROMPT_TOOL.into(),
                args: json!({"message": "nested child"}),
                context: ToolInvocationContext {
                    operation: Some(tool.operation().clone()),
                    execution: Some(tool),
                    call_id: "child-call".into(),
                    invoking_session_id: Some("nested".into()),
                    capabilities: Default::default(),
                },
                cancel: CancellationToken::new(),
            })
            .await
    });
    barrier.entered.wait().await;
    store
        .interrupt(
            &InterruptScope {
                gate_root: root.gate_root().clone(),
                operation: root.operation().clone(),
                reason: "root stop".into(),
            },
            "stop",
        )
        .await?;
    barrier.release.wait().await;
    assert!(matches!(task.await?, Err(ToolInvokeError::Interrupted(_))));
    assert!(metadata.list().await?.is_empty());
    assert!(
        NatsSessionLog::new(js.clone(), "nested")
            .load_events_latest_async()
            .await?
            .is_empty(),
        "no SubAgentStarted announcement"
    );
    assert!(
        store.current("nested").await?.is_none(),
        "no physical child prompt admitted"
    );
    Ok(())
}

async fn register(
    store: &ExecutionStore,
    parent: &harnx_execution_control::ExecutionContext,
    id: &str,
    kind: OperationKind,
) -> Result<harnx_execution_control::ExecutionContext> {
    let child = WorkRegistration {
        operation: OperationRef::new(
            if kind == OperationKind::Session {
                id
            } else {
                &parent.generation().session_id
            },
            id,
        ),
        kind,
        owner: Owner::invocation("worker"),
    };
    store
        .commit_if_admissible(
            parent,
            CommitAction {
                id: id.into(),
                kind: GateAction::StartWork {
                    child: child.clone(),
                    input: Value::Null,
                },
            },
        )
        .await?;
    Ok(child.context(parent))
}
