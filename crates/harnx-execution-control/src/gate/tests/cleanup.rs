use super::*;
use crate::CleanupStatus;

#[tokio::test]
async fn cleanup_scan_recovers_stops_after_restart_and_generation_replacement() -> Result<()> {
    let (_server, store, restarted) = stores().await?;
    let g1 = root(&store).await?;
    let stop = store.interrupt(&scope(&g1), "stable-cancel").await?;
    // No wake, physical projection or resource metadata was ever written.
    let g2 = replace(&store, &g1).await?;
    let scopes = restarted.cleanup_scopes().await?;
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].stop, stop);
    assert_eq!(scopes[0].context, g1);
    assert_eq!(scopes[0].cleanup.state, CleanupState::Pending);
    restarted
        .record_cleanup(&g1, CleanupStatus::unconfirmed("owner unreachable"))
        .await?;
    assert_eq!(
        restarted.interrupt(&scope(&g1), "stable-cancel").await?,
        stop
    );
    restarted
        .record_cleanup(&g1, CleanupStatus::confirmed())
        .await?;
    assert!(restarted.cleanup_scopes().await?.is_empty());
    assert_eq!(
        restarted.gate_cleanup(&g1).await?.state,
        CleanupState::Confirmed
    );
    assert_eq!(
        restarted.gate_cleanup(&g2).await?.state,
        CleanupState::Pending
    );
    restarted
        .commit_if_admissible(&g2, output("g2-model", OutputKind::ModelResponse))
        .await?;
    Ok(())
}

#[tokio::test]
async fn cleanup_confirmation_requires_evidence_and_late_pending_cannot_reopen_it() -> Result<()> {
    let (_server, store, _) = stores().await?;
    let root = root(&store).await?;
    store.interrupt(&scope(&root), "interrupt").await?;
    let malformed = CleanupStatus {
        state: CleanupState::Confirmed,
        ..CleanupStatus::default()
    };
    assert!(store.record_cleanup(&root, malformed).await.is_err());
    let late = pause(
        store.clone(),
        root.clone(),
        action(
            "old-progress",
            GateAction::CleanupUpdate {
                cleanup: CleanupStatus::default(),
            },
        ),
    )
    .await?;
    store
        .record_cleanup(&root, CleanupStatus::confirmed())
        .await?;
    assert!(late.publish().await?.is_none());
    store
        .record_cleanup(&root, CleanupStatus::unconfirmed("late lost ack"))
        .await?;
    assert_eq!(
        store.gate_cleanup(&root).await?.state,
        CleanupState::Confirmed
    );
    Ok(())
}
