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
    nats_session_index::{
        delete_record, ensure_index_bucket, get_record, put_record, SessionIndexRecord,
        SESSION_INDEX_BUCKET,
    },
    nats_session_log::{stream_name_for_session, NatsSessionLog},
};
use indexmap::IndexMap;

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
    log.append_event_async(&header(session_id)).await?;
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
            session_index: None,
        },
    )
    .await?
    .expect("lease acquired");
    let lease_key = NatsLeaseConfig::default().key_for_session(session_id);
    lease.stop_renewal_for_test().await;

    let index_store = ensure_index_bucket(&jetstream, 1).await?;
    let index_record = SessionIndexRecord {
        session_id: session_id.to_string(),
        agent_name: "oracle".to_string(),
        working_dir: Some("/tmp".to_string()),
        git_branch: Some("main".to_string()),
        git_remote: Some("origin".to_string()),
        title: None,
        last_activity: 1_700_000_000,
    };
    put_record(&index_store, &index_record).await?;

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
        get_record(&index_store, session_id).await?.is_none(),
        "session index record should be removed"
    );

    let deleted_again = delete_remote_session(&config, "local", session_id).await?;
    assert!(!deleted_again.stream_deleted);
    assert!(!deleted_again.lease_deleted);

    delete_record(&index_store, session_id).await.ok();

    Ok(())
}

/// Raising replicas on the session index bucket after it already exists
/// must not fail startup, same as the lease and tool/hook registry buckets.
/// Does not exercise a genuine "the cluster refused the raise" rejection —
/// see `harnx-nats-common`'s `registry_ttl.rs` tests for why a single-node
/// test server can't demonstrate that for an existing stream.
#[tokio::test]
async fn session_index_bucket_raising_replicas_does_not_fail_startup() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = local_nats_config(server.url());
    let jetstream = config.nats_jetstream("local").await?;

    ensure_index_bucket(&jetstream, 1)
        .await
        .context("create session index bucket at replicas=1")?;
    ensure_index_bucket(&jetstream, 3)
        .await
        .context("raising replicas on an existing bucket must not fail startup")?;

    let info = jetstream
        .get_stream(format!("KV_{SESSION_INDEX_BUCKET}"))
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

fn header(session_id: &str) -> SessionLogEntry {
    SessionLogEntry::Header {
        model_id: "test-model".to_string(),
        temperature: None,
        top_p: None,
        use_tools: None,
        compress_threshold: None,
        agent_name: Some("oracle".to_string()),
        session_id: Some(session_id.to_string()),
        working_dir: None,
        git_branch: None,
        git_remote: None,
        terminal_session_id: None,
        agent_variables: IndexMap::new(),
        agent_instructions: "delete me".to_string(),
        model_fallbacks: vec![],
        compaction_agent: None,
    }
}
