mod common;

use anyhow::Result;
use common::spawn_nats_server;
use harnx_core::{
    event::{AgentEvent, AgentEventSink, TurnEvent, TurnOutcome},
    message::{MessageContent, MessageRole},
    require_nextest,
    session::SessionLogEntry,
    session_reconstruct::{reconstruct_state_from_nats, TurnStatus},
};
use harnx_runtime::{
    nats_event_sink::{events_subject, AdvisoryEnvelope},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{SessionAgentSource, SessionMetadataStore},
    nats_worker::ControlCommand,
    send_control_command, NatsSession, NatsSessionConfig,
};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

fn new_remote_session_id() -> String {
    format!("test-{}", Uuid::new_v4())
}

async fn seed_prior_completed_turn(log: &NatsSessionLog) -> Result<u64> {
    let prior_user_seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some("prior-user".to_string()),
            role: MessageRole::User,
            content: MessageContent::Text("old prompt".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    let prior_assistant_seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some("prior-assistant".to_string()),
            role: MessageRole::Assistant,
            content: MessageContent::Text("old reply".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    assert!(prior_assistant_seq > prior_user_seq);
    Ok(prior_assistant_seq)
}

fn resumed_session_config(session_id: String) -> NatsSessionConfig {
    NatsSessionConfig {
        cluster: "test".to_string(),
        initializer: harnx_runtime::SessionInitializer::named("test-agent", Default::default()),
        session_id: Some(session_id),
        activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
    }
}

async fn append_new_reply_after_current_turn_user(
    log: NatsSessionLog,
    client: async_nats::Client,
    session_id: String,
    prior_assistant_seq: u64,
) -> Result<u64> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let entries = log.load_events_async().await?;
            let saw_current_turn_user = entries.iter().any(|(seq, entry)| {
                *seq > prior_assistant_seq
                    && matches!(
                        entry,
                        SessionLogEntry::Message {
                            role: MessageRole::User,
                            content: MessageContent::Text(text),
                            ..
                        } if text == "new prompt"
                    )
            });
            if saw_current_turn_user {
                break Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!("timed out waiting for current-turn user entry before appending new reply")
    })??;
    let seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some("new-assistant".to_string()),
            role: MessageRole::Assistant,
            content: MessageContent::Text("new reply".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    let ended = AdvisoryEnvelope::new(
        seq,
        AgentEvent::Turn(TurnEvent::Ended {
            outcome: TurnOutcome::default(),
        }),
    );
    let subject = events_subject(&session_id);
    let payload = ended.to_bytes()?;
    // Legacy workers had no durable TurnEnd marker. Repeat their lossy
    // advisory in this compatibility test so it cannot race the client's
    // subscribe-after-append setup.
    for _ in 0..4 {
        client
            .publish(subject.clone(), payload.clone().into())
            .await?;
        client.flush().await?;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(seq)
}

struct NoopEventSink;

impl AgentEventSink for NoopEventSink {
    fn emit(&self, _event: AgentEvent) {}
}

/// Test that control commands are sent correctly
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_session_sends_control_commands() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;

    // Subscribe to control subject
    let session_id = new_remote_session_id();
    let mut control_sub = client
        .subscribe(harnx_runtime::nats_worker::control_subject(&session_id))
        .await?;

    // Send a control command
    send_control_command(&client, &session_id, ControlCommand::Cancel).await?;

    // Verify it was received
    use futures_util::StreamExt;
    let msg = tokio::time::timeout(Duration::from_secs(2), control_sub.next())
        .await?
        .expect("should receive control message");

    let cmd: ControlCommand = serde_json::from_slice(&msg.payload)?;
    assert!(matches!(cmd, ControlCommand::Cancel));

    Ok(())
}

/// Test that user messages are stamped with a client-generated ID.
///
/// This test directly appends a user message (mimicking NatsSession's append path)
/// and verifies the ID field is set to a valid UUID. Does NOT call run_turn because
/// there's no worker to pick up the activation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_message_has_client_id() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());

    let session_id = new_remote_session_id();
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());

    // Create a NATS session (mirrors retract test pattern)
    let config = resumed_session_config(session_id.clone());
    let abort_signal = harnx_runtime::utils::create_abort_signal();

    let _session = NatsSession::new(config, client, jetstream, abort_signal).await?;

    // Append a user message directly (same pattern as in NatsSession::run_turn)
    let user_msg_id = uuid::Uuid::new_v4().to_string();
    let user_entry = SessionLogEntry::Message {
        id: Some(user_msg_id.clone()),
        role: MessageRole::User,
        content: MessageContent::Text("Hello world".to_string()),
        timestamp: None,
        fence_token: None,
    };
    log.append_event_async(&user_entry).await?;

    // Load entries and verify the user message has a valid UUID ID
    let entries = log.load_events_async().await?;
    let user_msg = entries.iter().find(
        |(_, e)| matches!(e, SessionLogEntry::Message { role, .. } if *role == MessageRole::User),
    );

    assert!(user_msg.is_some(), "User message should be in log");

    if let Some((_seq, entry)) = user_msg {
        if let SessionLogEntry::Message { id, .. } = entry {
            assert!(
                id.is_some(),
                "User message should have a client-generated ID"
            );
            let id = id.as_ref().unwrap();
            // UUID v4 format: 8-4-4-4-12 hex chars
            assert!(
                Uuid::parse_str(id).is_ok(),
                "Client ID should be a valid UUID"
            );
        } else {
            panic!("Expected Message entry");
        }
    }

    Ok(())
}

