//! Real worker + isolated JetStream + scripted OpenAI-compatible streaming LLM.
//! Follows MCP's Readiness harness; barriers replace timing guesses in turns.
use anyhow::{bail, Context, Result};
use axum::{routing::post, Router};
use harnx_a2a_server::runner::{SessionRequest, TurnRequest};
use harnx_a2a_server::{
    exports::{AgentCardMeta, Export},
    identity::Principal,
    input_map::InputLimits,
    runner::{Runner, StartTurnResult},
    store::{A2aStore, TaskRecord},
};
use harnx_runtime::{
    config::{Config, ConfigLock, GlobalConfig},
    nats_session_metadata::SessionMetadataStore,
    nats_worker::{run_worker_daemon, WorkerDaemonConfig},
    NatsSession, SessionActivationRoute,
};
use serde_json::Value;
use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Notify;
use tokio_util::task::AbortOnDropHandle;

#[path = "support/completion.rs"]
mod completion;
use completion::completion;

pub const DEADLINE: Duration = Duration::from_secs(120);
pub fn alice() -> Principal {
    Principal::User("alice".into())
}

struct EnvGuard {
    name: &'static str,
    previous: Option<OsString>,
}
impl EnvGuard {
    fn set(name: &'static str, value: Option<OsString>) -> Self {
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

pub(super) struct Broker {
    child: Child,
    _dir: tempfile::TempDir,
}
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Broker {
    #[cfg(all(unix, feature = "fault-injection"))]
    pub(super) fn signal(&self, signal: &str) -> Result<()> {
        anyhow::ensure!(
            Command::new("kill")
                .args([signal, &self.child.id().to_string()])
                .status()?
                .success(),
            "broker signal failed"
        );
        Ok(())
    }
    #[cfg(all(unix, feature = "fault-injection"))]
    fn respawn_on_port(&mut self, port: &str) -> Result<()> {
        self.child.kill()?;
        self.child.wait()?;
        self.child = Command::new(
            std::env::var_os("NATS_SERVER_BIN").unwrap_or_else(|| "nats-server".into()),
        )
        .args(["-js", "-a", "127.0.0.1", "-p", port, "-sd"])
        .arg(self._dir.path().join("data"))
        .arg("--ports_file_dir")
        .arg(self._dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
        Ok(())
    }

    #[cfg(all(unix, feature = "fault-injection"))]
    pub(super) async fn restart(&mut self) -> Result<()> {
        let (url, _) = self.wait_for_client().await?;
        let port = url.rsplit(':').next().context("broker port")?;
        self.respawn_on_port(port)?;
        tokio::time::timeout(DEADLINE, self.wait_for_client()).await??;
        Ok(())
    }
    pub(super) async fn start() -> Result<(Self, String, async_nats::Client)> {
        let mut broker = Self::spawn()?;
        let (url, client) = tokio::time::timeout(DEADLINE, broker.wait_for_client())
            .await
            .context("broker readiness deadline")??;
        Ok((broker, url, client))
    }

    pub(super) async fn start_with_config(
        config: &str,
    ) -> Result<(Self, String, async_nats::Client)> {
        let mut broker = Self::spawn_config(Some(config))?;
        let (url, client) = tokio::time::timeout(DEADLINE, broker.wait_for_client())
            .await
            .context("broker readiness deadline")??;
        Ok((broker, url, client))
    }

    fn spawn() -> Result<Self> {
        Self::spawn_config(None)
    }

    fn spawn_config(config: Option<&str>) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let mut command = Command::new(
            std::env::var_os("NATS_SERVER_BIN").unwrap_or_else(|| "nats-server".into()),
        );
        if let Some(config) = config {
            let path = dir.path().join("nats.conf");
            std::fs::write(&path, config)?;
            command.arg("--config").arg(path);
        }
        let child = command
            .args(["-js", "-a", "127.0.0.1", "-p", "-1", "-sd"])
            .arg(dir.path().join("data"))
            .arg("--ports_file_dir")
            .arg(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("required test broker")?;
        Ok(Self { child, _dir: dir })
    }

    async fn wait_for_client(&mut self) -> Result<(String, async_nats::Client)> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!("broker stopped: {status}");
            }
            if let Some(pair) = try_connect_broker_ports(self._dir.path()).await? {
                return Ok(pair);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn try_connect_broker_ports(
    dir: &std::path::Path,
) -> Result<Option<(String, async_nats::Client)>> {
    for entry in std::fs::read_dir(dir)? {
        let Some(url) = broker_ports_url(&entry?.path())? else {
            continue;
        };
        if let Ok(client) = async_nats::connect(&url).await {
            return Ok(Some((url, client)));
        }
    }
    Ok(None)
}
fn broker_ports_url(path: &std::path::Path) -> Result<Option<String>> {
    if !path.extension().is_some_and(|e| e == "ports") {
        return Ok(None);
    }
    let Ok(value) = serde_json::from_slice::<Value>(&std::fs::read(path)?) else {
        return Ok(None);
    };
    Ok(value["nats"][0].as_str().map(str::to_owned))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Script {
    Text,
    Tool,
    SerialTool,
    SameRoundTools,
    CountedTool,
    Fail,
    Many,
    Large,
    Boundary,
    Overflow,
}

pub struct Llm {
    pub requests: parking_lot::Mutex<Vec<Value>>,
    pub release: Notify,
    pub requested: Notify,
    script: Script,
}

#[derive(Clone, Default)]
pub struct Logs(Arc<parking_lot::Mutex<Vec<u8>>>);
impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Logs {
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().clone()).unwrap()
    }
}

fn test_logs() -> Logs {
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .try_init()
        .expect("test tracing subscriber");
    logs
}

async fn start_llm(
    script: Script,
) -> Result<(Arc<Llm>, std::net::SocketAddr, AbortOnDropHandle<()>)> {
    let llm = Arc::new(Llm {
        script,
        requests: Default::default(),
        release: Notify::new(),
        requested: Notify::new(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let app = Router::new()
        .route("/v1/chat/completions", post(completion))
        .with_state(llm.clone());
    let http = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock LLM server");
    }));
    Ok((llm, address, http))
}

fn isolated_environment(root: &tempfile::TempDir, url: &str) -> Result<Vec<EnvGuard>> {
    let mut env = Vec::new();
    for (name, subdir) in [
        ("HARNX_CONFIG_DIR", "config"),
        ("HARNX_DATA_DIR", "data"),
        ("HARNX_STATE_DIR", "state"),
    ] {
        let path = root.path().join(subdir);
        std::fs::create_dir_all(&path)?;
        env.push(EnvGuard::set(name, Some(path.into_os_string())));
    }
    for name in [
        "HARNX_CONFIG_FILE",
        "HARNX_NATS_SERVER",
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
    // Agent-scoped hook setup also resolves the worker's default endpoint.
    // Keep that endpoint on this isolated broker, as in runtime HITL tests.
    env.push(EnvGuard::set("HARNX_NATS_URL", Some(url.into())));
    env.push(EnvGuard::set("HARNX_NATS_TOKEN", Some("".into())));
    Ok(env)
}

fn init_fixture_dirs(dir: &std::path::Path) -> Result<()> {
    for subdir in ["agents", "clients", "nats_servers"] {
        std::fs::create_dir_all(dir.join(subdir))?;
    }
    Ok(())
}

fn write_counted_tool_config(dir: &std::path::Path) -> Result<()> {
    let command = binary("harnx-bash-tools")?;
    std::fs::create_dir_all(dir.join("tool_servers"))?;
    std::fs::write(
        dir.join("tool_servers/bash.yaml"),
        format!(
            "command: {:?}\nargs: [--name, bash, --no-sandbox, --allow-write, {:?}]\n",
            command.to_str().context("bash tool path")?,
            dir.to_str().context("config path")?
        ),
    )?;
    Ok(())
}

fn write_fixture_support_configs(
    dir: &std::path::Path,
    script: Script,
    url: &str,
    address: std::net::SocketAddr,
) -> Result<()> {
    init_fixture_dirs(dir)?;
    let mut config_yaml = "model: mock:test\nstream: true\nsave: false\n".to_owned();
    if matches!(script, Script::Large | Script::Boundary | Script::Overflow) {
        config_yaml.push_str("loop_detection:\n  output: false\n");
    }
    if script == Script::CountedTool {
        write_counted_tool_config(dir)?;
    }
    std::fs::write(dir.join("config.yaml"), config_yaml)?;
    std::fs::write(
        dir.join("nats_servers/runner.yaml"),
        format!("url: {url:?}\n"),
    )?;
    std::fs::write(dir.join("clients/mock.yaml"), format!("type: openai-compatible\nname: mock\napi_base: http://{address}/v1\napi_key: test-key\nmodels:\n  - name: test\n    max_input_tokens: 32000\n    max_output_tokens: 1024\n"))?;
    Ok(())
}

fn write_same_round_tools_agent(dir: &std::path::Path) -> Result<String> {
    std::fs::create_dir_all(dir.join("tool_servers"))?;
    std::fs::write(
        dir.join("tool_servers/time.yaml"),
        format!(
            "command: {:?}\n",
            binary("harnx-time-tools")?
                .to_str()
                .context("time tool path")?
        ),
    )?;
    let hook = binary("harnx-claude-compatible-hook-server")?;
    let command = shell_words::join([hook.to_str().context("hook path encoding")?, "--event", "PreToolUse", "--matcher", "^time_get_current_time$", "--jaq", "{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"Approve time lookup?\"}}"]);
    Ok(format!("---\nmodel: mock:test\nuse_tools: [time_get_current_time]\nhooks:\n  entries:\n    - command: {command:?}\n---\nTest same-round approvals\n"))
}

fn write_tool_handoff_agent() -> Result<String> {
    let binary = binary("harnx-claude-compatible-hook-server")?;
    let command = shell_words::join([binary.to_str().context("hook path encoding")?, "--event", "PreToolUse", "--matcher", "^target_session_handoff$", "--jaq", "{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"Approve handoff?\"}}"]);
    Ok(format!("---\nmodel: mock:test\nuse_tools:\n- target_session_handoff\nhooks:\n  entries:\n    - command: {command:?}\n---\nTest runner agent\n"))
}

fn fixture_agent_content(script: Script, dir: &std::path::Path) -> Result<String> {
    if script == Script::CountedTool {
        return Ok(
            "---\nmodel: mock:test\nuse_tools: [bash_exec]\n---\nTest counted tool agent\n".into(),
        );
    }
    if script == Script::SameRoundTools {
        return write_same_round_tools_agent(dir);
    }
    if matches!(script, Script::Tool | Script::SerialTool) {
        return write_tool_handoff_agent();
    }
    Ok("---\nmodel: mock:test\n---\nTest runner agent\n".into())
}

fn fixture_config(
    dir: &std::path::Path,
    script: Script,
    url: &str,
    address: std::net::SocketAddr,
) -> Result<GlobalConfig> {
    write_fixture_support_configs(dir, script, url, address)?;
    let agent = fixture_agent_content(script, dir)?;
    std::fs::write(dir.join("agents/runner.md"), agent)?;
    std::fs::write(
        dir.join("agents/target.md"),
        "---\nmodel: mock:test\n---\nHandoff target\n",
    )?;
    let config = Arc::new(ConfigLock::new(Config::load_from_file(
        &dir.join("config.yaml"),
    )?));
    Ok(config)
}

async fn start_worker(config: &GlobalConfig) -> Result<AbortOnDropHandle<Result<()>>> {
    let readiness = harnx_healthz::Readiness::default();
    let mut worker = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
        config.clone(),
        WorkerDaemonConfig::managing("runner", "a2a-test-worker"),
        None,
        Some(readiness.clone()),
    )));
    tokio::select! {
        result = &mut worker => bail!("worker startup failed: {result:?}"),
        result = tokio::time::timeout(DEADLINE, async {
            while !readiness.is_ready() { tokio::time::sleep(Duration::from_millis(20)).await; }
        }) => result.context("worker readiness deadline")?,
    }
    Ok(worker)
}

fn runner_export() -> Export {
    Export {
        public_name: "runner".into(),
        agent: "runner".into(),
        cluster: Some("runner".into()),
        card_meta: AgentCardMeta {
            name: "runner".into(),
            description: String::new(),
            version: "1".into(),
            conversation_starters: vec![],
        },
        lookup_keys: vec![],
    }
}

pub struct Harness {
    pub logs: Logs,
    pub runner: Arc<Runner>,
    pub store: Arc<A2aStore>,
    pub metadata: SessionMetadataStore,
    pub config: GlobalConfig,
    pub export: Export,
    pub llm: Arc<Llm>,
    pub jetstream: async_nats::jetstream::Context,
    worker: AbortOnDropHandle<Result<()>>,
    http: AbortOnDropHandle<()>,
    pub(super) _broker: Broker,
    _root: tempfile::TempDir,
    _env: Vec<EnvGuard>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.worker.abort();
        self.http.abort();
    }
}
struct BrokerLlmBootstrap {
    broker: Broker,
    url: String,
    client: async_nats::Client,
    llm: Arc<Llm>,
    address: std::net::SocketAddr,
    http: AbortOnDropHandle<()>,
}

