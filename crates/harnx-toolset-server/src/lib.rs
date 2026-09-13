//! Server-side adapters for hosting a [`harnx_toolset::Toolset`].

mod control;
use control::handle_control;
mod aggregate;
pub mod content;
mod drain;
mod execution;
mod lifecycle;
mod registration_identity;
pub mod schema;
mod subscriptions;

pub use aggregate::serve_many_with_shutdown;
pub use lifecycle::ServeLifecycle;
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
    ToolInvocation, ToolInvocationContext, ToolInvokeError, ToolReply, ToolRequest, Toolset,
    HDR_CALL_ID, HDR_IDEMPOTENCY_KEY, SUBAGENT_SESSION_NEW_TOOL, SUBAGENT_SESSION_PROMPT_TOOL,
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
pub const TOOL_PROTOCOL_VERSION: u32 = 3;

pub mod invocation_journal;
mod recovery;
pub const TOOL_SCHEMA_VERSION: u32 = 1;

const IDEMPOTENCY_CACHE_TTL: Duration = Duration::from_secs(60);
const IDEMPOTENCY_CACHE_MAX_ENTRIES: usize = 1_024;
const REGISTRATION_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

type InFlight = Arc<Mutex<HashMap<String, execution::ActiveCall>>>;
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
    execution_store: harnx_execution_control::ExecutionStore,
}

struct ValidatedToolRequest {
    reply_subject: harnx_nats_common::rpc::ReplyTarget,
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

fn metric_tool_name<'a>(toolset: &dyn Toolset, requested: &'a str) -> &'a str {
    if toolset.tools().iter().any(|tool| tool.name == requested) {
        requested
    } else {
        "unknown"
    }
}

struct ServeSettings {
    instance_id: ServerScope,
    connection: NatsConnection,
    lifecycle: ServeLifecycle,
    identity: RegistrationIdentity,
    started: Option<tokio::sync::oneshot::Sender<()>>,
}

/// Everything `serve_requests` needs to keep the KV registration alive, bundled
/// so the function stays under the argument-count limit.
struct RegistrationRefresh<'a> {
    registry: &'a kv::Store,
    instance_id: &'a ServerScope,
    identity_token: &'a str,
    registration: &'a Registration,
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
            lifecycle: ServeLifecycle::new(CancellationToken::new(), None),
            identity,
            started: None,
        },
    )
    .await
}

/// Serve a toolset using an existing NATS connection, exiting cleanly (and
/// running exit cleanup while the connection is still usable) when the
/// lifecycle's shutdown token is cancelled, in addition to the usual failure exits.
pub async fn serve_with_shutdown(
    toolset: Arc<dyn Toolset>,
    instance_id: ServerScope,
    connection: NatsConnection,
    lifecycle: ServeLifecycle,
) -> Result<()> {
    serve_configured(
        toolset,
        ServeSettings {
            instance_id,
            connection,
            lifecycle,
            identity: RegistrationIdentity::from_env(),
            started: None,
        },
    )
    .await
}

/// Serve a toolset with an existing NATS connection and configured lifecycle.
///
/// **Avoiding double-init (EADDRINUSE):** Callers that have already initialized healthz
/// (via `harnx_healthz::init()`) should put the resulting `Readiness` in the lifecycle.
/// `None` is appropriate only for entry points that do not pre-initialize healthz.
async fn serve_configured(toolset: Arc<dyn Toolset>, settings: ServeSettings) -> Result<()> {
    let ServeSettings {
        instance_id,
        connection,
        lifecycle,
        identity,
        started,
    } = settings;
    let (shutdown, readiness) = lifecycle.into_parts();
    let NatsConnection { client, replicas } = connection;
    let server_name = toolset.name().to_owned();
    let RegistrationIdentity { package, config } = identity;
    let identity_token = server_identity_token(package.as_deref(), &config, &server_name);
    let (mut tool_requests, mut controls) = subscriptions::subscribe_to_requests(
        &client,
        &instance_id,
        &identity_token,
        readiness.as_ref(),
    )
    .await?;

    let registration = Registration {
        package,
        config,
        server: server_name,
        tools: toolset.tools(),
        schema_version: TOOL_SCHEMA_VERSION,
        proto_version: TOOL_PROTOCOL_VERSION,
    };
    let (registry, execution_store) = ensure_control_stores(&client, replicas).await?;
    let mut revision = publish_registration(&registry, &instance_id, &registration).await?;
    signal_started(started);

    let (active_requests, active_requests_rx) = InFlightRequests::new();
    let request_context = ToolRequestContext {
        client: client.clone(),
        toolset,
        in_flight: Arc::new(Mutex::new(HashMap::new())),
        reply_cache: Arc::new(Mutex::new(HashMap::new())),
        active_requests,
        server_scope: instance_id.clone(),
        server_identity: identity_token.clone(),
        execution_store,
    };

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
            identity_token: &identity_token,
            registration: &registration,
            revision: &mut revision,
        },
    )
    .await;

    if let Some(readiness) = readiness.as_ref() {
        readiness.not_ready();
    }

    // Give callers already waiting on a reply a chance to get one: wait for
    // in-flight requests to finish before deregistering, bounded so a stuck
    // invocation can't stall shutdown forever. Once the registration is
    // removed, NATS providers fail any calls that are still waiting.
    drain::drain(active_requests_rx).await;

    // Best-effort: the TTL is the backstop when this cannot run.
    let key = registration_key(&instance_id, &identity_token);
    delete_own_registration(&registry, &key, revision).await;
    outcome
}

