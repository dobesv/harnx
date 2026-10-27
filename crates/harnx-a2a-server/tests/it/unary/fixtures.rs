use crate::support::{Harness, Script, DEADLINE};
use anyhow::{Context, Result};
use harnx_a2a_server::{
    handler::{Backend, BackendConfig, HarnxHandler},
    input_map::InputLimits,
    routes,
};
use harnx_runtime::SessionActivationRoute;
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) struct HttpRpcCall<'a> {
    pub(crate) path: &'a str,
    pub(crate) user: &'a str,
    pub(crate) method: &'a str,
    pub(crate) params: Value,
}

impl<'a> HttpRpcCall<'a> {
    pub(crate) fn new(path: &'a str, user: &'a str, method: &'a str, params: Value) -> Self {
        Self {
            path,
            user,
            method,
            params,
        }
    }
}

pub(crate) struct Http {
    pub(crate) h: Harness,
    pub(crate) url: String,
    pub(crate) client: reqwest::Client,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Http {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Http {
    pub(crate) async fn start() -> Result<Self> {
        Self::from_harness(Harness::start(Script::Text).await?).await
    }
    pub(crate) async fn from_harness(h: Harness) -> Result<Self> {
        Self::with_access_rules(h, None).await
    }
    pub(crate) async fn with_access_rules(
        h: Harness,
        rules: Option<Arc<harnx_core::access_rules::AccessRules>>,
    ) -> Result<Self> {
        Self::with_identity(
            h,
            rules,
            harnx_a2a_server::identity::Identity::new(&["X-User-ID".into()])?,
        )
        .await
    }
    pub(crate) async fn with_identity(
        h: Harness,
        rules: Option<Arc<harnx_core::access_rules::AccessRules>>,
        identity: harnx_a2a_server::identity::Identity,
    ) -> Result<Self> {
        let backend = Arc::new(Backend::new(
            h.runner.clone(),
            h.store.clone(),
            BackendConfig {
                config: h.config.clone(),
                route: SessionActivationRoute::ClusterShared,
                abort: harnx_core::abort::create_abort_signal(),
            },
        ));
        let mut exports = vec![h.export.clone(), h.export.clone()];
        exports[0].lookup_keys = vec![
            "runner".into(),
            "alias".into(),
            h.export.public_name.clone(),
        ];
        exports[1].public_name = "other".into();
        exports[1].agent = "target".into();
        exports[1].lookup_keys = vec!["other".into()];
        let app = routes::router_with_access_rules(
            &exports,
            None,
            identity,
            rules,
            |export, identity| {
                Arc::new(HarnxHandler::new(
                    export.clone(),
                    identity,
                    backend.clone(),
                    InputLimits::default(),
                ))
            },
        )?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/agents/runner", listener.local_addr()?);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Ok(Self {
            h,
            url,
            client: reqwest::Client::builder().timeout(DEADLINE).build()?,
            server,
        })
    }
    pub(crate) async fn rpc_call(&self, call: HttpRpcCall<'_>) -> Result<Value> {
        let response = self
            .client
            .post(call.path)
            .header("a2a-version", "1.0")
            .header("X-User-ID", call.user)
            .json(&json!({"jsonrpc":"2.0","id":"1","method":call.method,"params":call.params}))
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        Ok(response.json().await?)
    }
    pub(crate) async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        self.rpc_call(HttpRpcCall::new(&self.url, "alice", method, params))
            .await
    }
    pub(crate) async fn working(&self, message: Value) -> Result<Value> {
        let response = self
            .rpc(
                "SendMessage",
                json!({"message":message,"configuration":{"returnImmediately":true}}),
            )
            .await?;
        assert_eq!(
            response["result"]["task"]["status"]["state"], "TASK_STATE_WORKING",
            "{response}"
        );
        Ok(response["result"]["task"].clone())
    }
    pub(crate) async fn completed(&self, id: &Value) -> Result<Value> {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let response = self.rpc("GetTask", json!({"id":id})).await?;
                if response["result"]["status"]["state"] == "TASK_STATE_COMPLETED" {
                    return Ok::<_, anyhow::Error>(response);
                }
                anyhow::ensure!(
                    response["error"].is_null()
                        && response["result"]["status"]["state"] == "TASK_STATE_WORKING",
                    "unexpected state: {response}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .context("completion deadline")?
    }
}

pub(crate) fn message(id: &str, context: Option<&Value>) -> Value {
    let mut message = json!({"messageId":id,"role":"ROLE_USER","parts":[{"text":"Hello, agent"}]});
    if let Some(context) = context {
        message["contextId"] = context.clone();
    }
    message
}

pub(crate) fn code(response: &Value, expected: i32) {
    assert_eq!(response["error"]["code"], expected, "{response}");
    assert!(response.get("result").is_none());
}

pub(crate) fn fixture(name: &str) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
    .unwrap()
}

pub(crate) fn golden(actual: &Value, expected: &Value, key: &str) {
    match expected {
        Value::Object(fields) => golden_fields(actual, fields),
        Value::Array(values) => golden_array(actual, values, key),
        Value::String(_)
            if matches!(
                key,
                "id" | "contextId" | "messageId" | "timestamp" | "text" | "message"
            ) =>
        {
            assert!(actual.is_string())
        }
        _ => assert_eq!(actual, expected),
    }
}

