use crate::config::{Config, LOCAL_CLUSTER_KEY};
use crate::server_identity::ServerIdentity;
use anyhow::anyhow;
use async_trait::async_trait;
use futures_util::TryStreamExt;
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use harnx_core::tool::{JsonSchema, ToolDeclaration, ToolError, ToolProvider};
use harnx_toolset::{
    ControlKind, ControlMessage, Registration, ToolErrorPayload, ToolReply, ToolRequest, ToolSpec,
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
    selector_server: String,
    raw_name: String,
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
static INSTANCE_IN_FLIGHT: OnceLock<std::sync::Mutex<HashMap<ServerScope, Weak<InFlightMap>>>> =
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
    pub fn for_instance(instance_id: &ServerScope) -> Self {
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
    instance_id: ServerScope,
    parent_session_id: Option<String>,
    tools: HashMap<String, RegisteredTool>,
    registrations: Vec<Registration>,
    active_package: Option<String>,
    declarations: Vec<ToolDeclaration>,
    // Owning this subscription establishes the progress/cancel channel before requests start.
    _control_subscription: Mutex<async_nats::Subscriber>,
    in_flight: NatsInFlightCalls,
}

impl NatsToolProvider {
    /// Connect through runtime config and snapshot registered tools for this instance.
    pub async fn discover(
        config: &Config,
        instance_id: ServerScope,
        in_flight: NatsInFlightCalls,
        active_package: Option<&str>,
    ) -> anyhow::Result<Self> {
        let client = config.nats_client(LOCAL_CLUSTER_KEY).await?;
        let control_subject = instance_id.control_subject();
        let control_subscription = client.subscribe(control_subject).await?;
        client.flush().await?;

        let mut registrations = registration_snapshot(&client, &instance_id)
            .await
            .unwrap_or_else(|error| {
                // Degrading to zero tools is intended when a scope has none
                // registered; going silent about a KV scan that outright
                // failed is not — it looked identical to "no tools configured"
                // in the logs.
                log::warn!(
                    "tool registration discovery failed under scope '{}': {error:#}",
                    instance_id.as_str()
                );
                Vec::new()
            });
        registrations.sort_by_key(|registration| match registration.package.as_deref() {
            Some(package) if Some(package) == active_package => 0,
            None => usize::from(active_package.is_some()),
            Some(_) => 2,
        });
        let (tools, declarations) = build_registered_tools(active_package, registrations.clone());
        let parent_session_id = config
            .session
            .as_ref()
            .map(|session| session.id().to_string());

        Ok(Self {
            client,
            instance_id,
            parent_session_id,
            tools,
            registrations,
            active_package: active_package.map(str::to_string),
            declarations,
            _control_subscription: Mutex::new(control_subscription),
            in_flight,
        })
    }

    fn resolve_route(&self, tool_name: &str) -> Option<RegisteredTool> {
        if let Some((identity_token, raw_name)) = ServerIdentity::parse_agent_tool_name(
            tool_name,
            &self.registrations,
            self.active_package.as_deref(),
        ) {
            let registration = self.registrations.iter().find(|registration| {
                ServerIdentity::identity_token(registration) == identity_token
            })?;
            let spec = registration
                .tools
                .iter()
                .find(|spec| spec.name == raw_name)?;
            return Some(RegisteredTool {
                server: identity_token,
                selector_server: registration.server.clone(),
                raw_name,
                request_timeout: spec
                    .timeout_secs
                    .map(Duration::from_secs)
                    .unwrap_or(DEFAULT_REQUEST_TIMEOUT),
            });
        }

        // Raw aliases keep persisted calls from before server-prefixed naming routable.
        self.tools.get(tool_name).cloned()
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
                let route = self.tools.get(&declaration.name);
                selectors.iter().any(|selector| {
                    let selector = selector.trim();
                    selector == "*"
                        || selector == declaration.name
                        || route.is_some_and(|route| {
                            selector == route.selector_server || selector == route.raw_name
                        })
                        || globset::Glob::new(selector).is_ok_and(|pattern| {
                            let matcher = pattern.compile_matcher();
                            matcher.is_match(&declaration.name)
                                || route.is_some_and(|route| {
                                    matcher.is_match(&route.selector_server)
                                        || matcher.is_match(&route.raw_name)
                                })
                        })
                })
            })
            .cloned()
            .collect()
    }
    pub fn in_flight_calls(&self) -> NatsInFlightCalls {
        self.in_flight.clone()
    }

    /// Describe this snapshot's discovery for the zero-registration guard.
    ///
    /// Reuses the registrations already fetched by [`Self::discover`] instead
    /// of re-scanning the registry, so callers on the per-turn refresh path
    /// don't pay for another KV round trip just to log a diagnostic.
    pub fn discovery_report(&self) -> DiscoveryReport {
        let found = self.registrations.len();
        DiscoveryReport {
            found,
            message: discovery_message(&self.instance_id, found),
        }
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
        arguments: Value,
        route: &RegisteredTool,
    ) -> Result<PendingToolRequest, ToolError> {
        let call_id = Uuid::new_v4().to_string();
        let request = ToolRequest {
            call_id: call_id.clone(),
            tool: route.raw_name.clone(),
            args: arguments,
            parent_session_id: self.parent_session_id.clone(),
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
            subject: self
                .instance_id
                .tool_subject(&route.server, &route.raw_name),
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

fn parse_json_schema(mut value: Value) -> serde_json::Result<JsonSchema> {
    // schemars emits nullable fields as `type: [T, "null"]`, while the
    // completion schema uses `required` to represent optional fields.
    normalize_schema_types(&mut value);
    serde_json::from_value(value)
}

fn normalize_schema_types(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::Array(types)) = object.get_mut("type") {
                let schema_type = types
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|schema_type| *schema_type != "null")
                    .or_else(|| types.iter().find_map(Value::as_str));
                if let Some(schema_type) = schema_type {
                    *object.get_mut("type").expect("type key exists") =
                        Value::String(schema_type.to_string());
                }
            }
            for child in object.values_mut() {
                normalize_schema_types(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                normalize_schema_types(child);
            }
        }
        _ => {}
    }
}

