use anyhow::{bail, Context, Result};
use bytes::Bytes;
use harnx_core::cid_url::CidUrl;
use http::{Method, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Response};
use log::debug;

use crate::{nats_access::serve_nats_jetstream, AppResponse, Server};

pub(crate) const CID_CONTENT_SECURITY_POLICY: &str =
    "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; sandbox";

pub(crate) enum ResourceRoute {
    Cid,
    Agent,
}

impl ResourceRoute {
    pub(crate) fn metric_name(&self) -> &'static str {
        match self {
            Self::Cid => "/v1/cid/*",
            Self::Agent => "/v1/agents/*",
        }
    }
}

pub(crate) fn resource_route(path: &str) -> Option<ResourceRoute> {
    if path.starts_with("/v1/cid/") {
        return Some(ResourceRoute::Cid);
    }
    if path.starts_with("/v1/agents/") {
        return Some(ResourceRoute::Agent);
    }
    None
}

impl Server {
    pub(crate) async fn handle_resource_route(
        &self,
        req: hyper::Request<Incoming>,
        route: ResourceRoute,
    ) -> Result<AppResponse> {
        match route {
            ResourceRoute::Cid => self.get_cid_blob(req).await,
            ResourceRoute::Agent => self.handle_agent_tree(req).await,
        }
    }

    pub(crate) async fn get_cid_blob<B>(&self, req: hyper::Request<B>) -> Result<AppResponse> {
        if req.method() != Method::GET {
            bail!("Method Not Allowed");
        }
        let Some(url) = parse_cid_path(req.uri().path()) else {
            return cid_error_response(StatusCode::BAD_REQUEST, "malformed cid URL");
        };
        let jetstream =
            serve_nats_jetstream(&self.config, self.config.default_cluster_key()).await?;
        let resolved = match harnx_blob_store::resolve(&jetstream, &url).await {
            Ok(resolved) => resolved,
            Err(error) => {
                debug!("CID blob resolution failed for {url}: {error:#}");
                return cid_error_response(StatusCode::NOT_FOUND, "cid blob not found");
            }
        };
        cid_blob_response(req.headers(), resolved)
    }
}

fn parse_cid_path(path: &str) -> Option<CidUrl> {
    let encoded = path.strip_prefix("/v1/cid/")?;
    let decoded = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .ok()?;
    CidUrl::parse(&decoded).ok()
}

fn cid_blob_response(
    request_headers: &http::HeaderMap,
    resolved: harnx_blob_store::ResolvedBlob,
) -> Result<AppResponse> {
    let etag = resolved.etag.as_deref().map(|value| format!("\"{value}\""));
    let cache_control = if resolved.immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let not_modified = !resolved.immutable
        && etag
            .as_deref()
            .is_some_and(|etag| if_none_match(request_headers, etag));
    let status = if not_modified {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    };
    let body = if not_modified {
        Bytes::new()
    } else {
        Bytes::from(resolved.bytes)
    };
    let mut response = Response::builder()
        .status(status)
        .header("X-Content-Type-Options", "nosniff")
        .header("Content-Security-Policy", CID_CONTENT_SECURITY_POLICY)
        .header(http::header::CACHE_CONTROL, cache_control)
        .body(Full::new(body).boxed())?;
    if let Some(etag) = etag {
        response.headers_mut().insert(
            http::header::ETAG,
            http::HeaderValue::from_str(&etag).context("build CID ETag header")?,
        );
    }
    if !not_modified {
        let disposition = cid_content_disposition(&resolved.mime_type);
        response.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_str(&resolved.mime_type)
                .context("build CID Content-Type header")?,
        );
        response.headers_mut().insert(
            http::header::CONTENT_DISPOSITION,
            http::HeaderValue::from_static(disposition),
        );
    }
    Ok(response)
}

fn if_none_match(headers: &http::HeaderMap, etag: &str) -> bool {
    headers
        .get(http::header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|candidate| candidate.trim() == etag))
}

pub(crate) fn cid_content_disposition(mime_type: &str) -> &'static str {
    let media_type = mime_type.split(';').next().unwrap_or_default().trim();
    // Inline allowlist matches attachments.rs::is_inline_image_mime.
    // Raster images only; SVG is excluded because it can embed scripts.
    let safe_image = matches!(
        media_type.to_ascii_lowercase().as_str(),
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    );
    if media_type.eq_ignore_ascii_case("text/plain") || safe_image {
        "inline"
    } else {
        "attachment"
    }
}

fn cid_error_response(status: StatusCode, message: &str) -> Result<AppResponse> {
    let response = Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("X-Content-Type-Options", "nosniff")
        .header("Content-Security-Policy", CID_CONTENT_SECURITY_POLICY)
        .header(http::header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::copy_from_slice(message.as_bytes())).boxed())?;
    Ok(response)
}
