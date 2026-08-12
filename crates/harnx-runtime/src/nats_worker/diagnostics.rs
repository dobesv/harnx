//! Worker startup diagnostics.
//!
//! Reproduces exactly what the worker does to its tool servers — the same
//! selection, the same spawn, the same registry wait — and reports the outcome
//! per server instead of running a session.
//!
//! Without this the only view of tool startup is a log written during a
//! front-end run that exits after its turn, which routinely ends before slow
//! servers finish and leaves the interesting part unobserved.

use super::daemon_background::configured_worker_services;
use super::tool_registry::ensure_registry_bucket;
use super::tool_supervisor::{ToolServerStartConfig, ToolServerSupervisor};
use crate::config::{
    resolve_local_nats_server_config, GlobalConfig, ToolServerConfig, HARNX_NATS_TOKEN_ENV,
    HARNX_NATS_URL_ENV,
};
use anyhow::{Context, Result};
use futures_util::TryStreamExt;
use harnx_core::instance::ServerScope;
use harnx_toolset::{server_identity_token, Registration};
use harnx_toolset_server::registration_key;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// How long a diagnostic run waits for registrations.
///
/// Deliberately longer than the worker's own startup budget: a server that is
/// merely slow should show up as slow-but-working rather than as a failure,
/// and an MCP bridge waiting on a cold package download can take tens of
/// seconds by itself.
const DIAGNOSE_TIMEOUT: Duration = Duration::from_secs(60);

/// One configured tool server and what became of it.
struct ServerOutcome {
    label: String,
    command: String,
    tools: Option<usize>,
}

/// Start this worker's tool servers and report which ones registered.
pub async fn diagnose_tool_servers(config: &GlobalConfig) -> Result<String> {
    let (servers, _hooks) = configured_worker_services(config);
    if servers.is_empty() {
        return Ok("No tool servers are enabled for this configuration.\n".to_string());
    }

    // Hold the broker handle for the whole run. `resolve_local_nats_server_config`
    // starts a shared server on its fallback path and drops the owner before
    // returning, which kills it again; the worker never notices because its
    // supervisor always hands it a broker through the environment.
    let owned_broker = match (
        std::env::var(HARNX_NATS_URL_ENV).ok(),
        std::env::var(HARNX_NATS_TOKEN_ENV).ok(),
    ) {
        (Some(_), Some(_)) => None,
        _ => Some(
            crate::nats_local_server::ensure_shared_server()
                .await
                .context("start the shared local NATS broker")?,
        ),
    };
    // The auto-managed embedded broker is always single-node and TLS-less, so
    // only a full environment handoff to a real cluster ever has a replica
    // count or TLS settings to carry into the spawned tool servers below.
    let (url, token, replicas, tls_endpoint) = match &owned_broker {
        Some(server) => (
            server.url.clone(),
            server.token.clone(),
            None,
            harnx_nats_common::connect::NatsEndpoint::default(),
        ),
        None => {
            let local = resolve_local_nats_server_config()
                .await
                .context("resolve the local NATS broker")?;
            let tls_endpoint = harnx_nats_common::connect::NatsEndpoint::from(&local);
            let token = local.token.clone().context("local NATS requires a token")?;
            (local.url, token, local.replicas, tls_endpoint)
        }
    };
    let client = async_nats::ConnectOptions::new()
        .token(token.clone())
        .connect(&url)
        .await
        .with_context(|| format!("connect to the local NATS broker at {url}"))?;
    let instance_id = ServerScope::new();

    let mut report = format!(
        "Starting {} tool server(s) as instance {instance_id}\n\
         Waiting up to {}s for each to register.\n\n",
        servers.len(),
        DIAGNOSE_TIMEOUT.as_secs()
    );

    let start = ToolServerStartConfig::new(client.clone(), instance_id.clone(), &url, &token)
        .inheriting_child_output()
        .with_replicas(replicas)
        .with_tls(&tls_endpoint);
    let began = Instant::now();
    let supervisor =
        ToolServerSupervisor::start_local_with_timeout(start, &servers, DIAGNOSE_TIMEOUT).await?;
    let elapsed = began.elapsed();

    let registered = registered_tool_counts(&client, &instance_id, replicas.unwrap_or(1)).await;
    let outcomes = collect_outcomes(&servers, &registered);
    render_outcomes(&mut report, &outcomes, elapsed);
    drop(supervisor);
    drop(owned_broker);
    Ok(report)
}

/// Tool count per identity token currently registered for this instance.
async fn registered_tool_counts(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    replicas: usize,
) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    let Ok(registry) = ensure_registry_bucket(client, replicas).await else {
        return counts;
    };
    let Ok(keys) = registry.keys().await else {
        return counts;
    };
    let Ok(keys) = keys.try_collect::<Vec<_>>().await else {
        return counts;
    };
    let prefix = format!("{instance_id}.");
    for key in keys.into_iter().filter(|key| key.starts_with(&prefix)) {
        let Ok(Some(value)) = registry.get(&key).await else {
            continue;
        };
        let Ok(registration) = serde_json::from_slice::<Registration>(&value) else {
            continue;
        };
        let identity = server_identity_token(
            registration.package.as_deref(),
            &registration.config,
            &registration.server,
        );
        counts.insert(
            registration_key(instance_id, &identity),
            registration.tools.len(),
        );
    }
    counts
}

fn collect_outcomes(
    servers: &[ToolServerConfig],
    registered: &HashMap<String, usize>,
) -> Vec<ServerOutcome> {
    servers
        .iter()
        .map(|server| {
            // A server registers under its own advertised name, which need not
            // match the config name, so match on the package/config prefix.
            let prefix = server_identity_token(server.package.as_deref(), &server.name, "");
            let tools = registered
                .iter()
                .find(|(key, _)| key.contains(&prefix))
                .map(|(_, count)| *count);
            ServerOutcome {
                label: match &server.package {
                    Some(package) => format!("{package}/{}", server.name),
                    None => server.name.clone(),
                },
                command: shell_words::join(
                    std::iter::once(server.command.as_str())
                        .chain(server.args.iter().map(String::as_str)),
                ),
                tools,
            }
        })
        .collect()
}

fn render_outcomes(report: &mut String, outcomes: &[ServerOutcome], elapsed: Duration) {
    let width = outcomes
        .iter()
        .map(|outcome| outcome.label.len())
        .max()
        .unwrap_or(0);
    for outcome in outcomes {
        let status = match outcome.tools {
            Some(count) => format!("registered, {count} tool(s)"),
            None => "DID NOT REGISTER".to_string(),
        };
        let _ = writeln!(report, "  {:width$}  {status}", outcome.label);
        if outcome.tools.is_none() {
            let _ = writeln!(report, "  {:width$}    command: {}", "", outcome.command);
        }
    }

    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.tools.is_none())
        .count();
    let _ = writeln!(
        report,
        "\n{} of {} registered in {:.1}s.",
        outcomes.len() - failed,
        outcomes.len(),
        elapsed.as_secs_f64()
    );
    if failed > 0 {
        let _ = writeln!(
            report,
            "\nFor a server that did not register, run its command directly:\n  \
             harnx-mcp-bridge --list-tools -- <command>\n\
             which reports whether the child spawns, completes the MCP handshake, \
             and answers tools/list."
        );
    }
}
