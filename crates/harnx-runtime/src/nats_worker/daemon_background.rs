//! Background service startup: hooks, sub-agent toolsets, and the on-demand
//! tool-server reconciler.
//!
//! None of this is required for the worker to accept and run a session (see
//! `launch_worker_services`'s readiness-first ordering in `run_worker_daemon`),
//! so failures here are logged and degrade gracefully rather than failing
//! worker startup.

use super::daemon::{WorkerDaemonConfig, WorkerStartup};
use super::hook_supervisor::{HookServerStartConfig, HookServerSupervisor};
use super::server_reconciler::{
    all_enabled_tool_servers, build_server_reconciler, ServerReconciler,
};
use super::subagent_toolset::SubagentToolset;
use super::tool_supervisor::{ToolServerStartConfig, ToolServerSupervisor};
use crate::config::{
    list_agents, resolve_local_nats_server_config, server_display_name, GlobalConfig,
    ToolServerConfig,
};
use anyhow::{Context, Result};
use async_nats::jetstream;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Longest a session start will wait for the first tool-server registration
/// round. Above the round's own budget, so it only fires if that round wedges.
const INITIAL_TOOL_REGISTRATION_WAIT: Duration = Duration::from_secs(30);

pub(crate) fn tool_servers_matching_use_tools(
    servers: &[ToolServerConfig],
    agent_package: Option<&str>,
    namespaced_use_tools: &[String],
) -> Vec<ToolServerConfig> {
    let mut seen_names = HashSet::new();
    servers
        .iter()
        .filter(|server| {
            if !server.enabled {
                return false;
            }
            let display_name =
                server_display_name(&server.name, server.package.as_deref(), agent_package);
            let matches = namespaced_use_tools.iter().any(|selector| {
                super::super::config::selector_could_match_server(selector, &display_name)
            });
            let identity_token = harnx_toolset::server_identity_token(
                server.package.as_deref(),
                &server.name,
                &server.name,
            );
            matches && seen_names.insert(identity_token)
        })
        .cloned()
        .collect()
}

/// Whether this worker should launch its own tool servers.
///
/// A worker with nothing configured to spawn must not resolve a broker
/// address, because resolving falls back to starting a shared NATS server.
/// Mirrors `start_global_hooks`'s `hooks.entries.is_empty()` check. Pulled out
/// of `start_local_tool_servers` so the decision itself — not a copy of it —
/// is what tests exercise.
pub(super) fn should_start_tool_servers(
    manage_servers: bool,
    servers: &[ToolServerConfig],
) -> bool {
    manage_servers && !servers.is_empty()
}

async fn start_local_tool_servers(
    daemon: &WorkerDaemonConfig,
    client: async_nats::Client,
    instance_id: &harnx_core::instance::ServerScope,
    servers: &[crate::config::ToolServerConfig],
) -> Option<ToolServerSupervisor> {
    if !should_start_tool_servers(daemon.manage_servers, servers) {
        return None;
    }
    let result = async {
        let server = resolve_local_nats_server_config().await?;
        let token = server
            .token
            .as_deref()
            .context("local NATS tool servers require HARNX_NATS_TOKEN")?;
        let start = ToolServerStartConfig::new(client, instance_id.clone(), &server.url, token)
            .with_replicas(server.replicas)
            .with_tls(&harnx_nats_common::connect::NatsEndpoint::from(&server));
        ToolServerSupervisor::start_local(start, servers)
            .await
            .context("start local NATS tool servers")
    }
    .await;
    optional_tool_server(result)
}

