//! Assertions at each SSE boundary of the same-round approval scenario.
use super::*;
use crate::unary::hitl::reply;

pub(super) async fn assert_initial_gate(http: &Http) -> Result<Value> {
    let mut initial = Frames::open(
        http,
        "SendStreamingMessage",
        json!({"message":message("same-round-stream", None)}),
    )
    .await?;
    let task = initial.event().await?.context("initial snapshot")?["task"].clone();
    let mut interrupted = false;
    while let Some(event) = initial.event().await? {
        if let Some(state) = event["statusUpdate"]["status"]["state"].as_str() {
            assert_eq!(state, "TASK_STATE_INPUT_REQUIRED");
            interrupted = true;
        }
    }
    assert!(interrupted);
    assert_eq!(http.h.llm.requests.lock().len(), 1);
    Ok(task)
}

pub(super) async fn assert_reply_gate(
    http: &Http,
    task: &Value,
    index: usize,
    expected: &str,
) -> Result<()> {
    let mut frames = Frames::open(
        http,
        "SendStreamingMessage",
        json!({"message":reply(&format!("same-round-stream-{index}"), task, "y", false)}),
    )
    .await?;
    let first = frames.event().await?.context("resumed snapshot")?;
    assert_eq!(first["task"]["id"], task["id"]);
    assert_eq!(first["task"]["contextId"], task["contextId"]);
    assert_eq!(first["task"]["status"]["state"], "TASK_STATE_WORKING");
    let states = collect_gate_states(&mut frames, task).await?;
    assert_eq!(states, vec![expected]);
    assert_eq!(http.h.llm.requests.lock().len(), index + 1);
    Ok(())
}

async fn collect_gate_states(frames: &mut Frames, task: &Value) -> Result<Vec<String>> {
    let mut states = Vec::new();
    while let Some(event) = frames.event().await? {
        if let Some(state) = event["statusUpdate"]["status"]["state"].as_str() {
            assert_eq!(event["statusUpdate"]["taskId"], task["id"]);
            states.push(state.to_owned());
        }
    }
    Ok(states)
}

pub(super) async fn assert_cumulative_output(http: &Http, task: &Value) -> Result<()> {
    let final_record = http.h.task(task["id"].as_str().unwrap()).await?;
    let text = serde_json::to_value(&final_record.task.artifacts)?.to_string();
    assert!(text.contains("Before approval. "), "{text}");
    assert!(text.contains("Hello world"), "{text}");
    Ok(())
}