/// Test retract-before-consume appends correct EditEntries and removes the message
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retract_queued_user_message() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());

    let session_id = new_remote_session_id();
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());

    let config = resumed_session_config(session_id.clone());
    let abort_signal = harnx_runtime::utils::create_abort_signal();
    let session = NatsSession::new(config, client, jetstream.clone(), abort_signal).await?;

    // Append a user message directly (simulating what NatsSession does)
    let user_msg_id = uuid::Uuid::new_v4().to_string();
    let user_entry = SessionLogEntry::Message {
        id: Some(user_msg_id.clone()),
        role: MessageRole::User,
        content: MessageContent::Text("Hello world".to_string()),
        timestamp: None,
        fence_token: None,
    };
    let user_msg_seq = log.append_event_async(&user_entry).await?;

    // Verify the message is there
    let entries = log.load_events_async().await?;
    let user_msg_before = entries.iter().find(|(seq, _)| *seq == user_msg_seq);
    assert!(
        user_msg_before.is_some(),
        "User message should exist before retract"
    );

    // Retract the message
    let edit_seq = session.retract_user_message(user_msg_seq).await?;
    assert!(
        edit_seq > user_msg_seq,
        "EditEntries should come after user message"
    );

    // Load entries and verify EditEntries was appended
    let entries = log.load_events_async().await?;
    let edit_entry = entries.iter().find(|(seq, _)| *seq == edit_seq);
    assert!(edit_entry.is_some(), "EditEntries should be in log");

    if let Some((
        _,
        SessionLogEntry::EditEntries {
            from,
            to,
            replacements,
        },
    )) = edit_entry
    {
        assert_eq!(
            *from, user_msg_seq as usize,
            "EditEntries.from should match user message seq"
        );
        assert_eq!(
            *to, user_msg_seq as usize,
            "EditEntries.to should match user message seq"
        );
        assert!(
            replacements.is_empty(),
            "EditEntries.replacements should be empty for deletion"
        );
    } else {
        panic!("Expected EditEntries entry");
    }

    // Reconstruct state and verify the user message is gone from next_turn_messages
    // Reconstruct state and verify the user message is gone from next_turn_messages
    // Use the NATS-aware function that properly handles JetStream sequence numbers
    let state = reconstruct_state_from_nats(&entries);
    eprintln!("Turn status: {:?}", state.turn_status);
    eprintln!("Next turn messages: {:?}", state.next_turn_messages.len());

    assert!(
        matches!(state.turn_status, TurnStatus::Idle),
        "Turn should be idle after retraction"
    );

    // The user message should not appear in next_turn_messages
    // (It would be the first user message if present)
    // next_turn_messages filters out entries covered by EditEntries.
    let has_user_msg = state.next_turn_messages.iter().any(|m| {
        matches!(m, harnx_core::message::Message {
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text(t),
            ..
        } if t == "Hello world")
    });
    assert!(
        !has_user_msg,
        "User message should be removed by EditEntries deletion"
    );

    Ok(())
}

