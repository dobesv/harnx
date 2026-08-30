//! Server-side adapters for hosting a [`harnx_toolset::Toolset`].

pub mod content;
mod drain;
mod registration_identity;
pub mod schema;

pub use registration_identity::RegistrationIdentity;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv};
use drain::InFlightRequests;
use futures_util::StreamExt;
use harnx_core::execution_context::{
    put_result_execution_context, take_result_execution_context, ExecutionContextObservation,
    ToolObservationProvenance, EXECUTION_CONTEXT_NAMESPACE,
};
use harnx_core::instance::ServerScope;
use harnx_nats_common::connect::NatsConnection;
use harnx_toolset::{
    server_identity_token, ControlKind, ControlMessage, Registration, ToolErrorPayload,
    ToolInvokeError, ToolReply, ToolRequest, Toolset, HDR_CALL_ID, HDR_IDEMPOTENCY_KEY,
    SUBAGENT_SESSION_NEW_TOOL, SUBAGENT_SESSION_PROMPT_TOOL,
};
use opentelemetry::Context as OtelContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, ListToolsResult, MetaObject, PaginatedRequestParams, ServerCapabilities,
    ServerInfo, Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ServerHandler, ServiceExt};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

pub const TOOL_REGISTRY_BUCKET: &str = "harnx_tool_registry";
pub const TOOL_PROTOCOL_VERSION: u32 = 1;
pub const TOOL_SCHEMA_VERSION: u32 = 1;

const IDEMPOTENCY_CACHE_TTL: Duration = Duration::from_secs(60);
const IDEMPOTENCY_CACHE_MAX_ENTRIES: usize = 1_024;
const REGISTRATION_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

type InFlight = Arc<Mutex<HashMap<String, CancellationToken>>>;
type ReplyCache = Arc<Mutex<HashMap<String, ReplyCacheEntry>>>;

enum ReplyCacheEntry {
    InProgress {
        reply: watch::Receiver<Option<ToolReply>>,
    },
    Complete {
        created: Instant,
        reply: ToolReply,
    },
}

enum CacheReservation {
    Execute(watch::Sender<Option<ToolReply>>),
    Wait(watch::Receiver<Option<ToolReply>>),
    Complete(ToolReply),
    Full,
}

#[derive(Clone)]
struct ToolRequestContext {
    client: async_nats::Client,
    toolset: Arc<dyn Toolset>,
    in_flight: InFlight,
    reply_cache: ReplyCache,
    /// Tracks tool requests currently being processed, so shutdown can
    /// drain them before deregistering (see the `drain` module).
    active_requests: InFlightRequests,
    server_scope: ServerScope,
    server_identity: String,
}

struct ValidatedToolRequest {
    reply_subject: async_nats::Subject,
    request: ToolRequest,
    idempotency_key: String,
    parent_cx: OtelContext,
}

fn tool_exec_span(tool_name: &str, parent_cx: OtelContext) -> tracing::Span {
    let span = tracing::info_span!(
        "tool_exec",
        otel.kind = "server",
        harnx.tool.name = tool_name,
    );
    harnx_telemetry::set_span_parent(&span, parent_cx);
    span
}

struct ServeSettings {
    instance_id: ServerScope,
    connection: NatsConnection,
    shutdown: CancellationToken,
    identity: RegistrationIdentity,
}

/// Everything `serve_requests` needs to keep the KV registration alive, bundled
/// so the function stays under the argument-count limit.
struct RegistrationRefresh<'a> {
    registry: &'a kv::Store,
    instance_id: &'a ServerScope,
    registration: &'a Registration,
    interval: &'a mut tokio::time::Interval,
    /// The revision of our own last-published registration, so shutdown can
    /// delete it conditionally instead of unconditionally (see the delete
    /// call in `serve_with_shutdown`). Updated after every successful
    /// refresh publish, not just the initial one.
    revision: &'a mut u64,
}

/// The subscriptions `serve_requests` polls, plus the signal that ends the
/// loop on purpose. Bundled for the same reason as `RegistrationRefresh`.
///
/// `shutdown` is distinct from a subscription simply closing: losing a
/// subscription usually means the whole NATS connection is gone, at which
/// point the exit-cleanup delete can't reach the server either (the TTL is
/// the backstop). Cancelling `shutdown` exits the loop while the connection
/// is still healthy, so the delete actually lands.
struct ToolSubscriptions<'a> {
    tool_requests: &'a mut async_nats::Subscriber,
    controls: &'a mut async_nats::Subscriber,
    shutdown: CancellationToken,
}

