//! Regression for #2922 final-review B5: a `.set title` issued before the
//! first local turn — when the selected session has no persistence sink,
//! because history loads are read-only — must still durably persist and be
//! visible to a peer client, or fail clearly.

use crate::common;
use anyhow::{Context, Result};
use common::spawn_nats_server;
use harnx_core::require_nextest;
use harnx_runtime::config::{session, Config, ConfigLock, NatsRouting, NatsServerConfig};
use harnx_runtime::nats_session_metadata::{
    SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use std::sync::Arc;

fn cluster_config(url: &str) -> Config {
    Config {
        nats_servers: vec![NatsServerConfig {
            user_id: None,
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
        nats_routing: NatsRouting::Cluster("local".into()),
        // Remote-agent selection is what the TUI session picker leaves behind:
        // an agent, its cluster, and no installed session sink.
        remote_agent: Some(("test-agent".into(), "local".into())),
        ..Default::default()
    }
}

#[tokio::test]
async fn nats_set_title_before_first_turn_persists_and_peer_sees_it() -> Result<()> {
    require_nextest();

    let Some(handle) = spawn_nats_server().await? else {
        return Ok(());
    };
    let url = handle.url().to_string();

    let config = cluster_config(&url);
    let session_id = "pre-turn";

    // Canonical metadata, as the session's creator would have written it.
    let client = async_nats::connect(&url).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    store
        .create(&SessionMetadata::new(
            session_id,
            SessionInitializer::named("test-agent", Default::default()),
        ))
        .await?;

    // Select the session: in-memory state only, no sink — the read-only
    // history load deliberately installs none.
    let selected = session::new(&config, session_id, None)?;
    assert!(
        selected.runtime.is_none(),
        "pre-turn session must not have a persistence sink"
    );
    let storage_key = selected.storage_key();
    let global = Arc::new(ConfigLock::new(config));
    global.write().session = Some(selected);

    // The rename before any local turn must succeed durably, not just in
    // memory.
    Config::update_setting_durably(&global, "title Pre-turn Title", None).await?;

    {
        let guard = global.read();
        let session = guard.session.as_ref().unwrap();
        assert_eq!(session.title(), Some("Pre-turn Title"));
        // Manual title: regeneration is frozen.
        assert_eq!(session.title_last_updated_tokens(), usize::MAX);
    }

    // Peer view: a fresh client re-reading the same record sees the title,
    // which the old silent-skip behavior never wrote.
    let peer_client = async_nats::connect(&url).await?;
    let peer_jetstream = async_nats::jetstream::new(peer_client);
    let peer_store = SessionMetadataStore::ensure(&peer_jetstream, 1).await?;
    let record = peer_store
        .get(&storage_key)
        .await?
        .with_context(|| "canonical metadata for the renamed session")?;
    assert_eq!(
        record.metadata.title.value.as_deref(),
        Some("Pre-turn Title")
    );
    assert!(record.metadata.title.manual);
    Ok(())
}
