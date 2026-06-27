---
title: "Remote agent use_tools parity: whitelist filtering, forward sanitization, and thin-client completion"
date: 2026-06-26
category: "integration-issues"
problem_type: integration_issue
component: "agent-delegation"
root_cause: "whitelist-bypass + bare-ref selector dropped tools + thin-client never completed on non-streaming workers"
resolution_type: code_fix
severity: high
tags:
  - remote-agents
  - use-tools
  - acp
  - nats
  - thin-client
  - selector-filtering
  - sanitization
plan_ref: "harnx-913-remote-agent-tools"
---

## Problem

Remote agents whitelisted in `use_tools` did not produce the full session tool family, and ACP delegation tools leaked regardless of selectors. Additionally, thin-client NATS turns never completed for non-streaming workers, and stale prior-turn responses could leak on abnormal exit.

## Symptoms

- `use_tools: [metis@local]` yielded only `metis__at__local_session_handoff`, missing the ACP delegation tools (`_session_new`, `_session_prompt`, `_session_load`, `_session_cancel`)
- ACP tools appeared unconditionally regardless of `use_tools` whitelist (bypass bug)
- ThinClientSession::run_turn blocked indefinitely when the worker persisted a final assistant message without streaming advisories
- Stale prior-turn assistant replies could be returned as the current turn response

## Investigation Steps

1. Traced `Config::tool_declarations_for_use_tools` — ACP path called `manager.get_all_tools_blocking()` unconditionally, ignoring selectors (MCP path already filtered correctly)
2. Analyzed `AcpManager::get_tools_for_selectors_blocking` — two-stage filter; stage-1 prefiler passed bare refs, but stage-2 `matches_tool_glob` required exact equality, dropping whole family
3. Investigated thin-client hang — `run_turn` only checked completion inside advisory-event arm; non-streaming workers emit NO advisories, so interval poll never ran
4. Traced `is_turn_complete` — treated any `TurnStatus::Idle` as complete, but a freshly-appended user message also reconstructs to Idle (no assistant yet), causing immediate break with `response=None`
5. Found post-loop fallback `extract_final_response` returned LAST assistant in whole log, not gated by current turn seq

## Root Causes

### ACP Whitelist Bypass

`Config::tool_declarations_for_use_tools` called `acp_manager.get_all_tools_blocking()` unconditionally inside the `use_tools` block. MCP path correctly used `get_tools_for_selectors_blocking` for non-wildcard selectors.

### Bare-Ref Selector Filter

`AcpManager::get_tools_for_selectors_blocking` stage-2 used `matches_tool_glob(selector, tool_name)`. For bare ref `metis__at__local` vs tool `metis__at__local_session_new`, literal glob requires exact equality and returns false. Whole family dropped.

### Thin-Client Completion

`run_turn` lacked independent interval poll for completion. Advisory-only check meant non-streaming workers (persist final message without events) never triggered completion.

### Early Return on Idle

`is_turn_complete` returned true for any `TurnStatus::Idle`. User-only log (after append, before worker reply) is valid Idle state. Turn ended before assistant barrier existed.

### Stale Response Leak

`extract_final_response` searched entire log for last assistant message. On abnormal exit (stream closed without assistant), returned stale prior-turn reply.

## Solutions

### 1. ACP Selector Filtering (mirrors MCP)

**File:** `crates/harnx-acp/src/manager.rs:154-188`

```rust
pub fn get_tools_for_selectors_blocking(&self, selectors: &[String]) -> Vec<ToolDeclaration> {
    if selectors.iter().any(|selector| selector.trim() == "*") {
        return self.get_all_tools_blocking();
    }

    let trimmed_selectors: Vec<&str> = selectors.iter().map(|selector| selector.trim()).collect();
    let clients = self.clients.read();
    let mut tools = Vec::new();

    for (name, client) in clients.iter().filter(|(name, _)| {
        trimmed_selectors
            .iter()
            .any(|selector| selector_could_match_server(selector, name))
    }) {
        let family = generate_acp_tools(name, client.description());
        let whole_family_selected = trimmed_selectors.iter().any(|selector| {
            selector_could_match_server(selector, name)
                && !family.iter().any(|tool| selector == &tool.name)
        });

        if whole_family_selected {
            tools.extend(family);
        } else {
            tools.extend(family.into_iter().filter(|tool| {
                trimmed_selectors
                    .iter()
                    .any(|selector| matches_tool_glob(selector, &tool.name))
            }));
        }
    }

    tools.sort_by(|left, right| left.name.cmp(&right.name));
    tools
}
```

Rule: bare/forward-sanitized server-ref selector (e.g., `metis@local` → `metis__at__local`) selects WHOLE family; specific tool name stays narrow. Mirrors MCP's `get_tools_for_selectors`.

### 2. Forward-Only Sanitization

**File:** `crates/harnx-core/src/package_namespace.rs:11-25`

```rust
pub fn sanitize_for_tool_name(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        if ch == '/' {
            result.push_str("__");
        } else if ch == '@' {
            result.push_str("__at__");
        } else if ch.is_alphanumeric() || ch == '-' || ch == '_' {
            result.push(ch);
        } else {
            result.push('_');
        }
    }
    result
}
```

Design rule: FORWARD-only, lossy. Dispatch NEVER reverses:
- Handoff: `handoff_targets` map (sanitized display_name → raw `agent@cluster`)
- ACP: `find_client_for_tool` matches sanitized keys

