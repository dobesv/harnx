use super::*;

const SESSION_ID: &str = "multi-round-resume-with-queued-user";
type CapturedInputs = Vec<(String, Vec<String>)>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_tool_round_user_message_is_injected_once_into_same_turn() -> Result<()> {
    reset_test_state();
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let config = local_nats_runtime_config(server.url());
    let worker_config = WorkerDaemonConfig::managing("local", "worker-mid-round");
    let daemon = tokio::spawn({
        let cfg = config.clone();
        async move { run_worker_daemon(cfg, worker_config, Some(mid_round_call_fn())).await }
    });
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "mid-round-injection";
    let log = NatsSessionLog::new(js.clone(), session_id);
    let queue_session = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".to_string(),
            initializer: SessionInitializer::inline(
                "",
                Default::default(),
                SessionOverrides::default(),
            ),
            session_id: Some(session_id.to_string()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        async_nats::connect(server.url()).await?,
        js,
        create_abort_signal(),
    )
    .await?;

    let ready_fut = MID_ROUND_APPEND_READY.notified();
    queue_session
        .enqueue_text("seed message")
        .await?
        .into_activation_result()?;
    ready_fut.await;
    queue_session
        .enqueue_text("late message")
        .await?
        .into_activation_result()?;
    MID_ROUND_APPEND_DONE.notify_one();

    wait_until(CI_SAFE_TIMEOUT, || {
        MID_ROUND_FINAL_CALLS.load(Ordering::SeqCst) >= 1
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let entries = log.load_events_async().await?;
    let assistants = final_assistant_texts(&entries);
    assert!(assistants.iter().any(|text| text.contains("late message")));
    assert_eq!(
        assistants
            .iter()
            .filter(|text| text.contains("late message"))
            .count(),
        1
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

struct ResumeCapture {
    model_calls: Arc<AtomicUsize>,
    inputs: Arc<AsyncMutex<CapturedInputs>>,
}

struct ResumeFixture {
    log: NatsSessionLog,
    session: NatsSession,
    queued_user_seq: u64,
}

fn capturing_call_fn() -> (ResumeCapture, harnx_runtime::agent_loop::AgentCallFn) {
    let capture = ResumeCapture {
        model_calls: Arc::new(AtomicUsize::new(0)),
        inputs: Arc::new(AsyncMutex::new(Vec::new())),
    };
    let calls = Arc::clone(&capture.model_calls);
    let inputs = Arc::clone(&capture.inputs);
    let call_fn: harnx_runtime::agent_loop::AgentCallFn = Arc::new(move |input, config, _abort| {
        let raw = input.raw.0.clone();
        let wire_users = harnx_runtime::config::input::build_messages(input, config)
            .expect("resume input should build model messages")
            .into_iter()
            .filter(|message| message.role.is_user())
            .map(|message| message.content.to_text())
            .collect::<Vec<_>>();
        let calls = Arc::clone(&calls);
        let inputs = Arc::clone(&inputs);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            inputs.lock().await.push((raw, wire_users));
            Ok((
                "resumed successfully".to_string(),
                None,
                Vec::new(),
                CompletionTokenUsage::default(),
            ))
        })
    });
    (capture, call_fn)
}

fn tool_calls(name: &str, call_id: &str, text: &str) -> SessionLogEntry {
    SessionLogEntry::ToolCalls {
        text: text.to_string(),
        thought: None,
        calls: vec![ToolCall::new(
            name.to_string(),
            json!({}),
            Some(call_id.to_string()),
            None,
        )],
        timestamp: None,
        fence_token: None,
    }
}

fn successful_tool_result(name: &str, call_id: &str) -> SessionLogEntry {
    SessionLogEntry::ToolResults {
        results: vec![harnx_core::session::ToolOutput {
            id: Some(call_id.to_string()),
            name: name.to_string(),
            output: json!({"ok": true}),
            markdown: None,
            content: Vec::new(),
            switch_agent: None,
        }],
        timestamp: None,
    }
}

async fn seed_resume_fixture(server_url: &str) -> Result<ResumeFixture> {
    let js = local_test_nats(server_url).await?;
    let log = NatsSessionLog::new(js.clone(), SESSION_ID);
    seed_session_metadata(&js, SESSION_ID).await?;
    log.append_event_async(&append_user_message_entry("user-1", "original request"))
        .await?;
    log.append_event_async(&tool_calls("first_tool", "call-complete", "first round"))
        .await?;
    log.append_event_async(&successful_tool_result("first_tool", "call-complete"))
        .await?;
    log.append_event_async(&tool_calls(
        "non_idempotent_unknown_tool",
        "call-orphan",
        "second round",
    ))
    .await?;
    let queued_user_seq = log
        .append_event_async(&append_user_message_entry(
            "user-queued",
            "queued correction",
        ))
        .await?;
    let session = NatsSession::new(
        NatsSessionConfig {
            cluster: "local".to_string(),
            initializer: SessionInitializer::inline(
                "",
                Default::default(),
                SessionOverrides::default(),
            ),
            session_id: Some(SESSION_ID.to_string()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        async_nats::connect(server_url).await?,
        js,
        create_abort_signal(),
    )
    .await?;
    Ok(ResumeFixture {
        log,
        session,
        queued_user_seq,
    })
}

async fn assert_resume_outcome(
    fixture: &ResumeFixture,
    capture: &ResumeCapture,
    metrics_before: harnx_runtime::nats_metrics::NatsMetricsSnapshot,
) -> Result<()> {
    let entries = fixture.log.load_events_async().await?;
    assert_eq!(capture.model_calls.load(Ordering::SeqCst), 1);
    let captured = capture.inputs.lock().await;
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].0, "original request");
    assert_eq!(
        captured[0]
            .1
            .iter()
            .filter(|text| text.as_str() == "queued correction")
            .count(),
        1,
        "the queued user must reach the resumed model call exactly once"
    );
    assert_eq!(count_tool_results_with_id(&entries, "call-orphan"), 1);
    assert!(entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::TurnEnd { through_seq, .. }
            if *through_seq >= fixture.queued_user_seq
    )));
    assert!(!entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { through_seq: 0, .. })));
    assert_eq!(
        reconstruct_state_from_nats(&entries).turn_status,
        TurnStatus::Idle
    );
    assert_eq!(fixture.session.activate_pending_turn().await?, None);
    let metrics_after = harnx_runtime::nats_metrics::snapshot();
    assert_eq!(metrics_after.resumes, metrics_before.resumes + 1);
    assert_eq!(
        metrics_after.interrupt_errors_synthesized,
        metrics_before.interrupt_errors_synthesized + 1
    );
    Ok(())
}

/// Regression for #1573: repair a queued multi-round tail once and publish a
/// completion boundary that lets attached TUIs become idle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_round_resume_with_queued_user_repairs_once_and_becomes_idle() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let (capture, call_fn) = capturing_call_fn();
    let daemon = spawn_worker_daemon_with_call_fn(
        local_nats_runtime_config(server.url()),
        "worker-multi-round-resume",
        call_fn,
    )
    .await;
    let fixture = seed_resume_fixture(server.url()).await?;
    let metrics_before = harnx_runtime::nats_metrics::snapshot();

    assert_eq!(
        fixture.session.activate_pending_turn().await?,
        Some(fixture.queued_user_seq)
    );
    wait_for_worker_daemon_idle(metrics_before.lease_acquisitions).await?;
    assert_resume_outcome(&fixture, &capture, metrics_before).await?;

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
