//! Background service startup: sub-agent toolsets and the on-demand
//! tool-server reconciler.
//!
//! None of this is required for the worker to accept and run a session (see
//! `launch_worker_services`'s readiness-first ordering in `run_worker_daemon`),
//! so failures here are logged and degrade gracefully rather than failing
//! worker startup.

use super::daemon::{WorkerDaemonConfig, WorkerStartup};
use super::server_reconciler::{
    all_enabled_tool_servers, build_server_reconciler, ServerReconciler,
};
use super::subagent_toolset::{SubagentSessionRoute, SubagentToolset};
use super::tool_supervisor::{ToolServerStartConfig, ToolServerSupervisor};
use crate::config::{
    list_agents, resolve_local_nats_server_config, selector_could_match_server,
    server_display_name, Config, GlobalConfig, ToolServerConfig,
};
use anyhow::{Context, Result};
use async_nats::jetstream;
use futures_util::future::join_all;
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
            let matches = namespaced_use_tools
                .iter()
                .any(|selector| selector_could_match_server(selector, &display_name));
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
/// Pulled out of `start_local_tool_servers` so the decision itself — not a copy of it —
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
            .with_broker_settings(&harnx_nats_common::connect::NatsEndpoint::from(&server));
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
    let servers = configured_worker_services(config);
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

/// Test entry point for [`configured_worker_services`]: the same selection
/// worker startup and session reconciliation apply, without standing up a
/// worker daemon. Not part of the crate's real API — production code always
/// goes through the daemon paths.
// qual:test_helper — same selection as worker startup/reconciliation, exposed for the script-tools cold-start e2e.
#[doc(hidden)]
pub fn configured_worker_services_for_test(config: &GlobalConfig) -> Vec<ToolServerConfig> {
    configured_worker_services(config)
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
    config: GlobalConfig,
    agent: String,
    route: SubagentSessionRoute,
    instance_id: harnx_core::instance::ServerScope,
    client: async_nats::Client,
    jetstream: jetstream::Context,
    replicas: usize,
    session_metadata: crate::nats_session_metadata::SessionMetadataStore,
    lease_acquisition_timeout: std::time::Duration,
}

