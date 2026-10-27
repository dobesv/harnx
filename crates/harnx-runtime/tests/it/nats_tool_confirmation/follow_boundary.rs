#[path = "follow_boundary_support.rs"]
mod follow_boundary_support;
use follow_boundary_support::*;

use super::*;
use futures_util::StreamExt;
use harnx_core::event::{AgentEvent, AgentEventSink, ContentBlock, ModelEvent};
use harnx_runtime::{
    FollowBoundary, FollowBoundaryOptions, HitlDecisionOutcome, PendingHitlApproval,
};
use std::time::Duration;

#[derive(Default)]
struct TextSink(Mutex<String>);
impl AgentEventSink for TextSink {
    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::Model(ModelEvent::MessageChunk { blocks }) = event {
            for block in blocks {
                if let ContentBlock::Text(text) = block {
                    self.0.lock().push_str(&text);
                }
            }
        }
    }
}

struct BoundaryFixture {
    harness: ConfirmationHarness,
    final_started: Arc<tokio::sync::Notify>,
    finish: Arc<tokio::sync::Notify>,
}

async fn setup_boundary_environment(
    server_url: &str,
) -> Result<(
    TestEnvironment,
    harnx_runtime::config::GlobalConfig,
    async_nats::Client,
    async_nats::jetstream::Context,
)> {
    let environment = TestEnvironment::new(server_url)?;
    std::fs::create_dir_all(environment.config_dir.join("tool_servers"))?;
    std::fs::write(
        environment.config_dir.join("tool_servers/time.yaml"),
        "command: harnx-time-tools
",
    )?;
    let agent = std::fs::read_to_string(environment.config_dir.join("agents/approval-gated.md"))?
        .replace("target_session_handoff", "time_get_current_time");
    std::fs::write(
        environment.config_dir.join("agents/approval-gated.md"),
        agent,
    )?;
    let (config, client, jetstream) = environment.load().await?;
    Ok((environment, config, client, jetstream))
}

async fn create_boundary_session(
    client: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
) -> Result<NatsSession> {
    Ok(NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            initializer: SessionInitializer::named("approval-gated", Default::default()),
            session_id: Some(SOURCE_SESSION_ID.into()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        client,
        jetstream,
        create_abort_signal(),
    )
    .await?
    .with_external_admission())
}

impl BoundaryFixture {
    async fn start() -> Result<Self> {
        require_nextest();
        let server = common::spawn_nats_server()
            .await?
            .context("nats-server required")?;
        ensure_hook_server_binary().await?;
        let (environment, config, client, jetstream) =
            setup_boundary_environment(server.url()).await?;
        let final_started = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let call_fn = build_boundary_call_fn(
            final_started.clone(),
            finish.clone(),
            client.clone(),
            jetstream.clone(),
        );
        let daemon = start_ready_daemon(config, call_fn, &client).await?;
        let source = create_boundary_session(client.clone(), jetstream.clone()).await?;
        Ok(Self {
            harness: ConfirmationHarness {
                source,
                client,
                jetstream,
                daemon,
                _server: server,
                _environment: environment,
            },
            final_started,
            finish,
        })
    }

    async fn ticket(
        &self,
    ) -> Result<harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket> {
        let session = &self.harness.source;
        let ticket = session
            .prepare_fixed_admission(
                "boundary-invocation".into(),
                "boundary-prompt".into(),
                "boundary-closed".into(),
            )
            .await?;
        session
            .append_fixed_prompt(
                &ticket,
                MessageContent::Text("two approval-requiring tools".into()),
            )
            .await?;
        Ok(ticket)
    }

    async fn follow(
        &self,
        ticket: &harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket,
        sink: Arc<TextSink>,
        after_request_seq: Option<u64>,
    ) -> Result<FollowBoundary> {
        tokio::time::timeout(
            Duration::from_secs(15),
            self.harness.source.follow_admitted_prompt_until_boundary(
                self.harness
                    .source
                    .fixed_prompt_handle(ticket)
                    .await?
                    .context("original prompt")?,
                sink,
                FollowBoundaryOptions {
                    after_request_seq,
                    ..Default::default()
                },
            ),
        )
        .await?
    }
}

