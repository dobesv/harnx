//! Registry bookkeeping: handing out live handles, and reaping only when it is safe.

use super::*;

fn noop_call_fn() -> AgentCallFn {
    Arc::new(|_input, _config, _abort| {
        Box::pin(async {
            Ok((
                "done".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

fn self_handoff_call_fn(calls: Arc<AtomicUsize>) -> AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let call_index = calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let (message, tool_calls) = if call_index == 0 {
                (
                    "handoff to myself",
                    vec![ToolCall::new(
                        "loop-agent_session_handoff".to_string(),
                        json!({
                            "prompt": "continue locally",
                            "session_id": "same-session"
                        }),
                        Some("self-handoff".to_string()),
                        None,
                    )],
                )
            } else {
                ("continued", vec![])
            };
            Ok((
                message.to_string(),
                None,
                tool_calls,
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

/// Registry with a 50ms reap TTL and an agent to resolve, ready to spawn actors.
async fn registry_sandbox() -> (
    harnx_runtime::client::TestStateGuard<'static>,
    TestConfigSandbox,
    SessionRegistry,
) {
    let guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let registry = registry_with_call_fn(noop_call_fn());
    (guard, sandbox, registry)
}

/// Arm the reap deadline the way a departing subscriber does, and wait until the actor has
/// processed it: `Unsubscribe` carries no reply, so a following `Get` round-trip is the
/// confirmation that the deadline is set.
async fn arm_reap_deadline(handle: &SessionHandle) {
    handle
        .tx
        .send(SessionCommand::Unsubscribe)
        .await
        .expect("send unsubscribe");
    get_info(handle).await;
}

async fn wait_until_unregistered(registry: &SessionRegistry, key: &SessionKey) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while registry.has_session(key) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("idle session actor should be reaped once no caller holds its handle");
}

fn registered_actor_id(registry: &SessionRegistry, key: &SessionKey) -> Option<u64> {
    registry.map.get(key).map(|entry| entry.actor_id)
}

#[tokio::test]
async fn reap_spares_an_actor_whose_handle_a_caller_still_holds() {
    let (_guard, _sandbox, registry) = registry_sandbox().await;
    let key = key("plain", "reap-with-held-handle");
    let handle = registry.get_or_spawn(key.clone());
    let actor_id = handle.actor_id;

    arm_reap_deadline(&handle).await;
    // Well past the 50ms TTL: the reap must keep deferring while this handle is alive.
    sleep(Duration::from_millis(300)).await;

    let info = get_info(&handle).await;
    assert_eq!(info.state, SessionState::Idle);
    assert_eq!(
        registered_actor_id(&registry, &key),
        Some(actor_id),
        "the same actor should still be registered"
    );
}

#[tokio::test]
async fn reap_removes_an_idle_actor_once_its_handle_is_dropped() {
    let (_guard, _sandbox, registry) = registry_sandbox().await;
    let key = key("plain", "reap-after-handle-dropped");
    let handle = registry.get_or_spawn(key.clone());

    arm_reap_deadline(&handle).await;
    drop(handle);

    wait_until_unregistered(&registry, &key).await;
}

#[tokio::test]
async fn get_or_spawn_replaces_an_entry_whose_actor_has_stopped() {
    let (_guard, _sandbox, registry) = registry_sandbox().await;
    let key = key("plain", "stopped-actor-entry");

    // An actor that died without deregistering, e.g. a panicking task.
    let (dead_tx, dead_rx) = mpsc::channel(1);
    drop(dead_rx);
    registry.map.insert(
        key.clone(),
        SessionHandle {
            tx: dead_tx,
            actor_id: u64::MAX,
        },
    );

    let handle = registry.get_or_spawn(key.clone());
    assert_ne!(handle.actor_id, u64::MAX, "expected a fresh actor");
    assert_eq!(get_info(&handle).await.state, SessionState::Idle);
    assert_eq!(registered_actor_id(&registry, &key), Some(handle.actor_id));
}

#[tokio::test]
async fn has_session_rejects_a_closed_actor_channel() {
    let (_guard, _sandbox, registry) = registry_sandbox().await;
    let key = key("plain", "closed-actor-entry");
    let (dead_tx, dead_rx) = mpsc::channel(1);
    drop(dead_rx);
    registry.map.insert(
        key.clone(),
        SessionHandle {
            tx: dead_tx,
            actor_id: u64::MAX,
        },
    );

    assert!(
        !registry.has_session(&key),
        "a stale map entry is not a live session actor"
    );
}

#[tokio::test]
async fn same_session_handoff_queues_without_self_await() {
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent_with_front_matter(
        "loop-agent",
        "model: openai:gpt-4o\nuse_tools: loop-agent_session_handoff",
        "You are the loop agent.",
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let registry = registry_with_call_fn(self_handoff_call_fn(Arc::clone(&calls)));
    let handle = registry.get_or_spawn(key("loop-agent", "same-session"));
    let mut events = subscribe(&handle).await.events;
    let first_run = match prompt(&handle, "start").await {
        PromptResult::Accepted { run_id } => run_id,
        other => panic!("expected accepted prompt, got {other:?}"),
    };

    let mut committed = false;
    let mut finished_runs = 0;
    tokio::time::timeout(Duration::from_secs(10), async {
        while finished_runs < 2 {
            match events.recv().await.expect("session event") {
                Event::Custom(event) if event.name == "session_handoff" => {
                    assert_eq!(event.value["agent"], "loop-agent");
                    assert_eq!(event.value["session_id"], "same-session");
                    committed = true;
                }
                Event::RunFinished(event) => {
                    if event.run_id.to_string() == first_run {
                        assert!(
                            committed,
                            "handoff commitment must precede the source RUN_FINISHED"
                        );
                    }
                    finished_runs += 1;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("same-session handoff should not deadlock");

    assert!(committed, "self handoff should commit before completion");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn reap_predicate_rejects_buffered_mailbox_commands() {
    let (_guard, _sandbox, registry) = registry_sandbox().await;
    let actor_key = key("plain", "buffered-mailbox");
    let (tx, rx) = mpsc::channel(COMMAND_BUFFER);
    let (broadcast_tx, _) = broadcast::channel(BROADCAST_BUFFER);
    let (run_done_tx, run_done_rx) = mpsc::channel(COMMAND_BUFFER);
    tx.send(SessionCommand::Unsubscribe)
        .await
        .expect("buffer command");
    let actor = SessionActor {
        key: actor_key,
        actor_id: u64::MAX,
        registry: registry.map.clone(),
        rx,
        broadcast_tx,
        subscribers: 0,
        state: SessionState::Idle,
        pending: VecDeque::new(),
        active_run: None,
        run_done_tx,
        run_done_rx,
        run_done_task: None,
        reap_ttl: Duration::from_millis(50),
        reap_deadline: Some(Instant::now() - Duration::from_millis(1)),
        history_snapshot: Vec::new(),
        history_warnings: Vec::new(),
        actor_config: SessionActorConfig {
            base_config: Config::default(),
            call_fn: Some(noop_call_fn()),
            local_worker: Arc::new(Mutex::new(None)),
        },
    };

    assert!(
        !actor.should_reap(),
        "an expired idle actor must drain queued commands before reaping"
    );
}

#[tokio::test]
async fn a_stopping_actor_leaves_its_replacement_registered() {
    let (_guard, _sandbox, registry) = registry_sandbox().await;
    let key = key("plain", "replacement-survives");

    let outgoing = registry.get_or_spawn(key.clone());
    // Unregister the first actor so the key is free, then register a second one for it. The
    // first actor now only stops once `outgoing` goes away.
    registry.map.remove(&key);
    let replacement = registry.get_or_spawn(key.clone());
    assert_ne!(outgoing.actor_id, replacement.actor_id);

    drop(outgoing);
    sleep(Duration::from_millis(200)).await;

    assert_eq!(
        registered_actor_id(&registry, &key),
        Some(replacement.actor_id),
        "the outgoing actor must not remove its replacement's entry"
    );
    assert_eq!(get_info(&replacement).await.state, SessionState::Idle);
}
