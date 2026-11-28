//! TLS material shared by upstream MCP and OAuth HTTP clients.

use std::path::Path;

use anyhow::{bail, Context, Result};

use super::NativeConfig;

/// TLS flag values (`--tls-cert`/`--tls-key`/`--tls-ca`) for one client.
pub(crate) struct TlsPaths<'a> {
    pub cert: Option<&'a Path>,
    pub key: Option<&'a Path>,
    pub ca: Option<&'a Path>,
}

/// A `reqwest` builder with the shared TLS identity/CA flags applied.
///
/// Every OAuth-facing client (upstream MCP and token endpoint alike) builds
/// from this so identity/CA configuration cannot drift between them.
pub(crate) fn client_builder_for(tls: &TlsPaths<'_>) -> Result<reqwest::ClientBuilder> {
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

pub(crate) fn http_client_builder(config: &NativeConfig) -> Result<reqwest::ClientBuilder> {
    client_builder_for(&TlsPaths {
        cert: config.tls_cert.as_deref(),
        key: config.tls_key.as_deref(),
        ca: config.tls_ca.as_deref(),
    })
}
