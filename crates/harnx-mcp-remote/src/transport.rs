use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use std::sync::Arc;
use url::Url;

use crate::authenticated_http::AuthenticatedHttp;
use crate::cli::Cli;
use crate::client_credentials::ClientCredentialsProvider;
use crate::UpstreamAuth;
use harnx_http_auth::{
    net::{has_embedded_credentials, is_http_endpoint, is_https_or_loopback},
    tls::{credentialed_client_builder, TlsPaths},
};

fn config_from_cli(cli: &Cli) -> Result<StreamableHttpClientTransportConfig> {
    let parsed_url = Url::parse(cli.url.as_str())
        .with_context(|| format!("invalid remote MCP URL: {}", cli.url))?;
    ensure_bearer_transport_is_secure(&parsed_url, cli.bearer_token.is_some(), cli.insecure)?;

    let mut config = StreamableHttpClientTransportConfig::with_uri(cli.url.as_str());
    config.auth_header = cli.bearer_token.clone();
    config.custom_headers = parse_custom_headers(&cli.header)?;

    if cli.strict_session {
        config.allow_stateless = false;
    }

    Ok(config)
}

fn ensure_bearer_transport_is_secure(url: &Url, has_bearer: bool, insecure: bool) -> Result<()> {
    if bearer_transport_is_secure(url, has_bearer, insecure) {
        return Ok(());
    }

    anyhow::bail!(
        "refusing to send bearer token over non-HTTPS URL {}; use HTTPS, loopback, or --insecure",
        url
    );
}

/// A bearer token may be sent when there is no token, when the caller opts out
/// of the check, or when the endpoint is already safe (HTTPS or loopback).
fn bearer_transport_is_secure(url: &Url, has_bearer: bool, insecure: bool) -> bool {
    let bearer_check_waived = !has_bearer || insecure;
    bearer_check_waived || endpoint_is_safe(url)
}

fn endpoint_is_safe(url: &Url) -> bool {
    uses_https(url) || is_loopback_url(url)
}

fn uses_https(url: &Url) -> bool {
    url.scheme() == "https"
}

