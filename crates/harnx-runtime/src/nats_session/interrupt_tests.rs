use super::interrupt::*;
use crate::nats_session_log::NatsSessionLog;
use crate::nats_test_common::spawn_nats_server;
use crate::nats_worker::SessionActivationRoute;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;

fn user(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}
fn request(session: &str, id: &str) -> InterruptRequest {
    InterruptRequest {
        session_id: session.into(),
        cluster: "local".into(),
        replicas: 1,
        cancellation_id: id.into(),
        requested_by: "test".into(),
        reason: "test".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_session_is_not_appended_to() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let client = async_nats::connect(server.url()).await.unwrap();
    let js = async_nats::jetstream::new(client.clone());
    let log = NatsSessionLog::new_with_replicas(js.clone(), "idle-s", 1);
    log.append_event_async(&user("hi")).await.unwrap();
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: 1,
        fence_token: 1,
        timestamp: None,
        usage: None,
    })
    .await
    .unwrap();
    let outcome = interrupt_session(
        &js,
        &client,
        &SessionActivationRoute::ClusterShared,
        request("idle-s", "c1"),
    )
    .await
    .unwrap();
    assert_eq!(outcome, InterruptOutcome::Idle);
    assert_eq!(log.load_events_async().await.unwrap().len(), 2);
}

/// The `cancellation_id` is what makes a repeat a repeat: the same one sent
/// twice, and a different one sent after it, both find the turn already
/// interrupted and leave the single `Cancel` where it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_interrupt_is_idempotent_by_cancellation_id() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let client = async_nats::connect(server.url()).await.unwrap();
    let js = async_nats::jetstream::new(client.clone());
    let log = NatsSessionLog::new_with_replicas(js.clone(), "busy-s", 1);
    log.append_event_async(&user("go")).await.unwrap();
    let first = interrupt_session(
        &js,
        &client,
        &SessionActivationRoute::ClusterShared,
        request("busy-s", "c1"),
    )
    .await
    .unwrap();
    let InterruptOutcome::Accepted { cancel_seq } = first else {
        panic!("{first:?}")
    };
    assert_eq!(cancel_seq, 2);
    let repeat = interrupt_session(
        &js,
        &client,
        &SessionActivationRoute::ClusterShared,
        request("busy-s", "c1"),
    )
    .await
    .unwrap();
    assert_eq!(
        repeat,
        InterruptOutcome::AlreadyInterrupted { cancel_seq: 2 },
        "the same cancellation id names the Cancel already in the log"
    );
    let again = interrupt_session(
        &js,
        &client,
        &SessionActivationRoute::ClusterShared,
        request("busy-s", "c2"),
    )
    .await
    .unwrap();
    assert_eq!(
        again,
        InterruptOutcome::AlreadyInterrupted { cancel_seq: 2 },
        "a second interrupt does not start a second interruption"
    );
    let entries = log.load_events_async().await.unwrap();
    assert_eq!(entries.len(), 2);
    assert!(matches!(
        &entries[1].1,
        SessionLogEntry::Cancel { cancellation_id: Some(id), requested_by: Some(by), .. }
            if id == "c1" && by == "test"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_user_message_does_not_prevent_the_cancel() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let client = async_nats::connect(server.url()).await.unwrap();
    let js = async_nats::jetstream::new(client.clone());
    let log = NatsSessionLog::new_with_replicas(js.clone(), "race-s", 1);
    log.append_event_async(&user("go")).await.unwrap();
    // Simulate a frontend appending between the interrupt's tail read and its CAS.
    let racer = log.clone();
    let js2 = js.clone();
    let client2 = client.clone();
    let interrupt = tokio::spawn(async move {
        interrupt_session(
            &js2,
            &client2,
            &SessionActivationRoute::ClusterShared,
            request("race-s", "c1"),
        )
        .await
    });
    racer.append_event_async(&user("queued")).await.unwrap();
    let outcome = interrupt.await.unwrap().unwrap();
    assert!(matches!(outcome, InterruptOutcome::Accepted { .. }));
    let entries = log.load_events_async().await.unwrap();
    assert!(entries
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::Cancel { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_session_interrupt_uses_the_storage_key_and_reports_outcome() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let (session, log) =
        crate::nats_session::test_support::session_with_log(&server, "agent-a", "s1").await;
    log.append_event_async(&user("go")).await.unwrap();
    assert!(matches!(
        session.interrupt("test").await.unwrap(),
        InterruptOutcome::Accepted { cancel_seq: 2 }
    ));
    assert!(matches!(
        session.interrupt("test").await.unwrap(),
        InterruptOutcome::AlreadyInterrupted { cancel_seq: 2 }
    ));
}

/// Plant a payload no `SessionLogEntry` decode accepts at the next sequence
/// of `session`'s log, the way a reader that walked past its own turn would
/// trip over anything it does not understand.
async fn plant_undecodable_entry(js: &async_nats::jetstream::Context, session: &str) {
    js.publish(
        crate::nats_session_log::subject_for_session(session),
        "!! not a session log entry".into(),
    )
    .await
    .unwrap()
    .await
    .unwrap();
}

/// A prompt's interrupt watch runs every quarter second for as long as the
/// turn does, and `prompt_interrupted_at` only ever inspects entries above
/// the prompt. Reading below it is therefore pure cost — here made visible by
/// an entry below the prompt that no reader can decode, standing in for the
/// whole transcript a long session would otherwise re-read each poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_interrupt_watch_reads_nothing_below_the_prompt() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let (session, log) =
        crate::nats_session::test_support::session_with_log(&server, "agent-a", "narrow").await;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap());
    log.append_event_async(&user("an earlier turn"))
        .await
        .unwrap();
    plant_undecodable_entry(&js, session.storage_key()).await;

    let prompt_seq = log.append_event_async(&user("go")).await.unwrap();
    let cancel_seq = log
        .append_event_async(&SessionLogEntry::cancel_request("c1".into(), "test".into()))
        .await
        .unwrap();

    assert_eq!(
        session.prompt_interrupt_seq(prompt_seq).await.unwrap(),
        Some(cancel_seq)
    );
    assert_eq!(
        session.wait_for_prompt_interrupt(prompt_seq).await.unwrap(),
        cancel_seq
    );
}

/// A failed read is a broker hiccup, not an answer. The watch races the turn
/// itself in a `biased` select, so passing the first failure on ends a turn
/// that was never interrupted.
#[tokio::test(start_paused = true)]
async fn a_transient_read_failure_does_not_end_a_healthy_turn() {
    let reads = std::sync::atomic::AtomicUsize::new(0);
    let cancel_seq = await_prompt_interrupt(1, || {
        let attempt = reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async move {
            if attempt < 3 {
                anyhow::bail!("broker unreachable");
            }
            Ok(vec![(
                7,
                SessionLogEntry::cancel_request("c1".into(), "test".into()),
            )])
        }
    })
    .await
    .expect("a retried read must not fail the watch");
    assert_eq!(cancel_seq, 7);
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 4);
}

