#[path = "../../harnx-runtime/tests/common/mod.rs"]
mod common;

use anyhow::{Context, Result};
use harnx_execution_control::*;
use std::time::Duration;

async fn store() -> Result<(common::NatsServerHandle, ExecutionStore)> {
    let server = common::spawn_nats_server()
        .await?
        .context("nats-server is required for execution control tests")?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    Ok((server, ExecutionStore::ensure(&js, 1).await?))
}

fn owner(fence: u64) -> Owner {
    Owner {
        instance_id: format!("worker-{fence}"),
        fence,
    }
}

#[tokio::test]
async fn replacement_tool_server_cannot_reuse_a_live_invocations_owner() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store.session("owner-isolation", None, None).await?;
    let child = store
        .child(OperationRef::new("owner-isolation", "call"), root.reference)
        .await?;
    let first = Owner::invocation("same-routing-name");
    let replacement = Owner::invocation("same-routing-name");
    assert_ne!(first, replacement);
    store.claim(&child.reference, first.clone()).await?;
    assert!(store
        .claim(&child.reference, replacement.clone())
        .await
        .is_err());
    assert!(store
        .owner_stopped(&child.reference, &replacement)
        .await
        .is_err());
    assert!(!store.get(&child.reference).await?.unwrap().owner_stopped);
    store.owner_stopped(&child.reference, &first).await?;
    Ok(())
}

#[tokio::test]
async fn normal_owner_shutdown_preserves_registered_child_work() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store.session("normal-cleanup", None, None).await?;
    store.claim(&root.reference, owner(1)).await?;
    let child = OperationRef::new("normal-cleanup", "finishing-tool");
    store.child(child.clone(), root.reference.clone()).await?;
    store.claim(&child, owner(1)).await?;
    assert!(store.seal(&root.reference, &owner(1)).await?);
    store.owner_stopped(&root.reference, &owner(1)).await?;
    store.check_ancestors(&child).await?;
    assert!(store
        .reserve_prompt(&root.reference, "too-late")
        .await
        .is_err());
    assert!(store
        .child(
            OperationRef::new("normal-cleanup", "too-late"),
            root.reference.clone()
        )
        .await
        .is_err());
    store.owner_stopped(&child, &owner(1)).await?;
    assert_eq!(
        store.status(&root.reference).await?.state,
        OperationState::Completed
    );
    Ok(())
}

#[test]
fn terminal_states_never_reopen_and_cancel_does_not_complete_running_work() {
    use OperationState::*;
    for terminal in [Completed, Cancelled] {
        for next in [
            Preparing,
            Running,
            CancelRequested,
            Quiescing,
            Unconfirmed,
            Completed,
            Cancelled,
        ] {
            assert!(!terminal.can_transition(next));
        }
    }
    assert!(!Running.can_transition(Cancelled));
    assert!(!CancelRequested.can_transition(Cancelled));
    assert!(Unconfirmed.can_transition(Cancelled));
    assert!(!Unconfirmed.accepts_work());
}

#[test]
fn legacy_operation_without_abandoned_flag_remains_compatible() -> Result<()> {
    let operation = Operation::preparing(
        OperationRef::new("legacy", "execution"),
        OperationKind::Session,
        None,
    );
    let mut value = serde_json::to_value(operation)?;
    value.as_object_mut().unwrap().remove("abandoned");

    let operation: Operation = serde_json::from_value(value)?;
    assert!(!operation.abandoned);
    Ok(())
}

