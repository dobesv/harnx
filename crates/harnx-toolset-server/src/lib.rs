//! Server-side adapters for hosting a [`harnx_toolset::Toolset`].

use std::time::Duration;

pub mod cancellation_client;
mod control;
use control::handle_control;
mod aggregate;
mod cleanup_tasks;
pub mod content;
mod drain;
mod execution;
pub mod filter;
mod invocation;
use invocation::invoke_uncached_tool;
#[cfg(test)]
use invocation::{
    accepts_parent_session_id, add_parent_context_args, metric_tool_name, tool_exec_span,
};
mod mcp;
#[cfg(test)]
use mcp::call_tool_result_from_value;
mod lifecycle;
mod runner;
pub use runner::{
    run_toolset_main, run_toolset_main_with_factory,
    run_toolset_main_with_factory_and_http_options, run_toolset_mcp_http, McpHttpOptions,
};
mod name;
mod progress;
mod registration_identity;
pub mod schema;
mod subscriptions;
mod tool_observation;
use tool_observation::*;

pub use aggregate::{serve_many_with_shutdown, serve_many_with_shutdown_and_filter};
pub use filter::{
    compile_enable_globs, enable_tools_from_args, tool_enabled, ArcToolsetWrapper, FilteredToolset,
    ENABLE_TOOL_FLAG,
};
pub use lifecycle::{RegistrationShutdown, ServeLifecycle};
pub use name::{toolset_name_from_args, validate_toolset_name, NamedToolset, NAME_FLAG};
pub use registration_identity::RegistrationIdentity;
// Re-export globset for callers who need to construct filters
pub use globset;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv};
use drain::InFlightRequests;
use futures_util::StreamExt;
use harnx_core::execution_context::{
    put_result_execution_context, take_result_execution_context, ExecutionContextObservation,
    ToolObservationProvenance, EXECUTION_CONTEXT_NAMESPACE,
};
use harnx_core::instance::ServerScope;
use harnx_healthz::Readiness;
use harnx_nats_common::connect::NatsConnection;
use harnx_toolset::{
    server_identity_token, ControlMessage, Registration, ToolErrorPayload, ToolInvocation,
    ToolInvocationContext, ToolInvokeError, ToolReply, ToolRequest, Toolset, HDR_CALL_ID,
    HDR_IDEMPOTENCY_KEY, SUBAGENT_SESSION_NEW_TOOL, SUBAGENT_SESSION_PROMPT_TOOL,
    SUBAGENT_SESSION_SEND_TOOL, SUBAGENT_SESSION_WAIT_TOOL,
};
use opentelemetry::Context as OtelContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, ListToolsResult, MetaObject, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ServerHandler;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{watch, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

pub const TOOL_REGISTRY_BUCKET: &str = "harnx_tool_registry";
pub use harnx_toolset::TOOL_PROTOCOL_VERSION;

pub mod invocation_journal;
mod recovery;
mod reply_cache;
pub mod reply_limits;
use reply_cache::*;
pub const TOOL_SCHEMA_VERSION: u32 = 1;

const IDEMPOTENCY_CACHE_TTL: Duration = Duration::from_secs(60);
const IDEMPOTENCY_CACHE_MAX_ENTRIES: usize = 1_024;
const REGISTRATION_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

type InFlight = Arc<Mutex<HashMap<String, execution::ActiveCall>>>;
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
    journal: invocation_journal::InvocationJournal,
    /// The configured replica count, so the journal bucket's durability can be
    /// reconciled with the connection's while the server runs.
    replicas: usize,
    cleanup: Arc<cleanup_tasks::CleanupTasks>,
    /// Optional filter for tool admission. When set, requests for tools not
    /// matching the filter are rejected before cache/journal lookup.
    filter: Option<Arc<globset::GlobSet>>,
}

struct ValidatedToolRequest {
    reply_subject: harnx_nats_common::rpc::ReplyTarget,
    request: ToolRequest,
    parent_cx: OtelContext,
}

pub struct ServeConfig {
    pub instance_id: ServerScope,
    pub connection: NatsConnection,
    pub lifecycle: ServeLifecycle,
    pub filter: Option<Arc<globset::GlobSet>>,
}

struct ServeSettings {
    instance_id: ServerScope,
    connection: NatsConnection,
    lifecycle: ServeLifecycle,
    identity: RegistrationIdentity,
    started: Option<tokio::sync::oneshot::Sender<()>>,
    /// Optional filter for tool admission. When set, requests for tools not
    /// matching the filter are rejected before cache/journal lookup.
    filter: Option<Arc<globset::GlobSet>>,
}

