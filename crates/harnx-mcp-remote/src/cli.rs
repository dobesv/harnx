use std::path::PathBuf;

use clap::{ArgAction, Parser};

#[derive(Debug, Clone, Parser)]
#[command(name = "harnx-mcp-remote")]
#[command(about = "Stdio MCP proxy for remote HTTP MCP servers")]
pub struct Cli {
    #[arg(long, env = "MCP_REMOTE_URL", required = true)]
    pub url: String,

    #[arg(long, env = "MCP_REMOTE_BEARER_TOKEN", hide_env_values = true)]
    pub bearer_token: Option<String>,

    #[arg(
        long,
        env = "MCP_REMOTE_INSECURE",
        help = "Allow credentials over plaintext HTTP for non-loopback endpoints (does not disable TLS certificate verification)"
    )]
    pub insecure: bool,

    #[arg(long, action = ArgAction::Append)]
    pub header: Vec<String>,

    #[arg(long, env = "MCP_REMOTE_TLS_CERT")]
    pub tls_cert: Option<PathBuf>,

    #[arg(long, env = "MCP_REMOTE_TLS_KEY")]
    pub tls_key: Option<PathBuf>,

    #[arg(long, env = "MCP_REMOTE_TLS_CA")]
    pub tls_ca: Option<PathBuf>,

    #[arg(
        long,
        env = "MCP_REMOTE_STRICT_SESSION",
        help = "Require stateful remote MCP sessions and disable stateless fallback"
    )]
    pub strict_session: bool,

    /// Prometheus metrics endpoint address (e.g., "0.0.0.0:9090").
    /// Also reads from HARNX_METRICS_ADDR env.
    #[arg(long, env = "HARNX_METRICS_ADDR")]
    pub metrics_addr: Option<String>,

    /// Healthz readiness endpoint address (e.g., "0.0.0.0:8080").
    /// Also reads from HARNX_HEALTHZ_ADDR env.
    #[arg(long, env = "HARNX_HEALTHZ_ADDR")]
    pub healthz_addr: Option<String>,

    // ---- Native NATS mode (user-credentials plan, T5). Supplying
    // --credentials-key-file selects this mode; without it (and without
    // --oauth-token-url), plain stdio proxy behavior runs unchanged. ----
    /// Enables native NATS mode: encrypted per-user credentials, connect
    /// pages and per-user tool registrations.
    #[arg(long, env = "HARNX_CREDENTIALS_KEY_FILE")]
    pub credentials_key_file: Option<PathBuf>,

    /// Server name used for the registration and the `<server>_connect` tools.
    /// Also reads from HARNX_MCP_REMOTE_NAME.
    #[arg(long, env = "HARNX_MCP_REMOTE_NAME")]
    pub name: Option<String>,

    /// Connect methods offered on the page: comma-separated `oauth`, `token`.
    /// Also reads from HARNX_MCP_REMOTE_AUTH_METHODS.
    #[arg(
        long,
        env = "HARNX_MCP_REMOTE_AUTH_METHODS",
        default_value = "oauth,token"
    )]
    pub auth_methods: String,

    /// OAuth client ID (required in client-credentials proxy mode; optional in
    /// native mode, where dynamic client registration is attempted when omitted).
    #[arg(long)]
    pub oauth_client_id: Option<String>,

    /// File holding the OAuth client secret (required in client-credentials
    /// proxy mode; optional for a pre-registered confidential client in native
    /// mode). Trailing CR/LF is stripped; no environment variable is supported.
    #[arg(long)]
    pub oauth_client_secret_file: Option<PathBuf>,

    /// OAuth scope to request. Repeatable (sent space-joined in
    /// client-credentials proxy mode).
    #[arg(long, action = ArgAction::Append)]
    pub oauth_scope: Vec<String>,

    /// OAuth 2.0 token endpoint for client-credentials proxy authentication.
    /// Also reads from MCP_REMOTE_OAUTH_TOKEN_URL. Selects client-credentials
    /// mode for the stdio proxy; requires --oauth-client-id and
    /// --oauth-client-secret-file, and conflicts with --bearer-token, native
    /// mode (--credentials-key-file), and custom Authorization headers.
    #[arg(long, env = "MCP_REMOTE_OAUTH_TOKEN_URL", hide_env_values = true)]
    pub oauth_token_url: Option<String>,

    /// Maximum and default absolute credential lifetime (e.g. `365d`).
    /// Also reads from HARNX_CREDENTIAL_TTL.
    #[arg(long, env = "HARNX_CREDENTIAL_TTL", default_value = "365d")]
    pub credential_ttl: String,

    /// Header that carries a pasted token upstream (default `Authorization`).
    #[arg(long, default_value = "Authorization")]
    pub token_header: String,

    /// Scheme prepended to a pasted token (default `Bearer`); pass an empty
    /// value to send the raw token.
    #[arg(long, default_value = "Bearer")]
    pub token_scheme: String,

    /// Public base URL of the connect pages (default `http://127.0.0.1:{port}`).
    /// All links and the OAuth redirect URI derive from it.
    #[arg(long, env = "HARNX_MCP_REMOTE_PUBLIC_BASE_URL")]
    pub public_base_url: Option<String>,

    /// Bind address for the connect HTTP pages (default loopback, random port).
    #[arg(
        long,
        env = "HARNX_MCP_REMOTE_HTTP_BIND",
        default_value = "127.0.0.1:0"
    )]
    pub http_bind: String,

    /// Trusted request header that must carry the connecting user's identity.
    #[arg(long)]
    pub trusted_user_header: Option<String>,

    /// Serve only sessions without a harnx user identity (local use); stores
    /// the one credential under the ownerless principal.
    #[arg(long)]
    pub single_user: bool,
}
