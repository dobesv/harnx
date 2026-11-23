//! A lagging follower misses the upload's metadata and may also host the
//! consumer. Divert both paths while leaving raw message gets on the leader.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer::push::OrderedConfig, stream};
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use harnx_blob_store::media::{
    ensure_attachments_bucket, get_media, leader_media_info, put_media_with_chunk_limit,
    AttachmentsBucket, MediaBlob, ATTACHMENTS_BUCKET,
};
use harnx_core::{
    attachments::{cid_for_data_url, cid_for_data_url_with_session, read_attachment_async},
    cid_url::{CidUrl, SessionRef},
    crypto::base64_decode,
    message::{ImageUrl, MessageContent, MessageContentPart},
    require_nextest,
    safety::format_size,
};
use harnx_runtime::nats_attachments::{
    externalize_message_attachments, hydrate_attachment_refs, AttachmentLocation,
};
use harnx_test_bins::{spawn_configured_nats_server, NatsServerHandle};

const STALE_REPLICA: &str = "STALE_ATTACHMENTS_REPLICA";
const MIME: &str = "image/png";
const PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=";
const CHUNK_LIMIT: usize = 16;

fn data_url() -> String {
    format!("data:{MIME};base64,{PNG_BASE64}")
}

fn stale_replica_config() -> String {
    let bucket = format!("OBJ_{ATTACHMENTS_BUCKET}");
    format!(
        r#"mappings = {{
  "$JS.API.DIRECT.GET.{bucket}": "$JS.API.DIRECT.GET.{STALE_REPLICA}"
  "$JS.API.DIRECT.GET.{bucket}.>": "$JS.API.DIRECT.GET.{STALE_REPLICA}.>"
  "$JS.API.CONSUMER.CREATE.{bucket}": "harnx.test.unanswered"
  "$JS.API.CONSUMER.CREATE.{bucket}.>": "harnx.test.unanswered.>"
  "$JS.API.CONSUMER.DURABLE.CREATE.{bucket}.>": "harnx.test.unanswered.>"
}}"#
    )
}

struct LaggingBroker {
    _server: NatsServerHandle,
    jetstream: jetstream::Context,
    store: AttachmentsBucket,
    session: SessionRef,
    url: CidUrl,
}

impl LaggingBroker {
    async fn start() -> Result<Option<Self>> {
        require_nextest();
        match spawn_configured_nats_server(&stale_replica_config()).await? {
            Some(server) => Self::open(server).await.map(Some),
            None => Ok(None),
        }
    }

    async fn open(server: NatsServerHandle) -> Result<Self> {
        let jetstream = jetstream::new(async_nats::connect(server.url()).await?);
        jetstream
            .create_stream(stream::Config {
                name: STALE_REPLICA.into(),
                subjects: vec!["harnx.test.stale-attachments".into()],
                allow_direct: true,
                storage: stream::StorageType::Memory,
                ..Default::default()
            })
            .await?;
        let store = ensure_attachments_bucket(&jetstream, 1).await?;
        let session = SessionRef::new(None, "lagging-upload".into())?;
        let url = CidUrl::parse(&cid_for_data_url_with_session(&session, &data_url()))?;
        Ok(Self {
            _server: server,
            jetstream,
            store,
            session,
            url,
        })
    }

    fn location(&self) -> AttachmentLocation<'_> {
        AttachmentLocation::new(&self.jetstream, 1, &self.session)
    }

    async fn seed_chunked(&self, bytes: &[u8]) -> Result<()> {
        let blob = MediaBlob {
            url: &self.url,
            bytes,
            mime_type: MIME,
        };
        put_media_with_chunk_limit(&self.store, blob, CHUNK_LIMIT).await?;
        let info = leader_media_info(&self.store, &self.url.kv_key())
            .await?
            .context("leader should see the seeded attachment")?;
        assert!(!info.deleted && info.chunks > 1);
        assert_eq!(info.size, bytes.len());
        assert_eq!(get_media(&self.store, &self.url).await?, None);
        self.assert_chunk_consumer_blocked(&info.nuid).await
    }

    // Probe the same ordered consumer that ObjectStore would use for chunks.
    // If the mapping broke, raw-get hydration could still pass this test.
    async fn assert_chunk_consumer_blocked(&self, nuid: &str) -> Result<()> {
        let stream = self
            .jetstream
            .get_stream_no_info(format!("OBJ_{ATTACHMENTS_BUCKET}"))
            .await?;
        let consumer = stream
            .create_consumer(OrderedConfig {
                deliver_subject: self.jetstream.client().new_inbox(),
                filter_subject: format!("$O.{ATTACHMENTS_BUCKET}.C.{nuid}"),
                ..Default::default()
            })
            .await;
        assert!(consumer.is_err(), "chunk consumer creation must be blocked");
        Ok(())
    }

    async fn admission_and_hydration_errors(&self) -> Result<(anyhow::Error, anyhow::Error)> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("missing-cache");
        let cid = self.url.to_string();
        let admission =
            externalize_message_attachments(self.location(), &mut image_ref(&cid), Some(&source))
                .await
                .expect_err("admission must refuse this attachment");
        let hydration = hydrate_attachment_refs(self.location(), &source, &[cid])
            .await
            .expect_err("cold hydration must refuse this attachment");
        assert!(!source.exists());
        Ok((admission, hydration))
    }
}

