use super::*;
use crate::utils::create_abort_signal;
use harnx_core::tool::ToolInputRequired;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

fn reply(output: serde_json::Value) -> OperatorToolReply {
    OperatorToolReply {
        output: serde_json::to_string_pretty(&output).unwrap(),
        error: None,
        error_kind: None,
    }
}

fn error_reply(error: &str) -> OperatorToolReply {
    OperatorToolReply {
        output: String::new(),
        error: Some(error.to_string()),
        error_kind: None,
    }
}

/// A scripted transport: hands out replies in call order and records every
/// command it saw.
struct Scripted {
    commands: Arc<Mutex<Vec<OperatorToolCommand>>>,
    replies: Mutex<VecDeque<Result<OperatorToolReply, String>>>,
}

impl Scripted {
    fn new(replies: Vec<Result<OperatorToolReply, String>>) -> Self {
        Self {
            commands: Arc::new(Mutex::new(Vec::new())),
            replies: Mutex::new(replies.into()),
        }
    }

    fn call(
        &self,
        command: OperatorToolCommand,
    ) -> impl Future<Output = Result<OperatorToolReply>> + '_ {
        let commands = self.commands.clone();
        let replies = &self.replies;
        async move {
            commands.lock().unwrap().push(command);
            let next = replies.lock().unwrap().pop_front();
            match next {
                Some(Ok(reply)) => Ok(reply),
                Some(Err(error)) => Err(anyhow::anyhow!(error)),
                None => Err(anyhow::anyhow!("no scripted reply left")),
            }
        }
    }
}

fn list_reply(names: &[&str]) -> OperatorToolReply {
    reply(json!(names
        .iter()
        .map(|name| json!({ "name": name }))
        .collect::<Vec<_>>()))
}

fn connect_challenge(round: u64, challenge_id: &str) -> OperatorToolReply {
    reply(json!({
        "url": format!("https://tools.example.test/connect/{challenge_id}"),
        "message": format!("Connect jira (round {round})"),
        "request_state": { "challenge_id": challenge_id, "round": round },
    }))
}

fn text_result(text: &str) -> OperatorToolReply {
    reply(json!({ "content": [ { "type": "text", "text": text } ] }))
}

#[tokio::test]
async fn list_reports_each_credentialed_server_without_secrets() {
    let scripted = Scripted::new(vec![
        // Replies in call order: the discovery list, then jira's status, then
        // pendo's.
        Ok(list_reply(&[
            "jira_credential_status",
            "pendo_credential_status",
        ])),
        Ok(reply(json!({
            "connected": true,
            "account_label": "alice@example.com",
            "expires_at": "2027-11-22T12:00:00+00:00",
            "status": "active",
        }))),
        Ok(reply(json!({ "connected": false }))),
    ]);
    let commands = scripted.commands.clone();
    let outcome = run_credentials_command(
        CredentialsAction::List,
        &create_abort_signal(),
        &mut |_| {},
        |command| scripted.call(command),
    )
    .await
    .unwrap();
    assert_eq!(outcome.failures, 0);
    assert_eq!(
        outcome.output,
        [
            "jira: connected, as alice@example.com, expires 2027-11-22T12:00:00+00:00",
            "pendo: not connected",
        ]
        .join("\n")
    );
    let commands = commands.lock().unwrap();
    // The list narrows to the credential-status tools, and each status call
    // carries no arguments and no tool input.
    assert!(matches!(
        &commands[0],
        OperatorToolCommand::List { pattern: Some(pattern) } if pattern == "*_credential_status"
    ));
    for command in &commands[1..] {
        let OperatorToolCommand::Call {
            name,
            args_json,
            tool_input,
        } = command
        else {
            panic!("expected call commands, got {command:?}");
        };
        assert!(name.ends_with("_credential_status"));
        assert_eq!(args_json, "{}");
        assert!(tool_input.is_none());
        assert!(!format!("{command:?}").contains("secret"));
    }
}

#[tokio::test]
async fn list_marks_failures_and_ignores_unrelated_tools() {
    let scripted = Scripted::new(vec![
        Ok(list_reply(&[
            "jira_credential_status",
            "pendo_credential_status",
            "fs_read",
        ])),
        Ok(reply(json!({ "connected": true, "status": "needs_login" }))),
        Ok(error_reply(
            "sign in to harnx (or set user_id / HARNX_USER_ID) to use pendo",
        )),
    ]);
    let outcome = run_credentials_command(
        CredentialsAction::List,
        &create_abort_signal(),
        &mut |_| {},
        |command| scripted.call(command),
    )
    .await
    .unwrap();
    assert_eq!(outcome.failures, 1);
    assert!(outcome
        .output
        .contains("jira: connected but needs re-login"));
    assert!(outcome
        .output
        .contains("pendo: error: sign in to harnx (or set user_id / HARNX_USER_ID)"));
    // `fs_read` is not a credential-status tool: never called.
    let commands = scripted.commands.lock().unwrap();
    assert_eq!(commands.len(), 3);
}

#[tokio::test]
async fn list_without_credentialed_servers_is_a_clean_success() {
    let scripted = Scripted::new(vec![Ok(list_reply(&["fs_read"]))]);
    let outcome = run_credentials_command(
        CredentialsAction::List,
        &create_abort_signal(),
        &mut |_| {},
        |command| scripted.call(command),
    )
    .await
    .unwrap();
    assert_eq!(outcome.failures, 0);
    assert!(outcome.output.contains("No credentialed tool servers"));
}

