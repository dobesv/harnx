use super::*;

#[tokio::test]
async fn recovery_candidate_loses_to_original_tool_stop_before_branch_selection() -> Result<()> {
    let (_server, worker, canceller) = stores().await?;
    let root = root(&worker).await?;
    let tool = start(&worker, &root, "tool", OperationKind::Tool).await?;
    let admission = action(
        "recover",
        GateAction::AdmitRecovery {
            original: tool.clone(),
        },
    );
    let paused = pause(worker.clone(), root.clone(), admission.clone()).await?;
    let stop = canceller.interrupt(&scope(&tool), "stop-tool").await?;
    assert!(paused.publish().await?.is_none());
    let error = worker
        .commit_if_admissible(&root, admission)
        .await
        .unwrap_err();
    assert_eq!(error.downcast::<Interrupted>()?.stop, stop);
    // Parent itself remains active. Its state cannot override the source stop.
    worker
        .commit_if_admissible(
            &root,
            action("parent", GateAction::AdmitWork { input: json!(null) }),
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn recovery_retry_revalidates_source_stop_and_receiving_owner() -> Result<()> {
    let (_server, worker, canceller) = stores().await?;
    let root = root(&worker).await?;
    let tool = start(&worker, &root, "tool", OperationKind::Tool).await?;
    let admission = action(
        "recover",
        GateAction::AdmitRecovery {
            original: tool.clone(),
        },
    );
    worker
        .commit_if_admissible(&root, admission.clone())
        .await?;
    let paused = pause(worker.clone(), root.clone(), admission.clone()).await?;
    worker
        .commit_if_admissible(
            &root,
            action("owner", GateAction::ReplaceOwner { owner: owner(2) }),
        )
        .await?;
    assert!(paused.publish().await?.is_none());
    assert!(worker
        .commit_if_admissible(&root, admission.clone())
        .await
        .is_err());
    canceller.interrupt(&scope(&tool), "stop-tool").await?;
    let receiver = worker
        .gate_context(root.gate_root(), root.operation())
        .await?;
    let error = worker
        .commit_if_admissible(&receiver, action("new-attempt", admission.kind))
        .await
        .unwrap_err();
    assert!(error.is::<Interrupted>());
    Ok(())
}

#[tokio::test]
async fn completed_tool_can_recover_without_reopening_but_not_into_g2() -> Result<()> {
    let (_server, store, _) = stores().await?;
    let root = root(&store).await?;
    let tool = start(&store, &root, "tool", OperationKind::Tool).await?;
    store
        .commit_if_admissible(&tool, action("finish", GateAction::FinishWork))
        .await?;
    let recovery = GateAction::AdmitRecovery { original: tool };
    store
        .commit_if_admissible(&root, action("recover", recovery.clone()))
        .await?;
    store.interrupt(&scope(&root), "stop").await?;
    let next = replace(&store, &root).await?;
    let error = store
        .commit_if_admissible(&next, action("g2-recover", recovery))
        .await
        .unwrap_err();
    assert!(error.is::<Interrupted>());
    Ok(())
}

#[tokio::test]
async fn recovery_history_follows_retained_winners_not_bucket_enumeration() -> Result<()> {
    let (_server, store, restarted) = stores().await?;
    for generation in ["g1", "g2", "g3"] {
        let operation = store.session("history", None, Some(generation)).await?;
        let owner = owner(1);
        store.claim(&operation.reference, owner.clone()).await?;
        store
            .reserve_prompt(&operation.reference, generation)
            .await?;
        store
            .commit_prompt(&operation.reference, generation, 1)
            .await?;
        store
            .record_coverage(&operation.reference, &owner, 1, false)
            .await?;
        store.seal(&operation.reference, &owner).await?;
        store.owner_stopped(&operation.reference, &owner).await?;
    }
    // An uninstalled candidate is not history, even if a bucket listing finds it.
    store
        .kv
        .put(
            "sessions/history/operations/uninstalled",
            "incomplete candidate".into(),
        )
        .await?;
    let history = restarted.recovery_history("history").await?;
    assert_eq!(
        history
            .iter()
            .map(|record| record.reference.execution_id.as_str())
            .collect::<Vec<_>>(),
        ["g3", "g2", "g1"]
    );
    assert_eq!(history[2].admissions["g1"], Some(1));
    assert!(store.get(&history[2].reference).await?.is_none());
    Ok(())
}