/// Everything `serve_requests` needs to keep the KV registration alive, bundled
/// so the function stays under the argument-count limit.
struct RegistrationRefresh<'a> {
    registry: &'a kv::Store,
    instance_id: &'a ServerScope,
    identity_token: &'a str,
    registration: &'a Registration,
    interval: Duration,
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
        ..Default::default()
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
            filter: None,
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
    serve_with_config(
        toolset,
        ServeConfig {
            instance_id,
            connection,
            lifecycle,
            filter: None,
        },
    )
    .await
}

/// Serve a toolset using an existing NATS connection and configured lifecycle.
pub async fn serve_with_config(toolset: Arc<dyn Toolset>, config: ServeConfig) -> Result<()> {
    serve_configured(
        toolset,
        ServeSettings {
            instance_id: config.instance_id,
            connection: config.connection,
            lifecycle: config.lifecycle,
            identity: RegistrationIdentity::from_env(),
            started: None,
            filter: config.filter,
        },
    )
    .await
}

struct SubscriptionSetup {
    tool_requests: async_nats::Subscriber,
    controls: async_nats::Subscriber,
    identity_token: String,
    registration: Registration,
    registry: kv::Store,
    journal: invocation_journal::InvocationJournal,
}

async fn setup_subscriptions(
    toolset: &Arc<dyn Toolset>,
    settings: &ServeSettings,
    readiness: Option<&Readiness>,
) -> Result<SubscriptionSetup> {
    let RegistrationIdentity { package, config } = settings.identity.clone();
    let server_name = toolset.name().to_owned();
    let identity_token = server_identity_token(package.as_deref(), &config, &server_name);
    let (tool_requests, controls) = subscriptions::subscribe_to_requests(
        &settings.connection.client,
        &settings.instance_id,
        &identity_token,
        readiness,
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
    let (registry, journal) =
        ensure_control_stores(&settings.connection.client, settings.connection.replicas).await?;
    Ok(SubscriptionSetup {
        tool_requests,
        controls,
        identity_token,
        registration,
        registry,
        journal,
    })
}

struct ServeCleanup {
    client: async_nats::Client,
    readiness: Option<Readiness>,
    tool_requests: async_nats::Subscriber,
    request_context: ToolRequestContext,
    active_requests_rx: watch::Receiver<usize>,
    registration_shutdown: RegistrationShutdown,
    registry: kv::Store,
    instance_id: ServerScope,
    identity_token: String,
    revision: u64,
}

async fn finish_shutdown(cleanup: ServeCleanup, outcome: Result<()>) -> Result<()> {
    let ServeCleanup {
        client,
        readiness,
        mut tool_requests,
        request_context,
        active_requests_rx,
        registration_shutdown,
        registry,
        instance_id,
        identity_token,
        revision,
    } = cleanup;
    // Mark readiness not-ready so k8s stops routing traffic, then drain the
    // tool-request subscription so the broker removes this member from the
    // queue group (new calls route to siblings). The NATS connection stays
    // alive so in-flight handlers can still reply.
    if let Some(readiness) = readiness.as_ref() {
        readiness.not_ready();
    }

    // Drain the subscription: broker removes this member from the queue group
    // so new calls route to siblings, but the local receiver stays open to
    // deliver any messages already in flight. Log errors but continue so we
    // still run handler-drain and registration cleanup.
    let drain_succeeded = tool_requests.drain().await.is_ok();
    if !drain_succeeded {
        log::warn!("failed to drain tool request subscription");
    }
    if let Err(error) = client.flush().await {
        log::warn!("failed to flush after draining tool request subscription: {error}");
    }

    // Only process buffered messages if drain succeeded.
    // If drain failed, the subscription may still be registered with the broker,
    // and waiting for messages could block indefinitely.
    if drain_succeeded {
        // Continue dispatching any tool requests that were already buffered in
        // the subscription receiver when we initiated drain. The stream closes
        // once all queued messages have been delivered.
        // Bound the wait so a slow stream termination cannot stall shutdown.
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(request) = tool_requests.next().await {
                spawn_tool_request(request_context.clone(), request);
            }
        })
        .await;
    }

    // Give callers already waiting on a reply a chance to get one: wait for
    // in-flight requests to finish before deregistering, bounded so a stuck
    // invocation can't stall shutdown forever. Once the registration is
    // removed, NATS providers fail any calls that are still waiting.
    drain::drain(active_requests_rx).await;

    // Best-effort: the TTL is the backstop when this cannot run.
    // In HA mode (RegistrationShutdown::Expire), skip deletion and let the
    // bucket TTL reap the key after the last replica exits.
    let key = registration_key(&instance_id, &identity_token);
    if registration_shutdown == RegistrationShutdown::DeleteIfCurrent {
        delete_own_registration(&registry, &key, revision).await;
    }
    outcome
}

