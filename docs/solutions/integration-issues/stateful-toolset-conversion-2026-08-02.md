---
title: "Stateful Server Native Toolset Conversion (plans/bash/grep)"
date: 2026-08-02
category: "integration-issues"
problem_type: integration_issue
component: "harnx-plans-tools, harnx-bash-tools, harnx-grep-tools"
root_cause: "Stateful MCP servers required bridge wrapper; conversion needed to preserve background tasks, CLI flags, and shipping manifests"
resolution_type: code_fix
severity: medium
tags:
  - nats
  - toolset
  - stateful
  - background-tasks
  - rename
  - release-manifest
plan_ref: "tool-servers-native-conversion"
---

## Problem

The second wave of MCP-to-native conversions (plans, bash, grep) involved servers with stateful concerns absent from the S1 fs conversion: background retention cleanup loops, sandbox process tracking, CLI flag dependencies, and binaries missing from release manifests. The S1 pattern worked but needed extensions for these cases.

## Symptoms

- **Plans**: Background retention cleanup loop ran only in HTTP mode; native and MCP-stdio modes left cleanup orphaned or broken
- **Bash**: Roots initialization depended on an MCP peer that doesn't exist in native mode; sandbox/history/process state needed preservation
- **Grep**: Binary was never in release.yaml or Dockerfile (pre-existing gap), but native invocation made it load-bearing — grep broken in production
- **All three**: Server-specific CLI flags needed to work alongside `run_toolset_main`'s `--mcp-stdio` detection

## Investigation Steps

1. Examined `PlansServer` retention supervision — found HTTP-only cleanup task spawn; needed equivalent for native mode
2. Traced `BashServer` roots initialization — discovered `initialize_native_roots` method that seeds from CLI when no peer exists
3. Reviewed release.yaml and Dockerfile for all three binaries — found grep missing entirely
4. Verified `run_toolset_main` only detects `--mcp-stdio`; server-specific flags must be parsed before calling it
5. Audited `ToolSpec` vs MCP `Tool.meta` gap — native spec lacks templates/annotations; concluded old `ServerHandler` must remain for standalone MCP modes needing those fields

## Root Cause

The S1 fs conversion pattern assumed stateless tools with simple CLI parsing. Stateful servers introduced complications:

1. **Background tasks**: Retention cleanup, process management needed supervision around `run_toolset_main`
2. **CLI root seeding**: Native mode has no MCP peer to call `list_roots()` — must seed from CLI flags
3. **Multi-transport servers**: Existing `--http` and `--mcp-stdio` modes must stay functional alongside native NATS
4. **Release manifest gaps**: Previously bridged binaries weren't individually shipped; native invocation exposes the gap
5. **Rename scope explosion**: GHCR image identity, deployment configs, historical records needed careful delineation

## Solution

### 1. Stateful Servers: Supervise Background Tasks Around run_toolset_main

Spawn detached background tasks before calling `run_toolset_main`. Abort supervisors when the runner returns, restart after clean exits, apply exponential backoff after panics.

**Plans example** (retention cleanup loop):

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = parse_args(); // --retention-days, --dir, --http, --mcp-stdio
    
    // For native/MCP-stdio modes, start retention supervision
    let supervisor_handle = if config.retention_days > 0 && !config.http {
        Some(tokio::spawn(supervise_cleanup(config.dir.clone(), config.retention_days)))
    } else {
        None
    };
    
    let toolset = PlansToolset::new(config.dir.clone());
    let result = harnx_toolset_server::run_toolset_main(toolset).await;
    
    // Clean shutdown: abort supervisor
    if let Some(handle) = supervisor_handle {
        handle.abort();
    }
    
    result
}
```

HTTP mode keeps its existing cleanup supervision separate. The key insight: `run_toolset_main` is a blocking-ish async loop — spawn background tasks before calling it, manage lifecycle externally.

### 2. Native Root Seeding Without MCP Peer

When there's no MCP peer (native mode), skip `peer.list_roots()` and seed from CLI flags instead.

**Bash example** (also applies to fs):

```rust
impl BashToolset {
    pub async fn new(initial_roots: Vec<PathBuf>, default_root_cwd: bool) -> Self {
        let server = BashServer::new(
            initial_roots.clone(),
            SandboxConfig::default(),
            default_root_cwd,
        );
        // Native roots: no peer, seed from CLI
        server.initialize_native_roots().await; // applies guarded CWD fallback, marks initialized
        Self { server }
    }
}
```

The `initialize_native_roots` method applies the same CWD fallback logic as `(peer.list_roots(), ensure_roots_initialized)` would in MCP mode. Roots/extra allowlist semantics unchanged — only the initialization source differs.

### 3. Preserve Legacy Standalone Modes

`run_toolset_main` detects `--mcp-stdio` and wraps the toolset in `McpToolsetAdapter`. It does NOT handle server-specific flags like `--http`, `--retention-days`, or `--dir`. Parse those first, branch before calling the runner:

```rust
let config = parse_args();

// Legacy HTTP mode keeps its own server
if config.http {
    return run_plans_http_server(config).await;
}

