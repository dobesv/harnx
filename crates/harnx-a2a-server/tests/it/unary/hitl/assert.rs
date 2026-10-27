use crate::unary::fixtures::*;
use anyhow::{Context, Result};
use harnx_a2a_server::store::TaskRecord;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::nats_session_log::NatsSessionLog;
use harnx_runtime::ApprovalRef;
use serde_json::{json, Value};

pub(crate) fn reply(id: &str, task: &Value, text: &str, task_id: bool) -> Value {
    let mut message = json!({
        "messageId": id,
        "role": "ROLE_USER",
        "parts": [{"text": text}],
        "contextId": task["contextId"],
    });
    if task_id {
        message["taskId"] = task["id"].clone();
    }
    message
}

pub(crate) async fn challenge(http: &Http) -> Result<Value> {
    let response = http
        .rpc(
            "SendMessage",
            json!({"message": message("hitl-initial", None)}),
        )
        .await?;
    assert_eq!(
        response["result"]["task"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{response}"
    );
    Ok(response["result"]["task"].clone())
}

pub(crate) async fn entries(http: &Http, task: &Value) -> Result<Vec<(u64, SessionLogEntry)>> {
    let storage = harnx_core::session_identity::session_key(
        Some(&http.h.export.agent),
        task["contextId"].as_str().unwrap(),
    );
    NatsSessionLog::new_with_replicas(http.h.jetstream.clone(), &storage, 1)
        .load_events_latest_required_async()
        .await
}

pub(crate) fn decisions(entries: &[(u64, SessionLogEntry)]) -> usize {
    entries
        .iter()
        .filter(|(_, entry)| matches!(entry, SessionLogEntry::HitlApprovalDecision { .. }))
        .count()
}

pub(crate) fn external_prompts(entries: &[(u64, SessionLogEntry)]) -> usize {
    entries
        .iter()
        .filter(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::Message { role, content, .. }
                    if role.is_user() && !harnx_core::timing_identity::is_timing_notice(&content.to_text())
            )
        })
        .count()
}

pub(crate) async fn read_hitl_request(http: &Http, storage: &str) -> Result<ApprovalRef> {
    let context = http
        .h
        .store
        .read_context(storage)
        .await?
        .context("context")?;
    Ok(context
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .hitl_checkpoint
        .as_ref()
        .unwrap()
        .request
        .clone())
}

pub(crate) fn assert_same_round_step(response: &Value, task: &Value, expected: &str) {
    assert_eq!(response["result"]["task"]["id"], task["id"], "{response}");
    assert_eq!(response["result"]["task"]["contextId"], task["contextId"]);
    assert_eq!(
        response["result"]["task"]["status"]["state"], expected,
        "{response}"
    );
}

pub(crate) fn assert_same_round_request_continuity(requests: &[ApprovalRef]) {
    assert_eq!(requests[0].invocation_id, requests[1].invocation_id);
    assert_eq!(requests[0].tool_round_seq, requests[1].tool_round_seq);
    assert!(requests[0].request_seq < requests[1].request_seq);
    assert_same_round_call_ids(requests);
}

fn assert_same_round_call_ids(requests: &[ApprovalRef]) {
    assert_eq!(requests[0].tool_call_id, "first-approval");
    assert_eq!(requests[1].tool_call_id, "second-approval");
}

pub(crate) async fn assert_same_round_record_and_log(
    http: &Http,
    id: &str,
    task: &Value,
    original: &TaskRecord,
) -> Result<()> {
    let final_record = http.h.task(id).await?;
    assert_eq!(final_record.execution_id, original.execution_id);
    assert_eq!(final_record.user_msg_seq, original.user_msg_seq);
    let log = entries(http, task).await?;
    assert_eq!(external_prompts(&log), 1);
    assert_eq!(decisions(&log), 2);
    Ok(())
}

pub(crate) fn assert_same_round_native_tools(requests: &[Value]) -> Result<()> {
    assert!(!requests[1].to_string().contains("audit only"));
    let results: Vec<_> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(
        results.len(),
        2,
        "both native tools must execute: {results:?}"
    );
    for result in results {
        let output: Value = serde_json::from_str(result["content"].as_str().unwrap())?;
        assert_eq!(output["timezone"], "UTC", "{output}");
        assert!(
            output["datetime"].is_string(),
            "native tool must succeed: {output}"
        );
    }
    Ok(())
}

pub(crate) async fn assert_rejection_dedupe_and_conflict(
    http: &Http,
    task: &Value,
    answer: &Value,
) -> Result<()> {
    let duplicate = http.rpc("SendMessage", json!({"message": answer})).await?;
    assert_eq!(duplicate["result"]["task"]["id"], task["id"], "{duplicate}");
    let mut changed = answer.clone();
    changed["parts"] = json!([{"text": "y"}]);
    code(
        &http.rpc("SendMessage", json!({"message": changed})).await?,
        -32602,
    );
    assert_eq!(http.h.llm.requests.lock().len(), 3);
    Ok(())
}
