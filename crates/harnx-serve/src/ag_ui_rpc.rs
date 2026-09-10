use crate::ag_ui::AppResponse;
use crate::interrupt_resume::{parse_resume_params, InterruptResumeParam};
use crate::load_nats_session;
use crate::session_actor::{
    PromptResult, SessionCommand, SessionHandle, SessionInfo, SessionKey, SessionPromptOptions,
    SessionRegistry, SessionState,
};
use bytes::Bytes;
use http::{Method, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request};
use serde::Deserialize;
use serde_json::{json, Value};

pub const JSON_RPC_UNKNOWN_SESSION_CODE: i64 = -32001;
pub const JSON_RPC_IDLE_CANCEL_CODE: i64 = -32002;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceKind {
    Nats,
}

impl PersistenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nats => "nats",
        }
    }
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Value,
    #[serde(default)]
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct PromptParams {
    text: String,
    #[serde(default)]
    working_dir: Option<std::path::PathBuf>,
    #[serde(default)]
    attachment_refs: Vec<String>,
    #[serde(default)]
    resume: Vec<InterruptResumeParam>,
}

#[derive(Debug, Deserialize)]
struct HitlDecisionParams {
    tool_call_id: String,
    approved: bool,
    #[serde(default)]
    note: Option<String>,
}

pub async fn handle_ag_ui_rpc(
    req: Request<Incoming>,
    agent: &str,
    session: &str,
    config: &harnx_runtime::config::Config,
    registry: &SessionRegistry,
    persistence: PersistenceKind,
) -> anyhow::Result<AppResponse> {
    let (parts, body) = req.into_parts();
    handle_ag_ui_rpc_bytes(
        parts.method,
        agent,
        session,
        body.collect().await?.to_bytes(),
        config,
        registry,
        persistence,
    )
    .await
}

pub async fn handle_ag_ui_rpc_bytes(
    method: Method,
    agent: &str,
    session: &str,
    req_body: Bytes,
    config: &harnx_runtime::config::Config,
    registry: &SessionRegistry,
    persistence: PersistenceKind,
) -> anyhow::Result<AppResponse> {
    if method != Method::POST {
        return json_rpc_response(
            StatusCode::METHOD_NOT_ALLOWED,
            json_rpc_error(
                Value::Null,
                -32600,
                "invalid request",
                Some(json!({ "reason": "method must be POST" })),
            ),
        );
    }

    let rpc: JsonRpcRequest = match serde_json::from_slice(&req_body) {
        Ok(rpc) => rpc,
        Err(err) => {
            return json_rpc_response(
                StatusCode::BAD_REQUEST,
                json_rpc_error(
                    Value::Null,
                    -32700,
                    "parse error",
                    Some(json!({ "detail": err.to_string() })),
                ),
            );
        }
    };

    if rpc.jsonrpc != "2.0" || rpc.method.trim().is_empty() {
        return json_rpc_response(
            StatusCode::BAD_REQUEST,
            json_rpc_error(rpc.id, -32600, "invalid request", None),
        );
    }

    let key = SessionKey {
        agent: agent.to_string(),
        session: session.to_string(),
    };

    match rpc.method.as_str() {
        "session/get" => handle_get(rpc.id, config, registry, key, persistence).await,
        "session/prompt" => handle_prompt(rpc.id, rpc.params, config, registry, key).await,
        "session/hitl_decision" => {
            handle_hitl_decision(rpc.id, rpc.params, config, registry, key).await
        }
        "session/cancel" => handle_cancel((rpc.id, rpc.params), config, registry, key).await,
        _ => json_rpc_response(
            StatusCode::OK,
            json_rpc_error(rpc.id, -32601, "method not found", None),
        ),
    }
}

