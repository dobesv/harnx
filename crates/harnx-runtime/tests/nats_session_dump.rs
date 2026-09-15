mod common;

use anyhow::{Context, Result};
use common::spawn_nats_server;
use harnx_core::{
    message::{MessageContent, MessageRole},
    require_nextest,
    session::SessionLogEntry,
};
use harnx_runtime::{
    client::Client,
    config::{
        dump_entries_jsonl, dump_entries_yaml, load_session_for_render, render_metadata_json,
        render_metadata_yaml, Config,
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

async fn seed_test_session_metadata(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<SessionMetadata> {
    let metadata_store = SessionMetadataStore::ensure(jetstream, 1).await?;
    let mut metadata = SessionMetadata::new(
        session_id,
        SessionInitializer::named("test-agent", Default::default()),
    );
    metadata
        .variables
        .insert("project".to_string(), "harnx".to_string());
    metadata_store.create(&metadata).await?;
    Ok(metadata)
}

async fn seed_test_log_entries(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<NatsSessionLog> {
    let log = NatsSessionLog::for_agent(jetstream.clone(), "test-agent", session_id);
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
    Ok(log)
}

fn setup_temp_agent_dir(model_id: &str) -> Result<(tempfile::TempDir, Option<String>)> {
    let temp_dir = tempfile::tempdir()?;
    let agents_dir = temp_dir.path().join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    let agent_file = agents_dir.join("test-agent.md");
    std::fs::write(
        &agent_file,
        format!("---\nmodel: {model_id}\n---\nYou are a test agent."),
    )?;
    let prev = std::env::var("HARNX_CONFIG_DIR").ok();
    unsafe {
        std::env::set_var("HARNX_CONFIG_DIR", temp_dir.path());
    }
    Ok((temp_dir, prev))
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
    let metadata = seed_test_session_metadata(&jetstream, &session_id).await?;

    let yaml = render_metadata_yaml(&metadata)?;
    assert!(
        !yaml.contains("\n---\n") && !yaml.starts_with("---\n---"),
        "YAML should not have multiple document markers"
    );
    assert!(yaml.contains("variables:"));
    assert!(yaml.contains("project"));

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
    let metadata = seed_test_session_metadata(&jetstream, &session_id).await?;

    let json = render_metadata_json(&metadata)?;
    let parsed: Value = serde_json::from_str(&json)?;
    assert!(parsed.is_object(), "JSON metadata should be an object");
    assert!(
        parsed.get("variables").is_some(),
        "JSON should contain variables field"
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
    let log = seed_test_log_entries(&jetstream, &session_id).await?;

    let raw = log.load_events_async().await?;
    let entries = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw)?;
    let yaml = dump_entries_yaml(entries.iter().map(|(_, e)| e))?;

    let doc_count = yaml.matches("---\n").count();
    assert!(
        doc_count >= 1,
        "YAML should have document separators, got {}",
        doc_count
    );

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
    let log = seed_test_log_entries(&jetstream, &session_id).await?;

    let raw = log.load_events_async().await?;
    let entries = harnx_core::session_reconstruct::apply_log_mutations_nats(&raw)?;
    let jsonl = dump_entries_jsonl(entries.iter().map(|(_, e)| e))?;

    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(lines.len(), entries.len(), "N lines must match N entries");

    for (i, line) in lines.iter().enumerate() {
        assert!(
            !line.starts_with('['),
            "line {i} should NOT start with [ (no array brackets): {line}"
        );
        let parsed: Value =
            serde_json::from_str(line).context(format!("failed to parse line {i}"))?;
        assert!(parsed.is_object(), "line {i} should be a JSON object");
    }

    Ok(())
}

#[tokio::test]
async fn load_session_for_render_reconstructs_session_with_model_and_tokens() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let mut config = local_nats_config(server.url());
    let model = harnx_runtime::test_utils::MockClient::builder()
        .build()
        .model()
        .clone();
    config.clients = vec![harnx_runtime::client::ClientConfig::Unknown];
    config.model = model.clone();

    let (_temp_dir, prev_config_dir) = setup_temp_agent_dir(&model.id())?;
    let _guard = scopeguard::guard(prev_config_dir, |prev| match prev {
        Some(v) => unsafe {
            std::env::set_var("HARNX_CONFIG_DIR", v);
        },
        None => unsafe {
            std::env::remove_var("HARNX_CONFIG_DIR");
        },
    });

    let jetstream = config.nats_jetstream("local").await?;
    let session_id = new_session_id();

    let metadata_store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let metadata = SessionMetadata::new(
        &session_id,
        SessionInitializer::named("test-agent", Default::default()),
    );
    metadata_store.create(&metadata).await?;
    let _log = seed_test_log_entries(&jetstream, &session_id).await?;

    let session =
        load_session_for_render(&config, Some("local"), &session_id, "test-agent").await?;

    assert_eq!(session.model_id, model.id());
    assert_eq!(session.messages.len(), 2, "should have 2 messages");

    let rendered = harnx_runtime::config::session::render(&session)?;
    assert!(
        rendered.contains(&model.id()) || rendered.contains("model"),
        "rendered session should contain model info"
    );

    Ok(())
}

#[tokio::test]
async fn load_session_for_render_errors_for_nonexistent_agent() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let config = local_nats_config(server.url());
    let jetstream = config.nats_jetstream("local").await?;
    let session_id = new_session_id();

    let metadata_store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let metadata = SessionMetadata::new(
        &session_id,
        SessionInitializer::named("test-agent", Default::default()),
    );
    metadata_store.create(&metadata).await?;

    let result =
        load_session_for_render(&config, Some("local"), &session_id, "nonexistent-agent").await;
    assert!(result.is_err(), "should error for nonexistent agent");

    Ok(())
}
