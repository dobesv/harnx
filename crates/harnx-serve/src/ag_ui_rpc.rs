use crate::ag_ui::AppResponse;
use crate::agent_scoped_config;
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
    Filesystem,
    Nats,
}

impl PersistenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Filesystem => "filesystem",
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
}

pub async fn handle_ag_ui_rpc(
    req: Request<Incoming>,
    config: &harnx_runtime::config::Config,
    registry: &SessionRegistry,
    persistence: PersistenceKind,
) -> anyhow::Result<AppResponse> {
    let (parts, body) = req.into_parts();
    handle_ag_ui_rpc_bytes(
        parts.method,
        parts.uri.path().to_string(),
        body.collect().await?.to_bytes(),
        config,
        registry,
        persistence,
    )
    .await
}

pub async fn handle_ag_ui_rpc_bytes(
    method: Method,
    path: String,
    req_body: Bytes,
    config: &harnx_runtime::config::Config,
    registry: &SessionRegistry,
    persistence: PersistenceKind,
) -> anyhow::Result<AppResponse> {
    let (agent, session) = parse_rpc_path(&path).ok_or_else(|| anyhow::anyhow!("Not Found"))?;

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
        "session/cancel" => handle_cancel(rpc.id, config, registry, key).await,
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
    if !session_exists(config, &key) && !registry.has_session(&key) {
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
                "history_snapshot": info.history_snapshot,
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

    if params.text.trim().is_empty() {
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

    if !registry.has_session(&key) && !session_exists(config, &key) {
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
    let result = match prompt(
        &handle,
        &params.text,
        SessionPromptOptions {
            working_dir: params.working_dir.clone(),
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
    let result_json = match result {
        PromptResult::Accepted { run_id } => json!({ "status": "accepted", "run_id": run_id }),
        PromptResult::Enqueued { run_id } => json!({ "status": "enqueued", "run_id": run_id }),
    };
    json_rpc_response(
        StatusCode::OK,
        json!({ "jsonrpc": "2.0", "id": id, "result": result_json }),
    )
}

async fn handle_cancel(
    id: Value,
    config: &harnx_runtime::config::Config,
    registry: &SessionRegistry,
    key: SessionKey,
) -> anyhow::Result<AppResponse> {
    if !registry.has_session(&key) && !session_exists(config, &key) {
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
    if matches!(info.state, SessionState::Idle) {
        return json_rpc_response(
            StatusCode::BAD_REQUEST,
            json_rpc_error(
                id,
                JSON_RPC_IDLE_CANCEL_CODE,
                "session is not running",
                None,
            ),
        );
    }

    if let Err(message) = cancel(&handle).await {
        return json_rpc_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json_rpc_error(id, -32003, &message, None),
        );
    }
    json_rpc_response(
        StatusCode::OK,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "cancelled": true }
        }),
    )
}

fn parse_rpc_path(path: &str) -> Option<(&str, &str)> {
    let suffix = path.strip_prefix("/v1/agents/")?;
    let mut segments = suffix.split('/');
    let agent = segments.next()?;
    if segments.next()? != "sessions" {
        return None;
    }
    let session = segments.next()?;
    if segments.next()? != "rpc" {
        return None;
    }
    if segments.next().is_some() {
        return None;
    }
    Some((agent, session))
}

fn session_exists(config: &harnx_runtime::config::Config, key: &SessionKey) -> bool {
    let Ok(config) = agent_scoped_config(config, &key.agent) else {
        return false;
    };
    let session_path = config.session_file(&key.session);
    harnx_runtime::config::session::load(&config, &key.session, &session_path).is_ok()
}

fn session_state_json(state: &SessionState) -> Value {
    match state {
        SessionState::Idle => json!({ "status": "idle" }),
        SessionState::Running { run_id, started_at } => json!({
            "status": "running",
            "run_id": run_id,
            "started_at": started_at,
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

async fn cancel(handle: &SessionHandle) -> Result<(), String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::Cancel { reply: reply_tx })
        .await
        .map_err(|_| "session actor unavailable".to_string())?;
    reply_rx
        .await
        .map_err(|_| "session actor dropped cancel reply".to_string())
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
    use super::*;
    use crate::session_actor::{SessionKey, SessionRegistry};
    use bytes::Bytes;
    use harnx_runtime::{client::TestStateGuard, AgentCallFn};
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::{
        fs,
        path::PathBuf,
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::{
        sync::Notify,
        time::{sleep, Duration},
    };

    struct TestConfigSandbox {
        _lock: std::sync::MutexGuard<'static, ()>,
        root: PathBuf,
        vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl TestConfigSandbox {
        fn new() -> Self {
            let lock = crate::session_actor::TEST_CONFIG_DIR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = unique_test_config_dir();
            fs::create_dir_all(root.join("clients")).expect("create clients dir");
            fs::create_dir_all(root.join("agents")).expect("create agents dir");
            fs::create_dir_all(root.join("data")).expect("create data dir");
            fs::create_dir_all(root.join("state")).expect("create state dir");
            fs::write(
                root.join("config.yaml"),
                "model: openai:gpt-4o
save_session: true
",
            )
            .expect("write config");
            fs::write(
                root.join("clients/openai.yaml"),
                "type: openai-compatible
api_base: https://example.invalid/v1
api_key: test-key
models:
  - name: gpt-4o
",
            )
            .expect("write client cfg");
            let vars = set_test_env_vars(&root);
            Self {
                _lock: lock,
                root,
                vars,
            }
        }

        fn write_agent(&self, name: &str, prompt: &str) {
            let body = format!("---\nmodel: openai:gpt-4o\n---\n{prompt}\n");
            fs::write(self.root.join("agents").join(format!("{name}.md")), body)
                .expect("write agent prompt");
        }
    }

    impl Drop for TestConfigSandbox {
        fn drop(&mut self) {
            for (key, prev) in self.vars.iter().rev() {
                match prev {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn unique_test_config_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        std::env::temp_dir().join(format!("harnx-serve-rpc-tests-{nanos}"))
    }

    fn set_test_env_vars(
        root: &std::path::Path,
    ) -> Vec<(&'static str, Option<std::ffi::OsString>)> {
        let vars = [
            ("HARNX_CONFIG_DIR", root.as_os_str().to_os_string()),
            ("HARNX_DATA_DIR", root.join("data").into_os_string()),
            ("HARNX_STATE_DIR", root.join("state").into_os_string()),
        ];
        let mut prev = Vec::new();
        for (key, value) in vars {
            prev.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        prev
    }

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
        sleep(Duration::from_millis(80)).await;

        let response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "/v1/agents/plain/sessions/rpc-get/rpc".to_string(),
            Bytes::from(json!({"jsonrpc":"2.0","id":1,"method":"session/get"}).to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Filesystem,
        )
        .await
        .expect("rpc response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["capabilities"]["multiClient"], true);
        assert_eq!(body["result"]["capabilities"]["persistence"], "filesystem");
        assert_eq!(body["result"]["state"]["status"], "idle");
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
            "/v1/agents/plain/sessions/never-ran/rpc".to_string(),
            Bytes::from(json!({"jsonrpc":"2.0","id":"x","method":"session/get"}).to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Filesystem,
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

        let response = handle_ag_ui_rpc_bytes(Method::POST, "/v1/agents/plain/sessions/never-prompted/rpc".to_string(), Bytes::from(json!({"jsonrpc":"2.0","id":11,"method":"session/prompt","params":{"text":"hello"}}).to_string()), &crate::session_actor::load_base_config_for_tests(), &registry, PersistenceKind::Filesystem).await.expect("rpc response");
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
            "/v1/agents/plain/sessions/never-cancelled/rpc".to_string(),
            Bytes::from(json!({"jsonrpc":"2.0","id":12,"method":"session/cancel"}).to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Filesystem,
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

        // Seed session the simple way (not via SSE stream drain)
        let handle = registry.get_or_spawn(SessionKey {
            agent: "plain".into(),
            session: "rpc-prompt".into(),
        });
        let _ = prompt(&handle, "seed history", SessionPromptOptions::default()).await;
        sleep(Duration::from_millis(80)).await;

        let response = handle_ag_ui_rpc_bytes(Method::POST, "/v1/agents/plain/sessions/rpc-prompt/rpc".to_string(), Bytes::from(json!({"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"text":"run me"}}).to_string()), &crate::session_actor::load_base_config_for_tests(), &registry, PersistenceKind::Filesystem).await.expect("rpc response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["result"]["status"], "accepted");
        assert!(body["result"]["run_id"].as_str().is_some());

        sleep(Duration::from_millis(80)).await;
        let mut config = crate::session_actor::load_base_config_for_tests();
        config.use_agent_by_name("plain").expect("set agent");
        config.use_session(Some("rpc-prompt")).expect("set session");
        let messages = config.session.expect("session exists").messages;
        assert!(messages
            .iter()
            .any(|msg| msg.role.is_user() && msg.content.to_text() == "run me"));
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
            "/v1/agents/plain/sessions/rpc-cancel/rpc".to_string(),
            Bytes::from(json!({"jsonrpc":"2.0","id":9,"method":"session/cancel"}).to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Filesystem,
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
            "/v1/agents/plain/sessions/whatever/rpc".to_string(),
            Bytes::from(json!({"jsonrpc":"2.0","id":3,"method":"session/nope"}).to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Filesystem,
        )
        .await
        .expect("rpc response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn rpc_malformed_json_and_invalid_request_return_json_rpc_errors() {
        let _guard = TestStateGuard::new(None).await;
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let registry = SessionRegistry::new(crate::session_actor::load_base_config_for_tests());

        let parse_response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "/v1/agents/plain/sessions/oops/rpc".to_string(),
            Bytes::from("{not json".to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Filesystem,
        )
        .await
        .expect("parse response");
        assert_eq!(parse_response.status(), StatusCode::BAD_REQUEST);
        let parse_body = response_json(parse_response).await;
        assert_eq!(parse_body["error"]["code"], -32700);

        let invalid_response = handle_ag_ui_rpc_bytes(
            Method::POST,
            "/v1/agents/plain/sessions/oops/rpc".to_string(),
            Bytes::from(json!({"jsonrpc":"2.0","id":4}).to_string()),
            &crate::session_actor::load_base_config_for_tests(),
            &registry,
            PersistenceKind::Filesystem,
        )
        .await
        .expect("invalid response");
        assert_eq!(invalid_response.status(), StatusCode::BAD_REQUEST);
        let invalid_body = response_json(invalid_response).await;
        assert_eq!(invalid_body["error"]["code"], -32600);
    }
}
