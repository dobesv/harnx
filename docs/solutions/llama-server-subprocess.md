---
title: "Managed llama-server subprocess via Unix domain sockets"
date: 2026-06-12
category: "integration-issues"
problem_type: integration_issue
component: "llama-server-provider"
root_cause: "In-process llama.cpp (FFI) is heavy and fragile; TCP ports are prone to collision/leak"
resolution_type: code_fix
severity: low
tags:
  - llama.cpp
  - llama-server
  - unix-socket
  - subprocess
  - local-llm
  - hyperlocal
  - cross-platform
  - macro-code-generation
plan_ref: "llama-cpp-local-provider"
last_updated: 2026-06-12
---

# Managed llama-server subprocess via Unix domain sockets

## Problem

Integrating `llama.cpp` for local model support in `harnx` presented several challenges:
1. **In-process FFI**: Using `llama-cpp-2` or similar Rust bindings requires a heavy C++ build toolchain and links a massive binary, complicating the `harnx` distribution.
2. **External TCP Services**: Running a standalone `llama-server` over TCP (HTTP) requires the user to manage the process and risk port collisions or leaving orphan processes behind.
3. **Overhead**: Standard HTTP over loopback still involves the full TCP stack.

## Solution: Managed Subprocess + UDS

We implemented a `llama-server` provider that manages a local child process and communicates over **Unix domain sockets (AF_UNIX)**.

### 1. Managed Lifecycle
Harnx spawns the `llama-server` binary as a child process.
- **Lazy Spawn**: The process is only started on the first request to a specific model.
- **Kill-on-Drop**: Using `tokio::process::Command` with `kill_on_drop(true)` ensures that if the `harnx` process exits, the `llama-server` instances are also terminated.
- **Single-Flight Spawn**: A global registry ensures that multiple concurrent requests to the same subprocess config don't trigger multiple spawns.

### 2. Unix Domain Sockets (UDS)
Instead of a TCP port, we use a Unix socket file:
- **No Port Collisions**: Sockets are file-based and scoped to the user's data directory.
- **Performance**: UDS avoids the overhead of the TCP stack for local inter-process communication.
- **Automatic Cleanup**: The socket file is unlinked when the process manager is dropped.
- **Default Path**: `~/.local/share/harnx/llama-server-<pid>-<hash>.sock`.

### 3. Hyper-local Transport
Since `reqwest` does not support Unix sockets natively, we implemented a custom `hyper` connector using `tokio::net::UnixStream`. This allows us to use the standard OpenAI-compatible API provided by `llama-server` over the socket.

## Why This Works

- **Lean Binary**: The `harnx` binary stays small because it doesn't link `llama.cpp` directly.
- **Zero Configuration**: If `llama-server` is in the `PATH`, the user only needs to provide a `model_path`.
- **Robustness**: The subprocess approach isolates the C++ memory management from the Rust runtime.
- **Full Compatibility**: Supports streaming, tool calls (grammar-constrained), and all other standard chat completion features.

---

## Engineering Gotchas & Lessons Learned

### 1. Transport: `reqwest` Cannot Do Unix Sockets → Use `hyperlocal`

**Problem**: `reqwest` (used by all other harnx providers) has no native UDS support.

**Solution**: Use `hyperlocal` crate with `hyper 1.x` / `hyper-util 0.1`:
- Dependency: `hyperlocal = { version = "0.9.1", default-features = false, features = ["client"] }`
- Client: `hyper_util::client::legacy::Client<UnixConnector, Full<Bytes>>`
- URI encoding: `hyperlocal::Uri::new(&socket_path, "/v1/chat/completions")`

**Concurrency**: Multiple parallel HTTP requests allowed over single UDS. Only process **startup** is serialized (OnceCell). Hyper's `UnixConnector` opens a fresh `tokio::net::UnixStream` per request.

### 2. SSE Streaming: Buffer Events Across Frame Boundaries

**Problem**: UDS byte stream arrives in arbitrary frame chunks. SSE events are `\n\n`-delimited, but a frame may split mid-event.

**Solution**: Buffer bytes across frames, drain complete events only:
```rust
let mut sse_buffer = Vec::new();
while let Some(frame) = res.body_mut().frame().await {
    if let Some(data_bytes) = frame.data_ref() {
        sse_buffer.extend_from_slice(data_bytes);
        while let Some(event_end) = find_sse_event_boundary(&sse_buffer) {
            let event_bytes: Vec<u8> = sse_buffer.drain(..event_end).collect();
            sse_buffer.drain(..2); // consume "\n\n"
            handle_sse_event(&event_bytes, ...)?;
        }
    }
}
```

**Key**: Inner loop drains ALL available events before awaiting next frame.

### 3. Reusing OpenAI Logic Without Its Transport

**Problem**: Existing `openai.rs` `send()` fns are reqwest/TCP-bound. Cannot use `impl_client_trait!` macro.

**Solution**:
- Hand-implement `Client` trait for `LlamaServerClient`
- Bump visibility of body builder + response/SSE per-event parsing to `pub(crate)`:
  - `openai_build_chat_completions_body`, `openai_extract_chat_completions`
  - `openai_handle_stream_event`, `openai_emit_pending_tool_call`
  - `OpenAiStreamState`
- Single source of truth, no code duplication

