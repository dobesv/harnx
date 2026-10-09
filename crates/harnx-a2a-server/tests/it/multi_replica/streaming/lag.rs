use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lagged_remote_reader_errors_while_independent_reader_and_task_complete() -> Result<()> {
    let h = Harness::start(Script::Many).await?;
    let a = Replica::new(&h, h.store.clone(), h.runner.clone())?;
    let store = Arc::new(A2aStore::new(h.metadata.clone()));
    let b = Replica::new(&h, store.clone(), Runner::new(store))?;
    let first = a.rpc("alice", "SendMessage", first_message()).await?;
    let id = result_task(&first)?["id"].as_str().context("task id")?;
    let owner = alice().into();
    let mut healthy = b.backend.runner.subscribe(&h.export, &owner, id).await?;
    let mut text: String = healthy
        .snapshot
        .task
        .artifacts
        .as_ref()
        .map(|artifacts| {
            artifacts[0]
                .parts
                .iter()
                .filter_map(|part| match &part.content {
                    PartContent::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    if text.is_empty() {
        let event = tokio::time::timeout(DEADLINE, healthy.events.recv()).await??;
        if let a2a_lf::StreamResponse::ArtifactUpdate(update) = event.response {
            text.extend(
                update
                    .artifact
                    .parts
                    .iter()
                    .filter_map(|part| match &part.content {
                        PartContent::Text(text) => Some(text.as_str()),
                        _ => None,
                    }),
            );
        } else {
            anyhow::bail!("first remote chunk missing");
        }
    }
    assert_eq!(text, "x");
    let mut slow = b.backend.runner.subscribe(&h.export, &owner, id).await?;
    let chunks = harnx_a2a_server::runner::EVENT_CAPACITY + 5;
    for count in 2..=chunks {
        h.llm.release.notify_one();
        let event = tokio::time::timeout(DEADLINE, healthy.events.recv()).await??;
        let a2a_lf::StreamResponse::ArtifactUpdate(update) = event.response else {
            anyhow::bail!("chunk missing");
        };
        text.extend(
            update
                .artifact
                .parts
                .iter()
                .filter_map(|part| match &part.content {
                    PartContent::Text(text) => Some(text.as_str()),
                    _ => None,
                }),
        );
        assert_eq!(text, "x".repeat(count));
    }
    // Let the independent pump catch up durably before inspecting its bounded buffer.
    h.llm.release.notify_one();
    assert_eq!(
        direct_finish(healthy.events, text).await?,
        "x".repeat(chunks)
    );
    tokio::time::timeout(DEADLINE, async {
        while slow.events.len() <= harnx_a2a_server::runner::EVENT_CAPACITY {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert!(matches!(
        slow.events.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
    assert_eq!(h.task(id).await?.task.status.state, TaskState::Completed);
    let mut stream = h.jetstream.get_stream("HARNX_A2A_TASK_EVENTS").await?;
    assert_eq!(stream.info().await?.state.consumer_count, 0);
    a.backend.runner.shutdown().await;
    Ok(())
}
