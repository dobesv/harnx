//! Steering: a second message to a running context joins its live turn and
//! answers on the same task (issue #2774).
use super::fixtures::*;
use crate::streaming::{finish, snapshot_text, Frames};
use crate::support::{Harness, Script, DEADLINE};
use anyhow::{Context, Result};
use serde_json::{json, Value};

/// A steering message with distinguishable text.
fn steer(id: &str, context: &Value, text: &str) -> Value {
    let mut message = message(id, Some(context));
    message["parts"] = json!([{ "text": text }]);
    message
}

/// Count how often `needle` appears across a recorded LLM request's user
/// messages, so a duplicated steering delivery cannot pass unnoticed. Every
/// occurrence counts: two copies inside one message contribute 2.
fn user_message_occurrences(request: &Value, needle: &str) -> usize {
    request["messages"]
        .as_array()
        .expect("recorded request messages")
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .map(|content| content.matches(needle).count())
        .sum()
}

/// Wait for the running turn's model request, then steer a follow-up message
/// into it; the reply must be the same task (issue #2774).
async fn steer_live_turn(http: &Http, task: &Value, message_id: &str, text: &str) -> Result<()> {
    // Bounded: a wedged model round must fail the test, not hang it.
    tokio::time::timeout(DEADLINE, http.h.llm.requested.notified())
        .await
        .context("model round never started")?;
    let steered = http
        .working(steer(message_id, &task["contextId"], text))
        .await?;
    assert_eq!(steered["id"], task["id"], "{steered}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steering_message_reaches_next_request_returns_same_task_and_dedupes() -> Result<()> {
    let harness = Harness::start(Script::Steering).await?;
    let http = Http::from_harness(harness).await?;
    let first = http.working(message("steer-first", None)).await?;
    // Request 1 streams a tool call and holds before its completion frame,
    // so the steering append below lands before the injection seam.
    steer_live_turn(&http, &first, "steer-mid", "steer me: answer in blue").await?;

    // Retrying the steering message returns the same task without a second
    // delivery; changed parts under the same id keep the fingerprint error.
    let retry = http
        .working(steer(
            "steer-mid",
            &first["contextId"],
            "steer me: answer in blue",
        ))
        .await?;
    assert_eq!(retry["id"], first["id"]);
    let mismatch = http
        .rpc(
            "SendMessage",
            json!({"message": steer("steer-mid", &first["contextId"], "changed")}),
        )
        .await?;
    code(&mismatch, -32602);

    // Releasing the tool round injects the steered row into request 2.
    http.h.llm.release.notify_one();
    // Bounded: a wedged model round must fail the test, not hang it.
    tokio::time::timeout(DEADLINE, http.h.llm.requested.notified())
        .await
        .context("model round never started")?;
    let requests = http.h.llm.requests.lock().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        user_message_occurrences(&requests[1], "steer me: answer in blue"),
        1,
        "steered text must be delivered exactly once: {}",
        requests[1]
    );

    let completed = http.completed(&first["id"]).await?;
    assert_eq!(
        completed["result"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steered_rows_do_not_block_the_next_task_after_completion() -> Result<()> {
    let harness = Harness::start(Script::Text).await?;
    let http = Http::from_harness(harness).await?;
    let first = http.working(message("steer-covered-first", None)).await?;
    // Steer only after the running turn's model request is in flight: the
    // steering join targets a live turn (issue #2774), and racing it against
    // the turn's first dispatch leaves the runtime with an unpolled model
    // round. The Text script has no tool seam, so the steered row is answered
    // in a continuation round under the same invocation after this round.
    steer_live_turn(&http, &first, "steer-covered-mid", "steer me").await?;
    // Release the main round, prove the continuation round that answers the
    // steered row registered its own waiter, then release it: the task stays
    // working until that row drains, so the same task settles with the
    // steered answer included.
    http.h.llm.release.notify_one();
    crate::support::llm_requested_count(&http.h, 2).await?;
    http.h.llm.release.notify_one();
    http.completed(&first["id"]).await?;
    // A fresh message on the same context opens a new task normally; the
    // steered continuation must not wedge the admission head, and the new
    // prompt must reach the model exactly once.
    let next_response = http
        .rpc(
            "SendMessage",
            json!({"message": steer("steer-covered-next", &first["contextId"], "next turn"),
                   "configuration": {"returnImmediately": true}}),
        )
        .await?;
    anyhow::ensure!(
        next_response.get("error").is_none(),
        "next admission failed: {next_response}"
    );
    let next = next_response["result"]["task"].clone();
    assert_ne!(next["id"], first["id"]);
    http.h.llm.release.notify_one();
    http.completed(&next["id"]).await?;
    let requests = http.h.llm.requests.lock();
    // Round 1 answers the first task, a continuation round answers the
    // steered row, and the final round answers the new task.
    assert_eq!(
        requests.len(),
        3,
        "unexpected model rounds: {}",
        serde_json::to_string(&*requests).unwrap()
    );
    assert_eq!(user_message_occurrences(&requests[2], "next turn"), 1);
    assert_eq!(user_message_occurrences(&requests[1], "steer me"), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_steering_message_streams_same_task_to_final_state() -> Result<()> {
    let harness = Harness::start(Script::Steering).await?;
    let http = Http::from_harness(harness).await?;
    let mut first = Frames::open(
        &http,
        "SendStreamingMessage",
        json!({"message": message("stream-steer", None)}),
    )
    .await?;
    let snapshot = first.event().await?.context("missing snapshot")?;
    let id = snapshot["task"]["id"].clone();
    // Request 1 streams a tool call and holds before its completion frame.
    // Bounded: a wedged model round must fail the test, not hang it.
    tokio::time::timeout(DEADLINE, http.h.llm.requested.notified())
        .await
        .context("model round never started")?;

    let mut steered = Frames::open(
        &http,
        "SendStreamingMessage",
        json!({"message": steer("stream-steer-mid", &snapshot["task"]["contextId"], "steer me: answer in blue")}),
    )
    .await?;
    let steered_snapshot = steered
        .event()
        .await?
        .context("missing steering snapshot")?;
    assert_eq!(steered_snapshot["task"]["id"], id, "{steered_snapshot}");

    http.h.llm.release.notify_one();
    // Bounded: a wedged model round must fail the test, not hang it.
    tokio::time::timeout(DEADLINE, http.h.llm.requested.notified())
        .await
        .context("model round never started")?;
    let requests = http.h.llm.requests.lock().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        user_message_occurrences(&requests[1], "steer me: answer in blue"),
        1
    );

    let mut steered_answer = snapshot_text(&steered_snapshot["task"]);
    finish(&mut steered, &mut steered_answer).await?;
    let mut answer = snapshot_text(&snapshot["task"]);
    finish(&mut first, &mut answer).await?;
    assert_eq!(steered_answer, "Hello world");
    Ok(())
}

/// Wait for a task to reach INPUT_REQUIRED, tolerating the Working state it
/// passes through while the continuation round is in flight.
async fn input_required(http: &Http, id: &Value) -> Result<Value> {
    tokio::time::timeout(crate::support::DEADLINE, async {
        loop {
            let response = http.rpc("GetTask", json!({"id": id})).await?;
            if response["result"]["status"]["state"] == "TASK_STATE_INPUT_REQUIRED" {
                return Ok::<_, anyhow::Error>(response["result"].clone());
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
    .context("input required deadline")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steered_continuation_approval_parks_input_required_and_completes_after_reply() -> Result<()>
{
    let harness = Harness::start(Script::SteeringApproval).await?;
    let http = Http::from_harness(harness).await?;
    let first = http.working(message("steer-approval-root", None)).await?;
    // The root round streams plain text and holds, so the steering join
    // lands in the live turn (issue #2774).
    steer_live_turn(
        &http,
        &first,
        "steer-approval-mid",
        "steer me: call the tool",
    )
    .await?;
    http.h.llm.release.notify_one();
    // The continuation round answers with a bash_exec call the confirmation
    // middleware parks. The steered row is still outstanding, so the task
    // must surface INPUT_REQUIRED on the same task instead of holding a
    // completed-looking drain (review Q-A).
    let parked = input_required(&http, &first["id"]).await?;
    assert_eq!(parked["id"], first["id"], "{parked}");
    // Approving the parked request resumes the drain; the task completes
    // with the post-approval answer, not the root round's.
    let reply = crate::unary::hitl::reply(
        "steer-approval-reply",
        &first,
        "yes approval audit only",
        true,
    );
    let response = http.rpc("SendMessage", json!({"message": reply})).await?;
    assert_eq!(response["result"]["task"]["id"], first["id"], "{response}");
    assert_eq!(
        response["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
        "{response}"
    );
    let requests = http.h.llm.requests.lock().clone();
    assert_eq!(requests.len(), 3, "unexpected model rounds");
    assert_eq!(
        user_message_occurrences(&requests[1], "steer me: call the tool"),
        1,
        "steered text must be delivered exactly once: {}",
        requests[1]
    );
    let record = http.h.task(first["id"].as_str().unwrap()).await?;
    let artifact_text = record
        .task
        .artifacts
        .as_deref()
        .and_then(|artifacts| artifacts.first())
        .and_then(|artifact| artifact.parts.first())
        .and_then(|part| match &part.content {
            a2a_lf::PartContent::Text(text) => Some(text.as_str()),
            _ => None,
        });
    // Durable output reconciles through the steered-extended bound: the
    // invocation's whole text, root round plus the approved continuation.
    assert_eq!(
        artifact_text,
        Some("Hello worldSteered approved"),
        "the artifact must include the steered continuation answer"
    );
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steering_target_ended_falls_back_to_a_fresh_task() -> Result<()> {
    use harnx_a2a_server::fault_injection::Boundary;
    let harness = Harness::start(Script::Steering).await?;
    let http = Http::from_harness(harness).await?;
    let first = http.working(message("steer-ended-root", None)).await?;
    // Bounded: a wedged model round must fail the test, not hang it.
    tokio::time::timeout(DEADLINE, http.h.llm.requested.notified())
        .await
        .context("model round never started")?;
    // Pause between the busy decision and the steering join, end the live
    // turn inside that window, then resume: the join must refuse with the
    // typed end-of-turn error and the handler must fall back to a fresh
    // task that delivers the steered text exactly once (review C4-F3).
    let hooks = http.h.runner.fault_hooks();
    let mut pause = hooks.pause(
        Boundary::SteeringJoin,
        hooks.count(Boundary::SteeringJoin) + 1,
    );
    let steered_payload = json!({
        "jsonrpc": "2.0", "id": "1", "method": "SendMessage",
        "params": {"message": steer("steer-ended-mid", &first["contextId"], "steer me: run the tool")}
    });
    let client = http.client.clone();
    let url = http.url.clone();
    let steered = tokio::spawn(async move {
        let response = client
            .post(&url)
            .header("a2a-version", "1.0")
            .header("X-User-ID", "alice")
            .json(&steered_payload)
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        Ok::<_, anyhow::Error>(response.json::<Value>().await?)
    });
    pause.reached().await;
    // The root tool round completes, its tool executes, and the root turn
    // answers plain text; the old task settles without the steered row.
    http.h.llm.release.notify_one();
    http.completed(&first["id"]).await?;
    drop(pause);
    let response = steered.await??;
    anyhow::ensure!(
        response.get("error").is_none(),
        "fallback failed: {response}"
    );
    let task = response["result"]["task"].clone();
    assert_ne!(
        task["id"], first["id"],
        "the fallback must admit a new task"
    );
    // The fresh task may complete before the response is read.
    assert!(
        task["status"]["state"] == "TASK_STATE_WORKING"
            || task["status"]["state"] == "TASK_STATE_COMPLETED",
        "{response}"
    );
    // The fresh task answers with the steered text as its prompt.
    http.completed(&task["id"]).await?;
    let requests = http.h.llm.requests.lock().clone();
    assert_eq!(requests.len(), 3, "unexpected model rounds");
    assert!(
        !requests[1].to_string().contains("steer me: run the tool"),
        "the ended root round must not consume the steered text"
    );
    assert_eq!(
        user_message_occurrences(&requests[2], "steer me: run the tool"),
        1,
        "the fallback must deliver the steered text exactly once: {}",
        requests[2]
    );
    Ok(())
}
