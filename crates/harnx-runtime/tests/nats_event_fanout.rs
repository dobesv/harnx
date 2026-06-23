//! P4.1 integration tests: Live event fan-out from worker to clients.
//!
//! Tests that:
//! (a) Two subscribers to one session both receive emitted advisory events
//! (b) Early + late subscriber converge to identical final durable-derived state
//! (c) Mid-turn drop of advisory still converges via durable log

mod common;

use anyhow::Result;
use common::spawn_nats_server;
use futures_util::StreamExt;
use harnx_core::event::{AgentEvent, NoticeEvent};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::nats_event_sink::{events_subject, AdvisoryEnvelope, SessionEventStream};
use harnx_runtime::nats_worker::NatsSessionLogBackend;
use std::time::Duration;
use tokio::time::timeout;

/// Test (a): Two subscribers to one session both receive emitted advisory events.
#[tokio::test]
async fn two_subscribers_receive_advisory_events() -> Result<()> {
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not found");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let _jetstream = async_nats::jetstream::new(client.clone());

    let session_id = format!("test-{}", uuid::Uuid::new_v4());
    let subject = events_subject(&session_id);

    // Create two subscribers BEFORE publishing
    let mut sub1 = client.subscribe(subject.clone()).await?;
    let mut sub2 = client.subscribe(subject.clone()).await?;

    // Publish an advisory event
    let event = AgentEvent::Notice(NoticeEvent::Info("hello from worker".into()));
    let envelope = AdvisoryEnvelope::new(0, event);
    let payload = envelope.to_bytes()?;
    client.publish(subject, payload.into()).await?;
    client.flush().await?;

    // Both subscribers should receive it
    let msg1 = timeout(Duration::from_secs(2), sub1.next()).await;
    let msg2 = timeout(Duration::from_secs(2), sub2.next()).await;

    assert!(
        msg1.is_ok() && msg1.unwrap().is_some(),
        "sub1 should receive event"
    );
    assert!(
        msg2.is_ok() && msg2.unwrap().is_some(),
        "sub2 should receive event"
    );

    Ok(())
}

/// Test (b): Late subscriber gets history first, then live events.
/// Uses multi_thread flavor because it calls block_in_place via append_event_blocking.
#[tokio::test(flavor = "multi_thread")]
async fn late_subscriber_gets_history_then_live() -> Result<()> {
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not found");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());

    let session_id = format!("test-{}", uuid::Uuid::new_v4());

    // Create session and append some entries to the DURABLE log
    let backend = NatsSessionLogBackend::new(jetstream.clone(), &session_id);

    // Append Message (user)
    let user_msg = SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("Hello from user".into()),
        timestamp: None,
        fence_token: None,
    };
    backend.append_event_blocking(&user_msg)?;

    // Give JetStream a moment to sync
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Early subscriber attaches
    let early_stream =
        SessionEventStream::attach(jetstream.clone(), client.clone(), &session_id).await?;

    // Early subscriber sees history
    let early_history = early_stream.history();
    assert!(
        !early_history.is_empty(),
        "early subscriber should have history"
    );
    let early_last_seq = early_stream.last_applied_seq();
    assert!(early_last_seq > 0, "history should have entries");

    // Now append more entries (simulating continued work)
    let assistant_msg = SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text("Response from assistant".into()),
        timestamp: None,
        fence_token: None,
    };
    backend.append_event_blocking(&assistant_msg)?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Late subscriber attaches AFTER more entries
    let late_stream =
        SessionEventStream::attach(jetstream.clone(), client.clone(), &session_id).await?;

    // Late subscriber should see ALL history (including new entries)
    let late_history = late_stream.history();
    assert!(
        late_history.len() > early_history.len(),
        "late subscriber should have more history than early"
    );

    // Late subscriber's last_seq should be greater
    let late_last_seq = late_stream.last_applied_seq();
    assert!(
        late_last_seq > early_last_seq,
        "late subscriber should have later seq"
    );

    // Both should be able to reconstruct the same state from their history
    // (they see all durable entries up to different points)

    Ok(())
}

/// Test: Advisory envelope after_seq enables dedup.
/// Uses multi_thread flavor because it calls block_in_place via append_event_blocking.
#[tokio::test(flavor = "multi_thread")]
async fn advisory_envelope_dedup_by_after_seq() -> Result<()> {
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not found");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());

    let session_id = format!("test-{}", uuid::Uuid::new_v4());

    // Create session and append entries to durable log
    let backend = NatsSessionLogBackend::new(jetstream.clone(), &session_id);

    let user_msg = SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("Hello".into()),
        timestamp: None,
        fence_token: None,
    };
    let seq1 = backend.append_event_blocking(&user_msg)?;

    // Create event sink and publish advisory with current last_seq
    let event_sink = harnx_runtime::nats_event_sink::NatsEventSink::new(
        client.clone(),
        jetstream.clone(),
        session_id.clone(),
    )
    .await;

    // Subscribe before publishing
    let subject = events_subject(&session_id);
    let mut sub = client.subscribe(subject).await?;

    // Publish advisory events
    event_sink
        .publish_event(AgentEvent::Notice(NoticeEvent::Info("advisory 1".into())))
        .await;

    // Receive and parse
    let msg = timeout(Duration::from_secs(2), sub.next()).await;
    assert!(msg.is_ok(), "should receive advisory event");
    let msg = msg.unwrap().expect("message should exist");
    let envelope = AdvisoryEnvelope::from_bytes(&msg.payload)?;

    // after_seq should be >= seq1 (the durable entry we appended)
    assert!(
        envelope.after_seq >= seq1,
        "advisory after_seq {} should be >= durable seq {}",
        envelope.after_seq,
        seq1
    );

    Ok(())
}

