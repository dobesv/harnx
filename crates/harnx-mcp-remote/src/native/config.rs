//! Native-mode configuration and validation.

use anyhow::{bail, Context, Result};
use reqwest::header::HeaderName;
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use url::Url;

/// Which connect methods the page offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthMethods {
    pub oauth: bool,
    pub token: bool,
}

impl AuthMethods {
    fn parse(raw: &str) -> Result<Self> {
        let mut methods = Self {
            oauth: false,
            token: false,
        };
        for part in raw.split(',') {
            match part.trim().to_ascii_lowercase().as_str() {
                "oauth" => methods.oauth = true,
                "token" => methods.token = true,
                "" => {}
                other => {
                    bail!("unknown auth method '{other}' in --auth-methods (use oauth, token)")
                }
            }
        }
        if !methods.oauth && !methods.token {
            bail!("--auth-methods must enable at least one of oauth, token");
        }
        Ok(methods)
    }
}

/// Validated native-mode configuration (decision 10).
#[derive(Clone, Debug)]
pub struct NativeConfig {
    pub server_name: String,
    pub url: String,
    pub insecure: bool,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub tls_ca: Option<PathBuf>,
    pub strict_session: bool,
    pub auth_methods: AuthMethods,
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret_file: Option<PathBuf>,
    pub oauth_scopes: Vec<String>,
    pub credential_ttl: Duration,
    pub token_header: HeaderName,
    /// `None` sends the raw token (e.g. an integration key).
    pub token_scheme: Option<String>,
    pub public_base_url: Option<Url>,
    pub http_bind: SocketAddr,
    pub trusted_user_header: Option<HeaderName>,
    pub single_user: bool,
    pub credentials_key_file: PathBuf,
}

impl NativeConfig {
    pub fn from_cli(cli: &crate::cli::Cli) -> Result<Self> {
        let Some(credentials_key_file) = cli.credentials_key_file.clone() else {
            bail!("native mode requires --credentials-key-file");
        };
        let server_name = cli
            .name
            .clone()
            .context("--name is required in native mode (it names the <server>_connect tools)")?;
        if let Some(bearer) = cli.bearer_token.as_ref() {
            let _ = bearer;
            bail!("native mode does not accept --bearer-token: credentials are per user, never process-wide");
        }
        let token_header: HeaderName = cli
            .token_header
            .parse()
            .with_context(|| format!("invalid --token-header '{}'", cli.token_header))?;
        validate_static_headers(cli, &token_header)?;
        let auth_methods = AuthMethods::parse(&cli.auth_methods)?;
        let token_scheme = parse_token_scheme(&cli.token_scheme)?;
        let credential_ttl = parse_credential_ttl(&cli.credential_ttl)?;
        let http_bind: SocketAddr = cli
            .http_bind
            .parse()
            .with_context(|| format!("invalid --http-bind '{}'", cli.http_bind))?;
        validate_http_bind(cli, http_bind)?;
        let public_base_url = cli
            .public_base_url
            .as_deref()
            .map(parse_public_base_url)
            .transpose()?;
        let trusted_user_header = match &cli.trusted_user_header {
            Some(raw) => Some(
                raw.parse()
                    .with_context(|| format!("invalid --trusted-user-header '{raw}'"))?,
            ),
            None => None,
        };
        validate_upstream_url(cli)?;
        Ok(Self {
            server_name,
            url: cli.url.clone(),
            insecure: cli.insecure,
            tls_cert: cli.tls_cert.clone(),
            tls_key: cli.tls_key.clone(),
            tls_ca: cli.tls_ca.clone(),
            strict_session: cli.strict_session,
            auth_methods,
            oauth_client_id: cli.oauth_client_id.clone(),
            oauth_client_secret_file: cli.oauth_client_secret_file.clone(),
            oauth_scopes: cli.oauth_scope.clone(),
            credential_ttl,
            token_header,
            token_scheme,
            public_base_url,
            http_bind,
            trusted_user_header,
            single_user: cli.single_user,
            credentials_key_file,
        })
    }

    /// Whether a request identity matches the trusted-header requirement.
    pub fn trusted_header_matches(&self, headers: &axum::http::HeaderMap, user_id: &str) -> bool {
        match &self.trusted_user_header {
            Some(name) => headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value == user_id),
            None => true,
        }
    }
}

