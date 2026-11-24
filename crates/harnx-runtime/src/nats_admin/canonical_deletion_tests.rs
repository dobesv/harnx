use super::*;
use crate::nats_session_metadata::{SessionInitializer, SessionMetadata};
use async_nats::jetstream::{self, stream};
use futures_util::StreamExt;
use harnx_test_bins::{spawn_configured_nats_server, NatsServerHandle};
use std::{future::Future, time::Duration};

const DELETE_REQUEST: &str = "tests.delete_transcript";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

struct DeletionFixture {
    server: NatsServerHandle,
    js: jetstream::Context,
    store: SessionMetadataStore,
    metadata: SessionMetadata,
    key: String,
    stream_name: String,
}

impl DeletionFixture {
    // qual:test_helper
    async fn start() -> Result<Option<Self>> {
        let metadata = SessionMetadata::new(
            "delete-canonical",
            SessionInitializer::named("oracle", Default::default()).with_user_id("alice"),
        );
        let key = metadata.storage_key();
        let stream_name = stream_name_for_session(&key);
        let config = format!(
            "jetstream: {{ domain: cascade }}\nmappings: {{ \"$JS.API.STREAM.DELETE.{stream_name}\": \"{DELETE_REQUEST}\" }}"
        );
        let Some(server) = spawn_configured_nats_server(&config).await? else {
            return Ok(None);
        };
        let mut js = jetstream::new(async_nats::connect(server.url()).await?);
        js.set_timeout(REQUEST_TIMEOUT);
        let store = SessionMetadataStore::ensure(&js, 1).await?;
        Ok(Some(Self {
            server,
            js,
            store,
            metadata,
            key,
            stream_name,
        }))
    }

