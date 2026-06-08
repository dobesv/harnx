use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use http::{header::HeaderName, HeaderValue, Response, StatusCode};
use hudsucker::{
    certificate_authority::RcgenAuthority,
    hyper::{body::Bytes, Request},
    rcgen::Issuer,
    rustls::{
        client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        crypto::aws_lc_rs,
        pki_types::{CertificateDer, ServerName, UnixTime},
        ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme,
    },
    Body, HttpContext, HttpHandler, Proxy, RequestOrResponse,
};
use hyper_util::client::legacy::connect::HttpConnector;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;

use crate::ca::CaSetup;
use crate::filter::{self, CompiledFilter};

#[derive(Clone)]
struct AuthHandler {
    filter: Arc<CompiledFilter>,
    jaq_vars: Arc<crate::filter::JaqVars>,
    log_file: Option<PathBuf>,
}

impl HttpHandler for AuthHandler {
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        mut req: Request<Body>,
    ) -> RequestOrResponse {
        let req_json = request_json(&req);

        match filter::apply_filter_with_vars(&self.filter, req_json.clone(), &self.jaq_vars) {
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

                let changed_headers =
                    if let Some(headers) = result.get("headers").and_then(Value::as_object) {
                        replace_headers(req.headers_mut(), headers)
                    } else {
                        vec![]
                    };

                // Log every request when --log-file is specified.
                if let Some(log_path) = &self.log_file {
                    let auth_after = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let entry = serde_json::json!({
                        "host": result.get("host").and_then(Value::as_str).unwrap_or(""),
                        "method": result.get("method").and_then(Value::as_str).unwrap_or(""),
                        "path": result.get("path").and_then(Value::as_str).unwrap_or(""),
                        "auth": truncate_auth(auth_after),
                        "changed": changed_headers,
                    });
                    append_log(log_path, &entry).await;
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "request hook filter failed; passing through unchanged");
                // Still log the request even if the filter failed.
                if let Some(log_path) = &self.log_file {
                    let auth = req_json
                        .get("headers")
                        .and_then(|h| h.get("authorization"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let entry = serde_json::json!({
                        "host": req_json.get("host").and_then(Value::as_str).unwrap_or(""),
                        "method": req_json.get("method").and_then(Value::as_str).unwrap_or(""),
                        "path": req_json.get("path").and_then(Value::as_str).unwrap_or(""),
                        "auth": truncate_auth(auth),
                        "changed": ["filter-error"],
                    });
                    append_log(log_path, &entry).await;
                }
            }
        }

        req.into()
    }
}

/// Append one JSON log line per request to the log file. Errors are logged
/// but do not interrupt request handling.
async fn append_log(path: &std::path::Path, entry: &Value) {
    let mut line = entry.to_string();
    line.push('\n');
    let write = async {
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        f.write_all(line.as_bytes()).await?;
        anyhow::Ok(())
    };
    if let Err(err) = write.await {
        tracing::warn!(error = %err, "failed to write auth log line");
    }
}

/// Return a safe summary of an Authorization header value — never the full token.
fn truncate_auth(auth: &str) -> String {
    if auth.is_empty() {
        return "(none)".to_string();
    }
    if let Some(token) = auth.strip_prefix("Bearer ") {
        return format!("Bearer {}...", &token[..token.len().min(12)]);
    }
    if let Some(b64) = auth.strip_prefix("Basic ") {
        use base64::Engine as _;
        if let Some(decoded) = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
        {
            let (user, tok) = decoded.split_once(':').unwrap_or(("", &decoded));
            return format!("Basic {}:{}...", user, &tok[..tok.len().min(12)]);
        }
    }
    // Unknown scheme — show scheme only
    auth.split_whitespace()
        .next()
        .map(|s| format!("{s} (...)"))
        .unwrap_or_else(|| "(unknown)".to_string())
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
    // For tunnelled HTTPS requests (after CONNECT), the URI is path-only and
    // uri.host() returns "". Fall back to the Host header so jq filters can
    // match on the actual target hostname.
    let host = uri.host().unwrap_or_default();
    let host = if host.is_empty() {
        headers
            .get("host")
            .and_then(|v| v.as_str())
            .map(|h| h.split(':').next().unwrap_or(h))
            .unwrap_or_default()
    } else {
        host
    };
    serde_json::json!({
        "method": req.method().as_str(),
        "host": host,
        "path": uri.path_and_query().map_or("/", |value| value.as_str()),
        "headers": headers,
    })
}