/// KV key for one worker instance's tool server registration.
pub fn registration_key(instance_id: &ServerScope, identity_token: &str) -> String {
    format!("{instance_id}.{identity_token}")
}

/// Host a toolset over Core NATS request-reply and publish its KV registration.
pub async fn serve_over_nats<T>(
    toolset: T,
    instance_id: ServerScope,
    nats_url: &str,
    token: &str,
) -> Result<()>
where
    T: Toolset + 'static,
{
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    let endpoint = harnx_nats_common::connect::NatsEndpoint {
        name: "explicit".to_string(),
        url: nats_url.to_string(),
        token: Some(token.to_string()),
        replicas: None,
        tls: None,
        tls_cert: None,
        tls_key: None,
        tls_ca: None,
    };
    let client = endpoint.connect().await?;
    // This entry point takes an explicit URL/token instead of the environment,
    // so it has no way to read HARNX_NATS_REPLICAS either; callers that need a
    // configured replica count go through `serve_with_shutdown` directly.
    let connection = NatsConnection {
        client,
        replicas: 1,
    };
    serve_with_client(Arc::new(toolset), instance_id, connection).await
}

/// Serve a toolset using an existing NATS connection.
pub async fn serve_with_client(
    toolset: Arc<dyn Toolset>,
    instance_id: ServerScope,
    connection: NatsConnection,
) -> Result<()> {
    serve_with_client_and_identity(
        toolset,
        instance_id,
        connection,
        RegistrationIdentity::from_env(),
    )
    .await
}

/// Serve an in-process toolset with an explicit package/config identity.
pub async fn serve_with_client_and_identity(
    toolset: Arc<dyn Toolset>,
    instance_id: ServerScope,
    connection: NatsConnection,
    identity: RegistrationIdentity,
) -> Result<()> {
    // Never cancelled: this entry point has no shutdown signal of its own, so
    // it only ever exits through `serve_requests`' bail! conditions.
    serve_configured(
        toolset,
        ServeSettings {
            instance_id,
            connection,
            shutdown: CancellationToken::new(),
            identity,
        },
    )
    .await
}

/// Serve a toolset using an existing NATS connection, exiting cleanly (and
/// running exit cleanup while the connection is still usable) when
/// `shutdown` is cancelled, in addition to the usual failure exits.
pub async fn serve_with_shutdown(
    toolset: Arc<dyn Toolset>,
    instance_id: ServerScope,
    connection: NatsConnection,
    shutdown: CancellationToken,
) -> Result<()> {
    serve_configured(
        toolset,
        ServeSettings {
            instance_id,
            connection,
            shutdown,
            identity: RegistrationIdentity::from_env(),
        },
    )
    .await
}

