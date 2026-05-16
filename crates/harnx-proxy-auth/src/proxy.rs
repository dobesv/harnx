use std::sync::Arc;

use anyhow::Result;
use http::{header::HeaderName, HeaderValue, Response, StatusCode};
use hudsucker::{
    certificate_authority::RcgenAuthority,
    hyper::{body::Bytes, Request},
    rcgen::Issuer,
    rustls::crypto::aws_lc_rs,
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
};
use serde_json::{Map, Value};
use tokio::net::TcpListener;

use crate::ca::CaSetup;
use crate::filter::{self, CompiledFilter};

#[derive(Clone)]
struct AuthHandler {
    filter: Arc<CompiledFilter>,
}

impl HttpHandler for AuthHandler {
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        mut req: Request<Body>,
    ) -> RequestOrResponse {
        let req_json = request_json(&req);

        match filter::apply_filter(&self.filter, req_json) {
            Ok(result) => {
                // If the filter sets `.block` to a truthy value, return a 403 response
                // instead of forwarding the request.
                // - `true`          → generic "Blocked by proxy" message
                // - a string        → that string is used as the response body
                let block_reason = match result.get("block") {
                    Some(Value::Bool(true)) => Some("Blocked by proxy".to_string()),
                    Some(Value::String(reason)) if !reason.is_empty() => Some(reason.clone()),
                    _ => None,
                };
                if let Some(reason) = block_reason {
                    tracing::info!(reason = %reason, "request blocked by filter");
                    let response = Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .header("content-type", "text/plain")
                        .body(Body::from(Bytes::from(reason)))
                        .unwrap_or_else(|_| Response::new(Body::empty()));
                    return response.into();
                }

                if let Some(headers) = result.get("headers").and_then(Value::as_object) {
                    replace_headers(req.headers_mut(), headers);
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "request hook filter failed; passing through unchanged");
            }
        }

        req.into()
    }
}

fn request_json(req: &Request<Body>) -> Value {
    let headers = req
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value.to_str().ok().map(|value| {
                (
                    name.as_str().to_ascii_lowercase(),
                    Value::String(value.to_owned()),
                )
            })
        })
        .collect::<Map<_, _>>();

    let uri = req.uri();
    serde_json::json!({
        "method": req.method().as_str(),
        "host": uri.host().unwrap_or_default(),
        "path": uri.path_and_query().map_or("/", |value| value.as_str()),
        "headers": headers,
    })
}

fn replace_headers(headers: &mut http::HeaderMap, new_headers: &Map<String, Value>) {
    // Patch semantics: only touch keys present in new_headers.
    // null value → remove the header; string value → upsert; anything else → skip.
    // This preserves headers the filter didn't mention (e.g. Host, Content-Length).
    for (name, value) in new_headers {
        let Ok(name) = name.parse::<HeaderName>() else {
            continue;
        };
        if value.is_null() {
            headers.remove(&name);
        } else if let Some(s) = value.as_str() {
            if let Ok(v) = HeaderValue::from_str(s) {
                headers.insert(name, v);
            }
        }
    }
}

pub async fn start_proxy(filter: CompiledFilter, ca: CaSetup) -> Result<u16> {
    let issuer = Issuer::from_ca_cert_pem(&ca.cert.pem(), ca.key_pair)?;
    let issuer = RcgenAuthority::new(issuer, 256, aws_lc_rs::default_provider());

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let proxy = Proxy::builder()
        .with_listener(listener)
        .with_ca(issuer)
        .with_rustls_connector(aws_lc_rs::default_provider())
        .with_http_handler(AuthHandler {
            filter: Arc::new(filter),
        })
        .build()?;

    tokio::spawn(async move {
        if let Err(err) = proxy.start().await {
            tracing::error!(error = %err, "Proxy error");
        }
    });

    Ok(port)
}
