use super::fixtures::*;
use crate::support::DEADLINE;
use anyhow::{Context, Result};
use serde_json::{json, Value};

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

async fn setup_busy_and_complete(http: &Http) -> Result<Value> {
    let first = http.working(message("dedupe", None)).await?;
    let retry = http.working(message("dedupe", None)).await?;
    assert_eq!(first["id"], retry["id"]);
    let busy = http
        .rpc(
            "SendMessage",
            json!({"message": message("busy", Some(&first["contextId"]))}),
        )
        .await?;
    code(&busy, -32000);
    assert!(busy["error"]["message"]
        .as_str()
        .unwrap()
        .contains("retry later"));
    http.h.llm.release.notify_one();
    http.completed(&first["id"]).await?;
    Ok(first)
}

async fn assert_terminal_dedupe_and_mismatch(http: &Http, first: &Value) -> Result<()> {
    for context in [None, Some(&first["contextId"])] {
        let retry = http
            .rpc(
                "SendMessage",
                json!({"message": message("dedupe", context)}),
            )
            .await?;
        assert_eq!(retry["result"]["task"]["id"], first["id"]);
        let mut changed = message("dedupe", context);
        changed["parts"][0]["text"] = json!("changed");
        let mismatch = http.rpc("SendMessage", json!({"message": changed})).await?;
        golden(&mismatch, &fixture("error_invalid_params.json"), "");
    }
    Ok(())
}

async fn assert_terminal_task_id_retries(http: &Http, first: &Value) -> Result<()> {
    for context in [None, Some(&first["contextId"])] {
        let mut retry = message("dedupe", context);
        retry["taskId"] = first["id"].clone();
        let response = http.rpc("SendMessage", json!({"message": retry})).await?;
        assert_eq!(response["result"]["task"]["id"], first["id"]);
        assert_eq!(
            response["result"]["task"]["status"]["state"],
            "TASK_STATE_COMPLETED"
        );
    }
    Ok(())
}

async fn assert_terminal_cancel_fails(http: &Http, first: &Value) -> Result<()> {
    let cancel = http.rpc("CancelTask", json!({"id": first["id"]})).await?;
    code(&cancel, -32002);
    assert_eq!(
        cancel["error"]["message"],
        a2a_lf::A2AError::task_not_cancelable(first["id"].as_str().unwrap()).message
    );
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_dedupe_busy_terminal_and_fingerprint_mismatch() -> Result<()> {
    let http = Http::start().await?;
    let first = setup_busy_and_complete(&http).await?;
    assert_terminal_dedupe_and_mismatch(&http, &first).await?;
    assert_terminal_task_id_retries(&http, &first).await?;
    assert_terminal_cancel_fails(&http, &first).await?;
    Ok(())
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
