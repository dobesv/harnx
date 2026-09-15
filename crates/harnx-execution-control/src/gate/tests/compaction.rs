use super::*;

#[tokio::test]
async fn checkpoint_collects_losing_candidates_without_erasing_proofs() -> Result<()> {
    let (_server, writer, canceller) = stores().await?;
    let root = root(&writer).await?;
    let committed = writer
        .commit_if_admissible(&root, output("model", OutputKind::ModelResponse))
        .await?;
    let paused = pause(
        writer.clone(),
        root.clone(),
        output("late", OutputKind::Progress),
    )
    .await?;
    let garbage = paused.receipt.clone();
    let old_epoch = writer.gate_snapshot(root.gate_root()).await?.head.epoch;
    let stop = canceller.interrupt(&scope(&root), "stop").await?;
    let checkpoint = writer.checkpoint_gate(root.gate_root()).await?;
    assert_ne!(checkpoint.epoch, old_epoch);
    assert!(paused.publish().await?.is_none());
    writer.committed_decision(&committed).await?;
    assert_eq!(writer.interrupt(&scope(&root), "stop").await?, stop);
    let key = ledger::decision_key(&ledger::Proof {
        epoch: old_epoch,
        receipt: garbage,
    })?;
    assert!(writer.kv.get(key).await?.is_none());
    writer.checkpoint_gate(root.gate_root()).await?;
    assert_eq!(
        writer.gate_stop(root.gate_root(), root.operation()).await?,
        Some(stop)
    );
    Ok(())
}

#[tokio::test]
async fn physical_pruning_and_g2_do_not_remove_g1_stop_or_reply_fences() -> Result<()> {
    let (_server, store, _) = stores().await?;
    // The legacy graph here is a physical projection only. Gate tests never read
    // its Running state as write authority, even after it is retired/replaced.
    let physical = store.session("gate-session", None, Some("g1")).await?;
    store.claim(&physical.reference, owner(1)).await?;
    let root = root(&store).await?;
    let tool = start(&store, &root, "tool", OperationKind::Tool).await?;
    store
        .child(tool.operation.clone(), root.operation.clone())
        .await?;
    store.claim(tool.operation(), owner(1)).await?;
    let reply = store
        .commit_if_admissible(&tool, output("reply", OutputKind::ToolReply))
        .await?;
    let stop = store.interrupt(&scope(&root), "stop").await?;
    store
        .accept_interrupt(root.operation(), "stop", "projection")
        .await?;
    store.owner_stopped(tool.operation(), &owner(1)).await?;
    store
        .record_coverage(root.operation(), &owner(1), 0, true)
        .await?;
    store.owner_stopped(root.operation(), &owner(1)).await?;
    store.session("gate-session", None, Some("g2")).await?;
    let g2 = replace(&store, &root).await?;
    assert!(store.get(tool.operation()).await?.is_none());
    assert!(store.get(root.operation()).await?.is_none());
    store.checkpoint_gate(root.gate_root()).await?;
    assert_eq!(
        store.gate_stop(root.gate_root(), tool.operation()).await?,
        Some(stop)
    );
    assert!(store
        .commit_if_admissible(&tool, output("late", OutputKind::ToolReply))
        .await
        .is_err());
    let consume = action(
        "consume",
        GateAction::ConsumeReply {
            producer: tool,
            reply,
        },
    );
    assert!(store
        .commit_if_admissible(&root, consume.clone())
        .await
        .is_err());
    assert!(store.commit_if_admissible(&g2, consume).await.is_err());
    store
        .commit_if_admissible(&g2, output("new", OutputKind::ModelResponse))
        .await?;
    Ok(())
}

#[tokio::test]
async fn persistent_index_updates_remain_bounded_and_survive_checkpoint() -> Result<()> {
    let (_server, store, _) = stores().await?;
    let root = root(&store).await?;
    let mut receipts = Vec::new();
    for index in 0..24 {
        receipts.push(
            store
                .commit_if_admissible(
                    &root,
                    output(&format!("progress-{index}"), OutputKind::Progress),
                )
                .await?,
        );
    }
    let head = store
        .kv
        .get(ledger::anchor_key(root.gate_root()))
        .await?
        .unwrap();
    assert!(
        head.len() < 1024,
        "head must not contain the accumulating action log"
    );
    store.checkpoint_gate(root.gate_root()).await?;
    for receipt in receipts {
        assert_eq!(store.committed_decision(&receipt).await?.receipt, receipt);
    }
    store
        .commit_if_admissible(&root, output("after-checkpoint", OutputKind::Progress))
        .await?;
    Ok(())
}
