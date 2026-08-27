//! Detached NATS handoff lifecycle integration coverage.

mod common;

use anyhow::Result;
use common::{spawn_nats_server, NatsServerHandle};
use futures_util::StreamExt;
use harnx_core::{
    event::{AgentEvent, NullSink, SessionEvent, TurnEvent},
    message::{MessageContent, MessageRole},
    require_nextest,
    session::SessionLogEntry,
    tool::ToolCall,
};
use harnx_runtime::{
    client::CompletionTokenUsage,
    config::Config,
    nats_event_sink::SessionEventStream,
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{SessionInitializer, SessionMetadataStore},
    nats_worker::{notify_subject, run_worker_daemon, SessionActivate, WorkerDaemonConfig},
    utils::create_abort_signal,
    NatsSession, NatsSessionConfig, SessionActivationRoute,
};
use parking_lot::RwLock;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const CI_SAFE_TIMEOUT: Duration = Duration::from_secs(60);
const EXPLICIT_TARGET_ID: &str = "handoff-remote-session";
const OTHER_TARGET_ID: &str = "other-owned-session";

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

struct HandoffFixture {
    _server: NatsServerHandle,
    _root: tempfile::TempDir,
    _config_guard: EnvVarGuard,
    _data_guard: EnvVarGuard,
    _state_guard: EnvVarGuard,
    config: Arc<RwLock<Config>>,
    client: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
    daemon: tokio::task::JoinHandle<Result<()>>,
}

impl HandoffFixture {
    async fn start() -> Result<Option<Self>> {
        let Some(server) = spawn_nats_server().await? else {
            eprintln!("skipping: nats-server not available");
            return Ok(None);
        };
        let root = tempfile::tempdir()?;
        let config_dir = root.path().join("config");
        let data_dir = root.path().join("data");
        let state_dir = root.path().join("state");
        write_test_config(&config_dir, server.url())?;
        std::fs::create_dir_all(&data_dir)?;
        std::fs::create_dir_all(&state_dir)?;

        let config_guard = EnvVarGuard::set_path("HARNX_CONFIG_DIR", &config_dir);
        let data_guard = EnvVarGuard::set_path("HARNX_DATA_DIR", &data_dir);
        let state_guard = EnvVarGuard::set_path("HARNX_STATE_DIR", &state_dir);
        let config = Arc::new(RwLock::new(Config::load_from_file(
            &config_dir.join("config.yaml"),
        )?));
        let (client, jetstream) = {
            let snapshot = config.read().clone();
            (
                snapshot.nats_client("local").await?,
                snapshot.nats_jetstream("local").await?,
            )
        };
        SessionMetadataStore::ensure(&jetstream, 1).await?;
        let worker_config = WorkerDaemonConfig::managing("local", "worker-handoff");
        let worker_runtime = Arc::clone(&config);
        let daemon = tokio::spawn(async move {
            run_worker_daemon(worker_runtime, worker_config, Some(handoff_call_fn())).await
        });
        tokio::time::sleep(Duration::from_millis(500)).await;

        Ok(Some(Self {
            _server: server,
            _root: root,
            _config_guard: config_guard,
            _data_guard: data_guard,
            _state_guard: state_guard,
            config,
            client,
            jetstream,
            daemon,
        }))
    }

    async fn session(&self, agent: &str, session_id: &str) -> Result<NatsSession> {
        NatsSession::new(
            session_config(agent, Some(session_id)),
            self.client.clone(),
            self.jetstream.clone(),
            create_abort_signal(),
        )
        .await
    }

    async fn seed_destinations(&self) -> Result<NatsSessionLog> {
        let explicit_target = self.session("delegate-agent", EXPLICIT_TARGET_ID).await?;
        let explicit_log = NatsSessionLog::new(
            self.jetstream.clone(),
            explicit_target.session_id().to_string(),
        );
        append_prior_turn(&explicit_log).await?;
        self.session("other-agent", OTHER_TARGET_ID).await?;
        Ok(explicit_log)
    }