async fn initialize_server(
    toolset: Arc<dyn Toolset>,
    setup: SubscriptionSetup,
    settings: &ServeSettings,
) -> Result<(
    async_nats::Subscriber,
    async_nats::Subscriber,
    String,
    kv::Store,
    Registration,
    u64,
    ToolRequestContext,
    watch::Receiver<usize>,
)> {
    let SubscriptionSetup {
        tool_requests,
        controls,
        identity_token,
        registration,
        registry,
        journal,
    } = setup;
    let revision = publish_registration(&registry, &settings.instance_id, &registration).await?;
    let (active_requests, active_requests_rx) = InFlightRequests::new();
    let request_context = request_context(
        (&settings.connection.client, toolset),
        (&settings.instance_id, identity_token.clone()),
        (active_requests, journal, settings.connection.replicas),
        settings.filter.clone(),
    );
    Ok((
        tool_requests,
        controls,
        identity_token,
        registry,
        registration,
        revision,
        request_context,
        active_requests_rx,
    ))
}

async fn run_server_loop(
    context: &ToolRequestContext,
    subscriptions: ToolSubscriptions<'_>,
    refresh: RegistrationRefresh<'_>,
) -> Result<()> {
    serve_requests(context, subscriptions, refresh).await
}

async fn serve_configured(toolset: Arc<dyn Toolset>, settings: ServeSettings) -> Result<()> {
    let setup = setup_subscriptions(&toolset, &settings, settings.lifecycle.readiness()).await?;
    let (
        mut tool_requests,
        mut controls,
        identity_token,
        registry,
        registration,
        mut revision,
        request_context,
        active_requests_rx,
    ) = initialize_server(toolset, setup, &settings).await?;
    let ServeSettings {
        instance_id,
        connection,
        lifecycle,
        started,
        ..
    } = settings;
    let refresh_interval = lifecycle.refresh_interval();
    let (shutdown, readiness, registration_shutdown) = lifecycle.into_parts();
    let NatsConnection { client, .. } = connection;
    signal_started(started);

    let outcome = run_server_loop(
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
            interval: refresh_interval,
            revision: &mut revision,
        },
    )
    .await;

    finish_shutdown(
        ServeCleanup {
            client,
            readiness,
            tool_requests,
            request_context,
            active_requests_rx,
            registration_shutdown,
            registry,
            instance_id,
            identity_token,
            revision,
        },
        outcome,
    )
    .await
}

