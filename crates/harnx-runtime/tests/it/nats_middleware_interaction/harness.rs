use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_core::{
    instance::ServerScope,
    interaction::InteractionResponse,
    message::Message,
    require_nextest,
    session::SessionLogEntry,
    tool::{ToolCall, ToolProvider},
};
use harnx_runtime::{
    agent_loop::AgentCallFn,
    client::CompletionTokenUsage,
    config::{Config, ConfigLock},
    interactions::InteractionDecisionOutcome,
    nats_session::{NatsSession, NatsSessionConfig},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::SessionInitializer,
    nats_worker::{
        publish_session_activate, run_worker_daemon, SessionActivate, WorkerDaemonConfig,
    },
    utils::create_abort_signal,
};
use parking_lot::Mutex;
use serde_json::json;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[path = "probe.rs"]
pub mod probe;
pub use probe::{serve_probe, Env, Probe};

pub const SESSION_ID: &str = "test-middleware-session";
pub const AGENT_NAME: &str = "middleware-agent";

pub fn probe_test_two_call_model() -> (AgentCallFn, Arc<Mutex<Vec<Message>>>) {
    let turns = Arc::new(AtomicUsize::new(0));
    let messages = Arc::new(Mutex::new(Vec::new()));
    let seen = messages.clone();
    let model: AgentCallFn = Arc::new(move |input, config, _abort| {
        let first = turns.fetch_add(1, Ordering::SeqCst) == 0;
        if !first {
            *seen.lock() = harnx_runtime::config::input::build_messages(input, config).unwrap();
        }
        Box::pin(async move {
            let calls = if first {
                vec![
                    ToolCall::new(
                        "probe_run".into(),
                        json!({"value": "call_a"}),
                        Some("call-a".into()),
                        None,
                    ),
                    ToolCall::new(
                        "probe_aux".into(),
                        json!({"value": "call_b", "observe": true}),
                        Some("call-b".into()),
                        None,
                    ),
                ]
            } else {
                vec![]
            };
            Ok((
                "activation completed".to_string(),
                None,
                calls,
                CompletionTokenUsage::default(),
            ))
        })
    });
    (model, messages)
}

#[cfg(feature = "fault-injection")]
pub const BASH_PASSTHROUGH: &str = r#"
middleware:
- name: pass
  match: '^probe_run$'
  bash: |
    next
"#;

pub const BASH_CONFIRM: &str = r#"
middleware:
- name: gate
  match: '^probe_run$'
  bash: |
    if confirm --id gate 'Proceed?'; then
      next
    else
      deny 'user denied'
    fi
"#;

pub fn single_call_model(tool_name: &'static str, args: Value, id: &'static str) -> AgentCallFn {
    let turns = Arc::new(AtomicUsize::new(0));
    Arc::new(move |_input, _config, _abort| {
        let first = turns.fetch_add(1, Ordering::SeqCst) == 0;
        let tool_name = tool_name.to_string();
        let args = args.clone();
        let id = id.to_string();
        Box::pin(async move {
            let calls = if first {
                vec![ToolCall::new(tool_name, args, Some(id), None)]
            } else {
                vec![]
            };
            Ok((
                "activation completed".to_string(),
                None,
                calls,
                CompletionTokenUsage::default(),
            ))
        })
    })
}

#[cfg(feature = "fault-injection")]
pub fn counting_single_call_model(
    tool_name: &'static str,
    args: Value,
    call_id: &'static str,
) -> (AgentCallFn, Arc<AtomicUsize>) {
    let model_calls = Arc::new(AtomicUsize::new(0));
    let calls_counter = model_calls.clone();
    let model: AgentCallFn = Arc::new(move |_input, _config, _abort| {
        let count = calls_counter.fetch_add(1, Ordering::SeqCst);
        let args = args.clone();
        Box::pin(async move {
            let calls = if count == 0 {
                vec![ToolCall::new(
                    tool_name.into(),
                    args,
                    Some(call_id.into()),
                    None,
                )]
            } else {
                vec![]
            };
            Ok((
                "done".to_string(),
                None,
                calls,
                CompletionTokenUsage::default(),
            ))
        })
    });
    (model, model_calls)
}

