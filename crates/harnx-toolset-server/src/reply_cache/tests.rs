use super::*;
use harnx_execution_control::{
    CommitAction, ExecutionStore, GateAction, OperationKind, OperationRef, Owner, WorkRegistration,
};
use harnx_toolset::ToolExecution;
use tokio::sync::Barrier;

use crate::nats_test_common as common;

#[tokio::test]
async fn late_cache_insertion_is_payload_not_consumption_authority() -> Result<()> {
    let server = common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client);
    let store = ExecutionStore::ensure(&js, 1).await?;
    let root = store
        .open_gate(
            OperationRef::new("cache-session", "generation"),
            Owner::invocation("worker"),
        )
        .await?;
    let (request, saved) = committed_reply(&store, &js, &root).await?;
    reply_fence::consume(&store, &request, &saved).await?;
    let cache: ReplyCache = Arc::default();
    let key = cache_key(&request, "call")?;
    let CacheReservation::Execute(completion) = reserve_cache_entry(&cache, &key).await else {
        unreachable!()
    };
    let release = Arc::new(Barrier::new(2));
    let pending = {
        let cache = cache.clone();
        let key = key.clone();
        let release = release.clone();
        tokio::spawn(async move {
            release.wait().await;
            complete_cache_entry(&cache, key, Ok(saved), completion).await;
        })
    };
    store
        .interrupt(
            &harnx_execution_control::InterruptScope {
                gate_root: root.gate_root().clone(),
                operation: root.operation().clone(),
                reason: "stop".into(),
            },
            "cancel",
        )
        .await?;
    release.wait().await;
    pending.await?;
    let CacheReservation::Complete(saved) = reserve_cache_entry(&cache, &key).await else {
        anyhow::bail!("late value should remain cached for this test")
    };
    assert!(reply_fence::consume(&store, &request, &saved)
        .await
        .unwrap_err()
        .is::<harnx_execution_control::Interrupted>());
    Ok(())
}

async fn committed_reply(
    store: &ExecutionStore,
    js: &async_nats::jetstream::Context,
    root: &harnx_execution_control::ExecutionContext,
) -> Result<(ToolRequest, Arc<CommittedReply>)> {
    let child = WorkRegistration {
        operation: OperationRef::new("cache-session", "call"),
        kind: OperationKind::Tool,
        owner: Owner::invocation("tool"),
    };
    store
        .commit_if_admissible(
            root,
            CommitAction {
                id: "register".into(),
                kind: GateAction::StartWork {
                    child: child.clone(),
                    input: serde_json::json!({}),
                },
            },
        )
        .await?;
    let request = ToolRequest {
        execution: Some(ToolExecution {
            producer: child.context(root),
            consumer: root.clone(),
        }),
        replay_execution: None,
        replay: None,
        operation_id: "call".into(),
        call_id: "call".into(),
        parent_session_id: Some("cache-session".into()),
        tool_call_id: None,
        tool: "echo".into(),
        args: serde_json::json!({}),
        capabilities: Default::default(),
    };
    let journal = invocation_journal::InvocationJournal::ensure(js).await?;
    journal
        .record(&request, ("echo", "scope", "tool"), 1)
        .await?;
    journal
        .complete(
            &request,
            ToolReply {
                call_id: "call".into(),
                result: Ok(serde_json::json!("committed")),
            },
        )
        .await?;
    let saved = Arc::new(
        journal
            .committed_reply(&request)
            .await?
            .context("committed reply")?,
    );
    Ok((request, saved))
}
