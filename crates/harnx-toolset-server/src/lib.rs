//! Server-side adapters for hosting a [`harnx_toolset::Toolset`].

pub mod content;
pub mod schema;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv};
use futures_util::StreamExt;
use harnx_core::instance::{InstanceId, HARNX_INSTANCE_ID};
use harnx_toolset::{
    server_identity_token, ControlKind, ControlMessage, Registration, ToolErrorPayload,
    ToolInvokeError, ToolReply, ToolRequest, Toolset, HARNX_SERVER_CONFIG, HARNX_SERVER_PACKAGE,
    HDR_CALL_ID, HDR_IDEMPOTENCY_KEY,
};
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

pub const TOOL_REGISTRY_BUCKET: &str = "harnx_tool_registry";
pub const TOOL_PROTOCOL_VERSION: u32 = 1;
pub const TOOL_SCHEMA_VERSION: u32 = 1;

const HARNX_NATS_URL: &str = "HARNX_NATS_URL";
const HARNX_NATS_TOKEN: &str = "HARNX_NATS_TOKEN";
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
}

struct ValidatedToolRequest {
    reply_subject: async_nats::Subject,
    request: ToolRequest,
    idempotency_key: String,
}

/// Everything `serve_requests` needs to keep the KV registration alive, bundled
/// so the function stays under the argument-count limit.
struct RegistrationRefresh<'a> {
    registry: &'a kv::Store,
    instance_id: &'a InstanceId,
    registration: &'a Registration,
    interval: &'a mut tokio::time::Interval,
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
pub fn registration_key(instance_id: &InstanceId, identity_token: &str) -> String {
    format!("{instance_id}.{identity_token}")
}

/// Host a toolset over Core NATS request-reply and publish its KV registration.
pub async fn serve_over_nats<T>(
    toolset: T,
    instance_id: InstanceId,
    nats_url: &str,
    token: &str,
) -> Result<()>
where
    T: Toolset + 'static,
{
    harnx_core::server_logging::init_server_logger();
    let client = async_nats::ConnectOptions::new()
        .token(token.to_owned())
        .connect(nats_url)
        .await
        .with_context(|| format!("connect to NATS at {nats_url}"))?;
    serve_with_client(Arc::new(toolset), instance_id, client).await
}

/// Serve a toolset using an existing NATS client.
pub async fn serve_with_client(
    toolset: Arc<dyn Toolset>,
    instance_id: InstanceId,
    client: async_nats::Client,
) -> Result<()> {
    // Never cancelled: this entry point has no shutdown signal of its own, so
    // it only ever exits through `serve_requests`' bail! conditions.
    serve_with_shutdown(toolset, instance_id, client, CancellationToken::new()).await
}

/// Serve a toolset using an existing NATS client, exiting cleanly (and
/// running exit cleanup while the connection is still usable) when
/// `shutdown` is cancelled, in addition to the usual failure exits.
pub async fn serve_with_shutdown(
    toolset: Arc<dyn Toolset>,
    instance_id: InstanceId,
    client: async_nats::Client,
    shutdown: CancellationToken,
) -> Result<()> {
    let server_name = toolset.name().to_owned();
    let package = std::env::var(HARNX_SERVER_PACKAGE)
        .ok()
        .filter(|package| !package.is_empty());
    let config = std::env::var(HARNX_SERVER_CONFIG).unwrap_or_default();
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
    let registry = ensure_registry_bucket(&jetstream).await?;
    publish_registration(&registry, &instance_id, &registration).await?;

    let request_context = ToolRequestContext {
        client: client.clone(),
        toolset,
        in_flight: Arc::new(Mutex::new(HashMap::new())),
        reply_cache: Arc::new(Mutex::new(HashMap::new())),
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
        },
    )
    .await;

    // Best-effort: the TTL is the backstop when this cannot run.
    let key = registration_key(&instance_id, &identity_token);
    if let Err(error) = registry.delete(&key).await {
        log::warn!("could not remove tool registration '{key}' on shutdown: {error}");
    }
    outcome
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
                if let Err(error) =
                    publish_registration(refresh.registry, refresh.instance_id, refresh.registration).await
                {
                    log::warn!("refresh tool registration failed; retrying next interval: {error:#}");
                }
            }
            _ = subscriptions.shutdown.cancelled() => {
                return Ok(());
            }
        }
    }
}