#[cfg(feature = "fault-injection")]
pub fn handoff_single_call_model(
    tool_name: &'static str,
    args: Value,
    id: &'static str,
) -> (AgentCallFn, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let source_calls = Arc::new(AtomicUsize::new(0));
    let target_calls = Arc::new(AtomicUsize::new(0));
    let source_counter = source_calls.clone();
    let target_counter = target_calls.clone();
    let model: AgentCallFn = Arc::new(move |_input, config, _abort| {
        let agent = config.read().extract_agent().name().to_string();
        let tool_name = tool_name.to_string();
        let args = args.clone();
        let id = id.to_string();
        let source_counter = source_counter.clone();
        let target_counter = target_counter.clone();
        Box::pin(async move {
            if agent == AGENT_NAME {
                let first = source_counter.fetch_add(1, Ordering::SeqCst) == 0;
                let calls = if first {
                    vec![ToolCall::new(tool_name, args, Some(id), None)]
                } else {
                    vec![]
                };
                Ok((
                    "activation completed".to_string(),
                    None,
                    calls,
                    CompletionTokenUsage::default(),
                ))
            } else {
                target_counter.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "target completed".to_string(),
                    None,
                    vec![],
                    CompletionTokenUsage::default(),
                ))
            }
        })
    });
    (model, source_calls, target_calls)
}

#[cfg(feature = "fault-injection")]
pub fn handoff_multi_call_model(
    source_tool_calls: Vec<ToolCall>,
) -> (AgentCallFn, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let source_calls = Arc::new(AtomicUsize::new(0));
    let target_calls = Arc::new(AtomicUsize::new(0));
    let source_counter = source_calls.clone();
    let target_counter = target_calls.clone();
    let model: AgentCallFn = Arc::new(move |_input, config, _abort| {
        let agent = config.read().extract_agent().name().to_string();
        let source_tool_calls = source_tool_calls.clone();
        let source_counter = source_counter.clone();
        let target_counter = target_counter.clone();
        Box::pin(async move {
            if agent == AGENT_NAME {
                let first = source_counter.fetch_add(1, Ordering::SeqCst) == 0;
                let calls = if first { source_tool_calls } else { vec![] };
                Ok((
                    "activation completed".to_string(),
                    None,
                    calls,
                    CompletionTokenUsage::default(),
                ))
            } else {
                target_counter.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "target completed".to_string(),
                    None,
                    vec![],
                    CompletionTokenUsage::default(),
                ))
            }
        })
    });
    (model, source_calls, target_calls)
}

#[cfg(feature = "fault-injection")]
pub async fn start_confirm_fault(
    point: harnx_runtime::fault_injection::RecoveryFaultPoint,
    call_id: &'static str,
) -> Result<(
    Harness,
    harnx_runtime::fault_injection::PauseHandle,
    u64,
    String,
)> {
    let model = single_call_model("probe_run", json!({"value": "run"}), call_id);
    let (harness, req_seq, interaction_id, _) =
        Harness::start_and_suspend(BASH_CONFIRM, model, "start confirm fault").await?;
    let pause = harnx_runtime::fault_injection::pause_point(harness.session.storage_key(), point);
    Ok((harness, pause, req_seq, interaction_id))
}

pub fn extract_tool_results(
    entries: &[(u64, SessionLogEntry)],
) -> &[harnx_core::session::ToolOutput] {
    entries
        .iter()
        .find_map(|(_, e)| match e {
            SessionLogEntry::ToolResults { results, .. } => Some(results.as_slice()),
            _ => None,
        })
        .expect("ToolResults")
}

