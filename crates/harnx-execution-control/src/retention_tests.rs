use crate::test_common as common;

use super::*;

async fn store() -> Result<(common::NatsServerHandle, ExecutionStore)> {
    let server = common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let js = jetstream::new(async_nats::connect(server.url()).await?);
    Ok((server, ExecutionStore::ensure(&js, 1).await?))
}

async fn assert_generation_rejects_work(
    store: &ExecutionStore,
    reference: &OperationRef,
    owner: &Owner,
) {
    assert!(store.claim(reference, owner.clone()).await.is_err());
    assert!(store.reserve_prompt(reference, "late").await.is_err());
}

async fn assert_retired_stop(
    store: &ExecutionStore,
    reference: &OperationRef,
    stop: Option<StopDecision>,
) -> Result<()> {
    assert!(store.get(reference).await?.is_none());
    assert_eq!(store.stop_decision(reference).await?, stop);
    Ok(())
}

async fn assert_stop_mutation_rejected(store: &ExecutionStore, reference: &OperationRef) {
    assert!(store
        .mutate(reference, |op| {
            op.stop_decision = None;
            Ok(())
        })
        .await
        .is_err());
    assert!(store
        .mutate(reference, |op| {
            op.stop_decision.as_mut().unwrap().cancellation_id = "rewritten".into();
            Ok(())
        })
        .await
        .is_err());
}

fn assert_completed_without_cleanup(parent: &Operation) {
    assert_eq!(parent.state, OperationState::Completed);
    assert_eq!(parent.cleanup_state(), CleanupState::Unconfirmed);
    assert!(!parent.cleanup_confirmed());
}

#[tokio::test]
async fn stop_evidence_survives_child_pruning_and_new_generation_install() -> Result<()> {
    let (server, store) = store().await?;
    let root = store.session("retention", None, Some("old")).await?;
    let owner = Owner::invocation("owner");
    store.claim(&root.reference, owner.clone()).await?;
    let child = store
        .child(
            OperationRef::new("retention", "child"),
            root.reference.clone(),
        )
        .await?;
    store.claim(&child.reference, owner.clone()).await?;
    store.owner_stopped(&child.reference, &owner).await?;
    store.status(&root.reference).await?;
    assert!(store.get(&child.reference).await?.is_none());
    assert!(!store.is_stop_fenced(&child.reference).await?);
    // Even a child retired before acceptance may have a reply awaiting projection.
    let stopped = store
        .accept_interrupt(&root.reference, "stop", "user interrupt")
        .await?;
    let stop = stopped.stop_decision.context("stop receipt")?;
    assert_eq!(
        store.stop_decision(&child.reference).await?,
        Some(stop.clone())
    );
    store
        .record_coverage(&root.reference, &owner, 0, true)
        .await?;
    store.owner_stopped(&root.reference, &owner).await?;
    let next = store.session("retention", None, Some("new")).await?;
    assert!(store.get(&root.reference).await?.is_none());
    // Reconnect with no in-memory state: both physical records are now retired.
    let js = jetstream::new(async_nats::connect(server.url()).await?);
    let recovered = ExecutionStore::ensure(&js, 1).await?;
    for reference in [&root.reference, &child.reference] {
        assert!(recovered.is_stop_fenced(reference).await?);
        assert_eq!(
            recovered.stop_decision(reference).await?,
            Some(stop.clone())
        );
    }
    assert!(!recovered.is_stop_fenced(&next.reference).await?);
    assert!(
        recovered.create(&root).await.is_err(),
        "retired generation cannot reopen"
    );
    recovered.purge_session("retention").await?;
    assert!(recovered.is_stop_fenced(&root.reference).await.is_err());
    Ok(())
}

