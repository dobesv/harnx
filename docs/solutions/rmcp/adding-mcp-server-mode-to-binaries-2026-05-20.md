---
title: "Adding MCP server mode to existing Rust binaries"
date: 2026-05-20
category: "rmcp"
problem_type: integration_issue
component: "rmcp-server-integration"
root_cause: "pattern discovery for async-safe idempotent tool handlers"
resolution_type: code_fix
severity: medium
tags:
  - mcp
  - rmcp
  - tokio
  - oncecell
  - async
  - idempotency
  - testing
plan_ref: "harnx-mcp-auth-servers"
---

# Adding MCP server mode to existing Rust binaries

## Problem

Existing Rust binaries that run background services (credential servers, proxies) needed an MCP server mode to expose their functionality via a tool that Claude Code agents could call once per session. The implementation required async-safe idempotent initialization, proper resource lifetime management, and testable MCP handlers.

## Symptoms

- Agents had no way to call setup tools that return environment variables for authenticating subsequent commands
- Existing hook-based invocationJSONL over stdin/stdoutdid not fit MCP tool model
- Concurrent tool calls could race during server startup, causing resource leaks or spurious errors
- Resources like CA temp directories could be dropped too early if not owned by server struct

## Investigation Steps

1. Reviewed existing MCP servers in repo (`harnx-mcp-bash`, `harnx-mcp-fs`) for `ServerHandler` pattern
2. Identified `OnceLock` idempotency pattern but found `std::sync::OnceLock` unsuitable for async initialization
3. Discovered `tokio::sync::OnceCell::get_or_try_init()` handles async initialization atomically
4. Found `Mutex<Option<T>>` needed for consumed resources like `CaSetup` (non-Clone)
5. Tested both in-memory (`tokio::io::duplex` with `serve_server`/`serve_client`) and E2E (binary spawn with JSON-RPC over stdio)

## Root Cause

The challenge was not a bug but pattern discovery for:
1. **Async-safe idempotency**: `std::sync::OnceLock` doesn't support async initialization; `.get_or_init()` closure must be sync
2. **Consumed resource startup**: Some startup functions take ownership of non-Clone resources
3. **Testability**: Need both unit tests (in-memory) and E2E tests (real binary)

## Solution

### Pattern 1: `tokio::sync::OnceCell` for async-safe idempotent initialization

```rust
use tokio::sync::OnceCell;

pub(crate) struct AwsCredsServer {
    state: Arc<AppState>,
    port: OnceCell<u16>,  // async-safe
}

impl ServerHandler for AwsCredsServer {
    async fn call_tool(&self, request: CallToolRequestParams, _context: RequestContext<RoleServer>) 
        -> Result<CallToolResult, ErrorData> 
    {
        match request.name.as_ref() {
            "aws_auth_setup" => {
                // get_or_try_init runs the closure exactly once, even under concurrent access
                let port = *self.port
                    .get_or_try_init(|| async {
                        let listener = TcpListener::bind("127.0.0.1:0")
                            .await
                            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
                        start_server(Arc::clone(&self.state), listener)
                            .await
                            .map_err(|err| ErrorData::internal_error(err.to_string(), None))
                    })
                    .await?;
                // ... return env vars with port
            }
            name => Err(ErrorData::invalid_params(format!("unknown tool: {name}"), None)),
        }
    }
}
```

Key insight: `tokio::sync::OnceCell::get_or_try_init()` is the async-aware equivalent of `std::sync::OnceLock`. Only one caller runs the closure; others wait for the result.

### Pattern 2: `Mutex<Option<PreStartState>>` for consumed-resource startup

When `start_proxy` consumes ownership of `CaSetup` (non-Clone), use a Mutex to guard the pre-start state:

```rust
use tokio::sync::{Mutex, OnceCell};

struct PreStartState {
    filter: CompiledFilter,
    ca_setup: CaSetup,
}

pub(crate) struct ProxyAuthServer {
    pre_start: Mutex<Option<PreStartState>>,
    started: OnceCell<u16>,
    // other fields...
}

impl ProxyAuthServer {
    async fn proxy_port(&self) -> Result<u16, ErrorData> {
        // OnceCell ensures only one caller runs the closure even under concurrency
        let port = self.started
            .get_or_try_init(|| async {
                let mut guard = self.pre_start.lock().await;
                match guard.take() {
                    Some(pre_start) => {
                        // Drop lock before async call to avoid holding mutex across await
                        drop(guard);
                        proxy::start_proxy(pre_start.filter, pre_start.ca_setup)
                            .await
                            .map_err(|err| ErrorData::internal_error(err.to_string(), None))
                    }
                    None => {
                        // Startup was previously attempted but failed
                        // CaSetup consumed, cannot retry
                        Err(ErrorData::internal_error(
                            "proxy startup previously failed; restart process to retry".to_string(), 
                            None
                        ))
                    }
                }
            })
            .await?;
        Ok(*port)
    }
}
```

Key insight: Drop the mutex guard before the async `start_proxy` call to avoid holding a mutex across an await point.

### Pattern 3: Testing async MCP handlers in-memory

Use `tokio::io::duplex` with `rmcp::service::{serve_server, serve_client}`:

```rust
use rmcp::service::{serve_server, serve_client, RunningService};
use tokio::io::duplex;

struct TestConnection {
    _server_service: RunningService<RoleServer, MyServer>,
    client_service: RunningService<RoleClient, TestClientHandler>,
}

async fn connect_server(state: Arc<AppState>) -> TestConnection {
    let (client_transport, server_transport) = duplex(65_536);
    let server = MyServer { state, port: OnceCell::new() };
    let server_fut = serve_server(server, server_transport);
    let client_fut = serve_client(TestClientHandler, client_transport);
    let (server_res, client_res) = tokio::join!(server_fut, client_fut);
    TestConnection {
        _server_service: server_res.unwrap(),
        client_service: client_res.unwrap(),
    }
}

#[tokio::test]
async fn call_tool_returns_env_vars() {
    let state = crate::tests::test_state();
    let TestConnection { _server_service, client_service } = connect_server(state).await;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move { let _ = client_service.waiting().await; });
    
    let result = peer.call_tool(CallToolRequestParams::new("aws_auth_setup")).await.unwrap();
    let text = text_content(&result);
    assert!(text.contains("AWS_CONTAINER_CREDENTIALS_FULL_URI=http://127.0.0.1:"));
}
```

### Pattern 4: E2E testing via binary spawn

Spawn the real binary and speak MCP JSON-RPC over stdio:

```rust
struct McpSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpSession {
    async fn spawn_initialized(binary: &PathBuf, services: Option<&str>) -> Self {
        let mut child = Command::new(binary)
            .arg("--mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn binary --mcp");
        // ... wire up stdin/stdout, send initialize, wait for response
        // Send notifications/initialized (no response expected)
        // Return session
    }

    async fn send_request(&mut self, method: &str, params: Value) -> Value {
        // Write newline-delimited JSON-RPC, read response matching the id
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_call_aws_auth_setup_idempotent() {
    let Some(binary) = aws_creds_binary_path() else { return; };
    let mut session = McpSession::spawn_initialized(&binary, None).await;
    let r1 = session.send_request("tools/call", json!({"name": "aws_auth_setup", "arguments": {}})).await;
    let r2 = session.send_request("tools/call", json!({"name": "aws_auth_setup", "arguments": {}})).await;
    assert_eq!(extract_uri(&r1), extract_uri(&r2));
}
```

Key insight: Use `tokio::time::timeout` to avoid hanging if the binary crashes.

### Pattern 5: Adding `--mcp` as a clap flag

```rust
#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    mcp: bool,
    // ... other args
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let state = build_app_state(&args).await?;
    
    if args.mcp {
        mcp::run(state).await
    } else {
        // existing hook mode
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = start_server(state, listener).await?;
        // ... announce readiness, run JSONL loop
    }
}
```

### Pattern 6: Lifetime management for owned resources

Resources that must outlive the MCP server (e.g., `TempDir` holding CA cert) should be moved into the server struct:

```rust
pub(crate) struct ProxyAuthConfig {
    pub filter: CompiledFilter,
    pub ca_setup: CaSetup,
    pub ca_cert_path: PathBuf,
    pub ca_temp_dir: TempDir,  // owned for lifetime
    pub services: Option<String>,
}

pub(crate) struct ProxyAuthServer {
    pre_start: Mutex<Option<PreStartState>>,
    ca_cert_path: PathBuf,
    _ca_temp_dir: TempDir,  // underscore prefix: stored for lifetime, not read
    started: OnceCell<u16>,
    services: Option<String>,
}
```

Key insight: The underscore prefix (`_ca_temp_dir`) signals "stored for drop lifetime, not actively used."

## Why This Works

**`tokio::sync::OnceCell`** provides async-safe one-time initialization. The closure runs exactly once even under concurrent calls. Unlike `std::sync::OnceLock`, it handles async initialization correctly.

**`Mutex<Option<T>>`** guards consumed resources while allowing the async startup to proceed without holding the lock (drop guard before async call). This serializes access to the state while keeping async operations lock-free.

**In-memory testing** via duplex transport tests handler logic without process spawning. E2E testing via binary spawn validates full integration including CLI parsing and transport.

**Ownership pattern** for `TempDir` ensures the CA cert file exists for the server's lifetime. RAII semantics clean it up on drop.

## Prevention Strategies

**Tests:**
- Add unit tests for idempotency (sequential and concurrent calls)
- Add E2E tests for MCP protocol flow (initialize -> tools/list -> tools/call)
- Test failure paths where possible (error message formatting)

**Best Practices:**
- Use `tokio::sync::OnceCell` (not `std::sync::OnceLock`) for async initialization
- Drop mutex guards before async calls to avoid holding locks across await
- Store `TempDir` in server struct for CA cert lifetime management
- Use `_field_name` prefix for fields stored only for lifetime (Drop)

**Code Review Checklist:**
- [ ] Is `OnceCell` (not `OnceLock`) used for async initialization?
- [ ] Are mutex guards dropped before async calls?
- [ ] Are owned resources (TempDir) stored in server struct for lifetime?
- [ ] Are both unit and E2E tests present?

## Related Issues

- **Issue:** [#604](https://github.com/dobesv/harnx/issues/604) — Add MCP server mode to auth servers
- **Related Solution:** [rmcp/mcp-proxy-stdio-pattern-2026-05-19.md](./mcp-proxy-stdio-pattern-2026-05-19.md) — RunningService lifetime management
- **Related Solution:** [proxy-hooks/hudsucker-persistent-hook-proxy-2026-05-16.md](../proxy-hooks/hudsucker-persistent-hook-proxy-2026-05-16.md) — Proxy readiness protocol
