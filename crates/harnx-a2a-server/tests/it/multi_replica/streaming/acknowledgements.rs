use super::*;

async fn superseded_ack(checkpoint: bool) -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut publication =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 2);
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    publication.reached().await;
    let saved = reservation(&t).await?;
    let hooks = t.a.backend.store.context_fault_hooks();
    let visit = hooks.count(Boundary::ContextCas) + if checkpoint { 2 } else { 1 };
    let mut ack = hooks.pause(Boundary::ContextCas, visit);
    let mut remote =
        t.b.backend
            .runner
            .subscribe(&t.h.export, &alice().into(), &saved.allocation.task_id)
            .await?;
    drop(publication);
    ack.reached().await;
    let before =
        t.b.backend
            .store
            .read_context(&saved.allocation.storage_key)
            .await?
            .context("applied before ack")?;
    let active = before
        .document
        .state
        .active
        .as_ref()
        .context("pending task")?;
    assert_eq!(active.snapshot.stream_seq, 2);
    assert_eq!(active.publication.pending.is_none(), checkpoint);
    if !checkpoint {
        let changed =
            t.b.backend
                .store
                .prepare_context_update(
                    &saved.allocation.storage_key,
                    &before.version()?,
                    &uuid::Uuid::new_v4().to_string(),
                    |state| {
                        state.active.as_mut().unwrap().snapshot.task.artifacts = None;
                    },
                )
                .await;
        assert!(changed.is_err(), "pending event must freeze its snapshot");
        let cleared =
            t.b.backend
                .store
                .prepare_context_update(
                    &saved.allocation.storage_key,
                    &before.version()?,
                    &uuid::Uuid::new_v4().to_string(),
                    |state| {
                        state.active.as_mut().unwrap().publication.pending = None;
                    },
                )
                .await;
        assert!(
            cleared.is_err(),
            "missing publication checkpoint must not clear pending event"
        );
    }
    t.b.backend
        .store
        .request_task_cancel(&saved.allocation.storage_key, &saved.allocation.task_id)
        .await?;
    hooks.lose_next_context_ack();
    drop(ack);
    let mut text = String::new();
    let mut artifact_deltas = 0;
    tokio::time::timeout(DEADLINE, async {
        loop {
            let event = remote.events.recv().await?;
            if let a2a_lf::StreamResponse::ArtifactUpdate(update) = &event.response {
                if update.append != Some(true) {
                    text.clear();
                }
                let delta: String = update
                    .artifact
                    .parts
                    .iter()
                    .filter_map(|part| match &part.content {
                        PartContent::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if !delta.is_empty() {
                    artifact_deltas += 1;
                }
                text.push_str(&delta);
            }
            if event.is_terminal() {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    assert_eq!(text, "Hello ");
    assert_eq!(artifact_deltas, 1, "superseded ack must not reapply text");
    assert_eq!(
        t.h.task(&saved.allocation.task_id).await?.task.status.state,
        TaskState::Canceled
    );
    t.a.backend.runner.shutdown().await;
    let mut stream = t.h.jetstream.get_stream("HARNX_A2A_TASK_EVENTS").await?;
    assert_eq!(
        stream.info().await?.state.messages,
        t.h.task(&saved.allocation.task_id).await?.stream_seq
    );
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_stage_ack_superseded_by_cancel_uses_same_committed_envelope() -> Result<()> {
    superseded_ack(false).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_checkpoint_ack_superseded_by_cancel_uses_exact_subject_checkpoint() -> Result<()> {
    superseded_ack(true).await
}
