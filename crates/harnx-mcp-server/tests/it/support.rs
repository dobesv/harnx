use anyhow::{Context, Result};
use harnx_core::{abort::create_abort_signal, tool::ToolProvider};
use harnx_mcp_server::bootstrap::Bootstrap;
use harnx_runtime::{
    config::Config,
    nats_tool_provider::{NatsInFlightCalls, NatsToolProvider},
};
pub use harnx_runtime::{
    nats_worker::{run_worker_daemon, tool_reservation::*, WorkerDaemonConfig},
    tool_reservation_client::ToolReservationHandle,
};
pub use serde_json::json;
pub use std::time::Duration;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};
use tempfile::TempDir;
use tokio_util::task::AbortOnDropHandle;

pub const DEADLINE: Duration = Duration::from_secs(120);

pub struct EnvGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    pub fn set(name: &'static str, value: Option<&std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(name);
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
        Self { name, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

pub struct Fixture {
    pub root: TempDir,
    pub _env: Vec<EnvGuard>,
}

impl Fixture {
    pub fn new() -> Result<Self> {
        harnx_core::require_nextest();
        let root = tempfile::tempdir()?;
        let mut env = Vec::new();
        for (name, dir) in [
            ("HARNX_CONFIG_DIR", "config"),
            ("HARNX_DATA_DIR", "data"),
            ("HARNX_STATE_DIR", "state"),
        ] {
            let path = root.path().join(dir);
            std::fs::create_dir_all(&path)?;
            env.push(EnvGuard::set(name, Some(path.as_os_str())));
        }
        for name in [
            "HARNX_CONFIG_FILE",
            "HARNX_NATS_SERVER",
            "HARNX_NATS_URL",
            "HARNX_NATS_TOKEN",
            "HARNX_NATS_TLS",
            "HARNX_NATS_TLS_CERT",
            "HARNX_NATS_TLS_KEY",
            "HARNX_NATS_TLS_CA",
            "HARNX_NATS_IGNORE_DISCOVERED_SERVERS",
            "HARNX_NATS_REPLICAS",
            "HARNX_SERVER_SCOPE",
        ] {
            env.push(EnvGuard::set(name, None));
        }
        std::fs::write(
            root.path().join("config/config.yaml"),
            "save: false\nstream: false\n",
        )?;
        Ok(Self { root, _env: env })
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.path().join("config")
    }

    pub fn cluster(&self, name: &str, url: &str) -> Result<()> {
        let dir = self.config_dir().join("nats_servers");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{name}.yaml")), format!("url: {url:?}\n"))?;
        Ok(())
    }

    pub async fn bootstrap(&self, cluster: Option<&str>) -> Result<Bootstrap> {
        Bootstrap::new(
            cluster.map(str::to_owned),
            Some(self.config_dir()),
            create_abort_signal(),
        )
        .await
    }
}

pub fn binary(name: &str) -> Result<PathBuf> {
    let path = std::env::current_exe()?
        .parent()
        .context("test binary directory")?
        .parent()
        .context("target directory")?
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    anyhow::ensure!(
        path.is_file(),
        "build workspace first: {} missing",
        path.display()
    );
    Ok(path)
}

pub fn view(selector: &str) -> ToolReservationView {
    ToolReservationView {
        package: None,
        use_tools: vec![selector.to_owned()],
    }
}

pub async fn discover(handle: &ToolReservationHandle, tool: &str) -> Result<NatsToolProvider> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let scope = handle
                .server_scope()
                .context("reservation scope unavailable")?;
            let provider = NatsToolProvider::discover(
                handle.config(),
                scope.clone(),
                NatsInFlightCalls::for_instance(&scope),
                None,
            )
            .await?;
            if provider
                .declarations()
                .iter()
                .any(|declaration| declaration.name == tool)
            {
                return Ok(provider);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("tool did not register before deadline")?
}

pub struct ReadToolAssertion<'a> {
    pub tool: &'a str,
    pub path: &'a Path,
    pub marker: &'a str,
}

pub async fn assert_read(
    provider: &NatsToolProvider,
    assertion: ReadToolAssertion<'_>,
) -> Result<()> {
    let result = provider
        .call_tool(
            assertion.tool,
            json!({"path": assertion.path}),
            &create_abort_signal(),
        )
        .await
        .map_err(|error| match error {
            harnx_core::tool::ToolError::Recoverable(error)
            | harnx_core::tool::ToolError::Fatal(error) => error,
        })?;
    assert!(!result.to_string().contains("\"isError\":true"), "{result}");
    assert!(result.to_string().contains(assertion.marker), "{result}");
    Ok(())
}

pub struct WorkerOptions<'a> {
    pub config: Config,
    pub cluster: &'a str,
}

pub async fn start_worker(options: WorkerOptions<'_>) -> Result<AbortOnDropHandle<Result<()>>> {
    let readiness = harnx_healthz::Readiness::default();
    let daemon = WorkerDaemonConfig::new(options.cluster, format!("worker-{}", options.cluster));
    let mut worker = AbortOnDropHandle::new(tokio::spawn({
        let readiness = readiness.clone();
        async move {
            run_worker_daemon(
                Arc::new(parking_lot::RwLock::new(options.config)),
                daemon,
                None,
                Some(readiness),
            )
            .await
        }
    }));
    tokio::select! {
        stopped = &mut worker => anyhow::bail!("worker stopped at startup: {stopped:?}"),
        result = tokio::time::timeout(DEADLINE, async {
            while !readiness.is_ready() { tokio::time::sleep(Duration::from_millis(20)).await; }
        }) => result.context("worker startup deadline")?,
    }
    Ok(worker)
}

pub struct FsServerOptions<'a> {
    pub manager: &'a harnx_core::child_process::ChildProcessManager,
    pub url: &'a str,
    pub name: &'a str,
    pub root: &'a Path,
}

pub async fn start_fs(options: FsServerOptions<'_>) -> Result<tokio::process::Child> {
    let mut command = tokio::process::Command::new(binary("harnx-fs-tools")?);
    command
        .args(["--name", options.name, "--allow-read"])
        .arg(options.root)
        .env("HARNX_NATS_URL", options.url)
        .env("HARNX_NATS_TOKEN", "")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    Ok(options.manager.spawn(command).await?)
}
