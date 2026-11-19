//! `session/surface_action` JSON-RPC (plan T5.1): a frontend submits a user
//! action on a published non-modal A2UI surface.

use super::{json_rpc_error, json_rpc_response, session_exists, JSON_RPC_UNKNOWN_SESSION_CODE};
use crate::session_actor::{SessionCommand, SessionHandle, SessionKey, SessionRegistry};
use harnx_runtime::surfaces::{SurfaceActionOutcome, SurfaceActionSubmission};
use http::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

const INVALID_PARAMS_CODE: i64 = -32602;
const RESPONSE_REJECTED_CODE: i64 = -32004;
const ROUTING_FAILED_CODE: i64 = -32003;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SurfaceActionParams {
    surface_id: String,
    publish_seq: u64,
    response: harnx_core::interaction::InteractionResponse,
    #[serde(default)]
    user_message: Option<String>,
}

fn parse_params(params: Option<Value>) -> Result<SurfaceActionSubmission, &'static str> {
    let params: SurfaceActionParams = serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|_| {
            "expected surface_id, publish_seq, response {action, data, note?} and optional user_message"
        })?;
    if params.surface_id.trim().is_empty() || params.publish_seq == 0 {
        return Err("surface_id must be non-empty and publish_seq must be positive");
    }
    if params.response.action.trim().is_empty() {
        return Err("response.action must be non-empty");
    }
    Ok(SurfaceActionSubmission {
        surface_id: params.surface_id,
        publish_seq: params.publish_seq,
        response: params.response,
        user_message: params.user_message,
    })
}

pub(super) async fn handle(
    id: Value,
    params: Option<Value>,
    (config, registry, key): (&harnx_runtime::config::Config, &SessionRegistry, SessionKey),
) -> anyhow::Result<crate::ag_ui::AppResponse> {
    let submission = match parse_params(params) {
        Ok(submission) => submission,
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
    let result = route(&handle, submission).await;
    response(id, result)
}

async fn route(
    handle: &SessionHandle,
    submission: SurfaceActionSubmission,
) -> Result<SurfaceActionOutcome, String> {
    let (reply, receive) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::SurfaceAction { submission, reply })
        .await
        .map_err(|_| "session actor unavailable".to_string())?;
    receive
        .await
        .map_err(|_| "session actor unavailable".to_string())?
}

fn outcome_name(outcome: &SurfaceActionOutcome) -> &'static str {
    match outcome {
        SurfaceActionOutcome::Applied => "applied",
        SurfaceActionOutcome::AlreadyApplied => "already_applied",
        SurfaceActionOutcome::Conflicting => "conflicting",
        SurfaceActionOutcome::UnknownSurface => "unknown_surface",
        SurfaceActionOutcome::UndeclaredAction => "undeclared_action",
        SurfaceActionOutcome::InvalidVisibility => "invalid_visibility",
        SurfaceActionOutcome::InvalidTool => "invalid_tool",
        SurfaceActionOutcome::InvalidData => "invalid_data",
        SurfaceActionOutcome::UnauthorizedTool => "unauthorized_tool",
        SurfaceActionOutcome::ToolFailed => "tool_failed",
        SurfaceActionOutcome::LeaseLost => "lease_lost",
        SurfaceActionOutcome::AppendFailed => "append_failed",
        SurfaceActionOutcome::Pending => "pending",
    }
}

