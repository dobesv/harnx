//! Standalone coordination primitives. The legacy runner isn't switched over.
use anyhow::{Context, Result};
use harnx_a2a_server::store::context::*;
use harnx_runtime::nats_lease::{NatsLeaseConfig, NatsSessionLease};
#[cfg(feature = "fault-injection")]
mod acknowledgements;
#[cfg(feature = "fault-injection")]
mod fixed_admission;
#[cfg(feature = "fault-injection")]
mod leader;
mod projections;
pub(crate) mod support;
use support::*;

#[tokio::test]
async fn two_scoped_claimants_choose_one_owner_without_blocking_worker_execution() -> Result<()> {
    let f = Fixture::start().await?;
    let (a, b) = tokio::join!(
        NatsSessionLease::acquire_scoped(f.params("boot-a"), "a2a"),
        NatsSessionLease::acquire_scoped(f.params("boot-b"), "a2a")
    );
    let a = a?;
    let b = b?;
    assert_ne!(a.is_some(), b.is_some());
    let lease = a.or(b).context("winner")?;
    let claimed = f.claim(&f.a, &lease, "claim-winner").await?;
    assert_eq!(
        claimed.document.owner.as_ref().context("owner")?.boot_id,
        lease.worker_id()
    );
    let observed = f.b.read_context(STORAGE).await?.context("sibling read")?;
    assert_eq!(observed.revision, claimed.revision);
    assert!(
        !harnx_runtime::nats_lease::session_has_active_lease(&f.js, STORAGE).await?,
        "scoped lease isn't worker execution ownership"
    );
    let worker =
        NatsSessionLease::acquire_for_execution(f.params("worker"), Some("execution".into()))
            .await?
            .context("worker lease")?;
    assert_eq!(
        worker.key(),
        NatsLeaseConfig::default().key_for_session(STORAGE)
    );
    assert_eq!(lease.key(), "sessions/authority-storage/a2a/lock");
    assert_worker_record(&f, &worker).await?;
    worker.release().await?;
    lease.release().await?;
    Ok(())
}

#[tokio::test]
async fn competing_document_cas_tickets_do_not_rebase() -> Result<()> {
    let f = Fixture::start().await?;
    let lease = f.lease("boot-a").await?;
    let claim_a =
        f.a.prepare_context_claim(
            ContextIdentity {
                storage_key: STORAGE,
                local_id: LOCAL,
            },
            &lease,
            "claim-a",
        )
        .await?;
    let claim_b =
        f.b.prepare_context_claim(
            ContextIdentity {
                storage_key: STORAGE,
                local_id: LOCAL,
            },
            &lease,
            "claim-b",
        )
        .await?;
    let (a, b) = tokio::join!(f.a.commit_context(&claim_a), f.b.commit_context(&claim_b));
    assert_ne!(a.is_ok(), b.is_ok());
    let winner = a.or(b)?;
    let active = f.active(&f.a, &winner).await?;
    let version = active.version()?;
    let update_a =
        f.a.prepare_context_update(STORAGE, &version, "update-a", |_| {})
            .await?;
    let update_b =
        f.b.prepare_context_update(STORAGE, &version, "update-b", |_| {})
            .await?;
    let (a, b) = tokio::join!(f.a.commit_context(&update_a), f.b.commit_context(&update_b));
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(a.or(b)?.document.epoch, 1);
    lease.release().await?;
    Ok(())
}

#[tokio::test]
async fn expiry_takeover_fences_prepared_writes_and_old_revision_release() -> Result<()> {
    let f = Fixture::start().await?;
    let lease_a = f.lease("boot-a").await?;
    let claim = f.claim(&f.a, &lease_a, "claim-a").await?;
    let active = f.active(&f.a, &claim).await?;
    let delayed =
        f.a.prepare_context_update(STORAGE, &active.version()?, "old-update", |_| {})
            .await?;
    let release =
        f.a.prepare_context_release(STORAGE, &active.version()?, "old-release")
            .await?;
    expiry(&f, &lease_a).await?;
    let lease_b = f.lease("boot-b").await?;
    let successor = f.claim(&f.b, &lease_b, "takeover-b").await?;
    assert_eq!(successor.document.epoch, 2);
    assert_eq!(
        successor
            .document
            .state
            .active
            .as_ref()
            .context("retained active")?
            .admission
            .prompt_id,
        "stable-prompt"
    );
    assert_authority_error(
        f.a.commit_context(&delayed)
            .await
            .expect_err("old write applied"),
        AuthorityError::Conflict,
    );
    assert_authority_error(
        f.a.commit_context(&release)
            .await
            .expect_err("old release applied"),
        AuthorityError::Conflict,
    );
    assert_authority_error(
        f.a.prepare_context_update(STORAGE, &active.version()?, "new-old-write", |_| {})
            .await
            .expect_err("old owner accepted"),
        AuthorityError::StaleOwner,
    );
    // A still thinks held after renewal was stopped. Its revision-checked delete
    // cannot remove B's lease, even before A learns ownership was lost.
    lease_a.release().await?;
    assert!(lease_b.revalidate_ownership().await?);
    let latest = f.a.read_context(STORAGE).await?.context("latest")?;
    assert_eq!(latest.revision, successor.revision);
    lease_b.release().await?;
    Ok(())
}

