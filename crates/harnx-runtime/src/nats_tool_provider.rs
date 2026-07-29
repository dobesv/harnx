use crate::config::{Config, LOCAL_CLUSTER_KEY};
use anyhow::anyhow;
use async_trait::async_trait;
use futures_util::TryStreamExt;
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use harnx_core::instance::InstanceId;
use harnx_core::tool::{JsonSchema, ToolDeclaration, ToolError, ToolProvider};
use harnx_toolset::{
    ControlKind, ControlMessage, Registration, ToolErrorPayload, ToolReply, ToolRequest,
    HDR_CALL_ID, HDR_CONTENT_TYPE, HDR_IDEMPOTENCY_KEY, HDR_INSTANCE_ID,
};
use harnx_toolset_server::{registration_key, TOOL_REGISTRY_BUCKET};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};
use uuid::Uuid;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const JSON_CONTENT_TYPE: &str = "application/json";

#[derive(Clone, Debug)]
struct RegisteredTool {
    server: String,
    request_timeout: Duration,
}

struct PendingToolRequest {
    call_id: String,
    server: String,
    subject: String,
    request: async_nats::Request,
}

#[derive(Clone, Debug)]
enum InFlightFailure {
    Unavailable(String),
}

type InFlightMap = Mutex<HashMap<String, InFlightCall>>;
static INSTANCE_IN_FLIGHT: OnceLock<std::sync::Mutex<HashMap<InstanceId, Weak<InFlightMap>>>> =
    OnceLock::new();

/// Shared handle used by tool-process supervision to fail active NATS calls.
#[derive(Clone, Default)]
pub struct NatsInFlightCalls {
    calls: Arc<InFlightMap>,
}

struct InFlightCall {
    server: String,
    failure: oneshot::Sender<InFlightFailure>,
}

impl NatsInFlightCalls {
    /// Return the process-wide handle shared by provider and supervisor for an instance.
    pub fn for_instance(instance_id: &InstanceId) -> Self {
        let registry = INSTANCE_IN_FLIGHT.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        let mut registry = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.retain(|_, calls| calls.strong_count() > 0);
        if let Some(calls) = registry.get(instance_id).and_then(Weak::upgrade) {
            return Self { calls };
        }
        let calls = Arc::new(Mutex::new(HashMap::new()));
        registry.insert(instance_id.clone(), Arc::downgrade(&calls));
        Self { calls }
    }

    async fn register(
        &self,
        call_id: String,
        server: String,
    ) -> oneshot::Receiver<InFlightFailure> {
        let (failure, receiver) = oneshot::channel();
        self.calls
            .lock()
            .await
            .insert(call_id, InFlightCall { server, failure });
        receiver
    }

    async fn complete(&self, call_id: &str) {
        self.calls.lock().await.remove(call_id);
    }

    /// Fail current calls routed to a supervised server that became unavailable.
    pub async fn fail_server_unavailable(&self, server: &str, message: impl Into<String>) {
        let message = message.into();
        let failures = {
            let mut calls = self.calls.lock().await;
            let call_ids = calls
                .iter()
                .filter(|(_, call)| call.server == server)
                .map(|(call_id, _)| call_id.clone())
                .collect::<Vec<_>>();
            call_ids
                .into_iter()
                .filter_map(|call_id| calls.remove(&call_id).map(|call| call.failure))
                .collect::<Vec<_>>()
        };
        for failure in failures {
            let _ = failure.send(InFlightFailure::Unavailable(message.clone()));
        }
    }
}

/// Core-NATS tool provider built from one turn's KV registration snapshot.
pub struct NatsToolProvider {
    client: async_nats::Client,
    instance_id: InstanceId,
    tools: HashMap<String, RegisteredTool>,
    declarations: Vec<ToolDeclaration>,
    // Owning this subscription establishes the progress/cancel channel before requests start.
    _control_subscription: Mutex<async_nats::Subscriber>,
    in_flight: NatsInFlightCalls,
}

