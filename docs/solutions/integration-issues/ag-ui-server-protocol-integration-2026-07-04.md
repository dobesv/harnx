---
title: "AG-UI protocol server integration in harnx-serve"
date: 2026-07-04
category: "integration-issues"
problem_type: integration_issue
component: "harnx-serve, harnx-runtime, harnx-core"
root_cause: "ag-ui-core v0.1.0 UUID-newtype IDs incompatible with harnx session IDs; session storage agent-scoped not flat; resume reconciliation requires single-fork pattern"
resolution_type: code_fix
severity: high
tags:
  - ag-ui
  - sse
  - session-management
  - uuid
  - content-negotiation
  - run_agent_loop
plan_ref: "ag-ui-server-harnx-serve"
---

## Problem

AG-UI (Agent User Interaction Protocol) server support was added to `harnx-serve` as a content-negotiated REST tree under `/v1/agents`, backed by `harnx_runtime::run_agent_loop`. Multiple non-obvious integration issues arose from:

1. **ag-ui-core v0.1.0 ID types are UUID newtypes** — `ThreadId`, `RunId`, `MessageId` wrap `Uuid` with `FromStr` requiring valid UUID. harnx session IDs are 6-char base64/UUIDv7, NOT always UUID-parseable.
2. **Session storage is agent-scoped** — `config_paths::sessions_dir(Some(agent))` = `agent_data_dir(agent)/sessions`, NOT flat `$HARNX_STATE_DIR/sessions`.
3. **Resume reconciliation required single-fork architecture** — two-fork model caused duplicated prefix messages.
4. **Durable message IDs required threading through replay paths** — two code paths dropped IDs during reconstruction.

## Symptoms

```
# Session resolution failures:
ThreadId::from_str(session_id) => parse error for non-UUID session names

# Agent-scoped directory mismatch:
GET /v1/agents/:agent/sessions => [] (looked in flat dir)
GET /v1/agents/:agent/sessions/:session => 404 (wrong path)

# Resume duplication:
Session log after resume: [user1, user1, assistant1] (duplicated prefix)

# Dropped message IDs:
Compaction test: assistant turn IDs change after compaction
Old logs: load correctly but IDs lost on replay

# Dry-run confusion:
Integration tests: no sessions persisted, empty history
```

## Root Cause Analysis

### 1. ag-ui-core UUID ID Constraint

ag-ui-core types:
```rust
pub struct ThreadId(Uuid);  // FromStr = Uuid::parse_str, errors on non-UUID
pub struct MessageId(Uuid); // same
pub struct ToolCallId(String); // plain String — different!
```

harnx session IDs from `utils/session_name.rs`:
- UUIDv7 format, OR
- 6-char base64url encoded timestamp

**Trap**: Blindly parsing `:session` path as `ThreadId::from_str()` fails for non-UUID sessions.

### 2. Agent-Scoped Session Directories

Run handler:
```rust
// ag_ui.rs run path
let mut cfg = base_config.fork_session_scope();
cfg.use_agent_by_name(agent)?;
cfg.use_session(Some(session))?;  // writes to agent-scoped dir
```

List/history handlers (initially):
```rust
// lib.rs — WRONG
self.config.list_sessions_with_meta()  // scans flat dir
self.config.session_file(session)       // flat dir path
```

`config_paths::sessions_dir(agent_opt)`:
```rust
match agent_opt {
    Some(agent) => agent_data_dir(agent)/sessions,
    None => state_path("sessions"),  // flat
}
```

Result: run writes to `$DATA/agents/<agent>/sessions/<session>.yaml`, list reads `$STATE/sessions/`, finds nothing.

### 3. Two-Fork Resume Pattern (The Duplication Trap)

**Wrong** (two forks):
```rust
// Fork #1: reconcile
let fork1 = base_config.fork_session_scope();
fork1.use_agent_by_name(agent)?;
fork1.use_session(Some(session))?;  // loads history
let new_msgs = reconcile(&fork1.read().session.messages, &client_msgs);
// Maybe persist user here...

// Fork #2: run
let fork2 = base_config.fork_session_scope();  // PROBLEM!
fork2.use_agent_by_name(agent)?;
fork2.use_session(Some(session))?;  // reloads history again
run_agent_loop(&fork2, input).await;  // begin_turn appends user+assistant AGAIN
```

Result: `[user1, user1, assistant1]` — user persisted in fork1, then fork2 reloads and `begin_turn` appends again.

### 4. Dropped Message IDs in Replay Paths

`SessionLogEntry::Message.id: Option<String>` added for P1.5 durable IDs.

Two replay paths missed the ID:

1. `session_reconstruct.rs::cloned_message()` — copied fields, didn't thread `id` through.
2. `runtime/session.rs::replay_log_entries_for_external()` — the `Message` construction dropped `id`.

Compaction test caught #2: old `id: Some(uuid)` entries returned `id: None` after replay.

