//! End-to-end: a real managing worker starts only the servers its active
//! sessions' agents use, and actually deregisters a server it stops. Built on
//! the pattern in `package_tool_naming_e2e.rs`. See `tool_reconciler.rs` and
//! `tool_reconciler_race.rs` for the fake-launcher unit coverage.

#[allow(dead_code)]
mod common;

use futures_util::TryStreamExt;
use harnx_runtime::config::{
    Config, GlobalConfig, ToolServerConfig, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV,
    LOCAL_CLUSTER_KEY,
};
use harnx_runtime::nats_worker::server_reconciler::{ServerReconciler, SupervisorLauncher};
use harnx_runtime::nats_worker::{
    publish_session_activate, run_worker_daemon, SessionActivate, ToolServerStartConfig,
    WorkerDaemonConfig,
};
use harnx_toolset::Registration;
use harnx_toolset_server::TOOL_REGISTRY_BUCKET;
use parking_lot::RwLock;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const E2E_TOKEN: &str = "tool-reconciler-e2e-token";

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

/// Resolve a workspace binary by its crate/binary name (the two always match
/// for the servers this test builds), building it with `cargo build -p <name>`
/// if a package-scoped test run has not produced it yet. Mirrors
/// `fs_server_binary` in `package_tool_naming_e2e.rs`.
fn resolve_binary(name: &str) -> anyhow::Result<PathBuf> {
    let mut path = std::env::current_exe()?;
    path.pop();
    if path.file_name().is_some_and(|dir| dir == "deps") {
        path.pop();
    }
    path.push(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    });
    if path.is_file() {
        return Ok(path);
    }

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow::anyhow!("resolve workspace root"))?
        .to_path_buf();
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["build", "-p", name])
        .current_dir(workspace)
        .status()?;
    anyhow::ensure!(status.success(), "building {name} failed");
    anyhow::ensure!(path.is_file(), "{name} not found at {}", path.display());
    Ok(path)
}

/// A minimal agent fixture: the name `retrieve_agent` loads it by, and the
/// `use_tools` selector that decides which tool server its session gets.
/// Bundling the two avoids threading a `(name, use_tools)` string pair
/// through every helper that needs one or the other.
struct AgentFixture {
    name: &'static str,
    use_tools: &'static str,
}

/// Write a minimal agent file `retrieve_agent` can load by name: no `model:`
/// (so agent resolution needs no configured client), just a `use_tools`
/// selector naming which tool server this agent's session should get.
fn write_agent(agents_dir: &Path, agent: &AgentFixture) -> anyhow::Result<()> {
    std::fs::write(
        agents_dir.join(format!("{}.md", agent.name)),
        format!(
            "---\nuse_tools: {}\n---\nstub agent instructions\n",
            agent.use_tools
        ),
    )?;
    Ok(())
}

