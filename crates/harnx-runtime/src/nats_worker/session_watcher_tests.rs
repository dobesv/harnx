use super::session_watcher::{spawn_session_watcher, InterruptNotice, SessionWatcherCtx};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_test_common::spawn_nats_server;
use crate::nats_tool_provider::{InFlightRegistration, NatsInFlightCalls};
use futures_util::StreamExt;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use harnx_toolset::{CancelAcceptance, ControlKind, ControlMessage};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn user(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

/// Poll `condition` until it is true or `deadline` passes, without a fixed
/// sleep: the watcher's reaction is asynchronous and usually much faster than
/// the 2s ceiling the brief asks for.
async fn wait_until(deadline: tokio::time::Instant, mut condition: impl FnMut() -> bool) -> bool {
    while tokio::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    condition()
}

/// A session watcher already running against a freshly seeded session, with
/// two in-flight calls ("call-a" on "srv-a", "call-b" on "srv-b") registered
/// under its control subject for it to cancel on a foreign `Cancel`. Only
/// "call-a" ends its wait when a new message arrives.
struct WatchedSession {
    session_id: String,
    control_sub: async_nats::Subscriber,
    abort_signal: crate::utils::AbortSignal,
    pending_input: Arc<AtomicBool>,
    interrupted: Arc<parking_lot::Mutex<Option<InterruptNotice>>>,
    own_appends: Arc<AtomicU64>,
    in_flight: NatsInFlightCalls,
    client: async_nats::Client,
    watcher: tokio::task::JoinHandle<()>,
}

impl WatchedSession {
    async fn spawn(server_url: &str) -> Self {
        let client = async_nats::connect(server_url).await.unwrap();
        let jetstream = async_nats::jetstream::new(client.clone());
        let session_id = "watcher-s1".to_string();

        let log = NatsSessionLog::new_with_replicas(jetstream.clone(), session_id.clone(), 1);
        log.append_event_async(&user("hi")).await.unwrap(); // seq 1

        let control_subject = "watcher-s1.control-test".to_string();
        let control_sub = client.subscribe(control_subject.clone()).await.unwrap();

        let in_flight = NatsInFlightCalls::default();
        let watched_call = |call_id: &str, server: &str, waits: bool| InFlightRegistration {
            call_id: call_id.into(),
            server: server.into(),
            session_id: session_id.clone(),
            control_subject: control_subject.clone(),
            stop_on_new_message: waits,
        };
        // Nothing in this test triggers an in-flight failure, so the
        // notification receiver can be dropped right away; only the
        // registration itself, which the watcher's cancel path looks up, matters.
        in_flight
            .register(watched_call("call-a", "srv-a", true))
            .await;
        in_flight
            .register(watched_call("call-b", "srv-b", false))
            .await;
        let own_appends = Arc::new(AtomicU64::new(0));

        let abort_signal = crate::utils::create_abort_signal();
        let pending_input = Arc::new(AtomicBool::new(false));
        let interrupted: Arc<parking_lot::Mutex<Option<InterruptNotice>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let pending_compaction: Arc<parking_lot::Mutex<Option<String>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let watcher = spawn_session_watcher(SessionWatcherCtx {
            jetstream,
            client: client.clone(),
            session_id: session_id.clone(),
            start_after: 1,
            abort_signal: abort_signal.clone(),
            in_flight: in_flight.clone(),
            own_appends: Arc::clone(&own_appends),
            pending_input: Arc::clone(&pending_input),
            interrupted: Arc::clone(&interrupted),
            pending_compaction,
        });

        Self {
            session_id,
            control_sub,
            abort_signal,
            pending_input,
            interrupted,
            own_appends,
            in_flight,
            client,
            watcher,
        }
    }

    /// Append `entries` over a second connection, standing in for a frontend.
    async fn frontend_appends(&self, server_url: &str, entries: &[SessionLogEntry]) {
        let client = async_nats::connect(server_url).await.unwrap();
        let jetstream = async_nats::jetstream::new(client);
        let log = NatsSessionLog::new_with_replicas(jetstream, self.session_id.clone(), 1);
        for entry in entries {
            log.append_event_async(entry).await.unwrap();
        }
    }

    /// Answer the next control message with `acceptance` and return it.
    async fn answer_next_control(&mut self, acceptance: CancelAcceptance) -> ControlMessage {
        let message = tokio::time::timeout(Duration::from_secs(5), self.control_sub.next())
            .await
            .expect("timed out waiting for a control message")
            .expect("control subscription closed early");
        let control: ControlMessage = serde_json::from_slice(&message.payload).unwrap();
        let reply = message
            .reply
            .expect("a new-message signal asks for an answer");
        self.client
            .publish(
                reply,
                serde_json::to_vec(&control.acknowledgement(acceptance))
                    .unwrap()
                    .into(),
            )
            .await
            .unwrap();
        self.client.flush().await.unwrap();
        control
    }
}

/// Collect `count` tool-cancel messages published on `control_sub`, each
/// checked against `cancellation_id`, and return their call ids.
async fn collect_tool_cancels(
    control_sub: &mut async_nats::Subscriber,
    cancellation_id: &str,
    count: usize,
) -> Vec<String> {
    let mut received = Vec::new();
    for _ in 0..count {
        let message = tokio::time::timeout(Duration::from_secs(2), control_sub.next())
            .await
            .expect("timed out waiting for a tool cancel")
            .expect("control subscription closed early");
        let control: ControlMessage = serde_json::from_slice(&message.payload).unwrap();
        assert_eq!(control.kind, ControlKind::Cancel);
        assert_eq!(control.cancellation_id, cancellation_id);
        received.push(control.call_id);
    }
    received
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_cancel_interrupts_and_publishes_tool_cancels() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let mut watched = WatchedSession::spawn(server.url()).await;

    // A second connection stands in for the frontend that appends entries.
    let second_client = async_nats::connect(server.url()).await.unwrap();
    let second_js = async_nats::jetstream::new(second_client);
    let second_log =
        NatsSessionLog::new_with_replicas(second_js.clone(), watched.session_id.clone(), 1);

    // An undecodable entry (seq 2) must be skipped, not restart the
    // consumer: the Cancel appended right after it still has to land.
    second_js
        .publish(
            crate::nats_session_log::subject_for_session(&watched.session_id),
            bytes::Bytes::from_static(b"not a session log entry"),
        )
        .await
        .unwrap()
        .await
        .unwrap();

    second_log
        .append_event_async(&SessionLogEntry::cancel_request(
            "cancel-1".to_string(),
            "frontend".to_string(),
        ))
        .await
        .unwrap(); // seq 3

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    assert!(
        wait_until(deadline, || watched.abort_signal.aborted()).await,
        "watcher should fire the abort signal on a foreign Cancel, past the undecodable entry"
    );

    let notice = watched
        .interrupted
        .lock()
        .clone()
        .expect("interrupted should be set by the foreign Cancel");
    assert_eq!(
        notice.cancel_seq, 3,
        "seq 2 was the undecodable entry; the Cancel is seq 3"
    );
    assert_eq!(notice.cancellation_id.as_deref(), Some("cancel-1"));

    let mut received = collect_tool_cancels(&mut watched.control_sub, "cancel-1", 2).await;
    received.sort();
    assert_eq!(received, vec!["call-a".to_string(), "call-b".to_string()]);

    // A later user Message only flags pending input; it must not disturb the
    // abort signal that the Cancel already set.
    second_log
        .append_event_async(&user("more input"))
        .await
        .unwrap(); // seq 4
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    assert!(
        wait_until(deadline, || watched.pending_input.load(Ordering::Relaxed)).await,
        "a later user Message should set pending_input"
    );
    assert!(
        watched.abort_signal.aborted(),
        "the abort signal must remain set after a later Message"
    );

    watched.watcher.abort();
}

/// Longer than the resend delay, so a resend that should not happen would.
const QUIET: Duration = Duration::from_millis(800);

fn unsettled() -> CancelAcceptance {
    CancelAcceptance::Unknown {
        reason: "call is not running here yet; retry".into(),
    }
}

impl WatchedSession {
    /// The next control must be a new-message signal for the opted-in call.
    async fn answer_new_message(&mut self, acceptance: CancelAcceptance) {
        let control = self.answer_next_control(acceptance).await;
        assert_eq!(control.kind, ControlKind::NewMessage);
        assert_eq!(
            [&control.call_id, &control.server, &control.session_id],
            ["call-a", "srv-a", "watcher-s1"]
        );
    }

    /// Assert no further control message arrives for a while.
    async fn assert_quiet(&mut self, why: &str) {
        let next = tokio::time::timeout(QUIET, self.control_sub.next()).await;
        assert!(next.is_err(), "{why}: got {next:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_message_signals_only_the_calls_that_opted_in() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let mut watched = WatchedSession::spawn(server.url()).await;
    watched
        .frontend_appends(server.url(), &[user("steer")])
        .await;

    watched.answer_new_message(CancelAcceptance::Accepted).await;
    assert!(watched.in_flight.message_pending(&watched.session_id));
    watched
        .assert_quiet("call-b did not opt in, and an accepted signal is not sent again")
        .await;
    watched.watcher.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_message_resends_only_while_unsettled_and_within_its_budget() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let mut watched = WatchedSession::spawn(server.url()).await;
    watched
        .frontend_appends(server.url(), &[user("steer")])
        .await;
    watched.answer_new_message(unsettled()).await;
    watched
        .answer_new_message(CancelAcceptance::AlreadyFinished)
        .await;
    watched
        .assert_quiet("a finished call is not signalled again")
        .await;

    // A server answers Unknown for a call that already finished, too, so an
    // unsettled signal can't be resent for ever.
    watched
        .frontend_appends(server.url(), &[user("steer again")])
        .await;
    for _ in 0..crate::nats_tool_provider::NEW_MESSAGE_ATTEMPTS {
        watched.answer_new_message(unsettled()).await;
    }
    watched
        .assert_quiet("resends stop after their budget")
        .await;
    watched.watcher.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_message_ignores_own_appends_copies_timing_notices_and_runtime_notes() {
    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };
    let mut watched = WatchedSession::spawn(server.url()).await;
    // This worker's own append: the watcher skips everything through seq 2.
    watched.own_appends.store(2, Ordering::Relaxed);
    let receipt = harnx_core::timing_identity::TimingReceipt::initial(
        chrono::Utc::now(),
        "invocation".into(),
    );
    let notice = format!(
        "{}{} | elapsed",
        harnx_core::timing_identity::TIMING_HEADER,
        receipt.to_json()
    );
    let runtime_note = SessionLogEntry::Message {
        id: Some(harnx_core::runtime_note::new_runtime_note_message_id()),
        role: MessageRole::User,
        content: MessageContent::Text("[Runtime note] a sub-agent finished".into()),
        timestamp: None,
        fence_token: None,
    };
    // This worker's compaction copy of a prompt, seen before `own_appends`
    // caught up with it: everything the worker writes under its lease is
    // fenced, and nothing a frontend writes is.
    let own_copy = SessionLogEntry::Message {
        id: Some("prompt".into()),
        role: MessageRole::User,
        content: MessageContent::Text("hi".into()),
        timestamp: None,
        fence_token: Some(1),
    };
    let entries = [
        user("own"),
        user(&notice),
        runtime_note,
        own_copy,
        user("steer"),
    ];
    watched.frontend_appends(server.url(), &entries).await; // seq 2..=6

    // Only seq 6 signals: anything earlier would arrive first and show up as
    // a second signal here.
    watched.answer_new_message(CancelAcceptance::Accepted).await;
    watched
        .assert_quiet("own appends and copies, timing notices and runtime notes signal nothing")
        .await;
    watched.watcher.abort();
}
