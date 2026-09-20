use super::*;

static SOLO_TURN_ROUNDS: AtomicUsize = AtomicUsize::new(0);
static SOLO_TURN_INJECTIONS: AtomicUsize = AtomicUsize::new(0);
static LATE_MSG_READY: LazyLock<Notify> = LazyLock::new(Notify::new);
static LATE_MSG_DONE: LazyLock<Notify> = LazyLock::new(Notify::new);
static LATE_MSG_ROUNDS: AtomicUsize = AtomicUsize::new(0);
static LATE_MSG_INJECTIONS: AtomicUsize = AtomicUsize::new(0);
static WIRE_CALLS: AtomicUsize = AtomicUsize::new(0);
static WIRE_PROMPT_COPIES: AtomicUsize = AtomicUsize::new(0);

/// A tool-call round while `round <= max_rounds`, then a final "done" text.
/// Shared by the stub call_fns below, which differ only in how they detect
/// and count injected text.
#[allow(clippy::type_complexity)]
fn echo_round_or_done(
    round: usize,
    max_rounds: usize,
    call_id: String,
) -> Result<(String, Option<String>, Vec<ToolCall>, CompletionTokenUsage)> {
    if round <= max_rounds {
        Ok((
            format!("round-{round}"),
            None,
            vec![ToolCall::new(
                "echo".to_string(),
                json!({}),
                Some(call_id),
                None,
            )],
            CompletionTokenUsage::default(),
        ))
    } else {
        Ok((
            "done".to_string(),
            None,
            vec![],
            CompletionTokenUsage::default(),
        ))
    }
}

/// Stub LLM that emits a tool call for the first `TOOL_ROUNDS` calls and a
/// final text afterwards, recording how many calls arrived carrying an
/// `injected_user_text`. Nothing appends a mid-turn message in this scenario,
/// so a correct worker never injects.
fn solo_turn_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    const TOOL_ROUNDS: usize = 3;
    Arc::new(move |input, _config, _abort| {
        if input.injected_user_text().is_some() {
            SOLO_TURN_INJECTIONS.fetch_add(1, Ordering::SeqCst);
        }
        let round = SOLO_TURN_ROUNDS.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { echo_round_or_done(round, TOOL_ROUNDS, format!("call-{round}")) })
    })
}

/// Stub LLM for the "one queued message, many tool rounds" scenario: blocks on
/// the first call so the test can append a message mid-turn, then keeps the
/// tool loop running for several more rounds while counting how many of them
/// arrive carrying the queued text.
fn repeated_round_injection_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    const TOOL_ROUNDS: usize = 5;
    Arc::new(move |input, _config, _abort| {
        if input
            .injected_user_text()
            .is_some_and(|text| text.contains("late message"))
        {
            LATE_MSG_INJECTIONS.fetch_add(1, Ordering::SeqCst);
        }
        let round = LATE_MSG_ROUNDS.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move {
            if round == 1 {
                LATE_MSG_READY.notify_one();
                LATE_MSG_DONE.notified().await;
            }
            echo_round_or_done(round, TOOL_ROUNDS, format!("late-call-{round}"))
        })
    })
}