/// Config names of every server currently registered, regardless of which
/// worker scope registered them — the test doesn't know (and doesn't need to
/// know) the scope the worker minted for itself.
async fn registered_config_names(client: &async_nats::Client) -> Vec<String> {
    let Ok(store) = async_nats::jetstream::new(client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await
    else {
        // Bucket doesn't exist yet: no server has attempted to register.
        return Vec::new();
    };
    let Ok(keys) = store.keys().await else {
        return Vec::new();
    };
    let Ok(keys) = keys.try_collect::<Vec<_>>().await else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for key in keys {
        if let Ok(Some(entry)) = store.get(&key).await {
            if let Ok(registration) = serde_json::from_slice::<Registration>(&entry) {
                names.push(registration.config);
            }
        }
    }
    names
}

/// Poll the registry with a deadline until `config_name`'s presence matches
/// `want_present`. Server startup/teardown time varies, so this must never
/// be a fixed sleep.
async fn await_registration_state(
    client: &async_nats::Client,
    config_name: &str,
    want_present: bool,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let present = registered_config_names(client)
            .await
            .iter()
            .any(|name| name == config_name);
        if present == want_present {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "'{config_name}' did not reach the expected registration state within the deadline"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll the registry with a deadline until `config_name` appears.
async fn await_registered(client: &async_nats::Client, config_name: &str) -> anyhow::Result<()> {
    await_registration_state(client, config_name, true).await
}

async fn activate(
    jetstream: &async_nats::jetstream::Context,
    activation: &SessionActivate,
) -> anyhow::Result<()> {
    publish_session_activate(jetstream, LOCAL_CLUSTER_KEY, activation).await?;
    Ok(())
}

/// Build the worker config for the e2e test: a `time` server with no args and
/// a `plans` server pointed at `plans_dir`. Pulled out of the test body,
/// which was over the large-method threshold with this inlined.
fn e2e_worker_config(
    time_binary: PathBuf,
    plans_binary: PathBuf,
    plans_dir: &Path,
) -> GlobalConfig {
    Arc::new(RwLock::new(Config {
        tool_servers: vec![
            ToolServerConfig {
                name: "time".to_string(),
                command: time_binary.to_string_lossy().into_owned(),
                args: Vec::new(),
                env: Default::default(),
                enabled: true,
                description: None,
                package: None,
                hooks: None,
            },
            ToolServerConfig {
                name: "plans".to_string(),
                command: plans_binary.to_string_lossy().into_owned(),
                args: vec![
                    "--dir".to_string(),
                    plans_dir.to_string_lossy().into_owned(),
                ],
                env: Default::default(),
                enabled: true,
                description: None,
                package: None,
                hooks: None,
            },
        ],
        ..Config::default()
    }))
}

/// A trivial stub: ends every turn immediately with no tool calls. What
/// happens after `handle_activation` calls `session_started` (which is
/// everything this test cares about) doesn't depend on it — this exists only
/// so the session's background execution doesn't try to reach a real LLM.
fn stub_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        Box::pin(async move {
            Ok((
                "stub reply".to_string(),
                None,
                Vec::new(),
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_only_starts_the_servers_its_agent_uses() -> anyhow::Result<()> {
    harnx_core::require_nextest();
    let Some(nats) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(E2E_TOKEN.to_string()),
    })
    .await?
    else {
        eprintln!("skipping: nats-server binary not available");
        return Ok(());
    };

    let time_binary = resolve_binary("harnx-time-server")?;
    let plans_binary = resolve_binary("harnx-plans-tools")?;

    const AGENT_TIME: AgentFixture = AgentFixture {
        name: "agent-time",
        use_tools: "time_*",
    };
    const AGENT_PLANS: AgentFixture = AgentFixture {
        name: "agent-plans",
        use_tools: "plans_*",
    };

    let config_root = tempfile::tempdir()?;
    let agents_dir = config_root.path().join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    write_agent(&agents_dir, &AGENT_TIME)?;
    write_agent(&agents_dir, &AGENT_PLANS)?;

    let plans_dir = tempfile::tempdir()?;

    // The worker resolves both its main connection and its tool servers'
    // broker from this handoff (`LOCAL_CLUSTER_KEY` always does, regardless
    // of `HARNX_CONFIG_DIR`), and `retrieve_agent` finds the two agent files
    // above under `HARNX_CONFIG_DIR`.
    let _env = EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, nats.url()),
        (HARNX_NATS_TOKEN_ENV, E2E_TOKEN),
        ("HARNX_CONFIG_DIR", &config_root.path().to_string_lossy()),
    ]);

    let config = e2e_worker_config(time_binary, plans_binary, plans_dir.path());

    let daemon = WorkerDaemonConfig::managing(LOCAL_CLUSTER_KEY, "tool-reconciler-e2e");
    let worker_config = config.clone();
    let _worker = tokio::spawn(async move {
        run_worker_daemon(worker_config, daemon, Some(stub_call_fn())).await
    });

    let client = async_nats::ConnectOptions::new()
        .token(E2E_TOKEN.to_string())
        .connect(nats.url())
        .await?;
    let jetstream = async_nats::jetstream::new(client.clone());

    activate(&jetstream, &SessionActivate::new("s1", AGENT_TIME.name)).await?;
    await_registered(&client, "time").await?;

    assert!(
        !registered_config_names(&client)
            .await
            .iter()
            .any(|name| name == "plans"),
        "no active session uses the plans server, so it must not be running"
    );

    activate(&jetstream, &SessionActivate::new("s2", AGENT_PLANS.name)).await?;
    await_registered(&client, "plans").await?;
    assert!(
        registered_config_names(&client)
            .await
            .iter()
            .any(|name| name == "time"),
        "starting a second session must not disturb the first session's servers"
    );

    Ok(())
}

/// Poll the registry with a deadline until `config_name` no longer appears.
async fn await_deregistered(client: &async_nats::Client, config_name: &str) -> anyhow::Result<()> {
    await_registration_state(client, config_name, false).await
}

/// `SupervisorLauncher::stop` must actually deregister, not just drop the
/// supervisor: `ToolServerSupervisor` relies on its monitor tasks' own exit
/// path for that (see `tool_supervisor.rs`), and `Drop` only aborts those
/// tasks past the point where they'd run it. A bare drop leaves the
/// registration behind until its 90s TTL expires — long after `stop`
/// returned — so a session that starts moments later can be handed a tool
/// backed by an already-dead process.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_server_actually_removes_its_registration() -> anyhow::Result<()> {
    harnx_core::require_nextest();
    let Some(nats) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(E2E_TOKEN.to_string()),
    })
    .await?
    else {
        eprintln!("skipping: nats-server binary not available");
        return Ok(());
    };

    let time_binary = resolve_binary("harnx-time-server")?;
    let client = async_nats::ConnectOptions::new()
        .token(E2E_TOKEN.to_string())
        .connect(nats.url())
        .await?;
    let instance_id = harnx_core::instance::ServerScope::new();
    let start = ToolServerStartConfig::new(client.clone(), instance_id, nats.url(), E2E_TOKEN);
    let launcher = Arc::new(SupervisorLauncher::new(start));
    // Zero linger: `session_ended` stops the server on the same call that
    // drops its last user, so the test doesn't need to wait out a real
    // linger window to exercise `stop`.
    let reconciler = ServerReconciler::new(launcher, Duration::ZERO);

    let time_server = ToolServerConfig {
        name: "time".to_string(),
        command: time_binary.to_string_lossy().into_owned(),
        args: Vec::new(),
        env: Default::default(),
        enabled: true,
        description: None,
        package: None,
        hooks: None,
    };

    reconciler.session_started("s1", vec![time_server]).await;
    await_registered(&client, "time").await?;

    reconciler.session_ended("s1").await;
    await_deregistered(&client, "time").await
}
