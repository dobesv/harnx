//! Real worker and counted tool; only the model is stubbed.
use super::{InteractionModal, ModalState, Tui};
use crate::types::TuiEvent;
use anyhow::Result;
use futures_util::StreamExt;
use harnx_core::{instance::ServerScope, session::SessionLogEntry, tool::ToolCall};
use harnx_runtime::{
    agent_loop::AgentCallFn,
    config::{Config, ConfigLock, GlobalConfig},
    nats_local_server::SharedNatsServer,
    nats_session_log::NatsSessionLog,
    nats_worker::{run_worker_daemon, WorkerDaemonConfig},
    NatsSession, NatsSessionConfig, SessionActivationRoute, SessionInitializer,
};
use serde_json::json;
use std::{
    ffi::OsString,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::task::JoinHandle;

const ENV_KEYS: [&str; 3] = [
    harnx_core::instance::HARNX_SERVER_SCOPE,
    "HARNX_NATS_URL",
    "HARNX_NATS_TOKEN",
];

pub(super) const CLUSTER: &str = "interaction-test";
pub(super) const WAIT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum HarnessKind {
    Counter,
    ShippedDaedalus,
}

pub(super) struct LiveHarness {
    pub tui: Tui,
    pub log: NatsSessionLog,
    pub calls: Arc<AtomicUsize>,
    session: NatsSession,
    worker: JoinHandle<Result<()>>,
    tool: Option<JoinHandle<Result<()>>>,
    _broker: SharedNatsServer,
    prior_env: [Option<OsString>; 3],
}

impl Drop for LiveHarness {
    fn drop(&mut self) {
        self.worker.abort();
        if let Some(tool) = &self.tool {
            tool.abort();
        }
        for (key, prior) in ENV_KEYS.into_iter().zip(&self.prior_env) {
            match prior {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

impl LiveHarness {
    /// Caller holds ENV_LOCK and TestEnvironment for the entire harness lifetime.
    pub async fn start(root: &Path) -> Self {
        Self::start_calls(root, 1).await
    }

    pub async fn start_calls(root: &Path, call_count: usize) -> Self {
        Self::start_kind(root, HarnessKind::Counter, call_count).await
    }

    pub async fn start_daedalus(root: &Path) -> Self {
        Self::start_kind(root, HarnessKind::ShippedDaedalus, 1).await
    }

    pub async fn start_kind(root: &Path, kind: HarnessKind, call_count: usize) -> Self {
        let broker = harnx_runtime::nats_local_server::ensure_shared_server()
            .await
            .unwrap();
        let (config, agent_name, session_name) = configure(root, &broker, kind);
        let scope = ServerScope::new();
        let prior_env = ENV_KEYS.map(std::env::var_os);
        for (key, value) in ENV_KEYS
            .into_iter()
            .zip([scope.as_str(), &broker.url, &broker.token])
        {
            std::env::set_var(key, value);
        }
        let snapshot = config.read().clone();
        let client = snapshot.nats_client(CLUSTER).await.unwrap();
        let js = snapshot.nats_jetstream(CLUSTER).await.unwrap();
        let (tool, calls) = match kind {
            HarnessKind::Counter => {
                let calls = Arc::new(AtomicUsize::new(0));
                let tool = start_tool(&broker, scope, calls.clone(), &js).await;
                (Some(tool), calls)
            }
            HarnessKind::ShippedDaedalus => (None, Arc::new(AtomicUsize::new(0))),
        };
        let mut ready = client
            .subscribe(harnx_runtime::nats_worker::worker_ready_subject(CLUSTER))
            .await
            .unwrap();
        js.query_account().await.unwrap();
        let worker = tokio::spawn(run_worker_daemon(
            config.clone(),
            WorkerDaemonConfig::new(CLUSTER, "tui-test-worker"),
            Some(model(kind, call_count)),
            None,
        ));
        tokio::time::timeout(WAIT, ready.next())
            .await
            .unwrap()
            .unwrap();
        let session = NatsSession::from_global_config(
            NatsSessionConfig {
                cluster: CLUSTER.into(),
                initializer: SessionInitializer::named(agent_name, Default::default()),
                session_id: Some(session_name.into()),
                activation_route: SessionActivationRoute::ClusterShared,
            },
            &config,
            harnx_runtime::utils::create_abort_signal(),
        )
        .await
        .unwrap()
        .with_external_admission();
        let log = NatsSessionLog::new_with_replicas(js, session.storage_key(), 1);
        let mut tui = Tui::init(&config).await.unwrap();
        tui.app.modal = None;
        tui.active_remote_session = Some((session.storage_key().into(), CLUSTER.into()));
        Self {
            tui,
            log,
            calls,
            session,
            worker,
            tool,
            _broker: broker,
            prior_env,
        }
    }

    pub async fn activate(&self) {
        self.session.enqueue_text("Run counted tool").await.unwrap();
    }

    pub async fn wait_for_modal(&mut self) -> InteractionModal {
        self.tui.sync_interaction_client();
        tokio::time::timeout(WAIT, async {
            loop {
                if let Some(ModalState::ConfirmInteraction(state)) = &self.tui.app.modal {
                    return state.as_ref().clone();
                }
                self.pump().await;
            }
        })
        .await
        .expect("durable confirmation must reach the TUI")
    }

    pub async fn pump(&mut self) {
        let event = self
            .tui
            .event_rx
            .recv()
            .await
            .expect("TUI event channel open");
        self.tui.handle_tui_event_inner(event).await.unwrap();
    }

    pub async fn wait_for_decision(&mut self) {
        tokio::time::timeout(WAIT, async {
            loop {
                let event = self.tui.event_rx.recv().await.unwrap();
                let decided = matches!(
                    event,
                    TuiEvent::Interaction(super::InteractionEvent::Decided { .. })
                );
                self.tui.handle_tui_event_inner(event).await.unwrap();
                if decided {
                    break;
                }
            }
        })
        .await
        .expect("TUI durable decision must finish");
    }

    pub async fn completed_entries(&self) -> Vec<(u64, SessionLogEntry)> {
        tokio::time::timeout(WAIT, async {
            loop {
                let entries = self.log.load_events_async().await.unwrap();
                if entries.iter().any(|(_, entry)| {
                    matches!(
                        entry,
                        SessionLogEntry::TurnEnd { .. } | SessionLogEntry::HandoffCommitted { .. }
                    )
                }) {
                    return entries;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("worker must finish the tool round")
    }
}

fn configure(
    root: &Path,
    broker: &SharedNatsServer,
    kind: HarnessKind,
) -> (GlobalConfig, &'static str, &'static str) {
    for dir in ["agents", "clients", "nats_servers"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(root.join("config.yaml"), "model: openai:test-model\n").unwrap();
    std::fs::write(root.join("clients/openai.yaml"), "type: openai\napi_key: sk-test\nmodels:\n- name: test-model\n  type: chat\n  max_input_tokens: 4096\n").unwrap();
    std::fs::write(
        root.join(format!("nats_servers/{CLUSTER}.yaml")),
        format!("url: {}\ntoken: {}\n", broker.url, broker.token),
    )
    .unwrap();

    let (agent_name, session_name) = match kind {
        HarnessKind::Counter => {
            std::fs::write(
                root.join("agents/confirmation-agent.md"),
                "---\nmodel: openai:test-model\nuse_tools: [counter_run]\nmiddleware:\n- name: approval\n  match: '^counter_run$'\n  bash: |\n    if confirm --id gate 'Run counted tool?'; then next; else deny 'Denied by user'; fi\n---\nTest confirmations\n",
            )
            .unwrap();
            ("confirmation-agent", "confirmation-session")
        }
        HarnessKind::ShippedDaedalus => {
            let pantheon_agents = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../packages/pantheon/agents");
            let source = std::fs::read_to_string(pantheon_agents.join("daedalus.md")).unwrap();
            let model_line = source
                .lines()
                .find(|line| line.starts_with("model: "))
                .expect("daedalus model line");
            let source = source.replacen(model_line, "model: openai:test-model", 1);
            std::fs::write(root.join("agents/daedalus.md"), source).unwrap();
            std::fs::write(
                root.join("agents/atlas.md"),
                "---\nmodel: openai:test-model\n---\nAtlas target\n",
            )
            .unwrap();
            let shared_src = pantheon_agents.join("shared");
            let shared_dst = root.join("agents/shared");
            std::fs::create_dir_all(&shared_dst).unwrap();
            for entry in std::fs::read_dir(shared_src).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_file() {
                    std::fs::copy(entry.path(), shared_dst.join(entry.file_name())).unwrap();
                }
            }
            ("daedalus", "daedalus-session")
        }
    };

    let config = Arc::new(ConfigLock::new(
        Config::load_from_file(&root.join("config.yaml")).unwrap(),
    ));
    {
        let mut cfg = config.write();
        cfg.remote_agent = Some((agent_name.into(), CLUSTER.into()));
        cfg.nats_routing = harnx_runtime::config::NatsRouting::Cluster(CLUSTER.into());
        cfg.session = Some(harnx_runtime::config::session::new(&cfg, session_name, None).unwrap());
    }
    (config, agent_name, session_name)
}

fn model(kind: HarnessKind, call_count: usize) -> AgentCallFn {
    let turns = AtomicUsize::new(0);
    Arc::new(move |_, _, _| {
        let first = turns.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            let calls = if first {
                match kind {
                    HarnessKind::Counter => (0..call_count)
                        .map(|index| {
                            ToolCall::new(
                                "counter_run".into(),
                                json!({}),
                                Some(format!("counted-call-{index}")),
                                None,
                            )
                        })
                        .collect(),
                    HarnessKind::ShippedDaedalus => vec![ToolCall::new(
                        "atlas_session_handoff".into(),
                        json!({
                            "prompt": "execute plan",
                            "session_id": "atlas-execution"
                        }),
                        Some("daedalus-handoff-call".into()),
                        None,
                    )],
                }
            } else {
                vec![]
            };
            Ok(("done".into(), None, calls, Default::default()))
        })
    })
}

struct Counter(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl harnx_toolset::Toolset for Counter {
    fn name(&self) -> &str {
        "counter"
    }
    fn tools(&self) -> Vec<harnx_toolset::ToolSpec> {
        vec![harnx_toolset::ToolSpec {
            name: "run".into(),
            description: "Count effects".into(),
            input_schema: json!({"type": "object"}),
            idempotent_hint: true,
            read_only_hint: false,
            timeout_secs: None,
            meta: None,
            cancellation_guarantee: Default::default(),
        }]
    }
    async fn invoke(
        &self,
        _: &str,
        _: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, harnx_toolset::ToolInvokeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"counted": true}))
    }
}

async fn start_tool(
    broker: &SharedNatsServer,
    scope: ServerScope,
    calls: Arc<AtomicUsize>,
    js: &async_nats::jetstream::Context,
) -> JoinHandle<Result<()>> {
    let (url, token) = (broker.url.clone(), broker.token.clone());
    let key = harnx_toolset_server::registration_key(&scope, "____counter");
    let task = tokio::spawn(async move {
        harnx_toolset_server::serve_over_nats(Counter(calls), scope, &url, &token).await
    });
    tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(bucket) = js
                .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
                .await
            {
                if bucket.get(&key).await.unwrap().is_some() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("counter tool registration");
    task
}
