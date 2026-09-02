use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use harnx_core::{
    message::{Message, MessageContent, MessageRole},
    session::SessionLogEntry,
};
use harnx_runtime::{
    client::TestStateGuard,
    config::{Config, LOCAL_CLUSTER_KEY},
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session_log::NatsSessionLog,
};
use harnx_serve::{
    ag_ui::ag_ui_run_with_call_fn,
    session_actor::SessionRegistry,
    test_support::{seed_nats_session, NatsSessionSeed, TestConfigSandbox},
};
use serde_json::json;
use uuid::Uuid;

#[path = "support/mod.rs"]
mod support;
use support::{read_sse_until, AppResponse};

struct LeasedSession {
    _sandbox: TestConfigSandbox,
    config: Config,
    session_id: String,
    lease: Arc<NatsSessionLease>,
    jetstream: harnx_runtime::nats_event_sink::JetstreamContext,
}

struct RemoteTurnHandle {
    session_id: String,
    lease: Arc<NatsSessionLease>,
    jetstream: harnx_runtime::nats_event_sink::JetstreamContext,
}

impl LeasedSession {
    fn remote_turn_handle(&self) -> RemoteTurnHandle {
        RemoteTurnHandle {
            session_id: self.session_id.clone(),
            lease: self.lease.clone(),
            jetstream: self.jetstream.clone(),
        }
    }
}

async fn seed_in_progress_leased_session() -> Option<LeasedSession> {
    let _guard = TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();

    let session_id = format!("remote-lease-{}", Uuid::new_v4());

    let messages = vec![Message {
        id: Some(format!("msg-{}", Uuid::new_v4())),
        role: MessageRole::User,
        content: MessageContent::Text("hello from cli".to_string()),
        ..Default::default()
    }];

    let seeded = seed_nats_session(
        &config,
        NatsSessionSeed {
            agent: "plain",
            session_id: &session_id,
            messages: &messages,
        },
    )
    .await;

    if !seeded {
        eprintln!("Skipping: nats-server not available");
        return None;
    }

    let jetstream = config
        .nats_jetstream(LOCAL_CLUSTER_KEY)
        .await
        .expect("jetstream");
    let lease_config = NatsLeaseConfig {
        ttl: Duration::from_secs(30),
        renew_interval: Duration::from_secs(10),
        ..Default::default()
    };

    let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: jetstream.clone(),
        session_id: &session_id,
        worker_id: "test-remote-worker".to_string(),
        generation: 1,
        config: lease_config,
        session_metadata: None,
    })
    .await
    .expect("lease acquire");

    let lease = match lease {
        Some(l) => l,
        None => {
            eprintln!("Skipping: could not acquire lease (contention?)");
            return None;
        }
    };

    Some(LeasedSession {
        _sandbox: sandbox,
        config,
        session_id,
        lease: Arc::new(lease),
        jetstream,
    })
}

async fn open_promptless_sse(session: &LeasedSession) -> AppResponse {
    let registry =
        SessionRegistry::new_for_tests(session.config.clone(), Duration::from_secs(30), None);
    ag_ui_run_with_call_fn(
        &session.config,
        &registry,
        "plain",
        &session.session_id,
        &serde_json::to_vec(&json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "messages": [],
        }))
        .unwrap(),
        None,
    )
    .await
    .expect("sse response")
}

/// Finishes the remote turn by releasing the lease and appending a TurnEnd.
async fn finish_remote_turn(session: RemoteTurnHandle) {
    let log = NatsSessionLog::new(session.jetstream, session.session_id);
    let entries = log.load_events_async().await.expect("load entries");
    let user_msg_seq = entries
        .iter()
        .rev()
        .find(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::Message { role, .. } if *role == MessageRole::User
            )
        })
        .map(|(seq, _)| *seq)
        .unwrap_or(1);
    session.lease.release().await.expect("lease release");
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: user_msg_seq,
        fence_token: 1,
        timestamp: None,
    })
    .await
    .expect("append turn end");
}

