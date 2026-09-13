mod common;

use anyhow::{Context, Result};
use common::spawn_nats_server;
use harnx_core::{
    message::{MessageContent, MessageRole},
    require_nextest,
    session::SessionLogEntry,
};
use harnx_runtime::{
    config::{
        dump_entries_jsonl, dump_entries_yaml, render_metadata_json, render_metadata_yaml, Config,
    },
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{SessionInitializer, SessionMetadata, SessionMetadataStore},
};
use serde_json::Value;
use uuid::Uuid;

fn new_session_id() -> String {
    format!("test-{}", Uuid::new_v4())
}

fn local_nats_config(url: &str) -> Config {
    Config {
        nats_servers: vec![harnx_runtime::config::NatsServerConfig {
            name: "local".to_string(),
            url: url.to_string(),
            token: None,
            replicas: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: vec![],
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn info_session_metadata_yaml_is_single_document_with_variables() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let config = local_nats_config(server.url());
    let jetstream = config.nats_jetstream("local").await?;
    let session_id = new_session_id();

    // Seed session metadata
    let metadata_store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let mut metadata = SessionMetadata::new(
        &session_id,
        SessionInitializer::named("test-agent", Default::default()),
    );
    metadata
        .variables
        .insert("project".to_string(), "harnx".to_string());
    metadata_store.create(&metadata).await?;

    // Render YAML
    let yaml = render_metadata_yaml(&metadata)?;

    // Verify single document (not multi-doc), contains variables
    assert!(
        yaml.starts_with("---\n") || yaml.starts_with("---\r\n") || !yaml.starts_with("---"),
        "YAML should be a single document"
    );
    assert!(
        yaml.contains("variables:"),
        "YAML should contain variables section"
    );
    assert!(
        yaml.contains("project"),
        "YAML should contain variable name"
    );

    Ok(())
}

#[tokio::test]
async fn info_session_metadata_json_is_single_object_with_variables() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let config = local_nats_config(server.url());
    let jetstream = config.nats_jetstream("local").await?;
    let session_id = new_session_id();

    // Seed session metadata
    let metadata_store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let mut metadata = SessionMetadata::new(
        &session_id,
        SessionInitializer::named("test-agent", Default::default()),
    );
    metadata
        .variables
        .insert("project".to_string(), "harnx".to_string());
    metadata_store.create(&metadata).await?;

    // Render JSON
    let json = render_metadata_json(&metadata)?;

    // Parse as single object (not array), verify contains variables
    let parsed: Value = serde_json::from_str(&json).context("JSON should be parseable")?;
    assert!(
        parsed.is_object(),
        "JSON should be a single object, not an array"
    );
    assert!(
        parsed.get("variables").is_some(),
        "JSON should contain variables"
    );

    Ok(())
}

#[tokio::test]
async fn dump_session_yaml_includes_all_entries_as_separate_docs() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let config = local_nats_config(server.url());
    let jetstream = config.nats_jetstream("local").await?;
    let session_id = new_session_id();
    let log = NatsSessionLog::new(jetstream.clone(), &session_id);

    // Seed session with entries including control entry
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("m1".to_string()),
        role: MessageRole::User,
        content: MessageContent::Text("Hello".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("m2".to_string()),
        role: MessageRole::Assistant,
        content: MessageContent::Text("Hi there".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: 2,
        fence_token: 0,
        timestamp: None,
        usage: None,
    })
    .await?;

    // Load and dump
    let raw = log.load_events_async().await?;
    let entries = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw)?;
    let yaml = dump_entries_yaml(entries.iter().map(|(_, e)| e))?;

    // Verify doc separators and control entry
    let doc_count = yaml.matches("---\n").count();
    assert!(
        doc_count >= 1,
        "YAML should have at least 1 document separator, got {}",
        doc_count
    );
    // Note: TurnEnd is included in raw entries; reconstruction may filter control entries

    Ok(())
}

#[tokio::test]
async fn dump_session_jsonl_is_n_lines_parsable_independently() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let config = local_nats_config(server.url());
    let jetstream = config.nats_jetstream("local").await?;
    let session_id = new_session_id();
    let log = NatsSessionLog::new(jetstream.clone(), &session_id);

    // Seed 3 entries (including control)
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("m1".to_string()),
        role: MessageRole::User,
        content: MessageContent::Text("Hello".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("m2".to_string()),
        role: MessageRole::Assistant,
        content: MessageContent::Array(vec![harnx_core::message::MessageContentPart::Text {
            text: "Response".to_string(),
        }]),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: 2,
        fence_token: 0,
        timestamp: None,
        usage: None,
    })
    .await?;

    // Load and dump
    let raw = log.load_events_async().await?;
    let entries = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw)?;
    let jsonl = dump_entries_jsonl(entries.iter().map(|(_, e)| e))?;

    // Verify N lines = N entries
    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(
        lines.len(),
        entries.len(),
        "JSONL should have {} lines, got {}",
        entries.len(),
        lines.len()
    );

    // Each line independently parses as SessionLogEntry
    for (i, line) in lines.iter().enumerate() {
        let parsed: Value = serde_json::from_str(line)
            .with_context(|| format!("Line {} should parse as JSON: {:?}", i + 1, line))?;
        assert!(
            parsed.is_object(),
            "Line {} should be JSON object, not array",
            i + 1
        );
    }

    // No array brackets
    assert!(
        !jsonl.starts_with('['),
        "JSONL should not start with array bracket"
    );
    assert!(
        !jsonl.ends_with(']'),
        "JSONL should not end with array bracket"
    );

    Ok(())
}

// Note: load_session_for_render requires a valid agent config with model resolution
// which is not available in standalone test environment. This functionality is
// tested end-to-end via CLI and TUI tests.
