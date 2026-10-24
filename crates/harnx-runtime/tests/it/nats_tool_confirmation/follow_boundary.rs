use super::*;
use futures_util::StreamExt;
use harnx_core::event::{AgentEvent, AgentEventSink, ContentBlock, ModelEvent};
use harnx_runtime::{FollowBoundary, FollowBoundaryOptions, HitlDecisionOutcome};
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

impl BoundaryFixture {
    async fn start() -> Result<Self> {
        require_nextest();
        let server = common::spawn_nats_server()
            .await?
            .context("nats-server required")?;
        ensure_hook_server_binary().await?;
        let environment = TestEnvironment::new(server.url())?;
        std::fs::create_dir_all(environment.config_dir.join("tool_servers"))?;
        std::fs::write(
            environment.config_dir.join("tool_servers/time.yaml"),
            "command: harnx-time-tools\n",
        )?;
        let agent =
            std::fs::read_to_string(environment.config_dir.join("agents/approval-gated.md"))?
                .replace("target_session_handoff", "time_get_current_time");
        std::fs::write(
            environment.config_dir.join("agents/approval-gated.md"),
            agent,
        )?;
        let (config, client, jetstream) = environment.load().await?;
        let final_started = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let rounds = Arc::new(AtomicUsize::new(0));
        let call_fn: harnx_runtime::agent_loop::AgentCallFn = {
            let final_started = final_started.clone();
            let finish = finish.clone();
            let client = client.clone();
            let js = jetstream.clone();
            Arc::new(move |_input, _config, _abort| {
                let round = rounds.fetch_add(1, Ordering::SeqCst);
                let final_started = final_started.clone();
                let finish = finish.clone();
                let client = client.clone();
                let js = js.clone();
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
                    harnx_runtime::nats_event_sink::NatsEventSink::new(
                        client.clone(),
                        js,
                        source_key(),
                    )
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
        };
        let daemon = start_ready_daemon(config, call_fn, &client).await?;
        let source = NatsSession::new(
            NatsSessionConfig {
                cluster: "local".into(),
                initializer: SessionInitializer::named("approval-gated", Default::default()),
                session_id: Some(SOURCE_SESSION_ID.into()),
                activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
            },
            client.clone(),
            jetstream.clone(),
            create_abort_signal(),
        )
        .await?
        .with_external_admission();
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
                None,
                FollowBoundaryOptions {
                    after_request_seq,
                    ..Default::default()
                },
            ),
        )
        .await?
    }
}

async fn serial_approvals(reject_first: bool) -> Result<()> {
    let fixture = BoundaryFixture::start().await?;
    let session = &fixture.harness.source;
    let ticket = fixture.ticket().await?;
    let sink = Arc::new(TextSink::default());
    let FollowBoundary::InputRequired(first) = fixture.follow(&ticket, sink.clone(), None).await?
    else {
        anyhow::bail!("initial segment must suspend");
    };
    assert_eq!(first.tool_call_id, "first-approval");
    assert_eq!(
        &*sink.0.lock(),
        "before approval",
        "live text chunks must reach the sink"
    );
    let FollowBoundary::InputRequired(still_pending) = fixture
        .follow(&ticket, sink.clone(), Some(u64::MAX))
        .await?
    else {
        anyhow::bail!("cursor must not hide an undecided old request");
    };
    assert_eq!(still_pending, first);
    assert_eq!(
        session
            .decide_hitl_approval_for(
                &first.approval_ref(ticket.invocation_id()),
                !reject_first,
                reject_first.then(|| "reject first, continue second".into())
            )
            .await?,
        HitlDecisionOutcome::Applied
    );
    // Force Q2 to be durable before reattaching; subscribe-first must reconcile
    // history rather than wait for a live notification that was already sent.
    wait_for_source_count(
        &fixture.harness.jetstream,
        |entry| matches!(entry, SessionLogEntry::HitlApprovalRequested { .. }),
        2,
    )
    .await?;
    let FollowBoundary::InputRequired(second) = fixture
        .follow(&ticket, sink.clone(), Some(first.request_seq))
        .await?
    else {
        anyhow::bail!("resumed segment must return Q2, never decided Q1");
    };
    assert_eq!(second.tool_call_id, "second-approval");
    assert_eq!(first.tool_round_seq, second.tool_round_seq);
    assert!(second.request_seq > first.request_seq);
    assert_eq!(
        session
            .decide_hitl_approval_for(&second.approval_ref(ticket.invocation_id()), true, None)
            .await?,
        HitlDecisionOutcome::Applied
    );
    tokio::time::timeout(Duration::from_secs(10), fixture.final_started.notified()).await?;
    // The model is paused. Observe the follower's activation (published only
    // after subscription/history attachment), then let it stream the final text.
    let mut activations = fixture
        .harness
        .client
        .subscribe(harnx_runtime::nats_worker::notify_subject("local"))
        .await?;
    fixture.harness.client.flush().await?;
    let follow = fixture.follow(&ticket, sink.clone(), Some(second.request_seq));
    let release = async {
        tokio::time::timeout(Duration::from_secs(5), activations.next())
            .await?
            .context("resumed follower activation")?;
        fixture.finish.notify_one();
        Ok::<_, anyhow::Error>(())
    };
    let (boundary, _) = tokio::try_join!(follow, release)?;
    let FollowBoundary::Terminal(result) = boundary else {
        anyhow::bail!("final segment must terminate")
    };
    assert_eq!(result.user_msg_id, ticket.prompt_id());
    assert_eq!(result.response.as_deref(), Some("after approvals"));
    assert!(!result.was_cancelled);
    assert_eq!(&*sink.0.lock(), "before approvalafter approvals");
    assert!(
        matches!(
            fixture.follow(&ticket, sink, Some(u64::MAX)).await?,
            FollowBoundary::Terminal(_)
        ),
        "terminal detection must ignore request cursor"
    );
    let entries = NatsSessionLog::new(fixture.harness.jetstream.clone(), source_key())
        .load_events_async()
        .await?;
    assert_eq!(entries.iter().filter(|(_, entry)| matches!(entry, SessionLogEntry::Message { role, content, .. } if role.is_user() && !harnx_core::timing_identity::is_timing_notice(&content.to_text()))).count(), 1, "same invocation resumes without another prompt: {entries:?}");
    if reject_first {
        assert!(entries.iter().any(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { results, .. } if results.iter().any(|result| result.output.to_string().contains("reject first, continue second")))));
    }
    Ok(())
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_follow_boundary_keeps_pending_cancel_channel_and_durable_receipt() -> Result<()> {
    let fixture = BoundaryFixture::start().await?;
    let ticket = fixture.ticket().await?;
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(()).await?;
    let session = &fixture.harness.source;
    let boundary = tokio::time::timeout(
        Duration::from_secs(10),
        session.follow_admitted_prompt_until_boundary(
            session
                .fixed_prompt_handle(&ticket)
                .await?
                .context("original prompt")?,
            Arc::new(TextSink::default()),
            Some(rx),
            FollowBoundaryOptions::default(),
        ),
    )
    .await??;
    let FollowBoundary::Terminal(result) = boundary else {
        anyhow::bail!("cancel must end the invocation")
    };
    assert!(result.was_cancelled);
    assert_eq!(result.user_msg_id, ticket.prompt_id());
    assert!(
        matches!(session.fixed_prompt_progress(&ticket).await?, harnx_runtime::PromptProgress::Terminal(result) if result.was_cancelled)
    );
    Ok(())
}
