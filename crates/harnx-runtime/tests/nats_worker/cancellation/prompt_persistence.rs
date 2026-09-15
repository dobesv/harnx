//! Real frontend admission, worker execution and a barrier-held tool handler.
use super::*;
use anyhow::Context;
use futures_util::StreamExt;
use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use harnx_execution_control::{OperationRef, OperationState};
use harnx_nats_common::{connect::NatsConnection, registry};
use harnx_runtime::nats_session::AppendedPrompt;
use harnx_toolset::{CancellationGuarantee, ToolInvokeError, ToolSpec, Toolset};
use harnx_toolset_server::{
    registration_key, serve_with_client_and_identity, TOOL_REGISTRY_BUCKET,
};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

const SESSION_ID: &str = "cancel-tool-input-history";
const PROMPT: &str = "cancel me during the tool";
const NEXT_PROMPT: &str = "new generation after cancel";

#[derive(Default)]
struct PausedTool {
    entered: Notify,
    release: Notify,
    returned: Notify,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Toolset for PausedTool {
    fn name(&self) -> &str {
        "paused"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "wait".into(),
            description: "Hold a tool handler at a test barrier".into(),
            input_schema: json!({"type": "object"}),
            idempotent_hint: true,
            read_only_hint: true,
            cancellation_guarantee: CancellationGuarantee::Cooperative,
            timeout_secs: None,
            meta: None,
        }]
    }

    async fn invoke(
        &self,
        tool: &str,
        _args: serde_json::Value,
        _cancel: CancellationToken,
    ) -> std::result::Result<serde_json::Value, ToolInvokeError> {
        assert_eq!(tool, "wait");
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        // Ignore cancellation until released: the returned success must lose
        // to durable stop, rather than hiding the race by dropping the future.
        self.release.notified().await;
        self.returned.notify_one();
        Ok(json!({"content": [{"type": "text", "text": "late tool success"}]}))
    }
}

struct EnvGuard(&'static str, Option<std::ffi::OsString>);

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        // Nextest gives this fixture its own process.
        unsafe { std::env::set_var(key, value) };
        Self(key, previous)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.1 {
            Some(value) => unsafe { std::env::set_var(self.0, value) },
            None => unsafe { std::env::remove_var(self.0) },
        }
    }
}

struct Fixture {
    session: NatsSession,
    js: async_nats::jetstream::Context,
    config: Arc<RwLock<Config>>,
    tool: Arc<PausedTool>,
    model_calls: Arc<AtomicUsize>,
    daemon: AbortOnDropHandle<Result<()>>,
    _tool_server: AbortOnDropHandle<Result<()>>,
    _environment: [EnvGuard; 3],
    server: common::NatsServerHandle,
}

impl Fixture {
    async fn start() -> Result<Self> {
        let server = require_nats_server()
            .await?
            .context("nats-server required")?;
        let client = async_nats::connect(server.url()).await?;
        let js = async_nats::jetstream::new(client.clone());
        let scope = ServerScope::new();
        let environment = [
            EnvGuard::set(HARNX_SERVER_SCOPE, scope.as_str()),
            EnvGuard::set("HARNX_NATS_URL", server.url()),
            EnvGuard::set("HARNX_NATS_TOKEN", ""),
        ];
        let tool = Arc::new(PausedTool::default());
        let tool_server = start_tool_server(&js, client.clone(), scope, tool.clone()).await?;
        let config = local_nats_runtime_config(server.url());
        let model_calls = Arc::new(AtomicUsize::new(0));
        let daemon = AbortOnDropHandle::new(tokio::spawn(run_worker_daemon(
            config.clone(),
            WorkerDaemonConfig::new("local", "input-history-worker"),
            Some(model(model_calls.clone())),
            None,
        )));
        let session = NatsSession::new(
            NatsSessionConfig {
                cluster: "local".into(),
                initializer: SessionInitializer::inline(
                    "",
                    Default::default(),
                    SessionOverrides {
                        use_tools: Some(vec!["paused_wait".into()]),
                        ..Default::default()
                    },
                ),
                session_id: Some(SESSION_ID.into()),
                activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
            },
            client,
            js.clone(),
            create_abort_signal(),
        )
        .await?;
        Ok(Self {
            session,
            js,
            config,
            tool,
            model_calls,
            daemon,
            _tool_server: tool_server,
            _environment: environment,
            server,
        })
    }

