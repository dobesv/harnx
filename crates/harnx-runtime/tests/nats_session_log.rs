mod common;

use anyhow::Result;
use common::spawn_nats_server;
use harnx_core::{
    message::{MessageContent, MessageRole},
    require_nextest,
    session::{SessionLogEntry, ToolOutput},
    session_reconstruct::{reconstruct_state, TurnStatus},
    tool::ToolCall,
};
use harnx_runtime::{config::Config, nats_session_log::NatsSessionLog};
use serde_json::json;

#[tokio::test]
async fn nats_session_log_round_trips_and_reconstructs() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let config = Config {
        nats_servers: vec![harnx_runtime::config::NatsServerConfig {
            name: "local".to_string(),
            url: server.url().to_string(),
            token: None,
            replicas: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: vec![],
        }],
        ..Default::default()
    };
    let jetstream = config.nats_jetstream("local").await?;
    let log = NatsSessionLog::new(jetstream.clone(), "session-roundtrip");

    let entries = mixed_entries();
    let expected_yaml: Vec<String> = entries.iter().map(entry_yaml).collect::<Result<_>>()?;
    let mut seqs = Vec::new();
    for entry in &entries {
        seqs.push(log.append_event_async(entry).await?);
    }

    let loaded = log.load_events_async().await?;
    let loaded_entries: Vec<SessionLogEntry> =
        loaded.iter().map(|(_, entry)| entry.clone()).collect();
    let loaded_yaml: Vec<String> = loaded_entries
        .iter()
        .map(entry_yaml)
        .collect::<Result<_>>()?;
    assert_eq!(loaded_yaml, expected_yaml);
    assert_eq!(seqs, loaded.iter().map(|(seq, _)| *seq).collect::<Vec<_>>());

    let replayed = log.replay_from_async(loaded[1].0).await?;
    let replayed_yaml: Vec<String> = replayed.iter().map(entry_yaml).collect::<Result<_>>()?;
    assert_eq!(replayed_yaml, expected_yaml[2..].to_vec());

    let state = reconstruct_state(&loaded_entries);
    assert_eq!(state.turn_status, TurnStatus::Idle);
    assert!(state.next_turn_messages.is_empty());
    assert!(state.resumable_ctx.is_none());

    Ok(())
}

#[tokio::test]
async fn nats_session_log_orphan_repair_matches_file_replay() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let config = Config {
        nats_servers: vec![harnx_runtime::config::NatsServerConfig {
            name: "local".to_string(),
            url: server.url().to_string(),
            token: None,
            replicas: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: vec![],
        }],
        ..Default::default()
    };
    let jetstream = config.nats_jetstream("local").await?;
    let log = NatsSessionLog::new(jetstream.clone(), "session-orphan");

    let entries = orphan_entries();
    let expected_yaml: Vec<String> = entries.iter().map(entry_yaml).collect::<Result<_>>()?;
    for entry in &entries {
        log.append_event_async(entry).await?;
    }

    let loaded = log.load_events_async().await?;
    let loaded_entries: Vec<SessionLogEntry> =
        loaded.iter().map(|(_, entry)| entry.clone()).collect();
    let loaded_yaml: Vec<String> = loaded_entries
        .iter()
        .map(entry_yaml)
        .collect::<Result<_>>()?;
    assert_eq!(loaded_yaml, expected_yaml);

    let state = reconstruct_state(&loaded_entries);
    assert_eq!(state.turn_status, TurnStatus::InFlightResumable);
    let resumable = state.resumable_ctx.expect("resumable ctx");
    assert_eq!(resumable.pending_tool_results.len(), 0);
    assert_eq!(resumable.fence_token, Some(77));

    let repaired =
        harnx_runtime::nats_session_log::load_session_from_entries(&loaded, "nats-session")?;
    let tail = repaired.messages.last().expect("tail message");
    assert_eq!(tail.role, MessageRole::Tool);
    // Orphan repair synthesizes a "lost" ToolResults for the un-answered tool
    // call. The marker lives in the tool result's `output` JSON (same as the
    // file-log path), not in the message's rendered text.
    let MessageContent::ToolCalls(tc) = &tail.content else {
        panic!("expected ToolCalls content on repaired tail message");
    };
    assert_eq!(tc.tool_results.len(), 1);
    let output_str = tc.tool_results[0].output.to_string();
    assert!(
        output_str.contains("tool response lost"),
        "expected synthesized lost-response error, got: {output_str}"
    );

    Ok(())
}

fn entry_yaml(entry: &SessionLogEntry) -> Result<String> {
    harnx_runtime::nats_session_log::serialize_entry(entry)
}

fn mixed_entries() -> Vec<SessionLogEntry> {
    vec![
        SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("hello".to_string()),
            timestamp: None,
            fence_token: None,
        },
        SessionLogEntry::ToolCalls {
            text: "running bash".to_string(),
            thought: Some("need directory list".to_string()),
            calls: vec![ToolCall::new(
                "Bash".to_string(),
                json!({"command": "ls"}),
                Some("call-1".to_string()),
                None,
            )],
            timestamp: None,
            fence_token: Some(41),
        },
        SessionLogEntry::ToolResults {
            results: vec![ToolOutput {
                id: Some("call-1".to_string()),
                name: "Bash".to_string(),
                output: json!({"stdout": "Cargo.toml"}),
                markdown: None,
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        },
        SessionLogEntry::Cancel { fence_token: 41 },
        SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text("done".to_string()),
            timestamp: None,
            fence_token: None,
        },
    ]
}

fn orphan_entries() -> Vec<SessionLogEntry> {
    vec![
        SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("run tool".to_string()),
            timestamp: None,
            fence_token: None,
        },
        SessionLogEntry::ToolCalls {
            text: "working".to_string(),
            thought: Some("thinking".to_string()),
            calls: vec![ToolCall::new(
                "Bash".to_string(),
                json!({"command": "pwd"}),
                Some("call-orphan".to_string()),
                None,
            )],
            timestamp: None,
            fence_token: Some(77),
        },
    ]
}