fn is_loopback_url(url: &Url) -> bool {
    url.host_str().is_some_and(is_loopback_host)
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn parse_custom_headers(
    headers: &[String],
) -> Result<std::collections::HashMap<HeaderName, HeaderValue>> {
    let mut parsed = std::collections::HashMap::new();

    for header in headers {
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid header '{header}': expected NAME:VALUE"))?;
        let header_name: HeaderName = name.trim().parse()?;
        let header_value = HeaderValue::from_str(value.trim())?;
        parsed.insert(header_name, header_value);
    }

    Ok(parsed)
}

pub async fn build_transport(cli: &Cli) -> Result<StreamableHttpClientTransport<reqwest::Client>> {
    let mut client_builder = reqwest::Client::builder();

    match (&cli.tls_cert, &cli.tls_key) {
        (Some(cert_path), Some(key_path)) => {
            let cert = tokio::fs::read(cert_path).await.with_context(|| {
                format!("failed to read TLS cert PEM from {}", cert_path.display())
            })?;
            let key = tokio::fs::read(key_path).await.with_context(|| {
                format!("failed to read TLS key PEM from {}", key_path.display())
            })?;
            let mut identity_pem = cert;
            identity_pem.extend_from_slice(&key);
            let identity = reqwest::Identity::from_pem(&identity_pem)?;
            client_builder = client_builder.identity(identity);
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(anyhow!(
                "--tls-cert and --tls-key must be provided together"
            ));
        }
        (None, None) => {}
    }

    if let Some(ca_path) = &cli.tls_ca {
        let ca = tokio::fs::read(ca_path)
            .await
            .with_context(|| format!("failed to read TLS CA PEM from {}", ca_path.display()))?;
        let certificate = reqwest::Certificate::from_pem(&ca)?;
        client_builder = client_builder.add_root_certificate(certificate);
    }

    let client = client_builder.build()?;
    let config = config_from_cli(cli)?;

    Ok(StreamableHttpClientTransport::with_client(client, config))
}

// ---- Client-credentials proxy mode (T3) ----
//
// Selecting `--oauth-token-url` switches `RemoteProxyServer::connect` to a
// `StreamableHttpClientTransport<AuthenticatedHttp>` whose `UpstreamAuth` is
// the T2 client-credentials provider. Every check below runs before either
// endpoint is contacted or any HTTP client is constructed, and no error
// message contains the configured URLs: they may carry embedded credentials
// and must never reach logs or stderr.

/// Which connect branch `RemoteProxyServer::connect` serves through: the
/// unchanged plain proxy transport, or the auth-adapter transport behind the
/// client-credentials provider.
pub(crate) enum UpstreamTransport {
    Plain(StreamableHttpClientTransport<reqwest::Client>),
    Authenticated(StreamableHttpClientTransport<AuthenticatedHttp>),
}

/// Validated client-credentials endpoints. Both URLs are checked
/// independently — HTTP(S) with a host, no embedded userinfo, then HTTPS or
/// loopback or `--insecure` — and the token endpoint is returned for the
/// provider. Both parsed URLs are returned so every later stage (the
/// transport config included) uses the validated values.
fn validated_oauth_endpoints(cli: &Cli) -> Result<(Url, Url)> {
    let Some(raw_token) = cli.oauth_token_url.as_deref() else {
        bail!("client-credentials mode requires --oauth-token-url");
    };
    let token = Url::parse(raw_token)
        .map_err(|_| anyhow!("invalid OAuth token URL: expected an absolute http(s) URL"))?;
    let mcp = Url::parse(cli.url.as_str()).map_err(|_| anyhow!("invalid remote MCP URL"))?;
    require_http_endpoint(&token, "token URL")?;
    require_http_endpoint(&mcp, "remote MCP URL")?;
    reject_embedded_credentials(&token, "token URL")?;
    reject_embedded_credentials(&mcp, "remote MCP URL")?;
    ensure_credentials_transport_is_secure(&token, "token endpoint", cli.insecure)?;
    ensure_credentials_transport_is_secure(&mcp, "remote MCP endpoint", cli.insecure)?;
    Ok((token, mcp))
}

/// Require an absolute http(s) URL with a host on both endpoints. rmcp
/// accepts other schemes until request time, so a parseable
/// `file://localhost/mcp` or `ftp://localhost/mcp` MCP URL would survive the
/// loopback check and the first authenticated request would send the client
/// secret to the token endpoint before the transport rejects the MCP URL.
/// The scheme and host gate runs before anything that can contact either
/// endpoint, with or without `--insecure`.
fn require_http_endpoint(url: &Url, what: &'static str) -> Result<()> {
    anyhow::ensure!(
        is_http_endpoint(url),
        "the {what} must be an absolute http(s) URL with a host"
    );
    Ok(())
}

/// Reject nonempty userinfo (username or password). reqwest can turn URL
/// userinfo into a Basic Authorization header, so a safe scheme alone is not
/// enough to clear an embedded credential.
fn reject_embedded_credentials(url: &Url, what: &'static str) -> Result<()> {
    if has_embedded_credentials(url) {
        bail!("the {what} must not contain embedded user credentials (user:pass@...)");
    }
    Ok(())
}

fn ensure_credentials_transport_is_secure(
    url: &Url,
    what: &'static str,
    insecure: bool,
) -> Result<()> {
    if insecure || is_https_or_loopback(url) {
        return Ok(());
    }
    bail!(
        "refusing to send OAuth client credentials over the non-HTTPS {what}; \
         use HTTPS, loopback, or --insecure"
    );
}

/// Static rejection for malformed `--header` entries in client-credentials
/// mode. The shared plain-mode parser echoes the raw argument in its error,
/// and a `--header` value can carry a credential, so the OAuth path rejects
/// with fixed text before the parser runs.
const MALFORMED_HEADER_REJECTION: &str =
    "invalid --header entry in client-credentials mode: expected NAME:VALUE with a valid HTTP header name and value";

/// Configuration conflicts that select between process-wide auth sources.
/// The client-credentials provider owns the `Authorization` header, so no
/// other credential may be configured alongside it.
fn validate_client_credentials_conflicts(cli: &Cli) -> Result<()> {
    if cli.bearer_token.is_some() {
        bail!(
            "--oauth-token-url cannot be combined with --bearer-token \
             (or MCP_REMOTE_BEARER_TOKEN): the client-credentials provider \
             owns the Authorization header"
        );
    }
    for header in &cli.header {
        // A no-colon entry such as `Authorization Bearer <value>` cannot be
        // identified by name, but it must not fall through to the shared
        // parser: that error interpolates the raw argument.
        let Some((name, value)) = header.split_once(':') else {
            bail!(MALFORMED_HEADER_REJECTION);
        };
        if name.trim().eq_ignore_ascii_case("authorization") {
            bail!(
                "--oauth-token-url cannot be combined with a custom Authorization \
                 --header: the client-credentials provider owns the Authorization header"
            );
        }
        if name.trim().parse::<HeaderName>().is_err()
            || HeaderValue::from_str(value.trim()).is_err()
        {
            bail!(MALFORMED_HEADER_REJECTION);
        }
    }
    Ok(())
}

/// The nonempty client id and secret-file path the token provider needs.
/// Returning the values keeps the caller free of `expect("checked above")`
/// panics: everything below this function is already validated.
fn required_client_credentials(cli: &Cli) -> Result<(String, std::path::PathBuf)> {
    let client_id = cli
        .oauth_client_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("--oauth-token-url requires --oauth-client-id"))?;
    let secret_file = cli
        .oauth_client_secret_file
        .as_deref()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("--oauth-token-url requires --oauth-client-secret-file"))?;
    Ok((client_id.to_owned(), secret_file.to_owned()))
}

