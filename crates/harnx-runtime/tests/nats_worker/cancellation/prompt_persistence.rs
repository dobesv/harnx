//! Real frontend admission, worker execution and a barrier-held tool handler.
use super::*;
use anyhow::Context;
use harnx_core::instance::ServerScope;
use harnx_toolset::{CancellationGuarantee, ToolInvokeError, ToolSpec, Toolset};
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
        let environment = EnvGuard::tool_server_environment(&scope, server.url());
        let tool = Arc::new(PausedTool::default());
        let tool_server =
            crate::worker::start_tool_server(&js, client.clone(), scope, tool.clone()).await?;
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

    /// The opening prompt stays exactly where it was admitted: an interrupted
    /// turn never rewrites or replays the input that started it.
    async fn opening_prompt_seq(&self) -> Result<u64> {
        self.entries()
            .await?
            .iter()
            .find_map(|(seq, entry)| match entry {
                SessionLogEntry::Message {
                    role: MessageRole::User,
                    content,
                    ..
                } if content.to_text() == PROMPT => Some(*seq),
                _ => None,
            })
            .context("opening input retained")
    }
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
        // Runtime notes (e.g. the interruption note) are synthesized
        // context for the model, not prompts the user typed, but they
        // still carry `MessageRole::User`; exclude them here.
        .filter(|text| !text.starts_with(harnx_runtime::config::session::RUNTIME_NOTE_PREFIX))
        .collect::<Vec<_>>();
    assert_eq!(actual, prompts);
    Ok(())
}

async fn cancel_and_wait(fixture: &Fixture) -> Result<()> {
    let outcome = fixture.session.interrupt("client cancel").await?;
    assert!(
        matches!(outcome, InterruptOutcome::Accepted { .. }),
        "the tool-blocked turn must be interruptible, got {outcome:?}"
    );
    fixture.tool.release.notify_one();
    tokio::time::timeout(CI_SAFE_TIMEOUT, fixture.tool.returned.notified()).await?;
    wait_for_worker_session_cleanup(&fixture.js, fixture.session.storage_key()).await
}

/// The interrupted turn ends as a `Cancel` plus the wind-up that answers the
/// tool call it cut off. Nothing the model or the late tool produced survives
/// as transcript output, and the turn is over rather than pending.
fn assert_cancelled_history(entries: &[(u64, SessionLogEntry)]) -> Result<()> {
    assert_user_history(entries, &[PROMPT])?;
    assert!(entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })));
    assert!(
        entries.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::ToolResults { results, .. }
                if results.iter().any(|r| r.id.as_deref() == Some("held-tool"))
        )),
        "the wind-up owes the interrupted call a result"
    );
    assert!(
        !entries.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::TurnEnd { .. }
                | SessionLogEntry::Error { .. }
                | SessionLogEntry::Message {
                    role: MessageRole::Assistant,
                    ..
                }
        )),
        "late output never lands behind the interruption"
    );
    let state = reconstruct_state_from_nats(entries);
    assert!(matches!(state.turn_status, TurnStatus::Idle));
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
    fixture.session.admit_input(&input, None).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, fixture.tool.entered.notified()).await?;
    assert_user_history(&fixture.entries().await?, &[PROMPT])?;
    let admitted_seq = fixture.opening_prompt_seq().await?;

    cancel_and_wait(&fixture).await?;
    assert_cancelled_history(&fixture.entries().await?)?;

    let reopened = session(fixture.server.url(), SESSION_ID).await?;
    assert_eq!(reopened.activate_pending_turn().await?, None);
    reopened
        .run_turn(NEXT_PROMPT, Arc::new(NullSink), None)
        .await?;
    wait_for_worker_session_cleanup(&fixture.js, fixture.session.storage_key()).await?;
    assert_user_history(&fixture.entries().await?, &[PROMPT, NEXT_PROMPT])?;
    assert_eq!(fixture.opening_prompt_seq().await?, admitted_seq);
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.model_calls.load(Ordering::SeqCst), 2);
    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

/// Interrupting is one append and nothing else. It returns the follower
/// straight away — before the held tool handler has been released, let alone
/// cleaned up — and the prompt typed after it runs as its own turn. This
/// session has no children; that a descendant is not waited on either is what
/// the three-level hierarchy test covers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_returns_after_single_append_without_awaiting_children_or_tools() -> Result<()> {
    let fixture = Fixture::start().await?;
    let input = harnx_runtime::config::input::from_str(&fixture.config, PROMPT, None);
    let admitted = fixture.session.admit_input(&input, None).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, fixture.tool.entered.notified()).await?;
    let foreign = session(fixture.server.url(), SESSION_ID).await?;
    let follower = foreign.follow_admitted_prompt(
        admitted,
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
    assert!(
        !result.was_cancelled,
        "the interrupted turn cannot cancel the next one"
    );
    assert_eq!(result.response.as_deref(), Some("new turn completed"));

    fixture.tool.release.notify_one();
    tokio::time::timeout(CI_SAFE_TIMEOUT, fixture.tool.returned.notified()).await?;
    let entries = fixture.entries().await?;
    assert_user_history(&entries, &[PROMPT, NEXT_PROMPT])?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. }))
            .count(),
        1,
        "interrupting appends one Cancel and nothing else"
    );
    assert!(
        entries.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::ToolResults { results, .. }
                if results.iter().any(|r| r.id.as_deref() == Some("held-tool"))
        )),
        "the interrupted call is answered without waiting for its handler"
    );
    assert_eq!(fixture.model_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 1);
    Ok(())
}