impl NatsToolProvider {
    /// Connect through runtime config and snapshot registered tools for this instance.
    pub async fn discover(
        config: &Config,
        instance_id: InstanceId,
        in_flight: NatsInFlightCalls,
    ) -> anyhow::Result<Self> {
        let client = config.nats_client(LOCAL_CLUSTER_KEY).await?;
        let control_subject = instance_id.control_subject();
        let control_subscription = client.subscribe(control_subject).await?;
        client.flush().await?;

        let registrations = registration_snapshot(&client, &instance_id)
            .await
            .unwrap_or_default();
        let (tools, declarations) = build_registered_tools(registrations);

        Ok(Self {
            client,
            instance_id,
            tools,
            declarations,
            _control_subscription: Mutex::new(control_subscription),
            in_flight,
        })
    }

    pub fn declarations(&self) -> &[ToolDeclaration] {
        &self.declarations
    }

    pub fn declarations_for_use_tools(&self, use_tools: Option<&str>) -> Vec<ToolDeclaration> {
        let Some(use_tools) = use_tools else {
            return Vec::new();
        };
        let selectors = harnx_core::agent_config::split_tool_selectors(use_tools);
        self.declarations
            .iter()
            .filter(|declaration| {
                let server = self
                    .tools
                    .get(&declaration.name)
                    .map(|tool| tool.server.as_str());
                selectors.iter().any(|selector| {
                    let selector = selector.trim();
                    selector == "*"
                        || selector == declaration.name
                        || server.is_some_and(|server| selector == server)
                        || globset::Glob::new(selector).is_ok_and(|pattern| {
                            let matcher = pattern.compile_matcher();
                            matcher.is_match(&declaration.name)
                                || server.is_some_and(|server| matcher.is_match(server))
                        })
                })
            })
            .cloned()
            .collect()
    }
    pub fn in_flight_calls(&self) -> NatsInFlightCalls {
        self.in_flight.clone()
    }

    async fn publish_cancel(&self, call_id: &str) -> anyhow::Result<()> {
        let control = ControlMessage {
            call_id: call_id.to_string(),
            kind: ControlKind::Cancel,
        };
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(HDR_CALL_ID, call_id);
        headers.insert(HDR_INSTANCE_ID, self.instance_id.as_str());
        headers.insert(HDR_CONTENT_TYPE, JSON_CONTENT_TYPE);
        self.client
            .publish_with_headers(
                self.instance_id.control_subject(),
                headers,
                serde_json::to_vec(&control)?.into(),
            )
            .await?;
        self.client.flush().await?;
        Ok(())
    }

    fn prepare_request(
        &self,
        tool_name: &str,
        arguments: Value,
        route: &RegisteredTool,
    ) -> Result<PendingToolRequest, ToolError> {
        let call_id = Uuid::new_v4().to_string();
        let request = ToolRequest {
            call_id: call_id.clone(),
            tool: tool_name.to_string(),
            args: arguments,
        };
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(HDR_IDEMPOTENCY_KEY, Uuid::new_v4().to_string());
        headers.insert(HDR_INSTANCE_ID, self.instance_id.as_str());
        headers.insert(HDR_CALL_ID, call_id.as_str());
        headers.insert(HDR_CONTENT_TYPE, JSON_CONTENT_TYPE);
        let payload = serde_json::to_vec(&request).map_err(|error| {
            ToolError::Fatal(anyhow!("failed to encode NATS tool request: {error}"))
        })?;
        Ok(PendingToolRequest {
            call_id,
            server: route.server.clone(),
            subject: self.instance_id.tool_subject(&route.server, tool_name),
            request: async_nats::Request::new()
                .headers(headers)
                .payload(payload.into())
                .timeout(Some(route.request_timeout)),
        })
    }

    async fn await_response(
        &self,
        pending: PendingToolRequest,
        abort: &AbortSignal,
    ) -> Result<async_nats::Message, ToolError> {
        let PendingToolRequest {
            call_id,
            server,
            subject,
            request,
        } = pending;
        let mut supervised_failure = self.in_flight.register(call_id.clone(), server).await;
        let request = self.client.send_request(subject, request);
        tokio::pin!(request);
        let response = tokio::select! {
            _ = wait_abort_signal(abort) => {
                self.in_flight.complete(&call_id).await;
                if let Err(error) = self.publish_cancel(&call_id).await {
                    return Err(ToolError::Fatal(anyhow!(
                        "tool call aborted; failed to publish cancellation: {error}"
                    )));
                }
                return Err(ToolError::Fatal(anyhow!("tool call aborted")));
            }
            failure = &mut supervised_failure => {
                self.in_flight.complete(&call_id).await;
                let message = match failure {
                    Ok(InFlightFailure::Unavailable(message)) => message,
                    Err(_) => "tool server unavailable".to_string(),
                };
                return Err(ToolError::Recoverable(anyhow!(message)));
            }
            response = &mut request => response,
        };
        self.in_flight.complete(&call_id).await;
        response
            .map_err(|error| ToolError::Recoverable(anyhow!("tool server unavailable: {error}")))
    }
}

