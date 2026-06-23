---
title: "Renewable CAS KV Lease with Fencing for NATS HA Workers"
date: 2026-06-21
category: "integration-issues"
problem_type: integration_issue
component: "nats-worker-lease"
root_cause: "async-nats 0.42 lacks update_with_ttl; bucket-wide max_age breaks CAS re-acquire"
resolution_type: code_fix
severity: critical
tags:
  - nats
  - distributed-systems
  - lease
  - fencing
  - high-availability
  - jetstream
  - kv
plan_ref: "harnx-nats-ha"
last_updated: 2026-06-21
---

# Solution: Distributed HA Lease & Fencing

## Problem

In a distributed environment, ensuring exactly one worker executes a session's agent loop is critical. Without proper lease mechanics, multiple workers could process the same session, causing state corruption, duplicate tool execution, and split-brain scenarios.

## Symptoms

- Multiple workers processing the same session after failover
- `WrongLastSequence` errors during lease renewal being misdiagnosed
- Lease holders continuing execution after losing the lease
- Ghost sequence errors when re-acquiring leases after expiration
- Workers bypassing token/TLS auth when connecting to NATS
- Test hangs when reading bounded history from JetStream consumers
- Silent message drops due to content-derived Nats-Msg-Id deduplication

## Investigation Steps

1. **Library Gap Discovery**: Searched async-nats 0.42 source for `update_with_ttl` — doesn't exist. Only `update(key, value, revision)` without TTL override.

2. **Bucket max_age Rejection**: Tried bucket-wide `max_age` as alternative. Discovered CAS "ghost sequence" bug: when a key expires and a new worker attempts `create`, the server returns stale sequence numbers from pre-expiry state, breaking CAS guarantees on re-acquire.

3. **Low-Level Renewal Path**: Found that JetStream publishes to `$KV.<bucket>.<key>` with headers `Nats-Expected-Last-Subject-Sequence` (CAS) and `Nats-TTL` work for TTL-refreshing updates. This requires `server_2_11` feature flag.

4. **Limit_markers Requirement**: Discovered that per-key TTL requires `kv::Config.limit_markers: Some(tombstone_ttl)` on bucket creation. Without this, expired keys don't produce tombstones, and subsequent `create` calls fail to read the prior state correctly.

5. **Worker Auth Bypass**: Caught during review — `run_agent_loop_with_nats_inner` extracted only `server.url` and called bare `async_nats::connect(url)`, silently dropping token/TLS config.

6. **Consumer Read Hang**: Tried pull consumer `.messages()` for bounded history read — blocks forever (live subscription). Tried `.fetch()` — flaky under current-thread runtime, returns partial/zero batches.

7. **Nats-Msg-Id Dedup Trap**: Used content-hash for message IDs. Duplicate content (e.g., two identical user messages) caused silent drops within the duplicate window.

## Root Cause

The root causes are multi-layered:

1. **No TTL-refresh primitive**: async-nats 0.42 provides `create_with_ttl` for initial acquisition but has no high-level way to refresh TTL while preserving CAS guarantees. Plain `update()` drops the per-key TTL entirely.

2. **Bucket max_age Cas bug**: Using bucket-wide `max_age` instead of per-key TTL causes "ghost sequence" errors on re-acquire after expiration.

3. **Auth bypass in worker**: Bare `async_nats::connect(url)` skips all token/TLS configuration silently.

4. **Consumer vs. direct get**: Consumer `.messages()` is a live subscription that never terminates; `.fetch()` is runtime-flavor-sensitive.

5. **Dedup semantics**: Nats-Msg-Id deduplication is per-subject within duplicate_window; content-derived IDs cause silent drops of legitimate duplicate-content entries.

## Solution

### Lease Bucket Configuration

