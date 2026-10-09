use super::*;
use harnx_a2a_server::store::{TaskChanges, TaskSeed, TaskVersion};

#[tokio::test]
async fn active_authority_overrides_stale_archives_and_index_repair_input() -> Result<()> {
    let f = Fixture::start().await?;
    let stale = active_task().snapshot;
    f.a.create_task(
        STORAGE,
        TaskSeed {
            task: stale.task,
            user_msg_id: String::new(),
            user_msg_seq: 0,
            execution_id: String::new(),
        },
    )
    .await?;
    let lease = f.lease("owner").await?;
    let claim = f.claim(&f.a, &lease, "claim").await?;
    let active = f.active(&f.a, &claim).await?;
    let mut fake = active
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .snapshot
        .clone();
    fake.revision = 999;
    fake.task.status.state = a2a_lf::TaskState::Failed;
    f.b.repair_index(STORAGE, &fake).await?;
    let (index, _) = f
        .metadata
        .get_a2a_task_index(STORAGE)
        .await?
        .context("index")?;
    assert_eq!(index.entries[0].task_revision, 1);
    assert_eq!(
        index.entries[0].state,
        harnx_runtime::nats_session_metadata::TaskState::Working
    );
    let read = f.b.get_task(STORAGE, TASK).await?.context("task")?;
    assert_eq!(read.task.status.state, a2a_lf::TaskState::Working);
    assert_authority_error(
        f.b.update_task(
            TaskVersion {
                storage_key: STORAGE,
                task_id: TASK,
                revision: 1,
            },
            TaskChanges {
                status: Some(fake.task.status),
                ..Default::default()
            },
        )
        .await
        .unwrap_err(),
        AuthorityError::LegacyWrite,
    );
    assert!(f.b.archive_context_terminal(STORAGE, TASK).await.is_err());
    lease.release().await?;
    Ok(())
}

#[tokio::test]
async fn terminal_snapshots_are_immutable_and_retire_only_after_durable_projections() -> Result<()>
{
    let f = Fixture::start().await?;
    let lease = f.lease("owner").await?;
    let claim = f.claim(&f.a, &lease, "claim").await?;
    let active = f.active(&f.a, &claim).await?;
    let terminal = settle(&f, &active).await?;
    assert!(f
        .a
        .prepare_context_update(STORAGE, &terminal.version()?, "premature-clear", |state| {
            state.active = None
        })
        .await
        .is_err());
    assert!(f
        .a
        .prepare_context_update(STORAGE, &terminal.version()?, "mutate-terminal", |state| {
            state.active.as_mut().unwrap().snapshot.task.status.state = a2a_lf::TaskState::Canceled;
        })
        .await
        .is_err());
    assert!(f
        .a
        .prepare_context_update(STORAGE, &terminal.version()?, "fake-archive", |state| {
            state.active.as_mut().unwrap().projections.archive = true;
        })
        .await
        .is_err());
    let archive = f.a.archive_context_terminal(STORAGE, TASK).await?;
    let retry = f.b.archive_context_terminal(STORAGE, TASK).await?;
    assert_eq!(serde_json::to_value(archive)?, serde_json::to_value(retry)?);
    let projected = mark_projections(&f, &terminal).await?;
    let clear =
        f.a.prepare_context_update(STORAGE, &projected.version()?, "retire", |state| {
            state.active = None
        })
        .await?;
    f.a.commit_context(&clear).await?;
    assert_eq!(
        f.b.get_task(STORAGE, TASK)
            .await?
            .context("terminal archive")?
            .stream_seq,
        7
    );
    lease.release().await?;
    Ok(())
}

async fn settle(f: &Fixture, active: &ContextSnapshot) -> Result<ContextSnapshot> {
    let write =
        f.a.prepare_context_update(STORAGE, &active.version()?, "terminal", |state| {
            let task = state.active.as_mut().unwrap();
            task.snapshot.task.status.state = a2a_lf::TaskState::Failed;
            task.snapshot.stream_seq = 7;
            task.publication.stream_seq = 7;
        })
        .await?;
    f.a.commit_context(&write).await
}
async fn mark_projections(f: &Fixture, terminal: &ContextSnapshot) -> Result<ContextSnapshot> {
    let write =
        f.a.prepare_context_update(STORAGE, &terminal.version()?, "projections", |state| {
            let task = state.active.as_mut().unwrap();
            task.projections = TerminalProjections {
                archive: true,
                message_mapping: true,
                final_event: true,
            };
            task.stop_confirmed = true;
        })
        .await?;
    f.a.commit_context(&write).await
}

