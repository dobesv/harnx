use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_cancel_persists_exact_intent_and_stale_t1_cancel_cannot_stop_t2() -> Result<()> {
    let t = TwoBackends::start().await?;
    let first = t.a.rpc("alice", "SendMessage", first_message()).await?;
    let task = result_task(&first)?;
    let id = task["id"].as_str().context("task id")?;
    requested(&t, 1).await?;
    assert_eq!(
        t.b.rpc("bob", "CancelTask", json!({"id": id})).await?["error"]["code"],
        -32001
    );
    let canceled = t.b.rpc("alice", "CancelTask", json!({"id": id})).await?;
    assert!(canceled.get("error").is_none(), "{canceled}");
    assert_eq!(canceled["result"]["status"]["state"], "TASK_STATE_CANCELED");
    let saved = reservation(&t).await?;
    let context =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("authority")?;
    let active = context.document.state.active.as_ref().context("active")?;
    let intent = active.cancel.as_ref().context("persisted cancel")?;
    assert_eq!(intent.task_id, id);
    assert_eq!(intent.invocation_id, saved.allocation.invocation_id);
    assert!(active.stop_confirmed && active.projections.final_event);
    let old_session = session_for(&t.a, &t.h, &active.snapshot.task.context_id)
        .await?
        .with_admission_id(intent.invocation_id.clone());
    let mut next = first_message();
    next["message"]["contextId"] = task["contextId"].clone();
    next["message"]["messageId"] = json!("t2-after-cancel");
    let second = t.b.rpc("alice", "SendMessage", next).await?;
    let second_id = result_task(&second)?["id"].as_str().context("T2")?;
    requested(&t, 2).await?;
    assert!(t
        .a
        .backend
        .store
        .request_task_cancel(&saved.allocation.storage_key, id)
        .await
        .is_err());
    old_session
        .interrupt_admitted_invocation("delayed T1 cancel")
        .await?;
    let current =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("T2 authority")?;
    assert_eq!(
        current
            .document
            .state
            .active
            .as_ref()
            .context("T2 active")?
            .snapshot
            .task
            .id,
        second_id
    );
    assert!(current
        .document
        .state
        .active
        .as_ref()
        .context("T2 active")?
        .cancel
        .is_none());
    t.h.llm.release.notify_one();
    assert_eq!(
        terminal(&t, second_id).await?.task.status.state,
        TaskState::Completed
    );
    assert_eq!(t.h.llm.requests.lock().len(), 2);
    t.a.backend.runner.shutdown().await;
    t.b.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_cancel_ack_superseded_by_completion_resolves_identity_without_replay() -> Result<()> {
    let t = TwoBackends::start().await?;
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    requested(&t, 1).await?;
    let saved = reservation(&t).await?;
    let hooks = t.b.backend.store.context_fault_hooks();
    let mut ack = hooks.pause(Boundary::ContextCas, 1);
    hooks.lose_next_context_ack();
    let cancel = tokio::spawn({
        let store = t.b.backend.store.clone();
        let storage = saved.allocation.storage_key.clone();
        let id = saved.allocation.task_id.clone();
        async move { store.request_task_cancel(&storage, &id).await }
    });
    ack.reached().await;
    let terminal = terminal(&t, &saved.allocation.task_id).await?;
    assert_eq!(terminal.task.status.state, TaskState::Canceled);
    t.a.backend.runner.shutdown().await;
    let before =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("terminal before lost ack")?;
    drop(ack);
    cancel.await??;
    let after =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("terminal after lost ack")?;
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        after
            .document
            .state
            .active
            .as_ref()
            .context("active")?
            .cancel,
        before
            .document
            .state
            .active
            .as_ref()
            .context("active")?
            .cancel
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    t.a.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_cancel_of_unadmitted_ticket_closes_without_worker_execution() -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut admission =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::BeforeAdmission, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        async move { rpc(app, "alice", "SendMessage", first_message()).await }
    });
    admission.reached().await;
    let saved = reservation(&t).await?;
    t.b.backend
        .store
        .request_task_cancel(&saved.allocation.storage_key, &saved.allocation.task_id)
        .await?;
    let pending =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("unadmitted cancel")?;
    assert!(
        !pending
            .document
            .state
            .active
            .as_ref()
            .context("active")?
            .stop_confirmed
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
    assert!(t.h.llm.requests.lock().is_empty());
    drop(admission);
    let _ = send.await?;
    t.a.backend.runner.shutdown().await;
    assert!(prompts(&NatsSessionLog::new(
        t.h.jetstream.clone(),
        &saved.allocation.storage_key
    ))
    .await?
    .is_empty());
    let context =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("closed authority")?;
    let active = context
        .document
        .state
        .active
        .as_ref()
        .context("closed active")?;
    assert!(
        active.stop_confirmed
            && active.projections.archive
            && active.projections.message_mapping
            && active.projections.final_event
            && active.publication.pending.is_none()
    );
    t.b.backend.abort.set_ctrlc();
    Ok(())
}