```rust
// Cargo.toml: add server_2_11 feature
async-nats = { version = "0.42", features = ["server_2_10", "server_2_11", "ring"] }

// Bucket creation with limit_markers
jetstream.create_key_value(kv::Config {
    bucket: "harnx_leases",
    history: 1,
    limit_markers: Some(Duration::from_secs(3600)), // tombstone_ttl
    num_replicas: 1, // configurable; use 3 for production HA
    storage: StorageType::File,
    ..Default::default()
}).await?
```

### Acquisition

```rust
// Per-key TTL on acquire; revision = fence token
let revision = bucket.create_with_ttl(
    key,
    record_bytes,
    Duration::from_secs(30)
).await?; // Returns KV revision = fence token
```

### Renewal (Low-Level)

```rust
// async-nats 0.42 has NO update_with_ttl; use low-level publish
let subject = format!("$KV.{}.{}", bucket.name, key);
let mut headers = async_nats::HeaderMap::new();
headers.insert(NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, expected_revision.to_string());
headers.insert(NATS_MESSAGE_TTL, ttl.as_secs().to_string());

let ack = jetstream
    .publish_with_headers(subject, headers, payload.into())
    .await?
    .await?;

match ack {
    Ok(ack) => {
        // Update local fence token
        state.fence_token.store(ack.sequence, Ordering::SeqCst);
    }
    Err(error) if error.kind() == PublishErrorKind::WrongLastSequence => {
        // CAS failure => lease lost => ABORT immediately
        bail!("Lost lease CAS for key '{}'", key);
    }
    // ...
}
```

### Worker Connection (Auth-Safe)

```rust
// WRONG: bypasses auth
let client = async_nats::connect(&server.url).await?;

// CORRECT: goes through config-driven path
let jetstream = config.nats_jetstream(cluster_key).await?;
// Uses build_nats_connect_options internally, applies token/TLS
```

### Historical Read (No Hang)

```rust
// WRONG: consumer.messages() blocks forever
let mut messages = consumer.messages().await?;
while let Some(msg) = messages.next().await { ... } // NEVER terminates

// CORRECT: direct get per sequence, bounded by stream info
let stream_info = stream.info().await?;
for seq in first_seq..=last_seq {
    let raw = timeout(READ_TIMEOUT, stream.get_raw_message(seq)).await?;
    // Process entry
}
```

### Message ID (Unique Per Append)

```rust
// WRONG: content-derived
let msg_id = format!("{:x}", md5::compute(&payload));

// CORRECT: unique per append
let msg_id = uuid::Uuid::new_v4().to_string();
```

## Why This Works

1. **Per-key TTL + limit_markers**: Each lease key has its own TTL. On expiration, a tombstone is written, which subsequent `create` calls can read correctly for CAS semantics.

2. **Low-level renewal preserves TTL**: Publishing to `$KV.<bucket>.<key>` with `Nats-TTL` header refreshes the TTL while `Nats-Expected-Last-Subject-Sequence` maintains CAS. Failure means another worker took over.

3. **Fence token as revision**: The KV revision number is monotonically increasing and tied to the CAS operation. Embedding it in log entries allows resume-time detection of split-brain.

4. **Config-driven connection**: All worker connections go through `Config::nats_jetstream()`, which applies token/TLS via `build_nats_connect_options`. No silent auth bypass.

5. **Direct get terminates**: `get_raw_message(seq)` returns immediately (or errors). No live subscription, no blocking, works on any runtime flavor.

6. **Unique msg_id prevents drops**: UUID-based IDs ensure every append is unique, even if content duplicates. Dedup only catches true retries.

## Prevention Strategies

### Test Cases

- Lease contention test: 2 workers, 1 activation, exactly 1 acquires
- Failover test: holder stops, new worker acquires within TTL window
- Resume abort test: tail fence > held revision => abort
- Auth bypass test: worker connects to token-auth server with config; bare connect fails
- Historical read test: bounded read completes without hang
- Dedup test: identical content entries both live (different UUIDs)

### Code Review Checklist

