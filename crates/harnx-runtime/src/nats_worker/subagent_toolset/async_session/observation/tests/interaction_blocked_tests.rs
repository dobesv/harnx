use super::{fixture::*, *};
use harnx_core::{interaction::InteractionResponse, session::SessionLogEntry};

#[tokio::test]
async fn child_session_wait_returns_blocked_when_interaction_pending() -> Result<()> {
    let f = Fixture::new().await?;
    let prompt = f.dispatch().await?;

    // 1. Tool call and interaction requested in open tool round
    let round_seq = f.append(tool_calls("interact-call", "ask_user")).await?;
    let req_seq = f
        .append(SessionLogEntry::InteractionRequested {
            tool_round_seq: round_seq,
            tool_call_id: "interact-call".into(),
            interaction_id: "int-child-1".into(),
            producer: "middleware:confirm".into(),
            step_path: vec![],
            key: "1".into(),
            kind: "confirm".into(),
            payload: json!({}),
            summary: "Approve deploy".into(),
            chain_version: "v1".into(),
            checkpoint: "cp".into(),
            fence_token: 0,
        })
        .await?;

    // Unready before SuspensionReady -> still pending
    let pending = f
        .poll(json!({"seconds": 1, "include_partial": false}))
        .await?;
    assert_eq!(pending["status"], "pending");

    // 2. SuspensionReady gates pending interaction -> session_wait reports blocked non-terminal
    f.append(SessionLogEntry::SuspensionReady {
        tool_round_seq: round_seq,
        pending: vec!["int-child-1".into()],
        fence_token: 0,
    })
    .await?;

    let blocked = f
        .poll(json!({"seconds": 1, "include_partial": false}))
        .await?;
    assert_eq!(blocked["status"], "blocked");
    assert_eq!(blocked["terminal"], false);

    // 3. Response accepted -> unblocked, round completes normally
    f.append(SessionLogEntry::InteractionResponse {
        request_seq: req_seq,
        interaction_id: "int-child-1".into(),
        tool_call_id: "interact-call".into(),
        response: InteractionResponse {
            action: "approve".into(),
            data: serde_json::Map::new(),
            note: None,
        },
        fence_token: 0,
    })
    .await?;
    f.append(end(prompt.prompt.user_msg_seq())).await?;

    let completed = f
        .poll(json!({"seconds": 1, "include_partial": false}))
        .await?;
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["terminal"], true);

    Ok(())
}
