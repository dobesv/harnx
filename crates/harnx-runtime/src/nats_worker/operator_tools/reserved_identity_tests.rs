//! Regression tests: the agentless reserved operator path must act under
//! the operator's own identity (decision 11), not ownerless. Each test drives the real reserved dispatch — NATS reservation,
//! discovery, and request publication — with no mocked transport.

use crate::nats_worker::tests::{
    env_lock, seed_remote_config, spawn_test_nats, subagent_test_env, TestEnvGuard,
};
use crate::nats_worker::tool_reservation::ToolReservationView;
use crate::operator_tools::cli::{run_reserved_tool_command, ToolCommandExecution};
use crate::operator_tools::{OperatorToolCommand, OperatorToolReply};
use crate::tool_reservation_client::ToolReservationHandle;
use crate::utils::create_abort_signal;
use crate::SessionActivationRoute;
use anyhow::{Context as _, Result};
use futures_util::StreamExt;
use harnx_core::instance::ServerScope;
use harnx_toolset::{registration_kv_key, Registration, ToolRequest, ToolSpec};
use std::time::Duration;

/// Publish one registration into the shared tool registry bucket the way a
/// toolset server would, so discovery — not a mock — decides visibility.
async fn publish_registration(
    client: &async_nats::Client,
    scope: &ServerScope,
    registration: Registration,
) -> Result<()> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let store = harnx_nats_common::registry::ensure_bucket_with_ttl(
        &jetstream,
        harnx_toolset_server::TOOL_REGISTRY_BUCKET,
        harnx_nats_common::registry::REGISTRATION_TTL,
        1,
    )
    .await?;
    let key = registration_kv_key(scope.as_str(), &registration);
    store
        .put(&key, bytes::Bytes::from(serde_json::to_vec(&registration)?))
        .await
        .with_context(|| format!("publish registration '{key}'"))?;
    Ok(())
}

fn registration(
    server: &str,
    tool: &str,
    user_id: Option<&str>,
    ownerless_only: bool,
) -> Registration {
    Registration {
        package: None,
        config: server.into(),
        server: server.into(),
        tools: vec![ToolSpec {
            name: tool.into(),
            description: format!("{tool} test tool"),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
            cancellation_guarantee: Default::default(),
            idempotent_hint: true,
            read_only_hint: true,
            // No responder exists in these tests: the timeout bounds how long
            // a dispatched call waits before failing.
            timeout_secs: Some(2),
            meta: None,
        }],
        schema_version: harnx_toolset_server::TOOL_SCHEMA_VERSION,
        proto_version: harnx_toolset::TOOL_PROTOCOL_VERSION,
        user_id: user_id.map(str::to_string),
        ownerless_only,
        connection_id: Some("reserved-identity-tests".into()),
    }
}

/// One reserved command over a fresh reservation, mirroring the CLI's
/// agentless path. `config` supplies the caller identity decision 11 stamps.
async fn reserved_command(
    config: crate::config::Config,
    command: OperatorToolCommand,
) -> Result<OperatorToolReply> {
    let view = ToolReservationView {
        package: None,
        use_tools: vec!["*".into()],
    };
    let mut reservation =
        ToolReservationHandle::open(config, SessionActivationRoute::ClusterShared, view.clone())
            .await?;
    let result = run_reserved_tool_command(
        &reservation,
        &view,
        ToolCommandExecution {
            command: &command,
            json: true,
            abort: &create_abort_signal(),
            bash_timeout_secs: None,
        },
    )
    .await;
    reservation.close().await?;
    result
}

/// The reserved path needs a live worker to answer the reservation
/// handshake; start the real daemon on the seeded config, like the admission
/// tests do.
async fn start_worker(
    dir: &std::path::Path,
) -> Result<tokio_util::task::AbortOnDropHandle<Result<()>>> {
    let mut config = crate::config::Config::load_from_file(&dir.join("config.yaml"))?;
    config.nats_routing = crate::config::NatsRouting::Default;
    config.model = harnx_core::model::Model::new("test", "test-model");
    let config = std::sync::Arc::new(crate::config::ConfigLock::new(config));
    let call: crate::agent_loop::AgentCallFn = std::sync::Arc::new(|_, _, _| {
        Box::pin(async { Ok(("child completed".into(), None, vec![], Default::default())) })
    });
    let readiness = harnx_healthz::Readiness::default();
    let worker = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
        crate::nats_worker::run_worker_daemon(
            config,
            crate::nats_worker::WorkerDaemonConfig::new("local", "reserved-identity"),
            Some(call),
            Some(readiness.clone()),
        ),
    ));
    tokio::time::timeout(Duration::from_secs(30), async {
        while !readiness.is_ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    Ok(worker)
}

/// The reserved config for the seeded local cluster, like the CLI builds it.
fn reserved_config(dir: &std::path::Path) -> Result<crate::config::Config> {
    let mut config = crate::config::Config::load_from_file(&dir.join("config.yaml"))?;
    config.nats_routing = crate::config::NatsRouting::Cluster("local".into());
    config.model = harnx_core::model::Model::new("test", "test-model");
    Ok(config)
}

/// Wait for one dispatched `ToolRequest` for `tool` on the raw `>` tap.
async fn next_tool_request(
    requests: &mut async_nats::Subscriber,
    tool: &str,
) -> Result<ToolRequest> {
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(message) = requests.next().await {
            if let Ok(request) = serde_json::from_slice::<ToolRequest>(&message.payload) {
                if request.tool == tool {
                    return Ok(request);
                }
            }
        }
        anyhow::bail!("no {tool} request reached the wire")
    })
    .await
    .context("timed out waiting for the dispatched tool request")?
}