async fn bootstrap_broker_and_llm(
    script: Script,
    broker_config: Option<&str>,
) -> Result<BrokerLlmBootstrap> {
    let (broker, url, client) = match broker_config {
        Some(config) => Broker::start_with_config(config).await?,
        None => Broker::start().await?,
    };
    let (llm, address, http) = start_llm(script).await?;
    Ok(BrokerLlmBootstrap {
        broker,
        url,
        client,
        llm,
        address,
        http,
    })
}

struct DefaultBrokerSetup {
    config: GlobalConfig,
    client: async_nats::Client,
}

async fn configure_broker_client(
    root: &std::path::Path,
    url: &str,
    broker_config: Option<&str>,
    defaults: DefaultBrokerSetup,
) -> Result<(GlobalConfig, async_nats::Client)> {
    if broker_config.is_none() {
        return Ok((defaults.config, defaults.client));
    }
    let endpoint = url.replacen(
        "nats://",
        "nats://a2a_backend:a2a_backend_test_password@",
        1,
    );
    std::fs::write(
        root.join("config/nats_servers/runner.yaml"),
        format!("url: {endpoint:?}\n"),
    )?;
    let config = Arc::new(ConfigLock::new(Config::load_from_file(
        &root.join("config/config.yaml"),
    )?));
    let client = async_nats::ConnectOptions::new()
        .user_and_password("a2a_backend".into(), "a2a_backend_test_password".into())
        .connect(url)
        .await?;
    Ok((config, client))
}