To match `agent@cluster` against generated tool names, FORWARD-sanitize the selector. NEVER reverse.

### 3. Thin-Client Completion Interval

**File:** `crates/harnx-runtime/src/nats_client_session.rs:269-280`

```rust
// Poll durable completion independently so non-streaming workers
// that persist a final assistant message without an advisory still
// complete the turn promptly.
_ = completion_interval.tick() => {
    if let Ok(entries) = self.load_durable_entries().await {
        if self.is_turn_complete(&entries, user_msg_seq) {
            final_response = Self::extract_final_response(&entries, user_msg_seq);
            turn_complete = true;
            break;
        }
    }
}
```

Independent tokio interval poll runs regardless of advisory stream. Non-streaming workers now complete promptly.

### 4. Seq-Gated Completion Check

**File:** `crates/harnx-runtime/src/nats_client_session.rs:406-417`

```rust
fn is_turn_complete(&self, entries: &[(u64, SessionLogEntry)], user_msg_seq: u64) -> bool {
    let has_assistant_after_user = entries.iter().any(|(seq, entry)| {
        *seq > user_msg_seq
            && matches!(entry, SessionLogEntry::Message { role, .. } if role.is_assistant())
    });

    has_assistant_after_user
        || matches!(
            reconstruct_state_from_nats(entries).turn_status,
            TurnStatus::InFlightCancelled
        )
}
```

Requires assistant message with `seq > user_msg_seq` OR `InFlightCancelled`. Fresh `Idle` state (user-only) no longer triggers early completion.

### 5. Seq-Gated Response Extraction

**File:** `crates/harnx-runtime/src/nats_client_session.rs:420-450`

```rust
fn extract_final_response(
    entries: &[(u64, SessionLogEntry)],
    user_msg_seq: u64,
) -> Option<String> {
    let turn_entries: Vec<_> = entries
        .iter()
        .filter(|(seq, _)| *seq > user_msg_seq)
        .cloned()
        .collect();

    let effective_entries = match apply_log_mutations_nats(&turn_entries) {
        Ok(entries) => entries,
        Err(err) => {
            log::warn!("failed to apply NATS log mutations: {err}");
            return None;
        }
    };

    for (_, entry) in effective_entries.iter().rev() {
        if let SessionLogEntry::Message { role, content, .. } = entry {
            if role.is_assistant() {
                return Some(content.to_text());
            }
        }
    }
    None
}
```

All three call sites (interval poll, advisory completion, fallback) gated by `user_msg_seq`. No stale prior-turn replies on abnormal exit.

### 6. Description Threading

**File:** `crates/harnx-runtime/src/config/loader_split.rs:205-216`

Remote → `RemoteAgentEntry.description`, local → `AgentConfig.description`, fallback `None`:

```rust
let description = remote_descriptions
    .get(&agent_name)
    .cloned()
    .flatten()
    .or_else(|| local_descriptions.get(&agent_name).cloned().flatten());
acp_servers.push(AcpServerConfig {
    name: agent_name.clone(),
    command: command.clone(),
    args: vec![agent_name.clone()],
    description,
    // ...
});
```

`generate_acp_tools` and handoff declarations consume `AcpServerConfig.description` with format `"<hint> — <description>"`.

## Why This Works

- ACP selector filtering mirrors MCP's established pattern; bare refs select families, specific names stay narrow
- Forward-only sanitization avoids the unreliable decode problem (names can contain `_` or `__`)
- Independent interval poll decouples completion from advisory stream, handles both streaming and non-streaming workers
- Seq-gating ensures completion/response only considers the current turn's assistant messages

## Prevention Strategies

**Test Cases:**
- ACP whitelist regression: assert unselected agents do NOT appear in `tool_declarations_for_use_tools`
- Bare-ref selector: assert `metis@local` yields full 5-tool family
- Specific selector: assert `metis__at__local_session_prompt` stays ACP-narrow
- Thin-client non-streaming: use in-process worker daemon + `ThinClientSession::run_turn`
- Resumed session: seed prior turn, assert new reply returned (not stale)

**Best Practices:**
- Never reverse-sanitize `__at__` → `@`; forward-map all selectors
- Thin-client handlers MUST poll completion via interval, not only event stream
- Completion checks MUST gate by `user_msg_seq` to avoid false positives on fresh logs

**Testing Recipe:**
```rust
let config = Config::load_from_file(&config_path);
config.reinit_managers_for_agent(None); // <-- crucial: initializes acp_manager
let tools = config.tool_declarations_for_use_tools(Some("metis@local"), None);
```

Without `reinit_managers_for_agent`, only handoff tools appear.

## Related Issues

- [logic-errors/acp-server-extraction-delegation-fix-2026-06-11.md](../logic-errors/acp-server-extraction-delegation-fix-2026-06-11.md) — `AcpServerConfig.name` dual role: spawn target vs display name
- [logic-errors/package-relative-agent-delegation-2026-06-09.md](../logic-errors/package-relative-agent-delegation-2026-06-09.md) — handoff display name chain
- [static-remote-agent-catalog-2026-06-26.md](static-remote-agent-catalog-2026-06-26.md) — `RemoteAgentEntry` from NATS cluster configs
- GitHub [#913](https://github.com/dobesv/harnx/issues/913)
