use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use bytes::Bytes;
use harnx_blob_store::{
    media::{ensure_attachments_bucket, put_media},
    plans::{create_document, ensure_plans_bucket, serialize_plan, PlanDocument, PlanFrontMatter},
};
use harnx_core::cid_url::{CidUrl, PlanItem, SessionRef};
use harnx_runtime::config::ConfigLock;
use harnx_runtime::config::NatsRouting;
use http::{header, Method, StatusCode};
use http_body_util::{BodyExt, Full};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};

use crate::{
    cid::CID_CONTENT_SECURITY_POLICY, test_support::TestConfigSandbox, AppResponse, Server,
};

const IMAGE_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const HTML_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const MISSING_HASH: &str = "1111111111111111111111111111111111111111111111111111111111111111";

struct CidRouteFixture {
    _nats: harnx_test_bins::NatsServerHandle,
    _sandbox: TestConfigSandbox,
    server: Arc<Server>,
    image: CidUrl,
    html: CidUrl,
    plan: CidUrl,
    plan_revision: u64,
}

impl CidRouteFixture {
    async fn get(&self, url: &CidUrl, if_none_match: Option<&str>) -> Result<AppResponse> {
        let path = cid_path(url);
        let mut request = hyper::Request::builder().method(Method::GET).uri(path);
        if let Some(etag) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        self.server
            .get_cid_blob(request.body(Full::new(Bytes::new()))?)
            .await
    }
}

async fn cid_route_fixture() -> Result<Option<CidRouteFixture>> {
    let Some(nats) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(None);
    };
    let sandbox = TestConfigSandbox::new();
    sandbox.write_nats_server("cid-test", &format!("url: {}\n", nats.url()));
    let mut config = sandbox.config();
    config.nats_routing = NatsRouting::Cluster("cid-test".to_string());
    let jetstream = config.nats_jetstream("cid-test").await?;
    let session = SessionRef::new(Some("plain".to_string()), "cidtest".to_string())?;

    let image = media_url(&session, IMAGE_HASH);
    let html = media_url(&session, HTML_HASH);
    let media_store = ensure_attachments_bucket(&jetstream, 1).await?;
    put_media(&media_store, &image, b"png bytes", "image/png").await?;
    put_media(
        &media_store,
        &html,
        b"<script>alert(1)</script>",
        "text/html",
    )
    .await?;

    let plan = CidUrl::Plan {
        session,
        slug: "project-plan".to_string(),
        item: PlanItem::Index,
    };
    let plan_store = ensure_plans_bucket(&jetstream, 1).await?;
    let plan_document = PlanDocument {
        front: PlanFrontMatter {
            id: plan.to_string(),
            title: Some("Project Plan".to_string()),
            created_at: "2026-09-30T00:00:00Z".to_string(),
            ..PlanFrontMatter::default()
        },
        body: "Rendered plan body.".to_string(),
    };
    let plan_revision =
        create_document(&plan_store, &plan, &serialize_plan(&plan_document)?).await?;

    let config = Arc::new(ConfigLock::new(config));
    let server = Arc::new(Server::new(&config, PathBuf::from("web-assets")));
    Ok(Some(CidRouteFixture {
        _nats: nats,
        _sandbox: sandbox,
        server,
        image,
        html,
        plan,
        plan_revision,
    }))
}

fn media_url(session: &SessionRef, hash: &str) -> CidUrl {
    CidUrl::Media {
        session: session.clone(),
        hash: hash.to_string(),
    }
}

fn cid_path(url: &CidUrl) -> String {
    format!(
        "/v1/cid/{}",
        utf8_percent_encode(&url.to_string(), NON_ALPHANUMERIC)
    )
}

fn assert_security_headers(response: &AppResponse) {
    assert_eq!(response.headers()["X-Content-Type-Options"], "nosniff");
    assert_eq!(
        response.headers()["Content-Security-Policy"],
        CID_CONTENT_SECURITY_POLICY
    );
}

