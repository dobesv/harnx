use crate::support::{Harness, Script, DEADLINE};
use anyhow::{Context, Result};
use harnx_a2a_server::store::TaskSeed;
use harnx_a2a_server::{
    handler::{Backend, BackendConfig, HarnxHandler},
    input_map::InputLimits,
    routes,
};
use harnx_runtime::SessionActivationRoute;
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) struct HttpRpcCall<'a> {
    pub(super) path: &'a str,
    pub(super) user: &'a str,
    pub(super) method: &'a str,
    pub(super) params: Value,
}

impl<'a> HttpRpcCall<'a> {
    pub(super) fn new(path: &'a str, user: &'a str, method: &'a str, params: Value) -> Self {
        Self {
            path,
            user,
            method,
            params,
        }
    }
}

pub(super) struct Http {
    pub(super) h: Harness,
    pub(super) url: String,
    pub(super) client: reqwest::Client,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Http {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Http {
    pub(super) async fn start() -> Result<Self> {
        Self::from_harness(Harness::start(Script::Text).await?).await
    }
    pub(super) async fn from_harness(h: Harness) -> Result<Self> {
        Self::with_access_rules(h, None).await
    }
    pub(super) async fn with_access_rules(
        h: Harness,
        rules: Option<Arc<harnx_core::access_rules::AccessRules>>,
    ) -> Result<Self> {
        let backend = Arc::new(Backend::new(
            h.runner.clone(),
            h.store.clone(),
            BackendConfig {
                config: h.config.clone(),
                route: SessionActivationRoute::ClusterShared,
                abort: harnx_core::abort::create_abort_signal(),
            },
        ));
        let mut exports = vec![h.export.clone(), h.export.clone()];
        exports[0].lookup_keys = vec![
            "runner".into(),
            "alias".into(),
            h.export.public_name.clone(),
        ];
        exports[1].public_name = "other".into();
        exports[1].agent = "target".into();
        exports[1].lookup_keys = vec!["other".into()];
        let app = routes::router_with_access_rules(
            &exports,
            None,
            &["X-User-ID".into()],
            rules,
            |export, identity| {
                Arc::new(HarnxHandler::new(
                    export.clone(),
                    identity,
                    backend.clone(),
                    InputLimits::default(),
                ))
            },
        )?;
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
    pub(super) async fn rpc_call(&self, call: HttpRpcCall<'_>) -> Result<Value> {
        let response = self
            .client
            .post(call.path)
            .header("a2a-version", "1.0")
            .header("X-User-ID", call.user)
            .json(&json!({"jsonrpc":"2.0","id":"1","method":call.method,"params":call.params}))
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        Ok(response.json().await?)
    }
    pub(super) async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        self.rpc_call(HttpRpcCall::new(&self.url, "alice", method, params))
            .await
    }
    async fn working(&self, message: Value) -> Result<Value> {
        let response = self
            .rpc(
                "SendMessage",
                json!({"message":message,"configuration":{"returnImmediately":true}}),
            )
            .await?;
        assert_eq!(
            response["result"]["task"]["status"]["state"], "TASK_STATE_WORKING",
            "{response}"
        );
        Ok(response["result"]["task"].clone())
    }
    pub(super) async fn completed(&self, id: &Value) -> Result<Value> {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let response = self.rpc("GetTask", json!({"id":id})).await?;
                if response["result"]["status"]["state"] == "TASK_STATE_COMPLETED" {
                    return Ok::<_, anyhow::Error>(response);
                }
                anyhow::ensure!(
                    response["error"].is_null()
                        && response["result"]["status"]["state"] == "TASK_STATE_WORKING",
                    "unexpected state: {response}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .context("completion deadline")?
    }
}
pub(super) fn message(id: &str, context: Option<&Value>) -> Value {
    let mut message = json!({"messageId":id,"role":"ROLE_USER","parts":[{"text":"Hello, agent"}]});
    if let Some(context) = context {
        message["contextId"] = context.clone();
    }
    message
}
fn code(response: &Value, expected: i32) {
    assert_eq!(response["error"]["code"], expected, "{response}");
    assert!(response.get("result").is_none());
}
fn fixture(name: &str) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
    .unwrap()
}
// Compare the golden wire projection. IDs/timestamps and answer text are
// runtime-generated; optional artifacts/history extend the minimal fixtures.
fn golden(actual: &Value, expected: &Value, key: &str) {
    match expected {
        Value::Object(fields) => {
            for (key, value) in fields {
                assert!(actual.get(key).is_some(), "missing {key}: {actual}");
                golden(&actual[key], value, key);
            }
        }
        Value::Array(values) => {
            assert_eq!(actual.as_array().unwrap().len(), values.len());
            for (a, b) in actual.as_array().unwrap().iter().zip(values) {
                golden(a, b, key);
            }
        }
        Value::String(_)
            if matches!(
                key,
                "id" | "contextId" | "messageId" | "timestamp" | "text" | "message"
            ) =>
        {
            assert!(actual.is_string())
        }
        _ => assert_eq!(actual, expected),
    }
}

async fn persist_canonical_completed_task(http: &Http) -> Result<String> {
    let session = http.h.session(None, &crate::support::alice()).await?;
    let id = harnx_a2a_server::store::format_task_id(
        session.session_id(),
        "01234567-89ab-cdef-0123-456789abcdef",
    );
    let task = serde_json::from_value(json!({
        "id":id, "contextId":session.session_id(),
        "status":{"state":"TASK_STATE_COMPLETED"}
    }))?;
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
    Ok(id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noncanonical_task_ids_return_not_found_for_all_task_methods() -> Result<()> {
    let http = Http::start().await?;
    let id = persist_canonical_completed_task(&http).await?;
    let existing = http.rpc("GetTask", json!({"id":id})).await?;
    assert_eq!(existing["result"]["id"], id);
    let (context, uuid) = harnx_a2a_server::store::parse_task_id(&id)?;
    for noncanonical in [
        uuid.to_ascii_uppercase(),
        uuid.replace('-', ""),
        format!("{{{uuid}}}"),
        format!("urn:uuid:{uuid}"),
    ] {
        let malformed = harnx_a2a_server::store::format_task_id(context, &noncanonical);
        for method in ["GetTask", "CancelTask", "SubscribeToTask"] {
            code(&http.rpc(method, json!({"id":malformed})).await?, -32001);
        }
        let mut send = message("noncanonical-task-id", None);
        send["taskId"] = json!(malformed);
        for method in ["SendMessage", "SendStreamingMessage"] {
            code(&http.rpc(method, json!({"message":send})).await?, -32001);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_blocking_send_matches_completed_get_fixture() -> Result<()> {
    let http = Http::start().await?;
    let send = http.rpc("SendMessage", json!({"message":message("blocking", None)}));
    tokio::pin!(send);
    tokio::select! {
        result = &mut send => anyhow::bail!("blocking send returned before release: {result:?}"),
        result = tokio::time::timeout(DEADLINE, http.h.llm.requested.notified()) => { result?; },
    }
    http.h.llm.release.notify_one();
    let response = send.await?;
    assert_eq!(
        response["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{response}"
    );
    assert_eq!(response["result"].as_object().unwrap().len(), 1);
    let get = http
        .rpc("GetTask", json!({"id":response["result"]["task"]["id"]}))
        .await?;
    golden(&get, &fixture("get_task_result.json"), "");
    assert_eq!(
        get["result"]["artifacts"][0]["parts"][0]["text"],
        "Hello world"
    );
    assert_eq!(
        get["result"]["status"]["message"]["parts"][0]["text"],
        "Hello world"
    );
    assert!(get.to_string().find("\"kind\"").is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_raw_media_type_returns_content_type_not_supported() -> Result<()> {
    let http = Http::start().await?;
    let response = http
        .rpc(
            "SendMessage",
            json!({
                "message": {
                    "messageId": "unsupported-media",
                    "role": "ROLE_USER",
                    "parts": [{"raw": "AA==", "mediaType": "application/x-unsupported"}]
                }
            }),
        )
        .await?;
    code(&response, -32005);
    assert_eq!(
        response["error"]["message"],
        "unsupported raw mediaType: application/x-unsupported"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_data_returns_same_invalid_params_error() -> Result<()> {
    let http = Http::start().await?;
    let mut message = message("oversized-data", None);
    message["parts"] = json!([{"data": {"value": "x".repeat(65536)}}]);
    let response = http.rpc("SendMessage", json!({"message": message})).await?;
    code(&response, -32602);
    assert_eq!(
        response["error"]["message"],
        "unsupported or oversized message parts"
    );
    assert!(http.h.llm.requests.lock().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_first_turn_parts_never_create_session_metadata_or_binding() -> Result<()> {
    use futures::TryStreamExt;

    async fn session_keys(http: &Http) -> Result<std::collections::BTreeSet<String>> {
        Ok(http
            .h
            .metadata
            .kv_store()
            .keys()
            .await?
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .filter(|key| key.starts_with("sessions/"))
            .collect())
    }

    let http = Http::start().await?;
    let before = session_keys(&http).await?;
    assert!(
        before.is_empty(),
        "unexpected initial session keys: {before:?}"
    );
    for method in ["SendMessage", "SendStreamingMessage"] {
        for (name, parts, expected) in [
            (
                "oversized-data",
                json!([{"data": {"value": "x".repeat(InputLimits::default().max_data_part_bytes)}}]),
                -32602,
            ),
            (
                "unsupported-media",
                json!([{"raw": "AA==", "mediaType": "application/x-unsupported"}]),
                -32005,
            ),
            (
                "invalid-utf8",
                json!([{"raw": "/w==", "mediaType": "text/plain"}]),
                -32602,
            ),
        ] {
            let mut message = message(&format!("{method}-{name}"), None);
            message["parts"] = parts;
            let response = http.rpc(method, json!({"message": message})).await?;
            code(&response, expected);
            // The A2A binding is stored in sessions/*/meta. Check the whole
            // session prefix so a partial allocation cannot pass this assertion.
            assert_eq!(
                session_keys(&http).await?,
                before,
                "{method} {name} left session keys after rejection"
            );
        }
    }
    assert!(http.h.llm.requests.lock().is_empty());
    Ok(())
}

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

async fn assert_foreign_context_rejections(http: &Http, first: &Value) -> Result<()> {
    for (method, params) in [
        (
            "SendMessage",
            json!({"message":message("new", Some(&first["contextId"]))}),
        ),
        ("GetTask", json!({"id":first["id"]})),
        ("CancelTask", json!({"id":first["id"]})),
        ("ListTasks", json!({"contextId":first["contextId"]})),
    ] {
        code(
            &http
                .rpc_call(HttpRpcCall::new(&http.url, "bob", method, params.clone()))
                .await?,
            -32001,
        );
        code(
            &http
                .rpc_call(HttpRpcCall::new(
                    &http.url.replace("/runner", "/other"),
                    "alice",
                    method,
                    params,
                ))
                .await?,
            -32001,
        );
    }
    Ok(())
}

async fn assert_unknown_context_isolation(http: &Http, first_context: &str) -> Result<()> {
    let unknown = http
        .rpc(
            "SendMessage",
            json!({"message":message("unknown", Some(&json!("unknown")))}),
        )
        .await?;
    golden(&unknown, &fixture("error_task_not_found.json"), "");
    assert!(http
        .h
        .metadata
        .get_for_agent("unknown", "runner")
        .await?
        .is_none());
    assert!(http
        .h
        .metadata
        .get_for_agent(first_context, "target")
        .await?
        .is_none());
    Ok(())
}

async fn assert_task_id_resolution_and_validation(http: &Http, first: &Value) -> Result<()> {
    let mut mismatch = message("mismatch", Some(&json!("unknown")));
    mismatch["taskId"] = first["id"].clone();
    code(
        &http.rpc("SendMessage", json!({"message":mismatch})).await?,
        -32602,
    );
    let mut task_only = message("owner", None);
    task_only["taskId"] = first["id"].clone();
    let retry = http
        .rpc(
            "SendMessage",
            json!({"message":task_only,"configuration":{"returnImmediately":true}}),
        )
        .await?;
    assert_eq!(retry["result"]["task"]["id"], first["id"]);
    let mut unknown_task = message("unknown-task", None);
    unknown_task["taskId"] = json!(format!(
        "{}.{}",
        first["contextId"].as_str().unwrap(),
        uuid::Uuid::new_v4()
    ));
    code(
        &http
            .rpc("SendMessage", json!({"message":unknown_task}))
            .await?,
        -32001,
    );
    Ok(())
}

async fn assert_terminal_task_continuation_rejected(http: &Http, first_id: &Value) -> Result<()> {
    http.h.llm.release.notify_one();
    http.completed(first_id).await?;
    let mut terminal = message("new-on-terminal", None);
    terminal["taskId"] = first_id.clone();
    code(
        &http.rpc("SendMessage", json!({"message":terminal})).await?,
        -32602,
    );
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_unknown_foreign_contexts_and_task_resolution_never_create() -> Result<()> {
    let http = Http::start().await?;
    let first = http.working(message("owner", None)).await?;
    assert_foreign_context_rejections(&http, &first).await?;
    assert_unknown_context_isolation(&http, first["contextId"].as_str().unwrap()).await?;
    assert_task_id_resolution_and_validation(&http, &first).await?;
    assert_terminal_task_continuation_rejected(&http, &first["id"]).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_dedupe_busy_terminal_and_fingerprint_mismatch() -> Result<()> {
    let http = Http::start().await?;
    let first = http.working(message("dedupe", None)).await?;
    let retry = http.working(message("dedupe", None)).await?;
    assert_eq!(first["id"], retry["id"]);
    let busy = http
        .rpc(
            "SendMessage",
            json!({"message":message("busy", Some(&first["contextId"]))}),
        )
        .await?;
    code(&busy, -32000);
    assert!(busy["error"]["message"]
        .as_str()
        .unwrap()
        .contains("retry later"));
    http.h.llm.release.notify_one();
    http.completed(&first["id"]).await?;
    for context in [None, Some(&first["contextId"])] {
        let retry = http
            .rpc("SendMessage", json!({"message":message("dedupe", context)}))
            .await?;
        assert_eq!(retry["result"]["task"]["id"], first["id"]);
        let mut changed = message("dedupe", context);
        changed["parts"][0]["text"] = json!("changed");
        let mismatch = http.rpc("SendMessage", json!({"message":changed})).await?;
        golden(&mismatch, &fixture("error_invalid_params.json"), "");
    }
    for context in [None, Some(&first["contextId"])] {
        let mut retry = message("dedupe", context);
        retry["taskId"] = first["id"].clone();
        let response = http.rpc("SendMessage", json!({"message":retry})).await?;
        assert_eq!(response["result"]["task"]["id"], first["id"]);
        assert_eq!(
            response["result"]["task"]["status"]["state"],
            "TASK_STATE_COMPLETED"
        );
    }
    let cancel = http.rpc("CancelTask", json!({"id":first["id"]})).await?;
    code(&cancel, -32002);
    assert_eq!(
        cancel["error"]["message"],
        a2a_lf::A2AError::task_not_cancelable(first["id"].as_str().unwrap()).message
    );
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

async fn spawn_strict_limits_server(
    http: &Http,
) -> Result<(String, tokio_util::task::AbortOnDropHandle<()>)> {
    let backend = Arc::new(Backend::new(
        http.h.runner.clone(),
        http.h.store.clone(),
        BackendConfig {
            config: http.h.config.clone(),
            route: SessionActivationRoute::ClusterShared,
            abort: harnx_core::abort::create_abort_signal(),
        },
    ));
    let mut export = http.h.export.clone();
    export.lookup_keys = vec!["runner".into()];
    let app = routes::router(
        &[export],
        None,
        &["X-User-ID".into()],
        |export, identity| {
            Arc::new(HarnxHandler::new(
                export.clone(),
                identity,
                backend.clone(),
                InputLimits {
                    max_data_part_bytes: 0,
                },
            ))
        },
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/agents/runner", listener.local_addr()?);
    let handle = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    Ok((url, handle))
}

async fn assert_retry_response(
    http: &Http,
    call: HttpRpcCall<'_>,
    expected_id: &Value,
) -> Result<()> {
    let response = http
        .client
        .post(call.path)
        .header("a2a-version", "1.0")
        .header("X-User-ID", "alice")
        .json(&json!({"jsonrpc":"2.0","id":"stream-request","method":call.method,"params":call.params}))
        .send()
        .await?;
    let result = if call.method == "SendStreamingMessage" {
        let mut frames = crate::streaming::Frames::from_response(response)?;
        let result = frames.event().await?.context("missing deduped snapshot")?;
        assert!(frames.event().await?.is_none());
        result
    } else {
        let envelope: Value = response.json().await?;
        assert!(envelope["error"].is_null(), "{envelope}");
        envelope["result"].clone()
    };
    assert_eq!(result["task"]["id"], *expected_id);
    assert_eq!(result["task"]["status"]["state"], "TASK_STATE_COMPLETED");
    Ok(())
}

async fn assert_strict_limit_retries_and_new_rejections(
    http: &Http,
    url: &str,
    accepted: &Value,
    first: &Value,
) -> Result<()> {
    let retry_permutations = [(false, false), (false, true), (true, false), (true, true)];
    for method in ["SendMessage", "SendStreamingMessage"] {
        for (with_context, with_task) in retry_permutations {
            let mut retry = accepted.clone();
            if with_context {
                retry["contextId"] = first["contextId"].clone();
            }
            if with_task {
                retry["taskId"] = first["id"].clone();
            }
            assert_retry_response(
                http,
                HttpRpcCall::new(url, "alice", method, json!({"message":retry})),
                &first["id"],
            )
            .await?;
        }
        let mut new = accepted.clone();
        new["messageId"] = json!(format!("{method}-new-data"));
        code(
            &http
                .rpc_call(HttpRpcCall::new(
                    url,
                    "alice",
                    method,
                    json!({"message":new}),
                ))
                .await?,
            -32602,
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_message_retries_bypass_stricter_input_limits() -> Result<()> {
    let http = Http::start().await?;
    let mut accepted = message("data-dedupe", None);
    accepted["parts"] = json!([{"data": {"issue": "HARNX-2315"}}]);
    let first = http.working(accepted.clone()).await?;
    http.h.llm.release.notify_one();
    http.completed(&first["id"]).await?;

    // A changed limit must affect new admissions, not already-accepted IDs.
    let (url, _server) = spawn_strict_limits_server(&http).await?;
    assert_strict_limit_retries_and_new_rejections(&http, &url, &accepted, &first).await?;
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_parts_do_not_override_owner_busy_or_terminal_errors() -> Result<()> {
    let http = Http::start().await?;
    let first = http.working(message("validation-order", None)).await?;
    let mut invalid = message("invalid-new", Some(&first["contextId"]));
    invalid["parts"] = json!([{"raw": "AA==", "mediaType": "application/x-unsupported"}]);
    for method in ["SendMessage", "SendStreamingMessage"] {
        code(
            &http
                .rpc_call(HttpRpcCall::new(
                    &http.url,
                    "bob",
                    method,
                    json!({"message": invalid}),
                ))
                .await?,
            -32001,
        );
        code(&http.rpc(method, json!({"message":invalid})).await?, -32000);
        let mut mismatch = invalid.clone();
        mismatch["messageId"] = json!("validation-order");
        let response = http.rpc(method, json!({"message":mismatch})).await?;
        code(&response, -32602);
        assert_eq!(
            response["error"]["message"],
            "messageId was already used with different parts"
        );
    }
    http.h.llm.release.notify_one();
    http.completed(&first["id"]).await?;
    invalid["taskId"] = first["id"].clone();
    for method in ["SendMessage", "SendStreamingMessage"] {
        let response = http.rpc(method, json!({"message":invalid})).await?;
        code(&response, -32602);
        assert_eq!(
            response["error"]["message"],
            "terminal tasks cannot be continued; send a new message with contextId"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_cancel_live_and_optional_methods() -> Result<()> {
    let http = Http::start().await?;
    let first = http.working(message("cancel", None)).await?;
    let cancel = http.rpc("CancelTask", json!({"id":first["id"]})).await?;
    assert_eq!(
        cancel["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{cancel}"
    );
    let cancel = http.rpc("CancelTask", json!({"id":first["id"]})).await?;
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
    for (method, params) in [
        (
            "CreateTaskPushNotificationConfig",
            json!({"taskId":first["id"],"pushNotificationConfig":{"url":"https://example.com/push"}}),
        ),
        (
            "GetTaskPushNotificationConfig",
            json!({"taskId":first["id"],"id":"config"}),
        ),
        (
            "ListTaskPushNotificationConfigs",
            json!({"taskId":first["id"]}),
        ),
        (
            "DeleteTaskPushNotificationConfig",
            json!({"taskId":first["id"],"id":"config"}),
        ),
    ] {
        let response = http.rpc(method, params).await?;
        golden(&response, &fixture("error_push_not_supported.json"), "");
    }
    code(&http.rpc("GetExtendedAgentCard", json!({})).await?, -32004);
    code(
        &http
            .rpc("SubscribeToTask", json!({"id":first["id"]}))
            .await?,
        -32004,
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_concurrent_new_context_retries_and_disconnect() -> Result<()> {
    let http = Http::start().await?;
    let (one, two) = tokio::join!(
        http.working(message("race", None)),
        http.working(message("race", None))
    );
    let one = one?;
    assert_eq!(one["id"], two?["id"]);
    http.h.llm.release.notify_one();
    http.completed(&one["id"]).await?;
    // Drop a real HTTP request after the model sees its admitted prompt.
    let client = http.client.clone();
    let url = http.url.clone();
    let context = one["contextId"].clone();
    let request = tokio::spawn(async move {
        client.post(url).header("a2a-version", "1.0").header("X-User-ID", "alice").json(&json!({"jsonrpc":"2.0","id":"1","method":"SendMessage","params":{"message":message("disconnect",Some(&context))}})).send().await
    });
    tokio::time::timeout(DEADLINE, async {
        while http.h.llm.requests.lock().len() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    request.abort();
    let list = http
        .rpc("ListTasks", json!({"contextId":one["contextId"]}))
        .await?;
    let task = list["result"]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] != one["id"])
        .unwrap();
    http.h.llm.release.notify_one();
    http.completed(&task["id"]).await?;
    let retry = http
        .rpc(
            "SendMessage",
            json!({"message":message("disconnect",Some(&one["contextId"]))}),
        )
        .await?;
    assert_eq!(retry["result"]["task"]["id"], task["id"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_orphans_reconciled_before_get_cancel_and_new_send() -> Result<()> {
    let http = Http::start().await?;
    let session = http.h.session(None, &crate::support::alice()).await?;
    let context = json!(session.session_id());
    async fn orphan(http: &Http, session: &harnx_runtime::NatsSession) -> Result<String> {
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
    let get_id = orphan(&http, &session).await?;
    let get = http.rpc("GetTask", json!({"id":get_id})).await?;
    assert_eq!(get["result"]["status"]["state"], "TASK_STATE_FAILED");
    assert_eq!(
        get["result"]["status"]["message"]["parts"][0]["text"],
        "interrupted by server restart"
    );
    let cancel_id = orphan(&http, &session).await?;
    code(
        &http.rpc("CancelTask", json!({"id":cancel_id})).await?,
        -32002,
    );
    assert_eq!(
        http.h.task(&cancel_id).await?.task.status.state,
        a2a_lf::TaskState::Failed
    );
    let send_id = orphan(&http, &session).await?;
    let started = http
        .working(message("after-restart", Some(&context)))
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