/// Owned TLS flag values, moved into the blocking-pool task that loads the
/// cert/key/CA PEM and the client secret: those reads are synchronous
/// `std::fs` I/O (in harnx-http-auth), and this builder runs on a
/// Tokio worker.
struct OwnedTlsPaths {
    cert: Option<std::path::PathBuf>,
    key: Option<std::path::PathBuf>,
    ca: Option<std::path::PathBuf>,
}

/// One hardened HTTP client for OAuth mode: shared TLS identity/CA, no
/// proxies, redirects refused. No request timeout: the token exchange owns
/// its own 30-second deadline and MCP streams must stay unbounded.
fn oauth_http_client(tls: &OwnedTlsPaths) -> Result<reqwest::Client> {
    credentialed_client_builder(&TlsPaths {
        cert: tls.cert.as_deref(),
        key: tls.key.as_deref(),
        ca: tls.ca.as_deref(),
    })?
    .build()
    .context("build OAuth HTTP client")
}

/// Build the client-credentials proxy transport. Validation happens first, so
/// an invalid configuration contacts neither endpoint; unrelated custom
/// headers are preserved for the upstream MCP requests only.
pub(crate) async fn build_client_credentials_transport(
    cli: &Cli,
) -> Result<StreamableHttpClientTransport<AuthenticatedHttp>> {
    validate_client_credentials_conflicts(cli)?;
    let (token_endpoint, mcp_endpoint) = validated_oauth_endpoints(cli)?;
    let (client_id, secret_file) = required_client_credentials(cli)?;
    let config = oauth_transport_config(cli, &mcp_endpoint)?;

    // The blocking task must own its data, so the TLS flag values and the
    // scopes move out of one owned copy of the CLI instead of being cloned
    // field by field out of the borrowed one.
    let Cli {
        tls_cert,
        tls_key,
        tls_ca,
        oauth_scope,
        ..
    } = Cli::clone(cli);
    let tls = OwnedTlsPaths {
        cert: tls_cert,
        key: tls_key,
        ca: tls_ca,
    };
    // Everything below reads files (TLS PEM, client secret) through
    // synchronous std::fs, so it runs on the blocking pool instead of the
    // Tokio worker. Every check above is pure, so rejection still happens
    // before either endpoint is contacted.
    let transport = tokio::task::spawn_blocking(
        move || -> Result<StreamableHttpClientTransport<AuthenticatedHttp>> {
            // Both clients are constructed only after every URL check passed.
            let mcp_http = oauth_http_client(&tls)?;
            let token_http = oauth_http_client(&tls)?;

            // First token exchange stays lazy: this only reads the secret file.
            let provider = ClientCredentialsProvider::new(
                token_endpoint,
                client_id,
                &secret_file,
                oauth_scope,
                token_http,
            )?;
            let auth: Arc<dyn UpstreamAuth> = Arc::new(provider);
            Ok(StreamableHttpClientTransport::with_client(
                AuthenticatedHttp {
                    http: mcp_http,
                    auth,
                },
                config,
            ))
        },
    )
    .await
    .context("build client-credentials transport")??;
    Ok(transport)
}