struct FixtureRootEnv {
    root: tempfile::TempDir,
    env: Vec<EnvGuard>,
    config: GlobalConfig,
}

fn setup_fixture_environment(
    script: Script,
    worker_url: &str,
    address: std::net::SocketAddr,
) -> Result<FixtureRootEnv> {
    let root = tempfile::tempdir()?;
    let env = isolated_environment(&root, worker_url)?;
    let config = fixture_config(&root.path().join("config"), script, worker_url, address)?;
    Ok(FixtureRootEnv { root, env, config })
}
impl Harness {
    pub async fn start(script: Script) -> Result<Self> {
        Self::start_with_access_rules(script, None).await
    }
    pub async fn start_with_access_rules(
        script: Script,
        rules: Option<Arc<harnx_core::access_rules::AccessRules>>,
    ) -> Result<Self> {
        Self::start_with_broker(script, rules, None).await
    }
    pub async fn start_with_broker(
        script: Script,
        rules: Option<Arc<harnx_core::access_rules::AccessRules>>,
        broker_config: Option<&str>,
    ) -> Result<Self> {
        harnx_core::require_nextest();
        let logs = test_logs();
        let boot = bootstrap_broker_and_llm(script, broker_config).await?;
        let worker_url = if broker_config.is_some() {
            boot.url
                .replacen("nats://", "nats://worker:worker_test_password@", 1)
        } else {
            boot.url.clone()
        };
        let fixture_env = setup_fixture_environment(script, &worker_url, boot.address)?;
        let worker = start_worker(&fixture_env.config).await?;
        let (config, client) = configure_broker_client(
            fixture_env.root.path(),
            &boot.url,
            broker_config,
            DefaultBrokerSetup {
                config: fixture_env.config,
                client: boot.client,
            },
        )
        .await?;
        let jetstream = async_nats::jetstream::new(client);
        let metadata = SessionMetadataStore::ensure(&jetstream, 1).await?;
        let store = Arc::new(A2aStore::new_with_access_rules(metadata.clone(), rules));
        let runner = Runner::new(store.clone());
        let export = runner_export();
        Ok(Self {
            logs,
            runner,
            store,
            metadata,
            config,
            export,
            llm: boot.llm,
            jetstream,
            worker,
            http: boot.http,
            _broker: boot.broker,
            _root: fixture_env.root,
            _env: fixture_env.env,
        })
    }
    pub fn config_dir(&self) -> PathBuf {
        self._root.path().join("config")
    }
    pub async fn session(&self, local_id: Option<&str>, owner: &Principal) -> Result<NatsSession> {
        self.runner
            .session(SessionRequest {
                export: &self.export,
                owner: &owner.clone().into(),
                local_id,
                global_config: &self.config,
                activation_route: SessionActivationRoute::ClusterShared,
                abort: harnx_core::abort::create_abort_signal(),
            })
            .await
    }
    pub async fn send(
        &self,
        session: &NatsSession,
        message: a2a_lf::Message,
    ) -> Result<StartTurnResult> {
        tokio::time::timeout(
            DEADLINE,
            self.runner.start_turn(
                TurnRequest {
                    export: &self.export,
                    owner: &alice().into(),
                    session: session.clone(),
                    message,
                },
                InputLimits::default(),
            ),
        )
        .await
        .context("admission deadline")?
    }
    pub async fn task(&self, id: &str) -> Result<TaskRecord> {
        self.store
            .get_task_for_export(&self.export, &alice().into(), id)
            .await?
            .context("task missing")
    }
}
pub fn binary(name: &str) -> Result<PathBuf> {
    let path = std::env::current_exe()?
        .parent()
        .context("test executable directory")?
        .parent()
        .context("target directory")?
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    anyhow::ensure!(
        path.is_file(),
        "required test binary missing: {}; build workspace first",
        path.display()
    );
    Ok(path)
}

