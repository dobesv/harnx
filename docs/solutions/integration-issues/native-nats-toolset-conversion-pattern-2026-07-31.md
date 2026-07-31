---
title: "Native NATS Toolset Conversion Pattern (MCP-behind-bridge → native Toolset)"
date: 2026-07-31
category: "integration-issues"
problem_type: integration_issue
component: "harnx-fs-tools, harnx-toolset, harnx-toolset-server"
root_cause: "stdio MCP servers required harnx-mcp-bridge wrapper to serve over NATS; needed native Toolset implementation to eliminate bridge hop"
resolution_type: code_fix
severity: medium
tags:
  - nats
  - toolset
  - mcp
  - bridge
  - migration
  - rename
  - envelope-parity
plan_ref: "native-fs-tools"
---

## Problem

MCP servers (fs, bash, plans, grep) were designed for stdio transport and required `harnx-mcp-bridge` to serve over NATS. This added a process hop, complicated deployment, and prevented tools from registering directly in the NATS tool registry. The walking-skeleton migration (S1 for fs) needed a repeatable pattern to convert an MCP server into a native `Toolset` implementation while preserving backward compatibility and test coverage.

## Symptoms

- MCP servers ran behind `harnx-mcp-bridge --name X --`, adding an extra process layer
- Tools didn't register directly in `harnx_tool_registry` NATS KV bucket
- Config files required bridge-wrapper syntax: `command: harnx-mcp-bridge, args: [--name, fs, --, harnx-mcp-fs, ...]`
- Crate names still reflected MCP-only role (`harnx-mcp-*`)

## Investigation Steps

1. Analyzed `harnx_toolset::Toolset` trait signature: `name()`, `tools()`, `invoke()` returning `Result<Value, ToolInvokeError>`
2. Traced existing handler return types: `*_impl` methods return `Result<CallToolResult, ErrorData>`
3. Verified envelope parity: `harnx-mcp-bridge` uses `serde_json::to_value(CallToolResult)` at lib.rs:313 — identical to what native `invoke()` would produce
4. Examined `harnx-time-server` as template: minimal `Toolset` impl delegating to handler methods
5. Identified rename scope: live code/config/docs must update, historical files (CHANGELOG, landed changesets, solution docs) must be preserved as frozen records

## Root Cause

MCP servers were built for stdio transport with `rmcp::model::CallToolResult` envelopes. The bridge layer performed mechanical JSON serialization but added deployment complexity. A native `Toolset` could produce byte-identical output while eliminating the bridge hop, but needed:
- Result envelope mapping from `CallToolResult` to `Value`
- Error classification from `ErrorData` to `ToolInvokeError`
- Schema generation using existing `JsonSchema` param structs
- Roots initialization without rmcp peer (CLI-only for native mode)

## Solution

### 1. Transport Swap (Mechanical Adapter)

The `Toolset::invoke` implementation wraps existing `*_impl` handler methods:

```rust
fn map_result(result: Result<CallToolResult, ErrorData>) -> Result<Value, ToolInvokeError> {
    match result {
        Ok(result) => serde_json::to_value(result).map_err(|err| {
            ToolInvokeError::Fatal(format!("failed to serialize tool result: {err}"))
        }),
        Err(err) => Err(ToolInvokeError::Recoverable(err.message.to_string())),
    }
}

async fn invoke(&self, tool: &str, args: Value, _cancel: CancellationToken) 
    -> Result<Value, ToolInvokeError> 
{
    let result = match tool {
        "read" => self.server.read_file_impl(parse_args(args)?).await,
        "write" => self.server.write_file_impl(parse_args(args)?).await,
        // ... other tools ...
        _ => return Err(ToolInvokeError::Recoverable(format!("unknown fs tool: {tool}"))),
    };
    map_result(result)
}
```

**Key insight**: Keep existing `*_impl` handler methods unchanged. This preserves 48+ unit tests that call them directly.

### 2. Envelope Parity (Critical for Agent Compatibility)

The old bridge already performed `serde_json::to_value(CallToolResult)`. A native Toolset doing the same yields **structurally identical** agent-visible result envelopes — no normalization needed. (`serde_json::Value` equality is structural, which is what agents rely on; it does not depend on serialized byte order.)

**Verify with deep-equality test**:

```rust
async fn assert_envelope_parity(
    client: &async_nats::Client,
    instance_id: &InstanceId,
    root_path: &Path,
) -> anyhow::Result<()> {
    let params = ReadFileParams { path: /* ...a path under root_path... */ };
    let args = serde_json::to_value(&params)?;

    // Direct call to the handler (what the bridge serialized)
    let direct_server = FsServer::new(vec![root_path.to_path_buf()], false);
    let direct_read = direct_server.read_file_impl(params).await?;
    let bridged_read_value = serde_json::to_value(direct_read)?;

    // Over NATS
    let nats_reply = invoke(client, instance_id, "read", args).await?;
    let nats_value = nats_reply.result?;

    // Structurally identical to the bridge's envelope
    assert_eq!(nats_value, bridged_read_value);
    Ok(())
}
```

