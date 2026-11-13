use anyhow::{Context, Result};
use harnx_blob_store::{
    delete_owner, ensure_plans_bucket,
    media::{ensure_attachments_bucket, get_media, put_media},
    media_cid_url,
    plans::{
        create_document, delete_document, serialize_note, serialize_plan, serialize_task,
        NoteDocument, NoteFrontMatter, PlanDocument, PlanFrontMatter, TaskDocument,
        TaskFrontMatter,
    },
    resolve, resolve_bounded, touch_activity, BlobTooLarge, ResolvedBlob,
};
use harnx_core::cid_url::{CidUrl, PlanItem, SessionRef};
use harnx_nats_common::markers::DELETION_MARKER_TTL;

const HASH_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const HASH_B: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

async fn isolated_jetstream() -> Result<
    Option<(
        harnx_test_bins::NatsServerHandle,
        async_nats::jetstream::Context,
    )>,
> {
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(None);
    };
    let client = async_nats::connect(server.url())
        .await
        .context("connect to NATS test server")?;
    Ok(Some((server, async_nats::jetstream::new(client))))
}

#[tokio::test(flavor = "multi_thread")]
async fn media_put_get_round_trip() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let store = ensure_attachments_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(Some("pantheon/atlas".to_string()), "media1".to_string())?;
    let url = media_cid_url(&session, HASH_A);

    put_media(&store, &url, b"attachment bytes", "image/png").await?;

    assert_eq!(
        get_media(&store, &url).await?,
        Some((b"attachment bytes".to_vec(), "image/png".to_string()))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn media_chunk_limit_validates_clamps_and_preserves_existing_blob() -> Result<()> {
    use harnx_blob_store::media::{put_media_with_chunk_limit, MediaBlob};

    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let store = ensure_attachments_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(Some("pantheon/atlas".into()), "chunks1".into())?;
    let bytes = [0, 1, 254, 255].repeat(80_000);
    let mime_type = "audio/wav; codecs=pcm";
    for (hash, limit) in [(HASH_A, 16 * 1024), (HASH_B, 512 * 1024)] {
        let url = media_cid_url(&session, hash);
        let blob = MediaBlob {
            url: &url,
            bytes: &bytes,
            mime_type,
        };
        let error = put_media_with_chunk_limit(&store, blob, 0)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "attachment chunk limit must be positive");
        assert!(get_media(&store, &url).await?.is_none());
        put_media_with_chunk_limit(&store, blob, limit).await?;
        let info = store.info(url.kv_key()).await?;
        let chunk_size = limit.min(128 * 1024);
        assert_eq!(
            info.options.as_ref().unwrap().max_chunk_size,
            Some(chunk_size)
        );
        assert_eq!(info.chunks, bytes.len().div_ceil(chunk_size));
        assert_eq!(
            get_media(&store, &url).await?,
            Some((bytes.clone(), mime_type.into()))
        );
        put_media_with_chunk_limit(
            &store,
            MediaBlob {
                url: &url,
                bytes: b"not the original",
                mime_type: "text/plain",
            },
            1,
        )
        .await?;
        assert_eq!(store.info(url.kv_key()).await?, info);
        assert_eq!(
            get_media(&store, &url).await?,
            Some((bytes.clone(), mime_type.into()))
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_media_returns_bytes_and_cache_metadata() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let store = ensure_attachments_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(Some("pantheon/atlas".to_string()), "resolv1".to_string())?;
    let url = media_cid_url(&session, HASH_A);
    put_media(&store, &url, b"resolved bytes", "text/plain").await?;

    assert_eq!(
        resolve(&jetstream, &url).await?,
        ResolvedBlob {
            mime_type: "text/plain".to_string(),
            bytes: b"resolved bytes".to_vec(),
            etag: Some(HASH_A.to_string()),
            immutable: true,
        }
    );
    assert!(
        !activity_recorded(&jetstream, &session).await?,
        "a read must leave session activity alone"
    );
    Ok(())
}

/// Whether `session` has an activity record. Nothing creates the session
/// bucket until something records activity.
async fn activity_recorded(
    jetstream: &async_nats::jetstream::Context,
    session: &SessionRef,
) -> Result<bool> {
    let Ok(activity) = jetstream.get_key_value("harnx_sessions").await else {
        return Ok(false);
    };
    Ok(activity
        .get(format!("sessions/{}/activity", session.owner()))
        .await?
        .is_some())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_missing_media_returns_error() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    ensure_attachments_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(None, "miss01".to_string())?;
    let url = media_cid_url(&session, HASH_B);

    let error = resolve(&jetstream, &url).await.unwrap_err();
    assert!(error.to_string().contains("attachment not found"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_bounded_media_preserves_bytes_and_cache_metadata() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let store = ensure_attachments_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(Some("pantheon/atlas".into()), "bound1".into())?;
    let url = media_cid_url(&session, HASH_A);
    let bytes = [0, 255, 128, 13, 10].repeat(30_000);
    put_media(&store, &url, &bytes, "image/png").await?;

    let bounded = resolve_bounded(&jetstream, &url, bytes.len()).await?;
    assert!(!activity_recorded(&jetstream, &session).await?);
    assert_eq!(bounded, resolve(&jetstream, &url).await?);
    for max_bytes in [bytes.len() + 1, usize::MAX] {
        assert_eq!(resolve_bounded(&jetstream, &url, max_bytes).await?, bounded);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_bounded_empty_media_accepts_zero_limit() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let store = ensure_attachments_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(None, "bound2".into())?;
    let url = media_cid_url(&session, HASH_A);
    put_media(&store, &url, b"", "application/octet-stream").await?;

    assert_eq!(
        resolve_bounded(&jetstream, &url, 0).await?,
        resolve(&jetstream, &url).await?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_bounded_media_rejects_oversize_before_reading() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let store = ensure_attachments_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(None, "bound3".into())?;
    let url = media_cid_url(&session, HASH_A);
    let bytes = [255; 1024];
    put_media(&store, &url, &bytes, "application/octet-stream").await?;
    // Any read of this single-chunk object fails digest validation.
    rewrite_last_media_info(&jetstream, &url, |info| {
        info.digest = Some("SHA-256=invalid".into());
    })
    .await?;

    for max_bytes in [0, bytes.len() - 1] {
        let error = resolve_bounded(&jetstream, &url, max_bytes)
            .await
            .unwrap_err();
        assert_too_large(&error, bytes.len(), max_bytes);
    }
    let error = resolve(&jetstream, &url).await.unwrap_err();
    assert!(format!("{error:#}").contains("wrong digest"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_bounded_media_stops_when_metadata_understates_size() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let store = ensure_attachments_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(None, "bound4".into())?;
    let url = media_cid_url(&session, HASH_A);
    put_media(&store, &url, &vec![255; 512 * 1024], "image/png").await?;
    rewrite_last_media_info(&jetstream, &url, |info| {
        info.size = 0;
        // Reading through the final chunk would fail instead of returning BlobTooLarge.
        info.digest = Some("SHA-256=invalid".into());
    })
    .await?;

    for max_bytes in [0, 16, 128 * 1024] {
        let error = resolve_bounded(&jetstream, &url, max_bytes)
            .await
            .unwrap_err();
        assert_too_large(&error, max_bytes + 1, max_bytes);
    }
    Ok(())
}

async fn rewrite_last_media_info(
    jetstream: &async_nats::jetstream::Context,
    url: &CidUrl,
    rewrite: impl FnOnce(&mut async_nats::jetstream::object_store::ObjectInfo),
) -> Result<()> {
    let stream = jetstream.get_stream("OBJ_harnx_attachments").await?;
    let message = stream
        .get_raw_message(stream.cached_info().state.last_sequence)
        .await?;
    let mut info: async_nats::jetstream::object_store::ObjectInfo =
        serde_json::from_slice(&message.payload)?;
    assert_eq!(info.name, url.kv_key());
    rewrite(&mut info);
    jetstream
        .publish(message.subject, serde_json::to_vec(&info)?.into())
        .await?
        .await?;
    Ok(())
}

fn assert_too_large(error: &anyhow::Error, size_bytes: usize, max_bytes: usize) {
    assert_eq!(
        error.downcast_ref::<BlobTooLarge>(),
        Some(&BlobTooLarge {
            size_bytes,
            max_bytes,
        })
    );
    assert_eq!(
        error.to_string(),
        format!("blob too large: {size_bytes} bytes exceeds limit of {max_bytes} bytes")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_bounded_missing_media_returns_not_found() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let session = SessionRef::new(None, "bound5".into())?;
    let url = media_cid_url(&session, HASH_A);
    assert_bounded_not_found(&jetstream, &url).await?;
    let store = ensure_attachments_bucket(&jetstream, 1).await?;
    assert_bounded_not_found(&jetstream, &url).await?;
    put_media(&store, &url, b"deleted", "text/plain").await?;
    store.delete(url.kv_key()).await?;
    assert_bounded_not_found(&jetstream, &url).await?;
    Ok(())
}

async fn assert_bounded_not_found(
    jetstream: &async_nats::jetstream::Context,
    url: &CidUrl,
) -> Result<()> {
    let error = resolve_bounded(jetstream, url, 0).await.unwrap_err();
    assert!(error.to_string().contains("attachment not found"));
    assert!(error.downcast_ref::<BlobTooLarge>().is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_bounded_plan_checks_rendered_markdown_size() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let fixture = seed_rendered_plan(&jetstream).await?;
    let store = ensure_plans_bucket(&jetstream, 1).await?;
    let mut note = note_document(&fixture.note);
    note.body = "Résumé 🦀".into();
    store
        .put(fixture.note.kv_key(), serialize_note(&note)?.into())
        .await?;
    for url in [&fixture.plan, &fixture.task, &fixture.note] {
        let expected = resolve(&jetstream, url).await?;
        let size = expected.bytes.len();
        assert_eq!(resolve_bounded(&jetstream, url, size).await?, expected);
        assert_eq!(
            resolve_bounded(&jetstream, url, usize::MAX).await?,
            expected
        );
        for max_bytes in [0, size - 1] {
            let error = resolve_bounded(&jetstream, url, max_bytes)
                .await
                .unwrap_err();
            assert_too_large(&error, size, max_bytes);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_renders_plan_index_task_and_note() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let fixture = seed_rendered_plan(&jetstream).await?;
    let PlanResolveFixture {
        plan,
        task,
        note,
        plan_revision,
        task_revision,
        note_revision,
    } = fixture;

    let index = resolve(&jetstream, &plan).await?;
    assert_plan_blob(&index, note_revision);
    let index_markdown = String::from_utf8(index.bytes)?;
    assert!(index_markdown.starts_with("# Project Plan\n\nShip plan rendering"));
    assert!(index_markdown.contains(&format!("[Build renderer]({task})")));
    assert!(index_markdown.contains(&format!("[Decision]({note}) — Atlas")));

    let rendered_task = resolve(&jetstream, &task).await?;
    assert_plan_blob(&rendered_task, task_revision);
    let task_markdown = String::from_utf8(rendered_task.bytes)?;
    assert!(task_markdown.contains(&format!("[Plan Index]({plan})")));
    assert!(task_markdown.contains("# Build renderer"));

    let rendered_note = resolve(&jetstream, &note).await?;
    assert_plan_blob(&rendered_note, note_revision);
    let note_markdown = String::from_utf8(rendered_note.bytes)?;
    assert!(note_markdown.contains("# Note: Decision"));
    assert!(note_markdown.contains("Author: Atlas"));
    assert!(plan_revision < task_revision && task_revision < note_revision);
    Ok(())
}

struct PlanResolveFixture {
    plan: CidUrl,
    task: CidUrl,
    note: CidUrl,
    plan_revision: u64,
    task_revision: u64,
    note_revision: u64,
}

async fn seed_rendered_plan(
    jetstream: &async_nats::jetstream::Context,
) -> Result<PlanResolveFixture> {
    let store = ensure_plans_bucket(jetstream, 1).await?;
    let session = SessionRef::new(None, "plan01".to_string())?;
    let plan = plan_item_url(&session, PlanItem::Index);
    let task = plan_item_url(&session, PlanItem::Task("build".to_string()));
    let note = plan_item_url(&session, PlanItem::Note("decision".to_string()));
    let plan_revision =
        create_document(&store, &plan, &serialize_plan(&plan_document(&plan))?).await?;
    let task_revision = create_document(
        &store,
        &task,
        &serialize_task(&task_document(&plan, &task))?,
    )
    .await?;
    let note_revision =
        create_document(&store, &note, &serialize_note(&note_document(&note))?).await?;
    Ok(PlanResolveFixture {
        plan,
        task,
        note,
        plan_revision,
        task_revision,
        note_revision,
    })
}

fn plan_document(plan: &CidUrl) -> PlanDocument {
    PlanDocument {
        front: PlanFrontMatter {
            id: plan.to_string(),
            title: Some("Project Plan".to_string()),
            summary: Some("Ship plan rendering".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            ..PlanFrontMatter::default()
        },
        body: "Plan body".to_string(),
    }
}

fn task_document(plan: &CidUrl, task: &CidUrl) -> TaskDocument {
    TaskDocument {
        front: TaskFrontMatter {
            id: task.to_string(),
            title: "Build renderer".to_string(),
            summary: Some("Render every item".to_string()),
            author: None,
            assignee: None,
            executor: None,
            tags: Vec::new(),
            plan: plan.to_string(),
            status: "open".to_string(),
            created_at: "2026-01-02T00:00:00Z".to_string(),
            updated_at: None,
            dependencies: Vec::new(),
        },
        body: "Task body".to_string(),
    }
}

fn note_document(note: &CidUrl) -> NoteDocument {
    NoteDocument {
        front: NoteFrontMatter {
            id: note.to_string(),
            summary: Some("Decision".to_string()),
            author: Some("Atlas".to_string()),
            created_at: "2026-01-03T00:00:00Z".to_string(),
            updated_at: None,
        },
        body: "Use markdown.".to_string(),
    }
}

fn plan_item_url(session: &SessionRef, item: PlanItem) -> CidUrl {
    CidUrl::Plan {
        session: session.clone(),
        slug: "project-plan".to_string(),
        item,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_missing_plan_returns_error() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    ensure_plans_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(None, "misspl".to_string())?;
    let url = plan_item_url(&session, PlanItem::Index);

    let error = resolve(&jetstream, &url).await.unwrap_err();
    assert_eq!(error.to_string(), format!("plan document not found: {url}"));
    Ok(())
}

fn assert_plan_blob(blob: &ResolvedBlob, revision: u64) {
    assert_eq!(blob.mime_type, "text/markdown; charset=utf-8");
    assert_eq!(blob.etag, Some(revision.to_string()));
    assert!(!blob.immutable);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_owner_removes_media_and_plan_prefixes_only() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let media_store = ensure_attachments_bucket(&jetstream, 1).await?;
    let plans_store = ensure_plans_bucket(&jetstream, 1).await?;
    let deleted_session =
        SessionRef::new(Some("pantheon/atlas".to_string()), "delete1".to_string())?;
    let retained_session =
        SessionRef::new(Some("pantheon/atlas".to_string()), "retain1".to_string())?;
    let deleted_media = media_cid_url(&deleted_session, HASH_A);
    let retained_media = media_cid_url(&retained_session, HASH_B);
    put_media(&media_store, &deleted_media, b"delete", "text/plain").await?;
    put_media(&media_store, &retained_media, b"retain", "text/plain").await?;

    let deleted_plan = CidUrl::Plan {
        session: deleted_session.clone(),
        slug: "project-plan".to_string(),
        item: PlanItem::Index,
    };
    let deleted_task = CidUrl::Plan {
        session: deleted_session.clone(),
        slug: "project-plan".to_string(),
        item: PlanItem::Task("task-1".to_string()),
    };
    let retained_plan = CidUrl::Plan {
        session: retained_session,
        slug: "other-plan".to_string(),
        item: PlanItem::Index,
    };
    plans_store
        .put(&deleted_plan.kv_key(), b"plan".as_slice().into())
        .await?;
    plans_store
        .put(&deleted_task.kv_key(), b"task".as_slice().into())
        .await?;
    plans_store
        .put(&retained_plan.kv_key(), b"retain".as_slice().into())
        .await?;

    assert_eq!(delete_owner(&jetstream, &deleted_session.owner()).await?, 3);
    assert_eq!(get_media(&media_store, &deleted_media).await?, None);
    assert!(plans_store.get(&deleted_plan.kv_key()).await?.is_none());
    assert!(plans_store.get(&deleted_task.kv_key()).await?.is_none());
    assert!(get_media(&media_store, &retained_media).await?.is_some());
    assert!(plans_store.get(&retained_plan.kv_key()).await?.is_some());

    // The deleted object leaves no metadata entry behind, and the deleted
    // plan keys keep only markers that expire.
    assert_eq!(
        object_metadata_messages(&jetstream, &deleted_media.kv_key()).await?,
        0
    );
    for key in [deleted_plan.kv_key(), deleted_task.kv_key()] {
        assert_eq!(
            ttl_of(
                &jetstream,
                "KV_harnx_plans",
                &format!("$KV.harnx_plans.{key}")
            )
            .await?,
            Some(DELETION_MARKER_TTL.as_secs().to_string())
        );
    }
    Ok(())
}

/// Messages left on an attachment object's metadata subject.
async fn object_metadata_messages(
    jetstream: &async_nats::jetstream::Context,
    name: &str,
) -> Result<u64> {
    use base64::Engine as _;
    let subject = format!(
        "$O.harnx_attachments.M.{}",
        base64::engine::general_purpose::URL_SAFE.encode(name)
    );
    use futures_util::StreamExt as _;
    let stream = jetstream.get_stream("OBJ_harnx_attachments").await?;
    let mut subjects = stream.info_with_subjects(&subject).await?;
    let mut count = 0;
    while let Some(entry) = subjects.next().await {
        count += entry?.1 as u64;
    }
    Ok(count)
}

/// The `Nats-TTL` header on the latest message for `subject` in `stream`.
async fn ttl_of(
    jetstream: &async_nats::jetstream::Context,
    stream: &str,
    subject: &str,
) -> Result<Option<String>> {
    let message = jetstream
        .get_stream(stream)
        .await?
        .get_last_raw_message_by_subject(subject)
        .await
        .with_context(|| format!("read the latest message for {subject}"))?;
    Ok(message
        .headers
        .get(async_nats::header::NATS_MESSAGE_TTL)
        .map(|ttl| ttl.as_str().to_owned()))
}

/// Builds before this created the plans bucket without per-message TTLs.
/// Opening it turns them on, so a deleted document leaves a marker that
/// expires.
#[tokio::test(flavor = "multi_thread")]
async fn opening_an_older_plans_bucket_allows_expiring_markers() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    jetstream
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: "harnx_plans".to_string(),
            storage: async_nats::jetstream::stream::StorageType::File,
            num_replicas: 1,
            ..Default::default()
        })
        .await
        .context("create the bucket the way older builds did")?;

    let store = ensure_plans_bucket(&jetstream, 1).await?;

    assert!(store.stream.cached_info().config.allow_message_ttl);
    let session = SessionRef::new(Some("pantheon/atlas".to_string()), "older1".to_string())?;
    let plan = plan_item_url(&session, PlanItem::Index);
    create_document(&store, &plan, "plan").await?;
    delete_document(&store, &plan).await?;
    assert_eq!(
        ttl_of(
            &jetstream,
            "KV_harnx_plans",
            &format!("$KV.harnx_plans.{}", plan.kv_key())
        )
        .await?,
        Some(DELETION_MARKER_TTL.as_secs().to_string())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn touch_activity_debounces_second_write() -> Result<()> {
    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let owner = SessionRef::new(None, "touch01".to_string())?.owner();
    let key = format!("sessions/{owner}/activity");

    let first = touch_activity(&jetstream, &owner).await?;
    assert!(first.wrote());
    let store = jetstream
        .get_key_value("harnx_sessions")
        .await
        .context("open session activity bucket")?;
    let first_revision = store
        .entry(&key)
        .await?
        .context("first activity entry")?
        .revision;

    let second = touch_activity(&jetstream, &owner).await?;
    assert!(!second.wrote());
    let second_revision = store
        .entry(&key)
        .await?
        .context("second activity entry")?
        .revision;
    assert_eq!(second_revision, first_revision);
    Ok(())
}

#[test]
fn plan_documents_preserve_yaml_frontmatter_markdown() -> Result<()> {
    use harnx_blob_store::plans::{parse_plan, serialize_plan, PlanDocument, PlanFrontMatter};

    let document = PlanDocument {
        front: PlanFrontMatter {
            id: "cid:plan:pantheon%2Fatlas/abcDEF/test-plan".to_string(),
            title: Some("Test plan".to_string()),
            github_owner_repo: Some("dobesv/harnx".to_string()),
            github_issue: Some(2266),
            external_task_url: Some("https://tracker.invalid/browse/HARNX-2266".to_string()),
            created_at: "2026-09-29T00:00:00Z".to_string(),
            ..PlanFrontMatter::default()
        },
        body: "# Body\n\nMarkdown stays intact.\n".to_string(),
    };
    let serialized = serialize_plan(&document)?;
    assert!(serialized.starts_with("---\n"));
    assert!(serialized.contains("\n---\n# Body"));
    assert_eq!(parse_plan(&serialized)?, document);
    Ok(())
}

#[test]
fn plan_documents_without_parent_issue_remain_compatible() -> Result<()> {
    use harnx_blob_store::plans::{parse_plan, serialize_plan, PlanDocument, PlanFrontMatter};

    let legacy = "---\nid: cid:plan:pantheon%2Fatlas/abcDEF/test-plan\ntitle: Test plan\ngithub_owner_repo: dobesv/harnx\ncreated_at: 2026-09-29T00:00:00Z\n---\n# Body\n\nMarkdown stays intact.\n";
    let expected = PlanDocument {
        front: PlanFrontMatter {
            id: "cid:plan:pantheon%2Fatlas/abcDEF/test-plan".to_string(),
            title: Some("Test plan".to_string()),
            github_owner_repo: Some("dobesv/harnx".to_string()),
            created_at: "2026-09-29T00:00:00Z".to_string(),
            ..PlanFrontMatter::default()
        },
        body: "# Body\n\nMarkdown stays intact.\n".to_string(),
    };
    assert_eq!(parse_plan(legacy)?, expected);
    for fields in [
        "parent_issue: null\n",
        "github_issue: null\nexternal_task_url: null\n",
    ] {
        let with_null = legacy.replace("created_at:", &format!("{fields}created_at:"));
        assert_eq!(parse_plan(&with_null)?, expected);
    }
    let serialized = serialize_plan(&expected)?;
    assert!(!serialized.contains("parent_issue:"));
    assert!(!serialized.contains("github_issue:"));
    assert!(!serialized.contains("external_task_url:"));
    assert_eq!(parse_plan(&serialized)?, expected);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_update_retries_revision_conflict_without_losing_writes() -> Result<()> {
    use harnx_blob_store::plans::{
        create_document, get_document, parse_plan, serialize_plan, PlanDocument, PlanFrontMatter,
    };
    use std::sync::{Arc, Barrier};

    harnx_core::require_nextest();
    let Some((_server, jetstream)) = isolated_jetstream().await? else {
        return Ok(());
    };
    let store = ensure_plans_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(Some("pantheon/atlas".to_string()), "cas001".to_string())?;
    let url = CidUrl::Plan {
        session,
        slug: "conflict-test".to_string(),
        item: PlanItem::Index,
    };
    assert!(url.kv_key().ends_with("/conflict-test/plan"));
    let initial = serialize_plan(&PlanDocument {
        front: PlanFrontMatter {
            id: url.to_string(),
            created_at: "2026-09-29T00:00:00Z".to_string(),
            ..PlanFrontMatter::default()
        },
        body: "start".to_string(),
    })?;
    create_document(&store, &url, &initial).await?;

    let barrier = Arc::new(Barrier::new(2));
    let first = spawn_append(store.clone(), url.clone(), barrier.clone(), " A");
    let second = spawn_append(store.clone(), url.clone(), barrier, " B");
    first.await??;
    second.await??;

    let stored = get_document(&store, &url).await?.context("updated plan")?;
    let document = parse_plan(&stored.content)?;
    assert!(document.body.contains(" A"), "first update was lost");
    assert!(document.body.contains(" B"), "second update was lost");
    Ok(())
}

fn spawn_append(
    store: async_nats::jetstream::kv::Store,
    url: CidUrl,
    barrier: std::sync::Arc<std::sync::Barrier>,
    suffix: &'static str,
) -> tokio::task::JoinHandle<Result<harnx_blob_store::plans::StoredDocument>> {
    tokio::spawn(async move {
        let mut first_attempt = true;
        harnx_blob_store::plans::update_document(&store, &url, |content| {
            let mut document = harnx_blob_store::plans::parse_plan(content)?;
            if first_attempt {
                first_attempt = false;
                // Keep both initial reads aligned without parking a Tokio worker needed by its peer.
                tokio::task::block_in_place(|| barrier.wait());
            }
            document.body.push_str(suffix);
            harnx_blob_store::plans::serialize_plan(&document)
        })
        .await
    })
}

#[test]
fn legacy_parent_issue_frontmatter_serializes_with_canonical_names() -> Result<()> {
    use harnx_blob_store::plans::{parse_plan, serialize_plan};

    let legacy = "---\nid: cid:plan:pantheon%2Fatlas/abcDEF/test-plan\nparent_issue: 2266\ncreated_at: 2026-09-29T00:00:00Z\n---\nlegacy body";
    let document = parse_plan(legacy)?;
    assert_eq!(
        document,
        PlanDocument {
            front: PlanFrontMatter {
                id: "cid:plan:pantheon%2Fatlas/abcDEF/test-plan".to_string(),
                github_issue: Some(2266),
                created_at: "2026-09-29T00:00:00Z".to_string(),
                ..PlanFrontMatter::default()
            },
            body: "legacy body".to_string(),
        }
    );
    let serialized = serialize_plan(&document)?;
    assert!(serialized.contains("github_issue: 2266"));
    assert!(!serialized.contains("parent_issue:"));
    assert_eq!(parse_plan(&serialized)?, document);
    Ok(())
}
