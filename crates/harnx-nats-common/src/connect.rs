//! Shared NATS connect-options builder: token auth plus TLS/mTLS.
//!
//! `harnx-runtime` (config-file-defined clusters) and the standalone tool/hook
//! server binaries (environment-defined clusters) both need to turn the same
//! handful of settings into an [`async_nats::ConnectOptions`]. This module is
//! the one implementation, so a TLS fix lands once instead of twice.

use std::{io::BufReader, path::PathBuf};

use anyhow::{bail, Context, Result};
use async_nats::ConnectOptions;

/// Connection details for one NATS endpoint: URL plus optional auth/TLS.
///
/// Built either from a `NatsServerConfig` (config-file clusters, see
/// `harnx-runtime`) or from `HARNX_NATS_*` environment variables (standalone
/// tool/hook servers, see [`NatsEndpoint::from_env`]).
#[derive(Debug, Clone, Default)]
pub struct NatsEndpoint {
    pub name: String,
    pub url: String,
    pub token: Option<String>,
    /// JetStream replica count for buckets created on this endpoint.
    /// `None` means 1 (single replica, no HA).
    pub replicas: Option<usize>,
    pub tls: Option<bool>,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub tls_ca: Option<String>,
}

/// A connected NATS client plus the JetStream replica count resolved for it.
///
/// `serve_with_shutdown` (harnx-toolset-server, harnx-hookset-server) already
/// takes 4 arguments; bundling the replica count with the client it applies
/// to avoids a fifth rather than growing past the repo's argument limit.
#[derive(Debug, Clone)]
pub struct NatsConnection {
    pub client: async_nats::Client,
    pub replicas: usize,
}

/// Which TLS setting is under discussion.
///
/// A domain type instead of a bare `&str` field name keeps
/// `validate_tls_path` and its callers from reading as string-heavy, and it
/// makes the closed set of valid fields explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsField {
    Cert,
    Key,
    Ca,
}

impl TlsField {
    fn config_key(self) -> &'static str {
        match self {
            TlsField::Cert => "tls_cert",
            TlsField::Key => "tls_key",
            TlsField::Ca => "tls_ca",
        }
    }
}

