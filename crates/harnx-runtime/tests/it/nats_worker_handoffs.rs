//! Detached NATS handoff lifecycle integration coverage.

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
    config: harnx_runtime::config::GlobalConfig,
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
        let worker_config = WorkerDaemonConfig::managing("local", "worker-handoff");
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
        let source_prompt = source_entries
            .iter()
            .find_map(|(_, entry)| match entry {
                SessionLogEntry::Message {
                    id: Some(id), role, ..
                } if role.is_user() => Some(id),
                _ => None,
            })
            .unwrap();
        let source_admission = store
            .prompt_admission(source.storage_key(), source_prompt)
            .await?
            .unwrap();
        let source_limits = store
            .get_invocation_limits(
                source.storage_key(),
                source_admission.invocation_id.as_str(),
            )
            .await?
            .unwrap();
        let target_prompt = entries
            .iter()
            .rev()
            .find_map(|(_, entry)| match entry {
                SessionLogEntry::Message {
                    id: Some(id), role, ..
                } if role.is_user() => Some(id),
                _ => None,
            })
            .unwrap();
        let target_admission = store
            .prompt_admission(explicit_log.storage_key(), target_prompt)
            .await?
            .unwrap();
        let target_limits = store
            .get_invocation_limits(
                explicit_log.storage_key(),
                target_admission.invocation_id.as_str(),
            )
            .await?
            .unwrap();
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
        assert_eq!(
            target_limits.parent_invocation.as_ref().unwrap().edge_kind,
            harnx_runtime::nats_session_metadata::InvocationEdgeKind::Handoff
        );
        assert_eq!(
            source_limits.deadline,
            Some(source_admission.admitted_at + chrono::Duration::seconds(30))
        );

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
        let observed = observe_source_handoff(stream).await?;
        let (agent, target_id, _) = observed
            .committed
            .as_ref()
            .expect("generated handoff must commit a destination");
        assert_generated_events(&observed, agent, target_id);
        assert_eq!(
            tokio::time::timeout(CI_SAFE_TIMEOUT, activation_observer).await???,
            harnx_core::session_identity::session_key(Some("delegate-agent"), target_id)
        );
        let target_log =
            NatsSessionLog::for_agent(self.jetstream.clone(), "delegate-agent", target_id);
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
                "delegate-agent".to_string(),
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
                let (session_id, target_prompt) = if let Some(id) =
                    prompt.strip_prefix("identity-explicit:")
                {
                    (Some(id), "finish generated work")
                } else {
                    match prompt.as_str() {
                        "explicit handoff" => (Some(EXPLICIT_TARGET_ID), "finish explicit work"),
                        "ownership mismatch" => (Some(OTHER_TARGET_ID), "finish reused-ID work"),
                        _ => (None, "finish generated work"),
                    }
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

fn assert_explicit_events(observed: ObservedHandoff, durable_handoff_seq: u64) {
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
            EXPLICIT_TARGET_ID.to_string(),
            Some(durable_handoff_seq),
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

struct IdentityInheritanceCase {
    case: &'static str,
    source_user: Option<&'static str>,
    inherit: bool,
    expected: &'static str,
}

fn identity_inheritance_cases() -> [IdentityInheritanceCase; 5] {
    [
        IdentityInheritanceCase {
            case: "owned",
            source_user: Some("source-owner"),
            inherit: true,
            expected: "source-owner",
        },
        IdentityInheritanceCase {
            case: "anonymous",
            source_user: None,
            inherit: true,
            expected: "destination-default",
        },
        IdentityInheritanceCase {
            case: "blank",
            source_user: Some(" \t"),
            inherit: true,
            expected: "destination-default",
        },
        IdentityInheritanceCase {
            case: "private",
            source_user: Some("private-owner"),
            inherit: false,
            expected: "destination-default",
        },
        IdentityInheritanceCase {
            case: "existing",
            source_user: Some("source-owner"),
            inherit: true,
            expected: "existing-owner",
        },
    ]
}

async fn seed_existing_target_metadata(
    store: &SessionMetadataStore,
    target_id: &str,
) -> Result<()> {
    store
        .create(&harnx_runtime::nats_session_metadata::SessionMetadata::new(
            target_id,
            SessionInitializer::named("delegate-agent", Default::default())
                .with_user_id("existing-owner"),
        ))
        .await?
        .expect("seed existing target");
    Ok(())
}

async fn setup_identity_source_session(
    fixture: &HandoffFixture,
    source_id: &str,
    source_user: Option<&str>,
    inherit: bool,
) -> Result<NatsSession> {
    let mut source_config = session_config("source-agent", Some(source_id));
    if let Some(user) = source_user {
        source_config.initializer =
            source_config
                .initializer
                .with_properties(serde_json::from_value(json!({
                    "user_id": {"value": user, "inherit": inherit},
                    "git_branch": {"value": "source-only", "inherit": true}
                }))?);
    }
    // Raw creation preserves legacy blank properties so the inheritance
    // path, not just with_user_id(), must normalize them before defaults.
    NatsSession::new(
        source_config,
        fixture.client.clone(),
        fixture.jetstream.clone(),
        create_abort_signal(),
    )
    .await
}

async fn execute_identity_handoff_turn(
    fixture: &HandoffFixture,
    source: NatsSession,
    explicit: bool,
    target_id: &str,
) -> Result<String> {
    let stream = fixture.source_stream(&source).await?;
    let prompt = if explicit {
        format!("identity-explicit:{target_id}")
    } else {
        "generated handoff".to_string()
    };
    source
        .with_external_admission()
        .run_turn(&prompt, Arc::new(NullSink), None)
        .await?;
    let observed = observe_source_handoff(stream).await?;
    let (_, committed_id, _) = observed.committed.expect("handoff committed");
    if explicit {
        assert_eq!(committed_id, target_id);
    }
    Ok(committed_id)
}

struct HandoffTarget<'a> {
    committed_id: &'a str,
    expected: &'a str,
    case: &'a str,
    explicit: bool,
}

async fn assert_identity_handoff_target(
    fixture: &HandoffFixture,
    store: &SessionMetadataStore,
    target: HandoffTarget<'_>,
) -> Result<()> {
    let HandoffTarget {
        committed_id,
        expected,
        case,
        explicit,
    } = target;
    let target_log =
        NatsSessionLog::for_agent(fixture.jetstream.clone(), "delegate-agent", committed_id);
    wait_for_handoff_target(&target_log, "finish generated work").await?;
    let record = store
        .get_for_agent(committed_id, "delegate-agent")
        .await?
        .expect("target metadata");
    let properties = harnx_runtime::nats_session_metadata::session_properties(&record.metadata)?;
    assert_eq!(
        properties.text("user_id"),
        Some(expected),
        "case={case}, explicit={explicit}"
    );
    assert_ne!(properties.text("git_branch"), Some("source-only"));
    Ok(())
}

async fn run_identity_inheritance_case(
    fixture: &HandoffFixture,
    store: &SessionMetadataStore,
    explicit: bool,
    test_case: &IdentityInheritanceCase,
) -> Result<()> {
    let source_id = format!("identity-{}-{explicit}", test_case.case);
    let target_id = format!("target-{}-{explicit}", test_case.case);
    if test_case.case == "existing" {
        seed_existing_target_metadata(store, &target_id).await?;
    }
    let source = setup_identity_source_session(
        fixture,
        &source_id,
        test_case.source_user,
        test_case.inherit,
    )
    .await?;
    let committed_id = execute_identity_handoff_turn(fixture, source, explicit, &target_id).await?;
    assert_identity_handoff_target(
        fixture,
        store,
        HandoffTarget {
            committed_id: &committed_id,
            expected: test_case.expected,
            case: test_case.case,
            explicit,
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoffs_inherit_nonblank_user_identity_before_destination_defaults() -> Result<()> {
    require_nextest();
    let Some(fixture) = HandoffFixture::start().await? else {
        return Ok(());
    };
    let store = SessionMetadataStore::ensure(&fixture.jetstream, 1).await?;
    for explicit in [false, true] {
        for test_case in identity_inheritance_cases() {
            if test_case.case == "existing" && !explicit {
                continue;
            }
            run_identity_inheritance_case(&fixture, &store, explicit, &test_case).await?;
        }
    }
    Ok(())
}