const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
const SECONDS_PER_WEEK: u64 = 7 * SECONDS_PER_DAY;

/// Parse a credential TTL like `365d`, `12h`, `30m`, `90s` or bare seconds.
pub fn parse_credential_ttl(raw: &str) -> Result<Duration> {
    let raw = raw.trim();
    let (digits, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    let value: u64 = digits
        .parse()
        .with_context(|| format!("invalid credential TTL '{raw}'"))?;
    let duration = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "s" => Duration::from_secs(value),
        "m" => Duration::from_secs(value * SECONDS_PER_MINUTE),
        "h" => Duration::from_secs(value * SECONDS_PER_HOUR),
        "d" => Duration::from_secs(value * SECONDS_PER_DAY),
        "w" => Duration::from_secs(value * SECONDS_PER_WEEK),
        other => bail!("invalid credential TTL unit '{other}' (use s, m, h, d or w)"),
    };
    anyhow::ensure!(!duration.is_zero(), "credential TTL must be positive");
    Ok(duration)
}

fn validate_static_headers(cli: &crate::cli::Cli, token_header: &HeaderName) -> Result<()> {
    for header in &cli.header {
        let name = header
            .split_once(':')
            .map(|(name, _)| name.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if name.is_empty() {
            bail!("invalid header '{header}': expected NAME:VALUE");
        }
        if name == "authorization" || name == token_header.as_str() {
            bail!(
                    "native mode rejects process-wide auth headers: remove the '{name}' --header entry; credentials are per user"
                );
        }
    }
    Ok(())
}

fn parse_token_scheme(raw: &str) -> Result<Option<String>> {
    let token_scheme = if raw.trim().is_empty() {
        None
    } else {
        let scheme = raw.trim().to_string();
        anyhow::ensure!(
            scheme.bytes().all(|b| b.is_ascii_graphic() && b != b' '),
            "invalid --token-scheme '{}'",
            raw
        );
        Some(scheme)
    };
    Ok(token_scheme)
}

fn validate_http_bind(cli: &crate::cli::Cli, http_bind: SocketAddr) -> Result<()> {
    if cli.single_user && cli.trusted_user_header.is_some() {
        bail!("--single-user cannot be combined with --trusted-user-header");
    }
    if cli.single_user && !http_bind.ip().is_loopback() {
        bail!(
                "--single-user serves one local session: --http-bind must be loopback (hosted deployments use per-user mode behind an authenticated ingress)"
            );
    }
    if !http_bind.ip().is_loopback() && cli.public_base_url.is_none() {
        bail!("a non-loopback --http-bind requires --public-base-url (the pages must sit behind an authenticated ingress)");
    }
    Ok(())
}

fn parse_public_base_url(raw: &str) -> Result<Url> {
    let url: Url = raw
        .parse()
        .with_context(|| format!("invalid --public-base-url '{raw}'"))?;
    let loopback_http = url.scheme() == "http"
        && matches!(
            url.host(),
            Some(url::Host::Domain(host))
                if host.eq_ignore_ascii_case("localhost")
        );
    anyhow::ensure!(
    url.scheme() == "https" || loopback_http,
    "--public-base-url must be https (or loopback http); '{raw}' cannot produce a valid connect link",
);
    Ok(url)
}

fn validate_upstream_url(cli: &crate::cli::Cli) -> Result<()> {
    // Per-user credentials must not travel over plaintext non-loopback.
    let upstream: Url = cli
        .url
        .parse()
        .with_context(|| format!("invalid remote MCP URL: {}", cli.url))?;
    let upstream_loopback = matches!(
        upstream.host(),
        Some(url::Host::Ipv4(ip)) if ip.is_loopback()
    ) || matches!(upstream.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback())
        || matches!(
            upstream.host(),
            Some(url::Host::Domain(host)) if host.eq_ignore_ascii_case("localhost")
        );
    if !cli.insecure && upstream.scheme() != "https" && !upstream_loopback {
        bail!(
                "refusing to send per-user credentials over non-HTTPS URL {}; use HTTPS, loopback, or --insecure",
                cli.url
            );
    }
    Ok(())
}