#[cfg(feature = "fault-injection")]
pub fn assert_single_tool_results(
    entries: &[(u64, SessionLogEntry)],
) -> &[harnx_core::session::ToolOutput] {
    let tool_results_entries: Vec<_> = entries
        .iter()
        .filter(|(_, e)| matches!(e, SessionLogEntry::ToolResults { .. }))
        .collect();
    assert_eq!(
        tool_results_entries.len(),
        1,
        "exactly one ToolResults entry required in session log"
    );
    extract_tool_results(entries)
}

pub fn assert_middleware_changed_error(entries: &[(u64, SessionLogEntry)]) {
    let results = extract_tool_results(entries);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].output["is_error"], true);
    assert!(
        results[0].output["error"]
            .as_str()
            .unwrap_or_default()
            .contains("middleware changed while the call was waiting"),
        "error must report middleware changed while waiting: {:?}",
        results[0].output
    );
}

fn setup_test_config(
    root: &Path,
    server_url: &str,
    middleware_yaml: &str,
    handoff_targets: bool,
) -> Result<PathBuf> {
    let config_dir = root.join("config");
    for dir in ["agents", "clients", "nats_servers"] {
        std::fs::create_dir_all(config_dir.join(dir))?;
    }
    std::fs::write(
        config_dir.join("config.yaml"),
        "model: openai:test-model
",
    )?;
    std::fs::write(
        config_dir.join("clients/openai.yaml"),
        "type: openai
api_key: sk-test
models:
  - name: test-model
    type: chat
    max_input_tokens: 4096
",
    )?;
    std::fs::write(
        config_dir.join("nats_servers/local.yaml"),
        format!(
            "url: {server_url}
"
        ),
    )?;

    let use_tools = if handoff_targets {
        "[probe_run, probe_aux, target_agent_session_handoff, other_agent_session_handoff]"
    } else {
        "[probe_run, probe_aux]"
    };
    let agent_content = format!(
        "---
model: openai:test-model
use_tools: {use_tools}
{middleware_yaml}
---
Middleware Agent Instructions
",
    );
    std::fs::write(
        config_dir.join(format!("agents/{AGENT_NAME}.md")),
        agent_content,
    )?;
    if handoff_targets {
        std::fs::write(
            config_dir.join("agents/target_agent.md"),
            "---\nmodel: openai:test-model\n---\nTarget Agent Instructions\n",
        )?;
        std::fs::write(
            config_dir.join("agents/other_agent.md"),
            "---\nmodel: openai:test-model\n---\nOther Agent Instructions\n",
        )?;
    }
    Ok(config_dir)
}

pub struct Harness {
    pub client: async_nats::Client,
    pub js: async_nats::jetstream::Context,
    pub session: NatsSession,
    pub probe: Arc<Probe>,
    pub daemon: tokio::task::JoinHandle<Result<()>>,
    pub tool_server: tokio::task::JoinHandle<Result<()>>,
    pub config_dir: PathBuf,
    pub config: Arc<ConfigLock>,
    pub call_fn: AgentCallFn,
    pub _server: crate::common::NatsServerHandle,
    pub _root: tempfile::TempDir,
    pub _env: Env,
    pub _scope_env: Env,
}

impl Harness {
    pub async fn required_start(middleware_yaml: &str, call_fn: AgentCallFn) -> Result<Self> {
        let Some(harness) = Self::start(middleware_yaml, call_fn).await? else {
            anyhow::bail!("NATS test server is required for middleware interaction tests");
        };
        Ok(harness)
    }

    pub async fn start_and_suspend(
        middleware_yaml: &str,
        call_fn: AgentCallFn,
        activate_msg: &str,
    ) -> Result<(Self, u64, String, String)> {
        let harness = Self::required_start(middleware_yaml, call_fn).await?;
        harness.activate(activate_msg).await?;
        let (req_seq, interaction_id, call_id) = harness.wait_for_suspension().await?;
        Ok((harness, req_seq, interaction_id, call_id))
    }

    pub async fn start(middleware_yaml: &str, call_fn: AgentCallFn) -> Result<Option<Self>> {
        Self::start_inner(middleware_yaml, call_fn, false).await
    }

