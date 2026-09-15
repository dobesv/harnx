use crate::test_common as common;

use super::*;

#[tokio::test]
async fn reconciliation_tolerates_a_pruned_observed_child_but_not_a_missing_blocker() -> Result<()>
{
    let _server = common::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    let js = jetstream::new(async_nats::connect(_server.url()).await?);
    let store = ExecutionStore::ensure(&js, 1).await?;
    let root = store.session("observed-parent", None, None).await?;
    let owner = Owner::invocation("observer");
    store.claim(&root.reference, owner.clone()).await?;
    let child = OperationRef::new("observed-parent", "completed-child");
    store.child(child.clone(), root.reference.clone()).await?;
    store.claim(&child, owner.clone()).await?;
    assert!(store.get(&child).await?.is_some());
    // Another observer completes and prunes the child after the first observer
    // read it, but before that observer's reconciliation read/CAS.
    store.owner_stopped(&child, &owner).await?;
    store.status(&root.reference).await?;
    assert!(store.get(&child).await?.is_none());
    let interrupted_read = store.reconcile_one(&child).await.map(Some);
    assert!(interrupted_read.is_err());
    assert!(
        reconcile_observed(&store, &child, Some(&root.reference), interrupted_read)
            .await?
            .is_none()
    );

    // An absent record with a still-registered parent edge is a real blocker.
    store
        .mutate(&root.reference, |operation| {
            operation.children.insert(child.clone());
            Ok(())
        })
        .await?;
    assert!(store.status(&root.reference).await.is_err());
    let interrupted_read = store.reconcile_one(&child).await.map(Some);
    assert!(
        reconcile_observed(&store, &child, Some(&root.reference), interrupted_read)
            .await
            .is_err()
    );
    let missing_root = store.reconcile_one(&child).await.map(Some);
    assert!(reconcile_observed(&store, &child, None, missing_root)
        .await
        .is_err());
    Ok(())
}

#[test]
fn rejected_replay_restores_preparing_without_undoing_cancellation_or_a_new_owner() -> Result<()> {
    let previous = Operation::preparing(
        OperationRef::new("parent", "tool"),
        OperationKind::Tool,
        None,
    );
    let owner = Owner::invocation("replay");
    let mut claimed = previous.clone();
    claimed.owner = Some(owner.clone());
    claimed.state = OperationState::Running;
    let mut restored = claimed.clone();
    restore_replay_claim(&mut restored, &previous, &owner)?;
    assert_eq!(restored.state, OperationState::Preparing);
    assert!(restored.owner.is_none());

    claimed.request_cancel("cancel", false)?;
    claimed.sealed = true;
    restore_replay_claim(&mut claimed, &previous, &owner)?;
    assert_eq!(claimed.state, OperationState::CancelRequested);
    assert!(claimed.sealed);
    assert!(claimed.owner_stopped);
    assert!(claimed.cancel_recorded);
    assert!(claimed.cancellation.is_some());

    let replacement = Owner::invocation("replacement");
    claimed.owner = Some(replacement.clone());
    restore_replay_claim(&mut claimed, &previous, &owner)?;
    assert_eq!(claimed.owner, Some(replacement));
    Ok(())
}
