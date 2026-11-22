//! What a wait hands the parent: the child's final reply once the prompt
//! settles, a small activity sample while it runs, and never the child's
//! transcript.
use super::{fixture::*, *};
use harnx_core::{message::MessageRole, session::SessionLogEntry, tool::ToolCall};

/// A tool round whose narration alone is `bytes` long, so a handful of them
/// outgrows any transcript page the result could carry.
fn narrated_round(call_id: &str, bytes: usize) -> SessionLogEntry {
    SessionLogEntry::ToolCalls {
        text: format!("{call_id}: {}", "step detail ".repeat(bytes / 12)),
        thought: Some("private reasoning".into()),
        calls: vec![ToolCall {
            id: Some(call_id.into()),
            name: "bash_exec".into(),
            arguments: json!({"command": format!("cargo nextest run {}", "-p crate ".repeat(50))}),
            thought_signature: None,
            reasoning_provenance: None,
        }],
        timestamp: None,
        fence_token: None,
    }
}

fn results() -> SessionLogEntry {
    SessionLogEntry::ToolResults {
        handoff_selection: None,
        results: vec![],
        timestamp: None,
    }
}

#[tokio::test]
async fn settled_wait_returns_the_final_reply_of_a_long_turn() -> Result<()> {
    let f = Fixture::new().await?;
    let prompt = f.dispatch().await?;
    for round in 0..40 {
        f.append(narrated_round(&format!("call-{round}"), 8 * 1024))
            .await?;
        f.append(results()).await?;
    }
    f.append(message(
        "reply",
        MessageRole::Assistant,
        "<think>\nweighing it\n</think>\nthe final answer",
    ))
    .await?;
    f.append(end(prompt.prompt.user_msg_seq())).await?;

    let result = f.poll(json!({"seconds": 1})).await?;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["terminal"], true);
    assert_eq!(result["response"], "the final answer");
    assert!(result.get("output").is_none(), "{result}");
    assert!(result.get("activity").is_none(), "{result}");
    assert!(result.get("response_truncated").is_none(), "{result}");
    let text = result.to_string();
    assert!(!text.contains("step detail"), "no transcript: {text}");
    assert!(!text.contains("\"original\""), "no echoed prompt: {text}");
    assert!(serialized(&result) < 4096, "{text}");
    // The same reply comes back from send's own wait and from a repeat wait.
    let mut replay = f.request.args.clone();
    replay["wait"] = json!({"seconds": 1});
    let replayed = send(
        &f.toolset,
        replay,
        CancellationToken::new(),
        f.send_context(),
    )
    .await?;
    assert_eq!(replayed["response"], "the final answer");
    Ok(())
}

#[tokio::test]
async fn pending_wait_returns_a_small_sample_of_the_latest_step() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    for round in 0..20 {
        f.append(narrated_round(&format!("call-{round}"), 8 * 1024))
            .await?;
        f.append(results()).await?;
    }
    f.append(narrated_round("call-latest", 8 * 1024)).await?;

    let result = f.poll(json!({"seconds": 1})).await?;
    assert_pending(&result, true);
    assert!(result.get("output").is_none(), "{result}");
    assert!(result.get("response").is_none(), "{result}");
    let latest = &result["activity"]["latest"];
    assert!(latest["index"].is_u64(), "{result}");
    let text = latest["text"].as_str().unwrap();
    assert!(text.starts_with("call-latest: step detail"), "{text}");
    assert!(text.len() <= 320, "narration is trimmed: {text}");
    let calls = latest["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    let call = calls[0].as_str().unwrap();
    assert!(
        call.starts_with("bash_exec {\"command\":\"cargo nextest"),
        "{call}"
    );
    assert!(call.len() <= 200, "arguments are trimmed: {call}");
    let whole = result.to_string();
    assert!(!whole.contains("private reasoning"), "no thinking: {whole}");
    assert!(!whole.contains("call-0:"), "no older steps: {whole}");
    assert!(serialized(&result) < 4096, "{whole}");
    Ok(())
}

#[tokio::test]
async fn include_partial_false_omits_the_activity_sample() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    f.append(narrated_round("call-latest", 1024)).await?;
    let result = f
        .poll(json!({"seconds": 1, "include_partial": false}))
        .await?;
    assert_pending(&result, true);
    assert!(result.get("activity").is_none(), "{result}");
    Ok(())
}

#[tokio::test]
async fn failed_turn_reports_its_failure_and_any_reply() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    f.append(narrated_round("call-0", 64)).await?;
    f.append(results()).await?;
    f.append(SessionLogEntry::Error {
        message: "model provider refused the request".into(),
        fence_token: 0,
        timestamp: None,
    })
    .await?;
    let result = f.poll(json!({"seconds": 1})).await?;
    assert_eq!(result["status"], "failed");
    assert_eq!(result["terminal"], true);
    assert_eq!(result["failure"], "model provider refused the request");
    assert!(result.get("response").is_none(), "{result}");
    Ok(())
}

#[tokio::test]
async fn oversized_reply_is_cut_to_the_result_budget() -> Result<()> {
    let f = Fixture::new().await?;
    let prompt = f.dispatch().await?;
    let huge = "lorem ipsum \"quoted\" ✓\n".repeat(20_000);
    f.append(message("reply", MessageRole::Assistant, &huge))
        .await?;
    f.append(end(prompt.prompt.user_msg_seq())).await?;
    let result = f.poll(json!({"seconds": 1})).await?;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["response_truncated"], true);
    let response = result["response"].as_str().unwrap();
    assert!(!response.is_empty());
    assert!(huge.starts_with(response));
    assert!(
        serialized(&result)
            <= crate::nats_worker::subagent_toolset::session_read::DEFAULT_MAX_OUTPUT_BYTES
                as usize
    );
    Ok(())
}