/// The upstream transport configuration for OAuth mode: no fixed bearer (the
/// provider stamps Authorization per request), unrelated custom headers
/// preserved, and `--strict-session` honored. The MCP URL is the same
/// validated endpoint that passed every preflight check above — never the
/// raw CLI string.
fn oauth_transport_config(cli: &Cli, mcp: &Url) -> Result<StreamableHttpClientTransportConfig> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(mcp.as_str());
    config.auth_header = None;
    config.custom_headers = parse_custom_headers(&cli.header)?;
    if cli.strict_session {
        config.allow_stateless = false;
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use reqwest::header::{HeaderName, HeaderValue};
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

    use crate::cli::Cli;

    fn config_from_args(args: &[&str]) -> StreamableHttpClientTransportConfig {
        let cli = Cli::parse_from(args);
        super::config_from_cli(&cli).expect("valid transport config")
    }

    /// Table-driven coverage of the cases where a bearer token is accepted and
    /// forwarded verbatim as the auth header: HTTPS, loopback HTTP, and
    /// non-loopback HTTP with `--insecure`.
    #[test]
    fn bearer_token_is_forwarded_for_allowed_endpoints() {
        let cases: &[(&str, &[&str])] = &[
            ("https endpoint", &["--url", "https://example.com"]),
            ("loopback http", &["--url", "http://127.0.0.1:8000"]),
            (
                "insecure http",
                &["--url", "http://example.com", "--insecure"],
            ),
        ];

        for (label, url_args) in cases {
            let mut args = vec!["harnx-mcp-remote"];
            args.extend_from_slice(url_args);
            args.extend_from_slice(&["--bearer-token", "secret"]);
            let config = config_from_args(&args);
            assert_eq!(
                config.auth_header.as_deref(),
                Some("secret"),
                "auth header should be forwarded for {label}"
            );
        }
    }

    #[test]
    fn custom_header_populates_custom_headers() {
        let config = config_from_args(&[
            "harnx-mcp-remote",
            "--url",
            "https://example.com",
            "--header",
            "Foo:bar",
        ]);
        assert_eq!(
            config
                .custom_headers
                .get(&"foo".parse::<HeaderName>().expect("header name")),
            Some(&HeaderValue::from_static("bar"))
        );
    }

    #[test]
    fn stateless_allowed_by_default() {
        let config = config_from_args(&["harnx-mcp-remote", "--url", "https://example.com"]);
        assert!(config.allow_stateless);
    }

    #[test]
    fn strict_session_disables_stateless_mode() {
        let config = config_from_args(&[
            "harnx-mcp-remote",
            "--url",
            "https://example.com",
            "--strict-session",
        ]);
        assert!(!config.allow_stateless);
    }

    #[test]
    fn bearer_token_over_http_non_loopback_is_rejected() {
        let cli = Cli::parse_from([
            "harnx-mcp-remote",
            "--url",
            "http://example.com",
            "--bearer-token",
            "secret",
        ]);
        let err = super::config_from_cli(&cli).expect_err("plaintext bearer token should fail");
        assert!(err.to_string().contains("refusing to send bearer token"));
    }

    #[test]
    fn missing_url_is_parse_error() {
        let err = Cli::try_parse_from(["harnx-mcp-remote"]).expect_err("missing url should fail");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    // ---- Client-credentials endpoint policy (B10, S2, B4) ----
    //
    // Pure production-boundary checks: `build_client_credentials_transport`
    // validates and constructs without contacting either endpoint, so these
    // cases need no child processes.

    /// Never-contacted loopback endpoints, safe for every unit case.
    const IDLE_MCP_URL: &str = "http://127.0.0.1:9/mcp";
    const IDLE_TOKEN_URL: &str = "http://127.0.0.1:9/token";

    /// Writes a client-secret file and returns (tempdir, path); the tempdir
    /// guard keeps the file alive for the duration of the test. Async write:
    /// these tests run on a Tokio runtime, and AGENTS.md requires all I/O on
    /// it to go through Tokio.
    async fn secret_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("client-secret");
        tokio::fs::write(&path, "unit-test-secret\n")
            .await
            .expect("write secret");
        (dir, path)
    }

    /// Client-credentials argv with explicit endpoints, so each case varies
    /// one URL without a duplicate `--url`/`--oauth-token-url` argument.
    fn oauth_cli(mcp_url: &str, token_url: &str, extra: &[&str]) -> Cli {
        let mut args = vec![
            "harnx-mcp-remote",
            "--url",
            mcp_url,
            "--oauth-token-url",
            token_url,
            "--oauth-client-id",
            "unit-id",
        ];
        args.extend(extra);
        Cli::parse_from(args)
    }

    /// Runs the production builder expecting rejection. The transport type
    /// is not `Debug`, so `expect_err` is not usable here.
    async fn rejected_build(cli: &Cli, what: &str) -> anyhow::Error {
        super::build_client_credentials_transport(cli)
            .await
            .err()
            .unwrap_or_else(|| panic!("{what}"))
    }

    /// `--insecure` waives the HTTPS requirement independently per endpoint:
    /// either side may stay plaintext non-loopback while the other is safe.
    #[tokio::test]
    async fn insecure_waives_https_for_the_token_endpoint_only() {
        let (dir, secret) = secret_file().await;
        let cli = oauth_cli(
            IDLE_MCP_URL,
            // Token endpoint plaintext non-loopback, MCP endpoint loopback.
            "http://example.invalid/token",
            &[
                "--oauth-client-secret-file",
                &secret.display().to_string(),
                "--insecure",
            ],
        );
        super::build_client_credentials_transport(&cli)
            .await
            .expect("token-endpoint waiver must allow the build");
        drop(dir);
    }

    #[tokio::test]
    async fn insecure_waives_https_for_the_mcp_endpoint_only() {
        let (dir, secret) = secret_file().await;
        let cli = oauth_cli(
            // MCP endpoint plaintext non-loopback, token endpoint loopback.
            "http://example.invalid/mcp",
            IDLE_TOKEN_URL,
            &[
                "--oauth-client-secret-file",
                &secret.display().to_string(),
                "--insecure",
            ],
        );
        super::build_client_credentials_transport(&cli)
            .await
            .expect("mcp-endpoint waiver must allow the build");
        drop(dir);
    }

    /// Without `--insecure`, the same waiver cases are rejected, so the
    /// builds succeeding above really are the waiver and not a missing check.
    #[tokio::test]
    async fn plaintext_endpoints_without_insecure_are_rejected() {
        for (mcp_url, token_url) in [
            (IDLE_MCP_URL, "http://example.invalid/token"),
            ("http://example.invalid/mcp", IDLE_TOKEN_URL),
        ] {
            let cli = oauth_cli(mcp_url, token_url, &["--oauth-client-secret-file", "/x"]);
            let err = rejected_build(&cli, "plaintext must be rejected without --insecure").await;
            assert!(
                err.to_string()
                    .contains("use HTTPS, loopback, or --insecure"),
                "{err}"
            );
        }
    }

    /// Non-HTTP schemes that parse and carry a loopback host are rejected
    /// before anything can contact an endpoint, with and without `--insecure`
    /// (B4).
    #[tokio::test]
    async fn non_http_schemes_are_rejected_with_or_without_insecure() {
        for insecure in [false, true] {
            let mut extra = vec!["--oauth-client-secret-file", "/x"];
            if insecure {
                extra.push("--insecure");
            }
            // Invalid MCP URL scheme.
            let cli = oauth_cli("file://localhost/mcp", IDLE_TOKEN_URL, &extra);
            let err = rejected_build(&cli, "file:// MCP URL must be rejected").await;
            assert!(
                err.to_string()
                    .contains("remote MCP URL must be an absolute http(s) URL"),
                "{err}"
            );
            // Invalid token URL scheme.
            let cli = oauth_cli(IDLE_MCP_URL, "ftp://localhost/token", &extra);
            let err = rejected_build(&cli, "ftp:// token URL must be rejected").await;
            assert!(
                err.to_string()
                    .contains("token URL must be an absolute http(s) URL"),
                "{err}"
            );
        }
    }

    /// OAuth loopback policy recognizes the typed IPv6 loopback `[::1]`, which
    /// the plain `host_str` check never matches (S2).
    #[tokio::test]
    async fn oauth_loopback_ipv6_endpoints_are_accepted() {
        let (dir, secret) = secret_file().await;
        let secret_arg = secret.display().to_string();
        for (mcp_url, token_url) in [
            ("http://[::1]:9/mcp", IDLE_TOKEN_URL),
            (IDLE_MCP_URL, "http://[::1]:9/token"),
        ] {
            let cli = oauth_cli(
                mcp_url,
                token_url,
                &["--oauth-client-secret-file", &secret_arg],
            );
            super::build_client_credentials_transport(&cli)
                .await
                .expect("IPv6 loopback must satisfy the OAuth endpoint policy");
        }
        drop(dir);
    }

    /// The plain proxy's legacy bearer check keeps its `host_str` behavior and
    /// still rejects IPv6 loopback: the S2 fix is scoped to OAuth policy.
    #[test]
    fn plain_bearer_over_ipv6_loopback_stays_rejected() {
        let cli = Cli::parse_from([
            "harnx-mcp-remote",
            "--url",
            "http://[::1]:8000",
            "--bearer-token",
            "secret",
        ]);
        let err = super::config_from_cli(&cli).expect_err("plain legacy policy must not widen");
        assert!(
            err.to_string().contains("refusing to send bearer token"),
            "{err}"
        );
    }

    /// `--strict-session` reaches the OAuth transport config the same way it
    /// reaches the plain one, and stateless stays allowed without it (B10).
    #[test]
    fn oauth_strict_session_disables_stateless_mode() {
        let mcp: url::Url = IDLE_MCP_URL.parse().expect("loopback URL");
        let strict = super::oauth_transport_config(
            &oauth_cli(IDLE_MCP_URL, IDLE_TOKEN_URL, &["--strict-session"]),
            &mcp,
        )
        .expect("oauth transport config");
        assert!(!strict.allow_stateless);
        let default =
            super::oauth_transport_config(&oauth_cli(IDLE_MCP_URL, IDLE_TOKEN_URL, &[]), &mcp)
                .expect("oauth transport config");
        assert!(default.allow_stateless);
    }

    /// Malformed `--header` entries are rejected with static text that never
    /// echoes the raw argument, before the shared plain-mode parser runs
    /// (B3).
    #[tokio::test]
    async fn malformed_oauth_header_is_rejected_without_echoing_it() {
        for header in [
            "Authorization Bearer malformed-header-sentinel-771",
            "no-colon-here",
            "Bad Name: value",
            "X-Safe: bad\nvalue",
        ] {
            let (dir, secret) = secret_file().await;
            let cli = oauth_cli(
                IDLE_MCP_URL,
                IDLE_TOKEN_URL,
                &[
                    "--oauth-client-secret-file",
                    &secret.display().to_string(),
                    "--header",
                    header,
                ],
            );
            let err = rejected_build(&cli, "malformed --header entry must be rejected").await;
            let message = err.to_string();
            assert!(
                message.contains("invalid --header entry in client-credentials mode"),
                "{message}"
            );
            assert!(
                !message.contains("malformed-header-sentinel-771")
                    && !message.contains("no-colon-here")
                    && !message.contains("bad\nvalue"),
                "raw --header argument leaked: {message}"
            );
            drop(dir);
        }
    }

    /// The secret-file read keeps its static errors after moving to the
    /// blocking pool: neither a missing file nor an empty one puts a path,
    /// URL, or secret content into the rejection message.
    #[tokio::test]
    async fn secret_file_failures_are_static_after_the_blocking_offload() {
        let cli = oauth_cli(
            IDLE_MCP_URL,
            IDLE_TOKEN_URL,
            &[
                "--oauth-client-secret-file",
                "/definitely/missing/unit/secret",
            ],
        );
        let err = rejected_build(&cli, "missing secret file must reject the build").await;
        assert_eq!(
            err.to_string(),
            "cannot read --oauth-client-secret-file",
            "{err}"
        );

        let dir = tempfile::TempDir::new().expect("temp dir");
        let secret = dir.path().join("client-secret");
        tokio::fs::write(&secret, "\r\n")
            .await
            .expect("write empty secret");
        let cli = oauth_cli(
            IDLE_MCP_URL,
            IDLE_TOKEN_URL,
            &["--oauth-client-secret-file", &secret.display().to_string()],
        );
        let err = rejected_build(&cli, "empty secret file must reject the build").await;
        assert_eq!(
            err.to_string(),
            "--oauth-client-secret-file is empty",
            "{err}"
        );
    }
}
