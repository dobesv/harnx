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
fn user_with_id(id: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some(id.into()),
        role: MessageRole::User,
        content: MessageContent::Text("text".into()),
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
    let (session, log, js) =
        session_with_jetstream_above_earlier_turn(&server, "agent-a", "narrow").await;
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
        (cancel_seq, None)
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
        handoff_selection: None,
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

/// A session plus a fresh JetStream handle, seeded with an earlier turn
/// the fixture plants an undecodable entry under: readers must stop above
/// it, never decode it.
async fn session_with_jetstream_above_earlier_turn(
    server: &crate::nats_test_common::NatsServerHandle,
    agent: &str,
    id: &str,
) -> (
    crate::nats_session::NatsSession,
    NatsSessionLog,
    async_nats::jetstream::Context,
) {
    let (session, log) =
        crate::nats_session::test_support::session_with_log(server, agent, id).await;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await.unwrap());
    log.append_event_async(&user("an earlier turn"))
        .await
        .unwrap();
    (session, log, js)
}

/// A session whose admitted prompt `a1` is already ended by its own
/// TurnEnd, returning the session, its log, and the prompt's sequence.
async fn settled_prompt_session(
    server: &crate::nats_test_common::NatsServerHandle,
    agent: &str,
    id: &str,
) -> (crate::nats_session::NatsSession, NatsSessionLog, u64) {
    let (session, log) =
        crate::nats_session::test_support::session_with_log(server, agent, id).await;
    let prompt = crate::nats_session::test_support::append_admitted_user(&log, "a1", "first")
        .await
        .unwrap();
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: prompt,
        fence_token: 1,
        timestamp: None,
        usage: None,
    })
    .await
    .unwrap();
    (session, log, prompt)
}

/// Drive the steered-stop append path directly with a recorded owning
/// invocation, bypassing the gate in [`super::NatsSession::interrupt_prompt`]:
/// the per-attempt re-derivation inside the loop is what this exercises
/// (issue #2774).
async fn stop_with_recorded_owner(
    server: &crate::nats_test_common::NatsServerHandle,
    storage: &str,
    cancellation_id: &str,
    prompt_seq: u64,
    owning_head: Option<String>,
) -> InterruptOutcome {
    let client = async_nats::connect(server.url()).await.unwrap();
    let js = async_nats::jetstream::new(client.clone());
    interrupt_invocation_stop_steered(
        &js,
        &client,
        &SessionActivationRoute::ClusterShared,
        request(storage, cancellation_id),
        InvocationStop {
            prompt_seq,
            owning_head,
        },
    )
    .await
    .unwrap()
}

/// A settled-prompt session with one steered row appended past the
/// terminator and still uncovered, so the drain looks live to the log
/// alone.
async fn session_with_outstanding_steered_row(
    server: &crate::nats_test_common::NatsServerHandle,
    agent: &str,
    id: &str,
) -> (crate::nats_session::NatsSession, NatsSessionLog, u64) {
    let (session, log, prompt) = settled_prompt_session(server, agent, id).await;
    log.append_event_async(&user_with_id("s1")).await.unwrap();
    (session, log, prompt)
}

/// The ownership gate's reads are bounded (issue #2774, review F3): a read
/// that never completes surfaces as an unconfirmed error — the stop fails
/// closed instead of hanging the caller or guessing ownership.
#[tokio::test(start_paused = true)]
async fn stalled_ownership_read_fails_closed() {
    let err = bound_ownership_read(std::future::pending::<anyhow::Result<Option<String>>>())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("unconfirmed"),
        "the timed-out read must surface as an unconfirmed stop, not a hang: {err}"
    );
}

/// A steered prompt's stop still lands while its drain is outstanding
/// (issue #2774, review F4): the first TurnEnd is not the end while steered
/// rows are live, and the owning invocation's stop appends the covering
/// Cancel — through the public `interrupt_prompt` gate, not just the raw
/// append path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_stop_lands_while_steered_rows_are_outstanding() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let (session, _log, first) =
        session_with_outstanding_steered_row(&server, "gate", "steered-stop-live").await;
    let outcome = session
        .interrupt_prompt(first, "steered stop")
        .await
        .unwrap();
    assert!(matches!(outcome, InterruptOutcome::Accepted { .. }));
}

/// The owner-named stop the worker deadline callbacks use (issue #2774):
/// with steered rows still outstanding past the prompt's first terminator,
/// naming the invocation that holds the head appends the covering Cancel
/// instead of reporting the expired invocation idle, so its continuation
/// cannot keep running past the deadline. A deadline naming an invocation
/// that does not hold the head stays fenced to the prompt's own turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_named_deadline_stop_cancels_an_outstanding_steered_drain() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let (session, log, first) =
        session_with_outstanding_steered_row(&server, "gate", "deadline-owner-stop").await;
    // A deadline for an invocation that lost the head must not touch the
    // drain: idle, nothing appended.
    let fenced = stop_with_recorded_owner(
        &server,
        session.storage_key(),
        "deadline-fenced",
        first,
        Some("not-the-head".into()),
    )
    .await;
    assert_eq!(fenced, InterruptOutcome::Idle);
    let entries = log.load_events_async().await.unwrap();
    assert!(
        entries
            .iter()
            .all(|(_, entry)| !matches!(entry, SessionLogEntry::Cancel { .. })),
        "a headless owner's deadline must not append a Cancel: {entries:?}"
    );
    // The head-owning invocation's deadline stops the outstanding drain.
    let owned = stop_with_recorded_owner(
        &server,
        session.storage_key(),
        "deadline-owned",
        first,
        Some("a1".into()),
    )
    .await;
    assert!(matches!(owned, InterruptOutcome::Accepted { .. }));
    let entries = log.load_events_async().await.unwrap();
    assert!(
        entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })),
        "the owner's deadline must append the covering Cancel: {entries:?}"
    );
}

