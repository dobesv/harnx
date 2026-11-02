use super::*;
use crate::{FsServer, FsToolset};
use harnx_blob_store::{
    media::{ensure_attachments_bucket, put_media},
    plans::{create_document, serialize_plan, PlanDocument, PlanFrontMatter},
};
use harnx_tool_allow::ResolvedAllowlist;
use std::sync::Arc;

struct FakeResolver;

#[async_trait]
impl AttachmentResolver for FakeResolver {
    async fn resolve(
        &self,
        url: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, AttachmentResolveError> {
        assert_eq!(url, "cid:plan:_temp/abcdef/test");
        assert_eq!(max_bytes, 1024);
        Ok(vec![0, 255])
    }
}

#[tokio::test]
async fn resolver_defaults_to_none_and_is_shared_by_server_clones() {
    let server = FsServer::new(ResolvedAllowlist::new());
    let toolset = FsToolset::new(ResolvedAllowlist::new());
    assert!(server.attachment_resolver().is_none());
    assert!(toolset.attachment_resolver().is_none());

    let resolver: Arc<dyn AttachmentResolver> = Arc::new(FakeResolver);
    let server = server.with_attachment_resolver(resolver.clone());
    let toolset = toolset.with_attachment_resolver(resolver.clone());
    assert!(Arc::ptr_eq(
        server.clone().attachment_resolver().unwrap(),
        &resolver
    ));
    assert!(Arc::ptr_eq(
        toolset.clone().attachment_resolver().unwrap(),
        &resolver
    ));
    let bytes = server
        .attachment_resolver()
        .unwrap()
        .resolve("cid:plan:_temp/abcdef/test", 1024)
        .await
        .unwrap();
    assert_eq!(bytes, [0, 255]);
}

#[test]
fn resolution_error_categories_preserve_sources() {
    use std::error::Error;

    let url = CidUrl::parse("cid:plan:_temp/abcdef/test").unwrap();
    let too_large = classify_resolution_error(
        &url,
        anyhow::Error::new(BlobTooLarge {
            size_bytes: 3,
            max_bytes: 2,
        })
        .context("render plan"),
    );
    assert!(matches!(too_large, AttachmentResolveError::TooLarge(_)));
    assert!(too_large.source().unwrap().is::<BlobTooLarge>());
    assert!(too_large.to_string().contains("3 bytes exceeds limit of 2"));

    let missing = classify_resolution_error(
        &url,
        anyhow::anyhow!("plan document not found: {url}").context("resolve plan"),
    );
    assert!(matches!(missing, AttachmentResolveError::NotFound(_)));
    assert!(missing.source().is_some());

    let other = classify_resolution_error(&url, anyhow::anyhow!("server not found"));
    assert!(matches!(other, AttachmentResolveError::Other(_)));
    assert!(other.to_string().contains("server not found"));
}

async fn isolated_resolver() -> anyhow::Result<
    Option<(
        harnx_test_bins::NatsServerHandle,
        async_nats::jetstream::Context,
        NatsAttachmentResolver,
    )>,
> {
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(None);
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let resolver = NatsAttachmentResolver::new(jetstream.clone());
    Ok(Some((server, jetstream, resolver)))
}

#[tokio::test]
async fn nats_resolver_returns_exact_binary_and_enforces_bound() -> anyhow::Result<()> {
    let Some((_server, jetstream, resolver)) = isolated_resolver().await? else {
        return Ok(());
    };
    let store = ensure_attachments_bucket(&jetstream, 1).await?;
    let url = format!("cid:media:_temp/abcdef/{}", "a".repeat(64));
    let parsed = CidUrl::parse(&url)?;
    let bytes = b"\x89PNG\r\n\x1a\n\xff\0";
    put_media(&store, &parsed, bytes, "image/png").await?;
    assert_eq!(resolver.resolve(&url, bytes.len()).await?, bytes);
    assert!(matches!(
        resolver.resolve(&url, bytes.len() - 1).await,
        Err(AttachmentResolveError::TooLarge(BlobTooLarge { size_bytes, max_bytes }))
            if size_bytes == bytes.len() && max_bytes == bytes.len() - 1
    ));
    Ok(())
}

#[tokio::test]
async fn nats_resolver_classifies_invalid_and_missing_urls() -> anyhow::Result<()> {
    let Some((_server, jetstream, resolver)) = isolated_resolver().await? else {
        return Ok(());
    };
    for url in ["", " ", "cid:bad:value", "https://example.com"] {
        assert!(matches!(
            resolver.resolve(url, 1024).await,
            Err(AttachmentResolveError::InvalidUrl(_))
        ));
    }
    let media_url = format!("cid:media:_temp/abcdef/{}", "b".repeat(64));
    let urls = [media_url.as_str(), "cid:plan:_temp/abcdef/missing"];
    // Missing buckets and missing keys in existing buckets both mean NotFound.
    for url in urls {
        assert!(matches!(
            resolver.resolve(url, 1024).await,
            Err(AttachmentResolveError::NotFound(_))
        ));
    }
    ensure_attachments_bucket(&jetstream, 1).await?;
    harnx_blob_store::ensure_plans_bucket(&jetstream, 1).await?;
    for url in urls {
        assert!(matches!(
            resolver.resolve(url, 1024).await,
            Err(AttachmentResolveError::NotFound(_))
        ));
    }
    Ok(())
}

#[tokio::test]
async fn nats_resolver_returns_rendered_plan_markdown() -> anyhow::Result<()> {
    let Some((_server, jetstream, resolver)) = isolated_resolver().await? else {
        return Ok(());
    };
    let store = harnx_blob_store::ensure_plans_bucket(&jetstream, 1).await?;
    let url = CidUrl::parse("cid:plan:_temp/abcdef/test")?;
    let document = PlanDocument {
        front: PlanFrontMatter {
            id: url.to_string(),
            title: Some("Resolver test".to_string()),
            created_at: "2026-11-02T00:00:00Z".to_string(),
            ..Default::default()
        },
        body: "Exact Markdown: café\n".to_string(),
    };
    create_document(&store, &url, &serialize_plan(&document)?).await?;
    let expected = harnx_blob_store::resolve(&jetstream, &url).await?.bytes;
    assert_eq!(
        resolver.resolve(&url.to_string(), expected.len()).await?,
        expected
    );
    assert!(matches!(
        resolver.resolve(&url.to_string(), expected.len() - 1).await,
        Err(AttachmentResolveError::TooLarge(_))
    ));
    Ok(())
}
