//! Shared NATS connect-options builder: token auth plus TLS/mTLS.
//!
//! `harnx-runtime` (config-file-defined clusters) and the standalone tool/hook
//! server binaries (environment-defined clusters) both need to turn the same
//! handful of settings into an [`async_nats::ConnectOptions`]. This module is
//! the one implementation, so a TLS fix lands once instead of twice.

use std::{
    io::BufReader,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use async_nats::{
    rustls::pki_types::{CertificateDer, PrivateKeyDer},
    ConnectOptions,
};

/// Env var enabling TLS for a standalone tool/hook server's broker connection.
/// `"1"` or `"true"` enables it; see [`NatsEndpoint::has_tls_settings`] for
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
/// Env var overriding whether to ignore the peer addresses a clustered broker
/// advertises in its INFO. `"1"` or `"true"` ignores them; see
/// [`NatsEndpoint::resolved_ignore_discovered_servers`] for the default, which
/// depends on the transport.
pub const HARNX_NATS_IGNORE_DISCOVERED_SERVERS_ENV: &str = "HARNX_NATS_IGNORE_DISCOVERED_SERVERS";
/// Env var carrying the JetStream replica count for buckets harnx creates.
///
/// Read by both [`NatsEndpoint::from_env`] (standalone tool/hook servers) and
/// `harnx-runtime`'s `resolve_local_nats_server_config` (worker-side
/// discovery) — the same name and the same parsing via [`parse_replicas_env`],
/// so a typo can't make one side see a replicated bucket while the other
/// silently downgrades to a single replica.
pub const HARNX_NATS_REPLICAS_ENV: &str = "HARNX_NATS_REPLICAS";

/// Read an optional boolean env var the way every harnx NATS flag spells it:
/// absent means "unset, use the default", and only `1`/`true` mean enabled.
pub fn parse_bool_env(name: &str) -> Option<bool> {
    std::env::var(name)
        .ok()
        .map(|value| value == "1" || value == "true")
}

/// Spell an optional boolean flag for the environment of a child process, in
/// the form [`parse_bool_env`] reads back. `None` stays unset, so the child
/// applies the same default this process would.
pub fn format_bool_env(flag: Option<bool>) -> Option<&'static str> {
    flag.map(|enabled| if enabled { "true" } else { "false" })
}

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
    /// Whether to ignore the peer addresses a clustered broker advertises in
    /// its INFO. `None` defers to the transport; see
    /// [`NatsEndpoint::resolved_ignore_discovered_servers`].
    pub ignore_discovered_servers: Option<bool>,
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

/// Which wire protocol the configured URL selects.
///
/// async-nats decides this from the URL scheme alone, and the two families
/// behave differently enough that several options below have to branch on it:
/// a WebSocket connection carries its TLS (or not) from the scheme, and never
/// runs the NATS protocol's own TLS upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// `nats://`, and anything without a scheme — async-nats assumes `nats://`
    /// when the URL has no `://`. TLS is possible here but not implied: the
    /// server can still demand an upgrade in its INFO.
    Tcp,
    /// `tls://` — the NATS TCP protocol, upgraded to TLS before anything else.
    TcpTls,
    /// `ws://` — a WebSocket with no TLS of its own.
    PlaintextWebSocket,
    /// `wss://` — a WebSocket inside TLS.
    SecureWebSocket,
}

impl Transport {
    /// Classify a configured URL. async-nats assumes `nats://` when the URL
    /// carries no scheme at all.
    pub fn of_url(url: &str) -> Self {
        match url.split_once("://") {
            Some(("tls", _)) => Transport::TcpTls,
            Some(("ws", _)) => Transport::PlaintextWebSocket,
            Some(("wss", _)) => Transport::SecureWebSocket,
            _ => Transport::Tcp,
        }
    }

    pub fn is_websocket(self) -> bool {
        matches!(
            self,
            Transport::PlaintextWebSocket | Transport::SecureWebSocket
        )
    }

