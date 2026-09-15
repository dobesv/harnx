use super::*;

#[tokio::test]
async fn exact_action_idempotency_and_single_reply_slot() -> Result<()> {
    let (_server, store, _) = stores().await?;
    let root = root(&store).await?;
    let tool = start(&store, &root, "tool", OperationKind::Tool).await?;
    let reply = output("reply", OutputKind::ToolReply);
    let first = store.commit_if_admissible(&tool, reply.clone()).await?;
    let second = store.commit_if_admissible(&tool, reply.clone()).await?;
    assert_eq!(first, second);
    assert!(store
        .gate_snapshot(root.gate_root())
        .await?
        .head
        .validation_nonce
        .is_some());
    let mut changed = reply;
    if let GateAction::CommitOutput { output } = &mut changed.kind {
        output.payload = json!({"changed": true});
    }
    assert!(store.commit_if_admissible(&tool, changed).await.is_err());
    assert!(store
        .commit_if_admissible(&tool, output("second-slot", OutputKind::ToolReply))
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn completed_producer_reply_can_be_consumed_once_but_not_forged() -> Result<()> {
    let (_server, store, _) = stores().await?;
    let root = root(&store).await?;
    let tool = start(&store, &root, "tool", OperationKind::Tool).await?;
    let reply = store
        .commit_if_admissible(&tool, output("reply", OutputKind::ToolReply))
        .await?;
    store
        .commit_if_admissible(&tool, action("finish", GateAction::FinishWork))
        .await?;
    let mut forged = reply.clone();
    forged.commit_id = uuid::Uuid::now_v7().to_string();
    assert!(store
        .commit_if_admissible(
            &root,
            action(
                "forged",
                GateAction::ConsumeReply {
                    producer: tool.clone(),
                    reply: forged
                }
            )
        )
        .await
        .is_err());
    let consume = action(
        "consume",
        GateAction::ConsumeReply {
            producer: tool.clone(),
            reply: reply.clone(),
        },
    );
    let receipt = store.commit_if_admissible(&root, consume.clone()).await?;
    assert_eq!(store.commit_if_admissible(&root, consume).await?, receipt);
    assert!(store
        .commit_if_admissible(
            &root,
            action(
                "again",
                GateAction::ConsumeReply {
                    producer: tool,
                    reply
                }
            )
        )
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn old_cleanup_and_projection_are_not_new_output_capabilities() -> Result<()> {
    let (_server, store, _) = stores().await?;
    let root = root(&store).await?;
    let output = store
        .commit_if_admissible(&root, output("model", OutputKind::ModelResponse))
        .await?;
    store.interrupt(&scope(&root), "stop").await?;
    let g2 = replace(&store, &root).await?;
    store
        .commit_if_admissible(
            &root,
            action(
                "cleanup",
                GateAction::CleanupUpdate {
                    cleanup: crate::CleanupStatus::confirmed(),
                },
            ),
        )
        .await?;
    let projected = action(
        "project",
        GateAction::ProjectCommitted {
            commit: output.clone(),
            projector: "transcript".into(),
            expected_cursor: None,
        },
    );
    store.commit_if_admissible(&root, projected.clone()).await?;
    store.commit_if_admissible(&root, projected).await?;
    assert_eq!(
        store
            .projection_cursor(root.gate_root(), "transcript")
            .await?,
        Some(output.clone())
    );
    let wrong_context = action(
        "wrong",
        GateAction::ProjectCommitted {
            commit: output,
            projector: "other".into(),
            expected_cursor: None,
        },
    );
    assert!(store
        .commit_if_admissible(&g2, wrong_context)
        .await
        .is_err());
    assert!(store
        .commit_if_admissible(&root, super::output("late", OutputKind::Progress))
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn retry_cannot_restart_a_stopped_child_or_consume_its_old_reply() -> Result<()> {
    let (_server, store, _) = stores().await?;
    let root = root(&store).await?;
    let child = child(&root, "tool", OperationKind::Tool);
    let ctx = child.context(&root);
    let registration = action(
        "register",
        GateAction::StartWork {
            child,
            input: json!({"call": "tool"}),
        },
    );
    store
        .commit_if_admissible(&root, registration.clone())
        .await?;
    let reply = store
        .commit_if_admissible(&ctx, output("reply", OutputKind::ToolReply))
        .await?;
    let consume = action(
        "consume",
        GateAction::ConsumeReply {
            producer: ctx.clone(),
            reply,
        },
    );
    store.commit_if_admissible(&root, consume.clone()).await?;
    let paused = pause(store.clone(), root.clone(), consume.clone()).await?;
    store.interrupt(&scope(&ctx), "stop-child").await?;
    assert!(paused.publish().await?.is_none());
    assert!(store.commit_if_admissible(&root, consume).await.is_err());
    assert!(store
        .commit_if_admissible(&root, registration)
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn identity_registration_and_output_kind_are_checked() -> Result<()> {
    let (_server, store, _) = stores().await?;
    let root = root(&store).await?;
    let tool = start(&store, &root, "tool", OperationKind::Tool).await?;
    for invalid in [
        ExecutionContext::new(
            root.generation.clone(),
            root.gate_root.clone(),
            OperationRef::new("gate-session", "missing"),
            (owner(1), owner(1)),
        ),
        ExecutionContext::new(
            OperationRef::new("gate-session", "wrong-generation"),
            root.gate_root.clone(),
            tool.operation.clone(),
            (owner(1), owner(1)),
        ),
        ExecutionContext::new(
            root.generation.clone(),
            root.gate_root.clone(),
            tool.operation.clone(),
            (owner(2), owner(1)),
        ),
    ] {
        assert!(store
            .commit_if_admissible(&invalid, output("invalid", OutputKind::Progress))
            .await
            .is_err());
    }
    assert!(store
        .commit_if_admissible(&root, output("wrong-kind", OutputKind::ToolReply))
        .await
        .is_err());
    assert!(store
        .commit_if_admissible(&tool, output("wrong-kind", OutputKind::ModelResponse))
        .await
        .is_err());
    assert!(store
        .open_gate(OperationRef::new("gate-session", "other-gate"), owner(1))
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn child_interrupt_preserves_siblings_and_unrelated_gates() -> Result<()> {
    let (_server, store, other) = stores().await?;
    let root = root(&store).await?;
    let child = start(&store, &root, "tool", OperationKind::Tool).await?;
    let sibling = start(&store, &root, "sibling", OperationKind::Tool).await?;
    let independent = other
        .open_gate(OperationRef::new("independent", "g1"), owner(1))
        .await?;
    let paused = pause(
        other.clone(),
        independent.clone(),
        output("progress", OutputKind::Progress),
    )
    .await?;
    store.interrupt(&scope(&child), "stop").await?;
    assert!(
        paused.publish().await?.is_some(),
        "different trees do not contend on one anchor"
    );
    store
        .commit_if_admissible(&sibling, output("reply", OutputKind::ToolReply))
        .await?;
    assert!(store
        .gate_stop(root.gate_root(), root.operation())
        .await?
        .is_none());
    Ok(())
}