/// Test edit-before-consume appends parseable replacement YAML and keeps edited text in replay
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_queued_user_message_replaces_text_in_reconstructed_state() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());

    let session_id = new_remote_session_id();
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());

    let config = resumed_session_config(session_id.clone());
    let abort_signal = harnx_runtime::utils::create_abort_signal();
    let session = NatsSession::new(config, client, jetstream.clone(), abort_signal).await?;

    let original_entry = SessionLogEntry::Message {
        id: Some(Uuid::new_v4().to_string()),
        role: MessageRole::User,
        content: MessageContent::Text("Hello world".to_string()),
        timestamp: None,
        fence_token: None,
    };
    let user_msg_seq = log.append_event_async(&original_entry).await?;

    let edit_seq = session
        .edit_user_message(user_msg_seq, "edited text".to_string())
        .await?;
    assert!(
        edit_seq > user_msg_seq,
        "EditEntries should come after user message"
    );

    let entries = log.load_events_async().await?;
    let edit_entry = entries.iter().find(|(seq, _)| *seq == edit_seq);
    assert!(edit_entry.is_some(), "EditEntries should be in log");

    let replacement_yaml = match edit_entry {
        Some((
            _,
            SessionLogEntry::EditEntries {
                from,
                to,
                replacements,
            },
        )) => {
            assert_eq!(*from, user_msg_seq as usize);
            assert_eq!(*to, user_msg_seq as usize);
            assert_eq!(
                replacements.len(),
                1,
                "edit should write one replacement entry"
            );
            replacements[0].clone()
        }
        _ => panic!("Expected EditEntries entry"),
    };

    let parsed_replacement = serde_yaml::from_str::<SessionLogEntry>(&replacement_yaml)?;
    match parsed_replacement {
        SessionLogEntry::Message {
            role,
            content: MessageContent::Text(text),
            timestamp,
            fence_token,
            ..
        } => {
            assert_eq!(role, MessageRole::User);
            assert_eq!(text, "edited text");
            assert!(timestamp.is_none());
            assert!(fence_token.is_none());
        }
        other => panic!("expected replacement message entry, got {other:?}"),
    }

    let state = reconstruct_state_from_nats(&entries);
    assert!(matches!(state.turn_status, TurnStatus::Idle));

    let next_turn_texts: Vec<_> = state
        .next_turn_messages
        .iter()
        .filter_map(|message| match (&message.role, &message.content) {
            (MessageRole::User, MessageContent::Text(text)) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(next_turn_texts, vec!["edited text".to_string()]);
    assert!(!next_turn_texts.iter().any(|text| text == "Hello world"));

    Ok(())
}

/// Test resumed session ignores stale prior assistant reply and returns new turn reply
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_session_run_turn_ignores_stale_prior_reply_and_returns_new_reply() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = new_remote_session_id();
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());

    let abort_signal = harnx_runtime::utils::create_abort_signal();
    let session = NatsSession::new(
        resumed_session_config(session_id.clone()),
        client.clone(),
        jetstream.clone(),
        abort_signal,
    )
    .await?;
    let prior_assistant_seq = seed_prior_completed_turn(&log).await?;

    let log_for_reply = log.clone();
    let client_for_reply = client.clone();
    let session_id_for_reply = session_id.clone();
    let reply_task = tokio::spawn(async move {
        append_new_reply_after_current_turn_user(
            log_for_reply,
            client_for_reply,
            session_id_for_reply,
            prior_assistant_seq,
        )
        .await
    });

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        session.run_turn("new prompt", Arc::new(NoopEventSink), None),
    )
    .await??;

    let new_assistant_seq = reply_task.await??;
    assert!(new_assistant_seq > result.user_msg_seq);
    assert_ne!(result.response.as_deref(), Some("old reply"));
    assert_eq!(result.response.as_deref(), Some("new reply"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lazy_arbitrary_id_creation_precedes_the_first_user_entry() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = format!("arbitrary-id-{}", Uuid::new_v4());
    let abort = harnx_runtime::utils::create_abort_signal();
    let session = NatsSession::new(
        resumed_session_config(session_id.clone()),
        client,
        jetstream.clone(),
        abort.clone(),
    )
    .await?;

    let metadata_store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let metadata = metadata_store
        .get(&session_id)
        .await?
        .expect("lazy construction creates metadata");
    assert_eq!(
        metadata.metadata.agent,
        SessionAgentSource::Named {
            name: "test-agent".to_string()
        }
    );
    assert!(metadata_store.get_activity(&session_id).await?.is_some());

    let log = NatsSessionLog::new(jetstream, session_id);
    assert!(log.load_events_async().await?.is_empty());
    let turn = tokio::spawn(async move {
        session
            .run_turn("first prompt", Arc::new(NoopEventSink), None)
            .await
    });
    let entries = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let entries = log.load_events_async().await?;
            if !entries.is_empty() {
                break Ok::<_, anyhow::Error>(entries);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    abort.set_ctrlc();
    let _ = tokio::time::timeout(Duration::from_secs(2), turn).await;

    assert_eq!(entries.len(), 1);
    assert!(matches!(
        &entries[0].1,
        SessionLogEntry::Message {
            role: MessageRole::User,
            content: MessageContent::Text(text),
            ..
        } if text == "first prompt"
    ));
    assert!(!entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::EditEntries { .. })));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_creation_validates_the_winning_identity() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = format!("creation-race-{}", Uuid::new_v4());
    let mut first = resumed_session_config(session_id.clone());
    first.initializer = harnx_runtime::SessionInitializer::named("alpha", Default::default());
    let mut second = resumed_session_config(session_id.clone());
    second.initializer = harnx_runtime::SessionInitializer::named("beta", Default::default());

    let (first, second) = tokio::join!(
        NatsSession::new(
            first,
            client.clone(),
            jetstream.clone(),
            harnx_runtime::utils::create_abort_signal(),
        ),
        NatsSession::new(
            second,
            client,
            jetstream.clone(),
            harnx_runtime::utils::create_abort_signal(),
        )
    );
    assert_ne!(
        first.is_ok(),
        second.is_ok(),
        "exactly one identity must win"
    );
    let record = SessionMetadataStore::ensure(&jetstream, 1)
        .await?
        .get(&session_id)
        .await?
        .expect("winning metadata exists");
    assert!(matches!(
        record.metadata.agent,
        SessionAgentSource::Named { ref name } if name == "alpha" || name == "beta"
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_creation_with_the_same_identity_reloads_the_winner() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = format!("same-identity-race-{}", Uuid::new_v4());
    let config = resumed_session_config(session_id.clone());
    let (first, second) = tokio::join!(
        NatsSession::new(
            config.clone(),
            client.clone(),
            jetstream.clone(),
            harnx_runtime::utils::create_abort_signal(),
        ),
        NatsSession::new(
            config,
            client,
            jetstream.clone(),
            harnx_runtime::utils::create_abort_signal(),
        )
    );
    assert!(first.is_ok(), "first creator failed: {:?}", first.err());
    assert!(second.is_ok(), "race loser failed: {:?}", second.err());
    assert!(SessionMetadataStore::ensure(&jetstream, 1)
        .await?
        .get(&session_id)
        .await?
        .is_some());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metadata_creation_failure_leaves_the_transcript_empty() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = format!("invalid-metadata-{}", Uuid::new_v4());
    let result = NatsSession::new(
        NatsSessionConfig {
            cluster: "test".to_string(),
            initializer: harnx_runtime::SessionInitializer::named("", Default::default()),
            session_id: Some(session_id.clone()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        client,
        jetstream.clone(),
        harnx_runtime::utils::create_abort_signal(),
    )
    .await;
    assert!(result.is_err(), "invalid metadata must reject construction");
    assert!(NatsSessionLog::new(jetstream.clone(), &session_id)
        .load_events_async()
        .await?
        .is_empty());
    assert!(SessionMetadataStore::ensure(&jetstream, 1)
        .await?
        .get(&session_id)
        .await?
        .is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transcript_without_metadata_is_rejected_without_an_append() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = format!("legacy-headerless-{}", Uuid::new_v4());
    let log = NatsSessionLog::new(jetstream.clone(), session_id.clone());
    log.append_event_async(&SessionLogEntry::Message {
        id: Some(Uuid::new_v4().to_string()),
        role: MessageRole::User,
        content: MessageContent::Text("legacy prompt".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;

    let result = NatsSession::new(
        resumed_session_config(session_id.clone()),
        client,
        jetstream.clone(),
        harnx_runtime::utils::create_abort_signal(),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("metadata-less transcript must be rejected"),
        Err(error) => error,
    };
    assert!(error
        .to_string()
        .contains("transcript entries but no canonical metadata"));
    assert_eq!(log.load_events_async().await?.len(), 1);
    assert!(SessionMetadataStore::ensure(&jetstream, 1)
        .await?
        .get(&session_id)
        .await?
        .is_none());
    Ok(())
}
