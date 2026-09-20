mod common;

use anyhow::{Context, Result};
use common::spawn_nats_server;
use futures_util::StreamExt;
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
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
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
            ignore_discovered_servers: None,
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
    let log =
        NatsSessionLog::for_agent(jetstream.clone(), "test-agent", session_id).with_replicas(1);
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

// Regression for #1958: `harnx dump session --follow` used to abort the whole
// process when a transient `jetstream request timed out` hit the periodic
// durable poll. The follow loop now logs and keeps polling, so a momentary
// failure is a no-op the next tick recovers from. This exercises that seam via
// `SessionEventStream::refresh_history`, which `flush_new_entries` drives.
#[tokio::test]
async fn session_dump_follow_refresh_recovers_after_transient_failure() -> Result<()> {
    use harnx_runtime::nats_event_sink::SessionEventStream;

    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let session_id = new_session_id();

    // Seed one durable entry through the real `$JS.API` so the stream exists
    // and `attach` observes normal history.
    let log = NatsSessionLog::new_with_replicas(jetstream.clone(), session_id.clone(), 1);
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("m1".to_string()),
        role: MessageRole::User,
        content: MessageContent::Text("Hello".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;

    // A fault-injecting JetStream proxy on the `RETRY` API prefix models a brief
    // broker hiccup during a `--follow` poll without tearing the connection down.
    let proxy = spawn_faulting_jetstream_proxy(&client).await?;

    // Attach through the proxied context while disarmed, so initial history
    // loads cleanly.
    let proxy_jetstream =
        async_nats::jetstream::with_prefix(async_nats::connect(server.url()).await?, "RETRY");
    let mut stream =
        SessionEventStream::attach(proxy_jetstream, client.clone(), &session_id).await?;
    let attached_len = stream.history().len();
    assert_eq!(attached_len, 1);

    // Append an entry the follow loop should eventually surface.
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("m2".to_string()),
        role: MessageRole::Assistant,
        content: MessageContent::Text("Hi there".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;

    // A transient failure surfaces as an error. The `--follow` loop logs this
    // and continues instead of exiting the process.
    proxy.armed.store(true, Ordering::SeqCst);
    let transient = stream.refresh_history().await;
    assert!(
        transient.is_err(),
        "a transient JetStream failure should surface as a recoverable error"
    );
    assert!(
        proxy.injected.load(Ordering::SeqCst) >= 1,
        "the injected failure should have been served"
    );

    // The next poll recovers and picks up the entry appended in the meantime,
    // proving the tail keeps running after the hiccup.
    proxy.armed.store(false, Ordering::SeqCst);
    let recovered = stream.refresh_history().await?;
    assert!(
        recovered,
        "refresh should report new entries after recovery"
    );
    assert!(
        stream.history().len() > attached_len,
        "recovered refresh should append the entry added during the outage"
    );

    proxy.responder.abort();

    Ok(())
}

/// A JetStream proxy on the `RETRY.>` prefix that forwards to the real
/// `$JS.API` unless `armed` is set, in which case the first request thereafter
/// is answered with a transient 503. Lets a test inject a single momentary
/// broker failure at a chosen moment without dropping the connection. Build the
/// client context with `async_nats::jetstream::with_prefix(conn, "RETRY")`.
struct FaultingJetStreamProxy {
    armed: Arc<AtomicBool>,
    injected: Arc<AtomicUsize>,
    responder: tokio::task::JoinHandle<()>,
}

async fn spawn_faulting_jetstream_proxy(
    client: &async_nats::Client,
) -> Result<FaultingJetStreamProxy> {
    let mut requests = client.subscribe("RETRY.>".to_string()).await?;
    client.flush().await?;
    let armed = Arc::new(AtomicBool::new(false));
    let injected = Arc::new(AtomicUsize::new(0));
    let responder_armed = Arc::clone(&armed);
    let responder_injected = Arc::clone(&injected);
    let responder_client = client.clone();
    let responder = tokio::spawn(async move {
        while let Some(request) = requests.next().await {
            let suffix = request
                .subject
                .as_str()
                .strip_prefix("RETRY.")
                .expect("request uses retry test prefix");
            let inject = responder_armed.load(Ordering::SeqCst)
                && responder_injected.fetch_add(1, Ordering::SeqCst) == 0;
            let payload = if inject {
                serde_json::to_vec(&serde_json::json!({
                    "error": {
                        "code": 503,
                        "err_code": 10008,
                        "description": "injected transient JetStream failure"
                    }
                }))
                .expect("serialize injected JetStream error")
                .into()
            } else {
                responder_client
                    .request(format!("$JS.API.{suffix}"), request.payload)
                    .await
                    .expect("proxy JetStream request")
                    .payload
            };
            responder_client
                .publish(request.reply.expect("JetStream request has reply"), payload)
                .await
                .expect("reply to proxied JetStream request");
        }
    });
    Ok(FaultingJetStreamProxy {
        armed,
        injected,
        responder,
    })
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
