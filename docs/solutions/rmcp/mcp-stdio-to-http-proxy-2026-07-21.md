---
title: "MCP stdio-to-HTTP proxy using rmcp StreamableHttpClientTransport"
date: 2026-07-21
category: "rmcp"
problem_type: integration_issue
component: "rmcp-transport"
root_cause: "non-obvious rmcp defaults and API behaviors when building HTTP MCP proxies"
resolution_type: code_fix
severity: high
tags:
  - mcp
  - rmcp
  - proxy
  - http
  - streamable-http
  - sse
  - graceful-shutdown
  - mtls
  - auth
plan_ref: "harnx-mcp-remote"
---

# MCP stdio-to-HTTP proxy using rmcp StreamableHttpClientTransport

## Problem

Building a stdio-to-HTTP MCP proxy that transparently bridges an LLM harness (stdio) to a remote HTTP MCP server requires handling rmcp's unified transport, preserving default behaviors, avoiding auth header bugs, wiring graceful shutdown correctly, and reflecting remote capabilities accurately.

## Symptoms

- **SSE servers disconnect immediately**: Proxy works against streamable HTTP servers but fails against stateless SSE (2024-11) servers with `connection closed: initialize response` — no meaningful error
- **401 on valid bearer tokens**: Auth header set as `Bearer <token>` produces HTTP 401; server logs show `Authorization: Bearer Bearer <token>`
- **SIGTERM exits 143**: Process exits with code 143 in idle/pre-initialize state instead of graceful exit 0
- **Process hangs after cancel**: Service cancellation returns but process never exits (blocked in runtime drop)
- **Capability mismatch**: Host cannot call `list_prompts`/`list_resources` despite proxy implementing them

## Investigation Steps

1. Traced `allow_stateless` default through rmcp source: `StreamableHttpClientTransportConfig::default()` sets `true`; proxy overwrites with `clap` bool defaulting to `false`
2. Probed auth header handling: rmcp's reqwest transport calls `.bearer_auth(auth_header)` which prepends `"Bearer "`; setting header to full `"Bearer <token>"` doubles it
3. Instrumented SIGTERM handling: probe showed signal handler not installed before `serve()`; rmcp `serve()` blocks on `expect_next_message("initialize request")` — never reaches handler registration in idle state
4. Tested runtime shutdown: `#[tokio::main]` drop hangs on blocking stdio read thread; explicit runtime + `shutdown_timeout(1s)` exits cleanly
5. Verified capability reflection: `Peer::peer_info()` returns cached `ServerInfo` from handshake — safe to read in `initialize` after `serve_client` completes (no deadlock)

## Root Cause

1. **`allow_stateless` default overwritten**: rmcp 2.2.0 defaults `allow_stateless = true` for SSE compatibility; clap bool default `false` breaks stateless servers
2. **Double Bearer**: rmcp's transport prepends `"Bearer "`; setting full header value doubles it
3. **Pre-initialize SIGTERM**: rmcp `serve()` blocks until initialize; signal handler installed after cannot fire in idle state
4. **Blocking stdio thread**: `#[tokio::main]` runtime drop waits on blocking tasks
5. **Static capabilities**: Hardcoded capabilities exclude methods the proxy actually forwards

## Solution

### 1. StreamableHttpClientTransport unifies SSE + streamable HTTP

One rmcp 2.2.0 type handles both MCP 2024-11 SSE and 2025-03 streamable HTTP. Enable workspace feature:

```toml
# Cargo.toml root
rmcp = { version = "0.2", features = [
    "client",
    "server",
    "transport-streamable-http-client-reqwest",  # ADD THIS
] }
```

Build transport with config:

```rust
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};

let mut config = StreamableHttpClientTransportConfig::with_uri(&url);
config.auth_header = bearer_token;  // RAW token, no "Bearer " prefix
config.custom_headers = headers;    // HashMap<HeaderName, HeaderValue>
// allow_stateless defaults to true — preserve it for SSE compat

let transport = StreamableHttpClientTransport::with_client(reqwest_client, config);
```

### 2. Preserve allow_stateless default — expose opt-OUT

