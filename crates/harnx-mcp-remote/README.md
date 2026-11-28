# harnx-mcp-remote

`harnx-mcp-remote` is a high-performance stdio-to-HTTP proxy for the Model Context Protocol (MCP). It bridges the gap between MCP hosts that communicate via standard I/O (such as Claude Desktop or various LLM harnesses) and remote MCP servers hosted over HTTP.

## Overview

This binary acts as a stdio-to-HTTP proxy, allowing you to treat a remote HTTP-based MCP server as if it were running locally via stdio. It handles transport negotiation and authentication for forwarded MCP capabilities such as tools, prompts, and resources.

Key capabilities include:
- **Unified Transport Support**: Automatically handles both modern streamable HTTP (MCP 2025-03) and legacy SSE (MCP 2024-11) transports.
- **Flexible Authentication**: Support for Bearer tokens, custom HTTP headers, and mutual TLS (mTLS).
- **Environment-First Configuration**: Most settings can be provided via CLI flags or environment variables for easy deployment (`--header` remains CLI-only).

## Connection Lifecycle

The upstream connection opens lazily, on the first request that needs it. Legacy clients connect at the `initialize` handshake as before; MCP 2026-07-28 discover-flow clients (such as `harnx-mcp-bridge`) never send `initialize`, so their first `tools/list`, `tools/call`, `prompts/list`, or `resources/list` connects on demand. Concurrent first requests share one connection attempt, a failed attempt is retried on the next request, and shutdown cancels a connect that is still in progress. In this stdio proxy mode there is no deadline on the upstream handshake: if the upstream accepts the connection but never answers, requests block until shutdown. (Native mode below bounds its connects instead: a 30-second handshake budget and an 80-second credential-refresh budget, so a stalled upstream can't hold a per-user pool slot indefinitely.)

## Installation

To install `harnx-mcp-remote` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-mcp-remote
```

Alternatively, build it from the repository root:

```sh
cargo build -p harnx-mcp-remote --release
```

## Usage Examples

### Basic Proxying
Point the proxy to a remote URL. The proxy will automatically negotiate the best available transport.

```sh
harnx-mcp-remote --url https://mcp.example.com/api
```

### Authentication with Bearer Tokens
Secure your connection using a standard `Authorization: Bearer <token>` header.

```sh
harnx-mcp-remote --url https://mcp.example.com/api --bearer-token "secret-session-token"
```

### Custom Headers
Provide additional headers required by your infrastructure. This flag can be repeated multiple times.

```sh
harnx-mcp-remote \
  --url https://mcp.example.com/api \
  --header "X-Project-ID: internal-77" \
  --header "X-Client-Version: 1.2.0"
```

### Mutual TLS (mTLS)
For environments requiring client certificate authentication:

```sh
harnx-mcp-remote \
  --url https://mcp.example.com/api \
  --tls-cert /path/to/cert.pem \
  --tls-key /path/to/key.pem \
  --tls-ca /path/to/ca.pem
```

### OAuth client credentials (proxy mode)
For upstreams that authenticate machine clients with an OAuth 2.0
`client_credentials` grant, pass `--oauth-token-url` (or
`MCP_REMOTE_OAUTH_TOKEN_URL`) with `--oauth-client-id` and
`--oauth-client-secret-file`. This mode is opt-in: without `--oauth-token-url`
(and without `--credentials-key-file`), plain stdio proxy behavior is unchanged.

```sh
harnx-mcp-remote \
  --url https://mcp.example.com/api \
  --oauth-token-url https://sso.example.com/oauth2/token \
  --oauth-client-id svc-account \
  --oauth-client-secret-file /etc/harnx/upstream-client-secret
```

The proxy exchanges `grant_type=client_credentials` (plus any `--oauth-scope`
values, space-joined) for an access token and sends it as `Authorization: Bearer`
on upstream MCP requests. Client authentication is form-body only (RFC 6749
§4.4); token endpoints that require HTTP Basic client authentication are not
supported. Dynamic client registration (DCR), HTTP connect pages,
and per-user encrypted `harnx-credentials` NATS storage apply only to native
mode (`--credentials-key-file`): in client-credentials proxy mode, tokens are
cached in memory only (returned `refresh_token` fields are ignored), and the
client secret is read from `--oauth-client-secret-file` (trailing CR/LF
stripped; no environment variable is supported).

Validation and token acquisition are lazy: `--healthz-addr` can report ready as
soon as the stdio proxy starts listening, before OAuth flags, URLs, the secret
file, or the token endpoint are checked. Configuration/URL validation and
secret-file loading run on the first upstream MCP connect (before contacting
either endpoint), and the first token exchange runs on the first authenticated
upstream request. Each token exchange has a 30-second total deadline covering
response headers and body (1 MiB cap); upstream MCP streams keep no request
timeout.

If the token response includes `expires_in`, the proxy reuses the cached token
and refreshes lazily inside the 60-second pre-expiry window on the same upstream
session (`expires_in: 0` refreshes on the next independent request). If
`expires_in` is omitted, the cached token stays valid until the upstream returns
an identifiable HTTP `401`. `POST` and SSE `GET` requests replay at most once on
a positively identified HTTP `401`: the refresh step skips the token endpoint
when another caller already installed a replacement that has not actually
expired (the 60-second pre-expiry window applies only to proactive refresh),
and a replay is not sent if the credentials are invalidated again before
stamping. `DELETE` checks freshness proactively without replay, non-`401`
responses (including `403`) never replay, and statusless malformed
`WWW-Authenticate` challenges fail closed without retrying.

#### Client-credentials mode boundaries

- **Mode selection**: `--oauth-token-url` (or `MCP_REMOTE_OAUTH_TOKEN_URL`)
  selects this mode, requires `--oauth-client-id` and
  `--oauth-client-secret-file` (CLI flags only, no environment variables), and
  conflicts with `--bearer-token`, `MCP_REMOTE_BEARER_TOKEN`, custom
  `Authorization` `--header` values (any letter case), and native mode
  (`--credentials-key-file`). Conflicts and malformed `--header` entries are
  rejected during configuration with static diagnostics, before any network
  request.
- **Both endpoints must be safe**: the token URL and the MCP URL must each
  parse as an absolute `http://` or `https://` URL with a host and no embedded
  `user:pass@` credentials (rejected even with `--insecure`), and each must
  independently use `https://` or loopback (`localhost`, `127.0.0.1`, `[::1]`)
  unless `--insecure` is passed. `--insecure` waives only the endpoint HTTPS
  policy for plaintext `http://` endpoints; it never disables TLS certificate
  verification.
- **Hardened HTTP clients**: the token and MCP clients share the
  `--tls-ca`/`--tls-cert`/`--tls-key` identity, ignore system proxies, and
  never follow redirects — credentials cannot be re-sent to a redirect
  target. Custom headers other than `Authorization` still reach the MCP
  endpoint; the token endpoint sees only the grant form.
- **Safe failure surfaces**: upstream error text can echo the presented
  token (including a token retired mid-flight by rotation), so in this mode
  failed tool results (`is_error: true`), JSON-RPC errors, and
  connect/recovery failures replace untrusted upstream text with static
  diagnostics while preserving JSON-RPC error codes and the `is_error` flag.
  Token-endpoint failures emit only static text, the HTTP status when known,
  and at most one allowlisted standard OAuth error code (`invalid_request`,
  `invalid_client`, etc.) — never provider prose, `error_description`, bodies,
  or credentials. Successful tool output passes through unchanged.
- **Restricted stderr and telemetry (OAuth mode only)**: stderr logging is
  restricted to app-owned (`harnx*`) targets regardless of `HARNX_LOG_FILTER`
  or `HARNX_LOG_LEVEL`, and `rmcp`, `reqwest`, `hyper`, and `hyper_util` (exact
  targets and `::` descendants) are excluded from OTLP exports regardless of
  `RUST_LOG` — while keeping app-owned OTLP telemetry (and other modes'
  telemetry) active. The configured URLs are never logged.

## Transport Notes

`harnx-mcp-remote` utilizes `rmcp`'s `StreamableHttpClientTransport`, which unifies support for both SSE and streamable HTTP.

- **Stateless Fallback**: By default, the proxy allows stateless HTTP sessions, matching the behavior of legacy SSE (2024-11) servers.
- **Strict Sessions**: Use the `--strict-session` flag to disable stateless fallback. This requires the remote server to support stateful MCP sessions (2025-03 spec).

## Native NATS Mode (per-user credentials)

Supplying `--credentials-key-file` switches the binary from stdio proxying
to a native harnx tool server with **per-user credentials**: each user
connects their own account through `<server>_connect`, and their upstream
tools appear only in their sessions. Plain stdio proxy mode is unchanged and
remains the default when neither `--credentials-key-file` nor
`--oauth-token-url` is set. Dynamic client registration (DCR), HTTP connect
pages, and per-user encrypted `harnx-credentials` storage exist only in native
mode. Native mode registers each upstream tool's `outputSchema`
as a summary of its top-level properties in `harnx:output_schema`, so
`help <tool>` in a `bash` script lists the result fields.

```sh
harnx-mcp-remote \
  --url https://atlassian.com/atlassian-connect/.../mcp \
  --name jira \
  --credentials-key-file /etc/harnx/jira-credentials-key.json \
  --auth-methods oauth \
  --oauth-scope offline_access
```

Connect links are served from the tool server's own HTTP pages (loopback and
a random port by default; use `--public-base-url`/`--http-bind` behind an
ingress subpath). Secrets are entered only on those pages — never through
tool arguments or the CLI.

