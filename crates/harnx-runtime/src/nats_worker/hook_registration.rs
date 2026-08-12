//! KV bookkeeping for hook server registrations.
//!
//! Prepares a registration watch before a hook server is spawned, waits for
//! the server to publish its registration once running, and removes entries
//! from the registry/expectations buckets on shutdown or supersession.

use super::hook_crash::{publish_startup_rejector, RejectorTarget};
use super::hook_supervisor::{HookLaunch, HookServerStartConfig, HookServerSupervisor};
use anyhow::{bail, Context, Result};
use async_nats::jetstream::{self, kv, stream};
use futures_util::StreamExt;
use harnx_core::hooks::HookConfig;
use harnx_core::instance::ServerScope;
use harnx_hookset::{HookRegistration, HOOK_EXPECTATIONS_BUCKET, HOOK_REGISTRY_BUCKET};
use harnx_hookset_server::hook_registration_key;

pub(super) struct PreparedHook {
    pub(super) order_index: usize,
    pub(super) name: String,
    pub(super) key: String,
    pub(super) rejector_name: String,
    pub(super) display_label: String,
    pub(super) failure_label: String,
    pub(super) hook: HookConfig,
    pub(super) watch: kv::Watch,
}

pub(super) async fn prepare_hook_registrations(
    config: &HookServerStartConfig,
    launch_plan: &[HookLaunch<'_>],
    supervisor: &mut HookServerSupervisor,
) -> Result<Vec<PreparedHook>> {
    let registry = ensure_bucket_for(config, HOOK_REGISTRY_BUCKET).await?;
    let expectations = ensure_bucket_for(config, HOOK_EXPECTATIONS_BUCKET).await?;
    let mut prepared = Vec::new();
    for launch in launch_plan {
        let key = hook_registration_key(&config.instance_id, &launch.name);
        let rejector_name = format!("{}-rejector", launch.name);
        supervisor.registrations.push(launch.name.clone());
        supervisor.registrations.push(rejector_name.clone());
        let display_label = hook_display_label(launch.hook);
        let failure_label = startup_failure_label(launch.hook);

        if let Err(error) = registry.delete(&key).await {
            publish_startup_rejector(
                &expectations,
                &config.instance_id,
                RejectorTarget {
                    server: &rejector_name,
                    display_label: &failure_label,
                },
            )
            .await?;
            log::warn!(
                "hook server '{}' startup rejected because stale registration '{}' could not be deleted: {error:#}",
                launch.name,
                key
            );
            continue;
        }

        let watch = match registry.watch_with_history(&key).await {
            Ok(watch) => watch,
            Err(error) => {
                publish_startup_rejector(
                    &expectations,
                    &config.instance_id,
                    RejectorTarget {
                        server: &rejector_name,
                        display_label: &failure_label,
                    },
                )
                .await?;
                log::warn!(
                    "hook server '{}' registration watch failed: {error:#}",
                    launch.name
                );
                continue;
            }
        };
        prepared.push(PreparedHook {
            order_index: launch.order_index,
            name: launch.name.clone(),
            key,
            rejector_name,
            display_label,
            failure_label,
            hook: launch.hook.clone(),
            watch,
        });
    }
    Ok(prepared)
}

struct RegistrationExpectation<'a> {
    server: &'a str,
    key: &'a str,
}

pub(super) async fn wait_for_registration(
    watch: &mut kv::Watch,
    server: &str,
    key: &str,
) -> Result<HookRegistration> {
    loop {
        let Some(entry) = watch.next().await else {
            bail!("hook registration watch closed for '{key}'");
        };
        let entry = entry.with_context(|| format!("watch hook registration '{key}'"))?;
        if entry.operation != kv::Operation::Put {
            continue;
        }
        return decode_expected_registration(&entry.value, RegistrationExpectation { server, key });
    }
}

fn decode_expected_registration(
    value: &[u8],
    expected: RegistrationExpectation<'_>,
) -> Result<HookRegistration> {
    let registration: HookRegistration = serde_json::from_slice(value)
        .with_context(|| format!("decode hook registration '{}'", expected.key))?;
    if registration.server != expected.server {
        bail!(
            "hook registration '{}' declared server '{}' instead of assigned name '{}'",
            expected.key,
            registration.server,
            expected.server
        );
    }
    Ok(registration)
}

/// `ensure_bucket` for the common case of a bucket that belongs to a
/// configured cluster, so call sites don't have to spell out
/// `&config.client` and `config.resolved_replicas()` every time.
pub(super) async fn ensure_bucket_for(
    config: &HookServerStartConfig,
    bucket: &str,
) -> Result<kv::Store> {
    ensure_bucket(&config.client, bucket, config.resolved_replicas()).await
}

pub(super) async fn ensure_bucket(
    client: &async_nats::Client,
    bucket: &str,
    replicas: usize,
) -> Result<kv::Store> {
    let jetstream = jetstream::new(client.clone());
    let create = jetstream
        .create_key_value(kv::Config {
            bucket: bucket.to_string(),
            history: 1,
            num_replicas: replicas,
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await;
    if let Ok(store) = create {
        return Ok(store);
    }

    // The bucket already exists (this covers both HOOK_REGISTRY_BUCKET and
    // HOOK_EXPECTATIONS_BUCKET); raise its replicas to match config if an
    // operator changed it after the bucket was first created. Never lowers
    // (see reconcile_bucket_replicas), so a caller that only knows `1` can't
    // downgrade a bucket another caller already raised.
    if let Err(error) =
        harnx_nats_common::registry::reconcile_bucket_replicas(&jetstream, bucket, replicas).await
    {
        log::warn!("could not reconcile replicas for bucket '{bucket}': {error:#}");
    }

    jetstream
        .get_key_value(bucket)
        .await
        .map_err(anyhow::Error::from)
        .with_context(|| format!("open hook KV bucket '{bucket}'"))
}

pub(super) fn hook_display_label(hook: &HookConfig) -> String {
    hook.status_message
        .as_deref()
        .unwrap_or(&hook.command)
        .chars()
        .take(120)
        .collect()
}

pub(super) fn startup_failure_label(hook: &HookConfig) -> String {
    format!("hook server failed to start: {}", hook_display_label(hook))
        .chars()
        .take(120)
        .collect()
}

pub(super) async fn publish_registration(
    store: &kv::Store,
    key: &str,
    registration: &HookRegistration,
) -> Result<()> {
    let payload = serde_json::to_vec(registration).context("encode hook expectation")?;
    store
        .put(key.to_string(), payload.into())
        .await
        .with_context(|| format!("publish fail-closed hook expectation '{key}'"))?;
    Ok(())
}

pub(super) async fn remove_registration(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    server: &str,
) {
    let jetstream = jetstream::new(client.clone());
    let Ok(store) = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await else {
        return;
    };
    let _ = store
        .delete(hook_registration_key(instance_id, server))
        .await;
}

pub(super) async fn remove_registration_and_expectation(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    server: &str,
) {
    let jetstream = jetstream::new(client.clone());
    let key = hook_registration_key(instance_id, server);
    for bucket in [HOOK_REGISTRY_BUCKET, HOOK_EXPECTATIONS_BUCKET] {
        let Ok(store) = jetstream.get_key_value(bucket).await else {
            continue;
        };
        let _ = store.delete(key.clone()).await;
    }
}