fn request_context(
    (client, toolset): (&async_nats::Client, Arc<dyn Toolset>),
    (instance_id, server_identity): (&ServerScope, String),
    (active_requests, journal, replicas): (
        InFlightRequests,
        invocation_journal::InvocationJournal,
        usize,
    ),
    filter: Option<Arc<globset::GlobSet>>,
) -> ToolRequestContext {
    ToolRequestContext {
        client: client.clone(),
        toolset,
        in_flight: Arc::default(),
        reply_cache: Arc::default(),
        active_requests,
        server_scope: instance_id.clone(),
        server_identity,
        journal,
        replicas,
        cleanup: Arc::default(),
        filter,
    }
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
    let mut journal_reconciliation = Box::pin(invocation_journal::config_reconciliations(
        jetstream::new(request_context.client.clone()),
        request_context.replicas,
        refresh.interval,
    ));
    let mut renewals = Box::pin(harnx_nats_common::registry::refreshes(
        refresh.registry.clone(),
        registration_key(refresh.instance_id, refresh.identity_token),
        serde_json::to_vec(refresh.registration)?.into(),
        refresh.interval,
    ));
    loop {
        tokio::select! {
            Some(result) = journal_reconciliation.next() => {
                if let Err(error) = result {
                    log::warn!("reconcile invocation journal config failed; retrying next interval: {error:#}");
                }
            }
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
        request,
        parent_cx,
    } = validated;
    let key = cache_key(&request)?;
    let completion = match reserve_cache_entry(&context.reply_cache, &key).await {
        CacheReservation::Complete(saved) => {
            return serve_cached(context, reply_subject, saved).await;
        }
        CacheReservation::Wait(reply) => {
            let saved = wait_for_cached_reply(reply).await?;
            return serve_cached(context, reply_subject, saved).await;
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

    let reply = Arc::new(
        invoke_uncached_tool(context, &request, parent_cx)
            .await
            .unwrap_or_else(|error| ToolReply {
                call_id: request.call_id.clone(),
                result: Err(map_invoke_error(error)),
                final_progress: None,
            }),
    );
    complete_cache_entry(&context.reply_cache, key, reply.clone(), completion).await;
    serve_cached(context, reply_subject, reply).await
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
    if header_value(&message, HDR_IDEMPOTENCY_KEY).is_none() {
        publish_recoverable_reply(
            &context.client,
            reply_subject,
            request.call_id,
            "tool request is missing Idempotency-Key header".to_string(),
        )
        .await?;
        return Ok(None);
    }

    if !check_tool_admission(context, &request, reply_subject.clone()).await? {
        return Ok(None);
    }

    let validation = async {
        prepare(context, &request).await?;
        context.journal.validate_replay(&request).await
    }
    .await;
    if let Err(error) = validation {
        let reply = ToolReply {
            call_id: request.call_id,
            result: Err(map_invoke_error(recovery::invoke_error(error))),
            final_progress: None,
        };
        return publish_reply(&context.client, reply_subject, &reply)
            .await
            .map(|_| None);
    }
    Ok(Some(ValidatedToolRequest {
        reply_subject,
        request,
        parent_cx,
    }))
}

async fn check_tool_admission(
    context: &ToolRequestContext,
    request: &ToolRequest,
    reply_subject: harnx_nats_common::rpc::ReplyTarget,
) -> Result<bool> {
    let Some(filter_set) = &context.filter else {
        return Ok(true);
    };
    if tool_enabled(filter_set, &request.tool) {
        return Ok(true);
    }
    publish_recoverable_reply(
        &context.client,
        reply_subject,
        request.call_id.clone(),
        format!("tool '{}' is not available on this server", request.tool),
    )
    .await?;
    Ok(false)
}

/// Give every call a journal row before it runs. A worker records its own
/// invocations before dispatching them; a client calling a tool server directly
/// has nowhere else to record one, and without a row the call has no durable
/// place for its checkpoint or its reply.
///
/// A row written here is a direct-client row: it carries this server's own
/// scope and identity, and the round the request names. A direct client names
/// none, so the row records round zero, and
/// [`invocation_journal::InvocationJournal::find`], which resolves a worker's
/// `ToolCalls` round back to its invocation, never matches it — correctly,
/// since these calls are not in any worker's transcript.
async fn prepare(context: &ToolRequestContext, request: &ToolRequest) -> Result<()> {
    if context.journal.get(request).await?.is_some() {
        return Ok(());
    }
    context
        .journal
        .record(
            request,
            (
                &request.tool,
                context.server_scope.as_str(),
                &context.server_identity,
            ),
        )
        .await
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
            final_progress: None,
        },
    )
    .await
}

fn map_invoke_error(error: ToolInvokeError) -> ToolErrorPayload {
    match error {
        ToolInvokeError::Recoverable(message) => ToolErrorPayload::Recoverable(message),
        ToolInvokeError::Fatal(message) => ToolErrorPayload::Fatal(message),
        ToolInvokeError::Interrupted(interrupted) => ToolErrorPayload::Interrupted(interrupted),
    }
}

async fn publish_reply(
    client: &async_nats::Client,
    subject: harnx_nats_common::rpc::ReplyTarget,
    reply: &ToolReply,
) -> Result<()> {
    let payload = serde_json::to_vec(reply).context("encode tool reply")?;
    // ReplyTarget sends no headers, even when it asks for a delivery receipt.
    // Do not make an unjournaled replacement if limits changed after commit.
    anyhow::ensure!(
        payload.len() <= client.server_info().max_payload,
        "durable tool reply exceeds current broker payload limit"
    );
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
) -> Result<(kv::Store, invocation_journal::InvocationJournal)> {
    let js = jetstream::new(client.clone());
    Ok((
        ensure_registry_bucket(&js, replicas).await?,
        invocation_journal::InvocationJournal::ensure(&js, replicas).await?,
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

#[cfg(test)]
mod tests;
