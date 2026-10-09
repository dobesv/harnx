use super::*;

async fn handoff(boundary: Boundary, complete: bool) -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut publish =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 2);
    t.a.rpc("alice", "SendMessage", first_message()).await?;
    publish.reached().await;
    let http = Arc::new(RemoteHttp::start(&t.b).await?);
    let mut handoff = t.b.backend.runner.fault_hooks().pause(boundary, 1);
    let stream = tokio::spawn({
        let http = http.clone();
        async move { http.open("SendStreamingMessage", first_message()).await }
    });
    handoff.reached().await;
    drop(publish);
    wait_cursor(&t, 2, false).await?;
    if complete {
        t.h.llm.release.notify_one();
        wait_cursor(&t, 3, true).await?;
    }
    drop(handoff);
    let mut frames = stream.await??;
    let first = frames.event().await?.context("snapshot first")?;
    assert!(first.get("task").is_some());
    if !complete {
        t.h.llm.release.notify_one();
    }
    assert_eq!(finish_frames(&mut frames, first).await?, "Hello world");
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    t.a.backend.runner.shutdown().await;
    Ok(())
}

macro_rules! case {
    ($name:ident, $phase:ident, $terminal:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() -> Result<()> {
            handoff(Boundary::$phase, $terminal).await
        }
    };
}
case!(
    intermediate_after_global_watermark_before_snapshot,
    WatermarkCaptured,
    false
);
case!(
    intermediate_after_snapshot_before_reader,
    SnapshotCaptured,
    false
);
case!(
    intermediate_after_reader_creation_before_delivery,
    ReaderCreated,
    false
);
case!(
    terminal_after_global_watermark_before_snapshot,
    WatermarkCaptured,
    true
);
case!(
    terminal_after_snapshot_before_reader,
    SnapshotCaptured,
    true
);
case!(
    terminal_after_reader_creation_before_delivery,
    ReaderCreated,
    true
);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_subscribers_and_terminal_waiter_follow_remote_owner_once() -> Result<()> {
    let t = TwoBackends::start().await?;
    let mut publish =
        t.a.backend
            .runner
            .fault_hooks()
            .pause(Boundary::Publication, 2);
    let first = t.a.rpc("alice", "SendMessage", first_message()).await?;
    publish.reached().await;
    let id = result_task(&first)?["id"].as_str().context("task")?;
    let http = RemoteHttp::start(&t.b).await?;
    let mut one = http.open("SubscribeToTask", json!({"id":id})).await?;
    let mut two = http.open("SubscribeToTask", json!({"id":id})).await?;
    let first_one = one.event().await?.context("first subscriber snapshot")?;
    let first_two = two.event().await?.context("second subscriber snapshot")?;
    assert_eq!(first_one, first_two);
    let handler = HarnxHandler::new(
        t.h.export.clone(),
        harnx_a2a_server::identity::Identity::default(),
        t.b.backend.clone(),
        InputLimits::default(),
    );
    let waiter = tokio::spawn({
        let id = id.to_owned();
        async move { handler.wait_terminal(&alice().into(), &id).await }
    });
    drop(publish);
    wait_cursor(&t, 2, false).await?;
    // Reconnect uses the authoritative cursor, not a replay requirement on client.
    let mut reconnect = http.open("SubscribeToTask", json!({"id":id})).await?;
    let fresh = reconnect.event().await?.context("reconnected snapshot")?;
    assert_eq!(snapshot_text(&fresh["task"]), "Hello ");
    t.h.llm.release.notify_one();
    let (a, b, c) = tokio::try_join!(
        finish_frames(&mut one, first_one),
        finish_frames(&mut two, first_two),
        finish_frames(&mut reconnect, fresh)
    )?;
    assert_eq!(
        (a, b, c),
        (
            "Hello world".into(),
            "Hello world".into(),
            "Hello world".into()
        )
    );
    assert_eq!(waiter.await??.task.status.state, TaskState::Completed);
    let mut stream = t.h.jetstream.get_stream("HARNX_A2A_TASK_EVENTS").await?;
    assert_eq!(stream.info().await?.state.consumer_count, 0);
    assert_eq!(t.h.llm.requests.lock().len(), 1);
    t.a.backend.runner.shutdown().await;
    Ok(())
}