fn build_boundary_call_fn(
    final_started: Arc<tokio::sync::Notify>,
    finish: Arc<tokio::sync::Notify>,
    client: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
) -> harnx_runtime::agent_loop::AgentCallFn {
    let rounds = Arc::new(AtomicUsize::new(0));
    Arc::new(move |_input, _config, _abort| {
        let round = rounds.fetch_add(1, Ordering::SeqCst);
        let final_started = final_started.clone();
        let finish = finish.clone();
        let client = client.clone();
        let js = jetstream.clone();
        Box::pin(async move {
            if round > 0 {
                final_started.notify_one();
                finish.notified().await;
            }
            let text = if round == 0 {
                "before approval"
            } else {
                "after approvals"
            };
            harnx_runtime::nats_event_sink::NatsEventSink::new(client.clone(), js, source_key())
                .await
                .publish_event(AgentEvent::Model(ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text(text.into())],
                }))
                .await;
            client.flush().await?;
            let calls = if round == 0 {
                ["first-approval", "second-approval"]
                    .into_iter()
                    .map(|id| {
                        ToolCall::new(
                            "time_get_current_time".into(),
                            json!({"timezone": "UTC"}),
                            Some(id.into()),
                            None,
                        )
                    })
                    .collect()
            } else {
                vec![]
            };
            Ok((text.into(), None, calls, CompletionTokenUsage::default()))
        })
    })
}

async fn serial_approvals(reject_first: bool) -> Result<()> {
    let fixture = BoundaryFixture::start().await?;
    let ticket = fixture.ticket().await?;
    let sink = Arc::new(TextSink::default());
    let ctx = SerialApprovalContext {
        fixture: &fixture,
        ticket: &ticket,
        sink: &sink,
    };
    let first = assert_first_approval_gate(&ctx, reject_first).await?;
    let second = assert_second_approval_gate(&ctx, &first).await?;
    assert_terminal_follow_segment(&ctx, &second, reject_first).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_follow_boundary_two_serial_approvals_stream_text_and_resume_original_prompt(
) -> Result<()> {
    serial_approvals(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_follow_boundary_rejected_first_call_still_returns_second_approval() -> Result<()> {
    serial_approvals(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_follow_boundary_historical_error_wins_over_request_cursor() -> Result<()> {
    let fixture = BoundaryFixture::start().await?;
    let ticket = fixture.ticket().await?;
    NatsSessionLog::new(fixture.harness.jetstream.clone(), source_key())
        .append_event_async(&SessionLogEntry::Error {
            message: "historical worker failure".into(),
            fence_token: 1,
            timestamp: None,
        })
        .await?;
    let FollowBoundary::Terminal(result) = fixture
        .follow(&ticket, Arc::new(TextSink::default()), Some(u64::MAX))
        .await?
    else {
        anyhow::bail!("historical error is terminal regardless of request cursor");
    };
    assert_eq!(result.error.as_deref(), Some("historical worker failure"));
    assert_eq!(result.user_msg_id, ticket.prompt_id());
    assert!(!result.was_cancelled);
    Ok(())
}

async fn run_boundary_with_cancel(
    session: &NatsSession,
    ticket: &harnx_runtime::nats_session::fixed_admission::FixedAdmissionTicket,
    rx: tokio::sync::mpsc::Receiver<()>,
) -> Result<FollowBoundary> {
    let handle = session
        .fixed_prompt_handle(ticket)
        .await?
        .context("original prompt")?;
    let boundary = tokio::time::timeout(
        Duration::from_secs(10),
        session.follow_admitted_prompt_until_boundary(
            handle,
            Arc::new(TextSink::default()),
            FollowBoundaryOptions {
                pending_cancel: Some(rx),
                ..Default::default()
            },
        ),
    )
    .await??;
    Ok(boundary)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_follow_boundary_keeps_pending_cancel_channel_and_durable_receipt() -> Result<()> {
    let fixture = BoundaryFixture::start().await?;
    let ticket = fixture.ticket().await?;
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(()).await?;
    let session = &fixture.harness.source;
    let FollowBoundary::Terminal(result) = run_boundary_with_cancel(session, &ticket, rx).await?
    else {
        anyhow::bail!("cancel must end the invocation")
    };
    assert!(result.was_cancelled);
    assert_eq!(result.user_msg_id, ticket.prompt_id());
    assert!(
        matches!(session.fixed_prompt_progress(&ticket).await?, harnx_runtime::PromptProgress::Terminal(result) if result.was_cancelled)
    );
    Ok(())
}