fn signal_started(started: Option<tokio::sync::oneshot::Sender<()>>) {
    if let Some(started) = started {
        let _ = started.send(());
    }
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
    let mut renewals = Box::pin(harnx_nats_common::registry::refreshes(
        refresh.registry.clone(),
        registration_key(refresh.instance_id, refresh.identity_token),
        serde_json::to_vec(refresh.registration)?.into(),
        REGISTRATION_REFRESH_INTERVAL,
    ));
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
                let context = request_context.clone();
                tokio::spawn(async move { handle_control(control, &context).await; });
            }
            Some(renewal) = renewals.next() => {
                match renewal {
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
    let _activity = harnx_nats_common::rpc::RequestActivity::start(&context.client, &message);
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
            execution::complete_without_invocation(context, &request).await?;
            reply.call_id.clone_from(&request.call_id);
            finalize_execution_context(context, &request, &mut reply);
            return publish_reply(&context.client, reply_subject, &reply).await;
        }
        CacheReservation::Wait(reply) => {
            let mut reply = wait_for_cached_reply(reply).await?;
            execution::complete_without_invocation(context, &request).await?;
            reply.call_id.clone_from(&request.call_id);
            finalize_execution_context(context, &request, &mut reply);
            return publish_reply(&context.client, reply_subject, &reply).await;
        }
        CacheReservation::Full => {
            execution::complete_without_invocation(context, &request).await?;
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
    let recovery = recovery::InvocationRecovery::load(context, request).await?;
    if let Some(reply) = recovery.completed_reply(context).await? {
        return reply_result(reply);
    }
    recovery.check_policy(context).await?;
    let execution = execution::InvocationExecution::claim(
        &context.execution_store,
        request,
        &context.server_identity,
    )
    .await
    .map_err(|error| ToolInvokeError::Fatal(format!("register tool execution: {error:#}")))?;
    let cancel = CancellationToken::new();
    let (stopped, stopped_rx) = watch::channel(false);
    context.in_flight.lock().await.insert(
        request.call_id.clone(),
        execution::ActiveCall {
            reference: execution.reference.clone(),
            cancel: cancel.clone(),
            stopped: stopped_rx,
        },
    );
    let mut args = std::mem::take(&mut request.args);
    let invocation_context = ToolInvocationContext {
        operation: Some(execution.reference.clone()),
        call_id: request.call_id.clone(),
        invoking_session_id: request.parent_session_id.clone(),
        capabilities: request.capabilities.clone(),
    };
    add_parent_context_args(
        &request.tool,
        request.parent_session_id.take(),
        request.tool_call_id.take(),
        &mut args,
    );
    let metric_tool = metric_tool_name(context.toolset.as_ref(), &request.tool);
    let start = Instant::now();
    let guarantee = context
        .toolset
        .tools()
        .iter()
        .find(|spec| spec.name == request.tool)
        .map(|spec| spec.cancellation_guarantee)
        .unwrap_or_default();
    let invocation = ToolInvocation {
        tool: request.tool.clone(),
        args,
        context: invocation_context,
        cancel: cancel.clone(),
    };
    let invocation = recovery
        .invoke(context.toolset.as_ref(), invocation)
        .instrument(tool_exec_span(&request.tool, parent_cx));
    let result = execution
        .invoke(cancel, guarantee, invocation, stopped)
        .await;
    context.in_flight.lock().await.remove(&request.call_id);
    let elapsed = start.elapsed();
    let is_ok = result.is_ok();
    harnx_metrics::record_tool_call(metric_tool, is_ok, elapsed);
    result
}

fn reply_result(reply: ToolReply) -> Result<Value, ToolInvokeError> {
    reply.result.map_err(|error| match error {
        ToolErrorPayload::Recoverable(message) => ToolInvokeError::Recoverable(message),
        ToolErrorPayload::Fatal(message) => ToolInvokeError::Fatal(message),
    })
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
    finalize_execution_context_value(
        context.server_scope.as_str(),
        &context.server_identity,
        request,
        result,
    );
}

fn finalize_execution_context_value(
    server_scope: &str,
    server_identity: &str,
    request: &RequestAttestation,
    result: &mut Value,
) {
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
                server_identity,
                request.tool
            );
            return;
        }
    };
    observation.provenance = Some(ToolObservationProvenance::new(
        server_scope,
        server_identity,
        request.tool.clone(),
        request.call_id.clone(),
    ));
    if let Err(error) = observation.validate() {
        log::warn!(
            "stripping invalid execution context from tool result: server={} tool={} error={error:#}",
            server_identity,
            request.tool
        );
        return;
    }
    if let Ok(value) = serde_json::to_value(observation) {
        put_result_execution_context(result, value);
    }
}

