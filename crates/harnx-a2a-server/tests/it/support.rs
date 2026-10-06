//! Real worker + isolated JetStream + scripted OpenAI-compatible streaming LLM.
//! Follows MCP's Readiness harness; barriers replace timing guesses in turns.
use anyhow::{bail, Context, Result};
use axum::{
    extract::State,
    response::{
        sse::{Event, Sse},
        IntoResponse,
    },
    routing::post,
    Json, Router,
};
use futures::stream;
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
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    ffi::OsString,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Notify;
use tokio_util::task::AbortOnDropHandle;

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
    pub(super) async fn start() -> Result<(Self, String, async_nats::Client)> {
        let mut broker = Self::spawn()?;
        let (url, client) = tokio::time::timeout(DEADLINE, broker.wait_for_client())
            .await
            .context("broker readiness deadline")??;
        Ok((broker, url, client))
    }

    fn spawn() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let child = Command::new(
            std::env::var_os("NATS_SERVER_BIN").unwrap_or_else(|| "nats-server".into()),
        )
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
            for entry in std::fs::read_dir(self._dir.path())? {
                let Some(url) = broker_ports_url(&entry?.path())? else {
                    continue;
                };
                if let Ok(client) = async_nats::connect(&url).await {
                    return Ok((url, client));
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
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
    Fail,
    Many,
}

pub struct Llm {
    pub requests: parking_lot::Mutex<Vec<Value>>,
    pub release: Notify,
    pub requested: Notify,
    script: Script,
}

fn chunk(delta: Value, finish: Value) -> String {
    json!({"id":"runner-completion", "object":"chat.completion.chunk", "created":0,
        "model":"test", "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    .to_string()
}
async fn completion(
    State(llm): State<Arc<Llm>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let first = {
        let mut requests = llm.requests.lock();
        let first = requests.is_empty();
        requests.push(body.clone());
        first
    };
    llm.requested.notify_one();
    if llm.script == Script::Fail {
        return (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error":{"message":"secret-token /srv/private https://private.invalid", "type":"invalid_request_error"}}))).into_response();
    }
    assert_eq!(body["stream"], true, "worker must call streaming LLM");
    let many = llm.script == Script::Many;
    let frames = if many {
        let mut frames = vec![
            chunk(json!({"content":"x"}), Value::Null);
            harnx_a2a_server::runner::EVENT_CAPACITY + 5
        ];
        frames.push(chunk(json!({}), json!("stop")));
        frames.push("[DONE]".into());
        frames
    } else if llm.script == Script::Tool && first {
        vec![
            chunk(
                json!({"tool_calls":[{"index":0,"id":"runner-handoff", "type":"function",
            "function":{"name":"target_session_handoff", "arguments":"{\"prompt\":\"must not run\",\"session_id\":\"denied-target\"}"}}]}),
                Value::Null,
            ),
            chunk(json!({}), json!("tool_calls")),
            "[DONE]".into(),
        ]
    } else {
        vec![
            chunk(
                json!({"reasoning_content":"private reasoning", "content":"Hello "}),
                Value::Null,
            ),
            chunk(json!({"content":"world"}), Value::Null),
            chunk(json!({}), json!("stop")),
            "[DONE]".into(),
        ]
    };
    // Text turns stop after their first real SSE token until the test observes
    // an A2A artifact. This proves mid-stream behavior, without sleep-based races.
    let held = llm.script == Script::Text;
    Sse::new(stream::unfold(
        (0, frames, llm),
        move |(index, frames, llm)| async move {
            if index >= frames.len() {
                return None;
            }
            if (held && index == 1) || (many && index > 0 && index <= frames.len() - 2) {
                llm.release.notified().await;
            }
            let event = Event::default().data(frames[index].clone());
            Some((Ok::<_, Infallible>(event), (index + 1, frames, llm)))
        },
    ))
    .into_response()
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

fn fixture_config(
    dir: &std::path::Path,
    script: Script,
    url: &str,
    address: std::net::SocketAddr,
) -> Result<GlobalConfig> {
    for subdir in ["agents", "clients", "nats_servers"] {
        std::fs::create_dir_all(dir.join(subdir))?;
    }
    std::fs::write(
        dir.join("config.yaml"),
        "model: mock:test\nstream: true\nsave: false\n",
    )?;
    std::fs::write(
        dir.join("nats_servers/runner.yaml"),
        format!("url: {url:?}\n"),
    )?;
    std::fs::write(dir.join("clients/mock.yaml"), format!("type: openai-compatible\nname: mock\napi_base: http://{address}/v1\napi_key: test-key\nmodels:\n  - name: test\n    max_input_tokens: 32000\n    max_output_tokens: 1024\n"))?;
    let agent = if script == Script::Tool {
        let binary = binary("harnx-claude-compatible-hook-server")?;
        let command = shell_words::join([binary.to_str().context("hook path encoding")?, "--event", "PreToolUse", "--matcher", "^target_session_handoff$", "--jaq", "{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"Approve handoff?\"}}"]);
        format!("---\nmodel: mock:test\nuse_tools:\n- target_session_handoff\nhooks:\n  entries:\n    - command: {command:?}\n---\nTest runner agent\n")
    } else {
        "---\nmodel: mock:test\n---\nTest runner agent\n".into()
    };
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
    _broker: Broker,
    _root: tempfile::TempDir,
    _env: Vec<EnvGuard>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.worker.abort();
        self.http.abort();
    }
}
impl Harness {
    pub async fn start(script: Script) -> Result<Self> {
        harnx_core::require_nextest();
        let logs = test_logs();
        let (broker, url, client) = Broker::start().await?;
        let (llm, address, http) = start_llm(script).await?;
        let root = tempfile::tempdir()?;
        let env = isolated_environment(&root, &url)?;
        let config = fixture_config(&root.path().join("config"), script, &url, address)?;
        let worker = start_worker(&config).await?;
        let jetstream = async_nats::jetstream::new(client);
        let metadata = SessionMetadataStore::ensure(&jetstream, 1).await?;
        let store = Arc::new(A2aStore::new(metadata.clone()));
        let runner = Runner::new(store.clone());
        let export = runner_export();
        Ok(Self {
            logs,
            runner,
            store,
            metadata,
            config,
            export,
            llm,
            jetstream,
            worker,
            http,
            _broker: broker,
            _root: root,
            _env: env,
        })
    }
    pub fn config_dir(&self) -> PathBuf {
        self._root.path().join("config")
    }
    pub async fn session(&self, local_id: Option<&str>, owner: &Principal) -> Result<NatsSession> {
        self.runner
            .session(SessionRequest {
                export: &self.export,
                owner,
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
                    owner: &alice(),
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
            .get_task_for_export(&self.export, &alice(), id)
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