/// A stale prompt-stop must not reach a later live invocation's rows
/// (issue #2774): only the prompt whose invocation still holds the admission
/// head owns a steered drain past its own terminator. After the head moved
/// on, the old prompt's stop reports idle and appends nothing, while the
/// live prompt's stop still lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_prompt_stop_reports_idle_and_leaves_the_later_invocation_alive() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let (session, log, first) = settled_prompt_session(&server, "gate", "stale-prompt-stop").await;
    let second = crate::nats_session::test_support::append_admitted_user(&log, "b1", "second")
        .await
        .unwrap();
    let outcome = session.interrupt_prompt(first, "stale stop").await.unwrap();
    assert_eq!(outcome, InterruptOutcome::Idle);
    let entries = log.load_events_async().await.unwrap();
    assert!(
        entries
            .iter()
            .all(|(_, entry)| !matches!(entry, SessionLogEntry::Cancel { .. })),
        "the stale stop must not append a Cancel: {entries:?}"
    );
    assert_eq!(entries.last().unwrap().0, second);
    // The live invocation's own stop still lands.
    let outcome = session.interrupt_prompt(second, "live stop").await.unwrap();
    assert!(matches!(outcome, InterruptOutcome::Accepted { .. }));
}

/// Every fail-closed arm of the steered-drain ownership gate (issue #2774):
/// a prompt row without a message id, an id that was never bound, and a
/// binding whose invocation holds no admission head all report no owning
/// invocation, so the stop stays scoped to the prompt's own turn instead of
/// guessing at a drain. The admitted live prompt is the control.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_drain_owner_fails_closed_for_idless_unbound_and_headless_prompts() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let (session, log) =
        crate::nats_session::test_support::session_with_log(&server, "gate", "gate-arms").await;
    let idless = log.append_event_async(&user("id-less")).await.unwrap();
    assert!(
        session.prompt_drain_owner(idless).await.unwrap().is_none(),
        "an id-less row was never admitted"
    );

    let unbound = log
        .append_event_async(&user_with_id("unbound"))
        .await
        .unwrap();
    assert!(
        session.prompt_drain_owner(unbound).await.unwrap().is_none(),
        "an unbound row never joined an invocation's drain"
    );

    // Bound to an invocation that holds no head: the binding alone proves
    // nothing.
    let store = crate::nats_session_metadata::SessionMetadataStore::ensure(log.jetstream(), 1)
        .await
        .unwrap();
    store
        .bind_prompt_admission(log.storage_key(), "bound-row", "inv-headless")
        .await
        .unwrap();
    let bound = log
        .append_event_async(&user_with_id("bound-row"))
        .await
        .unwrap();
    assert!(
        session.prompt_drain_owner(bound).await.unwrap().is_none(),
        "a binding without the admission head owns no live drain"
    );

    // Control: the admitted live prompt names its invocation.
    let live = crate::nats_session::test_support::append_admitted_user(&log, "live", "live")
        .await
        .unwrap();
    assert!(session.prompt_drain_owner(live).await.unwrap().is_some());
}

/// The append loop re-derives steered-drain ownership from the admission
/// head on every CAS attempt (issue #2774, review F2): a stop whose
/// recorded owning invocation no longer holds the head must report idle,
/// even though `steered_rows_outstanding` alone would call the drain live.
/// With the real owning invocation recorded, the same stop still lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_recorded_owner_cannot_stop_the_steered_drain_mid_loop() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let (session, log, first) =
        session_with_outstanding_steered_row(&server, "gate", "owner-recheck").await;

    let storage = session.storage_key().to_string();
    let outcome =
        stop_with_recorded_owner(&server, &storage, "c1", first, Some("inv-stale".into())).await;
    assert_eq!(outcome, InterruptOutcome::Idle);
    let entries = log.load_events_async().await.unwrap();
    assert!(
        entries
            .iter()
            .all(|(_, entry)| !matches!(entry, SessionLogEntry::Cancel { .. })),
        "a stale recorded owner must not append a Cancel: {entries:?}"
    );

    // Control: with the invocation that really holds the head recorded, the
    // stop reaches the outstanding drain and lands.
    let owner = session.prompt_drain_owner(first).await.unwrap();
    assert!(owner.is_some(), "the live head is a1's invocation");
    let outcome = stop_with_recorded_owner(&server, &storage, "c2", first, owner).await;
    assert!(matches!(outcome, InterruptOutcome::Accepted { .. }));
}

/// Admission cannot infer a safe run boundary from unreadable durable state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_append_fails_closed_on_unreadable_admission_history() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let (session, log, js) =
        session_with_jetstream_above_earlier_turn(&server, "agent-a", "prompt-tail").await;
    plant_undecodable_entry(&js, session.storage_key()).await;
    log.append_event_async(&turn_end(1)).await.unwrap();
    assert!(log.load_events_async().await.is_err());

    assert!(session
        .append_prompt_entry(&log, &user("go"), "m1", false)
        .await
        .is_err());
    assert_eq!(log.last_entry_async().await.unwrap().unwrap().0, 3);
}
