//! Refcount and linger rules for `ServerReconciler`, pinned against a fake
//! launcher so they need no broker and no child processes; plus one
//! end-to-end test that a real managing worker only starts the servers its
//! active sessions' agents actually use.

#[allow(dead_code)]
mod common;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use harnx_runtime::config::{
    Config, GlobalConfig, ToolServerConfig, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV,
    LOCAL_CLUSTER_KEY,
};
use harnx_runtime::nats_worker::server_reconciler::{
    ServerLauncher, ServerReconciler, SupervisorLauncher,
};
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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct FakeLauncher {
    started: Mutex<Vec<String>>,
    stopped: Mutex<Vec<String>>,
}

#[async_trait]
impl ServerLauncher for FakeLauncher {
    async fn start(&self, server: &ToolServerConfig) -> anyhow::Result<()> {
        self.started.lock().unwrap().push(server.name.clone());
        Ok(())
    }
    async fn stop(&self, config_name: &str) {
        self.stopped.lock().unwrap().push(config_name.to_string());
    }
}

/// A launcher whose `start` always fails, for the "one bad server must not
/// cost the session the others" rule.
#[derive(Default)]
struct FailingLauncher {
    started_attempts: Mutex<Vec<String>>,
}

#[async_trait]
impl ServerLauncher for FailingLauncher {
    async fn start(&self, server: &ToolServerConfig) -> anyhow::Result<()> {
        self.started_attempts
            .lock()
            .unwrap()
            .push(server.name.clone());
        anyhow::bail!("boom")
    }
    async fn stop(&self, _config_name: &str) {}
}

