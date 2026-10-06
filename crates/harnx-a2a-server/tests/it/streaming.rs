use crate::{
    support::DEADLINE,
    unary::{message, Http, HttpRpcCall},
};
use anyhow::{Context, Result};
use harnx_a2a_server::store::TaskSeed;
use serde_json::{json, Value};

// Read frames across arbitrary HTTP chunk boundaries, including comment frames.
pub(super) struct Frames {
    response: reqwest::Response,
    buffer: Vec<u8>,
}
impl Frames {
    async fn open(http: &Http, method: &str, params: Value) -> Result<Self> {
        Self::open_version(http, method, params, Some("1.0")).await
    }
    async fn open_version(
        http: &Http,
        method: &str,
        params: Value,
        version: Option<&str>,
    ) -> Result<Self> {
        let mut request = http.client.post(&http.url).header("X-User-ID", "alice");
        if let Some(version) = version {
            request = request.header("a2a-version", version);
        }
        let response = request
            .json(&json!({"jsonrpc":"2.0","id":"stream-request","method":method,"params":params}))
            .send()
            .await?;
        Self::from_response(response)
    }
    pub(super) fn from_response(response: reqwest::Response) -> Result<Self> {
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert_eq!(response.headers()["cache-control"], "no-cache");
        assert_eq!(response.headers()["x-accel-buffering"], "no");
        Ok(Self {
            response,
            buffer: vec![],
        })
    }
    async fn frame(&mut self) -> Result<Option<String>> {
        tokio::time::timeout(DEADLINE, async {
            loop {
                if let Some(end) = self.buffer.windows(2).position(|bytes| bytes == b"\n\n") {
                    let frame = self.buffer.drain(..end + 2).collect();
                    return Ok(Some(String::from_utf8(frame)?));
                }
                match self.response.chunk().await? {
                    Some(chunk) => self.buffer.extend_from_slice(&chunk),
                    None => {
                        assert!(self.buffer.is_empty());
                        return Ok(None);
                    }
                }
            }
        })
        .await
        .context("SSE frame deadline")?
    }
    pub(super) async fn event(&mut self) -> Result<Option<Value>> {
        while let Some(frame) = self.frame().await? {
            if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data:")) {
                let envelope: Value = serde_json::from_str(data.trim())?;
                assert_eq!(envelope["jsonrpc"], "2.0");
                assert_eq!(envelope["id"], "stream-request");
                assert!(envelope["error"].is_null(), "{envelope}");
                let result = envelope["result"].clone();
                assert_eq!(result.as_object().unwrap().len(), 1, "{result}");
                assert!(["task", "message", "statusUpdate", "artifactUpdate"]
                    .iter()
                    .any(|key| result.get(key).is_some()));
                assert!(result.get("kind").is_none() && result.get("final").is_none());
                return Ok(Some(result));
            }
        }
        Ok(None)
    }
}
fn text(parts: &Value) -> String {
    parts
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect()
}
pub(super) fn snapshot_text(task: &Value) -> String {
    task["artifacts"]
        .as_array()
        .map(|artifacts| {
            artifacts
                .iter()
                .map(|artifact| text(&artifact["parts"]))
                .collect()
        })
        .unwrap_or_default()
}
fn apply(answer: &mut String, event: &Value) {
    let artifact = &event["artifactUpdate"];
    if artifact.is_object() {
        assert_eq!(artifact["artifact"]["artifactId"], "answer");
        if artifact["append"] != true {
            answer.clear();
        }
        answer.push_str(&text(&artifact["artifact"]["parts"]));
    }
}
pub(super) async fn until_artifact(frames: &mut Frames, answer: &mut String) -> Result<Value> {
    loop {
        let event = frames.event().await?.context("closed before artifact")?;
        apply(answer, &event);
        if event["artifactUpdate"].is_object() {
            return Ok(event);
        }
    }
}
pub(super) async fn finish(frames: &mut Frames, answer: &mut String) -> Result<()> {
    let mut last_chunk = false;
    let mut terminal = false;
    while let Some(event) = frames.event().await? {
        assert!(!terminal, "event after terminal status");
        apply(answer, &event);
        last_chunk |= event["artifactUpdate"]["lastChunk"] == true;
        if event["statusUpdate"].is_object() {
            assert_eq!(
                event["statusUpdate"]["status"]["state"],
                "TASK_STATE_COMPLETED"
            );
            assert_eq!(
                text(&event["statusUpdate"]["status"]["message"]["parts"]),
                "Hello world"
            );
            terminal = true;
            assert!(last_chunk, "terminal status before final artifact");
        }
    }
    assert!(terminal, "closed without terminal status");
    assert_eq!(answer, "Hello world");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_send_snapshot_artifacts_terminal_close_and_dedupe() -> Result<()> {
    let http = Http::start().await?;
    let request = json!({"message":message("stream", None)});
    let mut frames = Frames::open(&http, "SendStreamingMessage", request.clone()).await?;
    let first = frames.event().await?.context("missing snapshot")?;
    assert_eq!(first["task"]["status"]["state"], "TASK_STATE_WORKING");
    let id = first["task"]["id"].clone();
    let mut answer = snapshot_text(&first["task"]);
    let artifact = until_artifact(&mut frames, &mut answer).await?;
    assert_ne!(artifact["artifactUpdate"]["append"], true);
    let fixture: Value = serde_json::from_str(
        include_str!("../fixtures/sse_stream.txt")
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .nth(1)
            .unwrap(),
    )?;
    assert_eq!(
        artifact["artifactUpdate"]["artifact"]["parts"],
        fixture["result"]["artifactUpdate"]["artifact"]["parts"]
    );
    assert_eq!(answer, "Hello ");
    let mut retry = Frames::open(&http, "SendStreamingMessage", request.clone()).await?;
    let retry_snapshot = retry.event().await?.unwrap();
    assert_eq!(retry_snapshot["task"]["id"], id);
    let mut retry_answer = snapshot_text(&retry_snapshot["task"]);
    assert_eq!(retry_answer, "Hello ");
    http.h.llm.release.notify_one();
    finish(&mut frames, &mut answer).await?;
    finish(&mut retry, &mut retry_answer).await?;
    let mut terminal_retry = Frames::open(&http, "SendStreamingMessage", request).await?;
    let terminal = terminal_retry.event().await?.unwrap();
    assert_eq!(terminal["task"]["id"], id);
    assert_eq!(terminal["task"]["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(snapshot_text(&terminal["task"]), "Hello world");
    assert!(terminal_retry.event().await?.is_none());
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_disconnect_resubscribe_no_duplicate_chunks_and_ownership() -> Result<()> {
    let http = Http::start().await?;
    let mut frames = Frames::open(
        &http,
        "SendStreamingMessage",
        json!({"message":message("disconnect", None)}),
    )
    .await?;
    let snapshot = frames.event().await?.unwrap();
    let id = snapshot["task"]["id"].clone();
    let mut answer = snapshot_text(&snapshot["task"]);
    until_artifact(&mut frames, &mut answer).await?;
    drop(frames);
    let foreign = http
        .rpc_call(HttpRpcCall::new(
            &http.url,
            "bob",
            "SubscribeToTask",
            json!({"id":id}),
        ))
        .await?;
    assert_eq!(foreign["error"]["code"], -32001);
    let cross_export = http
        .rpc_call(HttpRpcCall::new(
            &http.url.replace("runner", "other"),
            "alice",
            "SubscribeToTask",
            json!({"id":id}),
        ))
        .await?;
    assert_eq!(cross_export["error"]["code"], -32001);
    let mut resumed = Frames::open(&http, "SubscribeToTask", json!({"id":id})).await?;
    let snapshot = resumed.event().await?.unwrap();
    assert_eq!(snapshot["task"]["id"], id);
    answer = snapshot_text(&snapshot["task"]);
    assert_eq!(answer, "Hello ");
    http.h.llm.release.notify_one();
    finish(&mut resumed, &mut answer).await?;
    let completed = http.completed(&id).await?;
    assert_eq!(answer, snapshot_text(&completed["result"]));
    let terminal = http.rpc("SubscribeToTask", json!({"id":id})).await?;
    assert_eq!(terminal["error"]["code"], -32004);
    Ok(())
}

#[tokio::test]
async fn streaming_keep_alive_comment_without_wall_clock_sleep() -> Result<()> {
    // Worker uses block_in_place, while the HTTP server's clock must be pausable.
    struct WorkerRuntime(Option<tokio::runtime::Runtime>);
    impl Drop for WorkerRuntime {
        fn drop(&mut self) {
            self.0.take().unwrap().shutdown_background();
        }
    }
    let worker = WorkerRuntime(Some(tokio::runtime::Runtime::new()?));
    let harness = worker
        .0
        .as_ref()
        .unwrap()
        .spawn(crate::support::Harness::start(crate::support::Script::Text))
        .await??;
    let http = Http::from_harness(harness).await?;
    let mut frames = Frames::open(
        &http,
        "SendStreamingMessage",
        json!({"message":message("keepalive", None)}),
    )
    .await?;
    frames.event().await?.unwrap();
    let mut answer = String::new();
    until_artifact(&mut frames, &mut answer).await?;
    // Pause only after real broker/worker admission and first token. Advance the
    // upstream 15s KeepAlive timer, then resume before polling network IO.
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(15)).await;
    tokio::time::resume();
    let comment = frames.frame().await?.context("missing keep-alive")?;
    assert!(
        comment
            .lines()
            .all(|line| line.is_empty() || line.starts_with(':')),
        "{comment}"
    );
    http.h.llm.release.notify_one();
    finish(&mut frames, &mut answer).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_disconnect_without_subscribers_finishes_task() -> Result<()> {
    let http = Http::start().await?;
    let mut frames = Frames::open(
        &http,
        "SendStreamingMessage",
        json!({"message":message("alone", None)}),
    )
    .await?;
    let snapshot = frames.event().await?.unwrap();
    let mut answer = String::new();
    until_artifact(&mut frames, &mut answer).await?;
    drop(frames);
    http.h.llm.release.notify_one();
    let completed = http.completed(&snapshot["task"]["id"]).await?;
    assert_eq!(snapshot_text(&completed["result"]), "Hello world");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_subscribe_authorizes_before_orphan_reconciliation() -> Result<()> {
    let http = Http::start().await?;
    let session = http.h.session(None, &crate::support::alice()).await?;
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
    let id = &record.task.id;
    let foreign = http
        .rpc_call(HttpRpcCall::new(
            &http.url,
            "bob",
            "SubscribeToTask",
            json!({"id":id}),
        ))
        .await?;
    assert_eq!(foreign["error"]["code"], -32001);
    assert_eq!(http.h.task(id).await?.revision, record.revision);
    let orphan = http.rpc("SubscribeToTask", json!({"id":id})).await?;
    assert_eq!(orphan["error"]["code"], -32004);
    assert_eq!(
        http.h.task(id).await?.task.status.state,
        a2a_lf::TaskState::Failed
    );
    let mut frames = Frames::open(
        &http,
        "SendStreamingMessage",
        json!({"message":message("after-orphan", Some(&json!(session.session_id())))}),
    )
    .await?;
    frames.event().await?.unwrap();
    let mut answer = String::new();
    until_artifact(&mut frames, &mut answer).await?;
    http.h.llm.release.notify_one();
    finish(&mut frames, &mut answer).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_failed_turn_emits_terminal_status_and_closes() -> Result<()> {
    let harness = crate::support::Harness::start(crate::support::Script::Fail).await?;
    let http = Http::from_harness(harness).await?;
    let mut frames = Frames::open(
        &http,
        "SendStreamingMessage",
        json!({"message":message("fail", None)}),
    )
    .await?;
    assert_eq!(
        frames.event().await?.unwrap()["task"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    let mut terminal = false;
    while let Some(event) = frames.event().await? {
        assert!(!terminal);
        if event["statusUpdate"].is_object() {
            assert_eq!(
                event["statusUpdate"]["status"]["state"],
                "TASK_STATE_FAILED"
            );
            assert_eq!(
                text(&event["statusUpdate"]["status"]["message"]["parts"]),
                "agent turn failed"
            );
            terminal = true;
        }
    }
    assert!(terminal);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_send_reuses_admission_checks_and_cancel_closes() -> Result<()> {
    let http = Http::start().await?;
    let mut frames = Frames::open(
        &http,
        "SendStreamingMessage",
        json!({"message":message("checks", None)}),
    )
    .await?;
    let snapshot = frames.event().await?.unwrap();
    let task = &snapshot["task"];
    let mut answer = String::new();
    until_artifact(&mut frames, &mut answer).await?;
    let mut mismatch = message("mismatch", Some(&json!("wrong")));
    mismatch["taskId"] = task["id"].clone();
    let mut changed = message("checks", Some(&task["contextId"]));
    changed["parts"] = json!([{"text":"changed"}]);
    for (request, expected) in [
        (message("unknown", Some(&json!("unknown"))), -32001),
        (mismatch, -32602),
        (changed, -32602),
        (message("busy", Some(&task["contextId"])), -32000),
    ] {
        let error = http
            .rpc("SendStreamingMessage", json!({"message":request}))
            .await?;
        assert_eq!(error["error"]["code"], expected, "{error}");
    }
    let foreign = http
        .rpc_call(HttpRpcCall::new(
            &http.url,
            "bob",
            "SendStreamingMessage",
            json!({"message":message("checks", Some(&task["contextId"]))}),
        ))
        .await?;
    assert_eq!(foreign["error"]["code"], -32001);
    let mut retry = message("checks", Some(&task["contextId"]));
    retry["taskId"] = task["id"].clone();
    let mut retried = Frames::open(&http, "SendStreamingMessage", json!({"message":retry})).await?;
    assert_eq!(retried.event().await?.unwrap()["task"]["id"], task["id"]);
    drop(retried);
    let cancel = http.rpc("CancelTask", json!({"id":task["id"]})).await?;
    assert_eq!(cancel["result"]["status"]["state"], "TASK_STATE_CANCELED");
    let mut terminal = false;
    while let Some(event) = frames.event().await? {
        assert!(!terminal);
        if event["statusUpdate"].is_object() {
            assert_eq!(
                event["statusUpdate"]["status"]["state"],
                "TASK_STATE_CANCELED"
            );
            terminal = true;
        }
    }
    assert!(terminal);
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compat_message_stream_resubscribe_and_cancel_aliases() -> Result<()> {
    let http = Http::start().await?;
    let mut msg = message("compat-stream", None);
    msg["role"] = json!("user");
    // Missing A2A-Version is intentional; all other stream tests stay explicit.
    let mut frames =
        Frames::open_version(&http, "message/stream", json!({"message":msg}), None).await?;
    let first = frames.event().await?.context("missing alias snapshot")?;
    assert_eq!(first["task"]["status"]["state"], "TASK_STATE_WORKING");
    assert_eq!(first["task"]["history"][0]["role"], "ROLE_USER");
    let id = first["task"]["id"].clone();
    let mut answer = snapshot_text(&first["task"]);
    until_artifact(&mut frames, &mut answer).await?;
    let mut subscription =
        Frames::open_version(&http, "tasks/resubscribe", json!({"id":id}), Some("0.3")).await?;
    assert_eq!(subscription.event().await?.unwrap()["task"]["id"], id);
    drop(subscription);
    http.h.llm.release.notify_one();
    finish(&mut frames, &mut answer).await?;
    let get = http.rpc("tasks/get", json!({"id":id})).await?;
    assert_eq!(get["result"]["status"]["state"], "TASK_STATE_COMPLETED");

    let mut frames = Frames::open(
        &http,
        "message/stream",
        json!({"message":message("compat-cancel",None)}),
    )
    .await?;
    let id = frames.event().await?.unwrap()["task"]["id"].clone();
    let canceled = http.rpc("tasks/cancel", json!({"id":id})).await?;
    assert_eq!(
        canceled["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{canceled}"
    );
    while let Some(event) = frames.event().await? {
        if event["statusUpdate"].is_object() {
            assert_eq!(
                event["statusUpdate"]["status"]["state"],
                "TASK_STATE_CANCELED"
            );
        }
    }
    Ok(())
}