#[tokio::test]
async fn actual_renewal_loss_is_signalled_and_does_not_erase_context_authority() -> Result<()> {
    let f = Fixture::start().await?;
    let lease = f.lease("boot-a").await?;
    let claimed = f.claim(&f.a, &lease, "claim").await?;
    let mut lost = lease.lost_watch();
    let kv = f.js.get_key_value("harnx_leases").await?;
    // Delete at a broker-observed revision. Retrying CAS conflicts here doesn't
    // infer success from elapsed time or mutate the context document.
    tokio::time::timeout(crate::support::DEADLINE, async {
        loop {
            let current = harnx_nats_common::leader_reads::entry(&kv, lease.key())
                .await?
                .context("lease")?;
            match kv
                .delete_expect_revision(lease.key(), Some(current.revision))
                .await
            {
                Ok(()) => break Ok::<_, anyhow::Error>(()),
                Err(error)
                    if error.kind()
                        == async_nats::jetstream::kv::UpdateErrorKind::WrongLastRevision =>
                {
                    tokio::task::yield_now().await
                }
                Err(error) => return Err(error.into()),
            }
        }
    })
    .await??;
    tokio::time::timeout(crate::support::DEADLINE, lost.wait_for(|held| !held)).await??;
    assert!(!lease.is_held());
    let current =
        f.b.read_context(STORAGE)
            .await?
            .context("authority persists")?;
    assert_eq!(current.revision, claimed.revision);
    assert!(f
        .b
        .prepare_context_claim(
            ContextIdentity {
                storage_key: STORAGE,
                local_id: LOCAL
            },
            &lease,
            "lost-claim"
        )
        .await
        .is_err());
    let successor = f.lease("boot-b").await?;
    assert_eq!(
        f.claim(&f.b, &successor, "new-owner").await?.document.epoch,
        2
    );
    successor.release().await?;
    Ok(())
}

#[tokio::test]
async fn owner_boot_task_revision_and_ticket_identity_are_all_checked() -> Result<()> {
    let f = Fixture::start().await?;
    let lease = f.lease("boot-a").await?;
    let claimed = f.claim(&f.a, &lease, "claim").await?;
    let active = f.active(&f.a, &claimed).await?;
    let version = active.version()?;
    assert_wrong_versions(&f, &version).await?;
    assert!(f
        .a
        .prepare_context_update(STORAGE, &version, "replace-message", |state| {
            state.active.as_mut().unwrap().message.fingerprint = "different".into();
        })
        .await
        .is_err());
    assert!(f
        .a
        .prepare_context_update(STORAGE, &version, "rebase-ticket", |state| {
            state.active.as_mut().unwrap().admission.fixed_predecessor += 1;
        })
        .await
        .is_err());
    assert!(f
        .a
        .prepare_context_update(STORAGE, &version, "clear-active", |state| state.active =
            None)
        .await
        .is_err());
    lease.release().await?;
    Ok(())
}

async fn assert_wrong_versions(f: &Fixture, version: &ContextVersion) -> Result<()> {
    let mut wrong = version.clone();
    wrong.owner.boot_id = "another-boot".into();
    assert_authority_error(
        f.b.prepare_context_update(STORAGE, &wrong, "boot", |_| {})
            .await
            .unwrap_err(),
        AuthorityError::StaleOwner,
    );
    wrong = version.clone();
    wrong.owner.epoch += 1;
    assert_authority_error(
        f.b.prepare_context_update(STORAGE, &wrong, "epoch", |_| {})
            .await
            .unwrap_err(),
        AuthorityError::StaleOwner,
    );
    wrong = version.clone();
    wrong.task_id = None;
    assert_authority_error(
        f.b.prepare_context_update(STORAGE, &wrong, "task", |_| {})
            .await
            .unwrap_err(),
        AuthorityError::TaskMismatch,
    );
    wrong = version.clone();
    wrong.revision += 1;
    assert_authority_error(
        f.b.prepare_context_update(STORAGE, &wrong, "revision", |_| {})
            .await
            .unwrap_err(),
        AuthorityError::Conflict,
    );
    Ok(())
}

