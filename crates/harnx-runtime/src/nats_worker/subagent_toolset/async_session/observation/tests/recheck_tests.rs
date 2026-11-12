use super::{fixture::*, *};

async fn pending(f: &Fixture) -> Result<(Scope, Snapshot, Budget)> {
    f.dispatch().await?;
    let budget = Budget::new(WaitLimit::Seconds(120), BudgetStops::default());
    let handle = PromptHandle {
        session_id: "child".into(),
        prompt_id: "original-prompt".into(),
    };
    let scope = match Scope::resolve(&f.toolset, &handle, &f.wait_context(), &budget).await {
        Ok(scope) => scope,
        Err(_) => anyhow::bail!("scope resolution"),
    };
    let mut snapshot = Snapshot::default();
    snapshot.refresh(&f.log, &scope, &budget).await.unwrap();
    assert_eq!(snapshot.status(), SessionPromptStatus::Pending);
    Ok((scope, snapshot, budget))
}

#[tokio::test]
async fn terminal_committed_after_snapshot_suppresses_repair_and_preserves_first_result(
) -> Result<()> {
    let f = Fixture::new().await?;
    let (scope, mut snapshot, budget) = pending(&f).await?;
    f.append(end(1)).await?;
    block(&f).await?;
    let result = super::super::repair::repair_once(&f.toolset, &scope, &mut snapshot, &budget)
        .await
        .unwrap();
    assert_eq!(result, None);
    assert_eq!(snapshot.status(), SessionPromptStatus::Completed);
    assert_eq!(snapshot.output_end(), 2);
    assert_eq!(f.activation_count().await?, 1);
    Ok(())
}

#[tokio::test]
async fn purged_original_after_cached_pending_snapshot_cannot_be_repaired() -> Result<()> {
    let f = Fixture::new().await?;
    let (scope, mut snapshot, budget) = pending(&f).await?;
    // Keep a tail row, so the incremental scanner cannot infer this purge from
    // a missing future sequence. Repair must revalidate the exact source row.
    f.append(fixture::message(
        "partial",
        harnx_core::message::MessageRole::Assistant,
        "partial",
    ))
    .await?;
    let stream =
        f.js.get_stream(crate::nats_session_log::stream_name_for_session(
            f.log.storage_key(),
        ))
        .await?;
    stream.delete_message(1).await?;
    let result =
        super::super::repair::repair_once(&f.toolset, &scope, &mut snapshot, &budget).await;
    assert_eq!(result, Err(Stop::Unavailable));
    assert_eq!(f.activation_count().await?, 1);
    Ok(())
}
