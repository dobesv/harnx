use super::*;
use anyhow::{Context, Result};
use harnx_execution_control::{
    CommitAction, GateAction, InterruptScope, OperationKind, OperationRef, Owner, WorkRegistration,
};

#[tokio::test]
async fn child_binding_before_registration_is_display_denied_not_a_failed_turn() -> Result<()> {
    let nats = crate::nats_test_common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(nats.url()).await?);
    let store = ExecutionStore::ensure(&js, 1).await?;
    let root = store
        .open_gate(
            OperationRef::new("parent", "g1"),
            Owner::invocation("worker"),
        )
        .await?;
    // Exact persisted phase of StartWork before its anchor CAS. Parallel worker
    // preparation can expose this binding to the already-admitted follower.
    js.get_key_value(harnx_execution_control::BUCKET)
        .await?
        .create(
            "sessions/child/gate-authority",
            serde_json::to_vec(root.gate_root())?.into(),
        )
        .await?;
    let state = LiveEventState::default();
    state.bind("child-g1".into());
    state.select(Some("child-g1".into()));
    state.refresh(&store, "child").await?;
    assert!(!state.allows(Some("child-g1")));
    let child = WorkRegistration {
        operation: OperationRef::new("child", "child-g1"),
        kind: OperationKind::Session,
        owner: Owner::invocation("child-worker"),
    };
    store
        .commit_if_admissible(
            &root,
            CommitAction {
                id: "child".into(),
                kind: GateAction::StartWork {
                    child: child.clone(),
                    input: serde_json::Value::Null,
                },
            },
        )
        .await?;
    state.refresh(&store, "child").await?;
    assert!(state.allows(Some("child-g1")));
    store
        .interrupt(
            &InterruptScope {
                gate_root: root.gate_root().clone(),
                operation: child.operation,
                reason: "cancel".into(),
            },
            "stop",
        )
        .await?;
    state.refresh(&store, "child").await?;
    assert!(!state.allows(Some("child-g1")));
    Ok(())
}
