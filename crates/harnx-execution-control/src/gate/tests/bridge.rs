use super::*;
use crate::Operation;

#[path = "pending_registration.rs"]
mod pending_registration;

async fn physical_root(store: &ExecutionStore) -> Result<Operation> {
    let root = store.session("bridge-session", None, Some("g1")).await?;
    store.claim(&root.reference, owner(1)).await
}

#[tokio::test]
async fn cancellation_before_activation_cannot_be_imported_as_running() -> Result<()> {
    let (_server, writer, canceller) = stores().await?;
    let root = physical_root(&writer).await?;
    canceller
        .cancel_operation(&root.reference, Some("stop"), false)
        .await?;
    assert!(writer.activate_gate(&root.reference).await.is_err());
    assert!(writer
        .gate_root(&root.reference.session_id)
        .await?
        .is_none());
    Ok(())
}

#[tokio::test]
async fn cancellation_helps_initialize_a_paused_activation_and_fences_admission() -> Result<()> {
    let (_server, writer, canceller) = stores().await?;
    let root = physical_root(&writer).await?;
    let ctx = ExecutionContext::new(
        root.reference.clone(),
        root.reference.clone(),
        root.reference.clone(),
        (owner(1), owner(1)),
    );
    let registration = GateRegistration {
        context: ctx.clone(),
        parent: None,
        kind: OperationKind::Session,
        previous_generation: None,
    };
    // Crash/pause after the physical activation CAS, before open_gate. Cancellation
    // must help complete initialization, never fall back to a separate stop key.
    writer
        .mutate(&root.reference, |op| {
            op.gate_registration = Some(Box::new(registration.clone()));
            Ok(())
        })
        .await?;
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let pending = {
        let ready = ready.clone();
        let release = release.clone();
        let writer = writer.clone();
        tokio::spawn(async move {
            ready.wait().await;
            release.wait().await;
            writer.materialize_gate(&registration).await
        })
    };
    ready.wait().await;
    canceller
        .cancel_operation(&root.reference, Some("stop"), false)
        .await?;
    release.wait().await;
    pending.await??;
    assert!(writer
        .commit_if_admissible(
            &ctx,
            action("start", GateAction::AdmitWork { input: json!({}) })
        )
        .await
        .unwrap_err()
        .is::<Interrupted>());
    Ok(())
}

#[tokio::test]
async fn replay_admission_candidate_loses_to_interrupt_on_the_same_cas() -> Result<()> {
    let (_server, writer, canceller) = stores().await?;
    let root = root(&writer).await?;
    let tool = start(&writer, &root, "replayed-tool", OperationKind::Tool).await?;
    let admission = action(
        "replay-attempt",
        GateAction::AdmitWork {
            input: json!({"request": "original"}),
        },
    );
    let paused = pause(writer.clone(), tool.clone(), admission.clone()).await?;
    let candidate = paused.receipt.clone();
    canceller.interrupt(&scope(&root), "stop").await?;
    assert!(paused.publish().await?.is_none());
    assert!(writer.committed_decision(&candidate).await.is_err());
    assert!(writer
        .commit_if_admissible(&tool, admission)
        .await
        .unwrap_err()
        .is::<Interrupted>());
    Ok(())
}

#[tokio::test]
async fn bridge_owner_handover_and_new_generation_fence_old_context() -> Result<()> {
    let (_server, store, _) = stores().await?;
    let first = physical_root(&store).await?;
    let old = store.activate_gate(&first.reference).await?;
    store.claim(&first.reference, owner(2)).await?;
    store
        .claim(
            &first.reference,
            Owner {
                instance_id: owner(2).instance_id,
                fence: 3,
            },
        )
        .await?;
    assert!(store
        .commit_if_admissible(
            &old,
            action("stale", GateAction::AdmitWork { input: json!({}) })
        )
        .await
        .is_err());
    let current = store.gate_context(old.gate_root(), old.operation()).await?;
    store
        .owner_stopped(&first.reference, current.owner())
        .await?;
    let second = store.session("bridge-session", None, Some("g2")).await?;
    store.claim(&second.reference, owner(4)).await?;
    let new = store.activate_gate(&second.reference).await?;
    assert_eq!(new.gate_root(), old.gate_root());
    assert!(store
        .commit_if_admissible(
            &current,
            action("late", GateAction::AdmitWork { input: json!({}) })
        )
        .await
        .is_err());
    store
        .commit_if_admissible(
            &new,
            action("fresh", GateAction::AdmitWork { input: json!({}) }),
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn server_owner_claim_candidate_cannot_win_after_interrupt() -> Result<()> {
    let (_server, writer, canceller) = stores().await?;
    let root = root(&writer).await?;
    let tool = start(&writer, &root, "claimed-tool", OperationKind::Tool).await?;
    let claim = action("server-claim", GateAction::ReplaceOwner { owner: owner(2) });
    let paused = pause(writer.clone(), tool.clone(), claim.clone()).await?;
    canceller.interrupt(&scope(&root), "stop").await?;
    assert!(paused.publish().await?.is_none());
    assert!(writer
        .commit_if_admissible(&tool, claim)
        .await
        .unwrap_err()
        .is::<Interrupted>());
    Ok(())
}