    // qual:test_helper
    async fn seed_stream(&self) -> Result<()> {
        self.js
            .create_stream(stream::Config {
                name: self.stream_name.clone(),
                subjects: vec![format!("sessions.{}.>", self.key)],
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    // qual:test_helper
    async fn change_metadata_revision(&self) -> Result<u64> {
        let mut updated = self.metadata.clone();
        updated.title.value = Some("Changed title".into());
        Ok(self
            .store
            .kv_store()
            .put(
                metadata_key(&self.key),
                serde_json::to_vec(&updated)?.into(),
            )
            .await?)
    }

    /// Pause the real cascade at its transcript-delete request. Forward the
    /// request via the domain API after the check so the transcript is really
    /// deleted. No sleeps or production hooks are needed to force the race.
    // qual:test_helper
    async fn delete_with_transcript_check<T>(
        &self,
        order: StreamDeletion,
        check: impl Future<Output = Result<T>>,
    ) -> Result<(SessionDeleteResult, T)> {
        self.with_transcript_check(
            delete_session_state(&self.js, None, &self.key, order),
            check,
        )
        .await
    }

    // qual:test_helper
    async fn with_transcript_check<R, T>(
        &self,
        cascade: impl Future<Output = Result<R>>,
        check: impl Future<Output = Result<T>>,
    ) -> Result<(R, T)> {
        let client = self.js.client();
        let mut requests = client.subscribe(DELETE_REQUEST).await?;
        self.js.query_account().await?;
        let at_transcript = async {
            let request = tokio::time::timeout(REQUEST_TIMEOUT, requests.next())
                .await?
                .context("receive redirected transcript delete")?;
            let value = check.await?;
            let reply = client
                .request(
                    format!("$JS.cascade.API.STREAM.DELETE.{}", self.stream_name),
                    request.payload,
                )
                .await?;
            client
                .publish(
                    request.reply.context("transcript delete reply subject")?,
                    reply.payload,
                )
                .await?;
            Ok(value)
        };
        tokio::try_join!(cascade, at_transcript)
    }
}

#[tokio::test]
async fn canonical_metadata_is_purged_after_the_transcript_in_both_orders() -> Result<()> {
    let Some(fixture) = DeletionFixture::start().await? else {
        return Ok(());
    };
    for order in [StreamDeletion::First, StreamDeletion::Last] {
        let revision = fixture.store.create(&fixture.metadata).await?.unwrap();
        fixture.seed_stream().await?;
        let (deleted, ()) = fixture
            .delete_with_transcript_check(order, async {
                let record = fixture.store.get(&fixture.key).await?.unwrap();
                assert_eq!(record.revision, revision, "order {order:?}");
                assert_eq!(record.metadata, fixture.metadata);
                assert_eq!(
                    fixture.store.get_activity(&fixture.key).await?.is_some(),
                    order == StreamDeletion::First,
                    "activity should still be purged at the original step"
                );
                Ok(())
            })
            .await?;
        assert!(deleted.stream_deleted);
        assert_eq!(deleted.metadata_keys_deleted, 2);
        assert!(fixture.store.get(&fixture.key).await?.is_none());
        assert!(fixture.store.get_activity(&fixture.key).await?.is_none());
        let error = fixture
            .js
            .get_stream(&fixture.stream_name)
            .await
            .unwrap_err();
        assert!(stream_missing(&error.kind()));
    }
    Ok(())
}

#[tokio::test]
async fn absent_canonical_metadata_is_not_marked_by_deletion() -> Result<()> {
    let Some(fixture) = DeletionFixture::start().await? else {
        return Ok(());
    };
    fixture.seed_stream().await?;
    let (deleted, ()) = fixture
        .delete_with_transcript_check(StreamDeletion::Last, async { Ok(()) })
        .await?;
    assert!(deleted.stream_deleted);
    assert_eq!(deleted.metadata_keys_deleted, 0);
    let key = metadata_key(&fixture.key);
    assert!(fixture.store.leader_entry(&key).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn canonical_metadata_created_mid_deletion_survives() -> Result<()> {
    let Some(fixture) = DeletionFixture::start().await? else {
        return Ok(());
    };
    fixture.seed_stream().await?;
    let (deleted, revision) = fixture
        .delete_with_transcript_check(StreamDeletion::First, async {
            Ok(fixture.store.create(&fixture.metadata).await?.unwrap())
        })
        .await?;
    // Prefix enumeration happened after the PUT, but only activity was purged.
    assert!(deleted.stream_deleted);
    assert_eq!(deleted.metadata_keys_deleted, 1);
    let record = fixture.store.get(&fixture.key).await?.unwrap();
    assert_eq!(record.revision, revision);
    assert_eq!(record.metadata, fixture.metadata);
    Ok(())
}

#[tokio::test]
async fn canonical_metadata_with_changed_revision_survives_deletion() -> Result<()> {
    let Some(fixture) = DeletionFixture::start().await? else {
        return Ok(());
    };
    fixture.store.create(&fixture.metadata).await?.unwrap();
    fixture.seed_stream().await?;
    let (deleted, revision) = fixture
        .delete_with_transcript_check(StreamDeletion::Last, fixture.change_metadata_revision())
        .await?;
    assert!(deleted.stream_deleted);
    assert_eq!(
        deleted.canonical_metadata,
        CanonicalMetadataPurge::RevisionChanged
    );
    assert_eq!(deleted.metadata_keys_deleted, 1);
    let record = fixture.store.get(&fixture.key).await?.unwrap();
    assert_eq!(record.revision, revision);
    assert_eq!(
        record.metadata.title.value.as_deref(),
        Some("Changed title")
    );
    Ok(())
}

#[tokio::test]
async fn retention_gc_counts_changed_canonical_revision_as_skipped() -> Result<()> {
    use crate::remote_session_cleanup::{
        run_periodic_remote_cleanup_with, CleanupOutcome, PeriodicCleanupOverrides,
    };

    harnx_core::require_nextest();
    let Some(fixture) = DeletionFixture::start().await? else {
        return Ok(());
    };
    fixture.store.create(&fixture.metadata).await?.unwrap();
    fixture.seed_stream().await?;
    let mut config = Config::default();
    config
        .nats_servers
        .push(serde_json::from_value(serde_json::json!({
            "name": "local",
            "url": fixture.server.url(),
        }))?);
    let now = u64::try_from(fixture.metadata.created_at.timestamp())?;
    let cleanup = async {
        Ok(run_periodic_remote_cleanup_with(
            &config,
            1,
            "local",
            PeriodicCleanupOverrides {
                gc_session_id: "retention-test",
                epoch_hour: 1,
                now_unix_secs: now + 2 * 86_400,
            },
        )
        .await)
    };
    let (outcome, revision) = fixture
        .with_transcript_check(cleanup, fixture.change_metadata_revision())
        .await?;
    let CleanupOutcome::Ran(stats) = outcome else {
        panic!("expected GC to run, got {outcome:?}");
    };
    assert_eq!(stats.scanned, 1);
    assert_eq!(stats.skipped_metadata_changed, 1);
    assert_eq!(stats.deleted, 0);
    assert_eq!(stats.errors, 0);
    assert_eq!(
        fixture.store.get(&fixture.key).await?.unwrap().revision,
        revision
    );
    Ok(())
}