async fn handle_get(
    id: Value,
    config: &harnx_runtime::config::Config,
    registry: &SessionRegistry,
    key: SessionKey,
    persistence: PersistenceKind,
) -> anyhow::Result<AppResponse> {
    if !session_exists(config, &key).await && !registry.has_session(&key) {
        return json_rpc_response(
            StatusCode::NOT_FOUND,
            json_rpc_error(
                id,
                JSON_RPC_UNKNOWN_SESSION_CODE,
                "session not found",
                Some(json!({ "agent": key.agent, "session": key.session })),
            ),
        );
    }

    let handle = registry.get_or_spawn(key);
    let info = match get_info(&handle).await {
        Ok(info) => info,
        Err(message) => {
            return json_rpc_response(
                StatusCode::SERVICE_UNAVAILABLE,
                json_rpc_error(id, -32003, &message, None),
            );
        }
    };
    json_rpc_response(
        StatusCode::OK,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "state": session_state_json(&info.state),
                "execution_id": info.execution_id,
                "execution_state": info.execution_state,
                "canPrompt": info.capabilities.can_prompt,
                "canCancel": info.capabilities.can_cancel,
                "history_snapshot": info.history_snapshot,
                "history_warnings": info.history_warnings,
                "capabilities": {
                    "multiClient": true,
                    "persistence": persistence.as_str(),
                }
            }
        }),
    )
}

async fn handle_prompt(
    id: Value,
    params: Option<Value>,
    config: &harnx_runtime::config::Config,
    registry: &SessionRegistry,
    key: SessionKey,
) -> anyhow::Result<AppResponse> {
    let params: PromptParams = match params {
        Some(value) => match serde_json::from_value(value) {
            Ok(params) => params,
            Err(_) => {
                return json_rpc_response(
                    StatusCode::BAD_REQUEST,
                    json_rpc_error(
                        id,
                        -32602,
                        "invalid params",
                        Some(json!({ "expected": { "text": "string", "working_dir": "string?" } })),
                    ),
                );
            }
        },
        None => {
            return json_rpc_response(
                StatusCode::BAD_REQUEST,
                json_rpc_error(
                    id,
                    -32602,
                    "invalid params",
                    Some(json!({ "expected": { "text": "string" } })),
                ),
            );
        }
    };

    if params.text.trim().is_empty() && params.resume.is_empty() {
        return json_rpc_response(
            StatusCode::BAD_REQUEST,
            json_rpc_error(
                id,
                -32602,
                "invalid params",
                Some(json!({ "expected": { "text": "non-empty string" } })),
            ),
        );
    }

    if !registry.has_session(&key) && !session_exists(config, &key).await {
        return json_rpc_response(
            StatusCode::NOT_FOUND,
            json_rpc_error(
                id,
                JSON_RPC_UNKNOWN_SESSION_CODE,
                "session not found",
                Some(json!({ "agent": key.agent, "session": key.session })),
            ),
        );
    }

    let handle = registry.get_or_spawn(key.clone());
    let resume = match parse_resume_params(&params.resume) {
        Ok(resume) => resume,
        Err(err) => {
            return json_rpc_response(
                StatusCode::BAD_REQUEST,
                json_rpc_error(
                    id,
                    -32602,
                    "invalid params",
                    Some(json!({ "detail": err.to_string() })),
                ),
            );
        }
    };

    let resume_applied = if resume.is_empty() {
        None
    } else {
        let mut applied = false;
        for decision in resume {
            match route_hitl_decision(
                &handle,
                decision.interrupt_id,
                matches!(
                    decision.status,
                    crate::interrupt_resume::InterruptResumeStatus::Approved
                ),
                decision.payload.reason,
            )
            .await
            {
                Ok(decision_applied) => applied |= decision_applied,
                Err(message) => {
                    return json_rpc_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        json_rpc_error(id, -32003, &message, None),
                    );
                }
            }
        }
        Some(applied)
    };

    if params.text.trim().is_empty() {
        return json_rpc_response(
            StatusCode::OK,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "status": "accepted", "applied": resume_applied.unwrap_or(false) }
            }),
        );
    }
    submit_prompt(id, &handle, &params, resume_applied).await
}

