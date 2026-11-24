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

## Transport Notes

`harnx-mcp-remote` utilizes `rmcp`'s `StreamableHttpClientTransport`, which unifies support for both SSE and streamable HTTP.

- **Stateless Fallback**: By default, the proxy allows stateless HTTP sessions, matching the behavior of legacy SSE (2024-11) servers.
- **Strict Sessions**: Use the `--strict-session` flag to disable stateless fallback. This requires the remote server to support stateful MCP sessions (2025-03 spec).

## Native NATS Mode (per-user credentials)

Supplying `--credentials-key-file` switches the binary from stdio proxying
to a native harnx tool server with **per-user credentials**: each user
connects their own account through `<server>_connect`, and their upstream
tools appear only in their sessions. The stdio proxy mode is unchanged and
remains the default.

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
| `--oauth-client-id <ID>` | — | Pre-registered OAuth client; dynamic client registration is attempted when absent |
| `--oauth-client-secret-file <PATH>` | — | Secret file for a pre-registered confidential client |
| `--oauth-scope <SCOPE>` | — | OAuth scope to request; repeatable |
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

All command-line flags (except `--header`) have corresponding environment variables:

| Flag | Environment Variable | Description |
| :--- | :--- | :--- |
| `--url <URL>` | `MCP_REMOTE_URL` | **Required.** The URL of the remote MCP server. |
| `--bearer-token <TOKEN>` | `MCP_REMOTE_BEARER_TOKEN` | Token for the `Authorization: Bearer` header. HTTPS required unless loopback or `--insecure` is set. |
| `--insecure` | `MCP_REMOTE_INSECURE` | Allow bearer tokens over plaintext HTTP for non-loopback hosts. |
| `--header <NAME:VALUE>` | (None) | Repeatable flag for custom HTTP headers. |
| `--tls-cert <PATH>` | `MCP_REMOTE_TLS_CERT` | Path to the client's PEM certificate. |
| `--tls-key <PATH>` | `MCP_REMOTE_TLS_KEY` | Path to the client's PEM private key. |
| `--tls-ca <PATH>` | `MCP_REMOTE_TLS_CA` | Path to a custom CA certificate bundle (PEM). |
| `--strict-session` | `MCP_REMOTE_STRICT_SESSION` | Require stateful sessions; disable stateless fallback. |
