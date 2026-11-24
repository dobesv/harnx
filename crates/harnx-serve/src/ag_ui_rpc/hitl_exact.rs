//! Parent rows answer the displayed generation, never a reused provider tool ID.
use super::{json_rpc_error, json_rpc_response, session_exists, JSON_RPC_UNKNOWN_SESSION_CODE};
use crate::ag_ui::AppResponse;
use crate::session_actor::{SessionCommand, SessionHandle, SessionKey, SessionRegistry};
use harnx_runtime::{ApprovalRef, HitlDecisionOutcome};
use http::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

const INVALID_PARAMS_CODE: i64 = -32602;
const ROUTING_FAILED_CODE: i64 = -32003;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactDecisionParams {
    request: ApprovalRef,
    approved: bool,
    #[serde(default)]
    note: Option<String>,
}

fn parse_params(params: Option<Value>) -> Result<ExactDecisionParams, &'static str> {
    let params: ExactDecisionParams = serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|_| "expected request {invocation_id, tool_round_seq, request_seq, tool_call_id}, approved and optional note")?;
    let request = &params.request;
    if request.invocation_id.trim().is_empty()
        || request.tool_call_id.trim().is_empty()
        || request.tool_round_seq == 0
        || request.request_seq <= request.tool_round_seq
    {
        return Err("request must name a non-empty invocation/tool call and an approval after its tool round");
    }
    Ok(params)
}

pub(super) async fn handle(
    id: Value,
    params: Option<Value>,
    (config, registry, key): (&harnx_runtime::config::Config, &SessionRegistry, SessionKey),
) -> anyhow::Result<AppResponse> {
    let params = match parse_params(params) {
        Ok(params) => params,
        Err(message) => {
            return json_rpc_response(
                StatusCode::BAD_REQUEST,
                json_rpc_error(
                    id,
                    INVALID_PARAMS_CODE,
                    "invalid params",
                    Some(json!({"detail": message})),
                ),
            );
        }
    };
    if !registry.has_session(&key) && !session_exists(config, &key).await? {
        return json_rpc_response(
            StatusCode::NOT_FOUND,
            json_rpc_error(id, JSON_RPC_UNKNOWN_SESSION_CODE, "session not found", None),
        );
    }
    let handle = registry.get_or_spawn(key);
    let approved = params.approved;
    match route(&handle, params).await {
        Ok(outcome) => json_rpc_response(
            StatusCode::OK,
            json!({"jsonrpc": "2.0", "id": id, "result": outcome_value(outcome, approved)}),
        ),
        Err(message) => json_rpc_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json_rpc_error(id, ROUTING_FAILED_CODE, &message, None),
        ),
    }
}

async fn route(
    handle: &SessionHandle,
    params: ExactDecisionParams,
) -> Result<HitlDecisionOutcome, String> {
    let (reply, receive) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::HitlApprovalDecisionExact {
            request: params.request,
            approved: params.approved,
            note: params.note,
            reply,
        })
        .await
        .map_err(|_| "session actor unavailable".to_string())?;
    receive
        .await
        .map_err(|_| "session actor unavailable".to_string())?
}

fn outcome_value(outcome: HitlDecisionOutcome, approved: bool) -> Value {
    match outcome {
        HitlDecisionOutcome::Applied => json!({"applied": true, "outcome": "applied"}),
        HitlDecisionOutcome::AlreadyApplied { approved, note } => json!({
            "applied": true, "outcome": "already_applied", "approved": approved, "note": note,
        }),
        // Runtime's conflict means the opposite boolean is already durable for
        // this exact reference. Don't retry a decision just to read its note.
        HitlDecisionOutcome::Conflicting => json!({
            "applied": false, "outcome": "conflicting", "approved": !approved,
        }),
        HitlDecisionOutcome::Stale => json!({"applied": false, "outcome": "stale"}),
    }
}

#[cfg(test)]
mod tests;