Native-mode flags:

| Flag | Environment Variable | Description |
| :--- | :--- | :--- |
| `--credentials-key-file <PATH>` | `HARNX_CREDENTIALS_KEY_FILE` | Enables native mode; encrypted credential key file |
| `--name <SERVER>` | `HARNX_MCP_REMOTE_NAME` | Required in native mode; server name used for `<server>_connect` tool names |
| `--auth-methods <LIST>` | `HARNX_MCP_REMOTE_AUTH_METHODS` | Connect methods offered: `oauth`, `token` (default `oauth,token`) |
| `--oauth-client-id <ID>` | — | OAuth client ID (optional in native mode, where DCR is attempted when absent; required in client-credentials proxy mode, CLI-only) |
| `--oauth-client-secret-file <PATH>` | — | OAuth client secret file (optional for a pre-registered confidential client in native mode; required in client-credentials proxy mode, CLI-only) |
| `--oauth-scope <SCOPE>` | — | OAuth scope to request; repeatable (CLI-only) |
| `--credential-ttl <DURATION>` | `HARNX_CREDENTIAL_TTL` | Default and maximum absolute credential lifetime (default `365d`) |
| `--token-header <NAME>` | — | Header carrying a pasted token upstream (default `Authorization`) |
| `--token-scheme <SCHEME>` | — | Scheme prepended to a pasted token (default `Bearer`; empty sends the raw token) |
| `--public-base-url <URL>` | `HARNX_MCP_REMOTE_PUBLIC_BASE_URL` | Public base URL of the connect pages; all links and the OAuth redirect URI derive from it |
| `--http-bind <ADDR>` | `HARNX_MCP_REMOTE_HTTP_BIND` | Bind address for the connect pages (default `127.0.0.1:0`) |
| `--trusted-user-header <NAME>` | — | Request header that must carry the connecting user's identity (hosted deployments) |
| `--single-user` | — | Serve only sessions without a harnx user identity (local use) |

