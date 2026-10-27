use super::fixtures::*;
use crate::support::DEADLINE;
use anyhow::Result;
use harnx_a2a_server::store::TaskSeed;
use serde_json::{json, Value};

async fn assert_unsupported_optional_methods(http: &Http, task_id: &Value) -> Result<()> {
    for (method, params) in [
        (
            "CreateTaskPushNotificationConfig",
            json!({"taskId": task_id, "pushNotificationConfig": {"url": "https://example.com/push"}}),
        ),
        (
            "GetTaskPushNotificationConfig",
            json!({"taskId": task_id, "id": "config"}),
        ),
        (
            "ListTaskPushNotificationConfigs",
            json!({"taskId": task_id}),
        ),
        (
            "DeleteTaskPushNotificationConfig",
            json!({"taskId": task_id, "id": "config"}),
        ),
    ] {
        let response = http.rpc(method, params).await?;
        golden(&response, &fixture("error_push_not_supported.json"), "");
    }
    code(&http.rpc("GetExtendedAgentCard", json!({})).await?, -32004);
    code(
        &http.rpc("SubscribeToTask", json!({"id": task_id})).await?,
        -32004,
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_cancel_live_and_optional_methods() -> Result<()> {
    let http = Http::start().await?;
    let first = http.working(message("cancel", None)).await?;
    let cancel = http.rpc("CancelTask", json!({"id": first["id"]})).await?;
    assert_eq!(
        cancel["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{cancel}"
    );
    let cancel = http.rpc("CancelTask", json!({"id": first["id"]})).await?;
    code(&cancel, -32002);
    assert_eq!(
        cancel["error"]["message"],
        a2a_lf::A2AError::task_not_cancelable(first["id"].as_str().unwrap()).message
    );
    // Runtime interrupt must not poison later turns or other contexts.
    let followup = http
        .working(message("after-cancel", Some(&first["contextId"])))
        .await?;
    http.h.llm.release.notify_one();
    http.completed(&followup["id"]).await?;
    assert_unsupported_optional_methods(&http, &first["id"]).await?;
    Ok(())
}

async fn assert_concurrent_new_context_retry(http: &Http) -> Result<Value> {
    let (one, two) = tokio::join!(
        http.working(message("race", None)),
        http.working(message("race", None))
    );
    let one = one?;
    assert_eq!(one["id"], two?["id"]);
    http.h.llm.release.notify_one();
    http.completed(&one["id"]).await?;
    Ok(one)
}

async fn disconnect_inflight_request(http: &Http, one: &Value) -> Result<Value> {
    let client = http.client.clone();
    let url = http.url.clone();
    let context = one["contextId"].clone();
    let request = tokio::spawn(async move {
        client
            .post(url)
            .header("a2a-version", "1.0")
            .header("X-User-ID", "alice")
            .json(&json!({"jsonrpc":"2.0","id":"1","method":"SendMessage","params":{"message":message("disconnect",Some(&context))}}))
            .send()
            .await
    });
    tokio::time::timeout(DEADLINE, async {
        while http.h.llm.requests.lock().len() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    request.abort();
    let list = http
        .rpc("ListTasks", json!({"contextId": one["contextId"]}))
        .await?;
    let task = list["result"]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] != one["id"])
        .unwrap()
        .clone();
    Ok(task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_concurrent_new_context_retries_and_disconnect() -> Result<()> {
    let http = Http::start().await?;
    let one = assert_concurrent_new_context_retry(&http).await?;
    let task = disconnect_inflight_request(&http, &one).await?;
    http.h.llm.release.notify_one();
    http.completed(&task["id"]).await?;
    let retry = http
        .rpc(
            "SendMessage",
            json!({"message": message("disconnect", Some(&one["contextId"]))}),
        )
        .await?;
    assert_eq!(retry["result"]["task"]["id"], task["id"]);
    Ok(())
}

async fn create_orphan_task(http: &Http, session: &harnx_runtime::NatsSession) -> Result<String> {
    let task = a2a_lf::Task {
        id: harnx_a2a_server::store::new_task_id(session.session_id()),
        context_id: session.session_id().into(),
        status: a2a_lf::TaskStatus {
            state: a2a_lf::TaskState::Working,
            message: None,
            timestamp: None,
        },
        history: None,
        artifacts: None,
        metadata: None,
    };
    let record = http
        .h
        .store
        .create_task(
            session.storage_key(),
            TaskSeed {
                task,
                user_msg_id: String::new(),
                user_msg_seq: 0,
                execution_id: String::new(),
            },
        )
        .await?;
    Ok(record.task.id)
}

async fn assert_orphan_reconciled_on_get(
    http: &Http,
    session: &harnx_runtime::NatsSession,
) -> Result<()> {
    let get_id = create_orphan_task(http, session).await?;
    let get = http.rpc("GetTask", json!({"id": get_id})).await?;
    assert_eq!(get["result"]["status"]["state"], "TASK_STATE_FAILED");
    assert_eq!(
        get["result"]["status"]["message"]["parts"][0]["text"],
        "interrupted by server restart"
    );
    Ok(())
}

async fn assert_orphan_reconciled_on_cancel(
    http: &Http,
    session: &harnx_runtime::NatsSession,
) -> Result<()> {
    let cancel_id = create_orphan_task(http, session).await?;
    code(
        &http.rpc("CancelTask", json!({"id": cancel_id})).await?,
        -32002,
    );
    assert_eq!(
        http.h.task(&cancel_id).await?.task.status.state,
        a2a_lf::TaskState::Failed
    );
    Ok(())
}

async fn assert_orphan_reconciled_on_send(
    http: &Http,
    session: &harnx_runtime::NatsSession,
    context: &Value,
) -> Result<()> {
    let send_id = create_orphan_task(http, session).await?;
    let started = http
        .working(message("after-restart", Some(context)))
        .await?;
    assert_eq!(
        http.h.task(&send_id).await?.task.status.state,
        a2a_lf::TaskState::Failed
    );
    http.h.llm.release.notify_one();
    http.completed(&started["id"]).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_orphans_reconciled_before_get_cancel_and_new_send() -> Result<()> {
    let http = Http::start().await?;
    let session = http.h.session(None, &crate::support::alice()).await?;
    let context = json!(session.session_id());
    assert_orphan_reconciled_on_get(&http, &session).await?;
    assert_orphan_reconciled_on_cancel(&http, &session).await?;
    assert_orphan_reconciled_on_send(&http, &session, &context).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_shutdown_settles_detached_turn() -> Result<()> {
    let http = Http::start().await?;
    let started = http.working(message("shutdown", None)).await?;
    tokio::time::timeout(DEADLINE, http.h.runner.shutdown()).await?;
    let get = http.rpc("GetTask", json!({"id":started["id"]})).await?;
    assert_eq!(
        get["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{get}"
    );
    let rejected = http
        .rpc(
            "SendMessage",
            json!({"message":message("late-send", Some(&started["contextId"]))}),
        )
        .await?;
    code(&rejected, -32603);
    assert_eq!(rejected["error"]["message"], "request failed");
    Ok(())
}
