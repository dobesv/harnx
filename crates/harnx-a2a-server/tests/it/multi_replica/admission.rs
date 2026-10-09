use super::*;
use futures::StreamExt;
use harnx_a2a_server::store::{DedupeEntry, DedupeKey, FirstMessageReservation};

fn identity(t: &TwoBackends) -> DedupeKey {
    DedupeKey {
        cluster: t
            .h
            .export
            .cluster
            .clone()
            .unwrap_or_else(|| "__local__".into()),
        export: t.h.export.public_name.clone(),
        owner: Some("alice".into()),
        message_id: "same-first-message".into(),
    }
}
pub(super) async fn reservation(t: &TwoBackends) -> Result<FirstMessageReservation> {
    let message: a2a_lf::Message = serde_json::from_value(first_message()["message"].clone())?;
    t.b.backend
        .store
        .first_message_reservation(
            &identity(t),
            &harnx_a2a_server::store::message_fingerprint(&message.parts),
        )
        .await?
        .context("shared reservation")
}
pub(super) async fn remove_owner_lease(t: &TwoBackends, storage: &str) -> Result<()> {
    let kv = t.h.jetstream.get_key_value("harnx_leases").await?;
    let key = format!("sessions/{storage}/a2a/lock");
    let mut watch = kv.watch(&key).await?;
    let entry = harnx_nats_common::leader_reads::entry(&kv, &key)
        .await?
        .context("candidate lease")?;
    kv.delete_expect_revision(&key, Some(entry.revision))
        .await?;
    tokio::time::timeout(DEADLINE, async {
        while let Some(entry) = watch.next().await {
            if entry?.operation != async_nats::jetstream::kv::Operation::Put {
                return Ok::<_, anyhow::Error>(());
            }
        }
        anyhow::bail!("lease watch closed")
    })
    .await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialization_reservation_has_lease_before_metadata_and_sibling_does_not_orphan_it(
) -> Result<()> {
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
    assert_no_metadata(&t, &saved.allocation.storage_key).await?;
    assert!(t
        .b
        .backend
        .store
        .read_context(&saved.allocation.storage_key)
        .await?
        .is_none());
    let lease = t.h.jetstream.get_key_value("harnx_leases").await?;
    assert!(harnx_nats_common::leader_reads::get(
        &lease,
        &format!("sessions/{}/a2a/lock", saved.allocation.storage_key)
    )
    .await?
    .is_some());
    // Make a contradictory payload probe while winner is still initializing.
    let mut different = first_message();
    different["message"]["parts"][0]["text"] = json!("different");
    assert_eq!(
        t.b.rpc("alice", "SendMessage", different).await?["error"]["code"],
        -32602
    );
    assert_no_metadata(&t, &saved.allocation.storage_key).await?;
    let mut waiting =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::InitializationWait, 1);
    let sibling = tokio::spawn({
        let app = t.b.app.clone();
        async move { rpc(app, "alice", "SendMessage", first_message()).await }
    });
    waiting.reached().await;
    assert_no_metadata(&t, &saved.allocation.storage_key).await?;
    drop((pause, waiting));
    let (a, b) = (send.await??, sibling.await??);
    assert_eq!(result_task(&a)?["id"], saved.allocation.task_id);
    assert_eq!(result_task(&b)?["id"], saved.allocation.task_id);
    assert_eq!(t.h.metadata.list().await?.len(), 1);
    t.a.backend.runner.shutdown().await;
    t.b.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_dedupe_ignores_poisoned_lru_and_checks_identity_before_busy_and_terminal_target(
) -> Result<()> {
    let t = TwoBackends::start().await?;
    let response = t.a.rpc("alice", "SendMessage", first_message()).await?;
    let task = result_task(&response)?.clone();
    t.b.backend.store.record_dedupe_lru(
        identity(&t),
        DedupeEntry {
            task_id: "bad.task".into(),
            fingerprint: "wrong".into(),
        },
    );
    let retry = t.b.rpc("alice", "SendMessage", first_message()).await?;
    assert_eq!(result_task(&retry)?["id"], task["id"]);
    let mut retry = first_message();
    retry["message"]["contextId"] = task["contextId"].clone();
    assert_eq!(
        result_task(&t.b.rpc("alice", "SendMessage", retry.clone()).await?)?["id"],
        task["id"]
    );
    let mut changed = retry.clone();
    changed["message"]["parts"][0]["text"] = json!("changed");
    assert_eq!(
        t.b.rpc("alice", "SendMessage", changed).await?["error"]["code"],
        -32602
    );
    let mut distinct = retry.clone();
    distinct["message"]["messageId"] = json!("distinct");
    assert_eq!(
        t.b.rpc("alice", "SendMessage", distinct).await?["error"]["code"],
        -32000
    );
    t.h.llm.release.notify_one();
    let mut blocking = first_message();
    blocking["configuration"]["returnImmediately"] = json!(false);
    let terminal = t.b.rpc("alice", "SendMessage", blocking).await?;
    assert_eq!(
        result_task(&terminal)?["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    retry["message"]["taskId"] = task["id"].clone();
    assert_eq!(
        result_task(&t.b.rpc("alice", "SendMessage", retry).await?)?["id"],
        task["id"]
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    t.a.backend.runner.shutdown().await;
    t.b.backend.runner.shutdown().await;
    Ok(())
}

async fn crashed_boundary(boundary: Boundary, prompt_count: usize) -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut pause = t.a.backend.runner.fault_hooks().pause(boundary, 1);
    let send = tokio::spawn({
        let app = t.a.app.clone();
        async move { rpc(app, "alice", "SendMessage", first_message()).await }
    });
    pause.reached().await;
    let saved = reservation(&t).await?;
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    let response = t.b.rpc("alice", "SendMessage", first_message()).await?;
    assert_eq!(result_task(&response)?["id"], saved.allocation.task_id);
    assert_eq!(
        result_task(&response)?["status"]["state"],
        "TASK_STATE_FAILED"
    );
    drop(pause);
    let _old = tokio::time::timeout(DEADLINE, send).await???;
    let log = NatsSessionLog::new(t.h.jetstream.clone(), &saved.allocation.storage_key);
    assert_eq!(prompts(&log).await?.len(), prompt_count);
    assert!(
        t.h.llm.requests.lock().is_empty(),
        "recovery must not execute/replay a missing or abandoned prompt"
    );
    let retried = t.a.rpc("alice", "SendMessage", first_message()).await?;
    assert_eq!(result_task(&retried)?["id"], saved.allocation.task_id);
    assert_eq!(
        result_task(&retried)?["status"]["state"],
        "TASK_STATE_FAILED"
    );
    let context =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("retained terminal authority")?;
    let active = context
        .document
        .state
        .active
        .as_ref()
        .context("same retained task")?;
    assert_eq!(
        active.admission.invocation_id,
        saved.allocation.invocation_id
    );
    assert_eq!(active.admission.prompt_id, saved.allocation.prompt_id);
    assert_eq!(active.admission.fixed_predecessor, 0);
    assert!(
        active.stop_confirmed
            && active.projections.archive
            && active.projections.message_mapping
            && active.projections.final_event
    );
    assert!(active.publication.pending.is_none());
    t.a.backend.runner.shutdown().await;
    t.b.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_after_first_reservation_closes_same_ids_without_prompt() -> Result<()> {
    crashed_boundary(Boundary::FirstReservation, 0).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_after_session_initialization_closes_same_ids_without_prompt() -> Result<()> {
    crashed_boundary(Boundary::SessionInitialized, 0).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_after_context_reservation_fences_delayed_claimant_without_prompt() -> Result<()> {
    crashed_boundary(Boundary::Claim, 0).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_before_runtime_append_closes_missing_prompt_without_rebase() -> Result<()> {
    crashed_boundary(Boundary::BeforeAdmission, 0).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_after_runtime_append_resolves_and_stops_original_prompt_without_replay() -> Result<()>
{
    crashed_boundary(Boundary::AfterAdmission, 1).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_after_message_projection_keeps_same_identity_without_replay() -> Result<()> {
    crashed_boundary(Boundary::MessageMapping, 1).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_before_activation_never_replays_admitted_prompt() -> Result<()> {
    crashed_boundary(Boundary::Activation, 1).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn competing_followups_share_one_identity_or_preserve_busy_for_distinct_messages(
) -> Result<()> {
    let t = TwoBackends::start().await?;
    let first = t.a.rpc("alice", "SendMessage", first_message()).await?;
    let context = result_task(&first)?["contextId"].clone();
    t.h.llm.release.notify_one();
    let mut finish = first_message();
    finish["configuration"]["returnImmediately"] = json!(false);
    assert_eq!(
        result_task(&t.a.rpc("alice", "SendMessage", finish).await?)?["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    for identical in [true, false] {
        let mut winner = race_followup(&t, &context, identical).await?;
        t.h.llm.release.notify_one();
        winner["configuration"]["returnImmediately"] = json!(false);
        assert_eq!(
            result_task(&t.a.rpc("alice", "SendMessage", winner).await?)?["status"]["state"],
            "TASK_STATE_COMPLETED"
        );
    }
    let session = t.resume(&t.a, context.as_str().context("context")?).await?;
    assert_eq!(
        prompts(&NatsSessionLog::new(
            t.h.jetstream.clone(),
            session.storage_key()
        ))
        .await?
        .len(),
        3
    );
    assert_eq!(t.h.llm.requests.lock().len(), 3);
    t.a.backend.runner.shutdown().await;
    t.b.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_first_reservation_and_context_acks_follow_durable_winner_once() -> Result<()> {
    let t = TwoBackends::start().await?;
    let hooks = t.a.backend.store.context_fault_hooks();
    hooks.lose_next_first_ack();
    hooks.lose_next_context_ack();
    hooks.lose_next_terminal_ack();
    let response = t.a.rpc("alice", "SendMessage", first_message()).await?;
    let saved = reservation(&t).await?;
    assert_eq!(result_task(&response)?["id"], saved.allocation.task_id);
    assert_eq!(
        result_task(&t.b.rpc("alice", "SendMessage", first_message()).await?)?["id"],
        saved.allocation.task_id
    );
    assert_eq!(t.h.metadata.list().await?.len(), 1);
    let log = NatsSessionLog::new(t.h.jetstream.clone(), &saved.allocation.storage_key);
    assert_eq!(prompts(&log).await?.len(), 1);
    t.h.llm.release.notify_one();
    let mut blocking = first_message();
    blocking["configuration"]["returnImmediately"] = json!(false);
    assert_eq!(
        result_task(&t.b.rpc("alice", "SendMessage", blocking).await?)?["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    t.a.backend.runner.shutdown().await;
    t.b.backend.runner.shutdown().await;
    let mut stream = t.h.jetstream.get_stream("HARNX_A2A_TASK_EVENTS").await?;
    let count = stream.info().await?.state.messages;
    assert!(count >= 3);
    let (_, uuid) = harnx_a2a_server::store::parse_task_id(&saved.allocation.task_id)?;
    let raw = stream
        .get_last_raw_message_by_subject(&format!(
            "a2a.tasks.{}.{uuid}",
            saved.allocation.storage_key
        ))
        .await?;
    let event: harnx_a2a_server::store::context::PendingEvent =
        serde_json::from_slice(&raw.payload)?;
    assert_eq!(event.task_sequence, count);
    assert_eq!(event.expected_subject_sequence, raw.sequence - 1);
    let active =
        t.h.store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("terminal authority")?
            .document
            .state
            .active
            .context("terminal task")?;
    assert_eq!(active.snapshot.stream_seq, count);
    assert!(
        active.stop_confirmed
            && active.projections.final_event
            && active.publication.pending.is_none()
    );
    Ok(())
}

async fn assert_no_metadata(t: &TwoBackends, storage: &str) -> Result<()> {
    assert!(t.h.metadata.get(storage).await?.is_none());
    Ok(())
}

async fn race_followup(t: &TwoBackends, context: &Value, identical: bool) -> Result<Value> {
    let hooks_a = t.a.backend.runner.fault_hooks();
    let hooks_b = t.b.backend.runner.fault_hooks();
    let mut miss_a = hooks_a.pause(
        Boundary::DedupeMiss,
        hooks_a.count(Boundary::DedupeMiss) + 1,
    );
    let mut miss_b = hooks_b.pause(
        Boundary::DedupeMiss,
        hooks_b.count(Boundary::DedupeMiss) + 1,
    );
    let mut a = first_message();
    a["message"]["contextId"] = context.clone();
    a["message"]["messageId"] = json!(if identical {
        "same-followup"
    } else {
        "distinct-a"
    });
    let mut b = a.clone();
    if !identical {
        b["message"]["messageId"] = json!("distinct-b");
    }
    let send_a = tokio::spawn({
        let app = t.a.app.clone();
        let a = a.clone();
        async move { rpc(app, "alice", "SendMessage", a).await }
    });
    let send_b = tokio::spawn({
        let app = t.b.app.clone();
        let b = b.clone();
        async move { rpc(app, "alice", "SendMessage", b).await }
    });
    tokio::join!(miss_a.reached(), miss_b.reached());
    drop((miss_a, miss_b));
    let (response_a, response_b) = (send_a.await??, send_b.await??);
    let winner = if identical {
        assert_eq!(
            result_task(&response_a)?["id"],
            result_task(&response_b)?["id"]
        );
        a
    } else if response_a.get("error").is_some() {
        assert_eq!(response_a["error"]["code"], -32000);
        result_task(&response_b)?;
        b
    } else {
        assert_eq!(response_b["error"]["code"], -32000);
        result_task(&response_a)?;
        a
    };
    Ok(winner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_transport_backpressure_retains_authority_before_context_reuse() -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut publication =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 3);
    let first = t.a.rpc("alice", "SendMessage", first_message()).await?;
    let task = result_task(&first)?.clone();
    let saved = reservation(&t).await?;
    super::streaming::wait_cursor(&t, 2, false).await?;
    t.h.llm.release.notify_one();
    publication.reached().await;
    let mut event_stream = t.h.jetstream.get_stream("HARNX_A2A_TASK_EVENTS").await?;
    let before = event_stream.info().await?.state.messages;
    let mut config = event_stream.info().await?.config.clone();
    config.max_bytes = 1;
    t.h.jetstream.update_stream(config).await?;
    drop(publication);
    t.a.backend.runner.shutdown().await;
    let pending =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("pending terminal authority")?;
    let active = pending
        .document
        .state
        .active
        .as_ref()
        .context("retained task")?;
    assert!(!active.snapshot.task.status.state.is_terminal());
    assert!(active.publication.pending.is_some());
    assert!(!active.projections.final_event);
    assert_eq!(event_stream.info().await?.state.messages, before);
    let mut next = first_message();
    next["message"]["contextId"] = task["contextId"].clone();
    next["message"]["messageId"] = json!("after-backpressure");
    assert_eq!(
        t.b.rpc("alice", "SendMessage", next.clone()).await?["error"]["code"],
        -32603
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    let mut config = event_stream.info().await?.config.clone();
    config.max_bytes = 64 * 1024 * 1024;
    t.h.jetstream.update_stream(config).await?;
    // Shutdown A, not the live replica B. B repairs the committed terminal event
    // before reserving the next message, without re-running the original prompt.
    let admitted = t.b.rpc("alice", "SendMessage", next.clone()).await?;
    assert_ne!(result_task(&admitted)?["id"], saved.allocation.task_id);
    assert!(event_stream.info().await?.state.messages > before);
    t.h.llm.release.notify_one();
    next["configuration"]["returnImmediately"] = json!(false);
    assert_eq!(
        result_task(&t.b.rpc("alice", "SendMessage", next).await?)?["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(t.h.llm.requests.lock().len(), 2);
    t.b.backend.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn takeover_stops_running_invocation_and_fences_old_publisher_without_second_execution(
) -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut output =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 2);
    let first = t.a.rpc("alice", "SendMessage", first_message()).await?;
    output.reached().await;
    let task = result_task(&first)?;
    let saved = reservation(&t).await?;
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    let failed =
        t.b.rpc("alice", "GetTask", json!({"id": saved.allocation.task_id}))
            .await?;
    assert_eq!(failed["result"]["status"]["state"], "TASK_STATE_FAILED");
    let record = t.h.task(&saved.allocation.task_id).await?;
    let stale_view =
        t.a.backend
            .runner
            .live_record(&t.h.export, record.clone())
            .await;
    assert_eq!(stale_view.task.status.state, a2a_lf::TaskState::Failed);
    drop(output);
    t.a.backend.runner.shutdown().await;
    let after = t.h.task(&saved.allocation.task_id).await?;
    assert_eq!(
        serde_json::to_value(&after.task)?,
        serde_json::to_value(&record.task)?
    );
    assert_eq!(after.revision, record.revision);
    let retry = t.b.rpc("alice", "SendMessage", first_message()).await?;
    assert_eq!(result_task(&retry)?["id"], task["id"]);
    assert_eq!(result_task(&retry)?["status"]["state"], "TASK_STATE_FAILED");
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
    t.b.backend.runner.shutdown().await;
    Ok(())
}
