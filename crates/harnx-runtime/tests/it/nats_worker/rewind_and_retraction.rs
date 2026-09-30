use super::*;

async fn append_retracted_user_message(
    log: &NatsSessionLog,
    message_id: &str,
    content: &str,
) -> Result<u64> {
    let seq = log
        .append_event_async(&append_user_message_entry(message_id, content))
        .await?;
    log.append_event_async(&SessionLogEntry::EditEntries {
        from: seq as usize,
        to: seq as usize,
        replacements: vec![],
    })
    .await?;
    Ok(seq)
}

async fn assert_single_prompt(prompts: &Arc<AsyncMutex<Vec<String>>>, expected: &str) {
    let captured = prompts.lock().await.clone();
    assert_eq!(
        captured.len(),
        1,
        "worker should have made exactly one call"
    );
    assert_eq!(
        captured[0], expected,
        "worker input should match expected message"
    );
}

fn assert_single_assistant_contains(entries: &[(u64, SessionLogEntry)], needle: &str) {
    let assistant_texts = final_assistant_texts(entries);
    assert_eq!(
        assistant_texts.len(),
        1,
        "exactly one assistant message should be persisted"
    );
    assert!(
        assistant_texts[0].contains(needle),
        "assistant should contain {needle:?}, got: {:?}",
        assistant_texts
    );
}

async fn wait_for_turn_end(log: &NatsSessionLog) -> Result<Vec<(u64, SessionLogEntry)>> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            let entries = log.load_events_latest_async().await?;
            if entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. }))
            {
                return Ok(entries);
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rewind_truncates_worker_visible_tail_before_activation() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let counter = Arc::new(AtomicUsize::new(0));
    let prompts = Arc::new(AsyncMutex::new(Vec::<String>::new()));

    let config = local_nats_runtime_config(server.url());
    let daemon = spawn_worker_daemon_with_call_fn(
        config,
        "worker-rewind",
        fold_capture_call_fn(counter.clone(), prompts.clone()),
    )
    .await?;

    let js = local_test_nats(server.url()).await?;
    let session_id = "rewind-worker-test";
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);

    let first_seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some("msg-first".to_string()),
            role: MessageRole::User,
            content: harnx_core::message::MessageContent::Text("first prompt".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("msg-second".to_string()),
        role: MessageRole::User,
        content: harnx_core::message::MessageContent::Text("second prompt".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::Rewind {
        after_seq: usize::try_from(first_seq).expect("JetStream seq fits usize"),
    })
    .await?;

    activate_session(&js, session_id).await?;

    wait_until(CI_SAFE_TIMEOUT, || counter.load(Ordering::SeqCst) >= 1).await?;

    assert_single_prompt(&prompts, "first prompt").await;

    let entries = wait_for_turn_end(&log).await?;
    let assistant_texts = final_assistant_texts(&entries);
    assert_single_assistant_contains(&entries, "first prompt");
    assert!(
        assistant_texts
            .iter()
            .all(|text| !text.contains("second prompt")),
        "rewound tail must not leak into worker execution: {:?}",
        assistant_texts
    );

    let reconstructed = reconstruct_state_from_nats(&entries);
    assert_eq!(reconstructed.turn_status, TurnStatus::Idle);

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retracted_user_message_is_not_executed_by_worker() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let counter = Arc::new(AtomicUsize::new(0));
    let prompts = Arc::new(AsyncMutex::new(Vec::<String>::new()));

    let config = local_nats_runtime_config(server.url());
    let worker_config = WorkerDaemonConfig::managing("local", "worker-retract");
    let daemon = tokio::spawn({
        let cfg = config.clone();
        let calls = counter.clone();
        let captured_prompts = prompts.clone();
        async move {
            run_worker_daemon(
                cfg,
                worker_config,
                Some(fold_capture_call_fn(calls, captured_prompts)),
                None,
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "retract-test";
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);

    // Append a user message and an EditEntries that retracts it.
    append_retracted_user_message(&log, "msg-to-retract", "please ignore this").await?;

    // Append a valid user message that SHOULD be processed.
    log.append_event_async(&append_user_message_entry("valid-msg", "hello world"))
        .await?;

    // Activate the session.
    activate_session(&js, session_id).await?;

    // Wait for the worker to process.
    wait_until(CI_SAFE_TIMEOUT, || counter.load(Ordering::SeqCst) >= 1).await?;

    // Verify: only ONE call was made, and the prompt is "hello world", NOT "please ignore this"
    // If the bug is present, the prompt would contain the retracted message.
    assert_single_prompt(&prompts, "hello world").await;

    // Verify the durable log: no assistant turn for the retracted message.
    let entries = wait_for_turn_end(&log).await?;
    assert_single_assistant_contains(&entries, "hello world");

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
