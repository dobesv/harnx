use crate::{
    streaming::Frames,
    support::{Logs, DEADLINE},
    unary::{Http, HttpRpcCall},
};
use anyhow::Result;
use serde_json::{json, Value};

const TARGET: &str = "harnx_a2a_server::requests";
const CALLER: &str = "ari:cloud:identity::user/private-forge-account";
const PROMPT: &str = "private request log test prompt";

fn events(logs: &Logs) -> Vec<Value> {
    logs.text()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["target"] == TARGET && event["level"] == "INFO")
        .map(|event| event["fields"].clone())
        .collect()
}

fn message(id: &str) -> Value {
    json!({"messageId":id, "role":"ROLE_USER", "parts":[{"text":PROMPT}]})
}

fn assert_private(logs: &Logs) {
    let text = logs.text();
    assert!(!text.contains(CALLER), "identity at info: {text}");
    assert!(!text.contains(PROMPT), "prompt in logs: {text}");
    assert!(
        !text.contains("Hello world"),
        "response parts in logs: {text}"
    );
}

async fn working(http: &Http, message_id: &str) -> Result<Value> {
    let result = http
        .rpc_call(HttpRpcCall::new(
            &http.url,
            CALLER,
            "SendMessage",
            json!({"message":message(message_id), "configuration":{"returnImmediately":true}}),
        ))
        .await?;
    assert_eq!(
        result["result"]["task"]["status"]["state"], "TASK_STATE_WORKING",
        "{result}"
    );
    Ok(result["result"]["task"].clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_unary_success_and_mapped_errors() -> Result<()> {
    let http = Http::start().await?;
    let task = working(&http, "logged-send").await?;
    let records = events(&http.h.logs);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["export"], http.h.export.public_name);
    assert_eq!(records[0]["method"], "SendMessage");
    assert_eq!(records[0]["task_id"], task["id"]);
    assert_eq!(records[0]["context_id"], task["contextId"]);
    assert_eq!(records[0]["message_id"], "logged-send");
    assert_eq!(records[0]["state"], "Working");
    assert!(records[0]["duration_ms"].as_f64().unwrap() >= 0.0);
    let mut busy = message("logged-busy");
    busy["contextId"] = task["contextId"].clone();
    let response = http
        .rpc_call(HttpRpcCall::new(
            &http.url,
            CALLER,
            "SendMessage",
            json!({"message":busy}),
        ))
        .await?;
    assert_eq!(response["error"]["code"], -32000);
    let unknown = http.rpc("GetTask", json!({"id":"missing-task"})).await?;
    assert_eq!(unknown["error"]["code"], -32001);
    let records = events(&http.h.logs);
    assert_eq!(records.len(), 3);
    assert_eq!(records[1]["error_code"], -32000);
    assert_eq!(
        records[1]["error_message"],
        "context already has an active task, retry later"
    );
    assert_eq!(records[2]["method"], "GetTask");
    assert_eq!(records[2]["task_id"], "missing-task");
    assert_eq!(records[2]["error_code"], -32001);
    assert_eq!(records[2]["error_message"], "task not found");
    assert!(records[2]["duration_ms"].is_number());
    assert_private(&http.h.logs);
    Ok(())
}

async fn open(http: &Http, method: &str, params: Value) -> Result<Frames> {
    let response = http
        .client
        .post(&http.url)
        .header("X-User-ID", CALLER)
        .json(&json!({"jsonrpc":"2.0", "id":"stream-request", "method":method, "params":params}))
        .send()
        .await?;
    Frames::from_response(response)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_stream_finishes_with_count_and_terminal_reason() -> Result<()> {
    let http = Http::start().await?;
    let mut frames = open(
        &http,
        "SendStreamingMessage",
        json!({"message":message("streamed-send")}),
    )
    .await?;
    let first = frames.event().await?.unwrap();
    assert!(events(&http.h.logs).is_empty(), "stream logged before end");
    http.h.llm.release.notify_one();
    let mut count = 1;
    while frames.event().await?.is_some() {
        count += 1;
    }
    let records = events(&http.h.logs);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["method"], "SendStreamingMessage");
    assert_eq!(records[0]["message_id"], "streamed-send");
    assert_eq!(records[0]["task_id"], first["task"]["id"]);
    assert_eq!(records[0]["context_id"], first["task"]["contextId"]);
    assert_eq!(records[0]["state"], "Completed");
    assert_eq!(records[0]["events_sent"], count);
    assert_eq!(records[0]["end_reason"], "terminal");
    assert!(records[0]["duration_ms"].is_number());
    assert_private(&http.h.logs);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_subscribe_client_disconnect() -> Result<()> {
    let http = Http::start().await?;
    let task = working(&http, "before-subscribe").await?;
    let mut frames = open(&http, "SubscribeToTask", json!({"id":task["id"]})).await?;
    assert!(frames.event().await?.is_some());
    drop(frames);
    let record = tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some(record) = events(&http.h.logs)
                .into_iter()
                .find(|event| event["method"] == "SubscribeToTask")
            {
                break record;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(record["events_sent"], 1);
    assert_eq!(record["end_reason"], "client_disconnect");
    assert_eq!(record["task_id"], task["id"]);
    assert_eq!(record["context_id"], task["contextId"]);
    assert_eq!(record["state"], "Working");
    assert_private(&http.h.logs);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_identity_rejection_and_discovery() -> Result<()> {
    let http = Http::start().await?;
    let rejected = http.client.post(&http.url)
        .header("Authorization", "Bearer private-token")
        .json(&json!({"jsonrpc":"2.0", "id":1, "method":"SendMessage", "params":{"message":message("rejected-send")}}))
        .send().await?;
    assert_eq!(rejected.status(), 401);
    let records = events(&http.h.logs);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["method"], "SendMessage");
    assert_eq!(records[0]["message_id"], "rejected-send");
    assert_eq!(records[0]["error_code"], -32000);
    assert!(records[0]["error_message"].is_string());
    assert_eq!(records[0]["http_status"], 401);
    for path in ["agent-card.json", "agent.json"] {
        let response = http
            .client
            .get(format!("{}/.well-known/{path}", http.url))
            .send()
            .await?;
        assert_eq!(response.status(), 200);
    }
    let records = events(&http.h.logs);
    assert_eq!(records.len(), 3);
    for record in &records[1..] {
        assert_eq!(record["method"], "AgentCard");
        assert_eq!(record["http_status"], 200);
        assert!(record["duration_ms"].is_number());
    }
    assert!(!http.h.logs.text().contains("private-token"));
    assert_private(&http.h.logs);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_reads_cancel_list_and_unsupported_methods() -> Result<()> {
    let http = Http::start().await?;
    let task = working(&http, "read-cancel").await?;
    for (method, params) in [
        ("tasks/get", json!({"id":task["id"]})),
        ("ListTasks", json!({"contextId":task["contextId"]})),
        ("CancelTask", json!({"id":task["id"]})),
        (
            "GetTaskPushNotificationConfig",
            json!({"taskId":task["id"], "id":"push"}),
        ),
        ("GetExtendedAgentCard", json!({})),
    ] {
        http.rpc_call(HttpRpcCall::new(&http.url, CALLER, method, params))
            .await?;
    }
    let records = events(&http.h.logs);
    assert_eq!(records.len(), 6);
    assert_eq!(records[1]["method"], "GetTask");
    assert_eq!(records[1]["state"], "Working");
    assert_eq!(records[2]["method"], "ListTasks");
    assert_eq!(records[2]["context_id"], task["contextId"]);
    assert_eq!(records[2]["tasks_returned"], 1);
    assert_eq!(records[3]["method"], "CancelTask");
    assert_eq!(records[3]["task_id"], task["id"]);
    assert_eq!(records[3]["state"], "Canceled");
    assert_eq!(records[4]["method"], "GetTaskPushNotificationConfig");
    assert_eq!(records[4]["task_id"], task["id"]);
    assert_eq!(records[4]["error_code"], -32003);
    assert_eq!(records[5]["method"], "GetExtendedAgentCard");
    assert_eq!(records[5]["error_code"], -32004);
    assert_private(&http.h.logs);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_rejections_redact_raw_error_values() -> Result<()> {
    let http = Http::start().await?;
    let mut invalid = message("invalid-media");
    invalid["parts"] = json!([{"raw":"AA==", "mediaType":PROMPT}]);
    let result = http.rpc("SendMessage", json!({"message":invalid})).await?;
    assert_eq!(result["error"]["code"], -32005);
    assert!(result["error"]["message"]
        .as_str()
        .unwrap()
        .contains(PROMPT));
    let rejected: Value = http
        .client
        .post(&http.url)
        .header("a2a-version", PROMPT)
        .header("X-User-ID", CALLER)
        .json(&json!({"jsonrpc":"2.0", "id":1, "method":"GetTask", "params":{"id":"version-test"}}))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(rejected["error"]["code"], -32009);
    let rejected = http
        .rpc(
            "message/stream",
            json!({"message":message("invalid-blocking"), "configuration":{"blocking":PROMPT}}),
        )
        .await?;
    assert_eq!(rejected["error"]["code"], -32602);
    let records = events(&http.h.logs);
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["error_code"], -32005);
    assert_eq!(records[0]["error_message"], "unsupported raw mediaType");
    assert_eq!(records[1]["error_code"], -32009);
    assert_eq!(records[1]["error_message"], "A2A version not supported");
    assert_eq!(records[1]["task_id"], "version-test");
    assert_eq!(records[2]["method"], "SendStreamingMessage");
    assert_eq!(records[2]["error_message"], "blocking must be a boolean");
    assert_eq!(records[2]["events_sent"], 0);
    assert_eq!(records[2]["end_reason"], "error");
    assert_private(&http.h.logs);
    Ok(())
}

mod access;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_bounds_rejected_ids_and_handles_oversized_or_unknown_calls() -> Result<()> {
    let http = Http::start().await?;
    let id = "é".repeat(512);
    for (method, params) in [
        (
            "SendMessage",
            json!({"message":{"messageId":id, "contextId":id, "taskId":id, "parts":[{"text":PROMPT}]}}),
        ),
        ("tasks/pushNotificationConfig/get", json!({"taskId":id})),
        (PROMPT, json!({"id":PROMPT})),
    ] {
        let response = http
            .client
            .post(&http.url)
            .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}))
            .send()
            .await?;
        assert_eq!(response.status(), 401);
    }
    let response = http
        .client
        .post(&http.url)
        .header("X-User-ID", CALLER)
        .header("Content-Type", "application/json")
        .body(" ".repeat(a2a_server_lf::jsonrpc::MAX_REQUEST_BODY_BYTES + 1))
        .send()
        .await?;
    assert_eq!(response.status(), 413);
    let records = events(&http.h.logs);
    assert_eq!(records.len(), 4);
    let expected = format!("{}…", "é".repeat(256));
    assert_eq!(records[0]["task_id"], expected);
    assert_eq!(records[0]["context_id"], expected);
    assert_eq!(records[0]["message_id"], expected);
    assert_eq!(records[1]["method"], "GetTaskPushNotificationConfig");
    assert_eq!(records[1]["task_id"], expected);
    assert_eq!(records[2]["method"], "Unknown");
    assert!(records[2].get("task_id").is_none());
    assert_eq!(records[3]["method"], "Unknown");
    assert_eq!(records[3]["http_status"], 413);
    assert_private(&http.h.logs);
    Ok(())
}