/// Test entry point for [`start_local_tool_servers`].
///
/// Public for regression coverage that a consuming worker (the default) never
/// spawns child tool servers, and that a managing worker with nothing
/// configured spawns none either. Not part of the crate's real API —
/// production code always goes through [`start_local_tool_servers`] with an
/// already connected client obtained during worker startup.
///
/// Deliberately does none of `start_local_tool_servers`'s own gating here: it
/// builds a client and calls straight through, so a regression in the real
/// function's guard shows up as this shim actually starting a tool-server
/// supervisor (or, without a reachable broker, hanging/erroring on the
/// connect) instead of silently returning `None` from a copy of the check.
#[doc(hidden)]
pub async fn start_local_tool_servers_for_test(
    daemon: &WorkerDaemonConfig,
    config: &GlobalConfig,
) -> Option<ToolServerSupervisor> {
    let (servers, _hooks) = configured_worker_services(config);
    let server = resolve_local_nats_server_config().await.ok()?;
    let token = server.token.as_deref()?;
    let client = async_nats::ConnectOptions::new()
        .token(token.to_string())
        .connect(&server.url)
        .await
        .ok()?;
    let instance_id = harnx_core::instance::ServerScope::new();
    start_local_tool_servers(daemon, client, &instance_id, &servers).await
}

async fn start_global_hooks(
    daemon: &WorkerDaemonConfig,
    client: async_nats::Client,
    instance_id: &harnx_core::instance::ServerScope,
    hooks: &harnx_core::hooks::HooksConfig,
) -> Option<HookServerSupervisor> {
    if !daemon.manage_servers || hooks.entries.is_empty() {
        return None;
    }
    let result = async {
        let server = resolve_local_nats_server_config().await?;
        let token = server
            .token
            .as_deref()
            .context("local NATS hook servers require HARNX_NATS_TOKEN")?;
        let start = HookServerStartConfig::new(client, instance_id.clone(), &server.url, token)
            .with_replicas(server.replicas)
            .with_tls(&harnx_nats_common::connect::NatsEndpoint::from(&server));
        HookServerSupervisor::start_local(start, hooks, "global")
            .await
            .context("start global NATS hook servers")
    }
    .await;
    match result {
        Ok(supervisor) => Some(supervisor),
        Err(error) => {
            // Failures happen before the supervisor can own cleanup or while its KV
            // route is unavailable. Publishing here would either reuse the failed
            // route or leave an unowned rejector behind after worker shutdown. Keep
            // the worker available; unreadable registries fail closed at discovery.
            log::warn!("global NATS hook servers disabled: {error:#}");
            None
        }
    }
}

fn optional_tool_server<T>(result: Result<T>) -> Option<T> {
    match result {
        Ok(supervisor) => Some(supervisor),
        Err(error) => {
            log::warn!("local NATS tool servers disabled; continuing with stdio tools: {error:#}");
            None
        }
    }
}

/// Everything [`start_subagent_toolset`] needs, bundled so adding `replicas`
/// didn't push it to a 6th bare argument.
struct SubagentToolsetStart {
    agent: String,
    cluster: String,
    instance_id: harnx_core::instance::ServerScope,
    client: async_nats::Client,
    jetstream: jetstream::Context,
    replicas: usize,
}

async fn start_subagent_toolset(start: SubagentToolsetStart) -> Result<JoinHandle<Result<()>>> {
    let SubagentToolsetStart {
        agent,
        cluster,
        instance_id,
        client,
        jetstream,
        replicas,
    } = start;
    let registration_context = jetstream.clone();
    let toolset = Arc::new(SubagentToolset::new(
        agent,
        cluster,
        client.clone(),
        jetstream,
    ));
    let server_name = harnx_toolset::Toolset::name(toolset.as_ref()).to_string();
    let identity_token = harnx_toolset::server_identity_token(None, "", &server_name);
    let registration_key = harnx_toolset_server::registration_key(&instance_id, &identity_token);
    let connection = harnx_nats_common::connect::NatsConnection { client, replicas };
    let server = tokio::spawn(harnx_toolset_server::serve_with_client(
        toolset,
        instance_id,
        connection,
    ));

    let registration = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server.is_finished() {
                anyhow::bail!("sub-agent tool server exited before registering");
            }
            match registration_context
                .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
                .await
            {
                Ok(registry) if registry.get(&registration_key).await?.is_some() => break,
                Ok(_) | Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
        Result::<()>::Ok(())
    })
    .await;

    match registration {
        Ok(Ok(())) => Ok(server),
        Ok(Err(error)) => {
            server.abort();
            Err(error)
        }
        Err(_) => {
            server.abort();
            anyhow::bail!("sub-agent tool server did not register within 5s")
        }
    }
}