#[test]
fn lease_scope_rejects_paths_wildcards_and_worker_aliases() {
    harnx_core::require_nextest();
    for scope in ["", "a2a/../", "../", "*", ">", ".", "a2a.lock", "a2a/lock"] {
        assert!(
            NatsLeaseConfig::default()
                .key_for_scope(STORAGE, scope)
                .is_err(),
            "{scope}"
        );
    }
}

#[tokio::test]
async fn snapshot_cursor_cancel_and_pending_event_are_one_cas_and_survive_takeover() -> Result<()> {
    let f = Fixture::start().await?;
    let lease_a = f.lease("owner-a").await?;
    let claim = f.claim(&f.a, &lease_a, "claim-a").await?;
    let active = f.active(&f.a, &claim).await?;
    let write =
        f.a.prepare_context_update(
            STORAGE,
            &active.version()?,
            "commit-update",
            fill_pending_state,
        )
        .await?;
    let committed = f.a.commit_context(&write).await?;
    let read = f.b.read_context(STORAGE).await?.context("sibling read")?;
    assert_eq!(
        serde_json::to_value(&read.document.state)?,
        serde_json::to_value(&committed.document.state)?
    );
    lease_a.release().await?;
    let lease_b = f.lease("owner-b").await?;
    let successor = f.claim(&f.b, &lease_b, "claim-b").await?;
    assert_eq!(
        serde_json::to_value(&successor.document.state)?,
        serde_json::to_value(&committed.document.state)?
    );
    assert_eq!(
        f.a.get_task(STORAGE, TASK)
            .await?
            .context("authority snapshot")?
            .stream_seq,
        3
    );
    lease_b.release().await?;
    Ok(())
}

fn fill_pending_state(state: &mut ContextState) {
    let active = state.active.as_mut().unwrap();
    active.admission.phase = AdmissionPhase::Admitted;
    active.admission.prompt_sequence = Some(11);
    active.snapshot.user_msg_id = active.admission.prompt_id.clone();
    active.snapshot.user_msg_seq = 11;
    active.snapshot.execution_id = active.admission.invocation_id.clone();
    active.cancel = Some(CancelIntent {
        requested_at: Some(chrono::Utc::now()),
        operation_id: "cancel-operation".into(),
        task_id: TASK.into(),
        invocation_id: active.admission.invocation_id.clone(),
    });
    active.snapshot.stream_seq = 3;
    active.publication.stream_seq = 3;
    active.publication.subject_sequence = 20;
    active.publication.pending = Some(PendingEvent {
        committed_at: Some(chrono::Utc::now()),
        commit_id: "outbox-commit".into(),
        task_sequence: 3,
        expected_subject_sequence: 20,
        response: a2a_lf::StreamResponse::Task(active.snapshot.task.clone()),
    });
}

async fn assert_worker_record(f: &Fixture, worker: &NatsSessionLease) -> Result<()> {
    let kv = f.js.get_key_value("harnx_leases").await?;
    let entry = harnx_nats_common::leader_reads::entry(&kv, worker.key())
        .await?
        .context("worker lease")?;
    let record: harnx_runtime::nats_lease::LeaseRecord = serde_json::from_slice(&entry.value)?;
    assert_eq!(record.execution_id.as_deref(), Some("execution"));
    assert!(harnx_runtime::nats_lease::session_has_active_lease(&f.js, STORAGE).await?);
    Ok(())
}

#[tokio::test]
async fn claims_reject_worker_leases_and_other_lease_buckets() -> Result<()> {
    let f = Fixture::start().await?;
    let worker = NatsSessionLease::acquire(f.params("worker"))
        .await?
        .context("worker")?;
    let identity = || ContextIdentity {
        storage_key: STORAGE,
        local_id: LOCAL,
    };
    assert!(f
        .a
        .prepare_context_claim(identity(), &worker, "worker-claim")
        .await
        .is_err());
    worker.release().await?;
    let mut params = f.params("other-bucket");
    params.config.bucket = "other_coordinator".into();
    let other = NatsSessionLease::acquire_scoped(params, "a2a")
        .await?
        .context("other bucket")?;
    assert!(f
        .a
        .prepare_context_claim(identity(), &other, "other-bucket-claim")
        .await
        .is_err());
    assert!(f.b.read_context(STORAGE).await?.is_none());
    other.release().await?;
    Ok(())
}
