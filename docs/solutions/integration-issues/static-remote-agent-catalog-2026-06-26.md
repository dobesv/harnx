---
title: "Static remote agent catalog from NATS cluster config"
date: 2026-06-26
category: "integration-issues"
problem_type: integration_issue
component: "agent-discovery"
root_cause: "enumeration functions have no Config parameter; needed static path to nats_servers config"
resolution_type: code_fix
severity: medium
tags:
  - agent-discovery
  - nats
  - static-config
  - remote-agents
  - serde
plan_ref: "nats-static-agent-catalog"
---

## Problem

Agent enumeration functions `list_agents()` and `list_assistant_agents()` are free functions with no `Config` argument. To surface remote agents declared in NATS cluster configs (`nats_servers/<cluster>.yaml`), needed a way to read nats_servers without threading a Config instance through existing call sites.

## Symptoms

- Remote agents defined in NATS cluster YAML were invisible to `--list-agents` and shell completion
- No mechanism to declare cluster-resident agents for discovery without network calls
- Existing enumeration functions had no access point to NATS server configs

## Investigation Steps

Analyzed `list_agents()` signature — sync free fn, no Config parameter. Same for `list_assistant_agents()`. Config is loaded lazily in most paths, but these are utility functions called from completion/picker contexts.

Traced the established pattern: `Config::config_dir()` honors `HARNX_CONFIG_DIR`, which `with_test_config_dir` sets. The nats_servers directory lives at `Config::config_dir().join(paths::NATS_SERVERS_DIR_NAME)`.

Key insight: `Config::load_nats_servers_from_dir()` is a static method accepting any directory path. No Config instance needed.

## Root Cause

Enumeration functions were designed for local/package agent discovery only. Remote agents live in a different config namespace (`nats_servers/`), and the free-function signatures prevented passing Config-derived data.

## Solution

Added `RemoteAgentEntry` struct and helper function:

**Struct (nats_split.rs):**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteAgentEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub role: AgentRole,
}
```

**NatsServerConfig field:**

```rust
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub agents: Vec<RemoteAgentEntry>,
```

**Helper function (agent.rs):**

```rust
fn list_remote_agent_names(role_filter: Option<AgentRole>) -> Vec<String> {
    let nats_servers_dir = Config::config_dir().join(paths::NATS_SERVERS_DIR_NAME);
    let Ok(servers) = Config::load_nats_servers_from_dir(&nats_servers_dir) else {
        return vec![];
    };

    let mut names = vec![];
    for server in servers {
        let cluster_name = server.name;
        for agent in server.agents {
            if role_filter.as_ref().is_some_and(|role| agent.role != *role) {
                continue;
            }
            names.push(format!("{}@{}", agent.name, cluster_name));
        }
    }
    names
}
```

**Wiring:**

```rust
// list_agents() - all roles
output.extend(list_remote_agent_names(None));

// list_assistant_agents() - assistant only
output.extend(list_remote_agent_names(Some(AgentRole::Assistant)));
```

## Why This Works

**Static on-disk read pattern:** `Config::config_dir()` returns the test-configurable path. `load_nats_servers_from_dir()` parses YAML from that directory. No Config instance, no network I/O, pure filesystem read.

**Role filtering:** `list_agents()` passes `None` (all remote agents). `list_assistant_agents()` passes `Some(Assistant)` (assistant-only).

**Naming:** Format `name@cluster` ensures remote agents never collide with local/package agents.

**Merge pattern:** Extend existing Vec, then `sort()` + `dedup()` — matches prior art in package-agent-discovery.

## Prevention Strategies

**Testing seed pattern:**

```rust
// In test, seed under with_test_config_dir temp root:
// <config_dir>/nats_servers/<cluster>.yaml
// No config.yaml needed — code reads nats_servers/ directly
```

**Struct literal hygiene:** Adding a field to a serde struct with many manual struct-literal constructions in tests requires updating each literal:

```rust
// Always add:
agents: vec![],
```

Consider `#[serde(default)]` on new fields to minimize breakage, but Rust struct construction still requires all fields when using literals.

**Known caveat:** Single malformed cluster YAML makes `load_nats_servers_from_dir` return `Err`, which maps to `vec![]` — all remote entries silently disappear. Local agents unaffected. Deferred follow-up: log warning instead of swallowing.

**Sync I/O in async:** `list_assistant_agents()` is async but calls `list_remote_agent_names()` which performs sync file reads. Accepted for small config files, consistent with existing pattern. Document as known minor smell.

## Related Issues

- **agent-role-filtering-completions-2026-05-04.md** — Original `AgentRole` enum and `list_assistant_agents()` pattern
- **package-agent-discovery-paths-2026-05-16.md** — Extend→sort→dedup merge pattern, package scanning
- **nats-ha-lease.md** — NATS cluster config structure
- **Plan note `142c5b08`** — Helper design decision
- **Plan note `ac1f5fa0`** — Static config read path analysis