async fn submit_prompt(
    id: Value,
    handle: &SessionHandle,
    params: &PromptParams,
    resume_applied: Option<bool>,
) -> anyhow::Result<AppResponse> {
    let result = match prompt(
        handle,
        &params.text,
        SessionPromptOptions {
            working_dir: params.working_dir.clone(),
            attachment_refs: params.attachment_refs.clone(),
            ..Default::default()
        },
    )
    .await
    {
        Ok(result) => result,
        Err(message) => {
            return json_rpc_response(
                StatusCode::SERVICE_UNAVAILABLE,
                json_rpc_error(id, -32003, &message, None),
            );
        }
    };
    let mut result_json = match result {
        PromptResult::Accepted { run_id } => json!({ "status": "accepted", "run_id": run_id }),
        PromptResult::Enqueued { run_id } => json!({ "status": "enqueued", "run_id": run_id }),
        PromptResult::Rejected { reason } => {
            return json_rpc_response(
                StatusCode::CONFLICT,
                json_rpc_error(id, -32004, &reason, None),
            );
        }
    };
    if let Some(applied) = resume_applied {
        result_json["applied"] = json!(applied);
    }
    json_rpc_response(
        StatusCode::OK,
        json!({ "jsonrpc": "2.0", "id": id, "result": result_json }),
    )
}

async fn handle_hitl_decision(
    id: Value,
    params: Option<Value>,
    config: &harnx_runtime::config::Config,
    registry: &SessionRegistry,
    key: SessionKey,
) -> anyhow::Result<AppResponse> {
    let params: HitlDecisionParams = match params
        .and_then(|value| serde_json::from_value(value).ok())
        .filter(|params: &HitlDecisionParams| !params.tool_call_id.trim().is_empty())
    {
        Some(params) => params,
        None => {
            return json_rpc_response(
                StatusCode::BAD_REQUEST,
                json_rpc_error(
                    id,
                    -32602,
                    "invalid params",
                    Some(json!({
                        "expected": {
                            "tool_call_id": "non-empty string",
                            "approved": "boolean",
                            "note": "optional string"
                        }
                    })),
                ),
            );
        }
    };
    if !registry.has_session(&key) && !session_exists(config, &key).await {
        return json_rpc_response(
            StatusCode::NOT_FOUND,
            json_rpc_error(id, JSON_RPC_UNKNOWN_SESSION_CODE, "session not found", None),
        );
    }
    let handle = registry.get_or_spawn(key);
    match route_hitl_decision(&handle, params.tool_call_id, params.approved, params.note).await {
        Ok(applied) => json_rpc_response(
            StatusCode::OK,
            json!({ "jsonrpc": "2.0", "id": id, "result": { "applied": applied } }),
        ),
        Err(message) => json_rpc_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json_rpc_error(id, -32003, &message, None),
        ),
    }
}

pub(crate) async fn route_hitl_decision(
    handle: &SessionHandle,
    tool_call_id: String,
    approved: bool,
    note: Option<String>,
) -> Result<bool, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::HitlApprovalDecision {
            tool_call_id,
            approved,
            note,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "session actor unavailable".to_string())?;
    reply_rx
        .await
        .map_err(|_| "session actor unavailable".to_string())?
}

async fn handle_cancel(
    request: (Value, Option<Value>),
    config: &harnx_runtime::config::Config,
    registry: &SessionRegistry,
    key: SessionKey,
) -> anyhow::Result<AppResponse> {
    let (id, params) = request;
    if !registry.has_session(&key) && !session_exists(config, &key).await {
        return json_rpc_response(
            StatusCode::NOT_FOUND,
            json_rpc_error(
                id,
                JSON_RPC_UNKNOWN_SESSION_CODE,
                "session not found",
                Some(json!({ "agent": key.agent, "session": key.session })),
            ),
        );
    }

    let handle = registry.get_or_spawn(key);
    let request: harnx_execution_control::CancelRequest =
        match serde_json::from_value(params.unwrap_or_else(|| json!({}))) {
            Ok(request) => request,
            Err(error) => {
                return json_rpc_response(
                    StatusCode::BAD_REQUEST,
                    json_rpc_error(id, -32602, &error.to_string(), None),
                )
            }
        };
    match cancel(&handle, request.expected_execution_id).await {
        Ok(receipt) => json_rpc_response(
            StatusCode::OK,
            json!({ "jsonrpc": "2.0", "id": id, "result": receipt }),
        ),
        Err(message) => json_rpc_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json_rpc_error(id, -32003, &message, None),
        ),
    }
}

