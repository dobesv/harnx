use super::*;
use harnx_a2a_server::fault_injection::Boundary;

#[tokio::test]
async fn lost_claim_update_and_release_acks_resolve_exact_operation_once() -> Result<()> {
    let f = Fixture::start().await?;
    let lease = f.lease("owner").await?;
    let hooks = f.a.context_fault_hooks();
    hooks.lose_next_context_ack();
    let claim =
        f.a.prepare_context_claim(
            ContextIdentity {
                storage_key: STORAGE,
                local_id: LOCAL,
            },
            &lease,
            "claim",
        )
        .await?;
    let claimed = f.a.commit_context(&claim).await?;
    let retry = f.b.commit_context(&claim).await?;
    assert_eq!(retry.revision, claimed.revision);
    hooks.lose_next_context_ack();
    let active = f.active(&f.a, &claimed).await?;
    let release =
        f.a.prepare_context_release(STORAGE, &active.version()?, "release")
            .await?;
    hooks.lose_next_context_ack();
    let released = f.a.commit_context(&release).await?;
    assert_eq!(
        f.b.commit_context(&release).await?.revision,
        released.revision
    );
    assert!(released.document.owner.is_none());
    let task = released
        .document
        .state
        .active
        .context("release retains active")?;
    assert_eq!(task.admission.prompt_id, "stable-prompt");
    assert_eq!(
        hooks.count(Boundary::ContextCas),
        3,
        "retries didn't append duplicate context writes"
    );
    lease.release().await?;
    Ok(())
}

#[tokio::test]
async fn same_operation_id_with_different_payload_never_recovers_as_success() -> Result<()> {
    let f = Fixture::start().await?;
    let lease = f.lease("owner").await?;
    let claimed = f.claim(&f.a, &lease, "claim").await?;
    let noop =
        f.a.prepare_context_update(STORAGE, &claimed.version()?, "same-id", |_| {})
            .await?;
    let active = active_task();
    let different =
        f.b.prepare_context_update(STORAGE, &claimed.version()?, "same-id", |state| {
            state.active = Some(active)
        })
        .await?;
    f.a.commit_context(&noop).await?;
    assert_authority_error(
        f.b.commit_context(&different).await.unwrap_err(),
        AuthorityError::OperationMismatch,
    );
    assert!(f
        .b
        .read_context(STORAGE)
        .await?
        .context("context")?
        .document
        .state
        .active
        .is_none());
    lease.release().await?;
    Ok(())
}

#[tokio::test]
async fn takeover_during_lost_ack_does_not_rebase_or_report_successor_as_old_write() -> Result<()> {
    tokio::time::timeout(crate::support::DEADLINE, takeover_during_ack())
        .await
        .context("ack takeover deadline")?
}
async fn takeover_during_ack() -> Result<()> {
    let f = Fixture::start().await?;
    let lease_a = f.lease("owner-a").await?;
    let claimed = f.claim(&f.a, &lease_a, "claim-a").await?;
    let write =
        f.a.prepare_context_update(STORAGE, &claimed.version()?, "old-update", |_| {})
            .await?;
    let hooks = f.a.context_fault_hooks();
    let mut ack = hooks.pause(Boundary::ContextCas, 2);
    hooks.lose_next_context_ack();
    let store = f.a.clone();
    let send = tokio::spawn(async move { store.commit_context(&write).await });
    ack.reached().await;
    let applied =
        f.b.read_context(STORAGE)
            .await?
            .context("applied before ack")?;
    assert_eq!(applied.document.last_operation.id, "old-update");
    lease_a.release().await?;
    let lease_b = f.lease("owner-b").await?;
    let successor = f.claim(&f.b, &lease_b, "takeover-b").await?;
    drop(ack);
    assert_authority_error(send.await?.unwrap_err(), AuthorityError::Conflict);
    let read = f.a.read_context(STORAGE).await?.context("successor")?;
    assert_eq!(read.revision, successor.revision);
    assert_eq!(read.document.owner.as_ref().unwrap().boot_id, "owner-b");
    lease_b.release().await?;
    Ok(())
}

#[tokio::test]
async fn delayed_initial_claim_loses_to_newer_lease_candidate_cas() -> Result<()> {
    let f = Fixture::start().await?;
    let lease_a = f.lease("owner-a").await?;
    let old =
        f.a.prepare_context_claim(
            ContextIdentity {
                storage_key: STORAGE,
                local_id: LOCAL,
            },
            &lease_a,
            "old-claim",
        )
        .await?;
    lease_a.release().await?;
    let lease_b = f.lease("owner-b").await?;
    let successor = f.claim(&f.b, &lease_b, "new-claim").await?;
    assert_authority_error(
        f.a.commit_context(&old).await.unwrap_err(),
        AuthorityError::Conflict,
    );
    assert_eq!(
        f.a.read_context(STORAGE)
            .await?
            .context("context")?
            .revision,
        successor.revision
    );
    lease_b.release().await?;
    Ok(())
}