async fn process_index_entry(
    store: &A2aStore,
    key: &str,
    local_id: &str,
    entry: &harnx_runtime::nats_session_metadata::TaskIndexEntry,
) -> Result<Option<TaskRecord>> {
    let Some(record) = store.get_task(key, &entry.task_id).await? else {
        return Ok(None);
    };
    anyhow::ensure!(
        record.version == 1 && record.task.context_id == local_id,
        "invalid task record"
    );
    let state_mismatch =
        harnx_a2a_server::store::to_index_state(record.task.status.state.clone()) != entry.state;
    let time_mismatch = record.task.status.timestamp != entry.status_timestamp;
    if state_mismatch || time_mismatch {
        store.repair_index_best_effort(key, &record).await;
    }
    Ok(Some(record))
}

pub async fn list_all_tasks_for_test(
    store: &A2aStore,
    export: &Export,
    owner: &Principal,
    local_id: &str,
) -> Result<Option<Vec<TaskRecord>>> {
    let Some(key) = store
        .resolve_context(export, &owner.clone().into(), local_id)
        .await?
    else {
        return Ok(None);
    };

    let (index, _) = store.get_or_migrate_index(&key, Some(local_id)).await?;
    let mut records = Vec::new();
    for entry in &index.entries {
        if let Some(record) = process_index_entry(store, &key, local_id, entry).await? {
            records.push(record);
        }
    }
    records.sort_by(|a, b| a.task.id.cmp(&b.task.id));
    Ok(Some(records))
}
