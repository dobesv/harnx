use harnx_a2a_server::store::TaskRecord;
use serde_json::Value;
pub(crate) mod assert;

use crate::support::{Harness, Script, DEADLINE};
use crate::unary::fixtures::*;
use anyhow::{Context, Result};
pub(crate) use assert::*;
use harnx_core::session::SessionLogEntry;
use serde_json::json;

fn make_approve_reply(task: &Value, task_id: bool) -> Value {
    let mut reply = reply("yes", task, "yes approval audit only", task_id);
    if task_id {
        reply.as_object_mut().unwrap().remove("contextId");
    }
    reply
}

async fn assert_approve_record_and_log(
    http: &Http,
    task: &Value,
    original: &TaskRecord,
) -> Result<()> {
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
    let log = entries(http, task).await?;
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
    Ok(())
}

async fn approve(task_id: bool) -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::Tool).await?).await?;
    let task = challenge(&http).await?;
    let original = http.h.task(task["id"].as_str().unwrap()).await?;
    let reply = make_approve_reply(&task, task_id);
    let response = http.rpc("SendMessage", json!({"message": reply})).await?;
    assert_eq!(response["result"]["task"]["id"], task["id"], "{response}");
    assert_eq!(
        response["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{response}"
    );
    assert_approve_record_and_log(&http, &task, &original).await?;
    let duplicate = http.rpc("SendMessage", json!({"message": reply})).await?;
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
    assert_rejection_dedupe_and_conflict(&http, &task, &answer).await?;
    Ok(())
}

async fn wait_for_publication_settled(http: &Http, storage: &str) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        while http
            .h
            .store
            .read_context(storage)
            .await?
            .unwrap()
            .document
            .state
            .active
            .as_ref()
            .unwrap()
            .publication
            .pending
            .is_some()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

async fn await_parked_baseline(http: &Http, storage: &str, task: &Value) -> Result<(Value, Value)> {
    wait_for_publication_settled(http, storage).await?;
    let before = serde_json::to_value(http.h.store.read_context(storage).await?.unwrap().document)?;
    let log = serde_json::to_value(entries(http, task).await?)?;
    Ok((before, log))
}

async fn assert_unrecognized_replies(
    http: &Http,
    task: &Value,
    storage: &str,
    baseline: &(Value, Value),
) -> Result<()> {
    let (before, log) = baseline;
    for parts in [
        json!([{"text":"yesterday"}]),
        json!([{"data":{"answer":"y"}}]),
        json!([{"text":"y"},{"data":{}}]),
    ] {
        let mut answer = reply("unrecognized", task, "", false);
        answer["parts"] = parts;
        let response = http.rpc("SendMessage", json!({"message": answer})).await?;
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
            &serde_json::to_value(http.h.store.read_context(storage).await?.unwrap().document)?,
            before
        );
        assert_eq!(&serde_json::to_value(entries(http, task).await?)?, log);
        assert!(http
            .h
            .store
            .get_reply_dedupe(storage, "unrecognized")
            .await?
            .is_none());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_hitl_unrecognized_text_and_data_have_no_durable_or_model_effect() -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::Tool).await?).await?;
    let task = challenge(&http).await?;
    let storage = harnx_core::session_identity::session_key(
        Some(&http.h.export.agent),
        task["contextId"].as_str().unwrap(),
    );
    let baseline = await_parked_baseline(&http, &storage, &task).await?;
    assert_unrecognized_replies(&http, &task, &storage, &baseline).await?;
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

