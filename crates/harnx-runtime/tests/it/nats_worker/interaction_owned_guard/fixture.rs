use super::*;
use harnx_core::instance::ServerScope;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
use std::sync::Mutex;

pub(super) struct Warnings {
    pub messages: Mutex<Vec<String>>,
}
pub(super) static WARNINGS: Warnings = Warnings {
    messages: Mutex::new(Vec::new()),
};
impl log::Log for Warnings {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let message = record.args().to_string();
            if message.contains("newer worker needed")
                || message.contains("checkpoint recovery deferred")
            {
                self.messages.lock().unwrap().push(message);
            }
        }
    }
    fn flush(&self) {}
}

struct SafeTool(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Toolset for SafeTool {
    fn name(&self) -> &str {
        "guard"
    }
    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "read".into(),
            description: "Safe recovery probe".into(),
            input_schema: json!({"type":"object"}),
            idempotent_hint: true,
            read_only_hint: true,
            cancellation_guarantee: Default::default(),
            timeout_secs: None,
            meta: None,
        }]
    }
    async fn invoke(
        &self,
        _: &str,
        _: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
    ) -> std::result::Result<serde_json::Value, ToolInvokeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"ok":true}))
    }
}

pub(super) struct Fixture {
    pub id: &'static str,
    pub js: async_nats::jetstream::Context,
    pub log: NatsSessionLog,
    pub config: harnx_runtime::config::GlobalConfig,
    pub models: Arc<AtomicUsize>,
    pub tools: Arc<AtomicUsize>,
    scope: ServerScope,
    metadata: SessionMetadataStore,
    _tool: AbortOnDropHandle<Result<()>>,
    _env: [EnvGuard; 3],
    _server: common::NatsServerHandle,
}

impl Fixture {
    pub async fn new(case: Case) -> Result<Self> {
        require_nextest();
        log::set_logger(&WARNINGS).expect("nextest isolates logger per test");
        log::set_max_level(log::LevelFilter::Warn);
        let server = spawn_nats_server().await?.context("nats-server required")?;
        let client = async_nats::connect(server.url()).await?;
        let js = async_nats::jetstream::new(client.clone());
        let config = local_nats_runtime_config(server.url());
        let scope = ServerScope::new();
        let env = EnvGuard::tool_server_environment(&scope, server.url());
        let tools = Arc::new(AtomicUsize::new(0));
        let tool = worker::start_tool_server(
            &js,
            client,
            scope.clone(),
            Arc::new(SafeTool(tools.clone())),
        )
        .await?;
        harnx_runtime::tool_context::refresh_nats_tool_declarations(&config, &scope).await;
        let id = "interaction-owned-guard";
        let key = storage_key(id);
        let (metadata, _) = seed_session_metadata(&js, id).await?;
        let log = NatsSessionLog::new_with_replicas(js.clone(), &key, 1);
        worker::append_admitted_fixture_user(&log, "original-prompt", "original request").await?;
        let round = log.append_event_async(&records::calls()).await?;
        records::saved_journal(&js, &log, round).await?;
        records::seed_case(case, &js, &log, round).await?;
        Ok(Self {
            id,
            js,
            log,
            config,
            models: Arc::new(AtomicUsize::new(0)),
            tools,
            scope,
            metadata,
            _tool: tool,
            _env: env,
            _server: server,
        })
    }

    pub fn call_fn(&self) -> harnx_runtime::agent_loop::AgentCallFn {
        counting_stub_call_fn(self.models.clone())
    }

    pub async fn spawn_daemon(&self) -> Result<AbortOnDropHandle<Result<()>>> {
        let client = async_nats::connect(self._server.url()).await?;
        let mut ready = client
            .subscribe(harnx_runtime::nats_worker::worker_ready_subject("local"))
            .await?;
        client.flush().await?;
        let config = self.config.clone();
        let call = self.call_fn();
        let mut daemon = tokio::spawn(async move {
            run_worker_daemon(
                config,
                WorkerDaemonConfig::new("local", "guard-worker"),
                Some(call),
                None,
            )
            .await
        });
        worker::await_worker_ready(&mut daemon, &mut ready).await?;
        Ok(AbortOnDropHandle::new(daemon))
    }

    pub async fn direct_run(&self) -> Result<()> {
        let mut input =
            harnx_runtime::config::input::from_str(&self.config, "original request", None);
        input.with_session = true;
        input.skip_user_log_append = true;
        run_agent_loop_with_nats(RunAgentLoopArgs {
            cluster_key: "local",
            manage_servers: false,
            session_id: self.log.storage_key(),
            config: self.config.clone(),
            instance_id: self.scope.clone(),
            initial_input: input,
            abort_signal: create_abort_signal(),
            token_budget: None,
            call_fn: Some(self.call_fn()),
            lease: None,
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
            event_sink: None,
            after_seq_observer: None,
            session_metadata: Some(&self.metadata),
            on_tool_round: None,
            working_dir: None,
        })
        .await
    }
}