async fn serve_configured(toolset: Arc<dyn Toolset>, settings: ServeSettings) -> Result<()> {
    let ServeSettings {
        instance_id,
        connection,
        shutdown,
        identity,
    } = settings;
    let NatsConnection { client, replicas } = connection;
    let server_name = toolset.name().to_owned();
    let RegistrationIdentity { package, config } = identity;
    let identity_token = server_identity_token(package.as_deref(), &config, &server_name);
    let tool_subject = instance_id.tool_subject(&identity_token, ">");
    let control_subject = instance_id.control_subject();

    let mut tool_requests = client
        .queue_subscribe(tool_subject.clone(), identity_token.clone())
        .await
        .with_context(|| format!("subscribe to tool requests on {tool_subject}"))?;
    let mut controls = client
        .subscribe(control_subject.clone())
        .await
        .with_context(|| format!("subscribe to controls on {control_subject}"))?;

    // Both subscriptions must be active before registration makes this server discoverable.
    client.flush().await.context("flush tool subscriptions")?;

    let registration = Registration {
        package,
        config,
        server: server_name,
        tools: toolset.tools(),
        schema_version: TOOL_SCHEMA_VERSION,
        proto_version: TOOL_PROTOCOL_VERSION,
    };
    let jetstream = jetstream::new(client.clone());
    let registry = ensure_registry_bucket(&jetstream, replicas).await?;
    let mut revision = publish_registration(&registry, &instance_id, &registration).await?;

    let (active_requests, active_requests_rx) = InFlightRequests::new();
    let request_context = ToolRequestContext {
        client: client.clone(),
        toolset,
        in_flight: Arc::new(Mutex::new(HashMap::new())),
        reply_cache: Arc::new(Mutex::new(HashMap::new())),
        active_requests,
        server_scope: instance_id.clone(),
        server_identity: identity_token.clone(),
    };
    let mut refresh = tokio::time::interval(REGISTRATION_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    refresh.tick().await;

    let outcome = serve_requests(
        &request_context,
        ToolSubscriptions {
            tool_requests: &mut tool_requests,
            controls: &mut controls,
            shutdown,
        },
        RegistrationRefresh {
            registry: &registry,
            instance_id: &instance_id,
            registration: &registration,
            interval: &mut refresh,
            revision: &mut revision,
        },
    )
    .await;

    // Give callers already waiting on a reply a chance to get one instead of
    // blocking on their own 60s timeout: wait for in-flight requests to
    // finish before deregistering, bounded so a stuck invocation can't stall
    // shutdown forever.
    drain::drain(active_requests_rx).await;

    // Best-effort: the TTL is the backstop when this cannot run.
    let key = registration_key(&instance_id, &identity_token);
    delete_own_registration(&registry, &key, revision).await;
    outcome
}

/// Delete `key` on shutdown, but only if `revision` (our own last-published
/// one) is still current. On a rolling deploy, a replacement instance
/// publishes under this same key before this one finishes shutting down
/// (new pod ready before old pod terminates is Kubernetes' normal sequence);
/// an unconditional delete here would remove the replacement's registration
/// instead of this one's. `delete_expect_revision` fails harmlessly in that
/// case, so it's not treated as an error worth warning about.
async fn delete_own_registration(registry: &kv::Store, key: &str, revision: u64) {
    match registry.delete_expect_revision(key, Some(revision)).await {
        Ok(()) => {}
        Err(error) if error.kind() == kv::DeleteErrorKind::WrongLastRevision => {
            log::debug!(
                "tool registration '{key}' was already replaced by a newer instance; \
                 not deleting it"
            );
        }
        Err(error) => {
            log::warn!("could not remove tool registration '{key}' on shutdown: {error}");
        }
    }
}

async fn serve_requests(
    request_context: &ToolRequestContext,
    subscriptions: ToolSubscriptions<'_>,
    refresh: RegistrationRefresh<'_>,
) -> Result<()> {
    loop {
        tokio::select! {
            request = subscriptions.tool_requests.next() => {
                let Some(request) = request else {
                    anyhow::bail!("tool request subscription closed");
                };
                spawn_tool_request(request_context.clone(), request);
            }
            control = subscriptions.controls.next() => {
                let Some(control) = control else {
                    anyhow::bail!("control subscription closed");
                };
                handle_control(control, &request_context.in_flight).await;
            }
            _ = refresh.interval.tick() => {
                match publish_registration(refresh.registry, refresh.instance_id, refresh.registration).await {
                    Ok(new_revision) => *refresh.revision = new_revision,
                    Err(error) => {
                        log::warn!("refresh tool registration failed; retrying next interval: {error:#}");
                    }
                }
            }
            _ = subscriptions.shutdown.cancelled() => {
                return Ok(());
            }
        }
    }
}

fn spawn_tool_request(context: ToolRequestContext, message: async_nats::Message) {
    let in_flight = context.active_requests.enter();
    tokio::spawn(async move {
        let _in_flight = in_flight;
        if let Err(error) = process_tool_request(&context, message).await {
            log::warn!("harnx tool request failed: {error:#}");
        }
    });
}

async fn process_tool_request(
    context: &ToolRequestContext,
    message: async_nats::Message,
) -> Result<()> {
    let Some(validated) = validate_tool_request(context, message).await? else {
        return Ok(());
    };
    let ValidatedToolRequest {
        reply_subject,
        mut request,
        idempotency_key,
        parent_cx,
    } = validated;
    let completion = match reserve_cache_entry(&context.reply_cache, &idempotency_key).await {
        CacheReservation::Complete(mut reply) => {
            reply.call_id.clone_from(&request.call_id);
            finalize_execution_context(context, &request, &mut reply);
            return publish_reply(&context.client, reply_subject, &reply).await;
        }
        CacheReservation::Wait(reply) => {
            let mut reply = wait_for_cached_reply(reply).await?;
            reply.call_id.clone_from(&request.call_id);
            finalize_execution_context(context, &request, &mut reply);
            return publish_reply(&context.client, reply_subject, &reply).await;
        }
        CacheReservation::Full => {
            return publish_recoverable_reply(
                &context.client,
                reply_subject,
                request.call_id,
                "tool server idempotency cache is full".to_string(),
            )
            .await;
        }
        CacheReservation::Execute(completion) => completion,
    };

    let request_attestation = RequestAttestation {
        call_id: request.call_id.clone(),
        tool: request.tool.clone(),
        capabilities: request.capabilities.clone(),
    };
    let result = invoke_uncached_tool(context, &mut request, parent_cx).await;

    let reply = ToolReply {
        call_id: request.call_id,
        result: result.map_err(map_invoke_error),
    };
    complete_cache_entry(
        &context.reply_cache,
        idempotency_key,
        reply.clone(),
        completion,
    )
    .await;
    let mut published_reply = reply;
    finalize_execution_context_for_attestation(context, &request_attestation, &mut published_reply);
    publish_reply(&context.client, reply_subject, &published_reply).await
}

async fn invoke_uncached_tool(
    context: &ToolRequestContext,
    request: &mut ToolRequest,
    parent_cx: OtelContext,
) -> Result<Value, ToolInvokeError> {
    let cancel = CancellationToken::new();
    context
        .in_flight
        .lock()
        .await
        .insert(request.call_id.clone(), cancel.clone());
    let mut args = std::mem::take(&mut request.args);
    add_parent_session_id_arg(&request.tool, request.parent_session_id.take(), &mut args);
    let start = Instant::now();
    let result = context
        .toolset
        .invoke(&request.tool, args, cancel)
        .instrument(tool_exec_span(&request.tool, parent_cx))
        .await;
    context.in_flight.lock().await.remove(&request.call_id);
    let elapsed = start.elapsed();
    let is_ok = result.is_ok();
    harnx_metrics::record_tool_call(&request.tool, is_ok, elapsed);
    result
}

struct RequestAttestation {
    call_id: String,
    tool: String,
    capabilities: std::collections::BTreeSet<String>,
}

fn finalize_execution_context(
    context: &ToolRequestContext,
    request: &ToolRequest,
    reply: &mut ToolReply,
) {
    finalize_execution_context_for_attestation(
        context,
        &RequestAttestation {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            capabilities: request.capabilities.clone(),
        },
        reply,
    );
}

fn finalize_execution_context_for_attestation(
    context: &ToolRequestContext,
    request: &RequestAttestation,
    reply: &mut ToolReply,
) {
    let Ok(result) = &mut reply.result else {
        return;
    };
    let raw_context = take_result_execution_context(result);
    if !request.capabilities.contains(EXECUTION_CONTEXT_NAMESPACE) {
        return;
    }
    let Some(raw_context) = raw_context else {
        return;
    };
    let mut observation = match serde_json::from_value::<ExecutionContextObservation>(raw_context) {
        Ok(observation) => observation,
        Err(error) => {
            log::warn!(
                "stripping malformed execution context from tool result: server={} tool={} error={error}",
                context.server_identity,
                request.tool
            );
            return;
        }
    };
    observation.provenance = Some(ToolObservationProvenance::new(
        context.server_scope.to_string(),
        context.server_identity.clone(),
        request.tool.clone(),
        request.call_id.clone(),
    ));
    if let Err(error) = observation.validate() {
        log::warn!(
            "stripping invalid execution context from tool result: server={} tool={} error={error:#}",
            context.server_identity,
            request.tool
        );
        return;
    }
    if let Ok(value) = serde_json::to_value(observation) {
        put_result_execution_context(result, value);
    }
}

fn add_parent_session_id_arg(tool: &str, parent_session_id: Option<String>, args: &mut Value) {
    if accepts_parent_session_id(tool) {
        if let (Some(parent_session_id), Some(args)) = (parent_session_id, args.as_object_mut()) {
            args.insert(
                "__harnx_parent_session_id".to_string(),
                Value::String(parent_session_id),
            );
        }
    }
}

fn accepts_parent_session_id(tool: &str) -> bool {
    // Sub-agent toolsets reserve these raw names for calls that start a child turn.
    matches!(
        tool,
        SUBAGENT_SESSION_PROMPT_TOOL | SUBAGENT_SESSION_NEW_TOOL
    )
}

async fn validate_tool_request(
    context: &ToolRequestContext,
    message: async_nats::Message,
) -> Result<Option<ValidatedToolRequest>> {
    let parent_cx = message
        .headers
        .as_ref()
        .map(harnx_telemetry::propagate::extract_context_from_nats)
        .unwrap_or_default();
    let reply_subject = message
        .reply
        .clone()
        .context("tool request has no reply subject")?;
    let header_call_id = header_value(&message, HDR_CALL_ID);
    let request: ToolRequest = match serde_json::from_slice(&message.payload) {
        Ok(request) => request,
        Err(error) => {
            publish_recoverable_reply(
                &context.client,
                reply_subject,
                header_call_id.unwrap_or_default(),
                format!("decode tool request payload: {error}"),
            )
            .await?;
            return Ok(None);
        }
    };
    if let Some(header_call_id) = header_call_id {
        if header_call_id != request.call_id {
            publish_recoverable_reply(
                &context.client,
                reply_subject,
                header_call_id,
                "tool request call ID header does not match payload".to_string(),
            )
            .await?;
            return Ok(None);
        }
    }
    let Some(idempotency_key) = header_value(&message, HDR_IDEMPOTENCY_KEY) else {
        publish_recoverable_reply(
            &context.client,
            reply_subject,
            request.call_id,
            "tool request is missing Idempotency-Key header".to_string(),
        )
        .await?;
        return Ok(None);
    };
    Ok(Some(ValidatedToolRequest {
        reply_subject,
        request,
        idempotency_key,
        parent_cx,
    }))
}

async fn reserve_cache_entry(cache: &ReplyCache, key: &str) -> CacheReservation {
    let mut cache = cache.lock().await;
    remove_expired_replies(&mut cache, Instant::now());
    if let Some(entry) = cache.get(key) {
        return match entry {
            ReplyCacheEntry::InProgress { reply, .. } => CacheReservation::Wait(reply.clone()),
            ReplyCacheEntry::Complete { reply, .. } => CacheReservation::Complete(reply.clone()),
        };
    }
    if cache.len() >= IDEMPOTENCY_CACHE_MAX_ENTRIES {
        evict_oldest_completed_reply(&mut cache);
    }
    if cache.len() >= IDEMPOTENCY_CACHE_MAX_ENTRIES {
        return CacheReservation::Full;
    }
    let (completion, reply) = watch::channel(None);
    cache.insert(key.to_string(), ReplyCacheEntry::InProgress { reply });
    CacheReservation::Execute(completion)
}

fn remove_expired_replies(cache: &mut HashMap<String, ReplyCacheEntry>, now: Instant) {
    cache.retain(|_, entry| match entry {
        ReplyCacheEntry::InProgress { .. } => true,
        ReplyCacheEntry::Complete { created, .. } => {
            now.duration_since(*created) < IDEMPOTENCY_CACHE_TTL
        }
    });
}

fn evict_oldest_completed_reply(cache: &mut HashMap<String, ReplyCacheEntry>) {
    let oldest = cache
        .iter()
        .filter_map(|(key, entry)| match entry {
            ReplyCacheEntry::Complete { created, .. } => Some((key.clone(), *created)),
            ReplyCacheEntry::InProgress { .. } => None,
        })
        .min_by_key(|(_, created)| *created)
        .map(|(key, _)| key);
    if let Some(key) = oldest {
        cache.remove(&key);
    }
}

async fn wait_for_cached_reply(mut reply: watch::Receiver<Option<ToolReply>>) -> Result<ToolReply> {
    if reply.borrow().is_none() {
        reply
            .changed()
            .await
            .context("original idempotent tool request ended without a reply")?;
    }
    let cached = reply.borrow().clone();
    cached.context("original idempotent tool request ended without a reply")
}

async fn complete_cache_entry(
    cache: &ReplyCache,
    key: String,
    reply: ToolReply,
    completion: watch::Sender<Option<ToolReply>>,
) {
    cache.lock().await.insert(
        key,
        ReplyCacheEntry::Complete {
            created: Instant::now(),
            reply: reply.clone(),
        },
    );
    let _ = completion.send(Some(reply));
}

async fn publish_recoverable_reply(
    client: &async_nats::Client,
    subject: async_nats::Subject,
    call_id: String,
    message: String,
) -> Result<()> {
    log::warn!("rejecting NATS tool request: {message}");
    publish_reply(
        client,
        subject,
        &ToolReply {
            call_id,
            result: Err(ToolErrorPayload::Recoverable(message)),
        },
    )
    .await
}

fn map_invoke_error(error: ToolInvokeError) -> ToolErrorPayload {
    match error {
        ToolInvokeError::Recoverable(message) => ToolErrorPayload::Recoverable(message),
        ToolInvokeError::Fatal(message) => ToolErrorPayload::Fatal(message),
    }
}

async fn publish_reply(
    client: &async_nats::Client,
    subject: async_nats::Subject,
    reply: &ToolReply,
) -> Result<()> {
    let payload = serde_json::to_vec(reply).context("encode tool reply")?;
    client
        .publish(subject, payload.into())
        .await
        .context("publish tool reply")
}

async fn handle_control(message: async_nats::Message, in_flight: &InFlight) {
    let Ok(control) = serde_json::from_slice::<ControlMessage>(&message.payload) else {
        return;
    };
    let call_id = header_value(&message, HDR_CALL_ID).unwrap_or(control.call_id);
    match control.kind {
        ControlKind::Cancel => {
            if let Some(cancel) = in_flight.lock().await.get(&call_id) {
                cancel.cancel();
            }
        }
    }
}

fn header_value(message: &async_nats::Message, name: &str) -> Option<String> {
    message
        .headers
        .as_ref()?
        .get(name)
        .map(|value| value.as_str().to_owned())
}

async fn ensure_registry_bucket(
    jetstream: &jetstream::Context,
    replicas: usize,
) -> Result<kv::Store> {
    harnx_nats_common::registry::ensure_bucket_with_ttl(
        jetstream,
        TOOL_REGISTRY_BUCKET,
        harnx_nats_common::registry::REGISTRATION_TTL,
        replicas,
    )
    .await
}

async fn publish_registration(
    registry: &kv::Store,
    instance_id: &ServerScope,
    registration: &Registration,
) -> Result<u64> {
    let identity_token = server_identity_token(
        registration.package.as_deref(),
        &registration.config,
        &registration.server,
    );
    let key = registration_key(instance_id, &identity_token);
    let payload = serde_json::to_vec(registration).context("encode tool registration")?;
    registry
        .put(&key, payload.into())
        .await
        .map_err(anyhow::Error::from)
        .with_context(|| format!("publish tool registration '{key}'"))
}

/// Run a toolset in MCP stdio mode when `--mcp-stdio` is present, otherwise
/// NATS mode.
///
/// In NATS mode, wires SIGTERM/Ctrl+C to a graceful stop so a pod killed by
/// Kubernetes gets a chance to remove its own registration instead of
/// leaving it for the TTL.
pub async fn run_toolset_main<T>(toolset: T) -> Result<()>
where
    T: Toolset + 'static,
{
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);

    let mut args = std::env::args_os();
    let metrics_addr = args
        .find(|arg| arg == "--metrics-addr")
        .and_then(|_| args.next())
        .map(|addr| addr.to_string_lossy().into_owned())
        .or_else(|| std::env::var("HARNX_METRICS_ADDR").ok());
    harnx_metrics::init(&harnx_metrics::MetricsFlags { metrics_addr })?;

    let service_name = format!("harnx-{}-server", toolset.name());
    let telemetry = harnx_telemetry::init_telemetry(&service_name)?;

    let result: Result<()> = async {
        let toolset: Arc<dyn Toolset> = Arc::new(toolset);
        if std::env::args_os().any(|arg| arg == "--mcp-stdio") {
            let service = McpToolsetAdapter { toolset }
                .serve(rmcp::transport::stdio())
                .await
                .context("start MCP stdio server")?;
            service.waiting().await.context("run MCP stdio server")?;
            return Ok(());
        }

        let scope =
            harnx_core::instance::scope_from_env(harnx_core::instance::StandaloneMode::McpStdio)?;
        log::info!("serving under scope '{}'", scope.as_str());
        let endpoint = harnx_nats_common::connect::NatsEndpoint::from_env()?;
        let client = endpoint.connect().await?;
        let connection = NatsConnection {
            client,
            replicas: endpoint.resolved_replicas(),
        };
        let shutdown = harnx_nats_common::shutdown::cancel_token_on_shutdown_signal();
        serve_with_shutdown(toolset, scope, connection, shutdown).await
    }
    .await;

    telemetry.shutdown().await;
    result
}