/// The retry is bounded: a broker that is really gone is still reported
/// rather than polled forever.
#[tokio::test(start_paused = true)]
async fn a_read_that_never_recovers_is_reported() {
    let reads = std::sync::atomic::AtomicUsize::new(0);
    let error = await_prompt_interrupt(1, || {
        reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async move { anyhow::bail!("broker unreachable") }
    })
    .await
    .expect_err("a read that keeps failing must surface");
    assert!(format!("{error:#}").contains("broker unreachable"));
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 5);
}

/// Acceptance is the append and nothing else. The wake-ups that follow it are
/// best-effort and can take arbitrarily long — here because a notify stream
/// already exists under the name this cluster's own would take, carrying the
/// other cluster's subject, so the activation publish waits for an ack that
/// nothing will ever send. A caller that bounds its interrupt must not be
/// told the interrupt failed when the `Cancel` it asked for is already
/// durable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_announcement_does_not_fail_an_accepted_interrupt() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let client = async_nats::connect(server.url()).await.unwrap();
    let js = async_nats::jetstream::new(client.clone());
    // A notify stream's name replaces the characters a NATS name cannot hold,
    // so "slow.cluster" and "slow_cluster" share one stream while their notify
    // subjects stay distinct: only the first one's is bound to it.
    crate::nats_worker::publish_session_activate(
        &js,
        "slow_cluster",
        &crate::nats_worker::SessionActivate::new("unrelated"),
        1,
    )
    .await
    .unwrap();
    // With nothing listening on the subject at all the broker answers "no
    // responders" at once. A plain subscriber makes the publish wait instead,
    // for a stream acknowledgement that will never come.
    let _listener = client
        .subscribe(crate::nats_worker::notify_subject("slow.cluster"))
        .await
        .unwrap();
    client.flush().await.unwrap();

    let session = crate::NatsSession::new(
        crate::NatsSessionConfig {
            cluster: "slow.cluster".into(),
            initializer: crate::nats_session_metadata::SessionInitializer::named(
                "agent-a",
                Default::default(),
            ),
            session_id: Some("stalled-announce".into()),
            activation_route: SessionActivationRoute::ClusterShared,
        },
        client,
        js,
        harnx_core::abort::create_abort_signal(),
    )
    .await
    .unwrap();
    let log = NatsSessionLog::new_with_replicas(
        session.jetstream().clone(),
        session.storage_key().to_string(),
        1,
    );
    log.append_event_async(&user("go")).await.unwrap();

    let started = tokio::time::Instant::now();
    let outcome = session.interrupt("test").await.unwrap();

    assert!(
        matches!(outcome, InterruptOutcome::Accepted { .. }),
        "the append landed, so the interrupt was accepted: {outcome:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "acceptance waited on the announcement: {:?}",
        started.elapsed()
    );
}

