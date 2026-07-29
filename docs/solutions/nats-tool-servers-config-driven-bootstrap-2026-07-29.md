---
title: "Config-Driven NATS Tool Servers — Phase 2b Bootstrap"
date: 2026-07-29
category: "integration-issues"
problem_type: integration_issue
component: "harnx-runtime/harnx-config"
root_cause: "tool-server bootstrap relied on hardcoded Rust constants rather than declarative YAML configuration"
resolution_type: code_fix
severity: medium
tags:
  - nats
  - tool-servers
  - config
  - deduplication
plan_ref: "nats-tool-servers-phase2b-bootstrap"
last_updated: 2026-07-29
---

# Solution: Config-Driven NATS Tool Servers

## Problem

Phase 2a introduced instance-scoped Core NATS tool invocation but relied on a hardcoded `LOCAL_BOOTSTRAP_SERVERS` Rust constant to spawn `harnx-time-server`. Hardcoding prevented users and packages from declaring custom NATS tool servers.

## Solution

Phase 2b generalizes tool-server bootstrap into declarative configuration:

1. **Declarative discovery (`tool_servers/*.yaml`)**: Tool servers are defined in YAML files under `tool_servers/` within the user config directory and package directories (`packages/coding/tool_servers/`, `packages/pantheon/tool_servers/`, `example_config/tool_servers/`, `demos/config/tool_servers/`).

2. **Lazy-spawn filtering**: The local worker evaluates candidate tool servers against the active agent's `use_tools` pattern using `selector_could_match_server`, matching the existing MCP lazy-spawn model. Absent `use_tools` spawns no servers.

3. **Resilient process supervision**: Missing binaries or crashing tool servers emit a UI warning (`AgentEvent::Notice(Warning)`) while worker execution continues.

4. **Time tool migration**: The hardcoded `LOCAL_BOOTSTRAP_SERVERS` constant and `BootstrapServer` struct were removed. `time` now ships as `harnx-time-server` in `tool_servers/time.yaml`. The `HARNX_TIME_SERVER_BIN` environment variable remains as a per-server binary override seam for integration tests.

## Key Learning: KV-Key Name Collision Requires Pre-Spawn Deduplication

### The Collision Problem

`server.name` (derived from the config file stem, e.g., `time` from `time.yaml`) is also the NATS KV registration key stem: `{instance_id}.{name}`. When two configs share the same `server.name` — for example, a user-level `~/.config/harnx/tool_servers/time.yaml` alongside a package-provided `packages/coding/tool_servers/time.yaml`, or two packages each providing `tool_servers/time.yaml` — both would spawn processes registering under the identical KV key `{instance_id}.time`.

When either process exits, its monitor calls `remove_registration`, deleting the shared KV entry while the other process still runs. The result: registration flap, tool unavailability, and duplicate request processing.

### The Fix: First-Wins Deduplication After Filtering

`tool_servers_matching_use_tools` (daemon.rs:271-292) now deduplicates matching configs by `server.name`:

```rust
let mut seen_names = HashSet::new();
servers
    .iter()
    .filter(|server| {
        if !server.enabled { return false; }
        // ... selector matching ...
        matches && seen_names.insert(server.name.clone())
    })
    .cloned()
    .collect()
```

Critical ordering: `seen_names.insert` runs **after** the `enabled` check and selector match (`matches &&`). Disabled or non-matching configs do **not** consume the name slot, so a later matching config with the same name can still spawn.

Since the loader appends user configs first (`loader_split.rs:100-105`), then package configs (`servers_split.rs:53-58`), first-wins semantics give user configuration precedence over package defaults.

### Why This Matters for Config-Driven Systems

Any config system where multiple directories contribute entries that share a common key namespace (here, NATS KV key stems) must deduplicate before side effects. Without dedup at the spawn layer, duplicate processes can corrupt shared state even if individual processes are well-behaved.

### Follow-up Improvement: Consider Collision Logging

Current first-wins deduplication is silent — no warning or info log when a collision is detected and resolved. This could mask config-authoring mistakes (e.g., an operator unaware their user config shadows a package config, or two packages accidentally using the same server name for different binaries). An INFO-level log line when `seen_names.insert` returns `false` would surface these cases without blocking startup.

## Verification

- `cargo test -p harnx-runtime` passes (540 tests), covering supervisor, config loading, deduplication, and lazy-spawn behavior.
- Integration tests confirm missing server binaries trigger soft warnings without halting worker startup.
- Regression test `tool_server_filter_deduplicates_names_first_wins` verifies user-config takes precedence over package config for duplicate names.
