use super::*;

async fn takeover(boundary: Boundary) -> Result<()> {
    let t = TwoBackends::start().await?;
    let hooks = t.a.backend.store.context_fault_hooks();
    let mut pause = hooks.pause(boundary, 2);
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    pause.reached().await;
    let saved = reservation(&t).await?;
    let committed =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("committed outbox")?;
    let active = committed
        .document
        .state
        .active
        .as_ref()
        .context("pending active")?;
    assert_eq!(active.publication.stream_seq, 2);
    let pending = active
        .publication
        .pending
        .clone()
        .context("one event pending")?;
    let mut stream = t.h.jetstream.get_stream("HARNX_A2A_TASK_EVENTS").await?;
    let before = stream.info().await?.state.messages;
    assert_eq!(
        before,
        if matches!(boundary, Boundary::OutboxCommitted) {
            1
        } else {
            2
        }
    );
    let mut config = stream.info().await?.config.clone();
    config.duplicate_window = std::time::Duration::from_millis(100);
    t.h.jetstream.update_stream(config).await?;
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    let task =
        t.b.rpc("alice", "GetTask", json!({"id":saved.allocation.task_id}))
            .await?;
    assert_eq!(task["result"]["status"]["state"], "TASK_STATE_FAILED");
    // Expire the broker duplicate window, not a sleep used to choose a race winner.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    if matches!(boundary, Boundary::OutboxCommitted) {
        // Duplicate IDs are stream-wide. Acceptance on another test-only subject
        // proves this exact event ID is no longer protected by broker dedup.
        let probe =
            t.h.jetstream
                .send_publish(
                    "a2a.tasks.expiry.probe",
                    async_nats::jetstream::message::PublishMessage::build()
                        .message_id(pending.commit_id.clone())
                        .expected_last_subject_sequence(0)
                        .payload(serde_json::to_vec(&pending)?.into()),
                )
                .await?
                .await?;
        assert!(
            !probe.duplicate,
            "original event dedupe window must have expired"
        );
    }
    let sequence = stream.info().await?.state.last_sequence;
    let count = stream.info().await?.state.messages;
    drop(pause);
    t.a.backend.runner.shutdown().await;
    assert_eq!(stream.info().await?.state.messages, count);
    assert_eq!(stream.info().await?.state.last_sequence, sequence);
    let (_, uuid) = harnx_a2a_server::store::parse_task_id(&saved.allocation.task_id)?;
    let subject = format!("a2a.tasks.{}.{uuid}", saved.allocation.storage_key);
    let raw = stream
        .raw_message_builder()
        .sequence(pending.expected_subject_sequence + 1)
        .next_by_subject(&subject)
        .send()
        .await?;
    let published: harnx_a2a_server::store::context::PendingEvent =
        serde_json::from_slice(&raw.payload)?;
    assert_eq!(published.commit_id, pending.commit_id);
    assert_eq!(published.task_sequence, pending.task_sequence);
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_outbox_takeover_and_delayed_old_publish_after_dedupe_window() -> Result<()> {
    takeover(Boundary::OutboxCommitted).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn published_before_ack_cas_takeover_resolves_same_event_once() -> Result<()> {
    takeover(Boundary::EventPublished).await
}

async fn gap(missing_terminal: bool) -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut publish =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 2);
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    publish.reached().await;
    let http = Arc::new(RemoteHttp::start(&t.b).await?);
    let mut handoff =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::SnapshotCaptured, 1);
    let response = tokio::spawn({
        let http = http.clone();
        async move {
            http.client.post(&http.url).header("X-User-ID", "alice")
            .json(&json!({"jsonrpc":"2.0","id":"stream-request","method":"SendStreamingMessage","params":first_message()})).send().await
        }
    });
    handoff.reached().await;
    drop(publish);
    wait_cursor(&t, 2, false).await?;
    t.h.llm.release.notify_one();
    wait_cursor(&t, 3, true).await?;
    let mut stream = t.h.jetstream.get_stream("HARNX_A2A_TASK_EVENTS").await?;
    let last = stream.info().await?.state.last_sequence;
    stream
        .delete_message(if missing_terminal { last } else { 2 })
        .await?;
    drop(handoff);
    let body = response.await??.text().await?;
    let envelopes: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|line| serde_json::from_str(line.trim()))
        .collect::<std::result::Result<_, _>>()?;
    assert!(envelopes[0]["result"]["task"].is_object());
    assert!(
        envelopes
            .iter()
            .any(|event| event["error"]["code"] == -32603),
        "gap must explicitly interrupt: {body}"
    );
    assert!(!envelopes
        .iter()
        .any(|event| event["result"]["statusUpdate"]["status"]["state"] == "TASK_STATE_COMPLETED"));
    assert_eq!(
        t.h.task(&reservation(&t).await?.allocation.task_id)
            .await?
            .task
            .status
            .state,
        TaskState::Completed
    );
    t.a.backend.runner.shutdown().await;
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removed_intermediate_during_handoff_emits_reconnect_error() -> Result<()> {
    gap(false).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removed_terminal_during_handoff_does_not_hang_or_silently_finish() -> Result<()> {
    gap(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_artifact_ack_loss_has_one_logical_delta() -> Result<()> {
    let t = TwoBackends::start().await?;
    let hooks = t.a.backend.store.context_fault_hooks();
    let mut pending = hooks.pause(Boundary::OutboxCommitted, 2);
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    pending.reached().await;
    hooks.lose_next_event_ack();
    let saved = reservation(&t).await?;
    let sub =
        t.b.backend
            .runner
            .subscribe(&t.h.export, &alice().into(), &saved.allocation.task_id)
            .await?;
    let text: String = sub
        .snapshot
        .task
        .artifacts
        .as_ref()
        .context("committed snapshot")?[0]
        .parts
        .iter()
        .filter_map(|part| match &part.content {
            PartContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello ");
    drop(pending);
    wait_cursor(&t, 2, false).await?;
    t.h.llm.release.notify_one();
    assert_eq!(direct_finish(sub.events, text).await?, "Hello world");
    t.a.backend.runner.shutdown().await;
    let mut stream = t.h.jetstream.get_stream("HARNX_A2A_TASK_EVENTS").await?;
    assert_eq!(
        stream.info().await?.state.messages,
        t.h.task(&saved.allocation.task_id).await?.stream_seq
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_recovery_streams_full_replacement_before_terminal() -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut publish =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 2);
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    publish.reached().await;
    let saved = reservation(&t).await?;
    let sub =
        t.b.backend
            .runner
            .subscribe(&t.h.export, &alice().into(), &saved.allocation.task_id)
            .await?;
    assert!(sub.snapshot.task.artifacts.is_none());
    let session =
        t.b.backend
            .runner
            .session(SessionRequest {
                export: &t.h.export,
                owner: &alice().into(),
                local_id: Some(&saved.allocation.local_id),
                global_config: &t.h.config,
                activation_route: SessionActivationRoute::ClusterShared,
                abort: harnx_core::abort::create_abort_signal(),
            })
            .await?;
    let ticket = harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket::from_parts(
        saved.allocation.storage_key.clone(),
        saved.allocation.invocation_id.clone(),
        saved.allocation.prompt_id.clone(),
        saved.allocation.closure_id.clone(),
        0,
    )?;
    t.h.llm.release.notify_one();
    tokio::time::timeout(DEADLINE, async {
        while session.fixed_prompt_completion(&ticket).await?.is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    let final_task =
        t.b.rpc("alice", "GetTask", json!({"id":saved.allocation.task_id}))
            .await?;
    assert_eq!(
        final_task["result"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(
        direct_finish(sub.events, String::new()).await?,
        "Hello world"
    );
    drop(publish);
    t.a.backend.runner.shutdown().await;
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_checkpoint_then_takeover_keeps_durable_cursor_and_old_delivery_safe() -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut cleared =
        t.a.backend
            .store
            .context_fault_hooks()
            .pause(Boundary::OutboxCleared, 2);
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    cleared.reached().await;
    let saved = reservation(&t).await?;
    let context =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("checkpointed event")?;
    let active = context
        .document
        .state
        .active
        .as_ref()
        .context("checkpointed active")?;
    assert!(active.publication.pending.is_none());
    assert_eq!(active.snapshot.stream_seq, 2);
    let sub =
        t.b.backend
            .runner
            .subscribe(&t.h.export, &alice().into(), &saved.allocation.task_id)
            .await?;
    remove_owner_lease(&t, &saved.allocation.storage_key).await?;
    let failed =
        t.b.rpc("alice", "GetTask", json!({"id":saved.allocation.task_id}))
            .await?;
    assert_eq!(failed["result"]["status"]["state"], "TASK_STATE_FAILED");
    assert_eq!(direct_finish(sub.events, "Hello ".into()).await?, "Hello ");
    drop(cleared);
    t.a.backend.runner.shutdown().await;
    let mut stream = t.h.jetstream.get_stream("HARNX_A2A_TASK_EVENTS").await?;
    assert_eq!(stream.info().await?.state.messages, 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_latest_predecessor_covered_by_snapshot_interrupts_instead_of_waiting_forever(
) -> Result<()> {
    let t = TwoBackends::start().await?;
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    wait_cursor(&t, 2, false).await?;
    let saved = reservation(&t).await?;
    let http = Arc::new(RemoteHttp::start(&t.b).await?);
    let mut snapshot =
        t.b.backend
            .runner
            .fault_hooks()
            .pause(Boundary::SnapshotCaptured, 1);
    let response = tokio::spawn({
        let http = http.clone();
        let id = saved.allocation.task_id.clone();
        async move {
            http.client.post(&http.url).header("X-User-ID", "alice")
            .json(&json!({"jsonrpc":"2.0","id":"stream-request","method":"SubscribeToTask","params":{"id":id}})).send().await
        }
    });
    snapshot.reached().await;
    let stream = t.h.jetstream.get_stream("HARNX_A2A_TASK_EVENTS").await?;
    stream.delete_message(2).await?;
    drop(snapshot);
    let body = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        Ok::<_, anyhow::Error>(response.await??.text().await?)
    })
    .await??;
    assert!(
        body.contains("\"error\""),
        "covered predecessor loss must explicitly interrupt: {body}"
    );
    t.a.backend.runner.shutdown().await;
    Ok(())
}