async fn start_subagent_toolset(start: SubagentToolsetStart) -> Result<JoinHandle<Result<()>>> {
    let SubagentToolsetStart {
        config,
        agent,
        route,
        instance_id,
        client,
        jetstream,
        replicas,
        session_metadata,
        lease_acquisition_timeout,
    } = start;
    let package = harnx_core::package_namespace::pkg_from_qualified(&agent).map(str::to_string);
    let registration_context = jetstream.clone();
    let policy = super::subagent_toolset::TargetRunPolicy::resolve(&config.read(), &agent)?;
    let default_user_id = config.read().default_user_id_for_cluster(route.cluster());
    let toolset = Arc::new(
        SubagentToolset::new(
            agent,
            route,
            super::subagent_toolset::SubagentNats::new(
                client.clone(),
                jetstream,
                session_metadata,
                replicas,
            )
            .with_lease_acquisition_timeout(lease_acquisition_timeout),
        )
        .with_run_policy(policy)
        .with_default_user_id(default_user_id),
    );
    let server_name = harnx_toolset::Toolset::name(toolset.as_ref()).to_string();
    let identity_token = harnx_toolset::server_identity_token(package.as_deref(), "", &server_name);
    let registration_key = harnx_toolset_server::registration_key(&instance_id, &identity_token);
    let connection = harnx_nats_common::connect::NatsConnection { client, replicas };
    let server = tokio::spawn(harnx_toolset_server::serve_with_client_and_identity(
        toolset,
        instance_id,
        connection,
        harnx_toolset_server::RegistrationIdentity::new(package, ""),
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
    pub(super) session_metadata: crate::nats_session_metadata::SessionMetadataStore,
    pub(super) background_services_attempted: tokio::sync::watch::Receiver<bool>,
    /// `None` for a consuming worker, or a managing worker with nothing
    /// configured to spawn. Tool servers now start on demand per session
    /// (see `handle_activation`) rather than as one batch here.
    pub(super) server_reconciler: Option<Arc<ServerReconciler>>,
}

/// Sub-agent toolsets, started after the worker has already
/// announced readiness. Held only to keep the children alive for the
/// worker's lifetime. Tool servers are no longer part of this: they start
/// per session through `WorkerRuntime::server_reconciler`.
pub(super) struct BackgroundServices {
    _subagent_tool_servers: Vec<JoinHandle<Result<()>>>,
}

/// Tool servers this worker should run for the config it is holding.
///
/// Selection follows the active agent's `use_tools` (falling back to the
/// global `use_tools`), its `bash_tools`, and the `tools:`
/// selectors of every middleware entry active for that agent, so a server
/// that only hosts middleware helper tools starts too. Middleware selectors
/// go through `tool_servers_for_view` with the same agent package the
/// middleware itself resolves tools with, and matching ignores middleware
/// `match` patterns: the session can call a matching tool at any time, and
/// the server should already be running when it does. A middleware
/// resolution error warns and continues with the agent-derived servers; the
/// failing turn still reports the middleware error through its own path.
pub(super) fn configured_worker_services(config: &GlobalConfig) -> Vec<ToolServerConfig> {
    let config = config.read();
    let agent_package = config
        .agent
        .as_ref()
        .and_then(|agent| harnx_core::package_namespace::pkg_from_qualified(agent.name()));
    let mut selectors = config
        .agent
        .as_ref()
        .and_then(|agent| agent.use_tools())
        // Fall back when no agent is active or the active agent has no use_tools.
        .or_else(|| config.use_tools.clone())
        .unwrap_or_default();
    // Script mode (T3.4): a bash-only agent has no use_tools but still
    // needs the tool servers behind its bash_tools selectors, or the
    // inner set resolves to nothing on the worker.
    if let Some(bash_tools) = config.agent.as_ref().and_then(|agent| agent.bash_tools()) {
        selectors.extend(bash_tools.clone());
    }
    // Script tools (#2813, decision 13): a selected script tool wraps tools
    // resolved through its own `tools:` selectors, so the servers behind
    // those must start too. Unselected script tools contribute nothing.
    selectors.extend(config.script_tool_inner_selectors_for(&selectors));
    // #2796: middleware helper tools resolve with the same package context as
    // the agent's own selectors (see middleware/wiring.rs), so their servers
    // join the same selection instead of failing as not allowlisted.
    match config.resolved_middleware() {
        Ok(chain) => selectors.extend(
            chain
                .entries
                .iter()
                .flat_map(|entry| entry.tools.iter().cloned()),
        ),
        Err(error) => {
            log::warn!("could not resolve middleware for tool-server selection: {error:#}")
        }
    }
    match config.resolved_event_hooks() {
        Ok(hooks) => selectors.extend(hooks.tool_selectors().map(str::to_owned)),
        Err(error) => {
            log::warn!("could not resolve event hooks for tool-server selection: {error:#}")
        }
    }
    tool_servers_for_view(&config, agent_package, &selectors)
}

/// Configured servers that could supply tools for an explicit package/selector view.
///
/// Does not consult the active agent or global `use_tools`. Matching is conservative
/// because tool declarations aren't available until the servers have started.
pub(crate) fn tool_servers_for_view(
    config: &Config,
    package: Option<&str>,
    selectors: &[String],
) -> Vec<ToolServerConfig> {
    if selectors.is_empty() {
        return Vec::new();
    }
    let mut namespaced_selectors = Vec::new();
    for selector in config.expanded_tool_selectors(&selectors.join(",")) {
        if let Some(namespaced) = namespaced_selector(&selector, package) {
            namespaced_selectors.push(namespaced);
        }
        namespaced_selectors.push(selector);
    }
    tool_servers_matching_use_tools(&config.tool_servers, package, &namespaced_selectors)
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
    // Canonical metadata is a hard protocol prerequisite. Establish it before
    // announcing readiness or detaching any background service so a worker
    // that cannot load metadata never appears ready and leaves no orphaned
    // startup tasks behind.
    let session_metadata =
        super::daemon::ensure_session_metadata(&startup.jetstream, startup.replicas).await?;

    // Don't gate readiness on sub-agent toolsets: a broken server
    // must not stall the frontend past its startup deadline. The daemon
    // announces readiness after its request subscriptions are flushed, while
    // these services start in the background. Tool servers start on demand
    // through the reconciler for activations and tool reservations.

    let background = Arc::new(Mutex::new(None));
    // Session-specific tool servers are awaited during activation, but the
    // worker-wide sub-agent toolsets still start in the background. Keep the
    // first activation behind this barrier so discovery sees their registrations.
    let (background_services_tx, background_services_attempted) =
        tokio::sync::watch::channel(false);

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
        background_services_attempted: background_services_tx,
        session_metadata: session_metadata.clone(),
    });

    Ok(WorkerServices {
        background,
        session_metadata,
        background_services_attempted,
        server_reconciler,
    })
}