#[tokio::test]
async fn interrupted_generation_is_replaceable_before_owner_or_child_cleanup() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store.session("overlap", None, Some("old")).await?;
    let owner = Owner::invocation("owner");
    store.claim(&root.reference, owner.clone()).await?;
    let child = store
        .child(
            OperationRef::new("overlap", "child"),
            root.reference.clone(),
        )
        .await?;
    store.claim(&child.reference, owner.clone()).await?;
    let stopped = store
        .accept_interrupt(&root.reference, "stop", "user interrupt")
        .await?;
    assert!(!stopped.can_prune());
    assert!(!stopped.cleanup_confirmed());
    assert!(!stopped.cancel_recorded);
    let next = store.session("overlap", None, Some("new")).await?;
    assert!(store.get(&root.reference).await?.is_some());
    assert!(store.get(&child.reference).await?.is_some());
    store.status(&root.reference).await?;
    assert!(
        store.get(&child.reference).await?.is_some(),
        "pending cleanup is not pruned"
    );
    assert!(store.check_ancestors(&child.reference).await.is_err());
    assert_generation_rejects_work(&store, &root.reference, &owner).await;
    // Old cleanup must not disturb the new pointer or erase the old stop fence.
    store.owner_stopped(&child.reference, &owner).await?;
    store
        .record_coverage(&root.reference, &owner, 0, true)
        .await?;
    store.owner_stopped(&root.reference, &owner).await?;
    store.retire_observed(&root.reference).await?;
    assert!(store.is_stop_fenced(&child.reference).await?);
    assert_eq!(
        store.current("overlap").await?.unwrap().reference,
        next.reference
    );
    assert!(!store.is_stop_fenced(&next.reference).await?);
    Ok(())
}

#[tokio::test]
async fn direct_child_stop_remains_local_and_survives_retirement() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store.session("local", None, None).await?;
    let child = store
        .child(OperationRef::new("local", "child"), root.reference.clone())
        .await?;
    let sibling = store
        .child(
            OperationRef::new("local", "sibling"),
            root.reference.clone(),
        )
        .await?;
    let owner = Owner::invocation("owner");
    store.claim(&child.reference, owner.clone()).await?;
    let first = store
        .accept_interrupt(&child.reference, "first", "user interrupt")
        .await?;
    let duplicate = store
        .accept_interrupt(&child.reference, "duplicate", "retry")
        .await?;
    assert_eq!(first.stop_decision, duplicate.stop_decision);
    store.owner_stopped(&child.reference, &owner).await?;
    store.status(&root.reference).await?;
    assert_retired_stop(&store, &child.reference, first.stop_decision).await?;
    assert!(!store.is_stop_fenced(&root.reference).await?);
    assert!(!store.is_stop_fenced(&sibling.reference).await?);
    Ok(())
}

#[tokio::test]
async fn accepted_stop_cannot_be_cleared_or_rewritten_by_mutation() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store.session("immutable", None, None).await?;
    let stopped = store
        .accept_interrupt(&root.reference, "first", "user interrupt")
        .await?;
    assert_stop_mutation_rejected(&store, &root.reference).await;
    assert_eq!(
        store.stop_decision(&root.reference).await?,
        stopped.stop_decision
    );
    Ok(())
}

#[tokio::test]
async fn stop_query_fails_closed_on_unknown_lineage_and_cycles() -> Result<()> {
    let (_server, store) = store().await?;
    assert!(store
        .is_stop_fenced(&OperationRef::new("unknown", "generation"))
        .await
        .is_err());
    let root = store.session("cycle", None, None).await?;
    store
        .mutate(&root.reference, |op| {
            op.parent = Some(root.reference.clone());
            Ok(())
        })
        .await?;
    assert!(store
        .is_stop_fenced(&root.reference)
        .await
        .unwrap_err()
        .to_string()
        .contains("cycle"));
    Ok(())
}

#[tokio::test]
async fn pruned_abandoned_child_does_not_confirm_parent_cleanup() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store.session("abandoned-child", None, None).await?;
    let owner = Owner::invocation("owner");
    store.claim(&root.reference, owner.clone()).await?;
    let child = store
        .child(
            OperationRef::new("abandoned-child", "child"),
            root.reference.clone(),
        )
        .await?;
    store.claim(&child.reference, owner.clone()).await?;
    store
        .accept_interrupt(&child.reference, "stop", "user interrupt")
        .await?;
    store
        .mutate(&child.reference, |op| {
            op.transition(OperationState::Unconfirmed)?;
            op.abandon_unconfirmed()
        })
        .await?;
    store.status(&root.reference).await?;
    assert!(store.get(&child.reference).await?.is_none());
    let parent = store.owner_stopped(&root.reference, &owner).await?;
    assert_completed_without_cleanup(&parent);
    assert!(store.is_stop_fenced(&child.reference).await?);
    Ok(())
}

#[test]
fn malformed_operation_cannot_be_read_as_unfenced_retired_record() -> Result<()> {
    let op = Operation::preparing(
        OperationRef::new("corrupt", "generation"),
        OperationKind::Session,
        None,
    );
    let mut document = serde_json::to_value(op)?;
    document["state"] = "retired".into();
    assert!(serde_json::from_value::<StoredOperation>(document).is_err());
    Ok(())
}
