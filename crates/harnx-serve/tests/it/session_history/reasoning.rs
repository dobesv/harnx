use super::*;
use harnx_core::message::{MessageContent, MessageRole};

const ROUND_THOUGHT: &str = "Plan the verification round";
const REPLY_THOUGHT: &str = "Review the result";
const REPLY_ANSWER: &str = "The shared view is implemented. Paging tests passed.";

fn reasoning_entries() -> Vec<(u64, SessionLogEntry)> {
    let mut entries = entries();
    for (seq, entry) in &mut entries {
        match (seq, entry) {
            (17, SessionLogEntry::ToolCalls { text, thought, .. }) => {
                *text = format!("<think>\n{ROUND_THOUGHT}\n</think>\n\n{text}");
                *thought = Some("provider-only thought".into());
            }
            (
                19,
                SessionLogEntry::Message {
                    role: MessageRole::Assistant,
                    content,
                    ..
                },
            ) => {
                *content = MessageContent::Text(format!(
                    "<think>\n{REPLY_THOUGHT}\n</think>\n{REPLY_ANSWER}"
                ));
            }
            _ => {}
        }
    }
    entries
}

#[tokio::test]
async fn session_get_and_limit_one_pages_key_reasoning_only_to_returned_rows() -> Result<()> {
    let Some(f) = fixture_with_entries(true, reasoning_entries()).await? else {
        return Ok(());
    };
    let get = session_get(&f).await?;
    let rows = get["history_snapshot"].as_array().unwrap();
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[0]["id"], get["compaction"]["message_id"]);
    assert_round_with_reasoning(&get, 2);
    assert_eq!(rows[4]["content"], REPLY_ANSWER);
    assert_eq!(
        get["history_reasoning"],
        json!({
            rows[2]["id"].as_str().unwrap(): [ROUND_THOUGHT],
            rows[4]["id"].as_str().unwrap(): [REPLY_THOUGHT],
        })
    );
    assert_reasoning_pages(&f).await
}

async fn session_get(f: &Fixture) -> Result<Value> {
    let response = f.rpc("session/get", json!({})).await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(response.json::<Value>().await?["result"].clone())
}

async fn assert_reasoning_pages(f: &Fixture) -> Result<()> {
    let marker = f
        .history(json!({"from_seq":15,"to_seq":19,"limit":1}))
        .await?;
    assert_page_marker_id(&marker);
    let user = f.history(json!({"cursor":marker["next_cursor"]})).await?;
    assert_eq!(user["history_snapshot"][0]["role"], "user");
    assert_eq!(user["history_reasoning"], json!({}));
    let round = f.history(json!({"cursor":user["next_cursor"]})).await?;
    assert_eq!(round["history_snapshot"].as_array().unwrap().len(), 2);
    assert_round_with_reasoning(&round, 0);
    assert_eq!(round["history_reasoning"].as_object().unwrap().len(), 1);
    let reply = f.history(json!({"cursor":round["next_cursor"]})).await?;
    let rows = reply["history_snapshot"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["content"], REPLY_ANSWER);
    assert_eq!(
        reply["history_reasoning"],
        json!({rows[0]["id"].as_str().unwrap(): [REPLY_THOUGHT]})
    );
    assert!(reply["next_cursor"].is_null());
    Ok(())
}

fn assert_page_marker_id(marker: &Value) {
    assert_eq!(marker["history_snapshot"].as_array().unwrap().len(), 1);
    assert_eq!(
        marker["history_snapshot"][0]["id"],
        marker["compaction"]["message_id"]
    );
    assert_eq!(marker["history_reasoning"], json!({}));
}

fn assert_round_with_reasoning(result: &Value, start: usize) {
    let call = &result["history_snapshot"][start];
    let tool = &result["history_snapshot"][start + 1];
    assert_eq!(call["content"], "Check the transcript view tests.");
    assert_eq!(call["toolCalls"][0]["id"], "retained-round");
    assert_eq!(tool["toolCallId"], "retained-round");
    let id = call["id"].as_str().unwrap();
    assert_eq!(result["history_reasoning"][id], json!([ROUND_THOUGHT]));
}

#[tokio::test]
async fn sse_history_reasoning_companion_requires_opt_in_and_follows_snapshot() -> Result<()> {
    let Some(f) = fixture_with_entries(true, reasoning_entries()).await? else {
        return Ok(());
    };
    let plain = attach(&f, None).await?;
    let opted_in = attach(&f, Some("other-feature, history-reasoning")).await?;
    assert_companion_sequence(&opted_in);
    assert!(plain
        .iter()
        .all(|event| event["name"] != "history_reasoning"));
    assert_eq!(snapshot_index(&plain), snapshot_index(&opted_in));
    let snapshot = &plain[snapshot_index(&plain)];
    assert_eq!(snapshot["messages"][4]["content"], REPLY_ANSWER);
    assert!(snapshot.get("rawEvent").is_none());
    Ok(())
}

async fn attach(f: &Fixture, features: Option<&str>) -> Result<Vec<Value>> {
    let mut request = f
        .http
        .client
        .post(format!(
            "{}/v1/agents/allowed/sessions/history",
            f.http.base
        ))
        .header("x-user", "alice")
        .header("accept", "text/event-stream")
        .json(&json!({"messages":[]}));
    if let Some(features) = features {
        request = request.header("x-harnx-snapshot-features", features);
    }
    let response = request.send().await?;
    assert_eq!(response.status(), StatusCode::OK);
    response
        .text()
        .await?
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn snapshot_index(events: &[Value]) -> usize {
    events
        .iter()
        .position(|event| event["type"] == "MESSAGES_SNAPSHOT")
        .unwrap()
}

fn assert_companion_sequence(events: &[Value]) {
    let index = snapshot_index(events);
    let rows = events[index]["messages"].as_array().unwrap();
    assert_eq!(rows[2]["content"], "Check the transcript view tests.");
    assert_eq!(rows[4]["content"], REPLY_ANSWER);
    // Tool-call row IDs can change on replay. Check the companion against
    // the rows in the same snapshot, not those of an earlier request.
    let expected_reasoning = json!({
        rows[2]["id"].as_str().unwrap(): [ROUND_THOUGHT],
        rows[4]["id"].as_str().unwrap(): [REPLY_THOUGHT],
    });
    assert_eq!(
        events[index + 1],
        json!({
            "type":"CUSTOM", "name":"history_reasoning", "value":expected_reasoning,
        })
    );
    assert!(events[index].get("rawEvent").is_none());
}