// Native NATS (default) or MCP-stdio: use shared runner
let toolset = PlansToolset::new(config.dir);
harnx_toolset_server::run_toolset_main(toolset).await
```

Unknown-argument validation must explicitly accept `--mcp-stdio` in the local parser.

### 4. Catch Missing Release Manifest Entries

A binary that was previously bridge-wrapped (`harnx-vercel-grep-server`) may never have been in release.yaml or Dockerfile. Once natively invoked, it becomes load-bearing but still won't ship.

**Detection**: After YAML migration, grep archive specs and Dockerfiles for the new binary name. If missing, add:

```yaml
# release.yaml
archive_specs: [
  # ... existing entries ...
  "harnx-grep-tools:harnx-grep-tools",
]

# In artifact download patterns (x86_64 and aarch64):
- harnx-grep-tools-${{ env.VERSION }}-${{ matrix.target }}.tar.gz

# In docker bin verify loop:
for bin in harnx-fs-tools harnx-bash-tools harnx-plans-tools harnx-grep-tools; do

# Dockerfile:
COPY linux-${TARGETARCH}/harnx-grep-tools /usr/local/bin/harnx-grep-tools
```

### 5. Rename Discipline + GHCR Identity Preservation

Distinguish three categories:

**LIVE CODE (rename all references)**:
- Workspace `Cargo.toml` members
- Crate `Cargo.toml` package name and `[[bin]]` name
- `.github/workflows/release.yaml` build `-p` list, archive specs, artifact patterns, docker bin loop
- `docker/harnx.Dockerfile` COPY lines
- Code references to binary names (invocations, test fixtures)
- `tool_servers/*.yaml` configs
- Live documentation (READMEs, guides)

**DEPLOYMENT IDENTITY (preserve or migrate deliberately)**:
- GHCR image tags: `ghcr.io/dobesv/harnx-mcp-plans` — renaming breaks existing pull clients
- Dockerfile filename: `docker/harnx-mcp-plans.Dockerfile`
- Release step name: "Build and push harnx-mcp-plans"
- Decision: Repoint internal binary references (COPY dest, ENTRYPOINT) to new binary name, but preserve image tag/Dockerfile filename/release step name to avoid breaking deployments

**HISTORICAL (leave frozen)**:
- `CHANGELOG.md`
- Landed `.changeset/*.md`
- Existing `docs/solutions/*.md`
- Test references in committed snapshots

Path-dependency audit: Check for `path = "../old-crate-name"` in `Cargo.toml` files. None were found for the renamed crates (they weren't path-dependencies of other crates).

### 6. ToolSpec vs MCP Tool.meta Gap

At the time of this conversion, native `ToolSpec` had only: name, description, JSON schema, idempotent/read-only hints, timeout — no counterpart to MCP `Tool.meta` (templates, annotations).

`ToolSpec` since gained a `meta` field, and the native toolset path *does* use it: the runtime reads `call_template`/`result_template` out of it to render tool calls. Assuming otherwise is what broke the custom markdown rendering for every first-party server. Keep the templates in shared consts that both the `ServerHandler` and the `Toolset` read, so the two paths can't drift.

### 7. Lib.rs Sharing Pattern

Extract shared types to `src/lib.rs` so binary (`main.rs`) and tests can both access `Toolset` and server types. Handler visibility only needs `pub(crate)` when toolset is a sibling module — no need to rewrite handlers or export publicly.

## Why This Works

1. **Background task supervision**: Spawning before `run_toolset_main` ensures tasks live as long as the server; aborting on clean exit prevents orphan processes
2. **CLI root seeding**: Same validation/allowlist logic whether roots come from MCP peer or CLI flags
3. **Multi-transport preservation**: Branching before `run_toolset_main` keeps legacy modes untouched; no regression risk
4. **Manifest gap detection**: Simple grep after YAML migration catches what would otherwise be a production outage
5. **GHCR identity preservation**: Binary rename is internal; external clients continue pulling the same image tag

## Prevention Strategies

**Test Cases**:
- Native mode invocation: verify `--default-root-cwd` and `--root` flags seed properly
- MCP-stdio mode: `initialize` + `tools/list` over stdin returns all tools
- HTTP mode (if applicable): verify still works after native conversion
- Background task lifecycle: retention cleanup runs during native mode, aborts on clean shutdown
- Release artifact check: grep archive specs and Dockerfile for each natively-invoked binary

**Code Review Checklist**:
- [ ] Does `main.rs` parse server-specific flags before calling `run_toolset_main`?
- [ ] Does `Toolset::new` call `initialize_native_roots()` when there's no peer?
- [ ] Are background tasks spawned before `run_toolset_main` and aborted on shutdown?
- [ ] Is the binary in release.yaml archive_specs, artifact patterns, and Dockerfile?
- [ ] Did GHCR image identity change? (Should be preserved unless deliberate)
- [ ] Are legacy modes (`--http`, `--mcp-stdio`) still tested?

**Pitfall Detection**:
- Non-shipped native binary: After converting a previously-bridged server, grep for the new binary name in:
  - `.github/workflows/release.yaml` (archive_specs, artifact patterns, docker bin loop)
  - `docker/harnx.Dockerfile` (COPY lines)
  - If missing, add before merging

## Related Issues

- **Issue**: [#1224](https://github.com/dobesv/harnx/issues/1224) — Native tool-server migration (umbrella)
- **Related Solution**: `integration-issues/native-nats-toolset-conversion-pattern-2026-07-31.md` — S1 fs conversion pattern (foundational)
- **Template**: `crates/harnx-plans-tools/src/toolset.rs` — Stateful toolset with retention supervision
- **Template**: `crates/harnx-bash-tools/src/toolset.rs` — CLI-root-seeded toolset