#[tokio::test]
async fn duplicate_cancel_retry_and_stale_generation_are_idempotent() -> Result<()> {
    let (_server, store) = store().await?;
    let op = store.session("root", None, None).await?;
    store.claim(&op.reference, owner(1)).await?;
    let first = store
        .request_cancel("root", CancelRequest::default())
        .await?;
    let second = store
        .request_cancel("root", CancelRequest::default())
        .await?;
    assert_eq!(first.cancellation_id, second.cancellation_id);
    assert_eq!(second.disposition, CancelDisposition::AlreadyRequested);
    store
        .request_cancel(
            "root",
            CancelRequest {
                retry: true,
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(
        store
            .get(&op.reference)
            .await?
            .unwrap()
            .cancellation
            .unwrap()
            .attempt,
        2
    );
    store.quiesce(&op.reference, &owner(1)).await?;
    store
        .record_coverage(&op.reference, &owner(1), 0, true)
        .await?;
    store.owner_stopped(&op.reference, &owner(1)).await?;
    let new = store.session("root", None, None).await?;
    assert_ne!(new.reference, op.reference);
    let stale = store
        .request_cancel(
            "root",
            CancelRequest {
                expected_execution_id: Some(op.reference.execution_id),
                retry: false,
            },
        )
        .await?;
    assert_eq!(stale.disposition, CancelDisposition::Idle);
    assert!(store
        .get(&new.reference)
        .await?
        .unwrap()
        .state
        .accepts_work());
    Ok(())
}

#[tokio::test]
async fn operator_can_abandon_an_unconfirmed_graph_and_start_a_new_generation() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store.session("abandon", None, Some("old")).await?;
    let root_owner = owner(1);
    store.claim(&root.reference, root_owner.clone()).await?;
    let tool = store
        .child(OperationRef::new("abandon", "tool"), root.reference.clone())
        .await?;
    let tool_owner = Owner::invocation("tool-server");
    store.claim(&tool.reference, tool_owner.clone()).await?;
    let child = store
        .session(
            "child",
            Some(tool.reference.clone()),
            Some("child-execution"),
        )
        .await?;
    let child_owner = owner(2);
    store.claim(&child.reference, child_owner.clone()).await?;

    store
        .request_cancel("abandon", CancelRequest::default())
        .await?;
    store
        .mutate(&root.reference, |operation| {
            operation.transition(OperationState::Unconfirmed)
        })
        .await?;

    let receipt = store.abandon_unconfirmed("abandon", "old").await?;
    assert_eq!(receipt.disposition, CancelDisposition::Cancelled);
    assert!(receipt.abandoned);
    for reference in [&root.reference, &child.reference] {
        let operation = store.get(reference).await?.unwrap();
        assert_eq!(operation.state, OperationState::Cancelled);
        assert!(operation.abandoned);
        assert!(operation.owner_stopped);
    }
    assert!(store
        .record_coverage(&tool.reference, &tool_owner, 1, true)
        .await
        .is_err());
    assert!(store.get(&tool.reference).await?.is_none());

    let replacement = store.session("abandon", None, Some("new")).await?;
    assert_eq!(replacement.reference.execution_id, "new");
    Ok(())
}

#[tokio::test]
async fn abandonment_is_generation_scoped_and_requires_unconfirmed_state() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store
        .session("scoped-abandon", None, Some("current"))
        .await?;

    let stale = store.abandon_unconfirmed("scoped-abandon", "stale").await?;
    assert_eq!(stale.disposition, CancelDisposition::Idle);
    assert!(store
        .abandon_unconfirmed("scoped-abandon", "current")
        .await
        .is_err());
    assert_eq!(
        store.get(&root.reference).await?.unwrap().state,
        OperationState::Preparing
    );
    Ok(())
}

#[tokio::test]
async fn child_registration_racing_cancel_never_starts_detached_work() -> Result<()> {
    let (_server, store) = store().await?;
    for index in 0..30 {
        let root = store.session(&format!("root-{index}"), None, None).await?;
        let child = OperationRef::new(&root.reference.session_id, "tool");
        let (registered, cancelled) = tokio::join!(
            store.child(child.clone(), root.reference.clone()),
            store.cancel_operation(&root.reference, None, false)
        );
        cancelled?;
        assert!(store.check_ancestors(&child).await.is_err());
        let root = store.get(&root.reference).await?.unwrap();
        if registered.is_ok() {
            assert!(root.children.contains(&child));
        } else {
            assert!(store.get(&child).await?.unwrap().state.is_terminal());
        }
    }
    Ok(())
}