/// Stub LLM that records how many copies of the prompt the FIRST wire request
/// of the turn carries. `build_messages` is the same function the real client
/// calls, so this sees exactly what the model would.
fn wire_message_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |input, config, _abort| {
        if WIRE_CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
            let copies = harnx_runtime::config::input::build_messages(input, config)
                .map(|messages| {
                    messages
                        .iter()
                        .filter(|message| {
                            message.role.is_user() && message.content.to_text() == "seed message"
                        })
                        .count()
                })
                .unwrap_or(0);
            WIRE_PROMPT_COPIES.store(copies, Ordering::SeqCst);
        }
        Box::pin(async move {
            Ok((
                "done".to_string(),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    })
}

/// A lone user prompt driving a multi-round tool loop must never be re-injected
/// into its own turn.
///
/// The mid-round injection cursor must not fold the prompt that started the
/// activation back into a later tool round. Each injection is persisted as a
/// user log entry, so one bad cursor creates another candidate for every later
/// round and can leave a duplicate continuation after the turn ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lone_prompt_is_not_reinjected_across_tool_rounds() -> Result<()> {
    reset_test_state();
    SOLO_TURN_ROUNDS.store(0, Ordering::SeqCst);
    SOLO_TURN_INJECTIONS.store(0, Ordering::SeqCst);
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let config = local_nats_runtime_config(server.url());
    let daemon =
        spawn_worker_daemon_with_call_fn(config, "worker-solo-turn", solo_turn_call_fn()).await?;

    let js = local_test_nats(server.url()).await?;
    let session_id = "solo-turn-no-reinjection";
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);

    log.append_event_async(&append_user_message_entry("user-1", "seed message"))
        .await?;
    activate_session(&js, session_id).await?;

    // Four calls = three tool rounds plus the final text that ends the turn.
    wait_until(CI_SAFE_TIMEOUT, || {
        SOLO_TURN_ROUNDS.load(Ordering::SeqCst) >= 4
    })
    .await?;
    // Let the end-of-turn drain decide whether to run a continuation turn.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert_eq!(
        SOLO_TURN_INJECTIONS.load(Ordering::SeqCst),
        0,
        "no client appended a mid-turn message, so the worker must not inject one"
    );
    assert_eq!(
        SOLO_TURN_ROUNDS.load(Ordering::SeqCst),
        4,
        "the drain must not run a continuation turn for a prompt already answered"
    );

    let entries = log.load_events_async().await?;
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)?;
    assert_eq!(
        user_message_texts(&effective),
        vec!["seed message".to_string()],
        "the prompt must appear exactly once in the effective log"
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// One message queued mid-turn must reach the model exactly once, no matter how
/// many tool rounds follow.
///
/// The injected text is folded from the log, where the client already appended
/// it. If the worker persists it a second time as its own user entry, the next
/// round's fold sees that copy as another unanswered message and injects it
/// again — one self-sustaining duplicate per round for the rest of the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_message_is_injected_once_across_many_tool_rounds() -> Result<()> {
    reset_test_state();
    LATE_MSG_ROUNDS.store(0, Ordering::SeqCst);
    LATE_MSG_INJECTIONS.store(0, Ordering::SeqCst);
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let config = local_nats_runtime_config(server.url());
    let daemon = spawn_worker_daemon_with_call_fn(
        config,
        "worker-repeated-injection",
        repeated_round_injection_call_fn(),
    )
    .await?;

    let js = local_test_nats(server.url()).await?;
    let session_id = "repeated-round-injection";
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);

    // Register the wakeup before activating so notify_one() cannot be lost.
    let ready_fut = LATE_MSG_READY.notified();
    log.append_event_async(&append_user_message_entry("user-1", "seed message"))
        .await?;
    activate_session(&js, session_id).await?;
    ready_fut.await;

    log.append_event_async(&append_user_message_entry("user-2", "late message"))
        .await?;
    LATE_MSG_DONE.notify_one();

    // Six calls = five tool rounds plus the final text that ends the turn.
    wait_until(CI_SAFE_TIMEOUT, || {
        LATE_MSG_ROUNDS.load(Ordering::SeqCst) >= 6
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert_eq!(
        LATE_MSG_INJECTIONS.load(Ordering::SeqCst),
        1,
        "the queued message must be injected into exactly one round"
    );

    let entries = log.load_events_async().await?;
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)?;
    assert_eq!(
        user_message_texts(&effective),
        vec!["seed message".to_string(), "late message".to_string()],
        "each user message must appear exactly once in the effective log"
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// The worker must send the prompt to the model once, not twice.
///
/// `derive_turn_input` folds the user messages out of the durable log, so the
/// session loaded for the turn already ends with them. Appending
/// `input.message_content()` on top of that history — which is what
/// `build_messages` does for an ordinary prompt — sends the whole thing again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_turn_sends_the_prompt_to_the_model_once() -> Result<()> {
    reset_test_state();
    WIRE_CALLS.store(0, Ordering::SeqCst);
    WIRE_PROMPT_COPIES.store(0, Ordering::SeqCst);
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let config = local_nats_runtime_config(server.url());
    let daemon =
        spawn_worker_daemon_with_call_fn(config, "worker-wire-messages", wire_message_call_fn())
            .await?;

    let js = local_test_nats(server.url()).await?;
    let session_id = "wire-prompt-once";
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);

    log.append_event_async(&append_user_message_entry("user-1", "seed message"))
        .await?;
    activate_session(&js, session_id).await?;

    wait_until(CI_SAFE_TIMEOUT, || WIRE_CALLS.load(Ordering::SeqCst) >= 1).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        WIRE_PROMPT_COPIES.load(Ordering::SeqCst),
        1,
        "the model must see the prompt exactly once"
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// Worker-side retract: a user message that is retracted BEFORE the worker drains
/// must NOT be folded into worker input and NOT executed.
///
/// This test proves the worker path honors EditEntries mutations. Without the fix,
/// `fold_new_user_messages_since` would iterate raw entries and fold the retracted
/// message, causing unwanted worker execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retracted_mid_tool_round_message_is_not_injected() -> Result<()> {
    reset_test_state();
    MID_ROUND_RELOAD_SEEN.store(0, Ordering::SeqCst);
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
    let daemon =
        spawn_worker_daemon_with_call_fn(config, "worker-mid-round-retract", mid_round_call_fn())
            .await?;

    let js = local_test_nats(server.url()).await?;
    let session_id = "mid-round-retracted-injection";
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);
    let ready_fut = MID_ROUND_APPEND_READY.notified();

    log.append_event_async(&append_user_message_entry("user-1", "seed message"))
        .await?;
    activate_session(&js, session_id).await?;
    ready_fut.await;

    let retracted_seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some("msg-retracted-before-injection".to_string()),
            role: MessageRole::User,
            content: harnx_core::message::MessageContent::Text("late message".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    let retracted_seq = usize::try_from(retracted_seq).expect("JetStream seq fits usize");
    log.append_event_async(&SessionLogEntry::EditEntries {
        from: retracted_seq,
        to: retracted_seq,
        replacements: vec![],
    })
    .await?;

    MID_ROUND_APPEND_DONE.notify_one();
    wait_until(CI_SAFE_TIMEOUT, || {
        MID_ROUND_RELOAD_SEEN.load(Ordering::SeqCst) >= 1
    })
    .await?;

    let entries = log.load_events_async().await?;
    let assistants = final_assistant_texts(&entries);

    assert_eq!(
        MID_ROUND_FINAL_CALLS.load(Ordering::SeqCst),
        0,
        "retracted message should not trigger injected follow-up round"
    );
    assert!(
        assistants.is_empty(),
        "no final assistant turn should be persisted without injected message"
    );
    // The raw durable log still physically contains the retracted "late message"
    // entry plus its EditEntries tombstone (retract is a tombstone, never a
    // physical delete). The EFFECTIVE stream (mutations applied via
    // reconstruct_state_from_nats) is what the worker folds, and it must exclude
    // the retracted message from next_turn_messages.
    let effective_user_texts: Vec<String> = reconstruct_state_from_nats(&entries)
        .next_turn_messages
        .iter()
        .map(|m| m.content.to_text())
        .collect();
    assert!(
        !effective_user_texts.iter().any(|t| t == "late message"),
        "effective next-turn stream should exclude retracted mid-round message, got: {effective_user_texts:?}"
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