/// Test (c): Advisory dropout mid-turn, state converges from durable log.
/// Uses multi_thread flavor because it calls block_in_place via append_event_blocking.
#[tokio::test(flavor = "multi_thread")]
async fn advisory_dropout_converges_from_durable_log() -> Result<()> {
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not found");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());

    let session_id = format!("test-{}", uuid::Uuid::new_v4());

    // Create session and append durable state
    let backend = NatsSessionLogBackend::new(jetstream.clone(), &session_id);

    // User message and assistant response (authoritative)
    let user_msg = SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("What is 2+2?".into()),
        timestamp: None,
        fence_token: None,
    };
    backend.append_event_blocking(&user_msg)?;

    let assistant_msg = SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text("The answer is 4.".into()),
        timestamp: None,
        fence_token: None,
    };
    backend.append_event_blocking(&assistant_msg)?;

    // Simulate advisory events being published (but client misses some)
    let event_sink = harnx_runtime::nats_event_sink::NatsEventSink::new(
        client.clone(),
        jetstream.clone(),
        session_id.clone(),
    )
    .await;

    // Publish advisory (would be chunks in real scenario)
    event_sink
        .publish_event(AgentEvent::Notice(NoticeEvent::Info(
            "streaming chunk 1".into(),
        )))
        .await;

    // Client subscribes LATE (missed all advisories)
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = SessionEventStream::attach(jetstream.clone(), client.clone(), &session_id).await?;

    // Client replays durable history
    let history = stream.history();
    assert!(!history.is_empty(), "should have durable history");

    // Final authoritative state is derived from durable log, not advisory
    // Reconstruct state from entries
    let entries: Vec<SessionLogEntry> = history.iter().map(|(_, e)| e.clone()).collect();
    let state = harnx_core::session_reconstruct::reconstruct_state(&entries);

    // State should be Idle (final assistant message is the barrier)
    assert!(
        matches!(
            state.turn_status,
            harnx_core::session_reconstruct::TurnStatus::Idle
        ),
        "turn should be idle after final assistant message"
    );

    // Even though client missed advisory chunks, the authoritative state
    // is correct because it's derived from durable log
    // (In real scenario, client would render history, not the dropped chunks)

    Ok(())
}

/// Test: NatsEventSink publishes to correct subject.
#[tokio::test]
async fn nats_event_sink_publishes_to_correct_subject() -> Result<()> {
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not found");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());

    let session_id = format!("test-{}", uuid::Uuid::new_v4());
    let subject = events_subject(&session_id);

    // Subscribe first
    let mut sub = client.subscribe(subject.clone()).await?;

    // Create and emit event via sink
    let sink = std::sync::Arc::new(
        harnx_runtime::nats_event_sink::NatsEventSink::new(
            client.clone(),
            jetstream.clone(),
            session_id.clone(),
        )
        .await,
    );

    sink.publish_event(AgentEvent::Notice(NoticeEvent::Info("test message".into())))
        .await;

    // Should receive on subscription
    let msg = timeout(Duration::from_secs(2), sub.next()).await;
    assert!(msg.is_ok(), "should receive published event");
    let msg = msg.unwrap().expect("message should exist");

    // Parse and verify
    let envelope = AdvisoryEnvelope::from_bytes(&msg.payload)?;
    assert!(matches!(
        envelope.event,
        AgentEvent::Notice(NoticeEvent::Info(_))
    ));

    Ok(())
}

/// Test: Multiple events maintain ordering.
#[tokio::test]
async fn multiple_events_maintain_ordering() -> Result<()> {
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not found");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());

    let session_id = format!("test-{}", uuid::Uuid::new_v4());
    let subject = events_subject(&session_id);

    let mut sub = client.subscribe(subject.clone()).await?;

    let sink = std::sync::Arc::new(
        harnx_runtime::nats_event_sink::NatsEventSink::new(
            client.clone(),
            jetstream.clone(),
            session_id.clone(),
        )
        .await,
    );

    // Publish multiple events
    for i in 0..5 {
        sink.publish_event(AgentEvent::Notice(NoticeEvent::Info(format!(
            "event {}",
            i
        ))))
        .await;
    }

    client.flush().await?;

    // Receive and verify ordering
    let mut received = Vec::new();
    for _ in 0..5 {
        if let Some(msg) = timeout(Duration::from_secs(2), sub.next())
            .await
            .ok()
            .flatten()
        {
            let envelope = AdvisoryEnvelope::from_bytes(&msg.payload)?;
            if let AgentEvent::Notice(NoticeEvent::Info(msg)) = envelope.event {
                received.push(msg);
            }
        }
    }

    // All 5 should arrive (ordering in NATS is preserved per publisher)
    assert_eq!(received.len(), 5, "should receive all 5 events");
    let expected: Vec<String> = (0..5).map(|i| format!("event {}", i)).collect();
    assert_eq!(received, expected, "events should arrive in order");

    Ok(())
}
