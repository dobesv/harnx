//! `--manage-servers` replaces cluster-key topology inference: coverage for
//! the scope resolver, the tool-server spawn gate, and the one place a
//! regression here would silently break every local user (the local
//! orchestrator forgetting to pass the flag).

use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use harnx_runtime::nats_worker::resolve_worker_scope;
use std::path::Path;

#[test]
fn managing_worker_mints_its_own_scope() {
    harnx_core::require_nextest();
    // SAFETY: nextest gives each test its own process, so mutating this
    // process's environment does not race other tests.
    unsafe { std::env::remove_var(HARNX_SERVER_SCOPE) };
    let scope = resolve_worker_scope(true).expect("mint a scope");
    assert!(
        scope
            .as_str()
            .starts_with(&format!("{}-", std::process::id())),
        "a minted scope carries the worker pid; got {}",
        scope.as_str()
    );
}

#[test]
fn consuming_worker_requires_a_scope_from_the_environment() {
    harnx_core::require_nextest();
    // SAFETY: nextest gives each test its own process, so mutating this
    // process's environment does not race other tests.
    unsafe { std::env::remove_var(HARNX_SERVER_SCOPE) };
    let error = resolve_worker_scope(false).expect_err("should require a scope");
    assert!(
        error.to_string().contains(HARNX_SERVER_SCOPE),
        "got: {error}"
    );
}

#[test]
fn consuming_worker_uses_the_configured_scope() {
    harnx_core::require_nextest();
    // SAFETY: nextest gives each test its own process, so mutating this
    // process's environment does not race other tests.
    unsafe { std::env::set_var(HARNX_SERVER_SCOPE, "shared") };
    let scope = resolve_worker_scope(false).expect("read scope");
    assert_eq!(scope, ServerScope::from_string("shared"));
}

#[tokio::test]
async fn a_consuming_worker_launches_no_child_servers() {
    harnx_core::require_nextest();
    // The gates used to key off the cluster name, so a worker on a remote
    // cluster silently got no servers AND could not find any. Pin the half of
    // that which is now intended: without --manage-servers, nothing is spawned.
    let config = harnx_runtime::config::GlobalConfig::default();
    {
        let mut config = config.write();
        config.use_tools = Some(vec!["*".to_string()]);
        config.tool_servers = vec![harnx_runtime::config::ToolServerConfig {
            name: "time".to_string(),
            command: "harnx-time-server".to_string(),
            args: Vec::new(),
            env: Default::default(),
            enabled: true,
            description: None,
            package: None,
            hooks: None,
        }];
    }
    let daemon = harnx_runtime::nats_worker::WorkerDaemonConfig::new("prod", "w1");

    let supervisor =
        harnx_runtime::nats_worker::start_local_tool_servers_for_test(&daemon, &config).await;

    assert!(
        supervisor.is_none(),
        "a worker without --manage-servers must not spawn tool servers"
    );
}

/// The fix-round regression: `start_local_tool_servers` used to have no
/// `servers.is_empty()` short-circuit (unlike the hooks path), so a managing
/// worker with nothing configured to spawn still resolved a local NATS server
/// on every call — and, with no broker address handed down, that falls
/// through to spawning a real shared `nats-server` child just to spawn
/// nothing. No `HARNX_NATS_URL`/`HARNX_NATS_TOKEN` is set here; if the gate
/// regresses, this test hangs or fails trying to stand up a broker rather
/// than returning promptly.
#[tokio::test]
async fn a_managing_worker_with_no_tool_servers_configured_spawns_none() {
    harnx_core::require_nextest();
    let config = harnx_runtime::config::GlobalConfig::default();
    let daemon = harnx_runtime::nats_worker::WorkerDaemonConfig::managing("local", "w1");

    let supervisor =
        harnx_runtime::nats_worker::start_local_tool_servers_for_test(&daemon, &config).await;

    assert!(
        supervisor.is_none(),
        "a managing worker with no tool servers configured must not spawn any, \
         nor resolve or start a local NATS server to do so"
    );
}

/// The regression this task exists to prevent: the local orchestrator must
/// keep passing `--manage-servers` so local users don't silently lose every
/// tool and hook server. Asserts on the constructed command's argv rather
/// than spawning a worker or standing up a broker.
#[test]
fn local_orchestrator_spawns_the_worker_with_manage_servers() {
    harnx_core::require_nextest();
    let command = harnx_runtime::local_orchestrator::build_local_worker_command(
        Path::new("harnx-worker"),
        "nats://127.0.0.1:4222",
        "test-token",
    );
    let args: Vec<String> = command
        .as_std()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert!(
        args.iter().any(|arg| arg == "--manage-servers"),
        "local worker spawn must pass --manage-servers, got argv: {args:?}"
    );
}