fn add_parent_context_args(
    tool: &str,
    parent_session_id: Option<String>,
    tool_call_id: Option<String>,
    args: &mut Value,
) {
    let Some(args) = args.as_object_mut() else {
        return;
    };
    // These are transport-owned arguments. Always discard model-supplied
    // values before optionally replacing them with context from ToolRequest.
    args.remove("__harnx_parent_session_id");
    args.remove("__harnx_tool_call_id");
    if let Some(parent_session_id) = parent_session_id.filter(|_| accepts_parent_session_id(tool)) {
        args.insert(
            "__harnx_parent_session_id".to_string(),
            Value::String(parent_session_id),
        );
        if let Some(tool_call_id) = tool_call_id {
            args.insert(
                "__harnx_tool_call_id".to_string(),
                Value::String(tool_call_id),
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
    let reply_subject = harnx_nats_common::rpc::ReplyTarget::from_message(&message)?;
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
    if let Err(error) = recovery::validate_replay(context, &request).await {
        return publish_recoverable_reply(
            &context.client,
            reply_subject,
            request.call_id,
            format!("reject tool replay: {error:#}"),
        )
        .await
        .map(|_| None);
    }
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
    subject: harnx_nats_common::rpc::ReplyTarget,
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
    subject: harnx_nats_common::rpc::ReplyTarget,
    reply: &ToolReply,
) -> Result<()> {
    let payload = serde_json::to_vec(reply).context("encode tool reply")?;
    subject
        .send(client, payload)
        .await
        .context("publish tool reply")
}

fn header_value(message: &async_nats::Message, name: &str) -> Option<String> {
    message
        .headers
        .as_ref()?
        .get(name)
        .map(|value| value.as_str().to_owned())
}

async fn ensure_control_stores(
    client: &async_nats::Client,
    replicas: usize,
) -> Result<(kv::Store, harnx_execution_control::ExecutionStore)> {
    let js = jetstream::new(client.clone());
    Ok((
        ensure_registry_bucket(&js, replicas).await?,
        harnx_execution_control::ExecutionStore::ensure(&js, replicas).await?,
    ))
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

fn print_toolset_help() {
    eprintln!("Options:");
    eprintln!("  --mcp-stdio               Use MCP stdio transport instead of NATS");
    eprintln!("  --metrics-addr <ADDR>     Serve Prometheus metrics at http://ADDR/metrics.");
    eprintln!("                            Blank host binds 0.0.0.0, e.g. :8456. Unset disables.");
    eprintln!("                            Also honors HARNX_METRICS_ADDR env.");
    eprintln!("  --healthz-addr <ADDR>     Serve readiness checks at http://ADDR/healthz.");
    eprintln!("                            Blank host binds 0.0.0.0, e.g. :8457. Unset disables.");
    eprintln!("                            Also honors HARNX_HEALTHZ_ADDR env.");
    eprintln!("  --help, -h                Show this help message");
}

/// Run a toolset in MCP stdio mode when `--mcp-stdio` is present, otherwise
/// NATS mode.
///
/// In NATS mode, wires SIGTERM/Ctrl+C to a graceful stop so a pod killed by
/// Kubernetes gets a chance to remove its own registration instead of
/// leaving it for the TTL.
///
/// **Strict front-parser binaries:** `harnx-{bash,fs,grep}-tools` have their own argument
/// parsers that reject unknown flags BEFORE delegating here. When adding any new
/// cross-cutting CLI flag to this shared entry point, update EACH front parser to
/// recognize and skip both `--flag VALUE` (separate) and `--flag=VALUE` (equals) forms.
/// Use EXACT match (`arg == "--flag"` or `arg.strip_prefix("--flag=")`), not
/// `starts_with("--flag")`, to reject near-prefix typos like `--flag-typo`.
pub async fn run_toolset_main<T>(toolset: T) -> Result<()>
where
    T: Toolset + 'static,
{
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);

    if std::env::args_os().any(|arg| arg == "--help" || arg == "-h") {
        print_toolset_help();
        return Ok(());
    }

    let metrics_addr = harnx_metrics::metrics_addr_from_args(
        std::env::args_os().map(|arg| arg.to_string_lossy().into_owned()),
    )
    .or_else(|| std::env::var("HARNX_METRICS_ADDR").ok());
    harnx_metrics::init(&harnx_metrics::MetricsFlags { metrics_addr })?;

    let healthz_addr = harnx_healthz::healthz_addr_from_args(std::env::args())
        .or_else(|| std::env::var("HARNX_HEALTHZ_ADDR").ok());
    let readiness = harnx_healthz::init(&harnx_healthz::HealthzFlags { healthz_addr }).await?;

    let service_name = format!("harnx-{}-server", toolset.name());
    let telemetry = harnx_telemetry::init_telemetry(&service_name)?;

    let result: Result<()> = async {
        let toolset: Arc<dyn Toolset> = Arc::new(toolset);
        if std::env::args_os().any(|arg| arg == "--mcp-stdio") {
            let service = McpToolsetAdapter { toolset }
                .serve(rmcp::transport::stdio())
                .await
                .context("start MCP stdio server")?;
            if let Some(readiness) = readiness.as_ref() {
                readiness.ready();
            }
            let outcome = service.waiting().await.context("run MCP stdio server");
            if let Some(readiness) = readiness.as_ref() {
                readiness.not_ready();
            }
            outcome?;
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
        serve_with_shutdown(
            toolset,
            scope,
            connection,
            ServeLifecycle::new(shutdown, readiness),
        )
        .await
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
    /// `toolset.invoke_with_context` directly). Any cross-cutting concern (metrics, tracing, auth)
    /// added at one seam does NOT automatically cover the other. rmcp `--http` servers
    /// use their own `ServerHandler::call_tool`, a third seam.
    async fn dispatch_call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let tool_name = request.name.clone();
        let args = Value::Object(request.arguments.unwrap_or_default());
        let capabilities = context
            .meta
            .contains_key(EXECUTION_CONTEXT_NAMESPACE)
            .then(|| EXECUTION_CONTEXT_NAMESPACE.to_string())
            .into_iter()
            .collect();
        let invocation_context = ToolInvocationContext {
            operation: None,
            call_id: format!("{:?}", context.id),
            invoking_session_id: None,
            capabilities,
        };
        let attestation = RequestAttestation {
            call_id: invocation_context.call_id.clone(),
            tool: tool_name.to_string(),
            capabilities: invocation_context.capabilities.clone(),
        };
        let metric_tool = metric_tool_name(self.toolset.as_ref(), &tool_name);
        let started = Instant::now();
        let mut result = self
            .toolset
            .invoke_with_context(ToolInvocation {
                tool: tool_name.to_string(),
                args,
                context: invocation_context.clone(),
                cancel: CancellationToken::new(),
            })
            .await;
        harnx_metrics::record_tool_call(metric_tool, result.is_ok(), started.elapsed());

        if let Ok(value) = &mut result {
            finalize_execution_context_value("mcp", self.toolset.name(), &attestation, value);
        }
        match result {
            Ok(value) => Ok(call_tool_result_from_value(value)),
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(
                error.to_string(),
            )])),
        }
    }
}

fn call_tool_result_from_value(value: Value) -> CallToolResult {
    if let Ok(result) = serde_json::from_value::<CallToolResult>(value.clone()) {
        return result;
    }
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    CallToolResult::success(vec![ContentBlock::text(text)])
}

#[cfg(test)]
mod tests {
    use opentelemetry::trace::{SpanId, SpanKind, TraceId};
    use rmcp::model::RequestParamsMeta;

    use super::*;

    #[test]
    fn parent_session_argument_only_uses_transport_context() {
        let mut untrusted = serde_json::json!({
            "__harnx_parent_session_id": "other-session",
            "__harnx_tool_call_id": "model-supplied-call",
        });
        add_parent_context_args(SUBAGENT_SESSION_NEW_TOOL, None, None, &mut untrusted);
        assert!(untrusted.get("__harnx_parent_session_id").is_none());
        assert!(untrusted.get("__harnx_tool_call_id").is_none());

        add_parent_context_args(
            SUBAGENT_SESSION_NEW_TOOL,
            Some("attested-session".to_string()),
            None,
            &mut untrusted,
        );
        assert_eq!(
            untrusted,
            serde_json::json!({"__harnx_parent_session_id": "attested-session"})
        );
    }

    #[test]
    fn mcp_adapter_preserves_serialized_call_tool_results() {
        let value = serde_json::json!({
            "content": [{"type": "text", "text": "hello"}],
            "structuredContent": {"answer": 42},
            "isError": true,
            "_meta": {"private": "value"}
        });
        let result = call_tool_result_from_value(value.clone());
        assert_eq!(serde_json::to_value(result).unwrap(), value);
    }

    #[test]
    fn mcp_adapter_wraps_raw_json_values_as_text() {
        let result = call_tool_result_from_value(serde_json::json!({"answer": 42}));
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.content.len(), 1);
        assert!(serde_json::to_value(&result.content[0]).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("\"answer\": 42"));
    }

    fn result_with_execution_context() -> Value {
        let observation = ExecutionContextObservation::observe(
            std::path::Path::new("/workspace"),
            std::path::Path::new("/workspace"),
        );
        serde_json::json!({
            "content": [],
            "_meta": {EXECUTION_CONTEXT_NAMESPACE: observation}
        })
    }

    #[test]
    fn mcp_adapter_strips_unrequested_execution_context() {
        let mut result = result_with_execution_context();
        finalize_execution_context_value(
            "mcp",
            "bash",
            &RequestAttestation {
                call_id: "request-1".to_string(),
                tool: "exec".to_string(),
                capabilities: Default::default(),
            },
            &mut result,
        );

        assert!(result.get("_meta").is_none());
    }

    #[test]
    fn mcp_adapter_attests_requested_execution_context() {
        let mut result = result_with_execution_context();
        finalize_execution_context_value(
            "mcp",
            "bash",
            &RequestAttestation {
                call_id: "request-1".to_string(),
                tool: "exec".to_string(),
                capabilities: std::collections::BTreeSet::from([
                    EXECUTION_CONTEXT_NAMESPACE.to_string()
                ]),
            },
            &mut result,
        );

        let provenance = &result["_meta"][EXECUTION_CONTEXT_NAMESPACE]["provenance"];
        assert_eq!(provenance["server_scope"], "mcp");
        assert_eq!(provenance["server_identity"], "bash");
        assert_eq!(provenance["tool_name"], "exec");
        assert_eq!(provenance["call_id"], "request-1");
    }

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
    fn parent_context_args_include_parent_tool_call_id() {
        let mut args = serde_json::json!({ "message": "delegate" });

        add_parent_context_args(
            SUBAGENT_SESSION_PROMPT_TOOL,
            Some("parent-session".to_string()),
            Some("parent-tool-call".to_string()),
            &mut args,
        );

        assert_eq!(args["__harnx_parent_session_id"], "parent-session");
        assert_eq!(args["__harnx_tool_call_id"], "parent-tool-call");
    }

    struct MetricsTestToolset;

    #[async_trait::async_trait]
    impl Toolset for MetricsTestToolset {
        fn name(&self) -> &str {
            "metrics-test"
        }

        fn tools(&self) -> Vec<harnx_toolset::ToolSpec> {
            vec![harnx_toolset::ToolSpec {
                cancellation_guarantee: Default::default(),
                name: "known".to_owned(),
                description: "known test tool".to_owned(),
                input_schema: serde_json::json!({ "type": "object" }),
                idempotent_hint: false,
                read_only_hint: true,
                timeout_secs: None,
                meta: None,
            }]
        }

        async fn invoke(
            &self,
            _tool: &str,
            _args: Value,
            _cancel: CancellationToken,
        ) -> Result<Value, ToolInvokeError> {
            unreachable!("metric label test does not invoke tools")
        }
    }

    #[test]
    fn distinct_unknown_tools_share_one_metric_series() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        harnx_core::require_nextest();
        let toolset = MetricsTestToolset;
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            for requested in ["attacker-tool-one", "attacker-tool-two"] {
                harnx_metrics::record_tool_call(
                    metric_tool_name(&toolset, requested),
                    false,
                    Duration::from_millis(1),
                );
            }
        });

        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(snapshot.len(), 2, "unknown names must share both series");
        assert!(snapshot.iter().all(|(key, _, _, _)| key
            .key()
            .labels()
            .any(|label| label.key() == "tool" && label.value() == "unknown")));
        assert!(snapshot.iter().any(|(key, _, _, value)| {
            key.key().name() == harnx_metrics::TOOL_CALLS_TOTAL && *value == DebugValue::Counter(2)
        }));
        assert!(snapshot.iter().any(|(key, _, _, value)| {
            key.key().name() == harnx_metrics::TOOL_CALL_DURATION_SECONDS
                && matches!(value, DebugValue::Histogram(samples) if samples.len() == 2)
        }));
    }

    fn assert_success_and_error_tool_metric_snapshot() {
        use metrics::{Key, Label};
        use metrics_util::{
            debugging::{DebugValue, DebuggingRecorder},
            CompositeKey, MetricKind,
        };

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let ok_elapsed = Duration::from_millis(100);
        let error_elapsed = Duration::from_millis(50);
        metrics::with_local_recorder(&recorder, || {
            harnx_metrics::record_tool_call("test_tool_ok", true, ok_elapsed);
            harnx_metrics::record_tool_call("test_tool_err", false, error_elapsed);
        });

        let key = |kind, name, labels: &[(&str, &str)]| {
            CompositeKey::new(
                kind,
                Key::from_parts(
                    name,
                    labels
                        .iter()
                        .map(|(key, value)| Label::new((*key).to_owned(), (*value).to_owned()))
                        .collect::<Vec<_>>(),
                ),
            )
        };
        assert_eq!(
            snapshotter.snapshot().into_vec(),
            vec![
                (
                    key(
                        MetricKind::Counter,
                        harnx_metrics::TOOL_CALLS_TOTAL,
                        &[("tool", "test_tool_ok"), ("status", "ok")],
                    ),
                    None,
                    None,
                    DebugValue::Counter(1),
                ),
                (
                    key(
                        MetricKind::Histogram,
                        harnx_metrics::TOOL_CALL_DURATION_SECONDS,
                        &[("tool", "test_tool_ok")],
                    ),
                    None,
                    None,
                    DebugValue::Histogram(vec![ok_elapsed.as_secs_f64().into()]),
                ),
                (
                    key(
                        MetricKind::Counter,
                        harnx_metrics::TOOL_CALLS_TOTAL,
                        &[("tool", "test_tool_err"), ("status", "error")],
                    ),
                    None,
                    None,
                    DebugValue::Counter(1),
                ),
                (
                    key(
                        MetricKind::Histogram,
                        harnx_metrics::TOOL_CALL_DURATION_SECONDS,
                        &[("tool", "test_tool_err")],
                    ),
                    None,
                    None,
                    DebugValue::Histogram(vec![error_elapsed.as_secs_f64().into()]),
                ),
            ]
        );
    }

    #[test]
    fn tool_call_metrics_recorded_on_success_and_error() {
        harnx_core::require_nextest();
        assert_success_and_error_tool_metric_snapshot();
    }
}