async fn session_exists(config: &harnx_runtime::config::Config, key: &SessionKey) -> bool {
    match load_nats_session(config, &key.session).await {
        Ok((session, _entries)) => {
            return session.agent_name.as_deref() == Some(key.agent.as_str())
        }
        Err(error) if error.to_string() == "Not Found" => {}
        Err(_) => return false,
    }
    crate::session_routes::canonical_agent_session_exists(
        config,
        crate::session_routes::AgentSessionRef {
            agent: &key.agent,
            session: &key.session,
        },
    )
    .await
    .unwrap_or(false)
}

fn session_state_json(state: &SessionState) -> Value {
    match state {
        SessionState::Idle => json!({ "status": "idle" }),
        SessionState::Cancelling(receipt) => {
            json!({ "status": "cancelling", "cancellation": receipt })
        }
        SessionState::CancelUnconfirmed(receipt) => {
            json!({ "status": "cancel_unconfirmed", "cancellation": receipt })
        }
        SessionState::Running { run_id, started_at } => json!({
            "status": "running",
            "run_id": run_id,
            "started_at": started_at,
        }),
        SessionState::Interrupted { pending } => json!({
            "status": "interrupted",
            "pending_interrupts": pending.metadata,
        }),
    }
}

async fn prompt(
    handle: &SessionHandle,
    text: &str,
    options: SessionPromptOptions,
) -> Result<PromptResult, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::Prompt {
            text: text.to_string(),
            options,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "session actor unavailable".to_string())?;
    reply_rx
        .await
        .map_err(|_| "session actor dropped prompt reply".to_string())
}

async fn cancel(
    handle: &SessionHandle,
    expected_execution_id: Option<String>,
) -> Result<harnx_execution_control::CancelReceipt, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::Cancel {
            reply: reply_tx,
            expected_execution_id,
        })
        .await
        .map_err(|_| "session actor unavailable".to_string())?;
    reply_rx
        .await
        .map_err(|_| "session actor dropped cancel reply".to_string())?
}

async fn get_info(handle: &SessionHandle) -> Result<SessionInfo, String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::Get { reply: reply_tx })
        .await
        .map_err(|_| "session actor unavailable".to_string())?;
    reply_rx
        .await
        .map_err(|_| "session actor dropped get reply".to_string())
}

fn json_rpc_error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
            "data": data,

        }
    })
}