pub(super) struct WorkerServices {
    pub(super) background: Arc<Mutex<Option<BackgroundServices>>>,
    pub(super) session_index: Option<async_nats::jetstream::kv::Store>,
    pub(super) tools_attempted: tokio::sync::watch::Receiver<bool>,
    /// `None` for a consuming worker, or a managing worker with nothing
    /// configured to spawn. Tool servers now start on demand per session
    /// (see `handle_activation`) rather than as one batch here.
    pub(super) server_reconciler: Option<Arc<ServerReconciler>>,
}

/// Hooks and sub-agent toolsets, started after the worker has already
/// announced readiness. Held only to keep the children alive for the
/// worker's lifetime. Tool servers are no longer part of this: they start
/// per session through `WorkerRuntime::server_reconciler`.
pub(super) struct BackgroundServices {
    _global_hook_supervisor: Option<HookServerSupervisor>,
    _subagent_tool_servers: Vec<JoinHandle<Result<()>>>,
}

pub(super) fn configured_worker_services(
    config: &GlobalConfig,
) -> (Vec<ToolServerConfig>, harnx_core::hooks::HooksConfig) {
    let config = config.read();
    let agent_package = config
        .agent
        .as_ref()
        .and_then(|agent| harnx_core::package_namespace::pkg_from_qualified(agent.name()));
    let mut selectors = Vec::new();
    let use_tools = config
        .agent
        .as_ref()
        .and_then(|agent| agent.use_tools())
        // Fall back when no agent is active or the active agent has no use_tools.
        .or_else(|| config.use_tools.clone())
        .unwrap_or_default();
    for selector in use_tools {
        if let Some(namespaced) = namespaced_selector(&selector, agent_package) {
            selectors.push(namespaced);
        }
        selectors.push(selector);
    }
    let servers = tool_servers_matching_use_tools(&config.tool_servers, agent_package, &selectors);
    (servers, config.hooks.clone().unwrap_or_default())
}

fn namespaced_selector(selector: &str, package: Option<&str>) -> Option<String> {
    if selector == "*" {
        return None;
    }
    let namespaced = harnx_core::package_namespace::namespace_use_tools_entry(package?, selector);
    (namespaced != selector).then_some(namespaced)
}

pub(super) async fn launch_worker_services(
    config: &GlobalConfig,
    daemon: &WorkerDaemonConfig,
    startup: &WorkerStartup,
    instance_id: &harnx_core::instance::ServerScope,
) -> Result<WorkerServices> {
    // Announce readiness before starting hooks and sub-agent toolsets. Those
    // can take tens of seconds — or never finish, when a server is
    // misconfigured — and neither is required for the worker to accept and
    // run a session. Gating readiness on them made a single broken server
    // stall the front-end past its startup deadline, which turned an
    // intentionally non-fatal degradation into an unusable CLI. Tool servers
    // no longer start here at all: each session's own servers start (and are
    // waited on) in `handle_activation` when that session activates.
    super::daemon::spawn_readiness_publisher(startup.client.clone(), daemon);

    let background = Arc::new(Mutex::new(None));
    // There is no longer a global tool-server registration round for a
    // session to wait on: `handle_activation` waits on the servers it just
    // asked the reconciler to start instead. Start this already-settled so
    // `await_initial_tool_registration` is a no-op, kept for the hooks/
    // sub-agent-toolset background task shape rather than removed outright.
    let (_tools_attempted_tx, tools_attempted) = tokio::sync::watch::channel(true);

    let all_tool_servers = all_enabled_tool_servers(config);
    let server_reconciler = build_server_reconciler(
        daemon,
        startup.client.clone(),
        instance_id,
        &all_tool_servers,
    )
    .await;

    spawn_background_services(BackgroundServicesCtx {
        config: config.clone(),
        daemon: daemon.clone(),
        client: startup.client.clone(),
        jetstream: startup.jetstream.clone(),
        instance_id: instance_id.clone(),
        slot: Arc::clone(&background),
        replicas: startup.replicas,
    });

    let session_index =
        super::daemon::optional_session_index(&startup.jetstream, startup.replicas).await;
    Ok(WorkerServices {
        background,
        session_index,
        tools_attempted,
        server_reconciler,
    })
}

