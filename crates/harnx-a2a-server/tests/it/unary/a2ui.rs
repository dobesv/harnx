//! Wire formats converge on the shared response contract and exact receipt binding.
use anyhow::Result;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::interactions::derive_pending_interactions;
use serde_json::{json, Value};

use crate::{
    support::{a2ui::*, Harness, Script},
    unary::{
        fixtures::*,
        hitl::assert::*,
        interaction::{executions, response_count},
    },
};

async fn setup(script: Script) -> Result<(Http, Value)> {
    let http = Http::from_harness(Harness::start(script).await?).await?;
    let task = challenge(&http).await?;
    Ok((http, task))
}

fn text_reply(id: &str, task: &Value, text: &str) -> Value {
    json!({"message": reply(id, task, text, true)})
}

async fn send(http: &Http, params: Value) -> Result<Value> {
    let response = http.rpc("SendMessage", params).await?;
    anyhow::ensure!(response["error"].is_null(), "{response}");
    Ok(response["result"]["task"].clone())
}

fn assert_completed(task: &Value) {
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{task}");
}

async fn resolve(structured: bool) -> Result<()> {
    let (http, task) = setup(Script::A2uiTool).await?;
    let mut expected = surface();
    expected["createSurface"]["surfaceId"] =
        data_part(&task)["data"]["createSurface"]["surfaceId"].clone();
    expected["createSurface"]["metadata"] = json!({"extensions": {"harnx": {"kind": "a2ui"}}});
    assert_eq!(data_part(&task)["data"], expected);
    assert!(data_part(&task).get("mimeType").is_none());
    let text = task["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("Pick a deployment target"), "{text}");
    assert_eq!(
        text,
        harnx_core::interaction::text::render(&data_part(&task)["data"])
    );
    assert!(text.contains("Production"));
    assert!(text.contains("Release name"));
    let log = entries(&http, &task).await?;
    let pending = derive_pending_interactions(&log)?;
    assert_eq!(pending.len(), 1);
    assert_eq!(executions(), 0);
    let params = if structured {
        let mut params = action_reply("answer", &task, "deploy", cases()[0]["values"].clone());
        let action = &mut params["message"]["parts"][0]["data"]["action"];
        // Renderer context isn't authoritative, even for static context values.
        action["context"] = json!({"region": "forged", "mode": "slow"});
        action["metadata"]["extensions"]["harnx_response"]["note"] = json!("audit note");
        params
    } else {
        text_reply("answer", &task, "deploy choice=prod name=rel-1")
    };
    let done = send(&http, params.clone()).await?;
    assert_completed(&done);
    let log = entries(&http, &task).await?;
    assert_eq!(response_count(&log), 1);
    assert_eq!(external_prompts(&log), 1);
    assert_eq!(executions(), 1);
    assert!(log.iter().any(|(_, entry)| matches!(entry, SessionLogEntry::InteractionResponse { interaction_id, request_seq, response, .. }
        if interaction_id == &pending[0].interaction_id && request_seq == &pending[0].request_seq
            && response.action == "deploy" && &response.data == cases()[0]["expected"].as_object().unwrap()
            && response.note.as_deref() == structured.then_some("audit note"))));
    send(&http, params).await?;
    assert_eq!(response_count(&entries(&http, &task).await?), 1);
    assert_eq!(executions(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_a2ui_datapart_maps_inputs_and_context() -> Result<()> {
    resolve(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_a2ui_text_maps_identically() -> Result<()> {
    resolve(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_a2ui_invalid_answers_correctable_without_journal() -> Result<()> {
    let (http, task) = setup(Script::A2uiTool).await?;
    let before = entries(&http, &task).await?;
    let invalid = [
        action_reply("unknown-action", &task, "approve", json!({})),
        action_reply(
            "bad-option",
            &task,
            "deploy",
            json!({"choice": ["invalid"]}),
        ),
        action_reply("bad-shape", &task, "deploy", json!({"name": ["invalid"]})),
        action_reply("unknown-key", &task, "deploy", json!({"undeclared": true})),
        text_reply("unparseable", &task, "deploy name has spaces"),
        text_reply("bad-text-option", &task, "deploy choice=invalid"),
    ];
    for params in invalid {
        let corrected = send(&http, params.clone()).await?;
        assert_eq!(corrected["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
        assert!(
            corrected["status"]["message"]["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Reply rejected:"),
            "{corrected}"
        );
        assert_eq!(data_part(&corrected)["data"], data_part(&task)["data"]);
        assert_eq!(entries(&http, &task).await?, before);
        assert_eq!(executions(), 0);
        // A rejected message isn't a durable decision receipt either.
        send(&http, params).await?;
        assert_eq!(entries(&http, &task).await?, before);
    }
    let done = send(
        &http,
        text_reply("corrected", &task, "deploy choice=prod name=rel-1"),
    )
    .await?;
    assert_completed(&done);
    assert_eq!(response_count(&entries(&http, &task).await?), 1);
    assert_eq!(executions(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_a2ui_deny_without_tool_dispatch() -> Result<()> {
    let (http, task) = setup(Script::A2uiTool).await?;
    assert_completed(&send(&http, text_reply("denied", &task, "deny")).await?);
    let log = entries(&http, &task).await?;
    assert_eq!(response_count(&log), 1);
    assert_eq!(executions(), 0);
    assert!(log.iter().any(
        |(_, entry)| matches!(entry, SessionLogEntry::InteractionResponse { response, .. }
        if response.action == "deny" && response.data.is_empty())
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_a2ui_duplicate_receipt_never_answers_later_challenge() -> Result<()> {
    let (http, task) = setup(Script::A2uiSerial).await?;
    let first = derive_pending_interactions(&entries(&http, &task).await?)?[0].clone();
    let params = action_reply("first", &task, "deploy", cases()[0]["values"].clone());
    let next = send(&http, params.clone()).await?;
    assert_eq!(next["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
    assert_ne!(
        data_part(&next)["data"]["createSurface"]["surfaceId"],
        data_part(&task)["data"]["createSurface"]["surfaceId"]
    );
    assert_eq!(
        data_part(&next)["data"]["createSurface"]["components"][2]["text"],
        "Second deployment target"
    );
    let pending = derive_pending_interactions(&entries(&http, &task).await?)?;
    assert_ne!(pending[0].interaction_id, first.interaction_id);
    send(&http, params).await?;
    assert_eq!(
        derive_pending_interactions(&entries(&http, &task).await?)?,
        pending
    );
    assert_eq!(response_count(&entries(&http, &task).await?), 1);
    assert_eq!(executions(), 0);
    let done = send(
        &http,
        text_reply("second", &next, "deploy choice=staging name=rel-2"),
    )
    .await?;
    assert_completed(&done);
    assert_eq!(response_count(&entries(&http, &task).await?), 2);
    assert_eq!(executions(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_a2ui_two_pending_only_head_resolved() -> Result<()> {
    let (http, task) = setup(Script::A2uiSameRound).await?;
    let pending = derive_pending_interactions(&entries(&http, &task).await?)?;
    assert_eq!(pending.len(), 2);
    assert_eq!(
        task["status"]["message"]["parts"].as_array().unwrap().len(),
        2
    );
    let next = send(&http, text_reply("one", &task, "deny")).await?;
    assert_eq!(next["status"]["state"], "TASK_STATE_INPUT_REQUIRED");
    assert_eq!(
        derive_pending_interactions(&entries(&http, &task).await?)?,
        vec![pending[1].clone()]
    );
    send(&http, text_reply("one", &task, "deny")).await?;
    assert_eq!(response_count(&entries(&http, &task).await?), 1);
    assert_completed(&send(&http, text_reply("two", &next, "deny")).await?);
    assert_eq!(response_count(&entries(&http, &task).await?), 2);
    Ok(())
}