fn tool_server(name: &str) -> ToolServerConfig {
    // `ToolServerConfig` does not derive `Default`, so every field is
    // explicit. Same shape as `fn tool_server` in the `daemon.rs` test module
    // — keep the two in sync.
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

#[tokio::test]
async fn a_server_two_sessions_share_stays_up_until_both_end() {
    harnx_core::require_nextest();
    let launcher = Arc::new(FakeLauncher::default());
    let reconciler = ServerReconciler::new(launcher.clone(), Duration::ZERO);

    reconciler
        .session_started("s1", vec![tool_server("time")])
        .await;
    reconciler
        .session_started("s2", vec![tool_server("time")])
        .await;
    assert_eq!(reconciler.running().await, vec!["time"]);
    assert_eq!(
        launcher.started.lock().unwrap().len(),
        1,
        "the second session must reuse the running server, not start a second one"
    );

    reconciler.session_ended("s1").await;
    assert_eq!(
        reconciler.running().await,
        vec!["time"],
        "s2 still needs it"
    );
    assert!(launcher.stopped.lock().unwrap().is_empty());

    reconciler.session_ended("s2").await;
    assert!(reconciler.running().await.is_empty());
    assert_eq!(launcher.stopped.lock().unwrap().as_slice(), ["time"]);
}

#[tokio::test]
async fn linger_keeps_a_server_up_across_back_to_back_sessions() {
    harnx_core::require_nextest();
    let launcher = Arc::new(FakeLauncher::default());
    let reconciler = ServerReconciler::new(launcher.clone(), Duration::from_secs(30));

    reconciler
        .session_started("s1", vec![tool_server("time")])
        .await;
    reconciler.session_ended("s1").await;
    assert_eq!(
        reconciler.running().await,
        vec!["time"],
        "within the linger window the process should be reused, not restarted"
    );

    reconciler
        .session_started("s2", vec![tool_server("time")])
        .await;
    assert_eq!(
        launcher.started.lock().unwrap().len(),
        1,
        "no restart should have happened"
    );
    assert!(launcher.stopped.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unrelated_servers_do_not_share_a_refcount() {
    harnx_core::require_nextest();
    let launcher = Arc::new(FakeLauncher::default());
    let reconciler = ServerReconciler::new(launcher.clone(), Duration::ZERO);

    reconciler
        .session_started("s1", vec![tool_server("time"), tool_server("plans")])
        .await;
    assert_eq!(reconciler.running().await, vec!["plans", "time"]);

    reconciler.session_ended("s1").await;
    assert!(
        reconciler.running().await.is_empty(),
        "both servers lost their only user"
    );
    let mut stopped = launcher.stopped.lock().unwrap().clone();
    stopped.sort();
    assert_eq!(stopped, vec!["plans", "time"]);
}

#[tokio::test]
async fn a_server_that_fails_to_start_is_not_counted_as_running() {
    harnx_core::require_nextest();
    let launcher = Arc::new(FailingLauncher::default());
    let reconciler = ServerReconciler::new(launcher.clone(), Duration::ZERO);

    reconciler
        .session_started("s1", vec![tool_server("broken")])
        .await;

    assert!(
        reconciler.running().await.is_empty(),
        "a server that failed to start must not be tracked as running"
    );
    assert_eq!(
        launcher.started_attempts.lock().unwrap().as_slice(),
        ["broken"]
    );
}

/// A launcher whose `stop` blocks on a gate the test controls, so a test can
/// hold a teardown open for exactly as long as it needs and no longer — no
/// sleeps, no timing guesses. `entered_stop` fires the moment `stop` is
/// called (so the test knows it is safe to act "during" the teardown), and
/// `stop` itself doesn't return until the test sends on `release_stop`.
struct GatedLauncher {
    started: Mutex<Vec<String>>,
    stopped: Mutex<Vec<String>>,
    entered_stop: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release_stop: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl GatedLauncher {
    fn new() -> (
        Arc<Self>,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let launcher = Arc::new(Self {
            started: Mutex::new(Vec::new()),
            stopped: Mutex::new(Vec::new()),
            entered_stop: Mutex::new(Some(entered_tx)),
            release_stop: Mutex::new(Some(release_rx)),
        });
        (launcher, entered_rx, release_tx)
    }
}

#[async_trait]
impl ServerLauncher for GatedLauncher {
    async fn start(&self, server: &ToolServerConfig) -> anyhow::Result<()> {
        self.started.lock().unwrap().push(server.name.clone());
        Ok(())
    }
    async fn stop(&self, config_name: &str) {
        self.stopped.lock().unwrap().push(config_name.to_string());
        if let Some(entered) = self.entered_stop.lock().unwrap().take() {
            let _ = entered.send(());
        }
        let release = self.release_stop.lock().unwrap().take();
        if let Some(release) = release {
            let _ = release.await;
        }
    }
}

/// A session that asks for a server while `sweep` is mid-teardown of that
/// same name must end up with a running, registered server of its own —
/// not silently joined to an entry that teardown is about to delete out
/// from under it (see `server_reconciler.rs` module docs on `Slot`).
///
/// The whole scenario is driven by two oneshot gates rather than any sleep:
/// `entered_stop` proves the test has actually reached the window between
/// "teardown begun" and "entry removed" before it acts, and `release_stop`
/// lets teardown finish only once the racing session has had its chance to
/// observe that window.
#[tokio::test]
async fn a_session_that_arrives_mid_teardown_gets_a_fresh_running_server() {
    harnx_core::require_nextest();
    let (launcher, entered_stop, release_stop) = GatedLauncher::new();
    let reconciler = Arc::new(ServerReconciler::new(launcher.clone(), Duration::ZERO));

    reconciler
        .session_started("s1", vec![tool_server("time")])
        .await;

    // `linger` is zero, so dropping s1's only use of "time" sweeps
    // immediately and calls `stop`, which the fake now blocks inside until
    // `release_stop` is sent below. Run it on its own task since it won't
    // return until then.
    let ending = tokio::spawn({
        let reconciler = reconciler.clone();
        async move { reconciler.session_ended("s1").await }
    });

    entered_stop
        .await
        .expect("session_ended's sweep should have called stop on 'time'");

    // s2 asks for the same name while that teardown is still in flight.
    let joining = tokio::spawn({
        let reconciler = reconciler.clone();
        async move {
            reconciler
                .session_started("s2", vec![tool_server("time")])
                .await;
        }
    });

    release_stop
        .send(())
        .expect("stop is still parked on this gate waiting to be released");

    ending.await.expect("session_ended task panicked");
    joining.await.expect("session_started task panicked");

    assert_eq!(
        reconciler.running().await,
        vec!["time"],
        "a session that raced a teardown for the same name must end up with \
         a running, registered server, not silently none"
    );
    assert_eq!(
        launcher.started.lock().unwrap().as_slice(),
        ["time", "time"],
        "the racing session must trigger its own fresh start once the old \
         process is actually gone, not just join an entry that sweep is \
         about to delete out from under it"
    );
    assert_eq!(launcher.stopped.lock().unwrap().as_slice(), ["time"]);
}

/// A launcher whose `start` takes a fixed amount of time, for pinning
/// `session_started`'s concurrency: several of these in one call should take
/// about as long as the slowest one, not their sum.
#[derive(Default)]
struct SlowLauncher {
    delay: Duration,
    started: Mutex<Vec<String>>,
}

#[async_trait]
impl ServerLauncher for SlowLauncher {
    async fn start(&self, server: &ToolServerConfig) -> anyhow::Result<()> {
        tokio::time::sleep(self.delay).await;
        self.started.lock().unwrap().push(server.name.clone());
        Ok(())
    }
    async fn stop(&self, _config_name: &str) {}
}

#[tokio::test]
async fn session_started_starts_multiple_servers_concurrently_not_sequentially() {
    harnx_core::require_nextest();
    let launcher = Arc::new(SlowLauncher {
        delay: Duration::from_millis(200),
        started: Mutex::new(Vec::new()),
    });
    let reconciler = ServerReconciler::new(launcher.clone(), Duration::ZERO);

    let began = Instant::now();
    reconciler
        .session_started(
            "s1",
            vec![
                tool_server("time"),
                tool_server("plans"),
                tool_server("weather"),
            ],
        )
        .await;
    let elapsed = began.elapsed();

    assert_eq!(launcher.started.lock().unwrap().len(), 3);
    assert!(
        elapsed < Duration::from_millis(450),
        "three 200ms server starts should overlap (~200ms total) rather than sum \
         sequentially (~600ms); the activation ack window can't afford the sum once \
         more than a couple of servers are configured. took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// End-to-end: a real managing worker starts only the servers its active
// sessions' agents use. Built on the pattern in `package_tool_naming_e2e.rs`.
// ---------------------------------------------------------------------------

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

/// Poll the registry with a deadline until `config_name` appears. Server
/// startup time varies, so this must never be a fixed sleep.
async fn await_registered(client: &async_nats::Client, config_name: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if registered_config_names(client)
            .await
            .iter()
            .any(|name| name == config_name)
        {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "'{config_name}' did not register within the deadline"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if !registered_config_names(client)
            .await
            .iter()
            .any(|name| name == config_name)
        {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "'{config_name}' registration was not removed within the deadline"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