/// Apply header patches from the filter output.
/// Returns a list of header names that were actually changed (added, updated, or removed).
fn replace_headers(headers: &mut http::HeaderMap, new_headers: &Map<String, Value>) -> Vec<String> {
    let mut changed = Vec::new();
    // Patch semantics: only touch keys present in new_headers.
    // null value → remove the header; string value → upsert; anything else → skip.
    // This preserves headers the filter didn't mention (e.g. Host, Content-Length).
    for (name, value) in new_headers {
        let Ok(name) = name.parse::<HeaderName>() else {
            continue;
        };
        if value.is_null() {
            if headers.remove(&name).is_some() {
                changed.push(name.as_str().to_string());
            }
        } else if let Some(s) = value.as_str() {
            if let Ok(v) = HeaderValue::from_str(s) {
                let old = headers
                    .get(&name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let new_str = s.to_owned();
                headers.insert(name.clone(), v);
                if old.as_deref() != Some(&new_str) {
                    changed.push(name.as_str().to_string());
                }
            }
        }
    }
    changed
}

pub async fn start_proxy(
    filter: CompiledFilter,
    ca: CaSetup,
    jaq_vars: Arc<crate::filter::JaqVars>,
) -> Result<u16> {
    start_proxy_inner(
        filter,
        ProxyConfig {
            ca,
            jaq_vars,
            log_file: None,
            danger_accept_invalid_certs: false,
        },
    )
    .await
}

pub async fn start_proxy_with_log(
    filter: CompiledFilter,
    ca: CaSetup,
    jaq_vars: Arc<crate::filter::JaqVars>,
    log_file: Option<PathBuf>,
) -> Result<u16> {
    start_proxy_inner(
        filter,
        ProxyConfig {
            ca,
            jaq_vars,
            log_file,
            danger_accept_invalid_certs: false,
        },
    )
    .await
}

/// Like [`start_proxy`] but skips TLS certificate verification for upstream
/// connections. Only for use in integration tests with self-signed server certs.
#[doc(hidden)]
pub async fn start_proxy_danger_accept_invalid_certs(
    filter: CompiledFilter,
    ca: CaSetup,
    jaq_vars: Arc<crate::filter::JaqVars>,
) -> Result<u16> {
    start_proxy_inner(
        filter,
        ProxyConfig {
            ca,
            jaq_vars,
            log_file: None,
            danger_accept_invalid_certs: true,
        },
    )
    .await
}

struct ProxyConfig {
    ca: CaSetup,
    jaq_vars: Arc<crate::filter::JaqVars>,
    log_file: Option<PathBuf>,
    danger_accept_invalid_certs: bool,
}

fn build_danger_https_connector() -> Result<hyper_rustls::HttpsConnector<HttpConnector>> {
    // Nested so the trait impl is not visible at module scope.
    // Used only when accepting self-signed upstream certs in integration tests.
    #[derive(Debug)]
    struct AcceptAny;
    impl ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, TlsError> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _msg: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, TlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _msg: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, TlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::ED25519,
            ]
        }
    }

    let tls_config = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    Ok(hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config)
        .https_or_http()
        .enable_http1()
        .wrap_connector(http))
}

async fn start_proxy_inner(filter: CompiledFilter, config: ProxyConfig) -> Result<u16> {
    let ProxyConfig {
        ca,
        jaq_vars,
        log_file,
        danger_accept_invalid_certs,
    } = config;
    let issuer = Issuer::from_ca_cert_pem(&ca.cert.pem(), ca.key_pair)?;
    let issuer = RcgenAuthority::new(issuer, 256, aws_lc_rs::default_provider());

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let handler = AuthHandler {
        filter: Arc::new(filter),
        jaq_vars,
        log_file,
    };

    if danger_accept_invalid_certs {
        let proxy = Proxy::builder()
            .with_listener(listener)
            .with_ca(issuer)
            .with_http_connector(build_danger_https_connector()?)
            .with_http_handler(handler)
            .build()?;
        tokio::spawn(async move {
            if let Err(err) = proxy.start().await {
                tracing::error!(error = %err, "Proxy error");
            }
        });
    } else {
        let proxy = Proxy::builder()
            .with_listener(listener)
            .with_ca(issuer)
            .with_rustls_connector(aws_lc_rs::default_provider())
            .with_http_handler(handler)
            .build()?;
        tokio::spawn(async move {
            if let Err(err) = proxy.start().await {
                tracing::error!(error = %err, "Proxy error");
            }
        });
    }

    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hudsucker::hyper::Request;

    fn make_request(uri: &str, host_header: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().uri(uri);
        if let Some(h) = host_header {
            builder = builder.header("host", h);
        }
        builder.body(Body::empty()).unwrap()
    }

    /// For tunnelled HTTPS (CONNECT), the inner request URI is path-only.
    /// request_json must fall back to the Host header so jq host filters work.
    #[test]
    fn request_json_uses_host_header_when_uri_has_no_host() {
        let req = make_request("/dobesv/harnx.git/info/refs", Some("github.com"));
        let json = request_json(&req);
        assert_eq!(json["host"], "github.com");
    }

    /// Port suffix in Host header should be stripped.
    #[test]
    fn request_json_strips_port_from_host_header() {
        let req = make_request("/", Some("github.com:443"));
        let json = request_json(&req);
        assert_eq!(json["host"], "github.com");
    }

    /// When URI has a host (plain HTTP or absolute-form), use it directly.
    #[test]
    fn request_json_uses_uri_host_when_present() {
        let req = make_request("http://api.github.com/user", None);
        let json = request_json(&req);
        assert_eq!(json["host"], "api.github.com");
    }
}
