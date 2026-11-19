use super::{json_rpc_error, json_rpc_response, session_exists, JSON_RPC_UNKNOWN_SESSION_CODE};
use crate::ag_ui::AppResponse;
use crate::session_actor::{SessionCommand, SessionHandle, SessionKey, SessionRegistry};
use harnx_core::interaction::InteractionResponse;
use harnx_runtime::interactions::InteractionDecisionOutcome;
use http::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

mod recorded;

const INVALID_PARAMS_CODE: i64 = -32602;
const RESPONSE_REJECTED_CODE: i64 = -32004;
const ROUTING_FAILED_CODE: i64 = -32003;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseParams {
    interaction_id: String,
    request_seq: u64,
    response: InteractionResponse,
}

fn parse_params(params: Option<Value>) -> Result<ResponseParams, &'static str> {
    let params: ResponseParams = serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|_| "expected interaction_id, request_seq and response {action, data, note?}")?;
    if params.interaction_id.trim().is_empty() || params.request_seq == 0 {
        return Err("interaction_id must be non-empty and request_seq must be positive");
    }
    if !matches!(params.response.action.as_str(), "approve" | "deny")
        || !params.response.data.is_empty()
    {
        return Err("this client supports only approve/deny responses with empty data");
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
                    Some(json!({ "detail": message })),
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
    let handle = registry.get_or_spawn(key.clone());
    let identity = (params.interaction_id.clone(), params.request_seq);
    let result = route(&handle, params).await;
    let recorded = if matches!(result, Ok(InteractionDecisionOutcome::Conflicting)) {
        // Reading display details must not change the durable rejection outcome.
        recorded::response(config, &key, &identity.0, identity.1)
            .await
            .unwrap_or_else(|error| {
                log::warn!("cannot read recorded interaction response: {error:#}");
                None
            })
    } else {
        None
    };
    response(id, result, recorded)
}

async fn route(
    handle: &SessionHandle,
    params: ResponseParams,
) -> Result<InteractionDecisionOutcome, String> {
    let (reply, receive) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::InteractionResponse {
            interaction_id: params.interaction_id,
            request_seq: params.request_seq,
            response: params.response,
            reply,
        })
        .await
        .map_err(|_| "session actor unavailable".to_string())?;
    receive
        .await
        .map_err(|_| "session actor unavailable".to_string())?
}

fn outcome_name(outcome: &InteractionDecisionOutcome) -> &'static str {
    match outcome {
        InteractionDecisionOutcome::Applied => "applied",
        InteractionDecisionOutcome::AlreadyApplied => "already_applied",
        InteractionDecisionOutcome::Conflicting => "conflicting",
        InteractionDecisionOutcome::WrongSequence => "wrong_sequence",
        InteractionDecisionOutcome::Stale => "stale",
        InteractionDecisionOutcome::AfterComplete => "after_complete",
        InteractionDecisionOutcome::LeaseLost => "lease_lost",
    }
}

fn response(
    id: Value,
    result: Result<InteractionDecisionOutcome, String>,
    recorded: Option<InteractionResponse>,
) -> anyhow::Result<AppResponse> {
    match result {
        Ok(outcome) if outcome.is_applied() => json_rpc_response(
            StatusCode::OK,
            json!({ "jsonrpc": "2.0", "id": id, "result": {
                "applied": true, "outcome": outcome_name(&outcome)
            }}),
        ),
        Ok(outcome) => {
            let mut data = json!({"outcome": outcome_name(&outcome)});
            if let Some(recorded) = recorded {
                data["response"] = json!(recorded);
            }
            json_rpc_response(
                StatusCode::OK,
                json_rpc_error(
                    id,
                    RESPONSE_REJECTED_CODE,
                    "interaction response rejected",
                    Some(data),
                ),
            )
        }
        Err(message) => json_rpc_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json_rpc_error(id, ROUTING_FAILED_CODE, &message, None),
        ),
    }
}

#[cfg(test)]
mod tests;
