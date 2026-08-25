mod common;

use anyhow::{Context, Result};
use common::spawn_nats_server;
use harnx_core::{
    message::{MessageContent, MessageRole},
    require_nextest,
    session::SessionLogEntry,
};
use harnx_runtime::{
    config::Config,
    nats_admin::delete_remote_session,
    nats_lease::NatsLeaseConfig,
    nats_session_log::{stream_name_for_session, NatsSessionLog},
    nats_session_metadata::{
        SessionInitializer, SessionMetadata, SessionMetadataStore, SESSION_METADATA_BUCKET,
    },
};

#[tokio::test]
async fn session_delete_removes_stream_and_lease_and_is_idempotent() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let config = local_nats_config(server.url());
    let jetstream = config.nats_jetstream("local").await?;
    let session_id = "delete-me";
    let log = NatsSessionLog::new(jetstream.clone(), session_id);
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("hello".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;

    let lease = harnx_runtime::nats_lease::NatsSessionLease::acquire(
        harnx_runtime::nats_lease::NatsLeaseAcquireParams {
            jetstream: jetstream.clone(),
            session_id,
            worker_id: "w1".to_string(),
            generation: 1,
            config: NatsLeaseConfig::default(),
            session_metadata: None,
        },
    )
    .await?
    .expect("lease acquired");
    let lease_key = NatsLeaseConfig::default().key_for_session(session_id);
    lease.stop_renewal_for_test().await;

    let metadata_store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    metadata_store
        .create(&SessionMetadata::new(
            session_id,
            SessionInitializer::named("oracle", Default::default()),
        ))
        .await?;

    let deleted = delete_remote_session(&config, "local", session_id).await?;
    assert!(deleted.stream_deleted);
    assert!(deleted.lease_deleted);

    let stream_name = stream_name_for_session(session_id);
    let err = jetstream
        .get_stream(&stream_name)
        .await
        .expect_err("stream should be gone");
    assert!(
        err.to_string().contains("stream not found") || err.to_string().contains("no responders")
    );

    let leases = config.nats_kv_bucket("local", "harnx_leases").await?;
    let lease_entry = leases
        .entry(lease_key.clone())
        .await
        .context("load deleted lease entry")?;
    assert!(
        lease_entry.is_none()
            || !matches!(
                lease_entry.unwrap().operation,
                async_nats::jetstream::kv::Operation::Put
            )
    );

    assert!(
        metadata_store.get(session_id).await?.is_none(),
        "session metadata should be removed"
    );
    assert_eq!(deleted.metadata_keys_deleted, 2);

    let deleted_again = delete_remote_session(&config, "local", session_id).await?;
    assert!(!deleted_again.stream_deleted);
    assert!(!deleted_again.lease_deleted);

    Ok(())
}

/// Raising replicas on the session metadata bucket after it already exists
/// must not fail startup, same as the lease and tool/hook registry buckets.
/// Does not exercise a genuine "the cluster refused the raise" rejection —
/// see `harnx-nats-common`'s `registry_ttl.rs` tests for why a single-node
/// test server can't demonstrate that for an existing stream.
#[tokio::test]
async fn session_metadata_bucket_raising_replicas_does_not_fail_startup() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = local_nats_config(server.url());
    let jetstream = config.nats_jetstream("local").await?;

    SessionMetadataStore::ensure(&jetstream, 1)
        .await
        .context("create session metadata bucket at replicas=1")?;
    SessionMetadataStore::ensure(&jetstream, 3)
        .await
        .context("raising replicas on an existing bucket must not fail startup")?;

    let info = jetstream
        .get_stream(format!("KV_{SESSION_METADATA_BUCKET}"))
        .await
        .context("get backing stream")?
        .info()
        .await
        .context("stream info")?
        .clone();
    assert_eq!(info.config.num_replicas, 3, "the raise must actually apply");
    Ok(())
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
