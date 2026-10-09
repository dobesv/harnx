//! ListTasks, pagination, task history, and live query integration tests.
use super::unary::{code, fixture, golden, message, Http};
use a2a_lf::{Task, TaskState, TaskStatus};
use anyhow::{Context, Result};
use harnx_a2a_server::runner::A2aEvent;
use harnx_a2a_server::store::{new_task_id, parse_task_id, TaskSeed};
use harnx_runtime::nats_session_metadata::a2a_task_key;
use serde_json::{json, Value};

async fn execute_two_turn_history(http: &Http) -> Result<(Value, Value)> {
    let first = http.working(message("first", None)).await?;
    golden(
        &json!({"jsonrpc":"2.0","id":"1","result":{"task":first}}),
        &fixture("send_message_result.json"),
        "",
    );
    http.h.llm.release.notify_one();
    http.completed(&first["id"]).await?;
    let second = http
        .working(message("second", Some(&first["contextId"])))
        .await?;
    assert_ne!(second["id"], first["id"]);
    assert_eq!(second["contextId"], first["contextId"]);
    http.h.llm.release.notify_one();
    http.completed(&second["id"]).await?;
    let requests = http.h.llm.requests.lock();
    let followup = requests.last().unwrap().to_string();
    assert!(
        followup.contains("Hello world"),
        "follow-up lost prior answer: {followup}"
    );
    assert!(followup.matches("Hello, agent").count() >= 2);
    Ok((first, second))
}

async fn assert_get_task_history_options(http: &Http, second_id: &Value) -> Result<()> {
    for (length, expected) in [(0, 0), (1, 1), (9, 2)] {
        let response = http
            .rpc("GetTask", json!({"id":second_id,"historyLength":length}))
            .await?;
        assert_eq!(
            response["result"]["history"].as_array().map_or(0, Vec::len),
            expected,
            "{response}"
        );
    }
    code(
        &http
            .rpc("GetTask", json!({"id":second_id,"historyLength":-1}))
            .await?,
        -32602,
    );
    Ok(())
}