fn assistant(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text(text.to_string()),
        timestamp: None,
        fence_token: None,
    }
}

fn turn_end(through_seq: u64) -> SessionLogEntry {
    SessionLogEntry::TurnEnd {
        through_seq,
        fence_token: 1,
        timestamp: None,
        usage: None,
    }
}

fn cancel(id: &str) -> SessionLogEntry {
    SessionLogEntry::cancel_request(id.into(), "test".into())
}

fn tool_results() -> SessionLogEntry {
    SessionLogEntry::ToolResults {
        results: Vec::new(),
        timestamp: None,
    }
}

fn numbered(entries: Vec<SessionLogEntry>) -> Vec<(u64, SessionLogEntry)> {
    entries
        .into_iter()
        .enumerate()
        .map(|(i, e)| (i as u64 + 1, e))
        .collect()
}

/// `decide` only ever sees the log's last entry plus whatever a conflict
/// hands back, never the turn's start. A window without a terminator is a
/// turn in progress as far as the request can tell, so it appends; the
/// documented exception is an idle session whose last entry is a mutation
/// or control entry, which costs one stray `Cancel` marker.
#[test]
fn decide_from_the_tail_window() {
    assert_eq!(decide(&[]), Some(InterruptOutcome::Idle));
    assert_eq!(
        decide(&numbered(vec![turn_end(1)])),
        Some(InterruptOutcome::Idle)
    );
    assert_eq!(
        decide(&numbered(vec![cancel("c1")])),
        Some(InterruptOutcome::AlreadyInterrupted { cancel_seq: 1 })
    );
    assert_eq!(decide(&numbered(vec![tool_results()])), None);
    assert_eq!(decide(&numbered(vec![assistant("partial")])), None);
    assert_eq!(
        decide(&numbered(vec![tool_results(), turn_end(1)])),
        Some(InterruptOutcome::Idle)
    );
    assert_eq!(
        decide(&numbered(vec![tool_results(), turn_end(1), user("next")])),
        None
    );
    assert_eq!(
        decide(&numbered(vec![tool_results(), cancel("c1")])),
        Some(InterruptOutcome::AlreadyInterrupted { cancel_seq: 2 })
    );
    assert_eq!(
        decide(&numbered(vec![SessionLogEntry::Compress {
            prompt: "summary".into()
        }])),
        None
    );
}

/// The request reads the log's last entry and nothing else before its
/// fenced append, so a long transcript costs it nothing. An entry below the
/// tail that no reader can decode makes any wider read fail loudly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_reads_nothing_below_the_tail() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let client = async_nats::connect(server.url()).await.unwrap();
    let js = async_nats::jetstream::new(client.clone());
    let log = NatsSessionLog::new(js.clone(), "tail-s").with_replicas(1);
    log.append_event_async(&user("an earlier turn"))
        .await
        .unwrap();
    log.append_event_async(&turn_end(1)).await.unwrap();
    plant_undecodable_entry(&js, "tail-s").await;
    log.append_event_async(&user("go")).await.unwrap();
    log.append_event_async(&assistant("working on it"))
        .await
        .unwrap();
    assert!(log.load_events_async().await.is_err());

    let outcome = interrupt_session(
        &js,
        &client,
        &SessionActivationRoute::ClusterShared,
        request("tail-s", "c1"),
    )
    .await
    .unwrap();

    assert_eq!(outcome, InterruptOutcome::Accepted { cancel_seq: 6 });
}

/// Admitting a prompt only needs the tail to fence its append, so it must
/// not pay for the transcript above it either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_append_reads_nothing_below_the_tail() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let (session, log) =
        crate::nats_session::test_support::session_with_log(&server, "agent-a", "prompt-tail")
            .await;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap());
    log.append_event_async(&user("an earlier turn"))
        .await
        .unwrap();
    plant_undecodable_entry(&js, session.storage_key()).await;
    log.append_event_async(&turn_end(1)).await.unwrap();
    assert!(log.load_events_async().await.is_err());

    let seq = session
        .append_prompt_entry(&log, &user("go"), "m1")
        .await
        .unwrap();

    assert_eq!(seq, 4);
}