rmcp's default `true` supports stateless HTTP servers. Overwriting with clap bool (default false) is a silent regression. Expose an opt-OUT flag:

```rust
#[derive(Parser)]
struct Cli {
    /// Require stateful session; disable SSE fallback
    #[arg(long, env = "MCP_REMOTE_STRICT_SESSION")]
    strict_session: bool,
}

// In transport builder:
if cli.strict_session {
    config.allow_stateless = false;
}
// Else: preserve rmcp default (true)
```

### 3. Set raw token, not "Bearer <token>"

rmcp passes `auth_header` to reqwest's `.bearer_auth()` which prepends `"Bearer "`:

```rust
// WRONG — doubles "Bearer "
config.auth_header = Some(format!("Bearer {token}"));

// CORRECT — raw token only
config.auth_header = cli.bearer_token.clone();
```

### 4. mTLS with rustls reqwest

Combine cert+key PEM bytes into one buffer; use `from_pem` (not `from_pem_parts` on rustls backend):

```rust
fn build_identity(cert_pem: &[u8], key_pem: &[u8]) -> reqwest::Identity {
    let mut combined = Vec::with_capacity(cert_pem.len() + key_pem.len());
    combined.extend_from_slice(cert_pem);
    combined.extend_from_slice(key_pem);
    reqwest::Identity::from_pem(&combined)
        .expect("invalid cert/key PEM")
}

fn build_client(cert_path: &Path, key_path: &Path, ca_path: Option<&Path>) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    
    // mTLS identity
    let cert = std::fs::read(cert_path)?;
    let key = std::fs::read(key_path)?;
    builder = builder.identity(build_identity(&cert, &key));
    
    // Custom CA (optional)
    if let Some(ca) = ca_path {
        let ca_pem = std::fs::read(ca)?;
        let cert = reqwest::Certificate::from_pem(&ca_pem)?;
        builder = builder.add_root_certificate(cert);
    }
    
    Ok(builder.build()?)
}
```

### 5. Dependency: rmcp 2.2.0 requires sse-stream >= 0.2.4

rmcp 2.2.0 calls `SseStream::from_bytes_stream`; sse-stream 0.2.3 only has `from_byte_stream`. Fix:

```bash
cargo update -p sse-stream --precise 0.2.4
```

Cargo.lock-only change; rmcp requires `^0.2`, so no manifest edit needed.

### 6. Graceful shutdown: register signal BEFORE serve, use serve_with_ct

rmcp `serve()` blocks until initialize arrives. Signal handler must be registered BEFORE calling serve:

```rust
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(async_main());
    rt.shutdown_timeout(std::time::Duration::from_secs(1));  // Guarantees exit
    result
}

async fn async_main() -> anyhow::Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let serve_ct = CancellationToken::new();
    
    tokio::select! {
        // Post-initialize path
        service = server.serve_with_ct(transport, serve_ct.clone()) => {
            // Wait for shutdown signal
            sigterm.recv().await;
            // Ordered teardown: remote first, then local
            service.service().shutdown_remote().await?;
            service.cancel().await?;
        }
        // Pre-initialize path: signal wins before serve completes
        _ = sigterm.recv() => {
            serve_ct.cancel();  // Cancel pending serve
        }
    }
    
    Ok(())
}
```

Key points:
- Create `Signal` before `serve_with_ct` — installs OS handler immediately
- Use `serve_with_ct` + `tokio::select!` to handle pre-initialize SIGTERM
- Manual runtime + `shutdown_timeout(1s)` guarantees exit (blocked stdio thread)
- Take `RunningService` out of `RwLock` before `.await` — no lock held across await

### 7. Transparent capability reflection

After `serve_client()` completes handshake, read cached remote capabilities via `peer.peer_info()`:

```rust
async fn initialize(
    &self,
    request: InitializeRequestParams,
    context: RequestContext<RoleServer>,
) -> Result<InitializeResult, ErrorData> {
    // Connect to remote
    let transport = build_transport(&self.cli)?;
    let service = rmcp::service::serve_client(RemoteClientHandler, transport).await
        .map_err(proxy_error)?;
    let peer = service.peer().clone();
    
    // Store state
    *self.peer.write().await = Some(peer.clone());
    *self.client_service.write().await = Some(service);
    
    // Reflect remote capabilities + instructions, rebrand server_info
    let remote_info = peer.peer_info()
        .ok_or_else(|| ErrorData::internal_error("remote info unavailable", None))?;
    
    let mut result = InitializeResult::default();
    result.capabilities = remote_info.capabilities.clone();
    result.instructions = remote_info.instructions.clone();
    result.server_info = ServerInfo {
        name: "harnx-mcp-remote".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    
    Ok(result)
}
```

This is safe because `peer_info()` returns cached handshake state — no extra round-trip, no deadlock.

### 8. Prior-art patterns apply

From `mcp-proxy-stdio-pattern-2026-05-19.md`:

1. **Store BOTH Peer and RunningService**:
```rust
struct RemoteProxyServer {
    peer: RwLock<Option<Peer<RoleClient>>>,
    client_service: RwLock<Option<RunningService<RoleClient, RemoteClientHandler>>>,
}
```
Dropping `RunningService` closes connection.

2. **ClientHandler::list_roots returns method_not_found**:
```rust
async fn list_roots(&self, _: RequestContext<RoleClient>) -> Result<ListRootsResult, ErrorData> {
    Err(ErrorData::method_not_found::<rmcp::model::ListRootsRequestMethod>())
}
```

3. **Never call peer methods inside initialize**. The peer methods (e.g., `list_tools`) are only safe to call after handshake completes. In this implementation, `peer.peer_info()` is safe because it reads cached state.

## Why This Works

1. **Unified transport**: `StreamableHttpClientTransport` auto-negotiates SSE vs streamable HTTP via `allow_stateless`; preserves SSE compatibility by default
2. **Default preservation**: Opt-out flag avoids invalidating rmcp's sensible defaults for SSE servers
3. **Raw token**: rmcp's request builder handles `Authorization` header construction; caller provides token only
4. **Pre-initialize shutdown**: Signal registered before serve can fire; `serve_with_ct` + `select!` handles both timing windows
5. **Manual runtime**: `shutdown_timeout()` forces exit without waiting for blocked stdio thread
6. **Capability reflection**: `peer_info()` returns handshake result — no new request, usable immediately

## Prevention Strategies

**Tests:**
- Unit test: verify `auth_header` receives raw token (not `"Bearer <token>"`) 
- Unit test: verify `allow_stateless` defaults to `true` without `--strict-session`
- Integration test: SIGTERM in idle state exits 0 (not 143)
- Integration test: SIGTERM after initialize exits 0 with remote shutdown
- Integration test: initialize response reflects remote capabilities

**Code Review Checklist:**
- [ ] `transport-streamable-http-client-reqwest` feature enabled in root `Cargo.toml`
- [ ] `allow_stateless` NOT overwritten to `false` by default
- [ ] `auth_header` set to raw token (no `"Bearer "` prefix)
- [ ] mTLS uses `Identity::from_pem` with combined cert+key
- [ ] SIGTERM `Signal` created before `serve_with_ct`
- [ ] `tokio::select!` between serve and shutdown signal
- [ ] Manual runtime with `shutdown_timeout(1s)` 
- [ ] `initialize` returns remote capabilities (or minimal static set)
- [ ] BOTH `Peer` and `RunningService` stored in `RwLock<Option<...>>`
- [ ] `list_roots` returns `method_not_found`

**Best Practices:**
- Use `--strict-session` opt-OUT flag for stateless mode (preserve rmcp default)
- Test against stateless SSE servers (`with_stateful_mode(false)`)
- Log startup URL but never log secrets
- Ordered shutdown: remote service first, then local stdio

## Related Issues

- **GitHub:** [#1076](https://github.com/dobesv/harnx/issues/1076) — harnx-mcp-remote proxy
- **Related Solution:** [mcp-proxy-stdio-pattern-2026-05-19.md](mcp-proxy-stdio-pattern-2026-05-19.md) — RunningService lifetime, method_not_found
- **Related Solution:** [streamable-http-server-setup-2026-05-29.md](streamable-http-server-setup-2026-05-29.md) — HTTP server config patterns