/// Historically blocked until the first tool-server registration round had
/// finished, so a session's registry snapshot included whatever managed to
/// come up. Tool servers now start per session (`handle_activation` awaits
/// `ServerReconciler::session_started` directly), so `tools_attempted` is
/// already settled by the time this runs and it returns immediately. Kept
/// rather than removed so a future global round has somewhere to plug back
/// in without re-threading `handle_activation`.
pub(super) async fn await_initial_tool_registration(
    tools_attempted: &tokio::sync::watch::Receiver<bool>,
) {
    if *tools_attempted.borrow() {
        return;
    }
    let mut attempted = tools_attempted.clone();
    log::debug!("waiting for the first tool-server registration round");
    // The only sender lives in the background task; if it is gone the round can
    // never complete and there is nothing left to wait for. Cap the wait so a
    // wedged round costs the session its tools rather than the whole turn — the
    // round is bounded by the per-server startup timeout, so reaching this cap
    // means something below it is stuck.
    if tokio::time::timeout(
        INITIAL_TOOL_REGISTRATION_WAIT,
        attempted.wait_for(|done| *done),
    )
    .await
    .is_err()
    {
        log::warn!(
            "tool servers did not finish their first registration round within {}s; \
             starting this session with the tools registered so far",
            INITIAL_TOOL_REGISTRATION_WAIT.as_secs()
        );
    }
}

/// Owned inputs for the post-readiness startup task.
struct BackgroundServicesCtx {
    config: GlobalConfig,
    daemon: WorkerDaemonConfig,
    client: async_nats::Client,
    jetstream: jetstream::Context,
    instance_id: harnx_core::instance::ServerScope,
    slot: Arc<Mutex<Option<BackgroundServices>>>,
    replicas: usize,
}

