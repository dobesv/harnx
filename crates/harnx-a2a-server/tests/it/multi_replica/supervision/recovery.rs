use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_restart_recovers_first_reservation_without_http_or_prompt_replay() -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut pause =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::FirstReservation, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        async move { rpc(app, "alice", "SendMessage", first_message()).await }
    });
    pause.reached().await;
    let saved = reservation(&t).await?;
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
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
    assert!(t.h.llm.requests.lock().is_empty());
    assert!(prompts(&NatsSessionLog::new(
        t.h.jetstream.clone(),
        &saved.allocation.storage_key
    ))
    .await?
    .is_empty());
    drop(pause);
    let _ = send.await?;
    restarted.backend.abort.set_ctrlc();
    t.a.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_takeover_ignores_renewing_worker_lease_and_fences_local_waiter() -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut output =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 2);
    let first = t.a.rpc("alice", "SendMessage", first_message()).await?;
    output.reached().await;
    let saved = reservation(&t).await?;
    let leases = t.h.jetstream.get_key_value("harnx_leases").await?;
    assert!(harnx_nats_common::leader_reads::get(
        &leases,
        &format!("sessions/{}/lock", saved.allocation.storage_key)
    )
    .await?
    .is_some());
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    supervise(&t, &t.b);
    let failed = terminal(&t, &saved.allocation.task_id).await?;
    assert_eq!(failed.task.status.state, TaskState::Failed);
    assert_eq!(
        t.a.backend
            .runner
            .live_record(&t.h.export, failed.clone())
            .await
            .task
            .status
            .state,
        TaskState::Failed
    );
    drop(output);
    t.a.backend.runner.shutdown().await;
    assert_eq!(
        t.h.task(&saved.allocation.task_id).await?.revision,
        failed.revision
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    assert_eq!(result_task(&first)?["id"], saved.allocation.task_id);
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_completion_wins_cancel_before_terminal_publication() -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut pause =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 2);
    let first = t.a.rpc("alice", "SendMessage", first_message()).await?;
    pause.reached().await;
    let saved = reservation(&t).await?;
    t.h.llm.release.notify_one();
    let session = session_for(&t.b, &t.h, &saved.allocation.local_id).await?;
    let ticket = harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket::from_parts(
        saved.allocation.storage_key.clone(),
        saved.allocation.invocation_id.clone(),
        saved.allocation.prompt_id.clone(),
        saved.allocation.closure_id.clone(),
        0,
    )?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            if session.fixed_prompt_completion(&ticket).await?.is_some() {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;
    t.b.backend
        .store
        .request_task_cancel(&saved.allocation.storage_key, &saved.allocation.task_id)
        .await?;
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    supervise(&t, &t.b);
    assert_eq!(
        terminal(&t, &saved.allocation.task_id)
            .await?
            .task
            .status
            .state,
        TaskState::Completed
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    assert_eq!(result_task(&first)?["id"], saved.allocation.task_id);
    drop(pause);
    t.a.backend.runner.shutdown().await;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_registry_pages_reach_abandoned_work_with_two_concurrent_sweepers() -> Result<()> {
    let t = TwoBackends::start().await?;
    for _ in 0..40 {
        let allocation = harnx_a2a_server::store::TaskAllocation::new(
            &t.h.export.agent,
            uuid::Uuid::new_v4().simple().to_string(),
        );
        t.b.backend
            .store
            .register_recovery(&t.h.export, &alice(), &allocation, None)
            .await?;
    }
    let mut pause =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::FirstReservation, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        async move { rpc(app, "alice", "SendMessage", first_message()).await }
    });
    pause.reached().await;
    let saved = reservation(&t).await?;
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    let store = Arc::new(A2aStore::new(t.h.metadata.clone()));
    let restarted = Replica::new(&t.h, store.clone(), Runner::new(store))?;
    supervise(&t, &t.b);
    supervise(&t, &restarted);
    assert_eq!(
        terminal(&t, &saved.allocation.task_id)
            .await?
            .task
            .status
            .state,
        TaskState::Failed
    );
    assert!(t.h.llm.requests.lock().is_empty());
    drop(pause);
    let _ = send.await?;
    t.a.backend.runner.shutdown().await;
    t.b.backend.abort.set_ctrlc();
    restarted.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn takeover_terminal_wakes_local_waiter_before_former_owner_resumes() -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut output =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 2);
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    output.reached().await;
    let saved = reservation(&t).await?;
    let handler = HarnxHandler::new(
        t.h.export.clone(),
        harnx_a2a_server::identity::Identity::default(),
        t.a.backend.clone(),
        InputLimits::default(),
    );
    let waiter = tokio::spawn({
        let id = saved.allocation.task_id.clone();
        async move { handler.wait_terminal(&alice().into(), &id).await }
    });
    tokio::time::timeout(DEADLINE, async {
        while !t.h.logs.text().contains("waiting on local task completion") {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    supervise(&t, &t.b);
    let terminal = terminal(&t, &saved.allocation.task_id).await?;
    assert_eq!(terminal.task.status.state, TaskState::Failed);
    let returned = tokio::time::timeout(std::time::Duration::from_secs(5), waiter).await???;
    assert_eq!(returned.task.status.state, TaskState::Failed);
    assert_eq!(returned.revision, terminal.revision);
    // A remains paused throughout the waiter's authoritative terminal return.
    drop(output);
    t.a.backend.runner.shutdown().await;
    t.b.backend.abort.set_ctrlc();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_recovers_claimed_context_before_task_allocation_without_replay() -> Result<()> {
    let t = TwoBackends::start().await?;
    let hooks = t.a.backend.store.context_fault_hooks();
    let mut claim = hooks.pause(Boundary::ContextCas, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        async move { rpc(app, "alice", "SendMessage", first_message()).await }
    });
    claim.reached().await;
    let saved = reservation(&t).await?;
    let empty =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("claimed empty context")?;
    assert!(empty.document.state.active.is_none());
    assert_eq!(empty.document.epoch, 1);
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    supervise(&t, &t.b);
    assert_eq!(
        terminal(&t, &saved.allocation.task_id)
            .await?
            .task
            .status
            .state,
        TaskState::Failed
    );
    assert!(t.h.llm.requests.lock().is_empty());
    drop(claim);
    let _ = send.await?;
    t.a.backend.runner.shutdown().await;
    assert!(prompts(&NatsSessionLog::new(
        t.h.jetstream.clone(),
        &saved.allocation.storage_key
    ))
    .await?
    .is_empty());
    let retry = t.b.rpc("alice", "SendMessage", first_message()).await?;
    assert_eq!(result_task(&retry)?["id"], saved.allocation.task_id);
    assert_eq!(result_task(&retry)?["status"]["state"], "TASK_STATE_FAILED");
    t.b.backend.abort.set_ctrlc();
    Ok(())
}