    #[cfg(feature = "fault-injection")]
    pub async fn start_handoff(
        middleware_yaml: &str,
        call_fn: AgentCallFn,
    ) -> Result<Option<Self>> {
        Self::start_inner(middleware_yaml, call_fn, true).await
    }

    #[cfg(feature = "fault-injection")]
    pub async fn required_start_handoff(
        middleware_yaml: &str,
        call_fn: AgentCallFn,
    ) -> Result<Self> {
        require_nextest();
        Self::start_handoff(middleware_yaml, call_fn)
            .await?
            .context("NATS server required")
    }

    async fn start_inner(
        middleware_yaml: &str,
        call_fn: AgentCallFn,
        handoff_targets: bool,
    ) -> Result<Option<Self>> {
        require_nextest();
        let Some(server) = crate::common::spawn_nats_server().await? else {
            return Ok(None);
        };
        let root = tempfile::tempdir()?;
        let config_dir =
            setup_test_config(root.path(), server.url(), middleware_yaml, handoff_targets)?;

        let scope = ServerScope::new();
        let scope_env = Env::set(&[(harnx_core::instance::HARNX_SERVER_SCOPE, scope.as_str())]);

        let data_dir = root.path().join("data");
        let state_dir = root.path().join("state");
        let env_bindings = [
            ("HARNX_CONFIG_DIR", config_dir.to_str().unwrap()),
            ("HARNX_DATA_DIR", data_dir.to_str().unwrap()),
            ("HARNX_STATE_DIR", state_dir.to_str().unwrap()),
            ("HARNX_NATS_URL", server.url()),
            ("HARNX_NATS_TOKEN", "test-token"),
            (harnx_core::instance::HARNX_SERVER_SCOPE, scope.as_str()),
        ];
        let env = Env::set(&env_bindings);

        let config = Arc::new(ConfigLock::new(Config::load_from_file(
            &config_dir.join("config.yaml"),
        )?));
        let cfg = config.read().clone();
        let client = cfg.nats_client("local").await?;
        let js = cfg.nats_jetstream("local").await?;

        let probe = Arc::new(Probe::default());
        let tool_server = serve_probe(server.url(), &scope, probe.clone()).await?;

        let mut ready = client
            .subscribe(harnx_runtime::nats_worker::worker_ready_subject("local"))
            .await?;
        client.flush().await?;

        let worker_config = WorkerDaemonConfig::new("local", "worker-middleware-worker-a");
        assert!(!worker_config.manage_servers);

        let daemon = tokio::spawn(run_worker_daemon(
            config.clone(),
            worker_config,
            Some(call_fn.clone()),
            None,
        ));
        tokio::time::timeout(Duration::from_secs(30), ready.next())
            .await?
            .context("worker readiness subscription closed")?;

        let session = NatsSession::new(
            NatsSessionConfig {
                cluster: "local".into(),
                initializer: SessionInitializer::named(AGENT_NAME, Default::default()),
                session_id: Some(SESSION_ID.into()),
                activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
            },
            client.clone(),
            js.clone(),
            create_abort_signal(),
        )
        .await?;

        Ok(Some(Self {
            client,
            js,
            session,
            probe,
            daemon,
            tool_server,
            config_dir,
            config,
            call_fn,
            _server: server,
            _root: root,
            _env: env,
            _scope_env: scope_env,
        }))
    }

    pub async fn restart_worker_with_identity(&mut self, identity: &str) -> Result<()> {
        self.daemon.abort();
        let _ = (&mut self.daemon).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut ready = self
            .client
            .subscribe(harnx_runtime::nats_worker::worker_ready_subject("local"))
            .await?;
        self.client.flush().await?;

        let worker_config = WorkerDaemonConfig::new("local", identity);
        assert!(!worker_config.manage_servers);

        let daemon = tokio::spawn(run_worker_daemon(
            self.config.clone(),
            worker_config,
            Some(self.call_fn.clone()),
            None,
        ));
        tokio::time::timeout(Duration::from_secs(30), ready.next())
            .await?
            .context("replacement worker readiness subscription closed")?;
        self.daemon = daemon;
        Ok(())
    }