fn registered_tool(
    active_package: Option<&str>,
    registration: &Registration,
    spec: ToolSpec,
) -> Option<(String, RegisteredTool, ToolDeclaration)> {
    let template = |key| {
        spec.meta
            .as_ref()
            .and_then(|meta| meta.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let call_template = template("call_template");
    let result_template = template("result_template");
    let parameters = match parse_json_schema(spec.input_schema) {
        Ok(parameters) => parameters,
        Err(error) => {
            log::warn!(
                "ignoring invalid schema for NATS tool '{}.{}': {error}",
                registration.server,
                spec.name
            );
            return None;
        }
    };
    let raw_name = spec.name.clone();
    let name = ServerIdentity::agent_visible_name(active_package, registration, &raw_name);
    let route = RegisteredTool {
        server: ServerIdentity::identity_token(registration),
        selector_server: registration.server.clone(),
        raw_name,
        request_timeout: spec
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT),
    };
    let declaration = ToolDeclaration {
        name: name.clone(),
        description: spec.description,
        parameters,
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template,
        result_template,
        idempotent_hint: Some(spec.idempotent_hint),
        read_only_hint: Some(spec.read_only_hint),
    };
    Some((name, route, declaration))
}
fn build_registered_tools(
    active_package: Option<&str>,
    mut registrations: Vec<Registration>,
) -> (HashMap<String, RegisteredTool>, Vec<ToolDeclaration>) {
    registrations.sort_by(|left, right| left.server.cmp(&right.server));
    let mut registered = HashMap::new();
    for registration in registrations {
        for spec in registration.tools.clone() {
            let Some((name, route, declaration)) =
                registered_tool(active_package, &registration, spec)
            else {
                continue;
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
    let mut entries = registered.into_iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut tools = HashMap::new();
    let mut declarations = Vec::with_capacity(entries.len());
    for (name, (route, declaration)) in entries {
        // Keep raw aliases during the naming transition so injected and persisted
        // calls from older turns still route to the same server.
        tools.insert(route.raw_name.clone(), route.clone());
        tools.insert(name, route);
        declarations.push(declaration);
    }
    (tools, declarations)
}

#[async_trait]
impl ToolProvider for NatsToolProvider {
    fn name(&self) -> &str {
        "nats"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        self.resolve_route(tool_name).is_some()
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        abort: &AbortSignal,
    ) -> Result<Value, ToolError> {
        let Some(route) = self.resolve_route(tool_name) else {
            return Err(ToolError::Recoverable(anyhow!(
                "NATS tool is not registered: {tool_name}"
            )));
        };
        let pending = self.prepare_request(arguments, &route)?;
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

/// Open the tool registry bucket, treating "it doesn't exist yet" as absent
/// rather than an error — no tool server has ever registered against this
/// cluster, which the caller reports as zero registrations, not a failure.
async fn open_registry_store(
    jetstream: &async_nats::jetstream::Context,
) -> anyhow::Result<Option<async_nats::jetstream::kv::Store>> {
    use async_nats::jetstream::context::KeyValueErrorKind;
    match jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await {
        Ok(store) => Ok(Some(store)),
        Err(error) if error.kind() == KeyValueErrorKind::GetBucket => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn registration_snapshot(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> anyhow::Result<Vec<Registration>> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let Some(store) = open_registry_store(&jetstream).await? else {
        return Ok(Vec::new());
    };
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
        let identity_token = ServerIdentity::identity_token(&registration);
        if key == registration_key(instance_id, &identity_token) {
            registrations.push(registration);
        }
    }
    Ok(registrations)
}

/// What a discovery pass found, so a zero result can explain itself.
///
/// A worker that finds no tool servers under its scope looks identical to a
/// deployment with none configured; this makes the difference visible to
/// whoever is watching the logs.
pub struct DiscoveryReport {
    pub found: usize,
    pub message: String,
}

fn discovery_message(scope: &ServerScope, found: usize) -> String {
    if found == 0 {
        format!(
            "no tool servers are registered under scope '{}'; the model will \
             see built-in tools only. Check that the servers carry the same \
             {HARNX_SERVER_SCOPE} value.",
            scope.as_str()
        )
    } else {
        format!(
            "discovered {found} tool server(s) under scope '{}'",
            scope.as_str()
        )
    }
}

/// Scan the registry for `scope` and describe the result.
///
/// Runs its own prefix scan rather than reusing a cached [`NatsToolProvider`],
/// so it also works as a standalone check against a scope no provider has
/// discovered yet.
pub async fn describe_discovery(
    client: &async_nats::Client,
    scope: &ServerScope,
) -> anyhow::Result<DiscoveryReport> {
    let registrations = registration_snapshot(client, scope).await?;
    let found = registrations.len();
    Ok(DiscoveryReport {
        found,
        message: discovery_message(scope, found),
    })
}

#[cfg(test)]
mod tests {
    use super::build_registered_tools;
    use harnx_toolset::{Registration, ToolSpec};
    use serde_json::json;

    #[test]
    fn prefixes_registered_tool_with_server_and_keeps_raw_route_name() {
        harnx_core::require_nextest();
        let registration = Registration {
            package: None,
            config: String::new(),
            server: "fs".to_string(),
            tools: vec![ToolSpec {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "offset": { "type": ["integer", "null"] }
                    }
                }),
                idempotent_hint: true,
                read_only_hint: true,
                timeout_secs: None,
                meta: None,
            }],
            schema_version: 1,
            proto_version: 1,
        };

        let (tools, declarations) = build_registered_tools(None, vec![registration]);

        assert_eq!(declarations[0].name, "fs_read");
        assert_eq!(
            declarations[0].parameters.properties.as_ref().unwrap()["offset"]
                .type_value
                .as_deref(),
            Some("integer")
        );
        assert_eq!(tools["fs_read"].raw_name, "read");
    }

    #[test]
    fn builds_tool_declaration_templates_from_tool_spec_meta() {
        harnx_core::require_nextest();
        let meta = json!({
            "call_template": "Calling {{tool}}",
            "result_template": "Called {{tool}}"
        })
        .as_object()
        .expect("meta object")
        .clone();
        let registration = Registration {
            package: None,
            config: String::new(),
            server: "template-server".to_string(),
            tools: vec![ToolSpec {
                name: "template_tool".to_string(),
                description: "Tool with display templates".to_string(),
                input_schema: json!({ "type": "object" }),
                idempotent_hint: false,
                read_only_hint: false,
                timeout_secs: None,
                meta: Some(meta),
            }],
            schema_version: 1,
            proto_version: 1,
        };

        let (_, declarations) = build_registered_tools(None, vec![registration]);

        assert_eq!(declarations.len(), 1);
        assert_eq!(
            declarations[0].call_template.as_deref(),
            Some("Calling {{tool}}")
        );
        assert_eq!(
            declarations[0].result_template.as_deref(),
            Some("Called {{tool}}")
        );
    }
}