#[derive(Clone)]
struct McpToolsetAdapter {
    toolset: Arc<dyn Toolset>,
}

impl ServerHandler for McpToolsetAdapter {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(
                format!("harnx-{}-server", self.toolset.name()),
                env!("CARGO_PKG_VERSION"),
            ),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self
            .toolset
            .tools()
            .into_iter()
            .map(|spec| {
                let input_schema = match spec.input_schema {
                    Value::Object(schema) => schema,
                    _ => Map::new(),
                };
                let mut tool = Tool::new(spec.name, spec.description, input_schema).annotate(
                    ToolAnnotations::new()
                        .read_only(spec.read_only_hint)
                        .idempotent(spec.idempotent_hint),
                );
                tool.meta = spec.meta.map(MetaObject);
                tool
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let parent_cx = harnx_telemetry::propagate::extract_context_from_mcp_meta(&context.meta);
        let span = tool_exec_span(&request.name, parent_cx);
        self.dispatch_call_tool(request, context)
            .instrument(span)
            .await
            .map(Into::into)
    }
}

impl McpToolsetAdapter {
    /// The tool dispatch, which always finishes in a single step.
    ///
    /// `call_tool` must return `CallToolResponse`, whose other variants cover
    /// elicitation and long-running tasks that this server does not use.
    /// Dispatching separately keeps every arm returning a plain
    /// `CallToolResult`.
    ///
    /// Tool dispatch forks: `run_toolset_main` has two mutually exclusive paths:
    /// NATS → `invoke_uncached_tool`, and MCP stdio → this method (calls
    /// `toolset.invoke` directly). Any cross-cutting concern (metrics, tracing, auth)
    /// added at one seam does NOT automatically cover the other. rmcp `--http` servers
    /// use their own `ServerHandler::call_tool`, a third seam.
    async fn dispatch_call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let tool_name = request.name.clone();
        let args = Value::Object(request.arguments.unwrap_or_default());
        let started = Instant::now();
        let result = self
            .toolset
            .invoke(&tool_name, args, CancellationToken::new())
            .await;
        harnx_metrics::record_tool_call(&tool_name, result.is_ok(), started.elapsed());

