use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_capacity_pending_event_recovers_after_subject_purge_without_replay() -> Result<()> {
    let t = TwoBackends::start().await?;
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    let first = reservation(&t).await?;
    let js = &t.h.jetstream;
    let filler = "a2a.tasks.capacity-filler.00000000-0000-0000-0000-000000000001";
    js.publish(filler, vec![b'x'; 8192].into()).await?.await?;
    let mut stream = js.get_stream(a2a_events::STREAM).await?;
    let info = stream.info().await?.clone();
    let mut config = info.config;
    config.max_bytes = info.state.bytes as i64;
    js.update_stream(config).await?;
    t.h.llm.release.notify_one();
    let pending = tokio::time::timeout(DEADLINE, async {
        loop {
            let context =
                t.h.store
                    .read_context(&first.allocation.storage_key)
                    .await?
                    .context("context")?;
            let active = context.document.state.active.as_ref().context("active")?;
            if active.publication.pending.is_some() {
                return Ok::<_, anyhow::Error>(context);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;
    let checkpoint = pending
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .publication
        .subject_sequence;
    assert!(checkpoint > 0);
    let delete_config = t.h.config.read().clone();
    assert!(nats_admin::delete_remote_session(
        &delete_config,
        "runner",
        "runner",
        &first.allocation.local_id
    )
    .await
    .is_err());
    remove_owner_lease(&t, &first.allocation.storage_key).await?;
    let activity = harnx_runtime::nats_session_metadata::SessionActivity {
        first_activation_at: None,
        last_activity_at: chrono::DateTime::from_timestamp(1, 0).unwrap(),
    };
    t.h.metadata
        .kv_store()
        .put(
            harnx_runtime::nats_session_metadata::activity_key(&first.allocation.storage_key),
            serde_json::to_vec(&activity)?.into(),
        )
        .await?;
    let stats =
        harnx_runtime::remote_session_cleanup::run_remote_cleanup(&delete_config, 1, "runner")
            .await;
    assert_eq!(stats.errors, 0);
    assert!(
        stats.skipped_active > 0,
        "unresolved A2A authority must survive TTL retention even without its owner lease"
    );
    assert!(t
        .h
        .metadata
        .get(&first.allocation.storage_key)
        .await?
        .is_some());
    stream.purge().filter(filler).await?;
    let retry = t.b.rpc("alice", "SendMessage", first_message()).await?;
    assert_eq!(result_task(&retry)?["id"], first.allocation.task_id);
    let _ = settled(&t).await?;
    let context =
        t.h.store
            .read_context(&first.allocation.storage_key)
            .await?
            .unwrap();
    let active = context.document.state.active.as_ref().unwrap();
    assert!(active.publication.subject_sequence > checkpoint);
    assert!(active.publication.pending.is_none());
    assert!(
        stream.get_raw_message(checkpoint).await.is_ok(),
        "predecessor retained"
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_cleanup_load_preserves_predecessor_and_serialized_boundary_output() -> Result<()>
{
    let h = Harness::start(Script::Boundary).await?;
    let mut terminal_pause = h
        .store
        .context_fault_hooks()
        .pause(Boundary::TerminalOutboxCommitted, 1);
    let session = h.session(None, &alice()).await?;
    let message: a2a_lf::Message = serde_json::from_value(
        json!({"messageId":"boundary", "role":"ROLE_USER", "parts":[{"text":"Hello"}]}),
    )?;
    let started = h.send(&session, message).await?;
    let mut reader = h
        .runner
        .subscribe(&h.export, &alice().into(), &started.snapshot.task.id)
        .await?;
    let started_at = std::time::Instant::now();
    let mut chunks = reader
        .snapshot
        .task
        .artifacts
        .as_ref()
        .map_or(0, |artifacts| {
            part_text(&artifacts[0].parts[0]).len() / 3324
        });
    let mut max_authority = 0;
    if chunks > 0 {
        h.llm.release.notify_one();
    }
    while chunks < 69 {
        let event = tokio::time::timeout(DEADLINE, reader.events.recv())
            .await?
            .with_context(|| {
                let log = h.logs.text();
                format!(
                    "chunks={chunks} logs: {}",
                    &log[log.len().saturating_sub(7000)..]
                )
            })?;
        if matches!(event.response, StreamResponse::ArtifactUpdate(_)) {
            chunks += 1;
            let context = h.store.read_context(session.storage_key()).await?.unwrap();
            max_authority = max_authority.max(serde_json::to_vec(&context.document)?.len());
            h.llm.release.notify_one();
        }
    }
    tokio::time::timeout(DEADLINE, terminal_pause.reached()).await?;
    let pending = h.store.read_context(session.storage_key()).await?.unwrap();
    let pending_bytes = serde_json::to_vec(&pending.document)?.len();
    max_authority = max_authority.max(pending_bytes);
    assert!(pending_bytes <= 1_048_576 - 1024);
    let mut terminal_cursor = 0;
    let mut pending_stream = h.jetstream.get_stream(a2a_events::STREAM).await?;
    let before_cleanup = pending_stream.info().await?.state.messages;
    h.store
        .cleanup_terminal_events_for_test(&mut terminal_cursor, chrono::Utc::now().timestamp() + 31)
        .await?;
    assert_eq!(
        pending_stream.info().await?.state.messages,
        before_cleanup,
        "unconfirmed terminal outbox cannot authorize history purge"
    );
    drop(terminal_pause);
    loop {
        let event = tokio::time::timeout(DEADLINE, reader.events.recv())
            .await?
            .with_context(|| {
                let log = h.logs.text();
                format!(
                    "chunks={chunks} logs: {}",
                    &log[log.len().saturating_sub(7000)..]
                )
            })?;
        if matches!(event.response, StreamResponse::StatusUpdate(update) if update.status.state.is_terminal())
        {
            break;
        }
    }
    let task = h.task(&started.snapshot.task.id).await?;
    assert_eq!(task.task.status.state, TaskState::Completed);
    assert_eq!(
        part_text(&task.task.artifacts.as_ref().unwrap()[0].parts[0]).len(),
        229376
    );
    let context = h.store.read_context(session.storage_key()).await?.unwrap();
    max_authority = max_authority.max(serde_json::to_vec(&context.document)?.len());
    assert!(max_authority < 1_048_576);
    let active = context.document.state.active.as_ref().unwrap();
    assert_eq!(
        active.publication.confirmed_history.len(),
        a2a_events::CHECKPOINT_HISTORY
    );
    h.store
        .cleanup_context_events(session.storage_key())
        .await?;
    let mut stream = h.jetstream.get_stream(a2a_events::STREAM).await?;
    let state = stream.info().await?.state.clone();
    assert!(state.messages <= 66);
    assert_eq!(state.consumer_count, 0);
    assert!(stream
        .get_raw_message(active.publication.subject_sequence)
        .await
        .is_ok());
    println!("Task7 load: output_bytes=229376 authority_peak={max_authority} messages={} storage_bytes={} elapsed_ms={} consumers={}", state.messages, state.bytes, started_at.elapsed().as_millis(), state.consumer_count);
    h.runner.shutdown().await;
    Ok(())
}