fn spawn_tool_request(context: ToolRequestContext, message: async_nats::Message) {
    tokio::spawn(async move {
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
        request,
        idempotency_key,
    } = validated;
    let completion = match reserve_cache_entry(&context.reply_cache, &idempotency_key).await {
        CacheReservation::Complete(mut reply) => {
            reply.call_id.clone_from(&request.call_id);
            return publish_reply(&context.client, reply_subject, &reply).await;
        }
        CacheReservation::Wait(reply) => {
            let mut reply = wait_for_cached_reply(reply).await?;
            reply.call_id.clone_from(&request.call_id);
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

    let cancel = CancellationToken::new();
    context
        .in_flight
        .lock()
        .await
        .insert(request.call_id.clone(), cancel.clone());
    let mut args = request.args;
    if request.tool.ends_with("_session_prompt") || request.tool.ends_with("_session_new") {
        if let (Some(parent_session_id), Some(args)) =
            (request.parent_session_id, args.as_object_mut())
        {
            args.insert(
                "__harnx_parent_session_id".to_string(),
                Value::String(parent_session_id),
            );
        }
    }
    let result = context.toolset.invoke(&request.tool, args, cancel).await;
    context.in_flight.lock().await.remove(&request.call_id);

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
    publish_reply(&context.client, reply_subject, &reply).await
}

async fn validate_tool_request(
    context: &ToolRequestContext,
    message: async_nats::Message,
) -> Result<Option<ValidatedToolRequest>> {
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

async fn ensure_registry_bucket(jetstream: &jetstream::Context) -> Result<kv::Store> {
    harnx_nats_common::registry::ensure_bucket_with_ttl(
        jetstream,
        TOOL_REGISTRY_BUCKET,
        harnx_nats_common::registry::REGISTRATION_TTL,
        1,
    )
    .await
}

async fn publish_registration(
    registry: &kv::Store,
    instance_id: &InstanceId,
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

/// Run a toolset in MCP stdio mode when `--mcp-stdio` is present, otherwise NATS mode.
pub async fn run_toolset_main<T>(toolset: T) -> Result<()>
where
    T: Toolset + 'static,
{
    harnx_core::server_logging::init_server_logger();
    let toolset: Arc<dyn Toolset> = Arc::new(toolset);
    if std::env::args_os().any(|arg| arg == "--mcp-stdio") {
        let service = McpToolsetAdapter { toolset }
            .serve(rmcp::transport::stdio())
            .await
            .context("start MCP stdio server")?;
        service.waiting().await.context("run MCP stdio server")?;
        return Ok(());
    }

    let instance_id = std::env::var(HARNX_INSTANCE_ID)
        .with_context(|| format!("{HARNX_INSTANCE_ID} is required"))?;
    let nats_url =
        std::env::var(HARNX_NATS_URL).with_context(|| format!("{HARNX_NATS_URL} is required"))?;
    let token = std::env::var(HARNX_NATS_TOKEN)
        .with_context(|| format!("{HARNX_NATS_TOKEN} is required"))?;
    let client = async_nats::ConnectOptions::new()
        .token(token)
        .connect(&nats_url)
        .await
        .with_context(|| format!("connect to NATS at {nats_url}"))?;
    serve_with_client(toolset, InstanceId::from_string(instance_id), client).await
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
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.dispatch_call_tool(request, _context)
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
    async fn dispatch_call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = Value::Object(request.arguments.unwrap_or_default());
        match self
            .toolset
            .invoke(&request.name, args, CancellationToken::new())
            .await
        {
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
    use super::*;

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
}