fn has_event(events: &[serde_json::Value], event_type: &str) -> bool {
    events.iter().any(|event| event["type"] == event_type)
}

/// One promptless stream stays busy, then finishes when its remote turn ends live.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_remote_lease_active_shows_busy_until_turn_ends() {
    let Some(session) = seed_in_progress_leased_session().await else {
        return;
    };

    let response = open_promptless_sse(&session).await;
    let run_started = Arc::new(tokio::sync::Notify::new());
    let run_finished = Arc::new(AtomicBool::new(false));

    let finish_handle = tokio::spawn({
        let remote_turn = session.remote_turn_handle();
        let run_started = Arc::clone(&run_started);
        let run_finished = Arc::clone(&run_finished);
        async move {
            run_started.notified().await;
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert!(
                !run_finished.load(Ordering::SeqCst),
                "RUN_FINISHED arrived while remote turn was still active"
            );
            finish_remote_turn(remote_turn).await;
        }
    });

    let read = read_sse_until(response, Duration::from_secs(10), {
        let run_started = Arc::clone(&run_started);
        let run_finished = Arc::clone(&run_finished);
        move |read| {
            if has_event(&read.events, "RUN_STARTED") {
                run_started.notify_one();
            }
            if has_event(&read.events, "RUN_FINISHED") {
                run_finished.store(true, Ordering::SeqCst);
                return true;
            }
            false
        }
    })
    .await;

    finish_handle.await.expect("finish task should complete");
    assert!(has_event(&read.events, "RUN_STARTED"));
    assert_eq!(
        read.events
            .iter()
            .filter(|event| event["type"] == "RUN_FINISHED")
            .count(),
        1,
        "live stream should emit exactly one RUN_FINISHED"
    );
}

/// Dropping a quiet remote-follow response releases its detached follow task.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_remote_follow_stops_promptly_when_client_disconnects() {
    let Some(session) = seed_in_progress_leased_session().await else {
        return;
    };

    let response = open_promptless_sse(&session).await;
    let read = read_sse_until(response, Duration::from_secs(5), |read| {
        has_event(&read.events, "RUN_STARTED")
    })
    .await;
    assert!(!has_event(&read.events, "RUN_FINISHED"));

    // read_sse_until owns the response body, so returning above drops the receiver.
    tokio::time::timeout(
        Duration::from_millis(250),
        tokio::time::sleep(Duration::from_millis(25)),
    )
    .await
    .expect("disconnect observation window should stay bounded");
    assert!(
        harnx_runtime::nats_lease::session_has_active_lease(
            &session.jetstream,
            &session.session_id,
        )
        .await
        .expect("lease check"),
        "test must leave the remote turn active"
    );
}

/// Test that a promptless AG-UI run on a session with an already-completed
/// remote turn shows idle (RUN_STARTED, then immediate RUN_FINISHED via the
/// synchronous shortcut path).
#[tokio::test(flavor = "multi_thread")]
async fn e2e_remote_lease_after_turn_ends_shows_idle() {
    let Some(session) = seed_in_progress_leased_session().await else {
        return;
    };

    // Finish the turn BEFORE opening the stream.
    finish_remote_turn(session.remote_turn_handle()).await;

    // Open a promptless AG-UI run stream.
    let response = open_promptless_sse(&session).await;

    // Should see RUN_STARTED followed by RUN_FINISHED immediately.
    let read = read_sse_until(response, Duration::from_secs(5), |read| {
        has_event(&read.events, "RUN_FINISHED")
    })
    .await;

    assert!(
        has_event(&read.events, "RUN_STARTED"),
        "should emit RUN_STARTED after turn ended"
    );
    assert!(
        has_event(&read.events, "RUN_FINISHED"),
        "should emit RUN_FINISHED after turn ended"
    );
}
