use super::*;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::nats_session_log::NatsSessionLog;

pub(crate) fn reply(id: &str, task: &Value, text: &str, task_id: bool) -> Value {
    let mut message = json!({"messageId":id,"role":"ROLE_USER","parts":[{"text":text}],"contextId":task["contextId"]});
    if task_id {
        message["taskId"] = task["id"].clone();
    }
    message
}

pub(crate) async fn challenge(http: &Http) -> Result<Value> {
    let response = http
        .rpc(
            "SendMessage",
            json!({"message":message("hitl-initial",None)}),
        )
        .await?;
    assert_eq!(
        response["result"]["task"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{response}"
    );
    Ok(response["result"]["task"].clone())
}

async fn entries(http: &Http, task: &Value) -> Result<Vec<(u64, SessionLogEntry)>> {
    let storage = harnx_core::session_identity::session_key(
        Some(&http.h.export.agent),
        task["contextId"].as_str().unwrap(),
    );
    NatsSessionLog::new_with_replicas(http.h.jetstream.clone(), &storage, 1)
        .load_events_latest_required_async()
        .await
}

fn decisions(entries: &[(u64, SessionLogEntry)]) -> usize {
    entries
        .iter()
        .filter(|(_, entry)| matches!(entry, SessionLogEntry::HitlApprovalDecision { .. }))
        .count()
}

fn external_prompts(entries: &[(u64, SessionLogEntry)]) -> usize {
    entries.iter().filter(|(_, entry)| matches!(entry, SessionLogEntry::Message { role, content, .. } if role.is_user() && !harnx_core::timing_identity::is_timing_notice(&content.to_text()))).count()
}

async fn approve(task_id: bool) -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::Tool).await?).await?;
    let task = challenge(&http).await?;
    let original = http.h.task(task["id"].as_str().unwrap()).await?;
    let reply = reply("yes", &task, "yes approval audit only", task_id);
    let mut reply = reply;
    if task_id {
        reply.as_object_mut().unwrap().remove("contextId");
    }
    let response = http.rpc("SendMessage", json!({"message":reply})).await?;
    assert_eq!(response["result"]["task"]["id"], task["id"], "{response}");
    assert_eq!(
        response["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{response}"
    );
    let final_record = http.h.task(task["id"].as_str().unwrap()).await?;
    assert_eq!(final_record.execution_id, original.execution_id);
    assert_eq!(final_record.user_msg_id, original.user_msg_id);
    assert_eq!(final_record.user_msg_seq, original.user_msg_seq);
    assert_eq!(
        final_record
            .task
            .history
            .as_ref()
            .unwrap()
            .iter()
            .filter(|m| m.message_id == "yes")
            .count(),
        1
    );
    let log = entries(&http, &task).await?;
    assert_eq!(external_prompts(&log), 1);
    assert_eq!(decisions(&log), 1);
    assert!(!http
        .h
        .llm
        .requests
        .lock()
        .last()
        .unwrap()
        .to_string()
        .contains("approval audit only"));
    let duplicate = http.rpc("SendMessage", json!({"message":reply})).await?;
    assert_eq!(duplicate["result"]["task"]["id"], task["id"], "{duplicate}");
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_hitl_approve_task_id_retains_original_invocation() -> Result<()> {
    approve(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_hitl_approve_context_only_retains_original_invocation() -> Result<()> {
    approve(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_hitl_rejection_full_reason_is_model_visible_and_reply_dedupe_survives_retire(
) -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::Tool).await?).await?;
    let task = challenge(&http).await?;
    let reason = format!("{} end of full reason", "why ".repeat(1100));
    let answer = reply("no", &task, &format!("n {reason}"), true);
    let response = http.rpc("SendMessage", json!({"message":answer})).await?;
    assert_eq!(
        response["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{response}"
    );
    assert!(http
        .h
        .llm
        .requests
        .lock()
        .last()
        .unwrap()
        .to_string()
        .contains(&reason));
    let storage = harnx_core::session_identity::session_key(
        Some(&http.h.export.agent),
        task["contextId"].as_str().unwrap(),
    );
    let current = http.h.store.read_context(&storage).await?.unwrap();
    assert_eq!(
        current
            .document
            .state
            .active
            .as_ref()
            .unwrap()
            .hitl_checkpoint
            .as_ref()
            .unwrap()
            .reply
            .as_ref()
            .unwrap()
            .note
            .as_ref()
            .unwrap()
            .len(),
        4096
    );
    let next = http
        .rpc(
            "SendMessage",
            json!({"message":message("next", Some(&task["contextId"]))}),
        )
        .await?;
    assert_eq!(
        next["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{next}"
    );
    assert_ne!(next["result"]["task"]["id"], task["id"]);
    let duplicate = http.rpc("SendMessage", json!({"message":answer})).await?;
    assert_eq!(duplicate["result"]["task"]["id"], task["id"], "{duplicate}");
    let mut changed = answer;
    changed["parts"] = json!([{"text":"y"}]);
    code(
        &http.rpc("SendMessage", json!({"message":changed})).await?,
        -32602,
    );
    assert_eq!(http.h.llm.requests.lock().len(), 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_hitl_unrecognized_text_and_data_have_no_durable_or_model_effect() -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::Tool).await?).await?;
    let task = challenge(&http).await?;
    // Wait until suspension's outbox/owner release has finished before comparing
    // all durable fields; no follow-up is permitted to change any of them.
    let storage = harnx_core::session_identity::session_key(
        Some(&http.h.export.agent),
        task["contextId"].as_str().unwrap(),
    );
    tokio::time::timeout(DEADLINE, async {
        while http
            .h
            .store
            .read_context(&storage)
            .await?
            .unwrap()
            .document
            .owner
            .is_some()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let before =
        serde_json::to_value(http.h.store.read_context(&storage).await?.unwrap().document)?;
    let log = serde_json::to_value(entries(&http, &task).await?)?;
    for parts in [
        json!([{"text":"yesterday"}]),
        json!([{"data":{"answer":"y"}}]),
        json!([{"text":"y"},{"data":{}}]),
    ] {
        let mut answer = reply("unrecognized", &task, "", false);
        answer["parts"] = parts;
        let response = http.rpc("SendMessage", json!({"message":answer})).await?;
        assert_eq!(
            response["result"]["task"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
            "{response}"
        );
        assert!(
            response["result"]["task"]["status"]["message"]["parts"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Reply not recognized")
        );
        assert_eq!(
            serde_json::to_value(http.h.store.read_context(&storage).await?.unwrap().document)?,
            before
        );
        assert_eq!(serde_json::to_value(entries(&http, &task).await?)?, log);
        assert!(http
            .h
            .store
            .get_reply_dedupe(&storage, "unrecognized")
            .await?
            .is_none());
    }
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_hitl_parked_cancel_stops_original_ticket() -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::Tool).await?).await?;
    let task = challenge(&http).await?;
    let cancel = http.rpc("CancelTask", json!({"id":task["id"]})).await?;
    assert_eq!(
        cancel["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{cancel}"
    );
    let log = entries(&http, &task).await?;
    assert_eq!(external_prompts(&log), 1);
    assert_eq!(decisions(&log), 0);
    assert!(log
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::Cancel { .. })));
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_hitl_serial_replies_and_old_duplicate_never_answer_next_approval() -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::SerialTool).await?).await?;
    let task = challenge(&http).await?;
    let first = reply("first-no", &task, "n first reason", true);
    let second = http.rpc("SendMessage", json!({"message":first})).await?;
    let second = second["result"]["task"].clone();
    assert_eq!(
        second["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{second}"
    );
    assert_eq!(second["id"], task["id"]);
    assert!(second["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Reply `y`"));
    let log = entries(&http, &task).await?;
    assert_eq!(decisions(&log), 1);
    let retry = http.rpc("SendMessage", json!({"message":first})).await?;
    assert_eq!(
        retry["result"]["task"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{retry}"
    );
    assert_eq!(decisions(&entries(&http, &task).await?), 1);
    let last = http
        .rpc(
            "SendMessage",
            json!({"message":reply("second-no",&task,"n second reason",false)}),
        )
        .await?;
    assert_eq!(
        last["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{last}"
    );
    assert_eq!(last["result"]["task"]["id"], task["id"]);
    let log = entries(&http, &task).await?;
    assert_eq!(decisions(&log), 2);
    assert_eq!(external_prompts(&log), 1);
    assert_eq!(http.h.llm.requests.lock().len(), 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_hitl_replies_preserve_owner_pairing_and_terminal_validation() -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::Tool).await?).await?;
    let task = challenge(&http).await?;
    let answer = reply("auth-yes", &task, "y", true);
    code(
        &http
            .rpc_call(HttpRpcCall::new(
                &http.url,
                "bob",
                "SendMessage",
                json!({"message":answer}),
            ))
            .await?,
        -32001,
    );
    let mut mismatch = answer.clone();
    mismatch["contextId"] = json!("different-context");
    code(
        &http.rpc("SendMessage", json!({"message":mismatch})).await?,
        -32602,
    );
    let mut unknown = answer.clone();
    unknown["taskId"] = json!(format!(
        "{}.{}",
        task["contextId"].as_str().unwrap(),
        uuid::Uuid::new_v4()
    ));
    code(
        &http.rpc("SendMessage", json!({"message":unknown})).await?,
        -32001,
    );
    assert_eq!(decisions(&entries(&http, &task).await?), 0);
    let completed = http.rpc("SendMessage", json!({"message":answer})).await?;
    assert_eq!(
        completed["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    let terminal = http
        .rpc(
            "SendMessage",
            json!({"message":reply("new-terminal", &task, "y", true)}),
        )
        .await?;
    code(&terminal, -32602);
    assert_eq!(
        terminal["error"]["message"],
        "terminal tasks cannot be continued; send a new message with contextId"
    );
    assert_eq!(decisions(&entries(&http, &task).await?), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_hitl_same_round_approvals_keep_one_task_and_invocation() -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::SameRoundTools).await?).await?;
    let task = challenge(&http).await?;
    let id = task["id"].as_str().context("task id")?;
    let original = http.h.task(id).await?;
    let storage = harnx_core::session_identity::session_key(
        Some(&http.h.export.agent),
        task["contextId"].as_str().context("context id")?,
    );
    let mut requests = Vec::new();
    for (index, expected) in ["TASK_STATE_INPUT_REQUIRED", "TASK_STATE_COMPLETED"]
        .into_iter()
        .enumerate()
    {
        let context = http
            .h
            .store
            .read_context(&storage)
            .await?
            .context("context")?;
        let request = context
            .document
            .state
            .active
            .as_ref()
            .unwrap()
            .hitl_checkpoint
            .as_ref()
            .unwrap()
            .request
            .clone();
        requests.push(request);
        let response = http.rpc("SendMessage", json!({"message":reply(&format!("same-round-{index}"), &task, "y audit only", index == 0)})).await?;
        assert_eq!(response["result"]["task"]["id"], task["id"], "{response}");
        assert_eq!(response["result"]["task"]["contextId"], task["contextId"]);
        assert_eq!(
            response["result"]["task"]["status"]["state"], expected,
            "{response}"
        );
        assert_eq!(
            http.h.llm.requests.lock().len(),
            index + 1,
            "Q2 must not call the model"
        );
    }
    assert_eq!(requests[0].invocation_id, requests[1].invocation_id);
    assert_eq!(requests[0].tool_round_seq, requests[1].tool_round_seq);
    assert!(requests[0].request_seq < requests[1].request_seq);
    assert_eq!(requests[0].tool_call_id, "first-approval");
    assert_eq!(requests[1].tool_call_id, "second-approval");
    let final_record = http.h.task(id).await?;
    assert_eq!(final_record.execution_id, original.execution_id);
    assert_eq!(final_record.user_msg_seq, original.user_msg_seq);
    let log = entries(&http, &task).await?;
    assert_eq!(external_prompts(&log), 1);
    assert_eq!(decisions(&log), 2);
    let requests = http.h.llm.requests.lock();
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