fn listed_tool_names(reply: &OperatorToolReply) -> Result<Vec<String>> {
    assert!(reply.error.is_none(), "list failed: {:?}", reply.error);
    let tools: Vec<serde_json::Value> = serde_json::from_str(&reply.output)?;
    Ok(tools
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect())
}

fn call(name: &str, args_json: &str) -> OperatorToolCommand {
    OperatorToolCommand::Call {
        name: name.into(),
        args_json: args_json.into(),
        tool_input: None,
    }
}

fn list() -> OperatorToolCommand {
    OperatorToolCommand::List { pattern: None }
}

/// Part 1: an identified operator's reserved calls carry the operator's
/// identity in the real dispatched `ToolRequest`, and discovery scopes user
/// registrations by it — the operator reaches their own `.u.{U}` tool but
/// never an ownerless (`.anon`) one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reserved_operator_identity_reaches_requests_and_scopes_ownerless_tools() -> Result<()> {
    let _lock = env_lock().await;
    let (url, mut nats, _) = spawn_test_nats().await.context("nats-server required")?;
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let scope = ServerScope::new();
    let _scope = TestEnvGuard::new("HARNX_SERVER_SCOPE", scope.as_str());
    std::fs::write(
        seeded.config_dir().join("config.yaml"),
        "model: test:test-model\ntool_use: true\nrun_limits:\n  timeout_secs: 60\nuser_id: bob\n",
    )?;
    let client = async_nats::connect(&url).await?;
    // Start the worker first: its startup reconcile would prune
    // registrations whose connection id has no live tool server behind it.
    let _worker = start_worker(seeded.config_dir()).await?;
    publish_registration(
        &client,
        &scope,
        registration("echo", "echo_ping", None, false),
    )
    .await?;
    publish_registration(
        &client,
        &scope,
        registration("bob_vault", "bob_secret", Some("bob"), false),
    )
    .await?;
    publish_registration(
        &client,
        &scope,
        registration("anon_vault", "anon_secret", None, true),
    )
    .await?;

    let bob = reserved_config(seeded.config_dir())?;
    // The global tool dispatches on the wire; no tool responder exists, but
    // the published request is what proves the identity.
    let mut requests = client.subscribe(">".to_owned()).await?;
    client.flush().await?;
    // Declarations are server-prefixed; the wire request keeps the raw name.
    let _ = reserved_command(bob.clone(), call("echo_echo_ping", "{}")).await;
    let request = next_tool_request(&mut requests, "echo_ping").await?;
    assert_eq!(request.user_id.as_deref(), Some("bob"));

    // Discovery scoped by the operator identity: bob sees the global tool
    // and his own, never the ownerless-only one.
    let names = listed_tool_names(&reserved_command(bob.clone(), list()).await?)?;
    assert!(names.contains(&"echo_echo_ping".to_string()), "{names:?}");
    assert!(
        names.contains(&"bob_vault_bob_secret".to_string()),
        "{names:?}"
    );
    assert!(
        !names.contains(&"anon_vault_anon_secret".to_string()),
        "{names:?}"
    );

    // Bob cannot reach the ownerless credential even by direct call: the
    // tool never enters his catalog, so nothing is dispatched.
    let reply = reserved_command(bob, call("anon_vault_anon_secret", "{}")).await?;
    let error = reply.error.context("ownerless tool must be unavailable")?;
    assert!(
        error.contains("not available in the active session"),
        "{error}"
    );
    nats.kill()?;
    nats.wait()?;
    Ok(())
}

/// Part 2: the ownerless caller is unchanged — it still reaches
/// ownerless-only tools with `user_id: None` on the wire, and does not see
/// bob's user-scoped tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ownerless_reserved_caller_still_reaches_ownerless_tools_only() -> Result<()> {
    let _lock = env_lock().await;
    let (url, mut nats, _) = spawn_test_nats().await.context("nats-server required")?;
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let scope = ServerScope::new();
    let _scope = TestEnvGuard::new("HARNX_SERVER_SCOPE", scope.as_str());
    std::fs::write(
        seeded.config_dir().join("config.yaml"),
        "model: test:test-model\ntool_use: true\nrun_limits:\n  timeout_secs: 60\n",
    )?;
    let client = async_nats::connect(&url).await?;
    // Worker first, then the registrations (see the sibling test).
    let _worker = start_worker(seeded.config_dir()).await?;
    publish_registration(
        &client,
        &scope,
        registration("bob_vault", "bob_secret", Some("bob"), false),
    )
    .await?;
    publish_registration(
        &client,
        &scope,
        registration("anon_vault", "anon_secret", None, true),
    )
    .await?;

    let mut config = reserved_config(seeded.config_dir())?;
    config.user_id = None;
    let mut requests = client.subscribe(">".to_owned()).await?;
    client.flush().await?;
    // Declarations are server-prefixed; the wire request keeps the raw name.
    let _ = reserved_command(config.clone(), call("anon_vault_anon_secret", "{}")).await;
    let request = next_tool_request(&mut requests, "anon_secret").await?;
    assert!(
        request.user_id.is_none(),
        "ownerless caller must stay ownerless on the wire"
    );

    let names = listed_tool_names(&reserved_command(config, list()).await?)?;
    assert!(
        names.contains(&"anon_vault_anon_secret".to_string()),
        "{names:?}"
    );
    assert!(
        !names.contains(&"bob_vault_bob_secret".to_string()),
        "{names:?}"
    );
    nats.kill()?;
    nats.wait()?;
    Ok(())
}