    async fn entries(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
        harnx_runtime::nats_session_log::NatsSessionLog::new(
            self.js.clone(),
            self.session.storage_key(),
        )
        .load_events_latest_async()
        .await
    }

    async fn ownership(&self, admitted: &AppendedPrompt) -> Result<()> {
        let reference = OperationRef::new(self.session.storage_key(), admitted.execution_id());
        let operation = self
            .session
            .execution_store()
            .recovery_history(self.session.storage_key())
            .await?
            .into_iter()
            .find(|history| history.reference == reference)
            .context("original generation ownership retained after physical pruning")?;
        let entries = self.entries().await?;
        let (seq, id) = entries
            .iter()
            .find_map(|(seq, entry)| match entry {
                SessionLogEntry::Message {
                    id: Some(id),
                    role: MessageRole::User,
                    content,
                    ..
                } if content.to_text() == PROMPT => Some((*seq, id)),
                _ => None,
            })
            .context("opening input retained")?;
        assert_eq!(operation.admissions.get(id), Some(&Some(seq)));
        Ok(())
    }
}

async fn start_tool_server(
    js: &async_nats::jetstream::Context,
    client: async_nats::Client,
    scope: ServerScope,
    tool: Arc<PausedTool>,
) -> Result<AbortOnDropHandle<Result<()>>> {
    let registry =
        registry::ensure_bucket_with_ttl(js, TOOL_REGISTRY_BUCKET, registry::REGISTRATION_TTL, 1)
            .await?;
    let key = registration_key(&scope, "____paused");
    let mut registered = registry.watch(&key).await?;
    let server = AbortOnDropHandle::new(tokio::spawn(serve_with_client_and_identity(
        tool,
        scope,
        NatsConnection {
            client,
            replicas: 1,
        },
        Default::default(),
    )));
    tokio::time::timeout(CI_SAFE_TIMEOUT, registered.next())
        .await?
        .context("registration watch closed")??;
    Ok(server)
}

fn model(calls: Arc<AtomicUsize>) -> harnx_runtime::AgentCallFn {
    Arc::new(move |input, _, _| {
        let round = calls.fetch_add(1, Ordering::SeqCst);
        let text = input.text();
        Box::pin(async move {
            match round {
                0 => {
                    assert_eq!(text, PROMPT);
                    Ok((
                        "starting tool".into(),
                        None,
                        vec![ToolCall::new(
                            "paused_wait".into(),
                            json!({}),
                            Some("held-tool".into()),
                            None,
                        )],
                        Default::default(),
                    ))
                }
                1 => {
                    assert_eq!(text, NEXT_PROMPT, "G1 cannot be adopted by G2");
                    Ok((
                        "new turn completed".into(),
                        None,
                        vec![],
                        Default::default(),
                    ))
                }
                _ => panic!("cancelled model/tool generation replayed"),
            }
        })
    })
}

fn assert_user_history(entries: &[(u64, SessionLogEntry)], prompts: &[&str]) -> Result<()> {
    let raw = entries
        .iter()
        .map(|(seq, entry)| (*seq as usize, entry.clone()))
        .collect::<Vec<_>>();
    let restored =
        harnx_runtime::config::session::replay_log_entries_for_external(&raw, SESSION_ID)?;
    let actual = restored
        .messages
        .iter()
        .filter(|message| message.role.is_user())
        .map(|message| message.content.to_text())
        .collect::<Vec<_>>();
    assert_eq!(actual, prompts);
    Ok(())
}

async fn cancel_and_wait(fixture: &Fixture) -> Result<()> {
    let receipt = fixture
        .session
        .request_cancel(CancelRequest::default())
        .await?;
    assert!(receipt.cancelled);
    fixture.tool.release.notify_one();
    tokio::time::timeout(CI_SAFE_TIMEOUT, fixture.tool.returned.notified()).await?;
    let status = fixture
        .session
        .wait_for_cancel(&receipt, tokio::time::Instant::now() + CI_SAFE_TIMEOUT)
        .await?;
    assert_eq!(status.disposition, CancelDisposition::Cancelled);
    wait_for_worker_session_cleanup(&fixture.js, fixture.session.storage_key()).await
}

