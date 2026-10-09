use super::*;
use harnx_a2a_server::store::TaskSeed;

async fn legacy_terminal(h: &Harness) -> Result<a2a_lf::Task> {
    let session = h.session(None, &alice()).await?;
    let task = a2a_lf::Task {
        id: harnx_a2a_server::store::new_task_id(session.session_id()),
        context_id: session.session_id().into(),
        status: a2a_lf::TaskStatus {
            state: a2a_lf::TaskState::Completed,
            message: None,
            timestamp: None,
        },
        history: None,
        artifacts: None,
        metadata: None,
    };
    let record = h
        .store
        .create_task(
            session.storage_key(),
            TaskSeed {
                task: task.clone(),
                user_msg_id: "legacy".into(),
                user_msg_seq: 0,
                execution_id: "old-execution".into(),
            },
        )
        .await?;
    let mut encoded = serde_json::to_value(record)?;
    encoded.as_object_mut().unwrap().remove("stream_seq");
    let uuid = task.id.rsplit_once('.').context("task UUID")?.1;
    let key = harnx_runtime::nats_session_metadata::a2a_task_key(session.storage_key(), uuid);
    h.metadata
        .kv_store()
        .put(key, serde_json::to_vec(&encoded)?.into())
        .await?;
    Ok(task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alternating_process_old_terminal_records_and_principal_export_isolation() -> Result<()> {
    let pair = Pair::start(Script::Text).await?;
    let mut routes = pair.routes();
    let legacy = legacy_terminal(&pair.a.h).await?;
    for _ in 0..2 {
        let read = routes
            .rpc(RpcCall::new(
                "runner",
                "alice",
                "GetTask",
                json!({"id":legacy.id}),
            ))
            .await?;
        assert_eq!(read["result"]["status"]["state"], "TASK_STATE_COMPLETED");
    }
    for _ in 0..2 {
        let list = routes
            .rpc(RpcCall::new(
                "runner",
                "alice",
                "ListTasks",
                json!({"contextId":legacy.context_id}),
            ))
            .await?;
        assert_eq!(list["result"]["tasks"][0]["id"], legacy.id);
    }
    for (export, principal) in [("runner", "bob"), ("alias", "alice")] {
        denied_methods(&mut routes, export, principal, &legacy).await?;
    }
    assert_eq!(pair.a.h.llm.requests.lock().len(), 0);
    scoped_identities(&mut routes).await?;
    routes.assert_routes(&[
        "GetTask",
        "ListTasks",
        "CancelTask",
        "SubscribeToTask",
        "SendMessage",
    ]);
    Ok(())
}

async fn denied_methods(
    routes: &mut Alternating<'_>,
    export: &str,
    principal: &str,
    legacy: &a2a_lf::Task,
) -> Result<()> {
    for method in ["GetTask", "ListTasks", "CancelTask", "SubscribeToTask"] {
        for _ in 0..2 {
            let params = if method == "ListTasks" {
                json!({"contextId":legacy.context_id})
            } else {
                json!({"id":legacy.id})
            };
            let denied = routes
                .rpc(RpcCall::new(export, principal, method, params))
                .await?;
            assert_eq!(denied["error"]["code"], -32001, "{denied}");
        }
    }
    Ok(())
}

async fn scoped_identities(routes: &mut Alternating<'_>) -> Result<()> {
    let mut tasks = Vec::new();
    let mut streams = Vec::new();
    for (export, principal) in [("runner", "alice"), ("runner", "bob"), ("alias", "alice")] {
        let request = immediate(chat("same-scoped-identity", None));
        let first = task(
            &routes
                .rpc(RpcCall::new(
                    export,
                    principal,
                    "SendMessage",
                    request.clone(),
                ))
                .await?,
        )?;
        let retry = task(
            &routes
                .rpc(RpcCall::new(export, principal, "SendMessage", request))
                .await?,
        )?;
        assert_eq!(first["id"], retry["id"]);
        assert!(tasks
            .iter()
            .all(|previous: &Value| previous["id"] != first["id"]
                && previous["contextId"] != first["contextId"]));
        let mut frames = Frames::from_response(
            routes
                .request(RpcCall::new(
                    export,
                    principal,
                    "SubscribeToTask",
                    json!({"id":first["id"]}),
                ))
                .send()
                .await?,
        )?;
        let (_, answer) = lifecycle::partial(&mut frames).await?;
        streams.push((frames, answer));
        tasks.push(first);
    }
    assert_eq!(routes.pair.a.h.llm.requests.lock().len(), 3);
    routes.pair.a.h.llm.release.notify_waiters();
    for (mut frames, mut answer) in streams {
        finish(&mut frames, &mut answer).await?;
    }
    Ok(())
}