        match result {
            Ok(value) => {
                let text =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
            }
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(
                error.to_string(),
            )])),
        }
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::{SpanId, SpanKind, TraceId};
    use rmcp::model::RequestParamsMeta;

    use super::*;

    const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const PARENT_SPAN_ID: &str = "00f067aa0ba902b7";
    const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    fn assert_tool_exec_parent(extract_parent: impl FnOnce() -> OtelContext) {
        let spans = harnx_telemetry::collect_test_spans(|| {
            drop(tool_exec_span("test_tool", extract_parent()));
        });
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.name, "tool_exec");
        assert_eq!(span.span_kind, SpanKind::Server);
        assert!(span.attributes.contains(&opentelemetry::KeyValue::new(
            "harnx.tool.name",
            "test_tool"
        )));
        assert_eq!(
            span.span_context.trace_id(),
            TraceId::from_hex(TRACE_ID).expect("fixed trace ID")
        );
        assert_eq!(
            span.parent_span_id,
            SpanId::from_hex(PARENT_SPAN_ID).expect("fixed parent span ID")
        );
        assert!(span.parent_span_is_remote);
    }

    #[test]
    fn nats_tool_exec_span_continues_extracted_parent() {
        harnx_core::require_nextest();
        let mut headers = async_nats::HeaderMap::new();
        headers.insert("traceparent", TRACEPARENT);

        assert_tool_exec_parent(|| harnx_telemetry::propagate::extract_context_from_nats(&headers));
    }

    #[test]
    fn mcp_tool_exec_span_continues_extracted_parent() {
        harnx_core::require_nextest();
        let mut params = CallToolRequestParams::new("test_tool");
        params.set_traceparent(TRACEPARENT);

        assert_tool_exec_parent(|| harnx_telemetry::propagate::extract_context_from_mcp(&params));
    }

    #[tokio::test]
    async fn idempotency_cache_rejects_growth_past_cap() {
        harnx_core::require_nextest();
        let cache: ReplyCache = Arc::new(Mutex::new(HashMap::new()));
        for index in 0..IDEMPOTENCY_CACHE_MAX_ENTRIES {
            assert!(matches!(
                reserve_cache_entry(&cache, &format!("key-{index}")).await,
                CacheReservation::Execute(_)
            ));
        }
        assert!(matches!(
            reserve_cache_entry(&cache, "overflow").await,
            CacheReservation::Full
        ));
        assert_eq!(cache.lock().await.len(), IDEMPOTENCY_CACHE_MAX_ENTRIES);
    }

    #[test]
    fn parent_session_id_supports_raw_session_start_tools() {
        for tool in ["session_prompt", "session_new"] {
            assert!(
                accepts_parent_session_id(tool),
                "expected support for {tool}"
            );
        }
        assert!(!accepts_parent_session_id("session_load"));
        assert!(!accepts_parent_session_id("prompt"));
        assert!(!accepts_parent_session_id("agent_session_prompt"));
    }

    #[test]
    fn tool_call_metrics_recorded_on_success_and_error() {
        harnx_core::require_nextest();
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        use std::time::Duration;

        // DebuggingRecorder captures metric values in a local recorder scope.
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            // Success case
            harnx_metrics::record_tool_call("test_tool_ok", true, Duration::from_millis(100));
            // Error case
            harnx_metrics::record_tool_call("test_tool_err", false, Duration::from_millis(50));
        });

        let snapshot = snapshotter.snapshot();

        // Iterate through metrics and validate counters and histograms
        let mut ok_counter_found = false;
        let mut err_counter_found = false;
        let mut ok_hist_found = false;
        let mut err_hist_found = false;

        for (key, _unit, _desc, value) in snapshot.into_vec() {
            let key_name = key.key().name();
            let key_labels = key.key().labels();
            match value {
                DebugValue::Counter(c) if key_name == harnx_metrics::TOOL_CALLS_TOTAL => {
                    let labels: Vec<_> = key_labels.collect();
                    let tool_label = labels.iter().find(|l| l.key() == "tool");
                    let status_label = labels.iter().find(|l| l.key() == "status");
                    if let (Some(tool), Some(status)) = (tool_label, status_label) {
                        if tool.value() == "test_tool_ok" && status.value() == "ok" {
                            assert_eq!(c, 1, "ok counter incremented once");
                            ok_counter_found = true;
                        } else if tool.value() == "test_tool_err" && status.value() == "error" {
                            assert_eq!(c, 1, "error counter incremented once");
                            err_counter_found = true;
                        }
                    }
                }
                DebugValue::Histogram(samples)
                    if key_name == harnx_metrics::TOOL_CALL_DURATION_SECONDS =>
                {
                    let labels: Vec<_> = key_labels.collect();
                    let tool_label = labels.iter().find(|l| l.key() == "tool");
                    if let Some(tool) = tool_label {
                        // samples is Vec<OrderedFloat<f64>>
                        // Just check that we have at least one sample to confirm recording happened
                        assert!(
                            !samples.is_empty(),
                            "histogram should have recorded samples"
                        );
                        if tool.value() == "test_tool_ok" {
                            ok_hist_found = true;
                        } else if tool.value() == "test_tool_err" {
                            err_hist_found = true;
                        }
                    }
                }
                _ => {}
            }
        }

        assert!(ok_counter_found, "ok counter should have been recorded");
        assert!(err_counter_found, "error counter should have been recorded");
        assert!(ok_hist_found, "ok histogram should have been recorded");
        assert!(err_hist_found, "error histogram should have been recorded");
    }
}
