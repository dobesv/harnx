use super::*;

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_partition_preserves_cancel_intent_and_slot_until_background_recovery() -> Result<()>
{
    let t = TwoBackends::start().await?;
    let mut output =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 2);
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    output.reached().await;
    let saved = reservation(&t).await?;
    t.b.backend
        .store
        .request_task_cancel(&saved.allocation.storage_key, &saved.allocation.task_id)
        .await?;
    let before =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("before partition")?;
    assert!(before
        .document
        .state
        .active
        .as_ref()
        .context("active")?
        .cancel
        .is_some());
    t.h._broker.signal("-STOP")?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        t.b.backend
            .runner
            .cancel_task(&t.h.export, &alice().into(), &saved.allocation.task_id),
    )
    .await;
    assert!(
        !matches!(result, Ok(Ok(_))),
        "partition must not report cancel success"
    );
    t.h._broker.signal("-CONT")?;
    let after =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("after partition")?;
    let active = after
        .document
        .state
        .active
        .as_ref()
        .context("retained active")?;
    assert!(!active.stop_confirmed && !active.snapshot.task.status.state.is_terminal());
    assert_eq!(
        active.cancel,
        before
            .document
            .state
            .active
            .as_ref()
            .context("before active")?
            .cancel
    );
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    supervise(&t, &t.b);
    assert_eq!(
        terminal(&t, &saved.allocation.task_id)
            .await?
            .task
            .status
            .state,
        TaskState::Canceled
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    drop(output);
    t.a.backend.runner.shutdown().await;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_restart_reloads_registry_and_settles_without_incoming_http() -> Result<()> {
    let mut t = TwoBackends::start().await?;
    let mut output =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 2);
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    output.reached().await;
    let saved = reservation(&t).await?;
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    t.h._broker.restart().await?;
    let restarted_store = Arc::new(A2aStore::new(t.h.metadata.clone()));
    let restarted = Replica::new(&t.h, restarted_store.clone(), Runner::new(restarted_store))?;
    supervise(&t, &restarted);
    assert_eq!(
        terminal(&t, &saved.allocation.task_id)
            .await?
            .task
            .status
            .state,
        TaskState::Failed
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    assert_eq!(
        prompts(&NatsSessionLog::new(
            t.h.jetstream.clone(),
            &saved.allocation.storage_key
        ))
        .await?
        .len(),
        1
    );
    drop(output);
    t.a.backend.runner.shutdown().await;
    restarted.backend.abort.set_ctrlc();
    Ok(())
}
