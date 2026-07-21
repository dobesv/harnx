# harnx-mcp-remote

`harnx-mcp-remote` is a high-performance stdio-to-HTTP proxy for the Model Context Protocol (MCP). It bridges the gap between MCP hosts that communicate via standard I/O (such as Claude Desktop or various LLM harnesses) and remote MCP servers hosted over HTTP.

## Overview

This binary acts as a transparent proxy, allowing you to treat a remote HTTP-based MCP server as if it were running locally via stdio. It handles all necessary transport negotiation and authentication, providing a seamless integration for tools and services that require a remote backend.

Key capabilities include:
- **Unified Transport Support**: Automatically handles both modern streamable HTTP (MCP 2025-03) and legacy SSE (MCP 2024-11) transports.
- **Flexible Authentication**: Support for Bearer tokens, custom HTTP headers, and mutual TLS (mTLS).
- **Environment-First Configuration**: All settings can be provided via CLI flags or environment variables for easy deployment.

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

## Environment Variables

All command-line flags (except `--header`) have corresponding environment variables:

| Flag | Environment Variable | Description |
| :--- | :--- | :--- |
| `--url <URL>` | `MCP_REMOTE_URL` | **Required.** The URL of the remote MCP server. |
| `--bearer-token <TOKEN>` | `MCP_REMOTE_BEARER_TOKEN` | Token for the `Authorization: Bearer` header. |
| `--header <NAME:VALUE>` | (None) | Repeatable flag for custom HTTP headers. |
| `--tls-cert <PATH>` | `MCP_REMOTE_TLS_CERT` | Path to the client's PEM certificate. |
| `--tls-key <PATH>` | `MCP_REMOTE_TLS_KEY` | Path to the client's PEM private key. |
| `--tls-ca <PATH>` | `MCP_REMOTE_TLS_CA` | Path to a custom CA certificate bundle (PEM). |
| `--strict-session` | `MCP_REMOTE_STRICT_SESSION` | Require stateful sessions; disable stateless fallback. |