### 3. Schema Generation (Reuse JsonSchema)

Don't hand-write `ToolSpec.input_schema`. Use existing param struct `JsonSchema` impls via rmcp:

```rust
fn input_schema<T: JsonSchema + 'static>() -> Value {
    Tool::new("schema", "schema", Map::new())
        .with_input_schema::<T>()
        .schema_as_json_value()
}

fn spec<T: JsonSchema + 'static>(name: &str, description: &str, read_only_hint: bool) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: input_schema::<T>(),
        idempotent_hint: false,
        read_only_hint,
        timeout_secs: None,
    }
}
```

### 4. Roots-from-CLI-Only in Native Mode

No rmcp peer exists in native mode, so drop `peer.list_roots`/`ensure_roots_initialized` path. Seed from CLI:

```rust
impl FsToolset {
    pub async fn new(initial_roots: Vec<PathBuf>, default_root_cwd: bool) -> Self {
        let server = FsServer::new(initial_roots, default_root_cwd);
        server.seed_default_root_if_empty().await;
        Self { server }
    }
}
```

Keep peer-based methods for `--mcp-stdio` back-compat. Empty roots = deny-all (safe by default).

### 5. main.rs Template

`run_toolset_main(toolset)` provides `--mcp-stdio` back-compat for free:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (roots, default_root_cwd) = parse_args();
    let toolset = FsToolset::new(roots, default_root_cwd).await;
    harnx_toolset_server::run_toolset_main(toolset).await
}
```

### 6. Config Migration

Drop the bridge wrapper:

**Before**:
```yaml
tool_servers/fs.yaml:
  command: harnx-mcp-bridge
  args: [--name, fs, --, harnx-mcp-fs, --default-root-cwd]
```

**After**:
```yaml
tool_servers/fs.yaml:
  command: harnx-fs-tools
  args: [--default-root-cwd]
```

### 7. Rename Discipline

Distinguish LIVE references (rename all) from HISTORICAL records (leave frozen):

**LIVE (rename)**:
- `Cargo.toml` workspace.members, package.name, binary name
- `.github/workflows/release.yaml` (all 4 spots)
- `docker/harnx.Dockerfile` COPY line
- `tool_servers/*.yaml` configs
- `README.md`, `docs/*.md` (live docs)
- Doc-comments in other crates referencing the tool

**HISTORICAL (preserve)**:
- `CHANGELOG.md`
- Already-landed `.changeset/*.md` files
- Existing `docs/solutions/*.md` (post-mortems about past work under old name)

Run `cargo fmt --all` after rename (string-length changes cause line-wrap violations).

### 8. Gotchas

- **CodeScene `cs delta origin/HEAD`** flags large test methods (>70 lines) — extract into phase helpers
- **Project mandates `cargo nextest`**, never `cargo test`
- **`parse_args` name collision**: `main.rs` (CLI) vs `toolset.rs` (JSON deserialization) — rename one for searchability if desired
- **Cancel token unused**: fs ops are synchronous; no long-running await points to cancel

## Why This Works

1. **Envelope parity**: The bridge already serialized `CallToolResult` to JSON. Native Toolset produces identical wire format.
2. **Test preservation**: Existing `*_impl` handler tests survive unchanged because the native path wraps, not replaces.
3. **Back-compat**: `run_toolset_main` handles `--mcp-stdio` flag, serving MCP over stdio when needed.
4. **Schema reuse**: `JsonSchema` derives on param structs generate correct schemas for both MCP and Toolset paths.
5. **Safety preserved**: Roots validation and `$HOME`-ancestor guard work identically in both modes.

## Prevention Strategies

**Test Cases**:
- Envelope parity test: `assert_eq!` NATS reply against direct `to_value(CallToolResult)`
- Roots deny-on-empty: invoke tool with empty roots, assert `Recoverable` error
- Out-of-root denial: invoke tool with path outside roots, assert denial
- Write/read round-trip over real NATS

**Code Review Checklist**:
- [ ] Does `invoke()` map all `ErrorData` → `Recoverable`? (Fatal only for serde failures)
- [ ] Are all tool names in `tools()` covered by `invoke()` match arms?
- [ ] Is empty-roots path tested (deny-all)?
- [ ] Does integration test verify envelope parity?

**For S3/S4 (plans/grep/bash)**:
- Follow `FsToolset` template exactly
- Keep existing handler `*_impl` methods
- Add `run_toolset_main` for free `--mcp-stdio`
- Test envelope parity on first slice, copy pattern thereafter

## Related Issues

- **Issue**: [#1224](https://github.com/dobesv/harnx/issues/1224) — Native tool-server migration (umbrella)
- **Changeset**: `.changeset/native-fs-tools.md`
- **Related Solution**: `integration-issues/nats-fs-bash-bridged-cwd-default-2026-07-30.md` — Earlier bridge-based approach
- **Template**: `crates/harnx-time-server/src/lib.rs` — TimeToolset pattern