In native mode `--bearer-token` and auth values in `--header` are rejected at
startup, so no process-wide credential can act as a fallback for another
user's calls.

Instances refresh user registrations every 30 seconds. Each instance also
adopts live registrations whose catalog and active credential have the same
connection ID, so a surviving instance keeps tools available after another
instance crashes. Read or write failures retain refresh work for the next tick;
one user's failure doesn't stop the other users' refreshes.

Registrations expire after 90 seconds without a successful refresh. A running
instance never recreates a missing registration during refresh, so a disconnect
can't be undone by a stale refresher. If every instance stops, or a NATS outage
lasts long enough for registrations to expire, restart an instance to republish
matching active catalogs without contacting the upstream.

See [`docs/tool-credentials.md`](../../docs/tool-credentials.md) for key
generation, local and hosted setup, and provider recipes.

## Environment Variables

Proxy command-line flags (except `--header`, `--oauth-client-id`, `--oauth-client-secret-file`, and `--oauth-scope`) have corresponding environment variables:

| Flag | Environment Variable | Description |
| :--- | :--- | :--- |
| `--url <URL>` | `MCP_REMOTE_URL` | **Required.** The URL of the remote MCP server. |
| `--bearer-token <TOKEN>` | `MCP_REMOTE_BEARER_TOKEN` | Token for the `Authorization: Bearer` header. HTTPS required unless loopback or `--insecure` is set. |
| `--insecure` | `MCP_REMOTE_INSECURE` | Allow credentials over plaintext HTTP for non-loopback endpoints (does not disable TLS certificate verification). |
| `--header <NAME:VALUE>` | (None) | Repeatable flag for custom HTTP headers. |
| `--tls-cert <PATH>` | `MCP_REMOTE_TLS_CERT` | Path to the client's PEM certificate. |
| `--tls-key <PATH>` | `MCP_REMOTE_TLS_KEY` | Path to the client's PEM private key. |
| `--tls-ca <PATH>` | `MCP_REMOTE_TLS_CA` | Path to a custom CA certificate bundle (PEM). |
| `--strict-session` | `MCP_REMOTE_STRICT_SESSION` | Require stateful sessions; disable stateless fallback. |
| `--oauth-token-url <URL>` | `MCP_REMOTE_OAUTH_TOKEN_URL` | OAuth 2.0 token endpoint; selects client-credentials proxy mode (see above). |
