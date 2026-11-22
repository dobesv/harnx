//! The interactive `credentials connect` flow (decision 11).
//!
//! One challenge-response loop against the server's connect tool: poll the
//! flow, surface each link change to the frontend, and answer Ctrl-C with a
//! best-effort `cancel` response so the pending link expires on its own.

use anyhow::{bail, Result};
use harnx_core::tool::ToolInputRequired;
use serde_json::{json, Value};
use std::future::Future;
use std::time::Duration;

use super::{
    management_call, parse_output, reply_error, result_text, CredentialsOutcome,
    CredentialsProgress, OperatorToolCommand, OperatorToolInput, OperatorToolReply,
};
use crate::utils::{wait_abort_signal, AbortSignal};

/// Poll cadence for a started connect flow.
const CONNECT_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Safety cap on polling rounds; well past the 10-minute link expiry, so a
/// live flow always ends through its own completion or expiry first.
const MAX_CONNECT_ROUNDS: u64 = 400;

pub(super) async fn run_connect<F, Fut>(
    server: String,
    abort: &AbortSignal,
    report: &mut impl FnMut(CredentialsProgress),
    call: &mut F,
) -> Result<CredentialsOutcome>
where
    F: FnMut(OperatorToolCommand) -> Fut,
    Fut: Future<Output = Result<OperatorToolReply>>,
{
    let name = format!("{server}_connect");
    // The latest challenge's round and opaque state, taken from each
    // response's `request_state` (`{challenge_id, round}` for the
    // harnx-mcp-remote connect tool).
    let mut round: Option<u64> = None;
    let mut challenge_id: Option<String> = None;
    let mut reported_url: Option<String> = None;
    for _ in 0..MAX_CONNECT_ROUNDS {
        let tool_input = match (round, &challenge_id) {
            (Some(round), Some(challenge_id)) => Some(OperatorToolInput::url_response(
                connect_challenge_key(round),
                ToolInputRequired::URL_ACTION_CONTINUE,
                json!({ "challenge_id": challenge_id, "round": round }),
            )),
            _ => None,
        };
        let command = management_call(name.clone(), tool_input);
        let reply = tokio::select! {
            reply = call(command) => reply?,
            _ = wait_abort_signal(abort) => return cancel_connect(round, challenge_id, name, call).await,
        };
        reply_error(&reply)?;
        let output = parse_output(&reply.output)?;
        let url = output.get("url").and_then(Value::as_str);
        let Some(url) = url else {
            return Ok(CredentialsOutcome {
                output: result_text(&reply),
                failures: 0,
            });
        };
        // A re-issued challenge (flow still pending upstream): keep polling.
        round = output
            .pointer("/request_state/round")
            .and_then(Value::as_u64)
            .or(round.map(|round| round + 1));
        challenge_id = output
            .pointer("/request_state/challenge_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(challenge_id);
        if reported_url.as_deref() != Some(url) {
            let message = output
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            report(CredentialsProgress::ConnectUrl {
                url: url.to_string(),
                message,
            });
            reported_url = Some(url.to_string());
        }
        tokio::select! {
            _ = tokio::time::sleep(CONNECT_POLL_INTERVAL) => {}
            _ = wait_abort_signal(abort) => return cancel_connect(round, challenge_id, name, call).await,
        }
    }
    bail!("connect flow did not settle; call {name} again")
}

/// Ctrl-C: tell the tool server the flow is cancelled. Best effort — if the
/// transport refuses (its own abort already fired), the pending link simply
/// expires on its own within its 10-minute TTL.
#[allow(clippy::too_many_arguments)]
async fn cancel_connect<F, Fut>(
    round: Option<u64>,
    challenge_id: Option<String>,
    name: String,
    call: &mut F,
) -> Result<CredentialsOutcome>
where
    F: FnMut(OperatorToolCommand) -> Fut,
    Fut: Future<Output = Result<OperatorToolReply>>,
{
    let cancelled = "connection cancelled".to_string();
    let Some((round, challenge_id)) = round.zip(challenge_id) else {
        return Ok(CredentialsOutcome {
            output: cancelled,
            failures: 0,
        });
    };
    let tool_input = OperatorToolInput::url_response(
        connect_challenge_key(round),
        ToolInputRequired::URL_ACTION_CANCEL,
        json!({ "challenge_id": challenge_id, "round": round }),
    );
    match call(management_call(name, Some(tool_input))).await {
        Ok(reply) => {
            let text = result_text(&reply);
            Ok(CredentialsOutcome {
                output: if text.is_empty() { cancelled } else { text },
                failures: 0,
            })
        }
        Err(_) => Ok(CredentialsOutcome {
            output: cancelled,
            failures: 0,
        }),
    }
}

/// The connect tool's per-round challenge key. This is the wire contract with
/// `harnx-mcp-remote`'s native connect tool (`connect-round-{round}`); the
/// CLI cannot see the challenge key any other way, because the operator
/// result carries only `{url, message, request_state}`.
pub(super) fn connect_challenge_key(round: u64) -> String {
    format!("connect-round-{round}")
}
