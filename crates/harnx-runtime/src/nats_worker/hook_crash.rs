//! Fail-closed routing for hook servers.
//!
//! Covers the two moments a route must fail closed instead of vanishing: a
//! running server that just crashed (`replace_crashed_hook_route`, via a
//! crash marker on its live registration) and a server that never got far
//! enough to register in the first place (`publish_startup_rejector`, via a
//! synthetic rejector registration). Both fall back to `publish_crash_rejector`
//! when the preferred publication path is unavailable.

use super::hook_registration::{ensure_bucket, publish_registration, remove_registration};
use anyhow::{Context, Result};
use async_nats::jetstream::kv;
use harnx_core::instance::ServerScope;
use harnx_hookset::{
    FailPolicy, HookRegistration, HookSpec, HOOK_EXPECTATIONS_BUCKET, HOOK_PROTOCOL_VERSION,
    HOOK_REGISTRY_BUCKET, HOOK_SCHEMA_VERSION,
};
use harnx_hookset_server::hook_registration_key;

/// Identity for a fail-closed rejector: which server it stands in for, and
/// the label discovery shows for it. Bundled rather than passed as two more
/// `&str` parameters, matching every other multi-field context in this
/// module (`CrashRouteContext`, `RegistrationExpectation` in
/// `hook_registration.rs`).
///
/// `pub` (not `pub(super)`) because [`publish_crash_rejector`] takes one and
/// is itself exposed crate-externally for integration testing.
#[doc(hidden)]
pub struct RejectorTarget<'a> {
    pub server: &'a str,
    pub display_label: &'a str,
}

pub(super) fn crash_marker(
    mut registration: HookRegistration,
    display_label: String,
) -> HookRegistration {
    registration.display_label = Some(display_label);
    for hook in &mut registration.hooks {
        hook.fail_policy = FailPolicy::Closed;
    }
    registration
}

pub(super) async fn replace_crashed_hook_route(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    server: &str,
    marker: HookRegistration,
) {
    let key = hook_registration_key(instance_id, server);
    let rejector_name = format!("{server}-rejector");
    let rejector_label: String = format!(
        "hook server crashed: {}",
        marker.display_label.as_deref().unwrap_or(server)
    )
    .chars()
    .take(120)
    .collect();

    // Publish before deleting the live registration so discovery never sees
    // an empty route set. If both publications fail, retain the live route.
    let route = publish_marker_or_rejector(
        || async {
            let expectations = ensure_bucket(client, HOOK_EXPECTATIONS_BUCKET, 1).await?;
            publish_registration(&expectations, &key, &marker).await
        },
        || {
            publish_crash_rejector(
                client,
                instance_id,
                RejectorTarget {
                    server: &rejector_name,
                    display_label: &rejector_label,
                },
            )
        },
    )
    .await;
    finish_crash_route(
        CrashRouteContext {
            client,
            instance_id,
            server,
            rejector_name: &rejector_name,
        },
        route,
    )
    .await;
}

struct CrashRouteContext<'a> {
    client: &'a async_nats::Client,
    instance_id: &'a ServerScope,
    server: &'a str,
    rejector_name: &'a str,
}

async fn finish_crash_route(context: CrashRouteContext<'_>, route: Result<CrashRoute>) {
    let CrashRouteContext {
        client,
        instance_id,
        server,
        rejector_name,
    } = context;
    match route {
        Ok(CrashRoute::Marker) => remove_registration(client, instance_id, server).await,
        Ok(CrashRoute::Rejector) => {
            log::warn!("hook server '{server}' crash marker failed; installed fail-closed rejector '{rejector_name}'");
            remove_registration(client, instance_id, server).await;
        }
        Err(error) => {
            log::error!("failed to install any fail-closed route for crashed hook server '{server}': {error:#}");
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum CrashRoute {
    Marker,
    Rejector,
}

async fn publish_marker_or_rejector<M, MFut, R, RFut>(
    publish_marker: M,
    publish_rejector: R,
) -> Result<CrashRoute>
where
    M: FnOnce() -> MFut,
    MFut: std::future::Future<Output = Result<()>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = Result<()>>,
{
    match publish_marker().await {
        Ok(()) => Ok(CrashRoute::Marker),
        Err(marker_error) => {
            publish_rejector().await.with_context(|| {
                format!("crash marker failed ({marker_error:#}); publish crash rejector")
            })?;
            Ok(CrashRoute::Rejector)
        }
    }
}

/// Publishes a synthetic fail-closed route after a hook server crash.
///
/// Exposed for integration testing of the live JetStream fallback paths.
#[doc(hidden)]
pub async fn publish_crash_rejector(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    target: RejectorTarget<'_>,
) -> Result<()> {
    let server = target.server;
    let registration = fail_closed_rejector(target);
    let key = hook_registration_key(instance_id, server);

    let expectation_result = async {
        let expectations = ensure_bucket(client, HOOK_EXPECTATIONS_BUCKET, 1).await?;
        publish_registration(&expectations, &key, &registration).await
    }
    .await;
    if expectation_result.is_ok() {
        return Ok(());
    }

    // If the expectations path itself is unavailable, the registry bucket is a
    // second fail-closed publication path. The rejector still has no responder.
    let registry = ensure_bucket(client, HOOK_REGISTRY_BUCKET, 1).await?;
    publish_registration(&registry, &key, &registration)
        .await
        .with_context(|| format!("publish crash rejector after {expectation_result:#?}"))
}

fn fail_closed_rejector(target: RejectorTarget<'_>) -> HookRegistration {
    HookRegistration {
        server: target.server.to_string(),
        display_label: Some(target.display_label.to_string()),
        hooks: vec![
            HookSpec {
                event: "UserPromptSubmit".to_string(),
                matcher: None,
                priority: 0,
                timeout_secs: None,
                fail_policy: FailPolicy::Closed,
            },
            HookSpec {
                event: "PreToolUse".to_string(),
                matcher: Some(".*".to_string()),
                priority: 0,
                timeout_secs: None,
                fail_policy: FailPolicy::Closed,
            },
        ],
        schema_version: HOOK_SCHEMA_VERSION,
        proto_version: HOOK_PROTOCOL_VERSION,
    }
}

pub(super) async fn publish_startup_rejector(
    store: &kv::Store,
    instance_id: &ServerScope,
    target: RejectorTarget<'_>,
) -> Result<()> {
    let key = hook_registration_key(instance_id, target.server);
    let registration = fail_closed_rejector(target);
    publish_registration(store, &key, &registration).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn crash_marker_failure_invokes_fail_closed_rejector_fallback() {
        let rejector_called = AtomicBool::new(false);
        let route = publish_marker_or_rejector(
            || async { anyhow::bail!("marker unavailable") },
            || async {
                rejector_called.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .expect("rejector fallback succeeds");

        assert_eq!(route, CrashRoute::Rejector);
        assert!(rejector_called.load(Ordering::SeqCst));
    }
}
