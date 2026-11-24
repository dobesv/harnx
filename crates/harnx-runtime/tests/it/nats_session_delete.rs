use crate::common;

use anyhow::{Context, Result};
use async_nats::jetstream::{kv, Context as JetStream};
use common::{spawn_nats_server, NatsServerHandle};
use harnx_core::{
    message::{ImageUrl, MessageContent, MessageContentPart, MessageRole},
    require_nextest,
    session::SessionLogEntry,
};
use harnx_runtime::{
    config::Config,
    nats_admin::delete_remote_session,
    nats_attachments::{externalize_message_attachments, AttachmentLocation},
    nats_lease::NatsLeaseConfig,
    nats_session_log::{stream_name_for_session, NatsSessionLog},
    nats_session_metadata::{
        SessionInitializer, SessionMetadata, SessionMetadataStore, SESSION_METADATA_BUCKET,
    },
};

struct TestSessionSetup {
    // Preserve the lease until deletion; Drop otherwise releases it first.
    _lease: harnx_runtime::nats_lease::NatsSessionLease,
    lease_key: String,
    metadata_store: SessionMetadataStore,
}

impl TestSessionSetup {
    async fn prepare(
        jetstream: &async_nats::jetstream::Context,
        storage_key: &str,
        session_id: &str,
    ) -> Result<Self> {
        let log = NatsSessionLog::new_with_replicas(jetstream.clone(), storage_key, 1);
        append_user_message(&log).await?;

        let lease = harnx_runtime::nats_lease::NatsSessionLease::acquire(
            harnx_runtime::nats_lease::NatsLeaseAcquireParams {
                jetstream: jetstream.clone(),
                session_id: storage_key,
                worker_id: "w1".to_string(),
                generation: 1,
                config: NatsLeaseConfig::default(),
                session_metadata: None,
            },
        )
        .await?
        .expect("lease acquired");
        let lease_key = NatsLeaseConfig::default().key_for_session(storage_key);
        lease.stop_renewal_for_test().await;

        let metadata_store = SessionMetadataStore::ensure(jetstream, 1).await?;
        metadata_store
            .create(&SessionMetadata::new(
                session_id,
                SessionInitializer::named("oracle", Default::default()),
            ))
            .await?;

        Ok(Self {
            _lease: lease,
            lease_key,
            metadata_store,
        })
    }

    async fn assert_deleted(
        &self,
        config: &Config,
        jetstream: &async_nats::jetstream::Context,
        storage_key: &str,
    ) -> Result<()> {
        let stream_name = stream_name_for_session(storage_key);
        let err = jetstream
            .get_stream(&stream_name)
            .await
            .expect_err("stream should be gone");
        assert!(
            err.to_string().contains("stream not found")
                || err.to_string().contains("no responders")
        );

        let leases = config.nats_kv_bucket("local", "harnx_leases").await?;
        let lease_entry = leases
            .entry(self.lease_key.clone())
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
            self.metadata_store.get(storage_key).await?.is_none(),
            "session metadata should be removed"
        );
        Ok(())
    }
}

#[tokio::test]
async fn session_delete_removes_stream_and_lease_and_is_idempotent() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let config = local_nats_config(server.url());
    let jetstream = config.nats_jetstream("local").await?;
    let session_id = "delete-me";
    let storage_key = harnx_core::session_identity::session_key(Some("oracle"), session_id);
    let setup = TestSessionSetup::prepare(&jetstream, &storage_key, session_id).await?;

    let deleted = delete_remote_session(&config, "local", "oracle", session_id).await?;
    assert!(deleted.stream_deleted);
    assert!(deleted.lease_deleted);
    assert_eq!(deleted.metadata_keys_deleted, 2);

    setup
        .assert_deleted(&config, &jetstream, &storage_key)
        .await?;

    let deleted_again = delete_remote_session(&config, "local", "oracle", session_id).await?;
    assert!(!deleted_again.stream_deleted);
    assert!(!deleted_again.lease_deleted);

    Ok(())
}

#[tokio::test]
async fn session_delete_removes_attachments_and_is_idempotent() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let config = local_nats_config(server.url());
    let jetstream = config.nats_jetstream("local").await?;
    let session_id = "delete-attachments";
    let data_url = format!(
        "data:image/png;base64,{}",
        harnx_core::crypto::base64_encode(b"delete this attachment")
    );
    let mut content = MessageContent::Array(vec![MessageContentPart::ImageUrl {
        image_url: ImageUrl { url: data_url },
    }]);
    externalize_message_attachments(
        AttachmentLocation::new(
            &jetstream,
            1,
            &harnx_core::cid_url::SessionRef::new(
                Some("oracle".to_string()),
                session_id.to_string(),
            )
            .expect("test session_id valid"),
        ),
        &mut content,
        None,
    )
    .await?;

    let deleted = delete_remote_session(&config, "local", "oracle", session_id).await?;
    assert_eq!(deleted.attachments_deleted, 1);
    let deleted_again = delete_remote_session(&config, "local", "oracle", session_id).await?;
    assert_eq!(deleted_again.attachments_deleted, 0);
    Ok(())
}

