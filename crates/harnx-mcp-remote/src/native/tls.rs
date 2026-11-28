use anyhow::Result;
use harnx_http_auth::tls::{client_builder_for, TlsPaths};

use super::NativeConfig;

pub(crate) fn http_client_builder(config: &NativeConfig) -> Result<reqwest::ClientBuilder> {
    client_builder_for(&TlsPaths {
        cert: config.tls_cert.as_deref(),
        key: config.tls_key.as_deref(),
        ca: config.tls_ca.as_deref(),
    })
}