#[tokio::test]
async fn old_terminal_records_default_cursor_zero_and_remain_readable() -> Result<()> {
    let f = Fixture::start().await?;
    let mut record = active_task().snapshot;
    record.task.status.state = a2a_lf::TaskState::Completed;
    let mut old = serde_json::to_value(&record)?;
    old.as_object_mut().unwrap().remove("stream_seq");
    let key = harnx_runtime::nats_session_metadata::a2a_task_key(
        STORAGE,
        "01234567-89ab-cdef-0123-456789abcdef",
    );
    f.metadata
        .kv_store()
        .create(key, serde_json::to_vec(&old)?.into())
        .await?;
    let lease = f.lease("owner").await?;
    f.claim(&f.a, &lease, "claim").await?;
    let read = f.b.get_task(STORAGE, TASK).await?.context("old terminal")?;
    assert_eq!(read.stream_seq, 0);
    assert_eq!(read.task.status.state, a2a_lf::TaskState::Completed);
    lease.release().await?;
    Ok(())
}

#[tokio::test]
async fn oversized_authority_rejected_without_partial_write() -> Result<()> {
    let f = Fixture::start().await?;
    let lease = f.lease("owner").await?;
    let claim = f.claim(&f.a, &lease, "claim").await?;
    let mut active = active_task();
    active.message.fingerprint = "x".repeat(f.metadata.max_payload());
    let write =
        f.a.prepare_context_update(STORAGE, &claim.version()?, "oversized", |state| {
            state.active = Some(active)
        })
        .await?;
    let error = f.a.commit_context(&write).await.unwrap_err();
    assert!(error.to_string().contains("exceeds NATS payload budget"));
    assert_eq!(
        f.b.read_context(STORAGE)
            .await?
            .context("context")?
            .revision,
        claim.revision
    );
    lease.release().await?;
    Ok(())
}

#[tokio::test]
async fn former_owner_can_archive_committed_terminal_after_release_and_takeover() -> Result<()> {
    let f = Fixture::start().await?;
    let lease_a = f.lease("owner-a").await?;
    let claimed = f.claim(&f.a, &lease_a, "claim-a").await?;
    let active = f.active(&f.a, &claimed).await?;
    let terminal = settle(&f, &active).await?;
    let release =
        f.a.prepare_context_release(STORAGE, &terminal.version()?, "release-a")
            .await?;
    f.a.commit_context(&release).await?;
    assert_authority_error(
        f.a.prepare_context_update(STORAGE, &terminal.version()?, "released-update", |_| {})
            .await
            .unwrap_err(),
        AuthorityError::StaleOwner,
    );
    let archive = f.a.archive_context_terminal(STORAGE, TASK).await?;
    lease_a.release().await?;
    let lease_b = f.lease("owner-b").await?;
    let successor = f.claim(&f.b, &lease_b, "claim-b").await?;
    let retry = f.a.archive_context_terminal(STORAGE, TASK).await?;
    assert_eq!(
        serde_json::to_value(&archive)?,
        serde_json::to_value(&retry)?
    );
    let projected = mark_projections(&f, &successor).await?;
    let clear =
        f.b.prepare_context_update(STORAGE, &projected.version()?, "retire-b", |state| {
            state.active = None
        })
        .await?;
    f.b.commit_context(&clear).await?;
    assert!(f
        .a
        .prepare_context_update(
            STORAGE,
            &f.b.read_context(STORAGE).await?.unwrap().version()?,
            "reuse-id",
            |state| state.active = Some(active_task())
        )
        .await
        .is_err());
    lease_b.release().await?;
    Ok(())
}

#[tokio::test]
async fn task_watch_observes_authority_transition_instead_of_unwritten_legacy_key() -> Result<()> {
    use futures::StreamExt;
    let f = Fixture::start().await?;
    let lease = f.lease("owner").await?;
    let claim = f.claim(&f.a, &lease, "claim").await?;
    let active = f.active(&f.a, &claim).await?;
    let mut watch = f.b.watch_task(STORAGE, TASK).await?;
    let terminal = settle(&f, &active).await?;
    tokio::time::timeout(crate::support::DEADLINE, watch.next())
        .await?
        .context("terminal watch state")??;
    let read = f.b.get_task(STORAGE, TASK).await?.context("terminal")?;
    assert_eq!(
        read.revision,
        terminal
            .document
            .state
            .active
            .as_ref()
            .unwrap()
            .snapshot
            .revision
    );
    assert_eq!(read.task.status.state, a2a_lf::TaskState::Failed);
    lease.release().await?;
    Ok(())
}

#[tokio::test]
async fn deleted_authority_is_explicit_failure_not_epoch_reset_or_legacy_fallback() -> Result<()> {
    let f = Fixture::start().await?;
    let lease = f.lease("owner").await?;
    let claim = f.claim(&f.a, &lease, "claim").await?;
    f.metadata
        .kv_store()
        .purge(&context_authority_key(STORAGE))
        .await?;
    assert_authority_error(
        f.b.read_context(STORAGE).await.unwrap_err(),
        AuthorityError::Deleted,
    );
    assert_authority_error(
        f.a.prepare_context_claim(
            ContextIdentity {
                storage_key: STORAGE,
                local_id: LOCAL,
            },
            &lease,
            "reset-epoch",
        )
        .await
        .unwrap_err(),
        AuthorityError::Deleted,
    );
    assert_eq!(claim.document.epoch, 1);
    lease.release().await?;
    Ok(())
}
