//! Detached NATS handoff lifecycle integration coverage.

mod identity;
mod provider;
mod transcript;

use crate::common;

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
use harnx_runtime::config::ConfigLock;
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
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
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

fn find_user_prompt_id(entries: &[(u64, SessionLogEntry)], from_end: bool) -> &str {
    fn extract_user_id(entry: &SessionLogEntry) -> Option<&str> {
        match entry {
            SessionLogEntry::Message {
                id: Some(id),
                role,
                content,
                ..
            } if role.is_user()
                && !harnx_core::timing_identity::is_timing_notice(&content.to_text()) =>
            {
                Some(id.as_str())
            }
            _ => None,
        }
    }
    if from_end {
        entries
            .iter()
            .rev()
            .find_map(|(_, entry)| extract_user_id(entry))
            .unwrap()
    } else {
        entries
            .iter()
            .find_map(|(_, entry)| extract_user_id(entry))
            .unwrap()
    }
}

struct HandoffEndpoint<'a> {
    storage_key: &'a str,
    prompt_id: &'a str,
}

async fn fetch_invocation_limits(
    store: &SessionMetadataStore,
    endpoint: &HandoffEndpoint<'_>,
) -> Result<(
    harnx_runtime::nats_session_metadata::InvocationAdmission,
    harnx_runtime::nats_session_metadata::RunLimitsRecord,
)> {
    let admission = store
        .prompt_admission(endpoint.storage_key, endpoint.prompt_id)
        .await?
        .unwrap();
    let limits = store
        .get_invocation_limits(endpoint.storage_key, admission.invocation_id.as_str())
        .await?
        .unwrap();
    Ok((admission, limits))
}

fn assert_handoff_run_and_parent(
    target_limits: &harnx_runtime::nats_session_metadata::RunLimitsRecord,
    source_limits: &harnx_runtime::nats_session_metadata::RunLimitsRecord,
) {
    assert_eq!(target_limits.run_id, source_limits.run_id);
    assert_eq!(target_limits.deadline, source_limits.deadline);
    assert_eq!(
        target_limits
            .parent_invocation
            .as_ref()
            .unwrap()
            .invocation_id,
        source_limits.invocation_id
    );
}

fn assert_handoff_edge_and_source_deadline(
    target_limits: &harnx_runtime::nats_session_metadata::RunLimitsRecord,
    source_limits: &harnx_runtime::nats_session_metadata::RunLimitsRecord,
    source_admitted_at: chrono::DateTime<chrono::Utc>,
) {
    assert_eq!(
        target_limits.parent_invocation.as_ref().unwrap().edge_kind,
        harnx_runtime::nats_session_metadata::InvocationEdgeKind::Handoff
    );
    assert_eq!(
        source_limits.deadline,
        Some(source_admitted_at + chrono::Duration::seconds(30))
    );
}

async fn assert_handoff_invocation_limits(
    store: &SessionMetadataStore,
    source: HandoffEndpoint<'_>,
    target: HandoffEndpoint<'_>,
) -> Result<()> {
    let (source_admission, source_limits) = fetch_invocation_limits(store, &source).await?;
    let (_target_admission, target_limits) = fetch_invocation_limits(store, &target).await?;

    assert_handoff_run_and_parent(&target_limits, &source_limits);
    assert_handoff_edge_and_source_deadline(
        &target_limits,
        &source_limits,
        source_admission.admitted_at,
    );
    Ok(())
}

struct HandoffFixture {
    _server: NatsServerHandle,
    _root: tempfile::TempDir,
    _config_guard: EnvVarGuard,
    _data_guard: EnvVarGuard,
    _state_guard: EnvVarGuard,
    config: harnx_runtime::config::GlobalConfig,
    client: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
    daemon: tokio::task::JoinHandle<Result<()>>,
    _provider: Option<tokio_util::task::AbortOnDropHandle<Result<()>>>,
    _provider_env: Vec<EnvVarGuard>,
}

impl HandoffFixture {
    async fn start() -> Result<Option<Self>> {
        Self::start_with_provider(false).await
    }

