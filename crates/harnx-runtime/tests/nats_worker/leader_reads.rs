use super::*;

/// Read-your-writes seam (Step 2): a backend wired with an `after_seq_observer`
/// advances the observer to the durable ack sequence on every append, and
/// `load_events_consistent_async` waits until the stream reflects at least
/// that sequence before returning. This is the mechanism that lets the
/// end-of-turn drain re-read observe the worker's own just-persisted barrier
/// instead of a stale tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_seq_observer_advances_and_consistent_read_honors_it() -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "ryw-observer";

    let observer = Arc::new(AtomicU64::new(0));
    let backend = generation::fenced_backend(&js, &storage_key(session_id))
        .await?
        .with_after_seq_observer(Arc::clone(&observer));

    // Append two entries; the observer must advance to the durable ack seq of
    // the latest append (monotonic via fetch_max).
    let user = SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("hi".to_string()),
        timestamp: None,
        fence_token: None,
    };
    let assistant = SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::Assistant,
        content: harnx_core::message::MessageContent::Text("there".to_string()),
        timestamp: None,
        fence_token: Some(7),
    };

    let seq1 = backend.append_event_blocking(&user)?;
    assert_eq!(
        observer.load(AtomicOrdering::SeqCst),
        seq1,
        "observer must advance to the first append's ack sequence"
    );
    let seq2 = backend.append_event_blocking(&assistant)?;
    assert!(seq2 > seq1, "second append must get a higher sequence");
    assert_eq!(
        observer.load(AtomicOrdering::SeqCst),
        seq2,
        "observer must advance to the latest append's ack sequence"
    );

    // The consistent read uses the observer's high-water mark; it must return a
    // tail that reflects at least seq2 (both entries visible), never a stale
    // read missing the worker's own latest barrier.
    let entries = backend.load_events_consistent_async().await?;
    assert!(
        entries.iter().any(|(s, _)| *s == seq2),
        "consistent read must include the latest appended entry (seq {seq2}); got {entries:?}"
    );
    assert_eq!(
        final_assistant_texts(&entries),
        vec!["there".to_string()],
        "consistent read must surface the just-persisted assistant barrier"
    );
    Ok(())
}

/// Verifies `load_events_latest_async` reads leader-authoritative tail end-to-end.
///
/// On single-node NATS this cannot differ from old `stream.info()` path because
/// there is no replication lag, so this is a forward behavioral guarantee rather
/// than a fail-on-revert differential. Real #917 bug requires multi-node
/// STREAM.INFO-vs-leader divergence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_events_latest_async_reads_leader_authoritative_tail() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let js = local_test_nats(server.url()).await?;
    let session_id = "latest-tail-test";
    let log = NatsSessionLog::new(js, storage_key(session_id));

    let user_seq = log
        .append_event_async(&append_user_message_entry(
            "msg-to-retract",
            "please ignore this",
        ))
        .await?;
    let retract_seq = log
        .append_event_async(&SessionLogEntry::EditEntries {
            from: user_seq as usize,
            to: user_seq as usize,
            replacements: vec![],
        })
        .await?;

    let entries = log.load_events_latest_async().await?;
    let max_seq = entries.iter().map(|(seq, _)| *seq).max().unwrap_or(0);
    assert!(
        max_seq >= retract_seq,
        "latest read must include retract seq {retract_seq}, got max seq {max_seq}"
    );
    assert!(
        entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::EditEntries { .. })),
        "latest read must include EditEntries retract entry"
    );

    let effective_messages = reconstruct_state_from_nats(&entries).next_turn_messages;
    assert!(
        !effective_messages
            .iter()
            .any(|message| message.content.to_text().contains("please ignore this")),
        "retracted user text must not survive reconstruct_state_from_nats fold"
    );

    Ok(())
}

/// Structural regression guard for #917.
///
/// Runtime difference only appears under multi-node replication lag, which CI's
/// single-node NATS cannot reproduce. If production structure changes
/// legitimately, update this guard.
#[test]
fn injection_decision_points_use_leader_authoritative_read() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let agent_loop =
        std::fs::read_to_string(manifest_dir.join("src/nats_worker/agent_loop/mod.rs"))
            .expect("agent_loop/mod.rs must be readable");

    assert!(
        agent_loop.contains("build_mid_turn_injection_callback"),
        "agent_loop.rs must still define build_mid_turn_injection_callback"
    );
    assert!(
        agent_loop.contains("load_events_latest_async"),
        "mid-turn injection callback must use load_events_latest_async"
    );
    assert!(
        !agent_loop.contains("load_events_consistent_async"),
        "agent_loop.rs must not route mid-turn injection through load_events_consistent_async"
    );

    // The turn-decision logic that used to live entirely in daemon.rs is now
    // split across the daemon_* siblings it was extracted into (turn-input
    // derivation and session execution), so check the whole family rather
    // than one file that no longer contains all three decision points.
    let daemon_family = [
        "daemon",
        "daemon_turn_input",
        "daemon_session_exec",
        "session_turn",
    ]
    .iter()
    .map(|name| {
        std::fs::read_to_string(manifest_dir.join(format!("src/nats_worker/{name}.rs")))
            .unwrap_or_else(|error| panic!("{name}.rs must be readable: {error}"))
    })
    .collect::<Vec<_>>()
    .join("\n");

    // Five leader reads: reconstruction and the continuation drain in
    // daemon_turn_input, the session watcher's start sequence in
    // daemon_session_exec, and the pending-HITL and drain checks in session_turn.
    assert_eq!(
        daemon_family
            .lines()
            .filter(|line| line.contains("load_events_latest_async()"))
            .count(),
        5,
        "turn decisions use leader reads; failure coverage uses its exact committed Error sequence"
    );
    assert_eq!(
        daemon_family
            .matches("load_events_consistent_async")
            .count(),
        0,
        "the daemon family's turn-decision logic must not use load_events_consistent_async"
    );
}

/// Exercises the NoMessageFound / empty-stream branch of load_events_latest_async (#917).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_events_latest_async_empty_stream_returns_empty() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let js = local_test_nats(server.url()).await?;
    let session_id = "latest-tail-empty-stream-test";
    let log = NatsSessionLog::new(js, storage_key(session_id));

    let entries = log.load_events_latest_async().await?;
    assert!(entries.is_empty(), "empty stream should return empty Vec");

    Ok(())
}