async fn assert_list_tasks_pagination_and_scoping(
    http: &Http,
    first_context: &Value,
) -> Result<()> {
    let list = http
        .rpc(
            "ListTasks",
            json!({"contextId":first_context,"pageSize":1,"includeArtifacts":false,"historyLength":0}),
        )
        .await?;
    assert_eq!(list["result"]["totalSize"], 2);
    assert_eq!(list["result"]["tasks"].as_array().unwrap().len(), 1);
    assert!(list["result"]["tasks"][0].get("artifacts").is_none());
    let next = http
        .rpc(
            "ListTasks",
            json!({"contextId":first_context,"pageSize":1,"pageToken":list["result"]["nextPageToken"]}),
        )
        .await?;
    assert_eq!(next["result"]["tasks"].as_array().unwrap().len(), 1);
    assert_ne!(
        next["result"]["tasks"][0]["id"],
        list["result"]["tasks"][0]["id"]
    );
    code(&http.rpc("ListTasks", json!({})).await?, -32602);
    code(
        &http
            .rpc(
                "ListTasks",
                json!({"contextId":first_context,"pageToken":"bogus"}),
            )
            .await?,
        -32602,
    );
    let third = http.working(message("third", None)).await?;
    http.h.llm.release.notify_one();
    http.completed(&third["id"]).await?;
    let third_list = http
        .rpc("ListTasks", json!({"contextId":third["contextId"]}))
        .await?;
    assert_eq!(third_list["result"]["tasks"].as_array().unwrap().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_immediate_get_multiturn_history_and_scoped_list() -> Result<()> {
    let http = Http::start().await?;
    let (first, second) = execute_two_turn_history(&http).await?;
    assert_get_task_history_options(&http, &second["id"]).await?;
    assert_list_tasks_pagination_and_scoping(&http, &first["contextId"]).await?;
    Ok(())
}

async fn create_completed_task(http: &Http, msg_id: &str, context: &Value) -> Result<Value> {
    let task = http.working(message(msg_id, Some(context))).await?;
    http.h.llm.release.notify_one();
    http.completed(&task["id"]).await?;
    Ok(task)
}

async fn fetch_completed_page(
    http: &Http,
    context: &Value,
    token: Option<&str>,
) -> Result<(Value, String)> {
    let mut params = json!({
        "contextId": context,
        "status": "TASK_STATE_COMPLETED",
        "pageSize": 1
    });
    if let Some(tok) = token {
        params["pageToken"] = json!(tok);
    }
    let page = http.rpc("ListTasks", params).await?;
    assert_eq!(page["result"]["totalSize"], 3);
    assert_eq!(page["result"]["tasks"].as_array().unwrap().len(), 1);
    let next_token = page["result"]["nextPageToken"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    Ok((page, next_token))
}

async fn assert_empty_filtered_status(http: &Http, context: &Value, status: &str) -> Result<()> {
    let page = http
        .rpc(
            "ListTasks",
            json!({
                "contextId": context,
                "status": status
            }),
        )
        .await?;
    let total = page["result"]
        .get("totalSize")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let task_count = page["result"]
        .get("tasks")
        .map(|t| t.as_array().unwrap().len())
        .unwrap_or(0);
    assert_eq!(total, 0);
    assert_eq!(task_count, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_list_tasks_metadata_filter_pagination_and_total_size() -> Result<()> {
    let http = Http::start().await?;
    let session = http.h.session(None, &crate::support::alice()).await?;
    let context = json!(session.session_id());

    for msg in ["msg-1", "msg-2", "msg-3"] {
        create_completed_task(&http, msg, &context).await?;
    }

    let (page1, next1) = fetch_completed_page(&http, &context, None).await?;
    assert!(!next1.is_empty());

    let (page2, next2) = fetch_completed_page(&http, &context, Some(&next1)).await?;
    assert_ne!(
        page1["result"]["tasks"][0]["id"],
        page2["result"]["tasks"][0]["id"]
    );
    assert!(!next2.is_empty());

    let (_page3, next3) = fetch_completed_page(&http, &context, Some(&next2)).await?;
    assert_eq!(next3, "");

    assert_empty_filtered_status(&http, &context, "TASK_STATE_FAILED").await?;
    Ok(())
}

async fn wait_for_stream_artifact(
    events: &mut tokio::sync::broadcast::Receiver<A2aEvent>,
) -> Result<()> {
    tokio::time::timeout(crate::support::DEADLINE, async {
        loop {
            if matches!(
                events.recv().await?.response,
                a2a_lf::StreamResponse::ArtifactUpdate(_)
            ) {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_live_get_list_and_blocking_dedupe_waiters() -> Result<()> {
    let http = Http::start().await?;
    let input = message("live-read-retry", None);
    let started = http.working(input.clone()).await?;
    let id = started["id"].as_str().unwrap();
    let mut sub = http
        .h
        .runner
        .subscribe(&http.h.export, &crate::support::alice().into(), id)
        .await?;
    if sub.snapshot.task.artifacts.is_none() {
        wait_for_stream_artifact(&mut sub.events).await?;
    }
    let get = http.rpc("GetTask", json!({"id":id})).await?;
    assert_eq!(get["result"]["artifacts"][0]["parts"][0]["text"], "Hello ");
    let list = http
        .rpc(
            "ListTasks",
            json!({"contextId":started["contextId"],"status":"TASK_STATE_WORKING"}),
        )
        .await?;
    assert_eq!(
        list["result"]["tasks"][0]["artifacts"][0]["parts"][0]["text"],
        "Hello "
    );
    // Every retry waits on completion but doesn't append or execute another prompt.
    let retries = (0..8).map(|_| http.rpc("SendMessage", json!({"message":input})));
    let release = async {
        http.h.llm.release.notify_one();
    };
    let (responses, ()) = tokio::join!(futures::future::join_all(retries), release);
    for response in responses {
        let response = response?;
        assert_eq!(response["result"]["task"]["id"], started["id"]);
        assert_eq!(
            response["result"]["task"]["status"]["state"],
            "TASK_STATE_COMPLETED"
        );
        assert_eq!(
            response["result"]["task"]["artifacts"][0]["parts"][0]["text"],
            "Hello world"
        );
    }
    // Already-terminal retry must return without waiting for another notification.
    let retry = http.rpc("SendMessage", json!({"message":input})).await?;
    assert_eq!(
        retry["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

async fn seed_task_with_state(
    http: &Http,
    storage_key: &str,
    session_id: &str,
    state: TaskState,
) -> Result<String> {
    let task = Task {
        id: new_task_id(session_id),
        context_id: session_id.into(),
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        artifacts: None,
        history: None,
        metadata: None,
    };
    let record = http
        .h
        .store
        .create_task(
            storage_key,
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

async fn corrupt_off_page_record(http: &Http, storage_key: &str, task_id: &str) -> Result<()> {
    let (_, uuid) = parse_task_id(task_id)?;
    let off_page = a2a_task_key(storage_key, uuid);
    http.h
        .metadata
        .kv_store()
        .put(off_page, "invalid json".into())
        .await?;
    Ok(())
}

async fn assert_reconciled_orphan_in_index(
    http: &Http,
    storage_key: &str,
    task_id: &str,
) -> Result<()> {
    let (index, _) = http
        .h
        .metadata
        .get_a2a_task_index(storage_key)
        .await?
        .unwrap();
    let entry = index
        .entries
        .iter()
        .find(|entry| entry.task_id == task_id)
        .context("orphan index entry missing")?;
    assert_eq!(
        entry.state,
        harnx_runtime::nats_session_metadata::TaskState::Failed
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_index_pages_do_not_read_off_page_records_and_reconcile_filtered_orphans(
) -> Result<()> {
    let http = Http::start().await?;
    let session = http.h.session(None, &crate::support::alice()).await?;
    let (storage_key, session_id) = (session.storage_key(), session.session_id());

    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(seed_task_with_state(&http, storage_key, session_id, TaskState::Completed).await?);
    }
    ids.sort();

    corrupt_off_page_record(&http, storage_key, &ids[3]).await?;

    let page = http
        .rpc(
            "ListTasks",
            json!({"contextId": session_id, "status": "TASK_STATE_COMPLETED", "pageSize": 1}),
        )
        .await?;
    assert_eq!(page["result"]["totalSize"], 4);
    assert_eq!(page["result"]["tasks"][0]["id"], ids[0]);
    assert_eq!(page["result"]["nextPageToken"], ids[0]);

    let orphan_id =
        seed_task_with_state(&http, storage_key, session_id, TaskState::Working).await?;
    let failed = http
        .rpc(
            "ListTasks",
            json!({"contextId": session_id, "status": "TASK_STATE_FAILED", "pageSize": 1}),
        )
        .await?;
    assert_eq!(failed["result"]["totalSize"], 1);
    assert_eq!(failed["result"]["tasks"][0]["id"], orphan_id);

    assert_reconciled_orphan_in_index(&http, storage_key, &orphan_id).await?;
    Ok(())
}