fn response(
    id: Value,
    result: Result<SurfaceActionOutcome, String>,
) -> anyhow::Result<crate::ag_ui::AppResponse> {
    match result {
        Ok(outcome) if outcome.is_applied() => json_rpc_response(
            StatusCode::OK,
            json!({ "jsonrpc": "2.0", "id": id, "result": {
                "applied": true, "outcome": outcome_name(&outcome)
            }}),
        ),
        Ok(outcome) => json_rpc_response(
            // Rejections mirror the interaction RPC: a well-formed submission
            // the durable state refuses answers with the rejected code, the
            // outcome named in the error data.
            StatusCode::OK,
            json_rpc_error(
                id,
                RESPONSE_REJECTED_CODE,
                "surface action rejected",
                Some(json!({ "outcome": outcome_name(&outcome) })),
            ),
        ),
        Err(message) => json_rpc_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json_rpc_error(id, ROUTING_FAILED_CODE, &message, None),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[test]
    fn params_require_identity_and_action() {
        assert!(parse_params(None).is_err());
        assert!(parse_params(Some(json!({
            "surface_id": "s-1", "publish_seq": 7,
            "response": {"action": "deploy", "data": {}}
        })))
        .is_ok());
        assert!(parse_params(Some(json!({
            "surface_id": "s-1", "publish_seq": 7,
            "response": {"action": "", "data": {}}
        })))
        .is_err());
        assert!(parse_params(Some(json!({
            "surface_id": "s-1", "publish_seq": 0,
            "response": {"action": "deploy", "data": {}}
        })))
        .is_err());
        // An undeclared extra field is a client bug, not a silent ignore.
        assert!(parse_params(Some(json!({
            "surface_id": "s-1", "publish_seq": 7,
            "response": {"action": "deploy", "data": {}},
            "forged_fence_token": 1
        })))
        .is_err());
    }

    #[test]
    fn params_accept_optional_user_message() {
        let submission = parse_params(Some(json!({
            "surface_id": "s-1", "publish_seq": 7,
            "response": {"action": "discuss", "data": {}},
            "user_message": "tell me more"
        })))
        .unwrap();
        assert_eq!(submission.user_message.as_deref(), Some("tell me more"));
    }

    async fn body(response: crate::ag_ui::AppResponse) -> Value {
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }

    #[tokio::test]
    async fn response_maps_every_surface_outcome_to_wire_status_and_code() {
        use SurfaceActionOutcome::{
            AlreadyApplied, AppendFailed, Applied, Conflicting, InvalidData, InvalidTool,
            InvalidVisibility, LeaseLost, Pending, ToolFailed, UnauthorizedTool, UndeclaredAction,
            UnknownSurface,
        };

        let cases = [
            (Applied, "applied", true),
            (AlreadyApplied, "already_applied", true),
            (Conflicting, "conflicting", false),
            (UnknownSurface, "unknown_surface", false),
            (UndeclaredAction, "undeclared_action", false),
            (InvalidVisibility, "invalid_visibility", false),
            (InvalidTool, "invalid_tool", false),
            (InvalidData, "invalid_data", false),
            (UnauthorizedTool, "unauthorized_tool", false),
            (ToolFailed, "tool_failed", false),
            (LeaseLost, "lease_lost", false),
            (AppendFailed, "append_failed", false),
            (Pending, "pending", false),
        ];
        for (outcome, expected_name, applied) in cases {
            assert_eq!(outcome_name(&outcome), expected_name);
            let wire = response(json!(41), Ok(outcome)).unwrap();
            assert_eq!(wire.status(), StatusCode::OK);
            let value = body(wire).await;
            if applied {
                assert_eq!(value["id"], 41);
                assert_eq!(
                    value["result"],
                    json!({
                        "applied": true,
                        "outcome": expected_name,
                    })
                );
                assert!(value.get("error").is_none());
            } else {
                assert_eq!(value["error"]["code"], RESPONSE_REJECTED_CODE);
                assert_eq!(value["error"]["data"]["outcome"], expected_name);
                assert!(value.get("result").is_none());
            }
        }

        let wire = response(json!(42), Err("actor unavailable".into())).unwrap();
        assert_eq!(wire.status(), StatusCode::SERVICE_UNAVAILABLE);
        let value = body(wire).await;
        assert_eq!(value["id"], 42);
        assert_eq!(value["error"]["code"], ROUTING_FAILED_CODE);
        assert_eq!(value["error"]["message"], "actor unavailable");
    }

    #[tokio::test]
    async fn route_forwards_surface_action_submission_over_session_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let handle = SessionHandle { tx, actor_id: 1 };
        let task = tokio::spawn(async move {
            let SessionCommand::SurfaceAction { submission, reply } = rx.recv().await.unwrap()
            else {
                panic!("expected SurfaceAction command");
            };
            assert_eq!(submission.surface_id, "surface-9");
            assert_eq!(submission.publish_seq, 12);
            assert_eq!(submission.response.action, "deploy");
            assert_eq!(submission.response.data["env"], "prod");
            assert_eq!(submission.response.note.as_deref(), Some("response note"));
            assert_eq!(submission.user_message.as_deref(), Some("model message"));
            reply.send(Ok(SurfaceActionOutcome::Applied)).unwrap();
        });

        let result = route(
            &handle,
            SurfaceActionSubmission {
                surface_id: "surface-9".into(),
                publish_seq: 12,
                response: harnx_core::interaction::InteractionResponse {
                    action: "deploy".into(),
                    data: json!({"env": "prod"}).as_object().unwrap().clone(),
                    note: Some("response note".into()),
                },
                user_message: Some("model message".into()),
            },
        )
        .await;
        assert_eq!(result, Ok(SurfaceActionOutcome::Applied));
        task.await.unwrap();
    }
}