fn assert_cancelled_history(entries: &[(u64, SessionLogEntry)]) -> Result<()> {
    assert_user_history(entries, &[PROMPT])?;
    assert!(entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })));
    assert!(
        !entries.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::ToolResults { .. }
                | SessionLogEntry::TurnEnd { .. }
                | SessionLogEntry::Error { .. }
                | SessionLogEntry::Message {
                    role: MessageRole::Assistant,
                    ..
                }
        )),
        "late output remains fenced"
    );
    let state = reconstruct_state_from_nats(entries);
    assert_eq!(state.turn_status, TurnStatus::InFlightCancelled);
    assert!(
        state.next_turn_messages.is_empty(),
        "history isn't pending work"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_during_tool_keeps_admitted_prompt_and_returns_idle() -> Result<()> {
    let fixture = Fixture::start().await?;
    let input = harnx_runtime::config::input::from_str(&fixture.config, PROMPT, None);
    let admitted = fixture.session.admit_input(&input, None).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, fixture.tool.entered.notified()).await?;
    assert_user_history(&fixture.entries().await?, &[PROMPT])?;
    fixture.ownership(&admitted).await?;
    assert_eq!(
        fixture
            .session
            .execution_store()
            .current(fixture.session.storage_key())
            .await?
            .unwrap()
            .state,
        OperationState::Running
    );

    cancel_and_wait(&fixture).await?;
    assert_cancelled_history(&fixture.entries().await?)?;

    let reopened = session(fixture.server.url(), SESSION_ID).await?;
    assert_eq!(reopened.activate_pending_turn().await?, None);
    reopened
        .run_turn(NEXT_PROMPT, Arc::new(NullSink), None)
        .await?;
    wait_for_worker_session_cleanup(&fixture.js, fixture.session.storage_key()).await?;
    assert_user_history(&fixture.entries().await?, &[PROMPT, NEXT_PROMPT])?;
    fixture.ownership(&admitted).await?;
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.model_calls.load(Ordering::SeqCst), 2);
    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_root_returns_followers_and_runs_g2_before_g1_handler_cleanup() -> Result<()> {
    let fixture = Fixture::start().await?;
    let input = harnx_runtime::config::input::from_str(&fixture.config, PROMPT, None);
    let admitted = fixture.session.admit_input(&input, None).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, fixture.tool.entered.notified()).await?;
    let foreign = session(fixture.server.url(), SESSION_ID).await?;
    let follower = foreign.follow_admitted_prompt(
        admitted.clone(),
        Arc::new(NullSink),
        None,
        None,
        Default::default(),
    );
    tokio::pin!(follower);

    // No handler release, descendant acknowledgement or physical status wait.
    assert!(fixture.session.cancel_pending_turn().await?);
    let result = tokio::time::timeout(CI_SAFE_TIMEOUT, &mut follower).await??;
    assert!(result.was_cancelled);
    assert!(result.response.is_none() && result.error.is_none());

    let next = session(fixture.server.url(), SESSION_ID).await?;
    let result = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        next.run_turn(NEXT_PROMPT, Arc::new(NullSink), None),
    )
    .await??;
    assert!(!result.was_cancelled, "G1's abort cannot reach G2");
    assert_eq!(result.response.as_deref(), Some("new turn completed"));
    let original = OperationRef::new(fixture.session.storage_key(), admitted.execution_id());
    let context = fixture
        .session
        .execution_store()
        .activate_gate(&original)
        .await?;
    assert_ne!(
        fixture
            .session
            .execution_store()
            .gate_cleanup(&context)
            .await?
            .state,
        harnx_execution_control::CleanupState::Confirmed
    );

    // A follower attaching after G2 completion must still return G1's stop, not
    // G2's response or an orphan error. The retained subscription is consumed.
    let late = session(fixture.server.url(), SESSION_ID).await?;
    let result = tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        late.follow_admitted_prompt(
            admitted.clone(),
            Arc::new(NullSink),
            None,
            None,
            Default::default(),
        ),
    )
    .await??;
    assert!(result.was_cancelled);
    assert!(result.response.is_none() && result.error.is_none());

    fixture.tool.release.notify_one();
    tokio::time::timeout(CI_SAFE_TIMEOUT, fixture.tool.returned.notified()).await?;
    let entries = fixture.entries().await?;
    assert_user_history(&entries, &[PROMPT, NEXT_PROMPT])?;
    assert!(!entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { .. })));
    fixture.ownership(&admitted).await?;
    assert_eq!(fixture.model_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 1);
    Ok(())
}