#[tokio::test]
async fn prompt_admission_racing_cancel_defines_the_winner() -> Result<()> {
    let (_server, store) = store().await?;
    for index in 0..30 {
        let root = store.session(&format!("root-{index}"), None, None).await?;
        let (admitted, cancelled) = tokio::join!(
            store.reserve_prompt(&root.reference, "message"),
            store.cancel_operation(&root.reference, None, false)
        );
        cancelled?;
        let root = store.get(&root.reference).await?.unwrap();
        assert_eq!(root.admissions.contains_key("message"), admitted.is_ok());
        assert!(store.reserve_prompt(&root.reference, "late").await.is_err());
        if admitted.is_ok() {
            store.commit_prompt(&root.reference, "message", 10).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn ancestor_watch_survives_requester_loss_and_direct_child_cancel_stays_local() -> Result<()>
{
    let (_server, store) = store().await?;
    let root = store.session("root", None, None).await?;
    let tool = store
        .child(OperationRef::new("root", "tool"), root.reference.clone())
        .await?;
    let child = store
        .session("child", Some(tool.reference.clone()), Some("invocation"))
        .await?;
    let nested = store
        .child(
            OperationRef::new("child", "nested"),
            child.reference.clone(),
        )
        .await?;
    store
        .request_cancel("child", CancelRequest::default())
        .await?;
    assert!(store.check_ancestors(&root.reference).await.is_ok());
    assert!(store.check_ancestors(&tool.reference).await.is_ok());
    // No Core NATS publication or explicit descendant traversal.
    let observed = tokio::time::timeout(
        Duration::from_secs(2),
        store.watch_cancellation(&nested.reference),
    )
    .await?;
    assert!(observed.is_err());
    Ok(())
}

#[tokio::test]
async fn owner_fencing_children_and_unconfirmed_block_completion() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store.session("root", None, None).await?;
    store.claim(&root.reference, owner(1)).await?;
    let child = store
        .child(OperationRef::new("root", "tool"), root.reference.clone())
        .await?;
    store.claim(&child.reference, owner(1)).await?;
    store.claim(&root.reference, owner(2)).await?;
    assert!(store
        .owner_stopped(&root.reference, &owner(1))
        .await
        .is_err());
    store.cancel_operation(&root.reference, None, false).await?;
    store.quiesce(&root.reference, &owner(2)).await?;
    store
        .record_coverage(&root.reference, &owner(2), 0, true)
        .await?;
    assert_eq!(
        store.owner_stopped(&root.reference, &owner(2)).await?.state,
        OperationState::Quiescing
    );
    store
        .mutate(&root.reference, |op| {
            op.cancellation.as_mut().unwrap().progress_at -= chrono::Duration::seconds(6);
            Ok(())
        })
        .await?;
    assert_eq!(
        store.status(&root.reference).await?.state,
        OperationState::Unconfirmed
    );
    assert!(store.session("root", None, None).await.is_err());
    store
        .cancel_operation(&child.reference, None, false)
        .await?;
    store.owner_stopped(&child.reference, &owner(1)).await?;
    assert_eq!(
        store.status(&root.reference).await?.state,
        OperationState::Cancelled
    );
    assert!(store.get(&child.reference).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn missing_ancestors_and_cycles_fail_closed() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store.session("root", None, None).await?;
    let child = store
        .child(OperationRef::new("child", "tool"), root.reference.clone())
        .await?;
    store
        .mutate(&root.reference, |op| {
            op.parent = Some(child.reference.clone());
            Ok(())
        })
        .await?;
    store
        .mutate(&child.reference, |op| {
            op.children.insert(root.reference.clone());
            Ok(())
        })
        .await?;
    assert!(store
        .check_ancestors(&child.reference)
        .await
        .unwrap_err()
        .to_string()
        .contains("cycle"));
    store.purge_session("root").await?;
    assert!(store
        .check_ancestors(&child.reference)
        .await
        .unwrap_err()
        .to_string()
        .contains("missing"));
    Ok(())
}

#[tokio::test]
async fn unresolved_admission_blocks_confirmation_until_reconciled() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store.session("root", None, None).await?;
    store.claim(&root.reference, owner(1)).await?;
    store
        .reserve_prompt(&root.reference, "admitted-before-cancel")
        .await?;
    assert!(!store.seal(&root.reference, &owner(1)).await?);
    store.cancel_operation(&root.reference, None, false).await?;
    store
        .record_coverage(&root.reference, &owner(1), 10, true)
        .await?;
    assert!(!store
        .owner_stopped(&root.reference, &owner(1))
        .await?
        .state
        .is_terminal());
    assert!(store.reserve_prompt(&root.reference, "late").await.is_err());
    store
        .commit_prompt(&root.reference, "admitted-before-cancel", 5)
        .await?;
    assert_eq!(
        store.status(&root.reference).await?.state,
        OperationState::Cancelled
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_waits_for_three_levels_and_concurrent_observers_converge() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store.session("root", None, None).await?;
    store.claim(&root.reference, owner(1)).await?;
    let tool = store
        .child(OperationRef::new("root", "tool"), root.reference.clone())
        .await?;
    store.claim(&tool.reference, owner(1)).await?;
    let child = store
        .session(
            "child",
            Some(tool.reference.clone()),
            Some("child-invocation"),
        )
        .await?;
    store.claim(&child.reference, owner(1)).await?;
    let nested = store
        .child(
            OperationRef::new("child", "nested"),
            child.reference.clone(),
        )
        .await?;
    store.claim(&nested.reference, owner(1)).await?;
    for operation in [&root, &tool, &child, &nested] {
        store
            .cancel_operation(&operation.reference, None, false)
            .await?;
        store
            .record_coverage(&operation.reference, &owner(1), 0, true)
            .await?;
    }
    for operation in [&root, &tool, &child] {
        assert!(!store
            .owner_stopped(&operation.reference, &owner(1))
            .await?
            .state
            .is_terminal());
    }
    store.owner_stopped(&nested.reference, &owner(1)).await?;
    let (a, b) = tokio::join!(store.status(&root.reference), store.status(&root.reference));
    assert_eq!(a?.state, OperationState::Cancelled);
    assert_eq!(b?.state, OperationState::Cancelled);
    assert!(store.get(&nested.reference).await?.is_none());
    assert!(store.get(&tool.reference).await?.is_none());
    assert_eq!(
        store.current("child").await?.unwrap().state,
        OperationState::Cancelled
    );
    Ok(())
}

#[tokio::test]
async fn status_recovers_cancellation_stranded_during_child_startup() -> Result<()> {
    let (_server, store) = store().await?;
    let root = store.session("stranded-root", None, None).await?;
    store.claim(&root.reference, owner(1)).await?;
    let tool = store
        .child(
            OperationRef::new("stranded-root", "subagent-call"),
            root.reference.clone(),
        )
        .await?;
    store.claim(&tool.reference, owner(1)).await?;
    let child = store
        .session(
            "stranded-child",
            Some(tool.reference.clone()),
            Some("subagent-call"),
        )
        .await?;
    store
        .reserve_prompt(&child.reference, "reserved-before-interrupt")
        .await?;
    store.owner_stopped(&tool.reference, &owner(1)).await?;

    store.cancel_operation(&root.reference, None, false).await?;
    store.quiesce(&root.reference, &owner(1)).await?;
    store
        .record_coverage(&root.reference, &owner(1), 0, true)
        .await?;
    store.owner_stopped(&root.reference, &owner(1)).await?;

    assert_eq!(
        store.status(&root.reference).await?.state,
        OperationState::Cancelled
    );
    assert!(store.get(&tool.reference).await?.is_none());
    assert_eq!(
        store.current("stranded-child").await?.unwrap().state,
        OperationState::Cancelled
    );
    Ok(())
}