    #[cfg(feature = "fault-injection")]
    pub async fn force_reclaim_lease(&self) -> Result<()> {
        let lease_config = harnx_runtime::nats_lease::NatsLeaseConfig::default();
        let leases = self.js.get_key_value(&lease_config.bucket).await?;
        let lease_key = lease_config.key_for_session(self.session.storage_key());
        leases.purge(&lease_key).await?;
        Ok(())
    }

    #[cfg(feature = "fault-injection")]
    pub async fn kill_and_replace_worker(&mut self, identity: &str) -> Result<()> {
        self.daemon.abort();
        let _ = (&mut self.daemon).await;
        self.force_reclaim_lease().await?;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut ready = self
            .client
            .subscribe(harnx_runtime::nats_worker::worker_ready_subject("local"))
            .await?;
        self.client.flush().await?;

        let worker_config = WorkerDaemonConfig::new("local", identity);
        assert!(!worker_config.manage_servers);

        let daemon = tokio::spawn(run_worker_daemon(
            self.config.clone(),
            worker_config,
            Some(self.call_fn.clone()),
            None,
        ));
        tokio::time::timeout(Duration::from_secs(30), ready.next())
            .await?
            .context("replacement worker readiness subscription closed")?;
        self.daemon = daemon;
        Ok(())
    }

    #[cfg(feature = "fault-injection")]
    pub async fn reactivate(&self) -> Result<()> {
        publish_session_activate(
            &self.js,
            "local",
            &SessionActivate::new(self.session.storage_key()),
            1,
        )
        .await?;
        Ok(())
    }

