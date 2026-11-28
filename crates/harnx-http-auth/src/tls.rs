//! TLS material shared by resource clients and OAuth token clients.

use std::path::Path;

use anyhow::{bail, Context, Result};

/// TLS flag values (`--tls-cert`/`--tls-key`/`--tls-ca`) for one client.
pub struct TlsPaths<'a> {
    pub cert: Option<&'a Path>,
    pub key: Option<&'a Path>,
    pub ca: Option<&'a Path>,
}

/// A `reqwest` builder with the shared TLS identity/CA flags applied.
///
/// Call this from a blocking thread. It reads the TLS files synchronously.
/// This function does not set redirect, proxy, retry, or DNS policy.
pub fn client_builder_for(tls: &TlsPaths<'_>) -> Result<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder();
    match (tls.cert, tls.key) {
        (Some(cert_path), Some(key_path)) => {
            let mut identity_pem = std::fs::read(cert_path)
                .with_context(|| format!("read TLS cert PEM from {}", cert_path.display()))?;
            let key = std::fs::read(key_path)
                .with_context(|| format!("read TLS key PEM from {}", key_path.display()))?;
            identity_pem.extend_from_slice(&key);
            builder = builder.identity(reqwest::Identity::from_pem(&identity_pem)?);
        }
        (Some(_), None) | (None, Some(_)) => {
            bail!("--tls-cert and --tls-key must be provided together");
        }
        (None, None) => {}
    }
    if let Some(ca_path) = tls.ca {
        let ca = std::fs::read(ca_path)
            .with_context(|| format!("read TLS CA PEM from {}", ca_path.display()))?;
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&ca)?);
    }
    Ok(builder)
}

/// TLS setup for credentialed HTTP without redirects or system proxies.
/// Call from a blocking thread. The consumer still owns destination checks,
/// DNS/IP policy, retry policy and resource-client timeouts.
pub fn credentialed_client_builder(tls: &TlsPaths<'_>) -> Result<reqwest::ClientBuilder> {
    Ok(client_builder_for(tls)?
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none()))
}
