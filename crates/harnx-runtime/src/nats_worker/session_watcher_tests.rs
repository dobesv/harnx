use super::session_watcher::{spawn_session_watcher, InterruptNotice, SessionWatcherCtx};
use crate::nats_session_log::NatsSessionLog;
use crate::nats_test_common::spawn_nats_server;
use crate::nats_tool_provider::{InFlightRegistration, NatsInFlightCalls};
use futures_util::StreamExt;
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use harnx_toolset::{ControlKind, ControlMessage};
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
/// under its control subject for it to cancel on a foreign `Cancel`.
struct WatchedSession {
    session_id: String,
    control_sub: async_nats::Subscriber,
    abort_signal: crate::utils::AbortSignal,
    pending_input: Arc<AtomicBool>,
    interrupted: Arc<parking_lot::Mutex<Option<InterruptNotice>>>,
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
        let watched_call = |call_id: &str, server: &str| InFlightRegistration {
            call_id: call_id.into(),
            server: server.into(),
            session_id: session_id.clone(),
            control_subject: control_subject.clone(),
        };
        // Nothing in this test triggers an in-flight failure, so the
        // notification receiver can be dropped right away; only the
        // registration itself, which the watcher's cancel path looks up, matters.
        in_flight.register(watched_call("call-a", "srv-a")).await;
        in_flight.register(watched_call("call-b", "srv-b")).await;

        let abort_signal = crate::utils::create_abort_signal();
        let pending_input = Arc::new(AtomicBool::new(false));
        let interrupted: Arc<parking_lot::Mutex<Option<InterruptNotice>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let watcher = spawn_session_watcher(SessionWatcherCtx {
            jetstream,
            client,
            session_id: session_id.clone(),
            start_after: 1,
            abort_signal: abort_signal.clone(),
            in_flight,
            own_appends: Arc::new(AtomicU64::new(0)),
            pending_input: Arc::clone(&pending_input),
            interrupted: Arc::clone(&interrupted),
        });

        Self {
            session_id,
            control_sub,
            abort_signal,
            pending_input,
            interrupted,
            watcher,
        }
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
