//! Server-side adapter for hosting a [`harnx_hookset::Hook`] over Core NATS.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv};
use futures_util::{stream::select_all, StreamExt};
use harnx_core::hooks::{HookOutcome, HookPayload, HookResult, HookResultControl};
use harnx_core::instance::ServerScope;
use harnx_hookset::{Hook, HookRegistration, HookSpec, HOOK_PROTOCOL_VERSION, HOOK_SCHEMA_VERSION};
use harnx_nats_common::connect::NatsConnection;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub use harnx_hookset::HOOK_REGISTRY_BUCKET;

const REGISTRATION_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// KV key for one hook server's registration under a scope. The scope may
/// belong to a worker's own children or to an independently deployed set with
/// no worker at all.
pub fn hook_registration_key(instance_id: &ServerScope, server: &str) -> String {
    format!("{instance_id}.{server}")
}

/// Host a hook over Core NATS request-reply and publish its KV registration.
pub async fn serve_over_nats<H: Hook + 'static>(
    hook: H,
    instance_id: ServerScope,
    nats_url: &str,
    token: &str,
) -> Result<()> {
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
    serve_with_client(Arc::new(hook), instance_id, connection).await
}

/// Serve a hook using an existing NATS connection.
pub async fn serve_with_client(
    hook: Arc<dyn Hook>,
    instance_id: ServerScope,
    connection: NatsConnection,
) -> Result<()> {
    // Never cancelled: this entry point has no shutdown signal of its own, so
    // it only ever exits through `serve_requests`' bail! conditions.
    serve_with_shutdown(hook, instance_id, connection, CancellationToken::new()).await
}

/// Serve a hook using an existing NATS connection, exiting cleanly (and
/// running exit cleanup while the connection is still usable) when
/// `shutdown` is cancelled, in addition to the usual failure exits.
pub async fn serve_with_shutdown(
    hook: Arc<dyn Hook>,
    instance_id: ServerScope,
    connection: NatsConnection,
    shutdown: CancellationToken,
) -> Result<()> {
    let NatsConnection { client, replicas } = connection;
    let server = hook.name().to_owned();
    let hooks = hook.hooks();
    let mut events = HashSet::new();
    let mut subscribers = Vec::new();

    for event in hooks.iter().map(|spec| &spec.event) {
        if !events.insert(event.clone()) {
            continue;
        }
        let subject = instance_id.hook_subject(&server, event);
        let subscriber = client
            .subscribe(subject.clone())
            .await
            .with_context(|| format!("subscribe to hook requests on {subject}"))?;
        subscribers.push(subscriber);
    }
    let mut requests = select_all(subscribers);

    // Subscriptions must be active before registration makes this server discoverable.
    client.flush().await.context("flush hook subscriptions")?;

    let registration = HookRegistration {
        server: server.clone(),
        display_label: None,
        hooks,
        schema_version: HOOK_SCHEMA_VERSION,
        proto_version: HOOK_PROTOCOL_VERSION,
    };
    let jetstream = jetstream::new(client.clone());
    let registry = ensure_hook_registry_bucket(&jetstream, replicas).await?;
    let mut revision = publish_hook_registration(&registry, &instance_id, &registration).await?;

    let mut refresh = tokio::time::interval(REGISTRATION_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    refresh.tick().await;

    let outcome = serve_requests(
        &client,
        &hook,
        HookSubscriptions {
            requests: &mut requests,
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

    // Best-effort: the TTL is the backstop when this cannot run.
    let key = hook_registration_key(&instance_id, &server);
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
                "hook registration '{key}' was already replaced by a newer instance; \
                 not deleting it"
            );
        }
        Err(error) => {
            log::warn!("could not remove hook registration '{key}' on shutdown: {error}");
        }
    }
}

/// Everything `serve_requests` needs to keep the KV registration alive, bundled
/// so the function stays under the argument-count limit.
struct RegistrationRefresh<'a> {
    registry: &'a kv::Store,
    instance_id: &'a ServerScope,
    registration: &'a HookRegistration,
    interval: &'a mut tokio::time::Interval,
    /// The revision of our own last-published registration, so shutdown can
    /// delete it conditionally instead of unconditionally (see the delete
    /// call in `serve_with_shutdown`). Updated after every successful
    /// refresh publish, not just the initial one.
    revision: &'a mut u64,
}

/// The subscriptions `serve_requests` polls, plus the signal that ends the
/// loop on purpose. See `harnx-toolset-server`'s `ToolSubscriptions` for why
/// `shutdown` is distinct from a subscription simply closing.
struct HookSubscriptions<'a> {
    requests: &'a mut futures_util::stream::SelectAll<async_nats::Subscriber>,
    shutdown: CancellationToken,
}

