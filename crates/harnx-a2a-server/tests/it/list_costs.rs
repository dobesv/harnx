//! ListTasks edge cases, pagination costs, and error paths.

use crate::support::{alice, Harness, Script, DEADLINE};
use a2a_lf::{Task, TaskState, TaskStatus};
use anyhow::Result;
use chrono::Utc;
use harnx_a2a_server::{
    handler::{Backend, BackendConfig, HarnxHandler},
    input_map::InputLimits,
    routes,
    store::{new_task_id, parse_task_id, IndexState, TaskSeed},
};
use harnx_runtime::nats_session_metadata::{a2a_task_key, TaskIndexEntry};
use harnx_runtime::{NatsSession, SessionActivationRoute};
use serde_json::{json, Value};
use std::sync::Arc;

struct Http {
    h: Harness,
    url: String,
    client: reqwest::Client,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Http {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Http {
    async fn start() -> Result<Self> {
        let h = Harness::start(Script::Text).await?;
        let backend = Arc::new(Backend::new(
            h.runner.clone(),
            h.store.clone(),
            BackendConfig {
                config: h.config.clone(),
                route: SessionActivationRoute::ClusterShared,
                abort: harnx_core::abort::create_abort_signal(),
            },
        ));
        let mut exports = vec![h.export.clone()];
        exports[0].lookup_keys = vec!["runner".into()];
        let app = routes::router(&exports, None, &["X-User-ID".into()], |export, identity| {
            Arc::new(HarnxHandler::new(
                export.clone(),
                identity,
                backend.clone(),
                InputLimits::default(),
            ))
        })?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/agents/runner", listener.local_addr()?);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Ok(Self {
            h,
            url,
            client: reqwest::Client::builder().timeout(DEADLINE).build()?,
            server,
        })
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let response = self
            .client
            .post(&self.url)
            .header("a2a-version", "1.0")
            .header("X-User-ID", "alice")
            .json(&json!({"jsonrpc":"2.0","id":"1","method":method,"params":params}))
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        Ok(response.json().await?)
    }
}