- [ ] Bucket uses `limit_markers: Some(tombstone_ttl)`?
- [ ] Renewal uses `publish_with_headers` with both CAS and TTL headers?
- [ ] Worker connects via `Config::nats_jetstream()`, NOT bare `async_nats::connect(url)`?
- [ ] Historical reads use `get_raw_message(seq)` in loop, not consumer?
- [ ] Nats-Msg-Id is UUID, not content-derived?
- [ ] `server_2_11` feature enabled for async-nats?

### Integration Test Convention

Spawn real `nats-server` binary (gated on presence, skip if absent):

```rust
pub async fn spawn_nats_server() -> Result<Option<NatsServerHandle>> {
    let bin = env::var("NATS_SERVER_BIN")
        .unwrap_or_else(|_| "nats-server".to_string());
    
    // Skip if binary not found
    if which::which(&bin).is_err() {
        eprintln!(" Skipping: nats-server binary not found");
        return Ok(None);
    }
    
    let tempdir = tempfile::tempdir()?;
    let port = get_free_port()?;
    let mut child = Command::new(&bin)
        .args(["-js", "-sd", tempdir.path().to_str().unwrap(), "-p", &port.to_string()])
        .spawn()?;
    
    // Wait for readiness
    wait_for_nats_ready(port).await?;
    
    Ok(Some(NatsServerHandle { child, tempdir }))
}
```

## Split-Brain Protection: Fencing

The fence token (KV revision) is embedded in every worker-originated log entry (e.g., `AssistantMessage`, `ToolCalls`, `Cancel`):

1. **Gated Appends**: Every append to the JetStream session log is checked against `lease.is_held()`.
2. **Resume Validation**: When a worker resumes a session, it reads the tail of the durable log. If it finds a fence token *greater* than its own held revision, another worker has taken over — abort immediately.

```rust
// Before agent loop
let tail_fence = max_fence_token_in_tail(&entries);
if tail_fence > lease.fence_token() {
    bail!("Split-brain detected: tail fence {} > held {}", tail_fence, lease.fence_token());
}
```

## Event Contract

### Durable vs. Advisory

- **Durable Log**: Authoritative history (User messages, final Assistant messages, Tool results, `Cancel`). Queued user messages that arrive while a turn is running are ordinary `Message` entries folded into the next turn. Stored in JetStream log streams (`SESSION_<id>`).
- **Advisory Events**: Lossy, real-time previews (Streaming chunks, thought segments, status updates). Published to core NATS fan-out subject `sessions.{id}.events`.

### Attach Logic

Clients subscribe to the advisory subject *before* loading durable history. Advisory envelopes carry `after_seq` (the durable seq they follow). Client renders advisory only when `after_seq >= last_applied_durable_seq` — gap-free and duplicate-free transition from history to live streaming.

```rust
pub struct AdvisoryEnvelope {
    pub after_seq: u64,  // Durable log seq this advisory follows
    pub event: AgentEvent,
}

// Client-side filter
fn should_render(&self, envelope: &AdvisoryEnvelope) -> bool {
    envelope.after_seq >= self.last_applied_durable_seq
}
```

## Safety: Resume Heuristic

If a worker crashes mid-tool-call:

- **Idempotent tools** (per MCP `idempotent_hint` or `read_only_hint`): Safe to re-run. Worker re-executes the tool.
- **Non-idempotent tools**: Synthesize an interrupt-error result (`"tool response lost (session was interrupted before results were persisted)"`), allowing the user/operator to see the response was lost and decide how to proceed.

## Related Issues

- **Plan notes**: `cf699445` (lease TTL decision), `4f7c58ba` (blocking ambiguity), `10c63569` (lease+dispatch), `84ec130b` (NATS gotchas), `1ead17b4` (auth-bypass fix), `23777a0a` (test convention)
- **Code**: `crates/harnx-runtime/src/nats_lease.rs`, `nats_worker.rs`, `nats_session_log.rs`
- **Operator guide**: `docs/nats-ha.md`