## Solution

### 1. Session Resolution: Raw String + ThreadId Derivation

```rust
// ag_ui.rs
pub fn derive_thread_id(session_id: &str) -> ThreadId {
    if let Ok(uuid) = Uuid::parse_str(session_id) {
        ThreadId::from(uuid)
    } else {
        // Stable UUIDv5 for non-UUID sessions
        Uuid::new_v5(&Uuid::NAMESPACE_URL, session_id.as_bytes()).into()
    }
}

// Resolve session by RAW string, never parse as UUID
let session = Session::load(&config, session_id, &path)?;
let thread_id = derive_thread_id(session_id);  // for wire events
```

### 2. Agent-Scoped Config Helper

```rust
// lib.rs
fn agent_scoped_config(config: &Config, agent: &str) -> Result<Config> {
    let mut cfg = config.fork_prompt_config();  // clones
    cfg.use_agent_by_name(agent)?;
    Ok(cfg)
}

// Both list and history now use:
let scoped = agent_scoped_config(&self.config, agent)?;
scoped.list_sessions_with_meta()  // scans agent dir
scoped.session_file(session)       // agent-scoped path
```

### 3. Single-Fork Resume Architecture

```rust
// ag_ui.rs — correct pattern
let prompt_config: GlobalConfig = Arc::new(RwLock::new(
    base_config.fork_session_scope()
));

// ONE use_session call
prompt_config.write().use_agent_by_name(agent)?;
prompt_config.write().use_session(Some(session))?;
let persisted = prompt_config.read().session.as_ref()
    .map(|s| s.messages.clone()).unwrap_or_default();

// Reconcile
let new_msgs = reconcile_new_messages(&persisted, &input.messages);
if new_msgs.is_empty() {
    return Err(AgUiError::BadRequest("no new messages".into()));  // 400
}
if new_msgs.len() > 1 {
    return Err(AgUiError::BadRequest(
        "single new message per run in P1".into()
    ));  // 400, no silent drop
}

// Wire message ID BEFORE building input
let message_id = MessageId::random();
input.set_preferred_assistant_message_id(message_id.to_string());

// Run on SAME fork — begin_turn appends user+assistant exactly once
run_agent_loop(&loop_ctx, input).await;
```

**Invariants tested**:
- First run `[user1]` → `[user1, assistant1]` (no duplicate)
- Exact resend `[user1, assistant1]` (0 new) → 400, session unchanged
- Normal resume `[user1, assistant1, user2]` → grows by `[user2, assistant2]` only
- Multi-new `[.., user2, user3]` → 400 (explicit scope limit, no silent loss)

### 4. Thread Message IDs Through All Paths

```rust
// session.rs — append paths now write Some(uuid)
pub fn append_message_entries(...) {
    let id = msg.id.clone().unwrap_or_else(|| persisted_message_id());
    // ... write SessionLogEntry::Message { id: Some(id), ... }
}

// session_reconstruct.rs — preserve id
fn cloned_message(entry: &SessionLogEntry) -> Option<Message> {
    match entry {
        SessionLogEntry::Message { id, role, content, .. } => {
            Some(Message::new(...).with_id(id.clone()))  // thread through
        }
        // ...
    }
}

// runtime/session.rs — replay path
fn replay_log_entries_for_external(...) {
    // Was: Message::new(role, content)
    // Now: message.with_id(entry.id.clone())
}

// relog preserves across compaction
fn relog_message(session: &mut Session, msg: &Message) -> usize {
    // msg.id cloned to new entry
}
```

**Mandatory tests**:
```rust
#[test]
fn test_compaction_preserves_ids() {
    // Create session with IDs
    // Compact
    // Assert IDs unchanged
}

#[test]
fn load_from_log_accepts_old_message_entries_without_ids() {
    // Backward compat: id: None still loads
}
```

### 5. Input.preferred_assistant_message_id Threading

```rust
// runtime/input.rs
pub struct Input {
    // ... existing fields
    preferred_assistant_message_id: Option<String>,  // new, defaults None
}

impl Input {
    pub fn set_preferred_assistant_message_id(&mut self, id: String) {
        self.preferred_assistant_message_id = Some(id);
    }
}

// session.rs add_assistant_text
pub fn add_assistant_text(...) {
    let message_id = input.preferred_assistant_message_id
        .clone()
        .unwrap_or_else(persisted_message_id);
    // Use message_id for persistence
}
```

TUI/ACP/CLI unchanged (pass `None` → existing UUID generation).

### 6. RunAgentInput Parsing: Lenient for Envelope

ag-ui-core `RunAgentInput` fields `state`, `tools`, `context`, `forwarded_props` are non-optional with no serde default. Stock assistant-ui sends all, but minimal test bodies fail.

