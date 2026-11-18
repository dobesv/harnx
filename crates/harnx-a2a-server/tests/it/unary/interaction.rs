//! A2A replies resolve durable middleware interactions without admitting a new prompt.
use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::{config::Config, interactions::derive_pending_interactions};
use serde_json::{json, Value};

use crate::{
    support::{Harness, Script},
    unary::{fixtures::*, hitl::assert::*},
};

fn response_count(entries: &[(u64, SessionLogEntry)]) -> usize {
    entries
        .iter()
        .filter(|(_, e)| matches!(e, SessionLogEntry::InteractionResponse { .. }))
        .count()
}

fn executions() -> usize {
    std::fs::read_to_string(Config::config_dir().join("executions"))
        .unwrap_or_default()
        .lines()
        .count()
}

async fn answer(http: &Http, task: &Value, id: &str, text: &str) -> Result<Value> {
    let response = http
        .rpc(
            "SendMessage",
            json!({"message": reply(id, task, text, true)}),
        )
        .await?;
    assert!(response.get("error").is_none(), "{response}");
    Ok(response["result"]["task"].clone())
}

async fn setup_confirm(script: Script) -> Result<(Http, Value)> {
    let http = Http::from_harness(Harness::start(script).await?).await?;
    let task = challenge(&http).await?;
    Ok((http, task))
}

async fn decision(approved: bool) -> Result<()> {
    let (http, task) = setup_confirm(Script::ConfirmTool).await?;
    let original = http.h.task(task["id"].as_str().unwrap()).await?;
    let text = task["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("Run counted command?"), "{text}");
    assert!(!text.contains("Arguments:"));
    let pending = derive_pending_interactions(&entries(&http, &task).await?)?;
    assert_eq!(pending.len(), 1);
    assert_eq!(executions(), 0);
    let final_task = answer(
        &http,
        &task,
        "decision",
        if approved {
            "yes audit note"
        } else {
            "no deny note"
        },
    )
    .await?;
    assert_eq!(
        final_task["status"]["state"], "TASK_STATE_COMPLETED",
        "{final_task}"
    );
    assert_eq!(final_task["id"], task["id"]);
    let log = entries(&http, &task).await?;
    assert_eq!(external_prompts(&log), 1);
    assert_eq!(response_count(&log), 1);
    assert_eq!(executions(), usize::from(approved));
    assert!(log.iter().any(|(_, e)| matches!(e, SessionLogEntry::InteractionResponse { request_seq, interaction_id, response, .. }
        if *request_seq == pending[0].request_seq && *interaction_id == pending[0].interaction_id
            && response.action == if approved { "approve" } else { "deny" }
            && response.note.as_deref() == Some(if approved { "audit note" } else { "deny note" }))));
    let final_record = http.h.task(task["id"].as_str().unwrap()).await?;
    assert_eq!(final_record.execution_id, original.execution_id);
    assert_eq!(final_record.user_msg_id, original.user_msg_id);
    assert_eq!(final_record.user_msg_seq, original.user_msg_seq);
    answer(
        &http,
        &task,
        "decision",
        if approved {
            "yes audit note"
        } else {
            "no deny note"
        },
    )
    .await?;
    assert_eq!(response_count(&entries(&http, &task).await?), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_interaction_yes_runs_call_once() -> Result<()> {
    decision(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_interaction_no_denies_without_dispatch() -> Result<()> {
    decision(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_interaction_duplicate_receipt_never_answers_later_challenge() -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::ConfirmSerial).await?).await?;
    let task = challenge(&http).await?;
    let q1 = derive_pending_interactions(&entries(&http, &task).await?)?[0].clone();
    let next = answer(&http, &task, "first-reply", "yes").await?;
    assert_eq!(
        next["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{next}"
    );
    let q2 = derive_pending_interactions(&entries(&http, &task).await?)?[0].clone();
    assert_ne!(q1.interaction_id, q2.interaction_id);
    assert!(next["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Second confirmation?"));
    answer(&http, &task, "first-reply", "yes").await?;
    assert_eq!(
        derive_pending_interactions(&entries(&http, &task).await?)?,
        vec![q2]
    );
    assert_eq!(response_count(&entries(&http, &task).await?), 1);
    assert_eq!(executions(), 0);
    let done = answer(&http, &task, "second-reply", "yes").await?;
    assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED", "{done}");
    assert_eq!(executions(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_interaction_two_pending_show_one_at_a_time() -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::ConfirmSameRound).await?).await?;
    let task = challenge(&http).await?;
    let pending = derive_pending_interactions(&entries(&http, &task).await?)?;
    assert_eq!(pending.len(), 2);
    let text = task["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains(pending[0].summary()));
    assert!(!text.contains(pending[1].summary()));
    let next = answer(&http, &task, "one", "yes").await?;
    assert_eq!(
        next["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
        "{next}"
    );
    let remaining = derive_pending_interactions(&entries(&http, &task).await?)?;
    assert_eq!(remaining, vec![pending[1].clone()]);
    let text = next["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains(pending[1].summary()));
    assert!(!text.contains(pending[0].summary()));
    answer(&http, &task, "one", "yes").await?;
    assert_eq!(response_count(&entries(&http, &task).await?), 1);
    let done = answer(&http, &task, "two", "yes").await?;
    assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED", "{done}");
    let log = entries(&http, &task).await?;
    assert_eq!(response_count(&log), 2);
    assert_eq!(
        log.iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::ToolResults { .. }))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_interaction_unrecognized_reply_has_no_side_effects() -> Result<()> {
    let http = Http::from_harness(Harness::start(Script::ConfirmHeld).await?).await?;
    let task = http.working(message("initial", None)).await?;
    assert!(derive_pending_interactions(&entries(&http, &task).await?)?.is_empty());
    http.h.llm.release.notify_one();
    let ready = tokio::time::timeout(crate::support::DEADLINE, async {
        loop {
            let response = http.rpc("GetTask", json!({"id": task["id"]})).await?;
            if response["result"]["status"]["state"] == "TASK_STATE_INPUT_REQUIRED" {
                return Ok::<_, anyhow::Error>(response["result"].clone());
            }
            anyhow::ensure!(response["error"].is_null(), "{response}");
            tokio::task::yield_now().await;
        }
    })
    .await??;
    let before = entries(&http, &task).await?;
    let repeated = answer(&http, &ready, "unrecognized", "yesterday").await?;
    assert_eq!(repeated["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
    assert_eq!(entries(&http, &task).await?, before);
    let done = answer(&http, &ready, "deny", "no").await?;
    assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(executions(), 0);
    Ok(())
}