    async fn run_explicit_scenario(&self, explicit_log: &NatsSessionLog) -> Result<()> {
        let source = self
            .session("source-agent", "nats-handoff-explicit-root")
            .await?;
        let stream = self.source_stream(&source).await?;
        source
            .run_turn("explicit handoff", Arc::new(NullSink), None)
            .await?;
        let source_entries = self.log(&source).load_events_async().await?;
        assert_explicit_source(&source_entries);
        assert_explicit_events(observe_source_handoff(stream).await?);

        let entries = wait_for_handoff_target(explicit_log, "finish explicit work").await?;
        assert_handoff_target_log(&entries, "finish explicit work");
        assert!(entries.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text(text),
                ..
            } if text == "prior answer"
        )));
        Ok(())
    }

    async fn run_generated_scenario(&self) -> Result<()> {
        let source = self
            .session("source-agent", "nats-handoff-generated-root")
            .await?;
        let stream = self.source_stream(&source).await?;
        let activation_observer = self.observe_target_activation(&source).await?;
        source
            .run_turn("generated handoff", Arc::new(NullSink), None)
            .await?;
        let observed = observe_source_handoff(stream).await?;
        let (agent, target_id) = observed
            .committed
            .as_ref()
            .expect("generated handoff must commit a destination");
        assert_generated_events(&observed, agent, target_id);
        assert_eq!(
            tokio::time::timeout(CI_SAFE_TIMEOUT, activation_observer).await???,
            target_id.as_str()
        );
        let target_log = NatsSessionLog::new(self.jetstream.clone(), target_id);
        let entries = wait_for_handoff_target(&target_log, "finish generated work").await?;
        assert_handoff_target_log(&entries, "finish generated work");
        Ok(())
    }

    async fn run_ownership_mismatch_scenario(&self) -> Result<()> {
        let source = self
            .session("source-agent", "nats-handoff-mismatch-root")
            .await?;
        let stream = self.source_stream(&source).await?;
        let result = source
            .run_turn("ownership mismatch", Arc::new(NullSink), None)
            .await?;
        assert!(
            result.error.as_deref().is_some_and(|error| {
                error.contains("belongs to")
                    || error.contains("different agent")
                    || error.contains("identity mismatch")
            }),
            "ownership mismatch must fail the source turn: {:?}",
            result.error
        );
        let observed = observe_source_handoff(stream).await?;
        assert_eq!(
            observed.requested,
            Some((
                "delegate-agent".to_string(),
                Some(OTHER_TARGET_ID.to_string())
            ))
        );
        assert!(observed.committed.is_none());
        let entries = NatsSessionLog::new(self.jetstream.clone(), OTHER_TARGET_ID)
            .load_events_async()
            .await?;
        assert!(entries.is_empty());
        Ok(())
    }

    async fn source_stream(&self, session: &NatsSession) -> Result<SessionEventStream> {
        SessionEventStream::attach(
            self.jetstream.clone(),
            self.client.clone(),
            session.session_id(),
        )
        .await
    }

    fn log(&self, session: &NatsSession) -> NatsSessionLog {
        NatsSessionLog::new(self.jetstream.clone(), session.session_id())
    }

    async fn observe_target_activation(
        &self,
        source: &NatsSession,
    ) -> Result<tokio::task::JoinHandle<Result<String>>> {
        let subscriber = self.client.subscribe(notify_subject("local")).await?;
        self.client.flush().await?;
        Ok(spawn_activation_observer(
            subscriber,
            self.jetstream.clone(),
            source.session_id().to_string(),
        ))
    }
}

impl Drop for HandoffFixture {
    fn drop(&mut self) {
        self.daemon.abort();
    }
}

fn write_test_config(config_dir: &Path, nats_url: &str) -> Result<()> {
    std::fs::create_dir_all(config_dir.join("clients"))?;
    std::fs::create_dir_all(config_dir.join("nats_servers"))?;
    write_agent(
        config_dir,
        "source-agent",
        "---\nmodel: openai:test-model\nuse_tools: delegate-agent_session_handoff\n---\nSource instructions\n",
    )?;
    write_agent(
        config_dir,
        "delegate-agent",
        "---\nmodel: openai:test-model\n---\nTarget instructions\n",
    )?;
    write_agent(
        config_dir,
        "other-agent",
        "---\nmodel: openai:test-model\n---\nOther instructions\n",
    )?;
    std::fs::write(config_dir.join("config.yaml"), "model: openai:test-model\n")?;
    std::fs::write(
        config_dir.join("nats_servers/local.yaml"),
        format!("url: {nats_url}\n"),
    )?;
    std::fs::write(
        config_dir.join("clients/openai.yaml"),
        "type: openai\napi_key: sk-test\nmodels:\n  - name: test-model\n    type: chat\n    max_input_tokens: 4096\n",
    )?;
    Ok(())
}