/// Raising replicas on the session metadata bucket after it already exists
/// must not fail startup, same as the lease and tool registry buckets.
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
        ..Default::default()
    }
}

async fn append_user_message(log: &NatsSessionLog) -> Result<()> {
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("hello".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    Ok(())
}

struct PlanCleanupFailure {
    _server: NatsServerHandle,
    config: Config,
    js: JetStream,
    store: SessionMetadataStore,
    metadata: SessionMetadata,
    revision: u64,
    plans: kv::Store,
    plan_key: String,
}

impl PlanCleanupFailure {
    // qual:test_helper
    async fn seed() -> Result<Option<Self>> {
        let Some(server) = spawn_nats_server().await? else {
            return Ok(None);
        };
        let config = local_nats_config(server.url());
        let js = config.nats_jetstream("local").await?;
        let store = SessionMetadataStore::ensure(&js, 1).await?;
        let mut metadata = SessionMetadata::new(
            "partial-deletion",
            SessionInitializer::named("oracle", Default::default()).with_user_id("alice"),
        );
        metadata.created_at = chrono::DateTime::UNIX_EPOCH;
        let key = metadata.storage_key();
        let revision = store.create(&metadata).await?.unwrap();
        let activity = harnx_runtime::nats_session_metadata::SessionActivity {
            first_activation_at: None,
            last_activity_at: metadata.created_at,
        };
        store
            .kv_store()
            .put(
                harnx_runtime::nats_session_metadata::activity_key(&key),
                serde_json::to_vec(&activity)?.into(),
            )
            .await?;
        append_user_message(&NatsSessionLog::new_with_replicas(js.clone(), &key, 1)).await?;

        let plans = harnx_blob_store::plans::ensure_plans_bucket(&js, 1).await?;
        let plan_key = format!("plan/{key}/test/index.md");
        // The real plans cleanup rejects a document it can't decode as UTF-8.
        plans.put(&plan_key, vec![0xff].into()).await?;
        Ok(Some(Self {
            _server: server,
            config,
            js,
            store,
            metadata,
            revision,
            plans,
            plan_key,
        }))
    }

    // qual:test_helper
    async fn assert_partial_deletion(&self) -> Result<()> {
        let key = self.metadata.storage_key();
        assert!(self
            .js
            .get_stream(stream_name_for_session(&key))
            .await
            .is_err());
        assert!(self.store.get_activity(&key).await?.is_none());
        let record = self
            .store
            .get(&key)
            .await?
            .context("owner record was removed")?;
        assert_eq!(record.revision, self.revision);
        assert_eq!(record.metadata, self.metadata, "including the owner stamp");
        Ok(())
    }

    // qual:test_helper
    async fn repair_plan(&self) -> Result<()> {
        self.plans
            .put(&self.plan_key, "# Valid document".into())
            .await?;
        Ok(())
    }

    // qual:test_helper
    async fn assert_cleanup_finished(&self) -> Result<()> {
        assert!(self.plans.get(&self.plan_key).await?.is_none());
        assert!(self
            .store
            .get(&self.metadata.storage_key())
            .await?
            .is_none());
        Ok(())
    }
}

#[tokio::test]
async fn session_delete_keeps_owner_on_plan_cleanup_failure_and_can_retry() -> Result<()> {
    require_nextest();
    let Some(fixture) = PlanCleanupFailure::seed().await? else {
        return Ok(());
    };
    let id = &fixture.metadata.session_id;
    let error = delete_remote_session(&fixture.config, "local", "oracle", id)
        .await
        .expect_err("plan cleanup must fail the deletion");
    assert!(error.downcast_ref::<std::string::FromUtf8Error>().is_some());
    fixture.assert_partial_deletion().await?;
    fixture.repair_plan().await?;
    let retried = delete_remote_session(&fixture.config, "local", "oracle", id).await?;
    assert!(!retried.stream_deleted);
    assert_eq!(retried.attachments_deleted, 1);
    assert_eq!(retried.metadata_keys_deleted, 1);
    fixture.assert_cleanup_finished().await
}

#[tokio::test]
async fn retention_gc_reselects_partially_deleted_session_without_activity_or_transcript(
) -> Result<()> {
    use harnx_runtime::remote_session_cleanup::run_remote_cleanup;

    require_nextest();
    let Some(fixture) = PlanCleanupFailure::seed().await? else {
        return Ok(());
    };
    let failed = run_remote_cleanup(&fixture.config, 1, "local").await;
    assert_eq!((failed.scanned, failed.deleted, failed.errors), (1, 0, 1));
    fixture.assert_partial_deletion().await?;

    fixture.repair_plan().await?;
    let retried = run_remote_cleanup(&fixture.config, 1, "local").await;
    assert_eq!(
        (retried.scanned, retried.deleted, retried.errors),
        (1, 1, 0)
    );
    fixture.assert_cleanup_finished().await
}