    async fn start_with_provider(provider: bool) -> Result<Option<Self>> {
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

        let config_guard = EnvVarGuard::set("HARNX_CONFIG_DIR", &config_dir);
        let data_guard = EnvVarGuard::set("HARNX_DATA_DIR", &data_dir);
        let state_guard = EnvVarGuard::set("HARNX_STATE_DIR", &state_dir);
        let provider_env = if provider {
            vec![
                EnvVarGuard::set("HARNX_NATS_URL", server.url()),
                EnvVarGuard::set("HARNX_NATS_TOKEN", "provider-test"),
            ]
        } else {
            vec![]
        };
        let config = Arc::new(ConfigLock::new(Config::load_from_file(
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
        let mut provider_env = provider_env;
        let tool_server = if provider {
            Some(provider::start_tool_server(&client, &jetstream, &mut provider_env).await?)
        } else {
            None
        };
        let worker_config = if provider {
            WorkerDaemonConfig::new("local", "worker-handoff")
        } else {
            WorkerDaemonConfig::managing("local", "worker-handoff")
        };
        let worker_runtime = Arc::clone(&config);
        let daemon = tokio::spawn(async move {
            run_worker_daemon(worker_runtime, worker_config, Some(handoff_call_fn()), None).await
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
            _provider: tool_server,
            _provider_env: provider_env,
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
        let explicit_log = NatsSessionLog::new_with_replicas(
            self.jetstream.clone(),
            explicit_target.storage_key().to_string(),
            1,
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
            .clone()
            .with_external_admission()
            .with_admission_timeout(Some(30))
            .run_turn("explicit handoff", Arc::new(NullSink), None)
            .await?;
        let source_entries = self.log(&source).load_events_async().await?;
        assert_explicit_source(&source_entries);
        assert_durable_handoff_target(&source_entries, Some(EXPLICIT_TARGET_ID));
        let handoff_seq = source_entries
            .iter()
            .find_map(|(seq, entry)| {
                matches!(entry, SessionLogEntry::HandoffCommitted { .. }).then_some(*seq)
            })
            .expect("source log must contain durable handoff commit");
        assert_explicit_events(observe_source_handoff(stream).await?, handoff_seq);

        let entries = wait_for_handoff_target(explicit_log, "finish explicit work").await?;
        assert_handoff_target_log(&entries, "finish explicit work");
        let store =
            harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&self.jetstream, 1)
                .await?;
        let source_prompt = find_user_prompt_id(&source_entries, false);
        let target_prompt = find_user_prompt_id(&entries, true);
        assert_handoff_invocation_limits(
            &store,
            HandoffEndpoint {
                storage_key: source.storage_key(),
                prompt_id: source_prompt,
            },
            HandoffEndpoint {
                storage_key: explicit_log.storage_key(),
                prompt_id: target_prompt,
            },
        )
        .await?;

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
            .clone()
            .with_external_admission()
            .run_turn("generated handoff", Arc::new(NullSink), None)
            .await?;
        let source_entries = self.log(&source).load_events_async().await?;
        let persisted_target_id = assert_durable_handoff_target(&source_entries, None);
        let observed = observe_source_handoff(stream).await?;
        let (agent, target_id, _) = observed
            .committed
            .as_ref()
            .expect("generated handoff must commit a destination");
        assert_generated_events(&observed, agent, target_id);
        assert_eq!(persisted_target_id, *target_id);
        assert_eq!(
            tokio::time::timeout(CI_SAFE_TIMEOUT, activation_observer).await???,
            harnx_core::session_identity::session_key(Some("delegate-agent"), target_id)
        );
        let target_log = NatsSessionLog::for_agent(
            self.jetstream.clone(),
            "delegate-agent",
            &persisted_target_id,
        );
        let entries = wait_for_handoff_target(&target_log, "finish generated work").await?;
        assert_handoff_target_log(&entries, "finish generated work");
        Ok(())
    }

    async fn run_agent_scoped_id_reuse_scenario(&self) -> Result<()> {
        let source = self
            .session("source-agent", "nats-handoff-mismatch-root")
            .await?;
        let stream = self.source_stream(&source).await?;
        let result = source
            .clone()
            .with_external_admission()
            .run_turn("ownership mismatch", Arc::new(NullSink), None)
            .await?;
        assert!(
            result.error.is_none(),
            "another agent may reuse the same local ID: {:?}",
            result.error
        );
        let observed = observe_source_handoff(stream).await?;
        assert_eq!(
            observed.requested,
            Some((
                "delegate-agent@local".to_string(),
                Some(OTHER_TARGET_ID.to_string())
            ))
        );
        assert_eq!(
            observed.committed.as_ref().map(|(_, id, _)| id.as_str()),
            Some(OTHER_TARGET_ID)
        );
        let target_log =
            NatsSessionLog::for_agent(self.jetstream.clone(), "delegate-agent", OTHER_TARGET_ID);
        let target_entries = wait_for_handoff_target(&target_log, "finish reused-ID work").await?;
        assert_handoff_target_log(&target_entries, "finish reused-ID work");
        let entries =
            NatsSessionLog::for_agent(self.jetstream.clone(), "other-agent", OTHER_TARGET_ID)
                .load_events_async()
                .await?;
        assert!(entries.is_empty());
        Ok(())
    }

    async fn source_stream(&self, session: &NatsSession) -> Result<SessionEventStream> {
        SessionEventStream::attach(
            self.jetstream.clone(),
            self.client.clone(),
            session.storage_key(),
        )
        .await
    }

    fn log(&self, session: &NatsSession) -> NatsSessionLog {
        NatsSessionLog::new_with_replicas(self.jetstream.clone(), session.storage_key(), 1)
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
            source.storage_key().to_string(),
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
        "---\nmodel: openai:test-model\nuse_tools: delegate-agent_session_handoff,fixture_handoff\n---\nSource instructions\n",
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
    std::fs::write(
        config_dir.join("config.yaml"),
        "model: openai:test-model\nuser_id: global-default\n",
    )?;
    std::fs::write(
        config_dir.join("nats_servers/local.yaml"),
        format!("url: {nats_url}\nuser_id: destination-default\n"),
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
                Ok((
                    "handoff requested".to_string(),
                    None,
                    vec![source_handoff_call(&prompt)],
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

fn handoff_prompt(prompt: &str) -> (Option<&str>, &str) {
    if let Some(id) = prompt.strip_prefix("identity-explicit:") {
        (Some(id), "finish generated work")
    } else {
        match prompt {
            "explicit handoff" | "provider explicit" => {
                (Some(EXPLICIT_TARGET_ID), "finish explicit work")
            }
            "ownership mismatch" => (Some(OTHER_TARGET_ID), "finish reused-ID work"),
            _ => (None, "finish generated work"),
        }
    }
}

fn source_handoff_call(prompt: &str) -> ToolCall {
    let (session_id, target_prompt) = handoff_prompt(prompt);
    let provider = prompt.starts_with("provider");
    let mut arguments = json!({"prompt": target_prompt, "session_id": session_id});
    if provider {
        arguments["action"] = json!("switch_agent");
        arguments["agent"] = json!("delegate-agent");
        if session_id.is_none() {
            arguments.as_object_mut().unwrap().remove("session_id");
        }
    }
    ToolCall::new(
        if provider {
            "fixture_handoff"
        } else {
            "delegate-agent_session_handoff"
        }
        .to_string(),
        arguments,
        Some("handoff-call-1".to_string()),
        None,
    )
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
        usage: None,
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
    committed: Option<(String, String, Option<u64>)>,
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
            AgentEvent::Session(SessionEvent::HandoffCommitted {
                agent,
                session_id,
                after_seq,
                ..
            }) => {
                observed.order.push("committed");
                observed.committed = Some((agent, session_id, after_seq));
            }
            AgentEvent::Turn(TurnEvent::Ended { .. }) => {
                observed.order.push("ended");
                return Ok(observed);
            }
            _ => {}
        }
    }
}

/// Compare persisted JSON, switch data and the durable commit before following
/// the marker's ID to the target log that contains the child's completed turn.
fn assert_durable_handoff_target(
    entries: &[(u64, SessionLogEntry)],
    requested_id: Option<&str>,
) -> String {
    let (commit_seq, target_agent, target_session_id) = entries
        .iter()
        .find_map(|(seq, entry)| match entry {
            SessionLogEntry::HandoffCommitted {
                target_agent,
                target_session_id,
                handoff_tool_call_id,
                ..
            } => {
                assert_eq!(handoff_tool_call_id.as_deref(), Some("handoff-call-1"));
                Some((*seq, target_agent, target_session_id))
            }
            _ => None,
        })
        .expect("parent has durable handoff commit");
    let results: Vec<_> = entries
        .iter()
        .flat_map(|(seq, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => results
                .iter()
                .filter(|result| result.id.as_deref() == Some("handoff-call-1"))
                .map(|result| (*seq, result))
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(results.len(), 1, "handoff result is persisted exactly once");
    let (result_seq, result) = results[0];
    assert!(
        result_seq < commit_seq,
        "result is durable before dispatch commits"
    );
    assert_eq!(
        result.output["handoff"],
        json!({
            "agent": target_agent,
            "session_id": target_session_id,
        })
    );
    assert_eq!(result.output["session_id"], *target_session_id);
    let switch = result.switch_agent.as_ref().expect("handoff switch data");
    assert_eq!(switch.agent, *target_agent);
    assert_eq!(
        switch.session_id.as_deref(),
        Some(target_session_id.as_str())
    );
    let text = result.output["content"][0]["text"]
        .as_str()
        .expect("handoff text");
    assert!(text.contains(target_agent));
    assert!(text.contains(target_session_id));
    if let Some(requested_id) = requested_id {
        assert_eq!(target_session_id, requested_id, "caller ID is preserved");
    }
    target_session_id.clone()
}

fn assert_explicit_source(entries: &[(u64, SessionLogEntry)]) {
    assert!(entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::ToolResults { results, .. }
            if results.iter().any(|result| result.switch_agent.as_ref().is_some_and(|switch| {
                switch.agent == "delegate-agent@local"
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

fn assert_explicit_events(observed: ObservedHandoff, durable_handoff_seq: u64) {
    assert_eq!(
        observed.requested,
        Some((
            "delegate-agent@local".to_string(),
            Some(EXPLICIT_TARGET_ID.to_string())
        ))
    );
    assert_eq!(
        observed.committed,
        Some((
            "delegate-agent@local".to_string(),
            EXPLICIT_TARGET_ID.to_string(),
            Some(durable_handoff_seq),
        ))
    );
    assert_eq!(observed.order, ["requested", "committed", "ended"]);
}

fn assert_generated_target_identity(agent: &str, target_id: &str) {
    assert_eq!(agent, "delegate-agent@local");
    assert!(!target_id.trim().is_empty());
    assert_ne!(target_id, "nats-handoff-generated-root");
}

fn assert_observed_handoff_sequence(observed: &ObservedHandoff) {
    assert_eq!(
        observed.requested,
        observed
            .committed
            .as_ref()
            .map(|(agent, id, _)| (agent.clone(), Some(id.clone())))
    );
    assert_eq!(observed.order, ["requested", "committed", "ended"]);
}

fn assert_generated_events(observed: &ObservedHandoff, agent: &str, target_id: &str) {
    assert_generated_target_identity(agent, target_id);
    assert_observed_handoff_sequence(observed);
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
            let entries =
                NatsSessionLog::new_with_replicas(jetstream.clone(), &activation.session_id, 1)
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
    assert_eq!(entries.iter().filter(|(_, entry)| matches!(entry,
        SessionLogEntry::Message { role, content, .. }
            if role.is_user() && harnx_core::timing_identity::is_timing_notice(&content.to_text())
    )).count(), 1, "target persists its own initial timing notice");
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
async fn handoffs_queue_top_level_sessions_preserve_history_and_scope_ids_by_agent() -> Result<()> {
    require_nextest();
    let Some(fixture) = HandoffFixture::start().await? else {
        return Ok(());
    };
    let explicit_log = fixture.seed_destinations().await?;
    fixture.run_explicit_scenario(&explicit_log).await?;
    fixture.run_generated_scenario().await?;
    fixture.run_agent_scoped_id_reuse_scenario().await?;
    assert!(fixture.config.read().session.is_none());
    assert!(fixture.config.read().agent.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_handoff_result_links_to_committed_child_session() -> Result<()> {
    require_nextest();
    let Some(fixture) = HandoffFixture::start().await? else {
        return Ok(());
    };
    fixture.run_generated_scenario().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_handoff_result_preserves_caller_child_session_id() -> Result<()> {
    require_nextest();
    let Some(fixture) = HandoffFixture::start().await? else {
        return Ok(());
    };
    let explicit_log = fixture.seed_destinations().await?;
    fixture.run_explicit_scenario(&explicit_log).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoffs_inherit_nonblank_user_identity_before_destination_defaults() -> Result<()> {
    identity::run_scenario().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_handoff_without_session_id_reserves_and_dispatches() -> Result<()> {
    provider::run_provider_handoff(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_handoff_preserves_supplied_session_id() -> Result<()> {
    provider::run_provider_handoff(true).await
}