fn image_ref(cid: &str) -> MessageContent {
    MessageContent::Array(vec![MessageContentPart::ImageUrl {
        image_url: ImageUrl { url: cid.into() },
    }])
}

#[tokio::test]
async fn uploaded_attachment_is_admitted_and_hydrated_while_replicas_lag() -> Result<()> {
    let Some(broker) = LaggingBroker::start().await? else {
        return Ok(());
    };
    let bytes = base64_decode(PNG_BASE64)?;
    broker.seed_chunked(&bytes).await?;

    let temp = tempfile::tempdir()?;
    let source = temp.path().join("nonexistent-frontend-cache");
    let cid = broker.url.to_string();
    let mut content = image_ref(&cid);
    let location = broker.location();
    externalize_message_attachments(location, &mut content, Some(&source)).await?;
    let MessageContent::Array(parts) = content else {
        anyhow::bail!("expected multipart content");
    };
    let [MessageContentPart::ImageUrl { image_url }, MessageContentPart::Text { text }] =
        parts.as_slice()
    else {
        anyhow::bail!("expected the cid and its adjacent attachment marker");
    };
    assert_eq!(image_url.url, cid);
    let size = format_size(bytes.len());
    assert_eq!(text, &format!("[attachment: {cid} ({MIME}, {size})]"));
    assert!(!source.exists());

    let worker_cache = temp.path().join("cold-worker-cache");
    hydrate_attachment_refs(
        location,
        &worker_cache,
        std::slice::from_ref(&image_url.url),
    )
    .await?;
    assert_eq!(
        read_attachment_async(&worker_cache, &cid_for_data_url(&data_url())).await?,
        (bytes, MIME.to_owned())
    );
    Ok(())
}

#[tokio::test]
async fn missing_attachment_is_refused_after_leader_check() -> Result<()> {
    let Some(broker) = LaggingBroker::start().await? else {
        return Ok(());
    };
    assert!(leader_media_info(&broker.store, &broker.url.kv_key())
        .await?
        .is_none());
    let (admission, hydration) = broker.admission_and_hydration_errors().await?;
    assert!(admission
        .to_string()
        .starts_with("Failed to read attachments dir "));
    let cid = &broker.url;
    assert_eq!(
        hydration.to_string(),
        format!("download attachment '{cid}'")
    );
    Ok(())
}

#[tokio::test]
async fn hydration_checks_digest_of_bytes_read_from_leader() -> Result<()> {
    let Some(broker) = LaggingBroker::start().await? else {
        return Ok(());
    };
    let mut bytes = base64_decode(PNG_BASE64)?;
    bytes[0] ^= 1;
    broker.seed_chunked(&bytes).await?;

    let worker_cache = tempfile::tempdir()?;
    let error = hydrate_attachment_refs(
        broker.location(),
        worker_cache.path(),
        &[broker.url.to_string()],
    )
    .await
    .expect_err("digest mismatch must be rejected even on the leader path");
    assert!(error.to_string().starts_with("attachment digest mismatch:"));
    assert_eq!(std::fs::read_dir(worker_cache.path())?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn attachment_leader_metadata_decode_errors_are_not_treated_as_absence() -> Result<()> {
    let Some(broker) = LaggingBroker::start().await? else {
        return Ok(());
    };
    let subject = format!(
        "$O.{ATTACHMENTS_BUCKET}.M.{}",
        URL_SAFE.encode(broker.url.kv_key())
    );
    broker
        .jetstream
        .publish(subject, "not JSON".into())
        .await?
        .await?;
    assert_eq!(get_media(&broker.store, &broker.url).await?, None);

    let (admission, hydration) = broker.admission_and_hydration_errors().await?;
    for error in [admission, hydration] {
        assert!(
            error.downcast_ref::<serde_json::Error>().is_some(),
            "{error:#}"
        );
    }
    Ok(())
}