fn write_agent(config_dir: &Path, name: &str, body: &str) -> Result<()> {
    let agents_dir = config_dir.join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::write(agents_dir.join(format!("{name}.md")), body)?;
    Ok(())
}

fn session_config(agent: &str, session_id: Option<&str>) -> NatsSessionConfig {
    NatsSessionConfig {
        cluster: "local".to_string(),
        initializer: SessionInitializer::named(agent, Default::default()),
        session_id: session_id.map(str::to_string),
        activation_route: SessionActivationRoute::ClusterShared,
    }
}

fn handoff_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |input, config, _abort| {
        let agent = config.read().extract_agent().name().to_string();
        let prompt = input.text().to_string();
        Box::pin(async move {
            if agent == "source-agent" {
                let (session_id, target_prompt) = match prompt.as_str() {
                    "explicit handoff" => (Some(EXPLICIT_TARGET_ID), "finish explicit work"),
                    "ownership mismatch" => (Some(OTHER_TARGET_ID), "must not be queued"),
                    _ => (None, "finish generated work"),
                };
                Ok((
                    "handoff requested".to_string(),
                    None,
                    vec![ToolCall::new(
                        "delegate-agent_session_handoff".to_string(),
                        json!({"prompt": target_prompt, "session_id": session_id}),
                        Some("handoff-call-1".to_string()),
                        None,
                    )],
                    CompletionTokenUsage::default(),
                ))
            } else {
                Ok((
                    format!("handoff completed: {prompt}"),
                    None,
                    vec![],
                    CompletionTokenUsage::default(),
                ))
            }
        })
    })
}

async fn append_prior_turn(log: &NatsSessionLog) -> Result<()> {
    log.append_event_async(&message("prior-user", MessageRole::User, "prior question"))
        .await?;
    log.append_event_async(&message(
        "prior-answer",
        MessageRole::Assistant,
        "prior answer",
    ))
    .await?;
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: 1,
        fence_token: 1,
        timestamp: None,
    })
    .await?;
    Ok(())
}

fn message(id: &str, role: MessageRole, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some(id.to_string()),
        role,
        content: MessageContent::Text(text.to_string()),
        timestamp: None,
        fence_token: None,
    }
}

#[derive(Default)]
struct ObservedHandoff {
    requested: Option<(String, Option<String>)>,
    committed: Option<(String, String)>,
    order: Vec<&'static str>,
}

async fn observe_source_handoff(mut stream: SessionEventStream) -> Result<ObservedHandoff> {
    let deadline = tokio::time::Instant::now() + CI_SAFE_TIMEOUT;
    let mut observed = ObservedHandoff::default();
    loop {
        let envelope = tokio::time::timeout_at(deadline, stream.next())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for source handoff events"))?
            .ok_or_else(|| anyhow::anyhow!("source handoff event stream closed"))?;
        match envelope.event {
            AgentEvent::Turn(TurnEvent::HandoffRequested { agent, session_id }) => {
                observed.order.push("requested");
                observed.requested = Some((agent, session_id));
            }
            AgentEvent::Session(SessionEvent::HandoffCommitted { agent, session_id }) => {
                observed.order.push("committed");
                observed.committed = Some((agent, session_id));
            }
            AgentEvent::Turn(TurnEvent::Ended { .. }) => {
                observed.order.push("ended");
                return Ok(observed);
            }
            _ => {}
        }
    }
}

fn assert_explicit_source(entries: &[(u64, SessionLogEntry)]) {
    assert!(entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::ToolResults { results, .. }
            if results.iter().any(|result| result.switch_agent.as_ref().is_some_and(|switch| {
                switch.agent == "delegate-agent"
                    && switch.session_id.as_deref() == Some(EXPLICIT_TARGET_ID)
            }))
    )));
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { .. }))
            .count(),
        1,
        "source handoff must execute once: {entries:?}"
    );
}

