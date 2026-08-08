#[allow(dead_code)]
mod common;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_core::abort::create_abort_signal;
use harnx_core::event::{AgentEvent, AgentEventSink, NoticeEvent};
use harnx_core::instance::ServerScope;
use harnx_core::tool::{ToolCall, ToolError, ToolProvider};
use harnx_runtime::config::{Config, ToolServerConfig, HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV};
use harnx_runtime::nats_tool_provider::{NatsInFlightCalls, NatsToolProvider};
use harnx_runtime::nats_worker::{ToolServerStartConfig, ToolServerSupervisor};
use harnx_toolset::{ControlKind, ControlMessage};
// Only the Linux-gated #1350 identity/cleanup test uses these.
#[cfg(target_os = "linux")]
use harnx_toolset::{server_identity_token, Registration};
use harnx_toolset_server::{registration_key, TOOL_REGISTRY_BUCKET};
use parking_lot::RwLock;
use serde_json::json;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn time_server_config(command: impl Into<String>) -> ToolServerConfig {
    ToolServerConfig {
        name: "time".to_string(),
        command: command.into(),
        args: Vec::new(),
        env: Default::default(),
        enabled: true,
        description: None,
        package: None,
        hooks: None,
    }
}
const TOKEN: &str = "tool-supervisor-test-token";

#[derive(Default)]
struct RecordingEventSink {
    events: std::sync::Mutex<Vec<AgentEvent>>,
}

impl RecordingEventSink {
    fn warning_messages(&self) -> Vec<String> {
        self.events
            .lock()
            .expect("event sink lock")
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Notice(NoticeEvent::Warning(message)) => Some(message.clone()),
                _ => None,
            })
            .collect()
    }
}

impl AgentEventSink for RecordingEventSink {
    fn emit(&self, event: AgentEvent) {
        self.events.lock().expect("event sink lock").push(event);
    }
}

struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    fn install(values: &[(&'static str, &str)]) -> Self {
        let old = values
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in values {
            unsafe { std::env::set_var(name, value) };
        }
        Self(old)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..) {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

fn time_server_binary() -> Result<PathBuf> {
    if let Some(path) = option_env!("CARGO_BIN_EXE_harnx-time-server") {
        return Ok(PathBuf::from(path));
    }
    let mut path = std::env::current_exe().context("resolve test executable")?;
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    path.push(if cfg!(windows) {
        "harnx-time-server.exe"
    } else {
        "harnx-time-server"
    });
    if path.is_file() {
        return Ok(path);
    }

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .context("resolve workspace root")?
        .to_path_buf();
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .arg("build")
        .arg("-p")
        .arg("harnx-time-server")
        .current_dir(workspace)
        .status()
        .context("build harnx-time-server for supervisor test")?;
    anyhow::ensure!(status.success(), "building harnx-time-server failed");
    anyhow::ensure!(
        path.is_file(),
        "harnx-time-server not found at {}",
        path.display()
    );
    Ok(path)
}

async fn wait_until_registration_removed(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<()> {
    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?;
    let key = registration_key(instance_id, "__time__time");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if store.get(&key).await?.is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("tool registration was not removed after child exit");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn assert_pilot_registration(
    supervisor: &ToolServerSupervisor,
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<u32> {
    let pids = supervisor.server_pids().await;
    let (&pid, server_name) = pids.iter().next().context("time server PID")?;
    assert_eq!(server_name, "time");
    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?;
    assert!(store
        .get(registration_key(instance_id, "__time__time"))
        .await?
        .is_some());
    Ok(pid)
}

async fn assert_nats_eval_batch(instance_id: &ServerScope) -> Result<()> {
    let config = Arc::new(RwLock::new(Config::default()));
    let context = harnx_runtime::tool::build_tool_eval_context(
        harnx_runtime::tool::BuildToolEvalContextParams::new(&config, instance_id)
            .with_agent_use_tools(Some("*")),
    )
    .await;
    assert_eq!(context.providers[0].name(), "nats");
    let results = harnx_runtime::tool::eval_tool_calls(
        &context,
        vec![ToolCall::new(
            "time_get_current_time".to_string(),
            json!({ "timezone": "UTC" }),
            Some("nats-time".to_string()),
            None,
        )],
        &create_abort_signal(),
    )
    .await?;
    let result = results
        .iter()
        .find(|result| result.call.id.as_deref() == Some("nats-time"))
        .context("NATS time result in worker transcript")?;
    assert_eq!(result.output["timezone"], "UTC");
    assert!(result.output["datetime"].as_str().is_some());
    Ok(())
}

async fn assert_cancel_control(
    provider: &NatsToolProvider,
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<()> {
    let mut controls = client.subscribe(instance_id.control_subject()).await?;
    client.flush().await?;
    let abort = create_abort_signal();
    let abort_task = {
        let abort = abort.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            abort.set_ctrlc();
        })
    };
    let started = Instant::now();
    let cancelled = provider
        .call_tool("time_wait", json!({ "seconds": 30.0 }), &abort)
        .await;
    abort_task.await?;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(matches!(cancelled, Err(ToolError::Fatal(_))));
    let control = tokio::time::timeout(Duration::from_secs(1), controls.next())
        .await
        .context("cancel control message timeout")?
        .context("cancel control subscription closed")?;
    let control: ControlMessage = serde_json::from_slice(&control.payload)?;
    assert_eq!(control.kind, ControlKind::Cancel);
    Ok(())
}

async fn assert_crash_failure(
    provider: &NatsToolProvider,
    pid: u32,
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<()> {
    let kill_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        #[cfg(unix)]
        // SAFETY: PID came from the child owned by this test's supervisor.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .status();
        }
    });
    let started = Instant::now();
    let result = provider
        .call_tool(
            "time_wait",
            json!({ "seconds": 30.0 }),
            &create_abort_signal(),
        )
        .await;
    kill_task.await?;
    assert!(started.elapsed() < Duration::from_secs(2));
    match result {
        Err(ToolError::Recoverable(error)) => {
            assert!(error
                .to_string()
                .contains("tool server 'time' crashed, exit"));
        }
        _ => anyhow::bail!("crashed server should fail call with a recoverable error"),
    }
    wait_until_registration_removed(client, instance_id).await?;
    let snapshot = NatsToolProvider::discover(
        &Config::default(),
        instance_id.clone(),
        NatsInFlightCalls::for_instance(instance_id),
        None,
    )
    .await?;
    assert!(!snapshot.has_tool("time_wait"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn time_over_nats_pilot_e2e_eval_cancel_and_crash() -> Result<()> {
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let binary = time_server_binary()?.to_string_lossy().into_owned();
    let _env = EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, server.url()),
        (HARNX_NATS_TOKEN_ENV, TOKEN),
    ]);
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    let instance_id = ServerScope::new();
    let start =
        ToolServerStartConfig::new(client.clone(), instance_id.clone(), server.url(), TOKEN);
    let servers = [time_server_config(&binary)];
    let supervisor =
        ToolServerSupervisor::start_local_with_timeout(start, &servers, Duration::from_secs(5))
            .await?;
    let pid = assert_pilot_registration(&supervisor, &client, &instance_id).await?;
    let provider = NatsToolProvider::discover(
        &Config::default(),
        instance_id.clone(),
        NatsInFlightCalls::for_instance(&instance_id),
        None,
    )
    .await?;

    assert_nats_eval_batch(&instance_id).await?;
    assert_cancel_control(&provider, &client, &instance_id).await?;
    assert_crash_failure(&provider, pid, &client, &instance_id).await
}

#[cfg(unix)]
struct ReadinessTestFixture {
    _server: common::NatsServerHandle,
    _directory: tempfile::TempDir,
    client: async_nats::Client,
    instance_id: ServerScope,
    sink: Arc<RecordingEventSink>,
    supervisor: ToolServerSupervisor,
}

#[cfg(unix)]
async fn start_readiness_test(
    fake_binary_name: &str,
    fake_server_name: &str,
    healthy_binary: Option<String>,
    timeout: Duration,
) -> Result<Option<ReadinessTestFixture>> {
    use std::os::unix::fs::PermissionsExt;

    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(None);
    };
    let directory = tempfile::tempdir()?;
    let fake_binary = directory.path().join(fake_binary_name);
    std::fs::write(&fake_binary, "#!/bin/sh\nsleep 10\n")?;
    std::fs::set_permissions(&fake_binary, std::fs::Permissions::from_mode(0o755))?;
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    let instance_id = ServerScope::new();
    let start =
        ToolServerStartConfig::new(client.clone(), instance_id.clone(), server.url(), TOKEN);
    let mut fake_server = time_server_config(fake_binary.to_string_lossy().into_owned());
    fake_server.name = fake_server_name.to_string();
    let mut servers = vec![fake_server];
    if let Some(healthy_binary) = healthy_binary {
        servers.push(time_server_config(healthy_binary));
    }
    let sink = Arc::new(RecordingEventSink::default());
    let supervisor = harnx_core::sink::with_agent_event_sink(sink.clone(), async {
        ToolServerSupervisor::start_local_with_timeout(start, &servers, timeout).await
    })
    .await?;

    Ok(Some(ReadinessTestFixture {
        _server: server,
        _directory: directory,
        client,
        instance_id,
        sink,
        supervisor,
    }))
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn tool_server_readiness_failure_warns_and_continues() -> Result<()> {
    let Some(fixture) =
        start_readiness_test("never-registers", "time", None, Duration::from_millis(250)).await?
    else {
        return Ok(());
    };

    let store = async_nats::jetstream::new(fixture.client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?;
    assert!(
        store
            .get(registration_key(&fixture.instance_id, "__time__time"))
            .await?
            .is_none(),
        "non-registering server must not become available"
    );
    assert!(
        fixture.sink.warning_messages().iter().any(|message| {
            message.contains("tool server 'time'") && message.contains("has not registered")
        }),
        "readiness failure must emit a warning: {:?}",
        fixture.sink.warning_messages()
    );
    drop(fixture.supervisor);
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn readiness_waits_for_servers_concurrently() -> Result<()> {
    let time_binary = time_server_binary()?.to_string_lossy().into_owned();
    let Some(fixture) = start_readiness_test(
        "stall-server",
        "stall",
        Some(time_binary),
        Duration::from_secs(1),
    )
    .await?
    else {
        return Ok(());
    };

    let store = async_nats::jetstream::new(fixture.client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?;
    assert!(
        store
            .get(registration_key(&fixture.instance_id, "__time__time"))
            .await?
            .is_some(),
        "healthy server must register while another server stalls"
    );
    let warnings = fixture.sink.warning_messages();
    assert!(warnings.iter().any(|message| message.contains("'stall'")));
    assert!(
        warnings.iter().all(|message| !message.contains("'time'")),
        "healthy server must not receive a readiness warning: {warnings:?}"
    );
    drop(fixture.supervisor);
    Ok(())
}

#[cfg(target_os = "linux")]
fn packaged_time_servers(binary: &str) -> [ToolServerConfig; 2] {
    let mut alpha = time_server_config(binary.to_string());
    alpha.package = Some("alpha".to_string());
    let mut beta = time_server_config(binary.to_string());
    beta.package = Some("beta".to_string());
    [alpha, beta]
}

#[cfg(target_os = "linux")]
async fn assert_packaged_registrations(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<(async_nats::jetstream::kv::Store, String, String)> {
    let alpha_key = registration_key(
        instance_id,
        &server_identity_token(Some("alpha"), "time", "time"),
    );
    let beta_key = registration_key(
        instance_id,
        &server_identity_token(Some("beta"), "time", "time"),
    );
    let store = async_nats::jetstream::new(client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?;
    for (key, package) in [(&alpha_key, "alpha"), (&beta_key, "beta")] {
        let registration: Registration = serde_json::from_slice(
            &store
                .get(key)
                .await?
                .with_context(|| format!("{package} registration after supervisor readiness"))?,
        )?;
        assert_eq!(registration.package.as_deref(), Some(package));
        assert_eq!(registration.config, "time");
    }
    Ok((store, alpha_key, beta_key))
}

#[cfg(target_os = "linux")]
async fn assert_distinct_time_routes(instance_id: &ServerScope) -> Result<()> {
    let provider = NatsToolProvider::discover(
        &Config::default(),
        instance_id.clone(),
        NatsInFlightCalls::for_instance(instance_id),
        Some("alpha"),
    )
    .await?;
    for tool in ["time_get_current_time", "beta__time_get_current_time"] {
        assert!(provider.has_tool(tool), "missing distinct route {tool}");
        let result = provider
            .call_tool(tool, json!({ "timezone": "UTC" }), &create_abort_signal())
            .await
            .map_err(|error| match error {
                ToolError::Recoverable(error) | ToolError::Fatal(error) => error,
            })?;
        assert_eq!(result["timezone"], "UTC");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn kill_alpha_and_assert_cleanup(
    supervisor: &ToolServerSupervisor,
    store: &async_nats::jetstream::kv::Store,
    alpha_key: &str,
    beta_key: &str,
) -> Result<()> {
    let alpha_pid = supervisor
        .server_pids()
        .await
        .into_keys()
        .find(|pid| {
            std::fs::read(format!("/proc/{pid}/environ"))
                .map(|env| {
                    env.split(|byte| *byte == 0)
                        .any(|entry| entry == b"HARNX_SERVER_PACKAGE=alpha")
                })
                .unwrap_or(false)
        })
        .context("alpha tool server PID")?;
    // SAFETY: PID belongs to child owned by this test's supervisor.
    unsafe {
        libc::kill(alpha_pid as libc::pid_t, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while store.get(alpha_key).await?.is_some() {
        anyhow::ensure!(
            Instant::now() < deadline,
            "alpha registration remained after its child exited"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        store.get(beta_key).await?.is_some(),
        "crash cleanup removed the other package's registration"
    );
    Ok(())
}
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn same_named_packaged_servers_use_distinct_identities_and_cleanup() -> Result<()> {
    let Some(nats) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let binary = time_server_binary()?.to_string_lossy().into_owned();
    let instance_id = ServerScope::new();
    let _env = EnvGuard::install(&[
        (HARNX_NATS_URL_ENV, nats.url()),
        (HARNX_NATS_TOKEN_ENV, TOKEN),
    ]);
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(nats.url())
        .await?;
    let supervisor = ToolServerSupervisor::start_local_with_timeout(
        ToolServerStartConfig::new(client.clone(), instance_id.clone(), nats.url(), TOKEN),
        &packaged_time_servers(&binary),
        Duration::from_secs(5),
    )
    .await?;

    let (store, alpha_key, beta_key) = assert_packaged_registrations(&client, &instance_id).await?;
    assert_distinct_time_routes(&instance_id).await?;
    kill_alpha_and_assert_cleanup(&supervisor, &store, &alpha_key, &beta_key).await?;

    drop(supervisor);
    Ok(())
}
#[tokio::test(flavor = "multi_thread")]
async fn missing_tool_server_binary_warns_and_continues() -> Result<()> {
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(TOKEN.to_string()),
    })
    .await?
    else {
        return Ok(());
    };
    let missing_dir = tempfile::tempdir()?;
    let missing = missing_dir.path().join("does-not-exist");
    let missing = missing.to_string_lossy().into_owned();
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(server.url())
        .await?;
    let instance_id = ServerScope::new();
    let start = ToolServerStartConfig::new(client, instance_id, server.url(), TOKEN);
    let servers = [time_server_config(missing)];
    let sink = Arc::new(RecordingEventSink::default());
    let supervisor = harnx_core::sink::with_agent_event_sink(sink.clone(), async {
        ToolServerSupervisor::start_local_with_timeout(start, &servers, Duration::from_millis(250))
            .await
    })
    .await?;

    assert!(supervisor.server_pids().await.is_empty());
    assert!(
        sink.warning_messages().iter().any(|message| {
            message.contains("tool server 'time'") && message.contains("not found next to worker")
        }),
        "missing binary must emit a warning: {:?}",
        sink.warning_messages()
    );
    Ok(())
}
