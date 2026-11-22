//! TLS material shared by upstream MCP and OAuth HTTP clients.

use anyhow::{Context, Result};

use super::NativeConfig;

pub(crate) fn http_client_builder(config: &NativeConfig) -> Result<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder();
    match (&config.tls_cert, &config.tls_key) {
        (Some(cert_path), Some(key_path)) => {
            let mut identity_pem = std::fs::read(cert_path)
                .with_context(|| format!("read TLS cert PEM from {}", cert_path.display()))?;
            let key = std::fs::read(key_path)
                .with_context(|| format!("read TLS key PEM from {}", key_path.display()))?;
            identity_pem.extend_from_slice(&key);
            builder = builder.identity(reqwest::Identity::from_pem(&identity_pem)?);
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("--tls-cert and --tls-key must be provided together");
        }
        (None, None) => {}
    }
    if let Some(ca_path) = &config.tls_ca {
        let ca = std::fs::read(ca_path)
            .with_context(|| format!("read TLS CA PEM from {}", ca_path.display()))?;
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&ca)?);
    }
    Ok(builder)
}
