mod common;

use anyhow::Result;
use common::spawn_nats_server;
use harnx_core::attachments::{
    cid_for_data_url, cid_for_data_url_with_session, read_attachment_async,
    store_attachment_bytes_async,
};
use harnx_core::cid_url::{CidUrl, SessionRef};
use harnx_core::message::{ImageUrl, Message, MessageContent, MessageContentPart, MessageRole};
use harnx_core::require_nextest;
use harnx_core::session::Session;
use harnx_runtime::config::{Config, GlobalConfig, SessionAttachmentPath};
use harnx_runtime::nats_attachments::{
    delete_session_attachments, externalize_message_attachments, hydrate_attachment_refs,
    sync_session_attachments, AttachmentLocation,
};
use parking_lot::RwLock;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

struct DataDirGuard {
    previous: Option<OsString>,
}

impl DataDirGuard {
    fn isolated(directory: &Path) -> Self {
        let previous = std::env::var_os("HARNX_DATA_DIR");
        // Nextest runs every test in a separate process, so this cannot race another test.
        unsafe { std::env::set_var("HARNX_DATA_DIR", directory) };
        Self { previous }
    }
}

impl Drop for DataDirGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("HARNX_DATA_DIR", value) },
            None => unsafe { std::env::remove_var("HARNX_DATA_DIR") },
        }
    }
}

fn image_content(data_url: &str) -> MessageContent {
    MessageContent::Array(vec![
        MessageContentPart::Text {
            text: "inspect this".to_string(),
        },
        MessageContentPart::ImageUrl {
            image_url: ImageUrl {
                url: data_url.to_string(),
            },
        },
    ])
}

fn image_ref(content: &MessageContent) -> &str {
    let MessageContent::Array(parts) = content else {
        panic!("expected multipart content");
    };
    parts
        .iter()
        .find_map(|part| match part {
            MessageContentPart::ImageUrl { image_url } => Some(image_url.url.as_str()),
            _ => None,
        })
        .expect("expected image part")
}

fn location<'a>(
    jetstream: &'a async_nats::jetstream::Context,
    session: &'a SessionRef,
) -> AttachmentLocation<'a> {
    AttachmentLocation::new(jetstream, 1, session)
}

struct ExternalizeHydrateArgs<'a> {
    jetstream: &'a async_nats::jetstream::Context,
    session: &'a harnx_core::cid_url::SessionRef,
    cache_dir: &'a std::path::Path,
    message_content: &'a mut harnx_client::MessageContent,
    cid_refs: &'a [String],
}

async fn externalize_and_hydrate(args: ExternalizeHydrateArgs<'_>) -> Result<()> {
    let location = location(args.jetstream, args.session);
    externalize_message_attachments(location, args.message_content, None).await?;
    hydrate_attachment_refs(location, args.cache_dir, args.cid_refs).await
}

async fn assert_session_deleted(
    jetstream: &async_nats::jetstream::Context,
    owner: &str,
    expected: usize,
) -> Result<()> {
    assert_eq!(
        delete_session_attachments(jetstream, owner).await?,
        expected
    );
    Ok(())
}

#[tokio::test]
async fn attachments_round_trip_and_delete_with_their_session() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let bytes = b"not really a png, but stable attachment bytes";
    let data_url = format!(
        "data:image/png;base64,{}",
        harnx_core::crypto::base64_encode(bytes)
    );
    let local_cid = cid_for_data_url(&data_url);
    let first_session = format!("attachment-a-{}", uuid::Uuid::new_v4());
    let second_session = format!("attachment-b-{}", uuid::Uuid::new_v4());
    let first_session_ref = SessionRef::new(None, first_session.clone())?;
    let second_session_ref = SessionRef::new(None, second_session.clone())?;
    let first_cid = cid_for_data_url_with_session(&first_session_ref, &data_url);
    let second_cid = cid_for_data_url_with_session(&second_session_ref, &data_url);

    for (session, cid) in [
        (&first_session_ref, &first_cid),
        (&second_session_ref, &second_cid),
    ] {
        let hydrated = tempfile::tempdir()?;
        let mut content = image_content(&data_url);
        externalize_and_hydrate(ExternalizeHydrateArgs {
            jetstream: &jetstream,
            session,
            cache_dir: hydrated.path(),
            message_content: &mut content,
            cid_refs: std::slice::from_ref(cid),
        })
        .await?;
        assert_eq!(image_ref(&content), cid);
        let (hydrated_bytes, mime_type) =
            read_attachment_async(hydrated.path(), &local_cid).await?;
        assert_eq!(hydrated_bytes, bytes);
        assert_eq!(mime_type, "image/png");
    }

    assert_session_deleted(&jetstream, &first_session_ref.owner(), 1).await?;
    assert_session_deleted(&jetstream, &first_session_ref.owner(), 0).await?;

    let second_hydrated = tempfile::tempdir()?;
    hydrate_attachment_refs(
        location(&jetstream, &second_session_ref),
        second_hydrated.path(),
        std::slice::from_ref(&second_cid),
    )
    .await?;
    assert_eq!(
        read_attachment_async(second_hydrated.path(), &local_cid)
            .await?
            .0,
        bytes
    );
    assert_session_deleted(&jetstream, &second_session_ref.owner(), 1).await?;

    Ok(())
}