async fn response_body(response: AppResponse) -> Result<Bytes> {
    Ok(response.into_body().collect().await?.to_bytes())
}

#[tokio::test(flavor = "multi_thread")]
async fn get_cid_media_image_returns_inline_immutable_response() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = cid_route_fixture().await? else {
        return Ok(());
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let handle = Arc::clone(&fixture.server)
        .run(listener, crate::DEFAULT_DRAIN_TIMEOUT)
        .await?;

    let response = reqwest::get(format!("http://{address}{}", cid_path(&fixture.image))).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    assert_eq!(response.headers()[header::CONTENT_DISPOSITION], "inline");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        response.headers()[header::ETAG],
        format!("\"{IMAGE_HASH}\"")
    );
    assert_eq!(response.headers()["X-Content-Type-Options"], "nosniff");
    assert_eq!(
        response.headers()["Content-Security-Policy"],
        CID_CONTENT_SECURITY_POLICY
    );
    assert_eq!(response.bytes().await?.as_ref(), b"png bytes");
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn get_cid_plan_returns_rendered_markdown_with_revalidation_headers() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = cid_route_fixture().await? else {
        return Ok(());
    };

    let response = fixture.get(&fixture.plan, None).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/markdown; charset=utf-8"
    );
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    assert_eq!(
        response.headers()[header::ETAG],
        format!("\"{}\"", fixture.plan_revision)
    );
    assert_security_headers(&response);
    let markdown = String::from_utf8(response_body(response).await?.to_vec())?;
    assert!(markdown.starts_with("# Project Plan\n\n"));
    assert!(markdown.contains("Rendered plan body."));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn get_cid_plan_matching_etag_returns_not_modified() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = cid_route_fixture().await? else {
        return Ok(());
    };
    let etag = format!("\"{}\"", fixture.plan_revision);

    let response = fixture.get(&fixture.plan, Some(&etag)).await?;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
    assert_eq!(response.headers()[header::ETAG], etag);
    assert_security_headers(&response);
    assert!(response_body(response).await?.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn get_cid_html_forces_download() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = cid_route_fixture().await? else {
        return Ok(());
    };

    let response = fixture.get(&fixture.html, None).await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment"
    );
    assert_security_headers(&response);
    Ok(())
}

#[tokio::test]
async fn get_cid_malformed_url_returns_bad_request() -> Result<()> {
    let sandbox = TestConfigSandbox::new();
    let config = Arc::new(ConfigLock::new(sandbox.config()));
    let server = Server::new(&config, PathBuf::from("web-assets"));
    let request = hyper::Request::builder()
        .method(Method::GET)
        .uri("/v1/cid/not-a-cid")
        .body(Full::new(Bytes::new()))?;

    let response = server.get_cid_blob(request).await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_security_headers(&response);
    assert_eq!(
        response_body(response).await?.as_ref(),
        b"malformed cid URL"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn get_cid_missing_blob_returns_not_found() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = cid_route_fixture().await? else {
        return Ok(());
    };
    let missing = media_url(fixture.image.session(), MISSING_HASH);

    let response = fixture.get(&missing, None).await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_security_headers(&response);
    assert_eq!(
        response_body(response).await?.as_ref(),
        b"cid blob not found"
    );
    Ok(())
}

#[test]
fn cid_disposition_only_inlines_safe_images_and_plain_text() {
    // Safe raster images inline
    for mime in ["image/jpeg", "image/png", "image/gif", "image/webp"] {
        assert_eq!(crate::cid::cid_content_disposition(mime), "inline");
    }
    // text/plain inline (MIME params allowed)
    assert_eq!(
        crate::cid::cid_content_disposition("text/plain; charset=utf-8"),
        "inline"
    );
    // Everything else forced to download
    for mime in [
        "text/html",
        "image/svg+xml",
        "image/bmp", // non-standard raster
        "image/tiff",
        "image/avif",
        "application/pdf",
        "text/markdown",
    ] {
        assert_eq!(crate::cid::cid_content_disposition(mime), "attachment");
    }
}