fn build_registered_tools(
    mut registrations: Vec<Registration>,
) -> (HashMap<String, RegisteredTool>, Vec<ToolDeclaration>) {
    registrations.sort_by(|left, right| left.server.cmp(&right.server));
    let mut registered = HashMap::new();
    for registration in registrations {
        for spec in registration.tools {
            let parameters = match serde_json::from_value::<JsonSchema>(spec.input_schema) {
                Ok(parameters) => parameters,
                Err(error) => {
                    log::warn!(
                        "ignoring invalid schema for NATS tool '{}.{}': {error}",
                        registration.server,
                        spec.name
                    );
                    continue;
                }
            };
            let name = spec.name.clone();
            let route = RegisteredTool {
                server: registration.server.clone(),
                request_timeout: spec
                    .timeout_secs
                    .map(Duration::from_secs)
                    .unwrap_or(DEFAULT_REQUEST_TIMEOUT),
            };
            let declaration = ToolDeclaration {
                name: spec.name,
                description: spec.description,
                parameters,
                mcp_tool_name: None,
                mcp_server_name: None,
                call_template: None,
                result_template: None,
                idempotent_hint: Some(spec.idempotent_hint),
                read_only_hint: Some(spec.read_only_hint),
            };
            if let Some((previous, _)) = registered.insert(name.clone(), (route, declaration)) {
                log::warn!(
                    "duplicate NATS tool '{name}' from server '{}'; server '{}' wins",
                    previous.server,
                    registration.server
                );
            }
        }
    }
    let (tools, mut declarations): (HashMap<_, _>, Vec<_>) = registered
        .into_iter()
        .map(|(name, (route, declaration))| ((name, route), declaration))
        .unzip();
    declarations.sort_by(|left, right| left.name.cmp(&right.name));
    (tools, declarations)
}

#[async_trait]
impl ToolProvider for NatsToolProvider {
    fn name(&self) -> &str {
        "nats"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        self.tools.contains_key(tool_name)
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        abort: &AbortSignal,
    ) -> Result<Value, ToolError> {
        let Some(route) = self.tools.get(tool_name) else {
            return Err(ToolError::Recoverable(anyhow!(
                "NATS tool is not registered: {tool_name}"
            )));
        };
        let pending = self.prepare_request(tool_name, arguments, route)?;
        let call_id = pending.call_id.clone();
        let message = self.await_response(pending, abort).await?;
        let reply: ToolReply = serde_json::from_slice(&message.payload).map_err(|error| {
            ToolError::Recoverable(anyhow!("invalid reply from tool server: {error}"))
        })?;
        if reply.call_id != call_id {
            return Err(ToolError::Recoverable(anyhow!(
                "tool server returned a mismatched call ID"
            )));
        }
        match reply.result {
            Ok(value) => Ok(value),
            Err(ToolErrorPayload::Recoverable(message)) => {
                Err(ToolError::Recoverable(anyhow!(message)))
            }
            Err(ToolErrorPayload::Fatal(message)) => Err(ToolError::Fatal(anyhow!(message))),
        }
    }
}

async fn registration_snapshot(
    client: &async_nats::Client,
    instance_id: &InstanceId,
) -> anyhow::Result<Vec<Registration>> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let store = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await?;
    let mut keys = store.keys().await?;
    let prefix = format!("{instance_id}.");
    let mut registrations = Vec::new();
    while let Some(key) = keys.try_next().await? {
        if !key.starts_with(&prefix) {
            continue;
        }
        let Some(value) = store.get(&key).await? else {
            continue;
        };
        let registration = match serde_json::from_slice::<Registration>(&value) {
            Ok(registration) => registration,
            Err(error) => {
                log::warn!("ignoring invalid NATS tool registration '{key}': {error}");
                continue;
            }
        };
        if key == registration_key(instance_id, &registration.server) {
            registrations.push(registration);
        }
    }
    Ok(registrations)
}