async fn serve_requests(
    client: &async_nats::Client,
    hook: &Arc<dyn Hook>,
    subscriptions: HookSubscriptions<'_>,
    refresh: RegistrationRefresh<'_>,
) -> Result<()> {
    loop {
        tokio::select! {
            request = subscriptions.requests.next() => {
                let Some(request) = request else {
                    anyhow::bail!("hook request subscriptions closed");
                };
                let client = client.clone();
                let hook = Arc::clone(hook);
                let specs = refresh.registration.hooks.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_hook_request(client, hook, &specs, request).await {
                        log::warn!("handle hook request failed: {error:#}");
                    }
                });
            }
            _ = refresh.interval.tick() => {
                match publish_hook_registration(refresh.registry, refresh.instance_id, refresh.registration).await {
                    Ok(new_revision) => *refresh.revision = new_revision,
                    Err(error) => {
                        log::warn!("refresh hook registration failed; retrying next interval: {error:#}");
                    }
                }
            }
            _ = subscriptions.shutdown.cancelled() => {
                return Ok(());
            }
        }
    }
}

fn continue_outcome() -> HookOutcome {
    HookOutcome {
        control: HookResultControl::Continue,
        result: HookResult::default(),
    }
}

async fn handle_hook_request(
    client: async_nats::Client,
    hook: Arc<dyn Hook>,
    specs: &[HookSpec],
    message: async_nats::Message,
) -> Result<()> {
    let Some(reply_subject) = message.reply else {
        log::warn!("hook request missing reply subject");
        return Ok(());
    };

    let outcome = match serde_json::from_slice::<HookPayload>(&message.payload) {
        Ok(payload) => {
            let event = payload.hook_event.event_name();
            let timeout = specs
                .iter()
                .find(|spec| spec.event == event)
                .and_then(|spec| spec.timeout_secs)
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_HOOK_TIMEOUT);
            match tokio::time::timeout(timeout, hook.handle_hook(payload)).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    log::warn!(
                        "{event} hook timed out after {}s; replying with Continue",
                        timeout.as_secs()
                    );
                    continue_outcome()
                }
            }
        }
        Err(error) => {
            log::warn!("decode hook request failed; replying with Continue: {error}");
            continue_outcome()
        }
    };
    let payload = serde_json::to_vec(&outcome).context("encode hook outcome")?;
    client
        .publish(reply_subject, payload.into())
        .await
        .context("publish hook outcome")?;
    client.flush().await.context("flush hook outcome")
}

/// Create or open the hook registration KV bucket.
pub async fn ensure_hook_registry_bucket(
    jetstream: &jetstream::Context,
    replicas: usize,
) -> Result<kv::Store> {
    harnx_nats_common::registry::ensure_bucket_with_ttl(
        jetstream,
        HOOK_REGISTRY_BUCKET,
        harnx_nats_common::registry::REGISTRATION_TTL,
        replicas,
    )
    .await
}

/// Publish one hook server registration.
pub async fn publish_hook_registration(
    registry: &kv::Store,
    instance_id: &ServerScope,
    registration: &HookRegistration,
) -> Result<u64> {
    let key = hook_registration_key(instance_id, &registration.server);
    let payload = serde_json::to_vec(registration).context("encode hook registration")?;
    registry
        .put(&key, payload.into())
        .await
        .map_err(anyhow::Error::from)
        .with_context(|| format!("publish hook registration '{key}'"))
}

/// Read hook server settings from the environment and serve over NATS.
///
/// Wires SIGTERM/Ctrl+C to a graceful stop so a pod killed by Kubernetes gets
/// a chance to remove its own registration instead of leaving it for the TTL.
pub async fn run_hookset_main<H: Hook + 'static>(hook: H) -> Result<()> {
    harnx_core::server_logging::init_server_logger();
    let scope =
        harnx_core::instance::scope_from_env(harnx_core::instance::StandaloneMode::WorkerLaunched)?;
    log::info!("serving under scope '{}'", scope.as_str());
    let endpoint = harnx_nats_common::connect::NatsEndpoint::from_env()?;
    let client = endpoint.connect().await?;
    let connection = NatsConnection {
        client,
        replicas: endpoint.resolved_replicas(),
    };
    let shutdown = harnx_nats_common::shutdown::cancel_token_on_shutdown_signal();
    serve_with_shutdown(Arc::new(hook), scope, connection, shutdown).await
}