```rust
pub fn parse_run_input(body: &[u8]) -> Result<RunAgentInput, AgUiError> {
    // Parse into shadow struct with #[serde(default)]
    #[derive(Deserialize)]
    struct LenientInput {
        #[serde(default)]
        state: JsonValue,
        #[serde(default)]
        tools: Vec<Tool>,
        #[serde(default)]
        context: Vec<Context>,
        #[serde(default = "empty_json_object")]
        forwarded_props: JsonValue,
        messages: Vec<Message>,  // REQUIRED
        #[serde(default)]
        thread_id: Option<ThreadId>,
        #[serde(default)]
        run_id: Option<RunId>,
    }
    
    let lenient: LenientInput = serde_json::from_slice(body)?;
    if lenient.messages.is_empty() {
        return Err(AgUiError::BadRequest("messages required".into()));
    }
    
    // Convert to strict RunAgentInput
    Ok(RunAgentInput {
        state: lenient.state,
        tools: lenient.tools,
        // ...
    })
}
```

## Why This Works

1. **Session resolution by raw string** never fails due to UUID format mismatch. ThreadId for wire derived stably (UUIDv5) for permalink consistency.

2. **Agent-scoped config** ensures all handlers (run, list, history) resolve through same directory structure. Path mismatch eliminated.

3. **Single-fork pattern** ensures `begin_turn` appends user+assistant exactly once. No manual pre-persist, no second `use_session` reload.

4. **ID threading through replay** ensures compaction and session restoration preserve permalinks. `preferred_assistant_message_id` lets SSE wire ID match persisted history ID.

5. **Lenient parsing** accepts minimal bodies while requiring semantic mandatory field (`messages`).

## Prevention Strategies

### Test Cases

```rust
#[test]
fn ag_ui_run_streams_ordered_events_with_stubbed_call_fn() {
    // Assert RUN_STARTED → TEXT_MESSAGE_START → CONTENT* → END → RUN_FINISHED
    // Assert messageId consistent across START/CONTENT/END
    // Assert threadId == derive_thread_id(session)
    // Assert runId echoed
    // Drain to RUN_FINISHED
    // Assert persisted session has same IDs
}

#[test]
fn ag_ui_run_resume_same_session_persists_only_new_turn() {
    // First run: [u1, a1]
    // Second run: [u1, a1, u2]
    // Reload session, assert exactly [u1, a1, a2] (no duplicates)
}

#[test]
fn ag_ui_rejects_exact_resend() {
    // Run creates session
    // Resend same messages → 400
    // Assert session unchanged
}

#[test]
fn agent_scoped_resolution_lists_and_loads_agent_sessions() {
    // Persist session in agent-scoped dir
    // Write decoy session to flat dir
    // Assert list returns only scoped session
    // Assert history loads scoped content
}

#[test]
fn test_compaction_preserves_ids_for_preserved_suffix_messages() {
    // Create session with known IDs
    // Compact
    // Reload
    // Assert all IDs unchanged
}
```

### Best Practices

1. **Resolve sessions by raw string** — never parse `:session` path as ThreadId/RunId.
2. **Derive ThreadId deterministically** — UUIDv5 for non-UUID sessions ensures stable permalinks.
3. **Use single-fork for resume** — one `use_session` load, reconcile, run on same fork.
4. **Reject exact resend and multi-new** — explicit 400 scope limit, no silent data loss.
5. **Thread message IDs through all paths** — replay, compaction, relog must preserve.
6. **Test persistence barrier** — drain SSE to `RUN_FINISHED` before asserting session state.
7. **Use non-dry stubs for integration tests** — dry-run skips persistence.

### Code Review Checklist

- [ ] Session resolution uses raw `:session` string?
- [ ] ThreadId derived via UUIDv5 for non-UUID sessions?
- [ ] List/history use agent-scoped config?
- [ ] Resume uses single fork + single use_session?
- [ ] Exact resend → 400?
- [ ] Multi-new → 400 (no silent drop)?
- [ ] preferred_assistant_message_id threaded to persistence?
- [ ] Replay paths preserve message.id?
- [ ] Compaction test covers ID preservation?
- [ ] Backward compat test for old logs without IDs?
- [ ] Integration tests drain to RUN_FINISHED?

## Related Issues

- **Plan**: `ag-ui-server-harnx-serve` — comprehensive review notes
- **ADR**: `ce1aaeb4` — architecture decision for AG-UI over OpenAI-plus
- **ag-ui-core**: v0.1.0 dependency, UUID newtype constraints documented in `54d709e4`
- **Session ID mapping**: Decision `8df10db1` — UUID/threadId binding rules
- **Agent-scoped bug**: Problem `463b6f6c` — directory mismatch investigation
- **Single-fork pattern**: Decision `1ea8647e` — Oracle-guided architecture
- **Review blockers**: Problem `47bc5064` — Aristarchus review cycle findings
- **P1.5 durable IDs**: Feature `d15bdb3b6c` — message ID threading implementation