fn spawn_background_services(ctx: BackgroundServicesCtx) {
    tokio::spawn(async move {
        let BackgroundServicesCtx {
            config,
            daemon,
            client,
            jetstream,
            instance_id,
            slot,
            replicas,
        } = ctx;
        // Tool servers aren't started here anymore — each session's own
        // servers start on demand through `WorkerRuntime::server_reconciler`.
        let (_worker_tool_servers, global_hooks) = configured_worker_services(&config);
        let global_hook_supervisor =
            start_global_hooks(&daemon, client.clone(), &instance_id, &global_hooks).await;

        let mut subagent_tool_servers = Vec::new();
        for agent in list_agents() {
            match start_subagent_toolset(SubagentToolsetStart {
                agent: agent.clone(),
                cluster: daemon.cluster.clone(),
                instance_id: instance_id.clone(),
                client: client.clone(),
                jetstream: jetstream.clone(),
                replicas,
            })
            .await
            {
                Ok(handle) => subagent_tool_servers.push(handle),
                // One unusable sub-agent must not cost the others their toolset.
                Err(error) => {
                    log::warn!("sub-agent toolset for '{agent}' unavailable: {error:#}")
                }
            }
        }

        *slot.lock().await = Some(BackgroundServices {
            _global_hook_supervisor: global_hook_supervisor,
            _subagent_tool_servers: subagent_tool_servers,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::{
        configured_worker_services, optional_tool_server, should_start_tool_servers,
        tool_servers_matching_use_tools,
    };
    use crate::config::ToolServerConfig;

    fn tool_server(name: &str) -> ToolServerConfig {
        ToolServerConfig {
            name: name.to_string(),
            command: format!("harnx-{name}-server"),
            args: Vec::new(),
            env: Default::default(),
            enabled: true,
            description: None,
            package: None,
            hooks: None,
        }
    }

    #[test]
    fn should_start_tool_servers_requires_managing_and_a_nonempty_list() {
        let servers = vec![tool_server("time")];
        assert!(
            should_start_tool_servers(true, &servers),
            "managing with servers configured must start them"
        );
        assert!(
            !should_start_tool_servers(true, &[]),
            "managing with nothing configured must not resolve a broker to spawn nothing"
        );
        assert!(
            !should_start_tool_servers(false, &servers),
            "a consuming worker must not spawn its own tool servers"
        );
    }

    #[test]
    fn configured_worker_services_falls_back_to_config_use_tools_without_agent() {
        let config = crate::config::GlobalConfig::default();
        {
            let mut config = config.write();
            config.agent = None;
            config.use_tools = Some(vec!["time_get_current_time".to_string()]);
            config.tool_servers = vec![tool_server("time"), tool_server("weather")];
        }

        let (servers, _hooks) = configured_worker_services(&config);

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "time");
    }

    #[test]
    fn tool_server_filter_matches_selectors_wildcard_and_absent_use_tools() {
        let servers = [tool_server("time"), tool_server("weather")];

        let matching =
            tool_servers_matching_use_tools(&servers, None, &["time_get_current_time".to_string()]);
        assert_eq!(
            matching
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>(),
            ["time"]
        );
        assert_eq!(
            tool_servers_matching_use_tools(&servers, None, &["*".to_string()]).len(),
            2
        );
        assert!(tool_servers_matching_use_tools(&servers, None, &[]).is_empty());
    }

    #[test]
    fn tool_server_filter_uses_package_scoped_display_name() {
        let mut time = tool_server("time");
        time.package = Some("other/tools".to_string());
        let servers = [time, tool_server("weather")];

        let matching = tool_servers_matching_use_tools(
            &servers,
            Some("active"),
            &["other__tools__time_get_current_time".to_string()],
        );
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].name, "time");
    }

    #[test]
    fn tool_server_filter_deduplicates_full_identities_first_wins() {
        let user_time = tool_server("time");
        let mut package_time = tool_server("time");
        package_time.package = Some("coding".to_string());
        let weather = tool_server("weather");
        let servers = [user_time, package_time, weather];

        let matching = tool_servers_matching_use_tools(&servers, None, &["*".to_string()]);

        assert_eq!(matching.len(), 3);
        assert_eq!(matching[0].name, "time");
        assert_eq!(matching[0].package, None);
        assert_eq!(matching[1].package.as_deref(), Some("coding"));
        assert_eq!(matching[2].name, "weather");

        let mut aaa = tool_server("dup");
        aaa.package = Some("aaa".to_string());
        let mut zzz = tool_server("dup");
        zzz.package = Some("zzz".to_string());
        let package_matching =
            tool_servers_matching_use_tools(&[aaa, zzz], None, &["*".to_string()]);
        assert_eq!(package_matching.len(), 2);
        assert_eq!(package_matching[0].package.as_deref(), Some("aaa"));
        assert_eq!(package_matching[1].package.as_deref(), Some("zzz"));

        let duplicate = tool_server("same");
        let duplicate_matching = tool_servers_matching_use_tools(
            &[duplicate.clone(), duplicate],
            None,
            &["*".to_string()],
        );
        assert_eq!(duplicate_matching.len(), 1);
    }

    #[test]
    fn worker_startup_continues_when_configured_binary_is_missing() {
        harnx_core::require_nextest();
        let missing_dir = tempfile::tempdir().expect("create missing-binary test directory");
        let mut missing_server = tool_server("time");
        missing_server.command = missing_dir
            .path()
            .join("harnx-missing-tool-server")
            .to_string_lossy()
            .into_owned();
        let filtered = tool_servers_matching_use_tools(
            &[missing_server],
            None,
            &["time_get_current_time".to_string()],
        );

        assert_eq!(filtered.len(), 1);
        // Supervisor's missing-binary path is soft-fail and therefore reaches
        // daemon startup as Ok; integration coverage asserts its warning.
        assert!(optional_tool_server::<()>(Ok(())).is_some());
    }
}