fn json_rpc_response(status: StatusCode, data: Value) -> anyhow::Result<AppResponse> {
    Ok(Response::builder()
        .status(status)
        .header("Content-Type", "application/json; charset=utf-8")
        .body(Full::new(Bytes::from(data.to_string())).boxed())?)
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn hitl_submit_routes_to_session_actor_worker_command() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let handle = SessionHandle { tx, actor_id: 1 };
        let receiver = tokio::spawn(async move {
            let command = rx.recv().await.expect("HITL command");
            let SessionCommand::HitlApprovalDecision {
                tool_call_id,
                approved,
                note,
                reply,
            } = command
            else {
                panic!("expected HITL decision command");
            };
            assert_eq!(tool_call_id, "tool-42");
            assert!(!approved);
            assert_eq!(note.as_deref(), Some("denied by operator"));
            reply.send(Ok(true)).expect("decision acknowledgement");
        });

        let applied = route_hitl_decision(
            &handle,
            "tool-42".to_string(),
            false,
            Some("denied by operator".to_string()),
        )
        .await
        .expect("route HITL decision");
        assert!(applied);
        receiver.await.expect("command receiver");
    }
    use super::*;
    use crate::{
        session_actor::{SessionKey, SessionRegistry},
        test_support::{wait_for_state, TestConfigSandbox},
    };
    use bytes::Bytes;
    use harnx_runtime::{client::TestStateGuard, AgentCallFn};
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::{sync::Notify, time::Duration};

    fn registry_with_call_fn(call_fn: AgentCallFn) -> SessionRegistry {
        SessionRegistry::new_for_tests(
            crate::session_actor::load_base_config_for_tests(),
            Duration::from_millis(25),
            Some(call_fn),
        )
    }

    async fn response_json(response: AppResponse) -> Value {
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        serde_json::from_slice(&body).expect("json body")
    }

    async fn seed_rpc_session(
        config: &harnx_runtime::config::Config,
        messages: &[harnx_core::message::Message],
    ) -> bool {
        crate::test_support::seed_nats_session(
            config,
            crate::test_support::NatsSessionSeed {
                agent: "plain",
                session_id: "rpc-get",
                messages,
            },
        )
        .await
    }

    #[tokio::test]
    async fn rpc_session_get_known_session_returns_state_history_and_capabilities() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");

        let call_fn: AgentCallFn = Arc::new(move |_input, _config, _abort| {
            Box::pin(async move {
                Ok((
                    "hello".to_string(),
                    None,
                    vec![],
                    harnx_runtime::client::CompletionTokenUsage::default(),
                ))
            })
        });
        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(SessionKey {
            agent: "plain".into(),
            session: "rpc-get".into(),
        });
        let _ = prompt(&handle, "seed history", SessionPromptOptions::default()).await;
        wait_for_state(&handle, "idle after seeding history", |state| {
            *state == SessionState::Idle
        })
        .await;

        let base_config = crate::session_actor::load_base_config_for_tests();
        let messages = crate::session_actor::load_test_session_messages("plain", "rpc-get");
        if !seed_rpc_session(&base_config, &messages).await {
            return;
        }

        let response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "rpc-get",
            Bytes::from(json!({"jsonrpc":"2.0","id":1,"method":"session/get"}).to_string()),
            &base_config,
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("rpc response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["capabilities"]["multiClient"], true);
        assert_eq!(body["result"]["capabilities"]["persistence"], "nats");
        assert_eq!(body["result"]["state"]["status"], "idle");
        assert_eq!(body["result"]["history_warnings"], json!([]));
        assert!(body["result"]["history_snapshot"]
            .as_array()
            .expect("history array")
            .iter()
            .any(|msg| msg["content"] == "seed history"));
    }

    #[tokio::test]
    async fn rpc_session_get_unknown_session_returns_not_found_error() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let registry = SessionRegistry::new(crate::session_actor::load_base_config_for_tests());

        let response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "never-ran",
            Bytes::from(json!({"jsonrpc":"2.0","id":"x","method":"session/get"}).to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("rpc response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], JSON_RPC_UNKNOWN_SESSION_CODE);
        assert_eq!(body["error"]["message"], "session not found");
    }

    #[tokio::test]
    async fn rpc_session_prompt_unknown_session_returns_not_found_without_spawning_actor() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let registry = SessionRegistry::new(crate::session_actor::load_base_config_for_tests());
        let key = SessionKey {
            agent: "plain".into(),
            session: "never-prompted".into(),
        };

        let response = handle_ag_ui_rpc_bytes(Method::POST, "plain", "never-prompted", Bytes::from(json!({"jsonrpc":"2.0","id":11,"method":"session/prompt","params":{"text":"hello"}}).to_string()), &crate::session_actor::load_base_config_for_tests(), &registry, PersistenceKind::Nats).await.expect("rpc response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], JSON_RPC_UNKNOWN_SESSION_CODE);
        assert_eq!(body["error"]["message"], "session not found");
        assert!(!registry.has_session(&key));
    }

    #[tokio::test]
    async fn rpc_session_cancel_unknown_session_returns_not_found_without_spawning_actor() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let registry = SessionRegistry::new(crate::session_actor::load_base_config_for_tests());
        let key = SessionKey {
            agent: "plain".into(),
            session: "never-cancelled".into(),
        };

        let response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "never-cancelled",
            Bytes::from(json!({"jsonrpc":"2.0","id":12,"method":"session/cancel"}).to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("rpc response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], JSON_RPC_UNKNOWN_SESSION_CODE);
        assert_eq!(body["error"]["message"], "session not found");
        assert!(!registry.has_session(&key));
    }

    #[tokio::test]
    async fn rpc_session_prompt_returns_ack_and_persists_effect() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");

        let call_fn: AgentCallFn = Arc::new(move |_input, _config, _abort| {
            Box::pin(async move {
                Ok((
                    "prompt reply".to_string(),
                    None,
                    vec![],
                    harnx_runtime::client::CompletionTokenUsage::default(),
                ))
            })
        });
        let registry = registry_with_call_fn(call_fn);

        let handle = registry.get_or_spawn(SessionKey {
            agent: "plain".into(),
            session: "rpc-prompt".into(),
        });

        let response = handle_ag_ui_rpc_bytes(Method::POST, "plain", "rpc-prompt", Bytes::from(json!({"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"text":"run me"}}).to_string()), &crate::session_actor::load_base_config_for_tests(), &registry, PersistenceKind::Nats).await.expect("rpc response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["result"]["status"], "accepted");
        assert!(body["result"]["run_id"].as_str().is_some());

        wait_for_state(&handle, "idle after RPC prompt", |state| {
            *state == SessionState::Idle
        })
        .await;
        let messages = crate::session_actor::load_test_session_messages("plain", "rpc-prompt");
        assert!(messages
            .iter()
            .any(|msg| msg.role.is_user() && msg.content.to_text() == "run me"));
    }

    #[tokio::test]
    async fn rpc_session_prompt_with_text_and_resume_routes_both_commands() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = crate::session_actor::load_base_config_for_tests();
        let registry = SessionRegistry::new(config.clone());
        let key = SessionKey {
            agent: "plain".into(),
            session: "rpc-prompt-resume".into(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        registry.insert_handle_for_tests(key, SessionHandle { tx, actor_id: 1 });

        let actor = tokio::spawn(async move {
            let SessionCommand::HitlApprovalDecision {
                tool_call_id,
                approved,
                note,
                reply,
            } = rx.recv().await.expect("resume decision command")
            else {
                panic!("resume decision must be routed before prompt");
            };
            assert_eq!(tool_call_id, "tool-99");
            assert!(approved);
            assert_eq!(note.as_deref(), Some("approved in test"));
            reply.send(Ok(true)).expect("decision acknowledgement");

            let SessionCommand::Prompt {
                text,
                options,
                reply,
            } = rx.recv().await.expect("prompt command")
            else {
                panic!("prompt must follow resume decision");
            };
            assert_eq!(text, "continue with this request");
            assert_eq!(options, SessionPromptOptions::default());
            reply
                .send(PromptResult::Accepted {
                    run_id: "combined-run".to_string(),
                })
                .expect("prompt acknowledgement");
        });

        let response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "rpc-prompt-resume",
            Bytes::from(
                json!({
                    "jsonrpc": "2.0",
                    "id": 8,
                    "method": "session/prompt",
                    "params": {
                        "text": "continue with this request",
                        "resume": [{
                            "interruptId": "tool-99",
                            "status": "resolved",
                            "payload": {
                                "approved": true,
                                "reason": "approved in test"
                            }
                        }]
                    }
                })
                .to_string(),
            ),
            &config,
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("combined prompt and resume response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["result"]["status"], "accepted");
        assert_eq!(body["result"]["run_id"], "combined-run");
        assert_eq!(body["result"]["applied"], true);
        actor.await.expect("mock session actor");
    }

    #[tokio::test]
    async fn rpc_session_prompt_with_text_stops_when_resume_routing_fails() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = crate::session_actor::load_base_config_for_tests();
        let registry = SessionRegistry::new(config.clone());
        let key = SessionKey {
            agent: "plain".into(),
            session: "rpc-prompt-resume-failure".into(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        registry.insert_handle_for_tests(key, SessionHandle { tx, actor_id: 2 });

        let actor = tokio::spawn(async move {
            let SessionCommand::HitlApprovalDecision { reply, .. } =
                rx.recv().await.expect("resume decision command")
            else {
                panic!("resume decision must be routed before prompt");
            };
            reply
                .send(Err("decision routing failed".to_string()))
                .expect("decision failure acknowledgement");
            assert!(
                tokio::time::timeout(Duration::from_millis(50), rx.recv())
                    .await
                    .is_err(),
                "prompt must not be submitted after decision failure"
            );
        });

        let response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "rpc-prompt-resume-failure",
            Bytes::from(
                json!({
                    "jsonrpc": "2.0",
                    "id": 9,
                    "method": "session/prompt",
                    "params": {
                        "text": "must not run",
                        "resume": [{
                            "interruptId": "tool-failure",
                            "status": "resolved",
                            "payload": { "approved": true }
                        }]
                    }
                })
                .to_string(),
            ),
            &config,
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("failed resume response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], -32003);
        assert_eq!(body["error"]["message"], "decision routing failed");
        actor.await.expect("mock session actor");
    }

    #[tokio::test]
    async fn rpc_session_cancel_while_running_returns_ack() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");

        let gate_ready = Arc::new(Notify::new());
        let gate_release = Arc::new(Notify::new());
        let call_fn: AgentCallFn = {
            let gate_ready = gate_ready.clone();
            let gate_release = gate_release.clone();
            Arc::new(move |_input, _config, _abort| {
                let gate_ready = gate_ready.clone();
                let gate_release = gate_release.clone();
                Box::pin(async move {
                    gate_ready.notify_one();
                    gate_release.notified().await;
                    Ok((
                        "done".to_string(),
                        None,
                        vec![],
                        harnx_runtime::client::CompletionTokenUsage::default(),
                    ))
                })
            })
        };
        let registry = registry_with_call_fn(call_fn);
        let handle = registry.get_or_spawn(SessionKey {
            agent: "plain".into(),
            session: "rpc-cancel".into(),
        });
        let _ = prompt(&handle, "cancel me", SessionPromptOptions::default()).await;
        gate_ready.notified().await;

        let response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "rpc-cancel",
            Bytes::from(json!({"jsonrpc":"2.0","id":9,"method":"session/cancel"}).to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("rpc response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["result"]["cancelled"], true);
        gate_release.notify_one();
    }

    #[tokio::test]
    async fn rpc_unknown_method_returns_json_rpc_method_not_found() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let registry = SessionRegistry::new(crate::session_actor::load_base_config_for_tests());

        let response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "whatever",
            Bytes::from(json!({"jsonrpc":"2.0","id":3,"method":"session/nope"}).to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("rpc response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn rpc_prompt_rejects_unresolvable_attachments_before_acceptance() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let registry = SessionRegistry::new(crate::session_actor::load_base_config_for_tests());

        let handle = registry.get_or_spawn(SessionKey {
            agent: "plain".into(),
            session: "attach-rpc".into(),
        });
        let _ = prompt(&handle, "seed history", SessionPromptOptions::default()).await;

        let response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "attach-rpc",
            Bytes::from(
                json!({
                    "jsonrpc": "2.0",
                    "id": 21,
                    "method": "session/prompt",
                    "params": {
                        "text": "look",
                        "attachment_refs": ["cid:abc123"]
                    }
                })
                .to_string(),
            ),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("rpc response");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], -32004);
        assert!(body["result"].is_null());
    }

    #[tokio::test]
    async fn rpc_malformed_json_and_invalid_request_return_json_rpc_errors() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let registry = SessionRegistry::new(crate::session_actor::load_base_config_for_tests());

        let parse_response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "oops",
            Bytes::from("{not json".to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("parse response");
        assert_eq!(parse_response.status(), StatusCode::BAD_REQUEST);
        let parse_body = response_json(parse_response).await;
        assert_eq!(parse_body["error"]["code"], -32700);

        let invalid_response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "oops",
            Bytes::from(json!({"jsonrpc":"2.0","id":4}).to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("invalid response");
        assert_eq!(invalid_response.status(), StatusCode::BAD_REQUEST);
        let invalid_body = response_json(invalid_response).await;
        assert_eq!(invalid_body["error"]["code"], -32600);
    }
}

#[cfg(test)]
mod extra_rpc_tests {
    use super::*;
    use crate::{session_actor::load_base_config_for_tests, test_support::TestConfigSandbox};
    use bytes::Bytes;
    use harnx_runtime::client::TestStateGuard;
    use http_body_util::BodyExt;
    use serde_json::{json, Value};

    async fn response_json(response: AppResponse) -> Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("collect rpc body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("parse rpc json")
    }

    #[tokio::test]
    async fn rpc_cancel_idle_missing_params_wrong_text_type_and_id_shapes() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = load_base_config_for_tests();
        let registry = SessionRegistry::new(config.clone());

        let handle = registry.get_or_spawn(SessionKey {
            agent: "plain".into(),
            session: "idle-cancel".into(),
        });
        let _ = get_info(&handle).await.expect("seed idle session");

        let idle_cancel = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "idle-cancel",
            Bytes::from(json!({"jsonrpc":"2.0","id":"idle","method":"session/cancel"}).to_string()),
            &config,
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("idle cancel response");
        assert_eq!(idle_cancel.status(), StatusCode::OK);
        let idle_body = response_json(idle_cancel).await;
        assert_eq!(idle_body["id"], "idle");
        assert_eq!(idle_body["result"]["disposition"], "idle");
        assert_eq!(idle_body["result"]["cancelled"], false);

        let missing_params = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "idle-cancel",
            Bytes::from(json!({"jsonrpc":"2.0","id":null,"method":"session/prompt"}).to_string()),
            &config,
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("missing params response");
        let missing_body = response_json(missing_params).await;
        assert_eq!(missing_body["id"], Value::Null);
        assert_eq!(missing_body["error"]["code"], -32602);

        let wrong_text_type = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "idle-cancel",
            Bytes::from(
                json!({
                    "jsonrpc":"2.0",
                    "id":{"kind":"object"},
                    "method":"session/prompt",
                    "params":{"text":42}
                })
                .to_string(),
            ),
            &config,
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("wrong text response");
        let wrong_body = response_json(wrong_text_type).await;
        assert_eq!(wrong_body["id"], json!({"kind":"object"}));
        assert_eq!(wrong_body["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn rpc_batch_and_notification_are_pinned_as_invalid_requests() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = load_base_config_for_tests();
        let registry = SessionRegistry::new(config.clone());

        // Current behavior: top-level arrays and notification-style bodies fail request deserialization
        // first, so handler returns JSON-RPC parse errors instead of batch/notification semantics.
        let batch = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "batchish",
            Bytes::from(
                json!([
                    {"jsonrpc":"2.0","id":1,"method":"session/get"},
                    {"jsonrpc":"2.0","method":"session/get"}
                ])
                .to_string(),
            ),
            &config,
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("batch response");
        assert_eq!(batch.status(), StatusCode::BAD_REQUEST);
        let batch_body = response_json(batch).await;
        assert_eq!(batch_body["error"]["code"], -32700);

        let notification = handle_ag_ui_rpc_bytes(
            Method::POST,
            "plain",
            "notify",
            Bytes::from(json!({"jsonrpc":"2.0","method":"session/get"}).to_string()),
            &config,
            &registry,
            PersistenceKind::Nats,
        )
        .await
        .expect("notification response");
        let notification_body = response_json(notification).await;
        assert_eq!(notification_body["id"], Value::Null);
        assert_eq!(notification_body["error"]["code"], -32700);
    }
}
