mod common;

use anyhow::Result;
use common::spawn_nats_server;
use harnx_core::attachments::{
    cid_for_data_url, read_attachment_async, store_attachment_bytes_async,
};
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
    let MessageContentPart::ImageUrl { image_url } = &parts[1] else {
        panic!("expected image part");
    };
    &image_url.url
}

fn location<'a>(
    jetstream: &'a async_nats::jetstream::Context,
    session_id: &'a str,
) -> AttachmentLocation<'a> {
    AttachmentLocation::new(jetstream, 1, session_id)
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
    let cid = cid_for_data_url(&data_url);
    let first_session = format!("attachment-a-{}", uuid::Uuid::new_v4());
    let second_session = format!("attachment-b-{}", uuid::Uuid::new_v4());

    let mut first_content = image_content(&data_url);
    externalize_message_attachments(
        location(&jetstream, &first_session),
        &mut first_content,
        None,
    )
    .await?;
    assert_eq!(image_ref(&first_content), cid);

    let mut second_content = image_content(&data_url);
    externalize_message_attachments(
        location(&jetstream, &second_session),
        &mut second_content,
        None,
    )
    .await?;
    assert_eq!(image_ref(&second_content), cid);

    let hydrated = tempfile::tempdir()?;
    hydrate_attachment_refs(
        location(&jetstream, &first_session),
        hydrated.path(),
        std::slice::from_ref(&cid),
    )
    .await?;
    let (hydrated_bytes, mime_type) = read_attachment_async(hydrated.path(), &cid).await?;
    assert_eq!(hydrated_bytes, bytes);
    assert_eq!(mime_type, "image/png");

    assert_eq!(
        delete_session_attachments(&jetstream, &first_session).await?,
        1
    );
    assert_eq!(
        delete_session_attachments(&jetstream, &first_session).await?,
        0
    );

    let second_hydrated = tempfile::tempdir()?;
    hydrate_attachment_refs(
        location(&jetstream, &second_session),
        second_hydrated.path(),
        std::slice::from_ref(&cid),
    )
    .await?;
    assert_eq!(
        read_attachment_async(second_hydrated.path(), &cid).await?.0,
        bytes
    );
    assert_eq!(
        delete_session_attachments(&jetstream, &second_session).await?,
        1
    );

    Ok(())
}

#[tokio::test]
async fn local_attachment_reference_round_trips_through_nats() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let referenced_session = format!("attachment-ref-{}", uuid::Uuid::new_v4());
    let source = tempfile::tempdir()?;
    let referenced_cid = store_attachment_bytes_async(
        source.path(),
        b"uploaded by the web UI",
        "text/plain;charset=utf-8",
    )
    .await?;
    let mut referenced_content = MessageContent::Array(vec![MessageContentPart::ImageUrl {
        image_url: ImageUrl {
            url: referenced_cid.clone(),
        },
    }]);
    externalize_message_attachments(
        location(&jetstream, &referenced_session),
        &mut referenced_content,
        Some(source.path()),
    )
    .await?;
    externalize_message_attachments(
        location(&jetstream, &referenced_session),
        &mut referenced_content,
        None,
    )
    .await?;
    let referenced_hydrated = tempfile::tempdir()?;
    hydrate_attachment_refs(
        location(&jetstream, &referenced_session),
        referenced_hydrated.path(),
        std::slice::from_ref(&referenced_cid),
    )
    .await?;
    let (referenced_bytes, referenced_mime) =
        read_attachment_async(referenced_hydrated.path(), &referenced_cid).await?;
    assert_eq!(referenced_bytes, b"uploaded by the web UI");
    assert_eq!(referenced_mime, "text/plain;charset=utf-8");
    assert_eq!(
        delete_session_attachments(&jetstream, &referenced_session).await?,
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
    let cid = store_attachment_bytes_async(&source, b"generated by a tool", "text/plain").await?;
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

    sync_session_attachments(&jetstream, &config, 1, &session_id).await?;

    let hydrated = tempfile::tempdir()?;
    hydrate_attachment_refs(
        location(&jetstream, &session_id),
        hydrated.path(),
        std::slice::from_ref(&cid),
    )
    .await?;
    assert_eq!(
        read_attachment_async(hydrated.path(), &cid).await?.0,
        b"generated by a tool"
    );
    Ok(())
}

#[tokio::test]
async fn legacy_local_only_attachment_is_backfilled_to_nats() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let migrated_session = format!("attachment-local-{}", uuid::Uuid::new_v4());
    let local_only = tempfile::tempdir()?;
    let local_only_cid =
        store_attachment_bytes_async(local_only.path(), b"legacy local blob", "text/plain").await?;
    hydrate_attachment_refs(
        location(&jetstream, &migrated_session),
        local_only.path(),
        std::slice::from_ref(&local_only_cid),
    )
    .await?;
    let migrated_worker = tempfile::tempdir()?;
    hydrate_attachment_refs(
        location(&jetstream, &migrated_session),
        migrated_worker.path(),
        std::slice::from_ref(&local_only_cid),
    )
    .await?;
    assert_eq!(
        read_attachment_async(migrated_worker.path(), &local_only_cid)
            .await?
            .0,
        b"legacy local blob"
    );
    assert_eq!(
        delete_session_attachments(&jetstream, &migrated_session).await?,
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
    let missing_cid = format!("cid:{}", harnx_core::crypto::sha256("missing"));

    let error = hydrate_attachment_refs(
        location(&jetstream, "missing-attachment"),
        local_cache.path(),
        &[missing_cid],
    )
    .await
    .expect_err("an attachment missing from NATS and the local cache must fail");
    assert!(error.to_string().contains("download attachment"));
    Ok(())
}