async fn create_test_task(
    http: &Http,
    session: &NatsSession,
    timestamp: Option<chrono::DateTime<Utc>>,
) -> Result<String> {
    let task_id = new_task_id(session.session_id());
    let task = Task {
        id: task_id.clone(),
        context_id: session.session_id().to_string(),
        status: TaskStatus {
            state: TaskState::Completed,
            message: None,
            timestamp,
        },
        artifacts: None,
        history: None,
        metadata: None,
    };
    http.h
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
    Ok(task_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_tasks_unknown_page_token_returns_invalid_params() -> Result<()> {
    let http = Http::start().await?;
    let session = http.h.session(None, &alice()).await?;

    create_test_task(&http, &session, None).await?;

    // Query ListTasks with unknown pageToken
    let list_res = http
        .rpc(
            "ListTasks",
            json!({
                "contextId": session.session_id(),
                "pageToken": "bogus-nonexistent-token"
            }),
        )
        .await?;

    assert!(list_res["error"].is_object(), "expected error response");
    assert_eq!(list_res["error"]["code"], -32602);
    assert_eq!(list_res["error"]["message"], "invalid pageToken");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_tasks_status_timestamp_after_end_to_end() -> Result<()> {
    let http = Http::start().await?;
    let session = http.h.session(None, &alice()).await?;

    let t1 = Utc::now() - chrono::Duration::seconds(100);
    let cutoff = Utc::now() - chrono::Duration::seconds(50);
    let t2 = Utc::now();

    // Turn 1 before cutoff
    create_test_task(&http, &session, Some(t1)).await?;

    // Turn 2 after cutoff
    let task2_id = create_test_task(&http, &session, Some(t2)).await?;

    // Timestamp-less tasks must be excluded. Simulate older index metadata
    // without a timestamp for a task whose authoritative record has one.
    create_test_task(&http, &session, None).await?;
    let (mut index, revision) = http
        .h
        .metadata
        .get_a2a_task_index(session.storage_key())
        .await?
        .unwrap();
    index
        .entries
        .iter_mut()
        .find(|entry| entry.task_id == task2_id)
        .unwrap()
        .status_timestamp = None;
    http.h
        .metadata
        .put_a2a_task_index(session.storage_key(), &index, Some(revision))
        .await?;

    // Query with statusTimestampAfter cutoff
    let filtered = http
        .rpc(
            "ListTasks",
            json!({
                "contextId": session.session_id(),
                "statusTimestampAfter": cutoff.to_rfc3339()
            }),
        )
        .await?;

    assert_eq!(filtered["result"]["totalSize"], 1);
    assert_eq!(filtered["result"]["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["result"]["tasks"][0]["id"], task2_id);
    let exact = http
        .rpc(
            "ListTasks",
            json!({"contextId":session.session_id(), "statusTimestampAfter":t2.to_rfc3339()}),
        )
        .await?;
    assert_eq!(exact["result"]["totalSize"].as_i64().unwrap_or(0), 0);

    Ok(())
}

async fn assert_list_page(
    http: &Http,
    session_id: &str,
    page_token: Option<&str>,
) -> Result<String> {
    let mut params = json!({"contextId": session_id, "pageSize": 1});
    if let Some(token) = page_token {
        params["pageToken"] = json!(token);
    }
    let res = http.rpc("ListTasks", params).await?;
    assert_eq!(res["result"]["totalSize"], 3);
    assert_eq!(res["result"]["tasks"].as_array().unwrap().len(), 1);
    let next_token = res["result"]["nextPageToken"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    Ok(next_token)
}

async fn inject_expired_and_recent_intents(
    http: &Http,
    storage_key: &str,
    session_id: &str,
    now: chrono::DateTime<Utc>,
) -> Result<(String, String)> {
    let expired_id = new_task_id(session_id);
    let mut expired = TaskIndexEntry::new(
        expired_id.clone(),
        IndexState::Working,
        now - chrono::Duration::seconds(400),
    );
    expired.updated_at = now - chrono::Duration::seconds(400);

    let (mut index, rev) = http
        .h
        .metadata
        .get_a2a_task_index(storage_key)
        .await?
        .unwrap();
    index.add(expired);
    let recent_id = new_task_id(session_id);
    index.add(TaskIndexEntry::new(
        recent_id.clone(),
        IndexState::Submitted,
        now,
    ));
    http.h
        .metadata
        .put_a2a_task_index(storage_key, &index, Some(rev))
        .await?;
    Ok((expired_id, recent_id))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_tasks_consistent_totals_across_pages() -> Result<()> {
    let http = Http::start().await?;
    let session = http.h.session(None, &alice()).await?;

    // Create 3 real tasks
    for _ in 1..=3 {
        create_test_task(&http, &session, None).await?;
    }

    let key = session.storage_key();
    let now = Utc::now();
    let (expired_id, recent_id) =
        inject_expired_and_recent_intents(&http, key, session.session_id(), now).await?;

    // 3 real tasks. Expired entry is cleaned up offpage before pagination,
    // ensuring consistent totalSize = 3 across all pages.
    let token1 = assert_list_page(&http, session.session_id(), None).await?;
    assert!(!token1.is_empty());

    let token2 = assert_list_page(&http, session.session_id(), Some(&token1)).await?;
    assert!(!token2.is_empty());

    let _ = assert_list_page(&http, session.session_id(), Some(&token2)).await?;

    // Verify expired dangling entry was deleted from durable index
    let (index_final, _) = http.h.metadata.get_a2a_task_index(key).await?.unwrap();
    assert!(!index_final.entries.iter().any(|e| e.task_id == expired_id));
    assert!(index_final.entries.iter().any(|e| e.task_id == recent_id));
    assert_eq!(
        index_final.entries.len(),
        4,
        "retain a recent creation intent but don't count it as a task"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_tasks_corrupt_requested_page_fails_rpc() -> Result<()> {
    let http = Http::start().await?;
    let session = http.h.session(None, &alice()).await?;

    let task_id = create_test_task(&http, &session, None).await?;

    let (_, uuid) = parse_task_id(&task_id)?;
    let record_key = a2a_task_key(session.storage_key(), uuid);

    // Corrupt record with invalid JSON
    http.h
        .metadata
        .kv_store()
        .put(record_key, "corrupt json".into())
        .await?;

    // Listing that requested page fails the RPC
    let list_res = http
        .rpc("ListTasks", json!({"contextId": session.session_id()}))
        .await?;

    assert_eq!(list_res["error"]["code"], -32603);
    assert_eq!(list_res["error"]["message"], "request failed");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn followup_send_reconciles_expired_missing_intents() -> Result<()> {
    let http = Http::start().await?;
    let session = http.h.session(None, &alice()).await?;
    let expired_id = new_task_id(session.session_id());
    let (mut index, revision) = http
        .h
        .metadata
        .get_a2a_task_index(session.storage_key())
        .await?
        .expect("bound index");
    index.add(TaskIndexEntry::new(
        expired_id.clone(),
        IndexState::Submitted,
        Utc::now() - chrono::Duration::minutes(6),
    ));
    http.h
        .metadata
        .put_a2a_task_index(session.storage_key(), &index, Some(revision))
        .await?;
    let started = http
        .rpc(
            "SendMessage",
            json!({"message": {
        "messageId":"cleanup-followup", "contextId": session.session_id(),
        "role":"ROLE_USER", "parts":[{"text":"new turn"}]
    }, "configuration":{"returnImmediately":true}}),
        )
        .await?;
    assert_eq!(
        started["result"]["task"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    let (index, _) = http
        .h
        .metadata
        .get_a2a_task_index(session.storage_key())
        .await?
        .unwrap();
    assert_eq!(index.entries.len(), 1);
    assert_ne!(index.entries[0].task_id, expired_id);
    http.h.runner.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_tasks_cleanup_failure_is_logged_without_failing_page() -> Result<()> {
    let http = Http::start().await?;
    let session = http.h.session(None, &alice()).await?;
    let task_id = create_test_task(&http, &session, None).await?;
    let missing_id = new_task_id(session.session_id());
    let (mut index, revision) = http
        .h
        .metadata
        .get_a2a_task_index(session.storage_key())
        .await?
        .unwrap();
    index.add(TaskIndexEntry::new(
        missing_id.clone(),
        IndexState::Submitted,
        Utc::now() - chrono::Duration::minutes(6),
    ));
    http.h
        .metadata
        .put_a2a_task_index(session.storage_key(), &index, Some(revision))
        .await?;
    // Keep point reads working, but make the opportunistic index rewrite fail.
    let mut config = http.h.metadata.kv_store().status().await?.info.config;
    config.max_message_size = 1;
    http.h.jetstream.update_stream(&config).await?;
    let page = http
        .rpc(
            "ListTasks",
            json!({"contextId":session.session_id(), "pageSize":1}),
        )
        .await?;
    assert!(page["error"].is_null(), "{page}");
    assert_eq!(page["result"]["totalSize"], 1);
    assert_eq!(page["result"]["tasks"][0]["id"], task_id);
    assert!(http
        .h
        .logs
        .text()
        .contains("dangling index cleanup deferred; continuing ListTasks"));
    let (index, _) = http
        .h
        .metadata
        .get_a2a_task_index(session.storage_key())
        .await?
        .unwrap();
    assert!(
        index.get(&missing_id).is_some(),
        "failed cleanup must leave the durable index unchanged"
    );
    Ok(())
}
