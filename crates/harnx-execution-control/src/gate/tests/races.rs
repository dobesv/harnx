use super::*;

#[tokio::test]
async fn interrupt_wins_against_persisted_output_candidate() -> Result<()> {
    let (_server, writer, canceller) = stores().await?;
    let root = root(&writer).await?;
    let tool = start(&writer, &root, "tool", OperationKind::Tool).await?;
    let late = output("reply", OutputKind::ToolReply);
    let paused = pause(writer.clone(), tool.clone(), late.clone()).await?;
    let candidate = paused.receipt.clone();
    let stop = canceller.interrupt(&scope(&root), "cancel").await?;
    assert!(paused.publish().await?.is_none());
    assert!(writer.committed_decision(&candidate).await.is_err());
    assert!(writer.commit_if_admissible(&tool, late).await.is_err());
    assert_eq!(
        writer.gate_stop(root.gate_root(), tool.operation()).await?,
        Some(stop)
    );
    Ok(())
}

#[tokio::test]
async fn output_wins_but_later_stop_rejects_consumption() -> Result<()> {
    let (_server, writer, canceller) = stores().await?;
    let root = root(&writer).await?;
    let tool = start(&writer, &root, "tool", OperationKind::Tool).await?;
    let snapshot = canceller.gate_snapshot(root.gate_root()).await?;
    let decision = crate::StopDecision {
        cancellation_id: "cancel".into(),
        accepted_at: chrono::Utc::now(),
        reason: "user interrupt".into(),
    };
    let stopped = canceller
        .prepare_stop(&snapshot, &scope(&root), &decision)
        .await?;
    let paused = pause(
        writer.clone(),
        tool.clone(),
        output("reply", OutputKind::ToolReply),
    )
    .await?;
    let reply = paused.publish().await?.context("output wins")?;
    assert!(canceller.publish_prepared(stopped).await?.is_none());
    canceller.interrupt(&scope(&root), "cancel").await?;
    let committed = writer.committed_decision(&reply).await?;
    assert_eq!(
        committed.action,
        CommittedAction::Action {
            action: output("reply", OutputKind::ToolReply)
        }
    );
    let consume = action(
        "consume",
        GateAction::ConsumeReply {
            producer: tool,
            reply,
        },
    );
    assert!(writer.commit_if_admissible(&root, consume).await.is_err());
    Ok(())
}

#[tokio::test]
async fn root_stop_fences_nested_registration_and_all_existing_descendants() -> Result<()> {
    let (_server, writer, canceller) = stores().await?;
    let root = root(&writer).await?;
    let tool = start(&writer, &root, "tool", OperationKind::Tool).await?;
    let session = start(&writer, &tool, "child-session", OperationKind::Session).await?;
    let nested = start(&writer, &session, "nested", OperationKind::Tool).await?;
    let pending = child(&nested, "too-late", OperationKind::Session);
    let action = action(
        "late-register",
        GateAction::StartWork {
            child: pending.clone(),
            input: json!({"prompt": "late"}),
        },
    );
    let paused = pause(writer.clone(), nested.clone(), action.clone()).await?;
    canceller.interrupt(&scope(&root), "cancel").await?;
    assert!(paused.publish().await?.is_none());
    assert!(writer.commit_if_admissible(&nested, action).await.is_err());
    for descendant in [&tool, &session, &nested] {
        assert!(writer
            .commit_if_admissible(descendant, output("late", OutputKind::Progress))
            .await
            .is_err());
        assert!(writer
            .gate_stop(root.gate_root(), descendant.operation())
            .await?
            .is_some());
    }
    assert!(writer
        .gate_stop(root.gate_root(), &pending.operation)
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn committed_child_registration_belongs_to_stopped_subtree() -> Result<()> {
    let (_server, writer, canceller) = stores().await?;
    let root = root(&writer).await?;
    let child = child(&root, "tool", OperationKind::Tool);
    let ctx = child.context(&root);
    let paused = pause(
        writer.clone(),
        root.clone(),
        action(
            "register",
            GateAction::StartWork {
                child,
                input: json!({"command": "work"}),
            },
        ),
    )
    .await?;
    let proof = paused.publish().await?.context("registration wins")?;
    canceller.interrupt(&scope(&root), "cancel").await?;
    writer.committed_decision(&proof).await?;
    assert!(writer
        .commit_if_admissible(&ctx, output("late", OutputKind::ToolReply))
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn lost_stop_ack_resolves_same_commit_after_head_and_generation_advance() -> Result<()> {
    let (server, writer, canceller) = stores().await?;
    let root = root(&writer).await?;
    let snapshot = canceller.gate_snapshot(root.gate_root()).await?;
    let stop = crate::StopDecision {
        cancellation_id: "stable".into(),
        accepted_at: chrono::Utc::now(),
        reason: "user interrupt".into(),
    };
    let candidate = canceller
        .prepare_stop(&snapshot, &scope(&root), &stop)
        .await?;
    // Discard the successful CAS receipt, simulating a lost response/crashed caller.
    let _lost_response = canceller.publish_prepared(candidate).await?;
    let g2 = replace(&writer, &root).await?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let recovered = ExecutionStore::ensure(&js, 1).await?;
    let first = recovered.interrupt(&scope(&root), "stable").await?;
    let second = recovered.interrupt(&scope(&root), "stable").await?;
    assert_eq!(first, second);
    assert_eq!(first.decision, stop);
    assert!(recovered.interrupt(&scope(&g2), "stable").await.is_err());
    recovered
        .commit_if_admissible(&g2, output("new", OutputKind::ModelResponse))
        .await?;
    assert!(recovered
        .gate_stop(g2.gate_root(), g2.operation())
        .await?
        .is_none());
    Ok(())
}

#[tokio::test]
async fn stale_positive_idempotency_snapshot_cannot_authorize_after_stop() -> Result<()> {
    let (_server, writer, canceller) = stores().await?;
    let root = root(&writer).await?;
    let action = output("output", OutputKind::ModelResponse);
    let receipt = writer.commit_if_admissible(&root, action.clone()).await?;
    let paused = pause(writer.clone(), root.clone(), action.clone()).await?;
    assert_eq!(paused.receipt, receipt);
    canceller.interrupt(&scope(&root), "cancel").await?;
    assert!(
        paused.publish().await?.is_none(),
        "even a positive retry validates the root CAS"
    );
    assert!(writer
        .commit_if_admissible(&root, action.clone())
        .await
        .is_err());
    let historical = writer
        .committed_action(&root, &action)
        .await?
        .context("lost response resolves by action identity")?;
    assert_eq!(historical.receipt, receipt);
    writer.committed_decision(&receipt).await?;
    Ok(())
}

#[tokio::test]
async fn owner_handover_cas_fences_an_old_writer_attempt() -> Result<()> {
    let (_server, writer, replacement) = stores().await?;
    let root = root(&writer).await?;
    let tool = start(&writer, &root, "tool", OperationKind::Tool).await?;
    let paused = pause(
        writer.clone(),
        tool.clone(),
        output("reply", OutputKind::ToolReply),
    )
    .await?;
    replacement
        .commit_if_admissible(
            &root,
            action("handover", GateAction::ReplaceOwner { owner: owner(2) }),
        )
        .await?;
    assert!(paused.publish().await?.is_none());
    assert!(writer
        .commit_if_admissible(&tool, output("reply", OutputKind::ToolReply))
        .await
        .is_err());
    Ok(())
}