/// Block until the worker's first sub-agent registration round has finished,
/// so the session's registry snapshot includes every available delegation
/// toolset. Session-specific tool servers have their own activation barrier.
pub(super) async fn await_initial_background_services(
    services_attempted: &tokio::sync::watch::Receiver<bool>,
) {
    if *services_attempted.borrow() {
        return;
    }
    let mut attempted = services_attempted.clone();
    log::debug!("waiting for the first background service registration round");
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
            "background services did not finish their first registration round within {}s; \
             starting this session with the registrations available so far",
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
    background_services_attempted: tokio::sync::watch::Sender<bool>,
    session_metadata: crate::nats_session_metadata::SessionMetadataStore,
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
            background_services_attempted,
            session_metadata,
        } = ctx;
        // Tool servers aren't started here anymore — each session's own
        // servers start on demand through `WorkerRuntime::server_reconciler`.
        let lease_acquisition_timeout =
            std::time::Duration::from_secs(config.read().nats_lease_acquisition_timeout_secs);
        let registrations = list_agents().into_iter().map(|agent| {
            let start = SubagentToolsetStart {
                config: config.clone(),
                agent: agent.clone(),
                route: SubagentSessionRoute::new(
                    daemon.connection_key(),
                    daemon.activation_route(),
                ),
                instance_id: instance_id.clone(),
                client: client.clone(),
                jetstream: jetstream.clone(),
                replicas,
                session_metadata: session_metadata.clone(),
                lease_acquisition_timeout,
            };
            async move { (agent, start_subagent_toolset(start).await) }
        });
        let mut subagent_tool_servers = Vec::new();
        for (agent, result) in join_all(registrations).await {
            match result {
                Ok(handle) => subagent_tool_servers.push(handle),
                // One unusable sub-agent must not cost the others their toolset.
                Err(error) => {
                    log::warn!("sub-agent toolset for '{agent}' unavailable: {error:#}")
                }
            }
        }
        let _ = background_services_attempted.send(true);

        *slot.lock().await = Some(BackgroundServices {
            _subagent_tool_servers: subagent_tool_servers,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::{
        configured_worker_services, optional_tool_server, should_start_tool_servers,
        tool_servers_for_view, tool_servers_matching_use_tools,
    };
    use crate::config::{Agent, AgentConfig, Config, ToolServerConfig};
    use harnx_core::hooks::HooksConfig;
    use harnx_core::middleware::{MiddlewareDefinition, MiddlewareEntry, MiddlewareSourceConfig};

    fn tool_server(name: &str) -> ToolServerConfig {
        ToolServerConfig {
            middleware: Default::default(),
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

    fn middleware_with_tools(name: &str, tools: &[&str]) -> MiddlewareEntry {
        MiddlewareEntry::Full(MiddlewareDefinition {
            name: name.to_string(),
            matcher: ".*".to_string(),
            source: MiddlewareSourceConfig::Bash {
                bash: "next".to_string(),
            },
            tools: tools.iter().map(|tool| tool.to_string()).collect(),
            fetch: Vec::new(),
            timeout: std::time::Duration::from_secs(30),
            on_error: Default::default(),
        })
    }

    fn disabled_middleware(name: &str) -> MiddlewareEntry {
        MiddlewareEntry::Disable {
            name: name.to_string(),
            enabled: false,
        }
    }

    fn worker_config_with_global_middleware(
        agent_markdown: &str,
        middleware: MiddlewareEntry,
        tool_servers: Vec<ToolServerConfig>,
    ) -> crate::config::GlobalConfig {
        let config = crate::config::GlobalConfig::default();
        {
            let mut config = config.write();
            config.middleware.entries.push(middleware);
            config.agent = Some(Agent::new(
                AgentConfig::from_markdown("worker", agent_markdown).unwrap(),
            ));
            config.tool_servers = tool_servers;
        }
        config
    }

    fn configured_server_names(config: &crate::config::GlobalConfig) -> Vec<String> {
        configured_worker_services(config)
            .into_iter()
            .map(|server| server.name)
            .collect()
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

        let servers = configured_worker_services(&config);

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "time");
    }

    #[test]
    fn configured_worker_services_starts_servers_for_bash_only_agent() {
        // Script mode (T3.4, C1 from the PR1 review): an agent with
        // `bash_tools` and no `use_tools` still starts the tool servers
        // behind its bash_tools selectors, or the inner set resolves to
        // nothing on the worker.
        let config = crate::config::GlobalConfig::default();
        {
            let mut config = config.write();
            let mut agent = AgentConfig::from_prompt("worker");
            agent.set_bash_tools(Some(vec!["time_*".to_string()]));
            config.agent = Some(Agent::new(agent));
            config.tool_servers = vec![tool_server("time"), tool_server("weather")];
        }

        let servers = configured_worker_services(&config);

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "time");
    }

    #[test]
    fn tool_servers_for_view_selects_fs_without_using_agent_or_global_selectors() {
        let mut config = Config {
            agent: Some(Agent::new(
                AgentConfig::from_markdown("other/worker", "---\nuse_tools: [weather_*]\n---\n")
                    .unwrap(),
            )),
            tool_servers: vec![
                tool_server("fs"),
                tool_server("time"),
                tool_server("weather"),
            ],
            ..Default::default()
        };

        config.use_tools = Some(vec!["time_*".to_string()]);

        let servers = tool_servers_for_view(&config, None, &["fs_*".to_string()]);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "fs");
        assert_eq!(servers[0].command, config.tool_servers[0].command);
        assert!(tool_servers_for_view(&config, None, &[]).is_empty());
        assert!(tool_servers_for_view(&config, None, &["missing_*".to_string()]).is_empty());
    }

    #[test]
    fn tool_servers_for_view_expands_toolsets_before_package_activation() {
        let mut fs = tool_server("fs");
        fs.package = Some("coding".into());
        let mut config = Config {
            tool_servers: vec![fs, tool_server("time")],
            ..Default::default()
        };
        config
            .toolsets
            .insert("inspection".into(), vec!["coding__fs_read".into()]);
        let servers = tool_servers_for_view(&config, None, &["inspection".into()]);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "fs");
        assert_eq!(servers[0].package.as_deref(), Some("coding"));
        config
            .toolsets
            .insert("local_read".into(), vec!["fs_read".into()]);
        let servers = tool_servers_for_view(&config, Some("coding"), &["local_read".into()]);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].package.as_deref(), Some("coding"));
        config.toolsets.insert("empty".into(), vec![]);
        assert!(tool_servers_for_view(&config, None, &["empty".into()]).is_empty());
    }

    #[test]
    fn tool_servers_for_view_matches_package_scoped_selectors() {
        let mut local_fs = tool_server("fs");
        local_fs.package = Some("coding".to_string());
        let mut other_fs = tool_server("fs");
        other_fs.package = Some("other/tools".to_string());
        let config = Config {
            tool_servers: vec![tool_server("fs"), local_fs, other_fs],
            ..Default::default()
        };

        let cases = [
            (None, "fs_*", vec![None]),
            (Some("coding"), "fs_*", vec![None, Some("coding")]),
            (None, "coding__fs_*", vec![Some("coding")]),
            (
                Some("coding"),
                "other__tools__fs_read",
                vec![Some("other/tools")],
            ),
            (
                Some("coding"),
                "*",
                vec![None, Some("coding"), Some("other/tools")],
            ),
        ];
        for (package, selector, expected_packages) in cases {
            let servers = tool_servers_for_view(&config, package, &[selector.to_string()]);
            let packages: Vec<_> = servers
                .iter()
                .map(|server| server.package.as_deref())
                .collect();
            assert_eq!(
                packages, expected_packages,
                "package={package:?}, selector={selector}"
            );
        }
    }

    #[test]
    fn configured_worker_services_preserves_agent_selector_precedence_and_fallback() {
        let config = crate::config::GlobalConfig::default();
        {
            let mut config = config.write();
            let mut fs = tool_server("fs");
            fs.package = Some("coding".to_string());
            config.tool_servers = vec![fs, tool_server("time")];
            config.use_tools = Some(vec!["time_*".to_string()]);
            config.agent = Some(Agent::new(
                AgentConfig::from_markdown("coding/worker", "---\nuse_tools: [fs_*]\n---\n")
                    .unwrap(),
            ));
        }
        let servers = configured_worker_services(&config);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "fs");
        assert_eq!(servers[0].package.as_deref(), Some("coding"));

        config.write().agent = Some(Agent::new(
            AgentConfig::from_markdown("coding/worker", "Worker prompt").unwrap(),
        ));
        let servers = configured_worker_services(&config);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "time");

        config.write().agent = Some(Agent::new(
            AgentConfig::from_markdown("coding/worker", "---\nuse_tools: []\n---\n").unwrap(),
        ));
        assert!(configured_worker_services(&config).is_empty());
    }

    #[test]
    fn configured_worker_services_starts_servers_only_middleware_needs() {
        // #2796: `plans` and `weather` are excluded from the agent's
        // use_tools, but agent-front-matter and global middleware declare
        // their tools, so both servers must start alongside `fs`.
        let config = worker_config_with_global_middleware(
            "---\nuse_tools: [fs_*]\nmiddleware: [{name: agent-helper, match: '.*', bash: next, tools: [plans_list]}]\n---\n",
            middleware_with_tools("global-helper", &["weather_get_forecast"]),
            vec![
                tool_server("fs"),
                tool_server("plans"),
                tool_server("weather"),
            ],
        );

        let names = configured_server_names(&config);

        assert_eq!(names, ["fs", "plans", "weather"]);
    }

    #[test]
    fn configured_worker_services_starts_servers_needed_only_by_hooks_for_every_event() {
        let config = crate::config::GlobalConfig::default();
        {
            let mut config = config.write();
            config.agent = Some(Agent::new(
                AgentConfig::from_markdown(
                    "worker",
                    r#"---
hooks:
  SessionStart:
    - {name: start, bash: next, tools: [fs_read]}
  UserPromptSubmit:
    - {name: prompt, bash: next, tools: [time_get_current_time]}
  Stop:
    - {name: stop, bash: next, tools: [plans_list]}
  StopFailure:
    - {name: failure, bash: next, tools: [weather_get_forecast]}
---
"#,
                )
                .unwrap(),
            ));
            config.tool_servers = vec![
                tool_server("fs"),
                tool_server("time"),
                tool_server("plans"),
                tool_server("weather"),
                tool_server("unused"),
            ];
        }

        let names = configured_server_names(&config);

        assert_eq!(names, ["fs", "time", "plans", "weather"]);
    }

    #[test]
    fn configured_worker_services_ignores_disabled_hook_tool_selectors() {
        let config = crate::config::GlobalConfig::default();
        {
            let mut config = config.write();
            config.hooks = Some(Box::new(
                serde_yaml::from_str::<HooksConfig>(
                    r#"Stop:
  - {name: helper, bash: next, tools: [weather_get_forecast]}
"#,
                )
                .unwrap(),
            ));
            config
                .middleware
                .entries
                .push(middleware_with_tools("helper-middleware", &["plans_list"]));
            config.agent = Some(Agent::new(
                AgentConfig::from_markdown(
                    "worker",
                    r#"---
use_tools: [fs_*]
hooks:
  Stop:
    - {name: helper, enabled: false}
---
"#,
                )
                .unwrap(),
            ));
            config.tool_servers = vec![
                tool_server("fs"),
                tool_server("plans"),
                tool_server("weather"),
            ];
        }

        let names = configured_server_names(&config);

        assert_eq!(names, ["fs", "plans"]);
    }

    #[test]
    fn configured_worker_services_keeps_agent_and_middleware_servers_when_hook_resolution_fails() {
        // A disable for an unknown hook makes hook resolution fail. The warning
        // must not discard servers selected by the agent or middleware.
        let config = worker_config_with_global_middleware(
            r#"---
use_tools: [fs_*]
hooks:
  Stop:
    - {name: ghost, enabled: false}
---
"#,
            middleware_with_tools("helper", &["weather_get_forecast"]),
            vec![
                tool_server("fs"),
                tool_server("weather"),
                tool_server("plans"),
            ],
        );

        let names = configured_server_names(&config);

        assert_eq!(names, ["fs", "weather"]);
    }

    #[test]
    fn configured_worker_services_starts_a_server_declaring_its_own_middleware() {
        // aws-creds shape (#2796): the middleware lives in the tool
        // server's own file, so the server is needed only by middleware
        // declared within it.
        let config = crate::config::GlobalConfig::default();
        {
            let mut config = config.write();
            let mut weather = tool_server("weather");
            weather.middleware.entries.push(middleware_with_tools(
                "weather-auth",
                &["weather_get_forecast"],
            ));
            config.agent = Some(Agent::new(
                AgentConfig::from_markdown("worker", "---\nuse_tools: [fs_*]\n---\n").unwrap(),
            ));
            config.tool_servers = vec![tool_server("fs"), weather];
        }

        let names = configured_server_names(&config);

        assert_eq!(names, ["fs", "weather"]);
    }

    #[test]
    fn configured_worker_services_ignores_middleware_removed_by_a_later_disable() {
        // The agent disables the global entry, so it never reaches the
        // resolved chain and its tools create no server demand.
        let config = worker_config_with_global_middleware(
            "---\nuse_tools: [fs_*]\nmiddleware: [{name: helper, enabled: false}]\n---\n",
            middleware_with_tools("helper", &["weather_get_forecast"]),
            vec![tool_server("fs"), tool_server("weather")],
        );

        let names = configured_server_names(&config);

        assert_eq!(names, ["fs"]);
    }

    #[test]
    fn configured_worker_services_ignores_middleware_in_a_disabled_server_file() {
        let config = crate::config::GlobalConfig::default();
        {
            let mut config = config.write();
            let mut weather = tool_server("weather");
            weather.enabled = false;
            weather.middleware.entries.push(middleware_with_tools(
                "weather-auth",
                &["weather_get_forecast"],
            ));
            config.agent = Some(Agent::new(
                AgentConfig::from_markdown("worker", "---\nuse_tools: [fs_*]\n---\n").unwrap(),
            ));
            config.tool_servers = vec![tool_server("fs"), weather];
        }

        let names = configured_server_names(&config);

        assert_eq!(names, ["fs"]);
    }

    #[test]
    fn configured_worker_services_does_not_duplicate_middleware_selected_servers() {
        // Agent and middleware selectors overlap on both servers; each
        // configured server still appears at most once.
        let config = crate::config::GlobalConfig::default();
        {
            let mut config = config.write();
            config.middleware.entries.push(middleware_with_tools(
                "helper",
                &["weather_get_forecast", "fs_*"],
            ));
            config.agent = Some(Agent::new(
                AgentConfig::from_markdown("worker", "---\nuse_tools: [weather_*]\n---\n").unwrap(),
            ));
            config.tool_servers = vec![tool_server("fs"), tool_server("weather")];
        }

        let names = configured_server_names(&config);

        assert_eq!(names, ["fs", "weather"]);
    }

    #[test]
    fn configured_worker_services_keeps_agent_servers_when_middleware_resolution_fails() {
        // Script paths are validated at config load, so the error is induced
        // with a Disable naming an entry no earlier level declared —
        // `apply_level` rejects it at resolution time. Selection must warn
        // and fall back to the agent's servers instead of aborting.
        let config = crate::config::GlobalConfig::default();
        {
            let mut config = config.write();
            config.middleware.entries.push(disabled_middleware("ghost"));
            config.agent = Some(Agent::new(
                AgentConfig::from_markdown("worker", "---\nuse_tools: [fs_*]\n---\n").unwrap(),
            ));
            config.tool_servers = vec![tool_server("fs"), tool_server("weather")];
        }

        let names = configured_server_names(&config);

        assert_eq!(names, ["fs"]);
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