#[tokio::test]
async fn canonical_local_attachment_reference_round_trips_through_nats() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let referenced_session = format!("attachment-ref-{}", uuid::Uuid::new_v4());
    let session_ref = SessionRef::new(None, referenced_session.clone())?;
    let source = tempfile::tempdir()?;
    let local_cid = store_attachment_bytes_async(
        source.path(),
        b"uploaded by the web UI",
        "text/plain;charset=utf-8",
    )
    .await?;
    let canonical_cid = CidUrl::Media {
        session: session_ref.clone(),
        hash: local_cid
            .strip_prefix("cid:")
            .expect("stored attachment has cid prefix")
            .to_string(),
    }
    .to_string();
    let mut referenced_content = MessageContent::Array(vec![MessageContentPart::ImageUrl {
        image_url: ImageUrl {
            url: canonical_cid.clone(),
        },
    }]);
    externalize_message_attachments(
        location(&jetstream, &session_ref),
        &mut referenced_content,
        Some(source.path()),
    )
    .await?;
    externalize_message_attachments(
        location(&jetstream, &session_ref),
        &mut referenced_content,
        None,
    )
    .await?;
    assert_eq!(image_ref(&referenced_content), canonical_cid);
    let referenced_hydrated = tempfile::tempdir()?;
    hydrate_attachment_refs(
        location(&jetstream, &session_ref),
        referenced_hydrated.path(),
        std::slice::from_ref(&canonical_cid),
    )
    .await?;
    let (referenced_bytes, referenced_mime) =
        read_attachment_async(referenced_hydrated.path(), &local_cid).await?;
    assert_eq!(referenced_bytes, b"uploaded by the web UI");
    assert_eq!(referenced_mime, "text/plain;charset=utf-8");
    assert_eq!(
        delete_session_attachments(&jetstream, &session_ref.owner()).await?,
        1
    );

    Ok(())
}

#[tokio::test]
async fn session_attachment_sync_uploads_local_cid_refs() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let data_root = tempfile::tempdir()?;
    let _data_dir = DataDirGuard::isolated(data_root.path());
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let session_id = format!("attachment-sync-{}", uuid::Uuid::new_v4());
    let agent_name = "attachment-sync-agent";
    let source = Config::session_attachments_dir(SessionAttachmentPath {
        agent_name,
        session_id: &session_id,
    })
    .expect("generated session ID is safe");
    let local_cid =
        store_attachment_bytes_async(&source, b"generated by a tool", "text/plain").await?;
    let session_ref = SessionRef::new(Some(agent_name.to_string()), session_id.clone())?;
    let cid = harnx_core::cid_url::CidUrl::Media {
        session: session_ref.clone(),
        hash: local_cid
            .strip_prefix("cid:")
            .expect("stored attachment has cid prefix")
            .to_string(),
    }
    .to_string();
    let session = Session {
        id: session_id.clone(),
        session_id: Some(session_id.clone()),
        agent_name: Some(agent_name.to_string()),
        messages: vec![Message::new(MessageRole::Tool, image_content(&cid))],
        ..Default::default()
    };
    let config = Config {
        session: Some(session),
        ..Default::default()
    };
    let config: GlobalConfig = Arc::new(RwLock::new(config));

    sync_session_attachments(&jetstream, &config, 1).await?;

    let hydrated = tempfile::tempdir()?;
    hydrate_attachment_refs(
        location(&jetstream, &session_ref),
        hydrated.path(),
        std::slice::from_ref(&cid),
    )
    .await?;
    assert_eq!(
        read_attachment_async(hydrated.path(), &local_cid).await?.0,
        b"generated by a tool"
    );
    Ok(())
}

#[tokio::test]
async fn local_only_attachment_is_backfilled_to_new_nats_key() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let migrated_session = format!("attachment-local-{}", uuid::Uuid::new_v4());
    let local_only = tempfile::tempdir()?;
    let local_only_cid =
        store_attachment_bytes_async(local_only.path(), b"local blob", "text/plain").await?;
    let session_ref = SessionRef::new(None, migrated_session.clone())?;
    let cid = harnx_core::cid_url::CidUrl::Media {
        session: session_ref.clone(),
        hash: local_only_cid
            .strip_prefix("cid:")
            .expect("stored attachment has cid prefix")
            .to_string(),
    }
    .to_string();
    hydrate_attachment_refs(
        location(&jetstream, &session_ref),
        local_only.path(),
        std::slice::from_ref(&cid),
    )
    .await?;
    let migrated_worker = tempfile::tempdir()?;
    hydrate_attachment_refs(
        location(&jetstream, &session_ref),
        migrated_worker.path(),
        std::slice::from_ref(&cid),
    )
    .await?;
    assert_eq!(
        read_attachment_async(migrated_worker.path(), &local_only_cid)
            .await?
            .0,
        b"local blob"
    );
    assert_eq!(
        delete_session_attachments(&jetstream, &session_ref.owner()).await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn missing_authoritative_and_local_attachment_is_rejected() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let local_cache = tempfile::tempdir()?;
    let session = SessionRef::new(None, "missing-attachment".to_string())?;
    let missing_cid = CidUrl::Media {
        session: session.clone(),
        hash: harnx_core::crypto::sha256("missing"),
    }
    .to_string();

    let error = hydrate_attachment_refs(
        location(&jetstream, &session),
        local_cache.path(),
        &[missing_cid],
    )
    .await
    .expect_err("an attachment missing from NATS and the local cache must fail");
    assert!(error.to_string().contains("download attachment"));
    Ok(())
}