impl NatsEndpoint {
    /// Read connection details the way every standalone harnx NATS process
    /// receives them.
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("HARNX_NATS_URL").context("HARNX_NATS_URL is required")?;
        Ok(Self {
            name: "environment".to_string(),
            url,
            token: std::env::var("HARNX_NATS_TOKEN").ok(),
            replicas: std::env::var("HARNX_NATS_REPLICAS")
                .ok()
                .and_then(|value| value.parse().ok()),
            tls: std::env::var("HARNX_NATS_TLS")
                .ok()
                .map(|value| value == "1" || value == "true"),
            tls_cert: std::env::var("HARNX_NATS_TLS_CERT").ok(),
            tls_key: std::env::var("HARNX_NATS_TLS_KEY").ok(),
            tls_ca: std::env::var("HARNX_NATS_TLS_CA").ok(),
        })
    }

    /// The JetStream replica count to actually use for buckets created on
    /// this endpoint: the configured value, or 1 when unset.
    pub fn resolved_replicas(&self) -> usize {
        self.replicas.unwrap_or(1)
    }

    /// Connect using the options from [`Self::connect_options`].
    pub async fn connect(&self) -> Result<async_nats::Client> {
        self.connect_options()?
            .connect(&self.url)
            .await
            .with_context(|| format!("connect to NATS at {}", self.url))
    }

    /// Build the `ConnectOptions` for this endpoint: token auth, and TLS or
    /// mTLS if configured.
    pub fn connect_options(&self) -> Result<ConnectOptions> {
        let mut options = ConnectOptions::new();
        options = self.apply_auth_options(options);
        options = self.apply_tls_options(options)?;

        if self.should_require_tls() {
            options = options.require_tls(true);
        }

        Ok(options)
    }

    fn apply_auth_options(&self, mut options: ConnectOptions) -> ConnectOptions {
        if let Some(token) = &self.token {
            options = options.token(token.clone());
        }
        options
    }

    fn apply_tls_options(&self, mut options: ConnectOptions) -> Result<ConnectOptions> {
        self.reject_unsupported_tls_combo()?;
        options = self.apply_client_certificate_options(options)?;
        if let Some(tls_client) = self.build_custom_tls_client_config()? {
            options = options.tls_client_config(tls_client);
        }
        Ok(options)
    }

    fn reject_unsupported_tls_combo(&self) -> Result<()> {
        // A custom `tls_ca` builds a dedicated rustls ClientConfig (below) which
        // replaces any client certificate registered via `add_client_certificate`.
        // async-nats 0.42 has no way to attach both a custom root store AND a client
        // cert through its high-level options, so reject the combination explicitly
        // rather than silently dropping the client certificate (which would make
        // mTLS fail at handshake time with a confusing error).
        if self.has_client_certificate() && self.tls_ca.is_some() {
            bail!(
                "NATS cluster '{}' sets both tls_ca and a client certificate (tls_cert/tls_key); \
                 this combination is not supported (the custom CA would override the client cert). \
                 Use a server cert chained to a publicly-trusted CA for mTLS, or drop tls_ca.",
                self.name
            );
        }
        Ok(())
    }

    fn apply_client_certificate_options(
        &self,
        mut options: ConnectOptions,
    ) -> Result<ConnectOptions> {
        match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => {
                let cert_path = self.validate_tls_path(TlsField::Cert, cert)?;
                let key_path = self.validate_tls_path(TlsField::Key, key)?;
                options = options.add_client_certificate(cert_path, key_path);
                Ok(options)
            }
            (Some(_), None) => bail!(
                "NATS cluster '{}' sets tls_cert but missing tls_key",
                self.name
            ),
            (None, Some(_)) => bail!(
                "NATS cluster '{}' sets tls_key but missing tls_cert",
                self.name
            ),
            (None, None) => Ok(options),
        }
    }

    fn build_custom_tls_client_config(&self) -> Result<Option<async_nats::rustls::ClientConfig>> {
        let Some(ca_path) = &self.tls_ca else {
            return Ok(None);
        };

        let ca_path = self.validate_tls_path(TlsField::Ca, ca_path)?;
        let ca_file = std::fs::File::open(&ca_path).with_context(|| {
            format!(
                "Failed to read tls_ca '{}' for NATS cluster '{}'",
                ca_path.display(),
                self.name
            )
        })?;
        let certs = rustls_pemfile::certs(&mut BufReader::new(ca_file))
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| {
                format!(
                    "Failed to parse PEM certificates from tls_ca '{}' for NATS cluster '{}'",
                    ca_path.display(),
                    self.name
                )
            })?;
        let mut root_store = async_nats::rustls::RootCertStore::empty();
        let (added, ignored) = root_store.add_parsable_certificates(certs);
        if added == 0 {
            bail!(
                "Failed to parse tls_ca '{}' for NATS cluster '{}' (ignored {ignored} certs)",
                ca_path.display(),
                self.name
            );
        }
        let tls_client = async_nats::rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        Ok(Some(tls_client))
    }

    fn has_client_certificate(&self) -> bool {
        self.tls_cert.is_some() || self.tls_key.is_some()
    }

    fn should_require_tls(&self) -> bool {
        self.tls.unwrap_or(false) || self.has_client_certificate() || self.tls_ca.is_some()
    }

    fn validate_tls_path(&self, field: TlsField, value: &str) -> Result<PathBuf> {
        let path = PathBuf::from(value);
        if !path.exists() {
            bail!(
                "NATS cluster '{}' {} path '{}' does not exist",
                self.name,
                field.config_key(),
                path.display()
            );
        }
        if !path.is_file() {
            bail!(
                "NATS cluster '{}' {} path '{}' is not a file",
                self.name,
                field.config_key(),
                path.display()
            );
        }
        Ok(path)
    }
}
