//! Server-side adapter for hosting a [`harnx_hookset::Hook`] over Core NATS.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, kv, stream};
use futures_util::{stream::select_all, StreamExt};
use harnx_core::hooks::{HookOutcome, HookPayload, HookResult, HookResultControl};
use harnx_core::instance::{InstanceId, HARNX_INSTANCE_ID};
use harnx_hookset::{Hook, HookRegistration, HookSpec, HOOK_PROTOCOL_VERSION, HOOK_SCHEMA_VERSION};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

pub use harnx_hookset::HOOK_REGISTRY_BUCKET;

const HARNX_NATS_URL: &str = "HARNX_NATS_URL";
const HARNX_NATS_TOKEN: &str = "HARNX_NATS_TOKEN";
const REGISTRATION_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// KV key for one worker instance's hook server registration.
pub fn hook_registration_key(instance_id: &InstanceId, server: &str) -> String {
    format!("{instance_id}.{server}")
}

/// Host a hook over Core NATS request-reply and publish its KV registration.
pub async fn serve_over_nats<H: Hook + 'static>(
    hook: H,
    instance_id: InstanceId,
    nats_url: &str,
    token: &str,
) -> Result<()> {
    let client = async_nats::ConnectOptions::new()
        .token(token.to_owned())
        .connect(nats_url)
        .await
        .with_context(|| format!("connect to NATS at {nats_url}"))?;
    serve_with_client(Arc::new(hook), instance_id, client).await
}

/// Serve a hook using an existing NATS client.
pub async fn serve_with_client(
    hook: Arc<dyn Hook>,
    instance_id: InstanceId,
    client: async_nats::Client,
) -> Result<()> {
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
    let registry = ensure_hook_registry_bucket(&jetstream).await?;
    publish_hook_registration(&registry, &instance_id, &registration).await?;

    let mut refresh = tokio::time::interval(REGISTRATION_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    refresh.tick().await;

    loop {
        tokio::select! {
            request = requests.next() => {
                let Some(request) = request else {
                    anyhow::bail!("hook request subscriptions closed");
                };
                let client = client.clone();
                let hook = Arc::clone(&hook);
                let specs = registration.hooks.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_hook_request(client, hook, &specs, request).await {
                        log::warn!("handle hook request failed: {error:#}");
                    }
                });
            }
            _ = refresh.tick() => {
                if let Err(error) = publish_hook_registration(&registry, &instance_id, &registration).await {
                    log::warn!("refresh hook registration failed; retrying next interval: {error:#}");
                }
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
pub async fn ensure_hook_registry_bucket(jetstream: &jetstream::Context) -> Result<kv::Store> {
    match jetstream
        .create_key_value(kv::Config {
            bucket: HOOK_REGISTRY_BUCKET.to_string(),
            history: 1,
            num_replicas: 1,
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await
    {
        Ok(store) => Ok(store),
        Err(create_error) => jetstream
            .get_key_value(HOOK_REGISTRY_BUCKET)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| {
                format!(
                    "open hook registry bucket '{HOOK_REGISTRY_BUCKET}' after create failed: {create_error}"
                )
            }),
    }
}

/// Publish one hook server registration.
pub async fn publish_hook_registration(
    registry: &kv::Store,
    instance_id: &InstanceId,
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
pub async fn run_hookset_main<H: Hook + 'static>(hook: H) -> Result<()> {
    harnx_core::server_logging::init_server_logger();
    let instance_id = std::env::var(HARNX_INSTANCE_ID)
        .with_context(|| format!("{HARNX_INSTANCE_ID} is required"))?;
    let nats_url =
        std::env::var(HARNX_NATS_URL).with_context(|| format!("{HARNX_NATS_URL} is required"))?;
    let token = std::env::var(HARNX_NATS_TOKEN)
        .with_context(|| format!("{HARNX_NATS_TOKEN} is required"))?;
    serve_over_nats(
        hook,
        InstanceId::from_string(instance_id),
        &nats_url,
        &token,
    )
    .await
}
