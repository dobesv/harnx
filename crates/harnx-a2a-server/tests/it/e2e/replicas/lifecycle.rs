use super::*;

pub(super) async fn partial(frames: &mut Frames) -> Result<(Value, String)> {
    let first = frames.event().await?.context("snapshot first")?;
    let mut answer = snapshot_text(&first["task"]);
    if answer.is_empty() {
        until_artifact(frames, &mut answer).await?;
    }
    assert_eq!(answer, "Hello ");
    Ok((first["task"].clone(), answer))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alternating_process_routes_dedupe_exact_stream_busy_and_scoped_cancel() -> Result<()> {
    let pair = Pair::start(Script::CountedTool).await?;
    let mut routes = pair.routes();
    let request = immediate(chat("one-identity", None));
    let a = routes.request(RpcCall::new(
        "runner",
        "alice",
        "SendMessage",
        request.clone(),
    ));
    let b = routes.request(RpcCall::new(
        "runner",
        "alice",
        "SendMessage",
        request.clone(),
    ));
    let (left, right) = tokio::try_join!(a.send(), b.send())?;
    let left = task(&left.json().await?)?;
    let right = task(&right.json().await?)?;
    assert_eq!(left["id"], right["id"]);
    assert_eq!(left["contextId"], right["contextId"]);
    let id = left["id"].clone();
    let context = left["contextId"].clone();
    pair.a.requested(2).await?;
    assert_eq!(tool_count(&pair.a.h)?, 1);
    one_prompt(&pair.a.h, &context).await?;

    let streams = four_readers(&mut routes, &request, &id).await?;
    active_reads_and_rejections(&mut routes, &context, &id, &request).await?;
    pair.a.h.llm.release.notify_one();
    for (mut frames, mut answer) in streams {
        finish(&mut frames, &mut answer).await?;
    }
    let completed = settled(&pair.a.h, &id).await?;
    assert_eq!(completed.task.status.state, a2a_lf::TaskState::Completed);
    for _ in 0..2 {
        assert_eq!(
            task(
                &routes
                    .rpc(RpcCall::new(
                        "runner",
                        "alice",
                        "SendMessage",
                        request.clone()
                    ))
                    .await?
            )?["id"],
            id
        );
    }
    assert_eq!(tool_count(&pair.a.h)?, 1);
    assert_eq!(pair.a.h.llm.requests.lock().len(), 2);

    follow_and_cancel(&mut routes, &context, &id).await?;
    routes.assert_routes(&[
        "SendMessage",
        "SendStreamingMessage",
        "SubscribeToTask",
        "GetTask",
        "ListTasks",
        "CancelTask",
    ]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alternating_process_disconnect_does_not_cancel_and_remote_cancel_targets_only_one_context(
) -> Result<()> {
    let pair = Pair::start(Script::Text).await?;
    let mut routes = pair.routes();
    let mut first = routes
        .stream(
            "SendStreamingMessage",
            json!({"message":chat("owner-a", None)}),
        )
        .await?;
    let (task_a, _) = partial(&mut first).await?;
    let mut second = routes
        .stream(
            "SendStreamingMessage",
            json!({"message":chat("owner-b", None)}),
        )
        .await?;
    let (task_b, _) = partial(&mut second).await?;
    drop(first);
    let canceled = routes
        .rpc(RpcCall::new(
            "runner",
            "alice",
            "CancelTask",
            json!({"id":task_b["id"]}),
        ))
        .await?;
    assert_eq!(canceled["result"]["status"]["state"], "TASK_STATE_CANCELED");
    terminal_frames(&mut second, "TASK_STATE_CANCELED").await?;
    let read = routes
        .rpc(RpcCall::new(
            "runner",
            "alice",
            "GetTask",
            json!({"id":task_a["id"]}),
        ))
        .await?;
    assert_eq!(read["result"]["status"]["state"], "TASK_STATE_WORKING");
    let mut reconnect = routes
        .stream("SubscribeToTask", json!({"id":task_a["id"]}))
        .await?;
    let (_, mut answer_a) = partial(&mut reconnect).await?;
    pair.a.h.llm.release.notify_one();
    finish(&mut reconnect, &mut answer_a).await?;
    assert_eq!(pair.a.h.llm.requests.lock().len(), 2);
    Ok(())
}

async fn follow_and_cancel(
    routes: &mut Alternating<'_>,
    context: &Value,
    id: &Value,
) -> Result<()> {
    let follow = immediate(chat("follow", Some(context)));
    let next = task(
        &routes
            .rpc(RpcCall::new(
                "runner",
                "alice",
                "SendMessage",
                follow.clone(),
            ))
            .await?,
    )?;
    let retry = task(
        &routes
            .rpc(RpcCall::new("runner", "alice", "SendMessage", follow))
            .await?,
    )?;
    assert_eq!(next["id"], retry["id"]);
    assert_ne!(next["id"], *id);
    routes.pair.a.requested(4).await?;
    assert_eq!(tool_count(&routes.pair.a.h)?, 2);
    for _ in 0..2 {
        let old_cancel = routes
            .rpc(RpcCall::new(
                "runner",
                "alice",
                "CancelTask",
                json!({"id":id}),
            ))
            .await?;
        assert_eq!(old_cancel["error"]["code"], -32002);
    }
    assert_eq!(
        routes
            .pair
            .a
            .h
            .task(next["id"].as_str().unwrap())
            .await?
            .task
            .status
            .state,
        a2a_lf::TaskState::Working
    );
    let mut subscription = routes
        .stream("SubscribeToTask", json!({"id":next["id"]}))
        .await?;
    partial(&mut subscription).await?;
    let canceled = routes
        .rpc(RpcCall::new(
            "runner",
            "alice",
            "CancelTask",
            json!({"id":next["id"]}),
        ))
        .await?;
    assert_eq!(canceled["result"]["status"]["state"], "TASK_STATE_CANCELED");
    terminal_frames(&mut subscription, "TASK_STATE_CANCELED").await?;
    assert_eq!(tool_count(&routes.pair.a.h)?, 2);
    assert_eq!(routes.pair.a.h.llm.requests.lock().len(), 4);
    Ok(())
}

async fn four_readers(
    routes: &mut Alternating<'_>,
    request: &Value,
    id: &Value,
) -> Result<Vec<(Frames, String)>> {
    let mut streams = Vec::new();
    for method in ["SendStreamingMessage", "SubscribeToTask"] {
        for _ in 0..2 {
            let params = if method == "SendStreamingMessage" {
                request.clone()
            } else {
                json!({"id":id})
            };
            let mut frames = routes.stream(method, params).await?;
            let (snapshot, answer) = partial(&mut frames).await?;
            assert_eq!(snapshot["id"], *id);
            streams.push((frames, answer));
        }
    }
    Ok(streams)
}

async fn active_reads_and_rejections(
    routes: &mut Alternating<'_>,
    context: &Value,
    id: &Value,
    request: &Value,
) -> Result<()> {
    for _ in 0..2 {
        let read = routes
            .rpc(RpcCall::new("runner", "alice", "GetTask", json!({"id":id})))
            .await?;
        assert_eq!(read["result"]["status"]["state"], "TASK_STATE_WORKING");
    }
    for _ in 0..2 {
        let list = routes
            .rpc(RpcCall::new(
                "runner",
                "alice",
                "ListTasks",
                json!({"contextId":context}),
            ))
            .await?;
        assert_eq!(
            list["result"]["tasks"].as_array().context("tasks")?.len(),
            1
        );
        assert_eq!(list["result"]["tasks"][0]["id"], *id);
    }
    for _ in 0..2 {
        let busy = routes
            .rpc(RpcCall::new(
                "runner",
                "alice",
                "SendMessage",
                immediate(chat("distinct-busy", Some(context))),
            ))
            .await?;
        assert_eq!(busy["error"]["code"], -32000, "{busy}");
    }
    let mut changed = request.clone();
    changed["message"]["parts"][0]["text"] = json!("changed payload");
    for _ in 0..2 {
        let mismatch = routes
            .rpc(RpcCall::new(
                "runner",
                "alice",
                "SendMessage",
                changed.clone(),
            ))
            .await?;
        assert_eq!(mismatch["error"]["code"], -32602);
    }
    Ok(())
}