async fn assert_serial_first_reply(http: &Http, task: &Value, first: &Value) -> Result<Value> {
    let second = http.rpc("SendMessage", json!({"message": first})).await?;
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
    let log = entries(http, task).await?;
    assert_eq!(decisions(&log), 1);
    let retry = http.rpc("SendMessage", json!({"message": first})).await?;
    assert_eq!(
        retry["result"]["task"]["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{retry}"
    );
    assert_eq!(decisions(&entries(http, task).await?), 1);
    Ok(second)
}

async fn assert_serial_second_reply(http: &Http, task: &Value) -> Result<()> {
    let last = http
        .rpc(
            "SendMessage",
            json!({"message": reply("second-no", task, "n second reason", false)}),
        )
        .await?;
    assert_eq!(
        last["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{last}"
    );
    assert_eq!(last["result"]["task"]["id"], task["id"]);
    let log = entries(http, task).await?;
    assert_eq!(decisions(&log), 2);
    assert_eq!(external_prompts(&log), 1);
    assert_eq!(http.h.llm.requests.lock().len(), 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_hitl_serial_replies_and_old_duplicate_never_answer_next_approval() -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::SerialTool).await?).await?;
    let task = challenge(&http).await?;
    let first = reply("first-no", &task, "n first reason", true);
    assert_serial_first_reply(&http, &task, &first).await?;
    assert_serial_second_reply(&http, &task).await?;
    Ok(())
}

async fn assert_invalid_reply_pairing(http: &Http, task: &Value, answer: &Value) -> Result<()> {
    code(
        &http
            .rpc_call(HttpRpcCall::new(
                &http.url,
                "bob",
                "SendMessage",
                json!({"message": answer}),
            ))
            .await?,
        -32001,
    );
    let mut mismatch = answer.clone();
    mismatch["contextId"] = json!("different-context");
    code(
        &http
            .rpc("SendMessage", json!({"message": mismatch}))
            .await?,
        -32602,
    );
    let mut unknown = answer.clone();
    unknown["taskId"] = json!(format!(
        "{}.{}",
        task["contextId"].as_str().unwrap(),
        uuid::Uuid::new_v4()
    ));
    code(
        &http.rpc("SendMessage", json!({"message": unknown})).await?,
        -32001,
    );
    assert_eq!(decisions(&entries(http, task).await?), 0);
    Ok(())
}

async fn assert_valid_reply_and_terminal_rejection(
    http: &Http,
    task: &Value,
    answer: &Value,
) -> Result<()> {
    let completed = http.rpc("SendMessage", json!({"message": answer})).await?;
    assert_eq!(
        completed["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    let terminal = http
        .rpc(
            "SendMessage",
            json!({"message": reply("new-terminal", task, "y", true)}),
        )
        .await?;
    code(&terminal, -32602);
    assert_eq!(
        terminal["error"]["message"],
        "terminal tasks cannot be continued; send a new message with contextId"
    );
    assert_eq!(decisions(&entries(http, task).await?), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_hitl_replies_preserve_owner_pairing_and_terminal_validation() -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::Tool).await?).await?;
    let task = challenge(&http).await?;
    let answer = reply("auth-yes", &task, "y", true);
    assert_invalid_reply_pairing(&http, &task, &answer).await?;
    assert_valid_reply_and_terminal_rejection(&http, &task, &answer).await?;
    Ok(())
}

struct SameRoundSetup {
    id: String,
    task: Value,
    original: TaskRecord,
    storage: String,
}

async fn setup_same_round(http: &Http) -> Result<SameRoundSetup> {
    let task = challenge(http).await?;
    let id = task["id"].as_str().context("task id")?.to_owned();
    let original = http.h.task(&id).await?;
    let storage = harnx_core::session_identity::session_key(
        Some(&http.h.export.agent),
        task["contextId"].as_str().context("context id")?,
    );
    Ok(SameRoundSetup {
        id,
        task,
        original,
        storage,
    })
}

async fn drive_same_round_steps(http: &Http, task: &Value, storage: &str) -> Result<()> {
    let mut requests = Vec::new();
    for (index, expected) in ["TASK_STATE_INPUT_REQUIRED", "TASK_STATE_COMPLETED"]
        .into_iter()
        .enumerate()
    {
        requests.push(read_hitl_request(http, storage).await?);
        let msg = reply(
            &format!("same-round-{index}"),
            task,
            "y audit only",
            index == 0,
        );
        let response = http.rpc("SendMessage", json!({"message": msg})).await?;
        assert_same_round_step(&response, task, expected);
        assert_eq!(
            http.h.llm.requests.lock().len(),
            index + 1,
            "Q2 must not call the model"
        );
    }
    assert_same_round_request_continuity(&requests);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_hitl_same_round_approvals_keep_one_task_and_invocation() -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::SameRoundTools).await?).await?;
    let s = setup_same_round(&http).await?;
    drive_same_round_steps(&http, &s.task, &s.storage).await?;
    assert_same_round_record_and_log(&http, &s.id, &s.task, &s.original).await?;
    assert_same_round_native_tools(&http.h.llm.requests.lock())?;
    Ok(())
}