fn assert_explicit_events(observed: ObservedHandoff) {
    assert_eq!(
        observed.requested,
        Some((
            "delegate-agent".to_string(),
            Some(EXPLICIT_TARGET_ID.to_string())
        ))
    );
    assert_eq!(
        observed.committed,
        Some((
            "delegate-agent@local".to_string(),
            EXPLICIT_TARGET_ID.to_string()
        ))
    );
    assert_eq!(observed.order, ["requested", "committed", "ended"]);
}

fn assert_generated_events(observed: &ObservedHandoff, agent: &str, target_id: &str) {
    assert_eq!(agent, "delegate-agent@local");
    assert!(!target_id.trim().is_empty());
    assert_ne!(target_id, "nats-handoff-generated-root");
    assert_eq!(
        observed.requested,
        Some(("delegate-agent".to_string(), None))
    );
    assert_eq!(observed.order, ["requested", "committed", "ended"]);
}

fn spawn_activation_observer(
    mut subscriber: async_nats::Subscriber,
    jetstream: async_nats::jetstream::Context,
    source_id: String,
) -> tokio::task::JoinHandle<Result<String>> {
    tokio::spawn(async move {
        loop {
            let message = subscriber
                .next()
                .await
                .ok_or_else(|| anyhow::anyhow!("activation subscription closed"))?;
            let activation: SessionActivate = serde_json::from_slice(&message.payload)?;
            if activation.session_id == source_id {
                continue;
            }
            let entries = NatsSessionLog::new(jetstream.clone(), &activation.session_id)
                .load_events_async()
                .await?;
            anyhow::ensure!(
                entries.iter().any(|(_, entry)| matches!(
                    entry,
                    SessionLogEntry::Message {
                        role: MessageRole::User,
                        content: MessageContent::Text(text),
                        ..
                    } if text == "finish generated work"
                )),
                "target activation overtook its durable handoff prompt"
            );
            return Ok(activation.session_id);
        }
    })
}

async fn wait_for_handoff_target(
    log: &NatsSessionLog,
    expected_prompt: &str,
) -> Result<Vec<(u64, SessionLogEntry)>> {
    let deadline = tokio::time::Instant::now() + CI_SAFE_TIMEOUT;
    loop {
        let entries = log.load_events_async().await?;
        if target_completed(&entries, expected_prompt) {
            return Ok(entries);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("handoff target did not finish within {CI_SAFE_TIMEOUT:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn target_completed(entries: &[(u64, SessionLogEntry)], expected_prompt: &str) -> bool {
    let has_reply = entries.iter().any(|(_, entry)| {
        matches!(
            entry,
            SessionLogEntry::Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text(text),
                ..
            } if text.contains("handoff completed")
        )
    });
    let has_prompt = entries.iter().any(|(_, entry)| {
        matches!(
            entry,
            SessionLogEntry::Message {
                role: MessageRole::User,
                content: MessageContent::Text(text),
                ..
            } if text == expected_prompt
        )
    });
    has_reply && has_prompt
}

fn assert_handoff_target_log(entries: &[(u64, SessionLogEntry)], expected_prompt: &str) {
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(
                entry,
                SessionLogEntry::Message {
                    role: MessageRole::User,
                    content: MessageContent::Text(text),
                    ..
                } if text == expected_prompt
            ))
            .count(),
        1,
        "expected exactly one queued handoff prompt: {entries:?}"
    );
    assert!(target_completed(entries, expected_prompt));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoffs_queue_top_level_sessions_preserve_history_and_validate_ownership() -> Result<()> {
    require_nextest();
    let Some(fixture) = HandoffFixture::start().await? else {
        return Ok(());
    };
    let explicit_log = fixture.seed_destinations().await?;
    fixture.run_explicit_scenario(&explicit_log).await?;
    fixture.run_generated_scenario().await?;
    fixture.run_ownership_mismatch_scenario().await?;
    assert!(fixture.config.read().session.is_none());
    assert!(fixture.config.read().agent.is_none());
    Ok(())
}