#[tokio::test]
async fn disconnect_calls_the_management_tool_and_reports_its_text() {
    let scripted = Scripted::new(vec![Ok(text_result("disconnected alice@example.com"))]);
    let commands = scripted.commands.clone();
    let outcome = run_credentials_command(
        CredentialsAction::Disconnect {
            server: "jira".into(),
        },
        &create_abort_signal(),
        &mut |_| {},
        |command| scripted.call(command),
    )
    .await
    .unwrap();
    assert_eq!(outcome.output, "disconnected alice@example.com");
    let commands = commands.lock().unwrap();
    let OperatorToolCommand::Call {
        name, args_json, ..
    } = &commands[0]
    else {
        panic!("expected a call");
    };
    assert_eq!(name, "jira_disconnect");
    assert_eq!(args_json, "{}");
}

#[tokio::test]
async fn disconnect_surfaces_the_tool_error() {
    let scripted = Scripted::new(vec![Ok(error_reply(
        "sign in to harnx (or set user_id / HARNX_USER_ID) to use jira",
    ))]);
    let error = run_credentials_command(
        CredentialsAction::Disconnect {
            server: "jira".into(),
        },
        &create_abort_signal(),
        &mut |_| {},
        |command| scripted.call(command),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("set user_id"), "{error:#}");
}

#[tokio::test]
async fn connect_prints_the_url_once_and_polls_until_completed() {
    // Replies in call order: the first challenge, a re-issued challenge, then
    // the completed result.
    let scripted = Scripted::new(vec![
        Ok(connect_challenge(1, "challenge-1")),
        Ok(connect_challenge(2, "challenge-1")),
        Ok(text_result(
            "Connected as alice@example.com; 23 jira tools are available from your next message.",
        )),
    ]);
    let commands = scripted.commands.clone();
    let mut urls = Vec::new();
    let outcome = run_credentials_command(
        CredentialsAction::Connect {
            server: "jira".into(),
        },
        &create_abort_signal(),
        &mut |progress| {
            let CredentialsProgress::ConnectUrl { url, message } = progress;
            urls.push((url, message));
        },
        |command| scripted.call(command),
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.output,
        "Connected as alice@example.com; 23 jira tools are available from your next message."
    );
    // The re-issued challenge has the same URL: reported once.
    assert_eq!(urls.len(), 1);
    assert_eq!(urls[0].0, "https://tools.example.test/connect/challenge-1");
    assert_eq!(urls[0].1, "Connect jira (round 1)");
    let commands = commands.lock().unwrap();
    // The first call starts a fresh flow; each poll continues the latest
    // round's challenge with a bare `continue` action.
    let OperatorToolCommand::Call { tool_input, .. } = &commands[0] else {
        panic!("expected a call");
    };
    assert!(tool_input.is_none());
    for (index, command) in commands.iter().enumerate().skip(1) {
        let OperatorToolCommand::Call { tool_input, .. } = command else {
            panic!("expected a call");
        };
        let input = tool_input.as_ref().unwrap();
        let round = input.request_state.get("round").unwrap().as_u64().unwrap();
        let responses = input
            .responses
            .get(&format!("connect-round-{round}"))
            .unwrap();
        assert_eq!(responses.action, ToolInputRequired::URL_ACTION_CONTINUE);
        assert!(responses.data.is_empty());
        assert!(responses.note.is_none());
        assert_eq!(round, u64::try_from(index).unwrap());
    }
    assert_eq!(commands.len(), 3);
}

#[tokio::test]
async fn connect_stops_on_tool_error() {
    let scripted = Scripted::new(vec![
        Ok(connect_challenge(1, "challenge-1")),
        Ok(error_reply(
            "connection link expired; call jira_connect again",
        )),
    ]);
    let error = run_credentials_command(
        CredentialsAction::Connect {
            server: "jira".into(),
        },
        &create_abort_signal(),
        &mut |_| {},
        |command| scripted.call(command),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("connection link expired"));
}

#[tokio::test]
async fn connect_ctrl_c_sends_cancel_and_reports_cancellation() {
    let scripted = Scripted::new(vec![
        Ok(connect_challenge(1, "challenge-1")),
        Ok(text_result("connection cancelled")),
    ]);
    let abort = create_abort_signal();
    let commands = scripted.commands.clone();
    let abort_for_task = abort.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        abort_for_task.set_ctrlc();
    });
    let outcome = run_credentials_command(
        CredentialsAction::Connect {
            server: "jira".into(),
        },
        &abort,
        &mut |_| {},
        |command| scripted.call(command),
    )
    .await
    .unwrap();
    let _ = canceller.await;
    assert_eq!(outcome.output, "connection cancelled");
    let commands = commands.lock().unwrap();
    let OperatorToolCommand::Call { tool_input, .. } = commands.last().unwrap() else {
        panic!("expected a call");
    };
    let input = tool_input.as_ref().unwrap();
    let response = input.responses.get("connect-round-1").unwrap();
    assert_eq!(response.action, ToolInputRequired::URL_ACTION_CANCEL);
}

#[test]
fn parses_credentials_actions() {
    assert_eq!(
        parse_credentials_action("").unwrap(),
        CredentialsAction::List
    );
    assert_eq!(
        parse_credentials_action("list").unwrap(),
        CredentialsAction::List
    );
    assert_eq!(
        parse_credentials_action("connect jira").unwrap(),
        CredentialsAction::Connect {
            server: "jira".into()
        }
    );
    assert_eq!(
        parse_credentials_action("disconnect pendo").unwrap(),
        CredentialsAction::Disconnect {
            server: "pendo".into()
        }
    );
    assert!(parse_credentials_action("connect").is_err());
    assert!(parse_credentials_action("remove jira").is_err());
}

#[test]
fn challenge_key_matches_the_connect_tools_wire_contract() {
    assert_eq!(connect::connect_challenge_key(3), "connect-round-3");
}