    #[cfg(feature = "fault-injection")]
    pub async fn metadata_execution_contexts(
        &self,
    ) -> Result<Vec<harnx_core::execution_context::ExecutionContextObservation>> {
        let store =
            harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&self.js, 1).await?;
        let record = store
            .get(self.session.storage_key())
            .await?
            .expect("session metadata record");
        harnx_runtime::nats_session_metadata::execution_contexts(&record.metadata)
    }

    #[cfg(feature = "fault-injection")]
    pub fn spawn_approve(
        &self,
        interaction_id: &str,
        request_seq: u64,
    ) -> tokio::task::JoinHandle<Result<InteractionDecisionOutcome>> {
        let session = self.session.clone();
        let int_id = interaction_id.to_string();
        tokio::spawn(async move {
            let response = InteractionResponse {
                action: "approve".into(),
                data: serde_json::Map::new(),
                note: None,
            };
            session
                .decide_interaction(&int_id, request_seq, &response)
                .await
        })
    }

    pub fn update_agent_middleware(&self, middleware_yaml: &str) -> Result<()> {
        let has_handoff = self.config_dir.join("agents/target_agent.md").exists();
        let use_tools = if has_handoff {
            "[probe_run, probe_aux, target_agent_session_handoff, other_agent_session_handoff]"
        } else {
            "[probe_run, probe_aux]"
        };
        let agent_content = format!(
            "---
model: openai:test-model
use_tools: {use_tools}
{}
---
Middleware Agent Instructions
",
            middleware_yaml
        );
        std::fs::write(
            self.config_dir.join(format!("agents/{AGENT_NAME}.md")),
            agent_content,
        )?;
        Ok(())
    }

    pub async fn activate(&self, text: &str) -> Result<()> {
        let storage_key = self.session.storage_key();
        let log = NatsSessionLog::new_with_replicas(self.js.clone(), storage_key, 1);
        crate::worker::append_admitted_fixture_user(&log, &uuid::Uuid::new_v4().to_string(), text)
            .await?;
        publish_session_activate(
            &self.js,
            "local",
            &SessionActivate::new(self.session.storage_key()),
            1,
        )
        .await?;
        Ok(())
    }

    pub async fn wait_for_suspension(&self) -> Result<(u64, String, String)> {
        let storage_key = self.session.storage_key();
        let log = NatsSessionLog::new_with_replicas(self.js.clone(), storage_key, 1);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let entries = log.load_events_async().await?;
            let has_ready = entries
                .iter()
                .any(|(_, e)| matches!(e, SessionLogEntry::SuspensionReady { .. }));
            if has_ready {
                if let Some((seq, req, call_id)) = entries.iter().find_map(|(s, e)| match e {
                    SessionLogEntry::InteractionRequested {
                        interaction_id,
                        tool_call_id,
                        ..
                    } => Some((*s, interaction_id.clone(), tool_call_id.clone())),
                    _ => None,
                }) {
                    return Ok((seq, req, call_id));
                }
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for suspension"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[cfg(feature = "fault-injection")]
    pub async fn entries(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
        let log = NatsSessionLog::new_with_replicas(self.js.clone(), self.session.storage_key(), 1);
        log.load_events_async().await
    }

    #[cfg(feature = "fault-injection")]
    pub async fn target_entries(
        &self,
        target_agent: &str,
        target_session_id: &str,
    ) -> Result<Vec<(u64, SessionLogEntry)>> {
        let storage_key =
            harnx_core::session_identity::session_key(Some(target_agent), target_session_id);
        use async_nats::jetstream::{context::GetStreamErrorKind, ErrorCode};
        let stream = harnx_runtime::nats_session_log::stream_name_for_session(&storage_key);
        match self.js.get_stream(stream).await {
            Ok(_) => {}
            Err(error)
                if matches!(error.kind(), GetStreamErrorKind::JetStream(ref error)
                    if error.kind() == ErrorCode::STREAM_NOT_FOUND) =>
            {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.into()),
        }
        NatsSessionLog::new(self.js.clone(), &storage_key)
            .load_events_latest_required_async()
            .await
    }

    #[cfg(feature = "fault-injection")]
    pub async fn count_target_prompts(
        &self,
        target_agent: &str,
        target_session_id: &str,
    ) -> Result<usize> {
        let entries = self.target_entries(target_agent, target_session_id).await?;
        Ok(entries
            .iter()
            .filter(|(_, entry)| {
                matches!(entry, SessionLogEntry::Message { role, content, .. }
                    if role.is_user() && !harnx_core::timing_identity::is_timing_notice(&content.to_text()))
            })
            .count())
    }

    #[cfg(feature = "fault-injection")]
    pub async fn count_source_commits(&self) -> Result<usize> {
        let entries = self.entries().await?;
        Ok(entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::HandoffCommitted { .. }))
            .count())
    }

    #[cfg(feature = "fault-injection")]
    pub async fn wait_for_target_completed(
        &self,
        target_agent: &str,
        target_session_id: &str,
    ) -> Result<Vec<(u64, SessionLogEntry)>> {
        let storage_key =
            harnx_core::session_identity::session_key(Some(target_agent), target_session_id);
        let log = NatsSessionLog::new_with_replicas(self.js.clone(), &storage_key, 1);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let entries = log.load_events_async().await?;
            let has_reply = entries.iter().any(|(_, entry)| {
                matches!(entry, SessionLogEntry::Message { role, .. } if role.is_assistant())
            });
            let has_turn_end = entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. }));
            if has_reply && has_turn_end {
                return Ok(entries);
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for target '{target_agent}' session '{target_session_id}' to complete"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn wait_for_turn_end(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
        let storage_key = self.session.storage_key();
        let log = NatsSessionLog::new_with_replicas(self.js.clone(), storage_key, 1);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let entries = log.load_events_async().await?;
            if entries
                .iter()
                .any(|(_, e)| matches!(e, SessionLogEntry::TurnEnd { .. }))
            {
                return Ok(entries);
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for TurnEnd"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn approve(
        &self,
        interaction_id: &str,
        request_seq: u64,
    ) -> Result<InteractionDecisionOutcome> {
        let response = InteractionResponse {
            action: "approve".into(),
            data: serde_json::Map::new(),
            note: None,
        };
        self.session
            .decide_interaction(interaction_id, request_seq, &response)
            .await
    }

    pub async fn deny(
        &self,
        interaction_id: &str,
        request_seq: u64,
        reason: Option<&str>,
    ) -> Result<InteractionDecisionOutcome> {
        let response = InteractionResponse {
            action: "deny".into(),
            data: serde_json::Map::new(),
            note: reason.map(str::to_string),
        };
        self.session
            .decide_interaction(interaction_id, request_seq, &response)
            .await
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.daemon.abort();
        self.tool_server.abort();
    }
}