fn golden_fields(actual: &Value, fields: &serde_json::Map<String, Value>) {
    for (key, value) in fields {
        assert!(actual.get(key).is_some(), "missing {key}: {actual}");
        golden(&actual[key], value, key);
    }
}

fn golden_array(actual: &Value, values: &[Value], key: &str) {
    assert_eq!(actual.as_array().unwrap().len(), values.len());
    for (a, b) in actual.as_array().unwrap().iter().zip(values) {
        golden(a, b, key);
    }
}

pub(crate) async fn spawn_strict_limits_server(
    http: &Http,
) -> Result<(String, tokio_util::task::AbortOnDropHandle<()>)> {
    let backend = Arc::new(Backend::new(
        http.h.runner.clone(),
        http.h.store.clone(),
        BackendConfig {
            config: http.h.config.clone(),
            route: SessionActivationRoute::ClusterShared,
            abort: harnx_core::abort::create_abort_signal(),
        },
    ));
    let mut export = http.h.export.clone();
    export.lookup_keys = vec!["runner".into()];
    let app = routes::router(
        &[export],
        None,
        &["X-User-ID".into()],
        |export, identity| {
            Arc::new(HarnxHandler::new(
                export.clone(),
                identity,
                backend.clone(),
                InputLimits {
                    max_data_part_bytes: 0,
                },
            ))
        },
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/agents/runner", listener.local_addr()?);
    let handle = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    Ok((url, handle))
}

async fn commit_injected_checkpoint(
    h: &Harness,
    storage: &str,
    committed_version: &harnx_a2a_server::store::context::ContextVersion,
    ack_sequence: u64,
) -> Result<harnx_a2a_server::store::TaskRecord> {
    let write = h
        .store
        .prepare_context_update(
            storage,
            committed_version,
            &uuid::Uuid::new_v4().to_string(),
            |context| {
                let active = context.active.as_mut().unwrap();
                active.publication.pending = None;
                active.publication.subject_sequence = ack_sequence;
                active.publication.confirmed_history.push(ack_sequence);
            },
        )
        .await?;
    let checkpointed = h.store.commit_context(&write).await?;
    Ok(checkpointed.document.state.active.unwrap().snapshot)
}

async fn publish_injected_event(
    h: &Harness,
    subject: String,
    pending: &harnx_a2a_server::store::context::PendingEvent,
) -> Result<u64> {
    let ack = h
        .jetstream
        .send_publish(
            subject,
            async_nats::jetstream::message::PublishMessage::build()
                .message_id(pending.commit_id.clone())
                .expected_last_subject_sequence(pending.expected_subject_sequence)
                .payload(serde_json::to_vec(pending)?.into()),
        )
        .await?
        .await?;
    Ok(ack.sequence)
}

async fn await_active_context(
    h: &Harness,
    storage: &str,
) -> Result<harnx_a2a_server::store::context::ContextSnapshot> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let current = h.store.read_context(storage).await?.context("authority")?;
            let active = current.document.state.active.as_ref().context("active")?;
            if active.snapshot.task.artifacts.is_some() && active.publication.pending.is_none() {
                return Ok::<_, anyhow::Error>(current);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?
}

pub(crate) async fn inject_response_state(
    h: &Harness,
    id: &str,
    state: a2a_lf::TaskState,
) -> Result<harnx_a2a_server::store::TaskRecord> {
    use a2a_lf::{StreamResponse, TaskStatus, TaskStatusUpdateEvent};
    use harnx_a2a_server::store::context::PendingEvent;
    let (local_id, uuid) = harnx_a2a_server::store::parse_task_id(id)?;
    let storage = harnx_core::session_identity::session_key(Some(&h.export.agent), local_id);
    let current = await_active_context(h, &storage).await?;
    let operation = uuid::Uuid::new_v4().to_string();
    let write = h
        .store
        .prepare_context_update(&storage, &current.version()?, &operation, |context| {
            let active = context.active.as_mut().unwrap();
            active.snapshot.task.status = TaskStatus {
                state,
                message: None,
                timestamp: Some(chrono::Utc::now()),
            };
            active.snapshot.stream_seq += 1;
            active.publication.stream_seq = active.snapshot.stream_seq;
            active.publication.pending = Some(PendingEvent {
                committed_at: Some(chrono::Utc::now()),
                commit_id: operation.clone(),
                task_sequence: active.snapshot.stream_seq,
                expected_subject_sequence: active.publication.subject_sequence,
                response: StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: id.into(),
                    context_id: local_id.into(),
                    status: active.snapshot.task.status.clone(),
                    metadata: None,
                }),
            });
        })
        .await?;
    let committed = h.store.commit_context(&write).await?;
    let pending = committed
        .document
        .state
        .active
        .as_ref()
        .unwrap()
        .publication
        .pending
        .as_ref()
        .unwrap();
    let ack_sequence =
        publish_injected_event(h, format!("a2a.tasks.{storage}.{uuid}"), pending).await?;
    commit_injected_checkpoint(h, &storage, &committed.version()?, ack_sequence).await
}
