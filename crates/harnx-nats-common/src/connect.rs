//! Shared NATS connect-options builder: token auth plus TLS/mTLS.
//!
//! `harnx-runtime` (config-file-defined clusters) and the standalone tool/hook
//! server binaries (environment-defined clusters) both need to turn the same
//! handful of settings into an [`async_nats::ConnectOptions`]. This module is
//! the one implementation, so a TLS fix lands once instead of twice.

use std::{io::BufReader, path::PathBuf};

use anyhow::{bail, Context, Result};
use async_nats::ConnectOptions;

/// Env var enabling TLS for a standalone tool/hook server's broker connection.
/// `"1"` or `"true"` enables it; see [`NatsEndpoint::should_require_tls`] for
/// how this combines with the client-certificate/CA settings below.
///
/// Shared here (rather than declared separately per crate) because
/// `harnx-runtime`'s worker-side discovery (`resolve_local_nats_server_config`)
/// and the standalone tool/hook server binaries (`NatsEndpoint::from_env`)
/// both need the exact same env var names — a worker that reads a differently
/// spelled variable than the child it spawns would silently never see TLS.
pub const HARNX_NATS_TLS_ENV: &str = "HARNX_NATS_TLS";
/// Env var carrying the client certificate path for mTLS.
pub const HARNX_NATS_TLS_CERT_ENV: &str = "HARNX_NATS_TLS_CERT";
/// Env var carrying the client key path for mTLS.
pub const HARNX_NATS_TLS_KEY_ENV: &str = "HARNX_NATS_TLS_KEY";
/// Env var carrying a custom CA bundle path for TLS.
pub const HARNX_NATS_TLS_CA_ENV: &str = "HARNX_NATS_TLS_CA";
/// Env var carrying the JetStream replica count for buckets harnx creates.
///
/// Read by both [`NatsEndpoint::from_env`] (standalone tool/hook servers) and
/// `harnx-runtime`'s `resolve_local_nats_server_config` (worker-side
/// discovery) — the same name and the same parsing via [`parse_replicas_env`],
/// so a typo can't make one side see a replicated bucket while the other
/// silently downgrades to a single replica.
pub const HARNX_NATS_REPLICAS_ENV: &str = "HARNX_NATS_REPLICAS";

/// Parse [`HARNX_NATS_REPLICAS_ENV`]: `Ok(None)` when unset (callers default
/// to 1 replica), `Ok(Some(n))` when it parses, `Err` when it's set to
/// something that isn't a valid replica count.
///
/// Unset is not an error — most deployments never set this. Set-but-invalid
/// must be, because silently falling back to `None`/1 here is exactly the
/// silent single-replica downgrade an operator who set this variable is
/// trying to avoid.
pub fn parse_replicas_env() -> Result<Option<usize>> {
    match std::env::var(HARNX_NATS_REPLICAS_ENV) {
        Ok(value) => value.parse::<usize>().map(Some).with_context(|| {
            format!("{HARNX_NATS_REPLICAS_ENV}={value:?} is not a valid replica count")
        }),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("{HARNX_NATS_REPLICAS_ENV} is not valid unicode"))
        }
    }
}

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
        let replicas = parse_replicas_env()?;
        Ok(Self {
            name: "environment".to_string(),
            url,
            token: std::env::var("HARNX_NATS_TOKEN").ok(),
            replicas,
            tls: std::env::var(HARNX_NATS_TLS_ENV)
                .ok()
                .map(|value| value == "1" || value == "true"),
            tls_cert: std::env::var(HARNX_NATS_TLS_CERT_ENV).ok(),
            tls_key: std::env::var(HARNX_NATS_TLS_KEY_ENV).ok(),
            tls_ca: std::env::var(HARNX_NATS_TLS_CA_ENV).ok(),
        })
    }

    /// The JetStream replica count to actually use for buckets created on
    /// this endpoint: the configured value, or 1 when unset.
    pub fn resolved_replicas(&self) -> usize {
        self.replicas.unwrap_or(1)
    }

    /// Connect using the options from [`Self::connect_options`].
    ///
    /// `connect_options()` itself stays synchronous (see its doc comment and
    /// `connect_options.rs`'s tests, which call it directly), so a
    /// `tls_ca`-configured endpoint's CA-file read runs here instead, inside
    /// `spawn_blocking`. `NatsEndpoint` is a handful of cheap `String`/
    /// `Option<String>` fields, so cloning it to satisfy `spawn_blocking`'s
    /// `'static` bound doesn't cost anything worth avoiding.
    pub async fn connect(&self) -> Result<async_nats::Client> {
        let endpoint = self.clone();
        let options = tokio::task::spawn_blocking(move || endpoint.connect_options())
            .await
            .context("connect_options task panicked")??;
        options
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
        // `ClientConfig::builder()` resolves rustls' *process-default* crypto
        // provider, which panics ("cannot be resolved using a combination of
        // the crate features and process default") whenever more than one
        // provider feature is compiled in and nothing has called
        // `CryptoProvider::install_default`. That ambiguity is real here: the
        // workspace pulls in both `ring` (via async-nats/tokio-rustls) and
        // `aws-lc-rs` (via the AWS SDK's hyper-rustls stack), and no
        // production binary installs a default. `builder_with_provider` picks
        // the provider explicitly instead of depending on whatever else
        // happens to be linked into the process, so it can't be broken by an
        // unrelated dependency bump. `ring` is the provider async-nats itself
        // defaults to (its `ring` feature is what we enable), so this matches
        // the crypto backend already used for connections that don't set a
        // custom `tls_ca`.
        let tls_client = async_nats::rustls::ClientConfig::builder_with_provider(
            std::sync::Arc::new(async_nats::rustls::crypto::ring::default_provider()),
        )
        .with_safe_default_protocol_versions()
        .context("build rustls ClientConfig with the ring CryptoProvider")?
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