pub async fn child_prompt_id(harness: &Harness) -> Result<String> {
    let log =
        NatsSessionLog::new_with_replicas(harness.js.clone(), harness.session.storage_key(), 1);
    log.load_events_async()
        .await?
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::Message { id, role, .. } if role.is_user() => id.clone(),
            _ => None,
        })
        .context("user prompt id not found in child session log")
}

pub async fn discover_parent_tools(
    harness: &Harness,
) -> Result<harnx_runtime::nats_tool_provider::NatsToolProvider> {
    let mut parent_config = harness.config.read().clone();
    parent_config.session = Some(harnx_core::session::Session {
        id: "parent-waiter".into(),
        agent_name: Some("parent-agent".into()),
        ..Default::default()
    });
    parent_config.run_context = Some(
        harnx_runtime::nats_session_metadata::RunLimitsRecord::admit_root(
            Default::default(),
            Default::default(),
            chrono::Utc::now(),
            Default::default(),
            None,
            harnx_runtime::nats_session_metadata::CallTimeoutOverride::Omitted,
        )?,
    );
    let scope = std::env::var(harnx_core::instance::HARNX_SERVER_SCOPE)
        .map(ServerScope::from_string)
        .unwrap_or_else(|_| ServerScope::new());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if let Ok(provider) = harnx_runtime::nats_tool_provider::NatsToolProvider::discover(
            &parent_config,
            scope.clone(),
            harnx_runtime::nats_tool_provider::NatsInFlightCalls::for_instance(&scope),
            None,
        )
        .await
        {
            if provider.has_tool("session_wait")
                || provider.has_tool(&format!("{AGENT_NAME}_session_wait"))
            {
                return Ok(provider);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("subagent tools not discovered on NATS")
}

pub async fn call_subagent_tool(
    provider: &harnx_runtime::nats_tool_provider::NatsToolProvider,
    tool_suffix: &str,
    args: Value,
) -> Result<Value> {
    use harnx_core::tool::ToolProvider;
    let prefixed = format!("{AGENT_NAME}_{tool_suffix}");
    let tool = if provider.has_tool(&prefixed) {
        &prefixed
    } else if provider.has_tool(tool_suffix) {
        tool_suffix
    } else {
        &prefixed
    };
    provider
        .call_tool(tool, args, &harnx_core::abort::create_abort_signal())
        .await
        .map(|output| output.value)
        .map_err(|error| match error {
            harnx_core::tool::ToolError::Fatal(e) | harnx_core::tool::ToolError::Recoverable(e) => {
                e
            }
        })
}

pub async fn suspended_child_case(
    value: &str,
    call_id: &'static str,
) -> Result<(
    Harness,
    u64,
    String,
    String,
    harnx_runtime::nats_tool_provider::NatsToolProvider,
)> {
    let model = single_call_model("probe_run", json!({"value": value}), call_id);
    let (harness, seq, interaction, _) =
        Harness::start_and_suspend(BASH_CONFIRM, model, "start child turn").await?;
    let prompt = child_prompt_id(&harness).await?;
    let provider = discover_parent_tools(&harness).await?;
    Ok((harness, seq, interaction, prompt, provider))
}
