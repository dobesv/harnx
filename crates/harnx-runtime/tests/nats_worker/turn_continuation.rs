use super::*;

static END_TURN_APPEND_READY: LazyLock<Notify> = LazyLock::new(Notify::new);
static END_TURN_APPEND_DONE: LazyLock<Notify> = LazyLock::new(Notify::new);

fn end_turn_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |input, _config, _abort| {
        let prompt = input.raw.0.clone();
        Box::pin(async move {
            let call_no = END_TURN_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
            // Signal readiness using notify_one() (stores a permit if no waiter yet).
            // This avoids the lost-wakeup race where notify_waiters fires before
            // the test registers its notified() future.
            if call_no == 1 {
                END_TURN_APPEND_READY.notify_one();
                // Block until test signals DONE - this prevents turn 1 from completing
                // before the test appends "second", ensuring the daemon's drain loop
                // sees the new message when it re-reads the tail.
                END_TURN_APPEND_DONE.notified().await;
            }
            Ok((
                format!("turn:{call_no} prompt:{prompt}"),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    })
}

/// Poll the durable log until at least `n` persisted assistant messages
/// contain `"turn:"`, returning the entries snapshot that first satisfies it.
async fn wait_for_n_turn_assistants(
    log: &NatsSessionLog,
    n: usize,
) -> Result<Vec<(u64, SessionLogEntry)>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let entries = log.load_events_async().await?;
        let count = final_assistant_texts(&entries)
            .iter()
            .filter(|text| text.contains("turn:"))
            .count();
        if count >= n {
            return Ok(entries);
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "continuation turn never persisted {n} 'turn:' assistant messages (got {count})"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_of_turn_reread_runs_continuation_turn_with_same_activation() -> Result<()> {
    reset_test_state();
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let config = Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url: server.url(),
        token: None,
    })));
    let worker_config = WorkerDaemonConfig::managing("local", "worker-reread");
    let daemon = tokio::spawn({
        let cfg = config.clone();
        async move { run_worker_daemon(cfg, worker_config, Some(end_turn_call_fn()), None).await }
    });
    // Give daemon time to initialize consumer
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "end-turn-reread";
    let log = NatsSessionLog::new(js.clone(), storage_key(session_id));

    // CRITICAL: Create the notified future BEFORE publishing the activate
    // to avoid lost wakeup race between notify_one() and notified().await
    let ready_fut = END_TURN_APPEND_READY.notified();

    log.append_event_async(&append_user_message_entry("user-1", "first"))
        .await?;
    activate_session(&js, session_id).await?;

    // Now await the ready signal - the permit was stored by notify_one()
    ready_fut.await;

    // Give a moment for the NATS message to propagate
    tokio::time::sleep(Duration::from_millis(100)).await;

    log.append_event_async(&append_user_message_entry("user-2", "second"))
        .await?;
    // Signal to turn 1 that it can complete now
    END_TURN_APPEND_DONE.notify_one();

    // `END_TURN_CALLS` is bumped at the START of each LLM call, so waiting on it
    // races turn 2's persistence; poll the committed log instead.
    let entries = wait_for_n_turn_assistants(&log, 2).await?;

    let assistants = final_assistant_texts(&entries);
    assert_eq!(
        assistants
            .iter()
            .filter(|text| text.contains("turn:"))
            .count(),
        2
    );
    // Turn 1 consumed "first" (its assistant barrier), so the continuation turn 2
    // folds only the post-barrier message "second".
    assert!(assistants
        .iter()
        .any(|text| text.contains("turn:1 prompt:first")));
    assert!(assistants
        .iter()
        .any(|text| text.contains("turn:2 prompt:second")));

    // `skip_user_log_append` invariant: the worker must NOT re-append the user
    // messages it reads from the log. The durable log must contain EXACTLY the
    // two client-appended user messages — "first" and "second" — with no
    // duplicates. A regression (re-appending the folded input) would both
    // duplicate the user message and reorder the assistant barrier past
    // concurrently-arrived messages.
    let users = user_message_texts(&entries);
    assert_eq!(
        users,
        vec!["first".to_string(), "second".to_string()],
        "durable log must contain exactly the two client user messages with no worker re-appends; got {users:?}"
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_concurrent_messages_fold_in_seq_order_into_single_turn() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let config = Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url: server.url(),
        token: None,
    })));
    let calls = Arc::new(AtomicUsize::new(0));
    let prompts = Arc::new(AsyncMutex::new(Vec::<String>::new()));
    let worker_config = WorkerDaemonConfig::managing("local", "worker-fold");
    let daemon = tokio::spawn({
        let cfg = config.clone();
        let calls = calls.clone();
        let prompts = prompts.clone();
        async move {
            run_worker_daemon(
                cfg,
                worker_config,
                Some(fold_capture_call_fn(calls, prompts)),
                None,
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "fold-order";
    let log = NatsSessionLog::new(js.clone(), storage_key(session_id));
    log.append_event_async(&append_user_message_entry("user-1", "alpha"))
        .await?;
    log.append_event_async(&append_user_message_entry("user-2", "beta"))
        .await?;
    activate_session(&js, session_id).await?;

    wait_until(CI_SAFE_TIMEOUT, || calls.load(Ordering::SeqCst) >= 1).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let prompts = prompts.lock().await.clone();
    assert_eq!(prompts, vec!["alpha\nbeta".to_string()]);

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