### 4. `register_client!` Macro Generates the Client Struct

**Problem**: `register_client!` macro generates `{config, model}` struct. Hand-written struct with extra fields (process manager handle) conflicts.

**Solution**: Let macro own the struct. Obtain manager lazily from process-global registry:
```rust
static MANAGERS: OnceLock<Mutex<HashMap<ProcessIdentity, Arc<LlamaServerProcessManager>>>>;

fn get_or_create_manager(config: &LlamaServerProcessConfig) -> Result<Arc<LlamaServerProcessManager>>;
```

### 5. Registry Keying: Composite Identity Over ALL Process-Affecting Config

**Problem** (from review): Keying by `model_path` alone silently reuses wrong subprocess or collides sockets.

**Solution**: Key by composite identity including:
- `model_path` (resolved)
- `binary_path` (resolved)
- `socket_path` (resolved)
- `context_size`, `gpu_layers`, `threads`
- `extra_args`

```rust
struct ProcessIdentity {
    canonical: String,  // "model_path=...\nbinary_path=...\n..." 
}
```

Default socket path: `llama-server-<pid>-<short_hash(identity)>.sock` — unique per process identity.

### 6. Process Lifecycle: Dead-Process Recovery + Stderr Tail

**Startup**:
- Single-flight spawn via `Mutex<Option<Arc<RunningServer>>>` (not `OnceCell`)
- Readiness via `/health` poll with LONG configurable timeout (default 5 min — large GGUFs load slowly)
- Surface child stderr tail (last 200 lines) on early exit or timeout

**Recovery**:
```rust
pub async fn ensure_ready(&self) -> Result<Arc<RunningServer>> {
    let mut state = self.state.lock().await;
    if let Some(running) = state.as_ref() {
        if running.child_try_wait().await?.is_none() {
            return Ok(running.clone());  // still alive
        }
        warn!("process exited; respawning");
        state.take();  // clear dead state
    }
    // spawn new...
}
```

**Drop**: Explicit cleanup of socket file, abort stdout/stderr drain tasks.

### 7. Socket Cleanup: Only Unlink Unix Sockets

**Problem** (from review): Unlinking arbitrary `socket_path` could delete user files.

**Solution**: Check file type before removal:
```rust
fn cleanup_socket_path(socket_path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => {
            if metadata.file_type().is_socket() {
                std::fs::remove_file(socket_path)?;
            } else {
                bail!("refusing to remove non-socket file at {}", socket_path.display());
            }
        }
        Err(e) if e.kind() == NotFound => {}  // ok
        Err(e) => return Err(e).context(...),
    }
    Ok(())
}
```

### 8. Cross-Platform: Compile on Windows

**Problem** (from review): Macro generates struct on all platforms; impl is `#[cfg(unix)]` → Windows build breaks.

**Solution**:
- Provide `#[cfg(not(unix))]` impl that bails at runtime:
  ```rust
  // client_unsupported.rs
  impl Client for LlamaServerClient {
      async fn chat_completions_inner(...) -> Result<ChatCompletionsOutput> {
          bail!("llama-server provider requires a Unix platform (uses unix domain sockets)")
      }
  }
  ```
- Platform-gate unix-only deps in `Cargo.toml`:
  ```toml
  [target.'cfg(unix)'.dependencies]
  hyperlocal = { workspace = true }
  ```
- Provide `PROMPTS` constant on all platforms (empty for llama-server).

---

## Rejected Alternatives

1. **In-process FFI (`llama-cpp-2`)**: Heavy cmake/C++ build, binary bloat, GPU pinning, `spawn_blocking` complexity. Subprocess isolation is cleaner.

2. **Random TCP Port**: Works but port-discovery via stderr parse is racier than known socket path. UDS path chosen by harnx up front.

3. **`llama-agent` stdio JSON-RPC**: DOES NOT EXIST in llama.cpp core. `llama-agents` is LlamaIndex Python, unrelated.

---

## Key Decisions

- **One Process per Unique Runtime Config**: Instances keyed by full process identity. Identical configs share one process; different knobs isolate processes.
- **Unix-Only**: Keeps implementation simple. Windows users use `ollama` or `openai-compatible`.
- **Stable Socket Hash**: Default socket names include short hash so distinct logical servers in one harnx process do not collide.

---

## File Pointers

- `crates/harnx-core/src/provider_config/llama_server.rs`: Configuration structure.
- `crates/harnx-client/src/llama_server/process.rs`: Process manager, binary discovery, UDS transport.
- `crates/harnx-client/src/llama_server/client.rs`: Implementation of the `Client` trait for llama-server.
- `crates/harnx-client/src/llama_server/client_unsupported.rs`: Windows stub impl.
- `crates/harnx-client/src/lib.rs`: Provider registration via `register_client!`.
- `crates/harnx-client/Cargo.toml`: Platform-gated `hyperlocal` dependency.

---

## Test Coverage

- Unit tests: SSE event buffering/splitting, tool-call delta accumulation, socket cleanup safety, process identity uniqueness.
- Integration: `#[ignore]`d smoke test (requires real `llama-server` + GGUF via `HARNX_LLAMA_SERVER_TEST_BIN` / `HARNX_LLAMA_SERVER_TEST_MODEL`).
- Concurrency: Single-flight spawn verification with fake `RunningServer`.