    /// Whether the scheme alone settles that the connection is encrypted.
    pub fn implies_tls(self) -> bool {
        matches!(self, Transport::TcpTls | Transport::SecureWebSocket)
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
            tls: parse_bool_env(HARNX_NATS_TLS_ENV),
            tls_cert: std::env::var(HARNX_NATS_TLS_CERT_ENV).ok(),
            tls_key: std::env::var(HARNX_NATS_TLS_KEY_ENV).ok(),
            tls_ca: std::env::var(HARNX_NATS_TLS_CA_ENV).ok(),
            ignore_discovered_servers: parse_bool_env(HARNX_NATS_IGNORE_DISCOVERED_SERVERS_ENV),
        })
    }

    /// The JetStream replica count to actually use for buckets created on
    /// this endpoint: the configured value, or 1 when unset.
    pub fn resolved_replicas(&self) -> usize {
        self.replicas.unwrap_or(1)
    }

    /// Whether to ignore the peer addresses a clustered broker advertises in
    /// its INFO: the configured value, or a default that follows the
    /// transport.
    ///
    /// A clustered broker advertises its peers as raw `nats://<host>:4222`
    /// addresses, and async-nats appends those to the server pool — it rejects
    /// a mix of WebSocket and non-WebSocket servers only for the pool it is
    /// constructed with, not for what it discovers later. Behind a WebSocket
    /// entry point those addresses are reachable only from inside the cluster,
    /// so honouring them means a reconnect abandons the entry point (an
    /// mTLS-gated load balancer, typically) and dials the broker directly.
    /// Hence the default: ignore them on a WebSocket, honour them on TCP,
    /// where discovery is how a client finds the rest of the cluster.
    pub fn resolved_ignore_discovered_servers(&self) -> bool {
        self.ignore_discovered_servers
            .unwrap_or_else(|| self.transport().is_websocket())
    }

    /// How this endpoint's URL reaches the broker.
    pub fn transport(&self) -> Transport {
        Transport::of_url(&self.url)
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
        crate::recovery::retry_until(
            tokio::time::Instant::now() + crate::recovery::RECOVERY_TIMEOUT,
            || self.connect_once(),
            |error| {
                error
                    .downcast_ref::<async_nats::ConnectError>()
                    .is_some_and(|error| {
                        matches!(
                            error.kind(),
                            async_nats::ConnectErrorKind::Io
                                | async_nats::ConnectErrorKind::Dns
                                | async_nats::ConnectErrorKind::TimedOut
                        )
                    })
            },
        )
        .await
    }

    async fn connect_once(&self) -> Result<async_nats::Client> {
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
        // Every production role uses this policy. Preserve the Client across
        // outages so async-nats restores its Core subscriptions. Application
        // operations still need their own deadlines and replay semantics.
        let mut options = ConnectOptions::new()
            .max_reconnects(None)
            .ping_interval(Duration::from_secs(5))
            .connection_timeout(Duration::from_secs(2))
            .reconnect_delay_callback(|attempt| {
                Duration::from_millis((100_u64 << attempt.min(4)).min(1_000))
            });
        let transport = self.transport();
        options = self.apply_auth_options(options);
        options = self.apply_tls_options(options, transport)?;
        options = self.apply_transport_options(options, transport);

        Ok(options)
    }

    fn apply_auth_options(&self, mut options: ConnectOptions) -> ConnectOptions {
        if let Some(token) = &self.token {
            options = options.token(token.clone());
        }
        options
    }

    /// Apply everything TLS-related in one place.
    ///
    /// Whenever TLS is in play, harnx builds the rustls config itself rather
    /// than letting async-nats build one. async-nats calls
    /// `ClientConfig::builder()`, which resolves rustls' *process-default*
    /// `CryptoProvider` and panics when several provider features are compiled
    /// in and nothing installed a default — which is this workspace exactly.
    /// See `crates/harnx-runtime/tests/tls_client_config.rs`, which reproduces
    /// the panic from a crate whose dependency graph has that ambiguity.
    ///
    /// Building the config here is also what lets `tls_ca` and a client
    /// certificate be used together: `tls_client_config` replaces whatever
    /// config async-nats would have built from `add_client_certificate`, so
    /// the certificate has to go into the same config as the root store.
    fn apply_tls_options(
        &self,
        mut options: ConnectOptions,
        transport: Transport,
    ) -> Result<ConnectOptions> {
        self.reject_tls_settings_on_a_plaintext_websocket(transport)?;
        let client_certificate = self.client_certificate_paths()?;
        // A `nats://` URL with no TLS settings is left alone, so a server that
        // demands an upgrade in its INFO anyway still reaches async-nats'
        // builder and still panics. That is a misconfiguration — the fix is to
        // say `tls: true` — and covering it would mean loading the platform's
        // trust roots on every plaintext connection to the local broker.
        if self.has_tls_settings() || transport.implies_tls() {
            options = options
                .tls_client_config(self.build_tls_client_config(client_certificate.as_ref())?);
        }
        Ok(options)
    }

    /// A `ws://` connection is plaintext for its whole life: async-nats reads
    /// TLS off the scheme and skips the NATS protocol's upgrade entirely for
    /// WebSocket addresses, so `require_tls`, a client certificate and a
    /// custom CA all have nowhere to apply. Silently honouring the URL would
    /// hand an operator who wrote `tls: true` an unencrypted connection, so
    /// refuse the pairing and name the fix.
    fn reject_tls_settings_on_a_plaintext_websocket(&self, transport: Transport) -> Result<()> {
        if transport == Transport::PlaintextWebSocket && self.has_tls_settings() {
            bail!(
                "NATS cluster '{}' has a plaintext ws:// URL together with TLS settings \
                 (tls/tls_cert/tls_key/tls_ca), which a WebSocket connection never applies. \
                 Use wss:// for an encrypted WebSocket, or drop the TLS settings.",
                self.name
            );
        }
        Ok(())
    }

    /// Validate the client-certificate pair and return its paths, or `None`
    /// when this endpoint configures no client certificate at all.
    fn client_certificate_paths(&self) -> Result<Option<(PathBuf, PathBuf)>> {
        match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => Ok(Some((
                self.validate_tls_path(TlsField::Cert, cert)?,
                self.validate_tls_path(TlsField::Key, key)?,
            ))),
            (Some(_), None) => bail!(
                "NATS cluster '{}' sets tls_cert but missing tls_key",
                self.name
            ),
            (None, Some(_)) => bail!(
                "NATS cluster '{}' sets tls_key but missing tls_cert",
                self.name
            ),
            (None, None) => Ok(None),
        }
    }

    /// Build the root store for a configured `tls_ca`, or `None` when this
    /// endpoint trusts the platform's roots.
    ///
    /// The store holds the configured CA and nothing else. Pinning is the
    /// point of naming a CA, so the platform roots are deliberately not
    /// merged in.
    fn load_tls_ca_root_store(&self) -> Result<Option<async_nats::rustls::RootCertStore>> {
        let Some(ca_path) = &self.tls_ca else {
            return Ok(None);
        };
        let ca_path = self.validate_tls_path(TlsField::Ca, ca_path)?;
        let certs = self.read_pem_certificates(&ca_path, TlsField::Ca)?;
        let mut root_store = async_nats::rustls::RootCertStore::empty();
        let (added, ignored) = root_store.add_parsable_certificates(certs);
        if added == 0 {
            bail!(
                "Failed to parse tls_ca '{}' for NATS cluster '{}' (ignored {ignored} certs)",
                ca_path.display(),
                self.name
            );
        }
        Ok(Some(root_store))
    }

    /// Build the rustls config for this endpoint: the configured CA or the
    /// platform's roots, plus the client certificate when there is one.
    fn build_tls_client_config(
        &self,
        client_certificate: Option<&(PathBuf, PathBuf)>,
    ) -> Result<async_nats::rustls::ClientConfig> {
        let root_store = match self.load_tls_ca_root_store()? {
            Some(pinned) => pinned,
            None => self.load_platform_root_store()?,
        };
        // `builder_with_provider` names the provider instead of resolving the
        // process default, which is the resolution that panics here. `ring` is
        // the provider async-nats itself defaults to (its `ring` feature is
        // what we enable), so this matches the crypto backend the connection
        // would have used anyway.
        let builder = async_nats::rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            async_nats::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .context("build rustls ClientConfig with the ring CryptoProvider")?
        .with_root_certificates(root_store);

        let Some((cert_path, key_path)) = client_certificate else {
            return Ok(builder.with_no_client_auth());
        };
        let chain = self.read_pem_certificates(cert_path, TlsField::Cert)?;
        let key = self.read_pem_private_key(key_path)?;
        builder.with_client_auth_cert(chain, key).with_context(|| {
            format!(
                "NATS cluster '{}' could not use tls_cert '{}' with tls_key '{}'",
                self.name,
                cert_path.display(),
                key_path.display()
            )
        })
    }

    /// The platform's trust roots, for an endpoint that named no `tls_ca`.
    /// This is what async-nats would have loaded for the same connection.
    fn load_platform_root_store(&self) -> Result<async_nats::rustls::RootCertStore> {
        let loaded = rustls_native_certs::load_native_certs();
        if !loaded.errors.is_empty() {
            let errors = loaded
                .errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            bail!(
                "Failed to load platform certificates for NATS cluster '{}': {errors}",
                self.name
            );
        }
        let mut root_store = async_nats::rustls::RootCertStore::empty();
        let (added, ignored) = root_store.add_parsable_certificates(loaded.certs);
        if added == 0 {
            bail!(
                "No usable platform certificates for NATS cluster '{}' (ignored {ignored});                  set tls_ca to name a CA explicitly",
                self.name
            );
        }
        Ok(root_store)
    }

    fn read_pem_certificates(
        &self,
        path: &Path,
        field: TlsField,
    ) -> Result<Vec<CertificateDer<'static>>> {
        let file = std::fs::File::open(path).with_context(|| {
            format!(
                "Failed to read {} '{}' for NATS cluster '{}'",
                field.config_key(),
                path.display(),
                self.name
            )
        })?;
        rustls_pemfile::certs(&mut BufReader::new(file))
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| {
                format!(
                    "Failed to parse PEM certificates from {} '{}' for NATS cluster '{}'",
                    field.config_key(),
                    path.display(),
                    self.name
                )
            })
    }

    fn read_pem_private_key(&self, path: &Path) -> Result<PrivateKeyDer<'static>> {
        let file = std::fs::File::open(path).with_context(|| {
            format!(
                "Failed to read tls_key '{}' for NATS cluster '{}'",
                path.display(),
                self.name
            )
        })?;
        rustls_pemfile::private_key(&mut BufReader::new(file))
            .with_context(|| {
                format!(
                    "Failed to parse a private key from tls_key '{}' for NATS cluster '{}'",
                    path.display(),
                    self.name
                )
            })?
            .with_context(|| {
                format!(
                    "tls_key '{}' for NATS cluster '{}' contains no private key",
                    path.display(),
                    self.name
                )
            })
    }

    /// Options that depend on how the connection reaches the broker rather
    /// than on what it carries.
    fn apply_transport_options(
        &self,
        mut options: ConnectOptions,
        transport: Transport,
    ) -> ConnectOptions {
        if self.resolved_ignore_discovered_servers() {
            options = options.ignore_discovered_servers();
        }
        if transport.is_websocket() {
            // TLS is settled by the scheme here; `require_tls` only drives the
            // TCP protocol's upgrade, which never runs on a WebSocket.
            return options;
        }
        if self.has_tls_settings() {
            options = options.require_tls(true);
        }
        options
    }

    fn has_client_certificate(&self) -> bool {
        self.tls_cert.is_some() || self.tls_key.is_some()
    }

    /// Whether this endpoint carries any TLS configuration at all. On a TCP
    /// connection that means TLS is required; on a WebSocket it means the
    /// settings need a `wss://` URL to have any effect.
    fn has_tls_settings(&self) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// async-nats assumes `nats://` for a URL with no scheme, so an operator
    /// who wrote a bare `host:port` must land on the TCP path rather than on
    /// whatever `split_once` makes of the string.
    #[test]
    fn classifies_url_schemes() {
        assert_eq!(Transport::of_url("nats://localhost:4222"), Transport::Tcp);
        assert_eq!(Transport::of_url("localhost:4222"), Transport::Tcp);
        assert_eq!(Transport::of_url("tls://localhost:4222"), Transport::TcpTls);
        assert_eq!(
            Transport::of_url("ws://localhost:8080"),
            Transport::PlaintextWebSocket
        );
        assert_eq!(
            Transport::of_url("wss://nats.example.com:443"),
            Transport::SecureWebSocket
        );
    }

    /// `wss` must not be read as a `ws` prefix — the two differ by exactly
    /// whether the connection is encrypted.
    #[test]
    fn secure_and_plaintext_websockets_are_distinct() {
        assert!(Transport::PlaintextWebSocket.is_websocket());
        assert!(Transport::SecureWebSocket.is_websocket());
        assert!(!Transport::Tcp.is_websocket());
        assert!(!Transport::TcpTls.is_websocket());
        assert_ne!(
            Transport::of_url("wss://host"),
            Transport::of_url("ws://host")
        );
    }

    /// Which schemes settle encryption on their own. This drives whether harnx
    /// builds the rustls config itself, so a wrong answer here is a panic at
    /// connect time rather than a failed assertion.
    #[test]
    fn only_tls_bearing_schemes_imply_tls() {
        assert!(Transport::TcpTls.implies_tls());
        assert!(Transport::SecureWebSocket.implies_tls());
        assert!(!Transport::Tcp.implies_tls());
        assert!(!Transport::PlaintextWebSocket.implies_tls());
    }
}
