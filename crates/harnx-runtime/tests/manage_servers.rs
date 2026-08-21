//! `--manage-servers` replaces cluster-key topology inference: coverage for
//! the scope resolver, the tool-server spawn gate, and the one place a
//! regression here would silently break every local user (the local
//! orchestrator forgetting to pass the flag).

#[allow(dead_code)]
mod common;

use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use harnx_runtime::config::{HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use harnx_runtime::nats_worker::resolve_worker_scope;
use std::ffi::OsString;
use std::path::Path;

const TEST_TOKEN: &str = "manage-servers-test-token";

/// Restores env vars this test overrode, so a later test in the same process
/// (nextest gives each test its own, but this keeps the pattern honest if
/// that ever changes) sees the environment it expected.
struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    fn install(values: &[(&'static str, &str)]) -> Self {
        let previous = values
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in values {
            // SAFETY: nextest gives each test its own process, so mutating
            // this process's environment does not race other tests.
            unsafe { std::env::set_var(name, value) };
        }
        Self(previous)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..) {
            // SAFETY: see `install`.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

/// Bring up a standalone `nats-server` and point `HARNX_NATS_URL`/
/// `HARNX_NATS_TOKEN` at it for the rest of the test.
///
/// Deliberately not `ensure_shared_server`/`resolve_local_nats_server_config`'s
/// lock-owned shared broker: that API drops its `SharedNatsServer` guard (and
/// so kills an owned child) the instant it returns, which self-destructs a
/// server nobody is holding onto — exactly what a bare, unheld resolve call
/// in an isolated test would do. Handing a URL/token through the env instead
/// makes `resolve_local_nats_server_config` take its direct-handoff branch,
/// so every call in this test (the shim's own, and the one inside
/// `start_local_tool_servers`) talks to the one server this function owns
/// for the test's duration, and skips the shared-broker lock entirely.
///
/// Returns `None` (having printed why) when `nats-server` isn't available, so
/// callers can skip the test the same way `common::spawn_nats_server` callers
/// do.
async fn hold_isolated_shared_nats_server() -> Option<(common::NatsServerHandle, EnvGuard)> {
    let server = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TEST_TOKEN.to_string()),
    })
    .await
    .expect("spawn standalone NATS server")?;
    let env = EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, server.url()),
        (HARNX_NATS_TOKEN_ENV, TEST_TOKEN),
    ]);
    Some((server, env))
}

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
    // that which is now intended: without --manage-servers, nothing is
    // spawned, even with a live broker and a configured tool server on hand.
    //
    // The shim below does no gating of its own — it builds a client and calls
    // straight through to `start_local_tool_servers` — so it needs a real,
    // reachable broker for this test to mean anything (see
    // `hold_isolated_shared_nats_server`).
    let Some(_guard) = hold_isolated_shared_nats_server().await else {
        return;
    };
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
/// nothing. `should_start_tool_servers` (unit-tested directly in
/// `daemon.rs`) is the extracted decision; this test exercises the real
/// function that gates on it, not a copy of the gate.
///
/// The shim genuinely delegates — no gating of its own — so it always builds
/// a real client and calls straight through. With a live broker held for the
/// duration (see `hold_isolated_shared_nats_server`), that succeeds
/// regardless of the guard's state, so what actually distinguishes pass from
/// fail is `start_local_tool_servers` itself: if its guard regresses,
/// `ToolServerSupervisor::start_local` runs with an empty server list and
/// comes back `Some` (a supervisor managing nothing) instead of `None`, and
/// this assertion catches that.
#[tokio::test]
async fn a_managing_worker_with_no_tool_servers_configured_spawns_none() {
    harnx_core::require_nextest();
    let Some(_guard) = hold_isolated_shared_nats_server().await else {
        return;
    };
    let config = harnx_runtime::config::GlobalConfig::default();
    let daemon = harnx_runtime::nats_worker::WorkerDaemonConfig::managing("local", "w1");

    let supervisor =
        harnx_runtime::nats_worker::start_local_tool_servers_for_test(&daemon, &config).await;

    assert!(
        supervisor.is_none(),
        "a managing worker with no tool servers configured must not spawn any"
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
        "local-test",
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
    assert!(args
        .windows(2)
        .any(|args| args == ["--session-scope", "__local__"]));
    assert!(args
        .windows(2)
        .any(|args| args == ["--worker-id", "local-test"]));
}
