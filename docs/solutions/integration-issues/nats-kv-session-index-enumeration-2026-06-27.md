---
title: "NATS KV-Backed Session Index for Remote Enumeration"
date: 2026-06-27
category: "integration-issues"
problem_type: integration_issue
component: "nats-session-index"
root_cause: "remote sessions stored in JetStream were invisible to local enumeration paths"
resolution_type: code_fix
severity: medium
tags:
  - nats
  - jetstream
  - kv
  - session-enumeration
  - distributed-systems
  - graceful-degradation
  - per-surface-error-handling
  - cas
plan_ref: "nats-remote-session-enumeration"
last_updated: 2026-06-29
---

# Solution: NATS KV-Backed Session Index for Remote Enumeration

## Problem

Remote sessions (`agent@cluster`) live in JetStream but were invisible to the TUI session picker and `--list-sessions`, which only enumerated local session directories.

## Symptoms

- TUI session picker showed only local sessions when connected to a remote agent
- `harnx --list-sessions` returned empty list for remote clusters
- Tab-completion for `.session` lacked remote session IDs
- No way to discover which remote sessions existed for an agent

## Solution

Built a NATS KV-backed session index bucket `harnx_sessions` that workers populate and clients read. The index is a denormalized copy of session metadata; the Session Header in JetStream remains canonical.

### Architecture Decision: KV Index vs Alternatives

Three approaches evaluated:

| Approach | Pros | Cons |
|----------|------|------|
| **KV Index (chosen)** | Works offline (workers need not respond), O(1)-ish enumeration, mirrors `harnx_leases`, KV-read perms only | Requires write on activation/renew, eventual consistency |
| Request-reply (`sessions.list`) | Simple | Workers must be online; enumeration fails without them |
| JetStream stream-listing (`SESSION_*`) | No extra infrastructure | Admin perms required, O(N) header reads, slow |

**Choice: KV Index** — works with offline workers, needs only KV read perms, mirrors proven `harnx_leases` pattern.

### Writer/Reader Split

**Writer (worker, lease holder only):**
1. Full upsert at header-write: writes `SessionIndexRecord` with all fields after session header created
2. Refresh on lease renew: updates only `last_activity` via CAS

**Reader (CLI/TUI/completion):**
- Calls `list_remote_sessions_with_meta(cluster)` → `list_records(bucket)` → maps to `SessionMeta`
- Returns `Ok(vec![])` on any failure (graceful degradation)

### Key Patterns

#### 1. CAS Read-Modify-Write for Partial Updates

Problem: Activation upsert (T3) can race with renew refresh (T4). Full upsert would clobber header-derived fields.

Solution: Use KV revision check:

```rust
pub async fn get_record_with_revision(
    store: &kv::Store,
    session_id: &str,
) -> Result<Option<(SessionIndexRecord, u64)>> {
    let key = session_index_key(session_id);
    match store.entry(key.clone()).await {
        Ok(Some(entry)) if matches!(entry.operation, kv::Operation::Put) => {
            let record: SessionIndexRecord = serde_json::from_slice(&entry.value)?;
            Ok(Some((record, entry.revision)))
        }
        _ => Ok(None),
    }
}

pub async fn update_record_with_revision(
    store: &kv::Store,
    session_id: &str,
    record: &SessionIndexRecord,
    revision: u64,
) -> Result<()> {
    let key = session_index_key(session_id);
    let payload = serde_json::to_vec(record)?;
    store.update(&key, payload.into(), revision).await?;
    Ok(())
}
```

Retry loop on `WrongLastRevision`:

```rust
const INDEX_REFRESH_RETRY_LIMIT: usize = 3;

for _ in 0..INDEX_REFRESH_RETRY_LIMIT {
    let (mut record, revision) = match get_record_with_revision(store, session_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()), // missing = no-op
        Err(e) => return Err(e),
    };
    record.last_activity = max(record.last_activity + 1, now_secs());
    match update_record_with_revision(store, session_id, &record, revision).await {
        Ok(_) => return Ok(()),
        Err(e) if is_wrong_last_revision(&e) => continue,
        Err(e) => return Err(e),
    }
}
```

#### 2. Best-Effort Isolation from Critical Path

Index refresh **must never delay lease renewal**. Wrap in bounded timeout:

```rust
const INDEX_REFRESH_TIMEOUT: Duration = Duration::from_secs(1);

match time::timeout(
    INDEX_REFRESH_TIMEOUT,
    refresh_session_index_last_activity(store, &session_id),
).await {
    Ok(Ok(())) => {}
    Ok(Err(error)) => { warn!("failed to refresh index: {error:#}"); }
    Err(_) => { warn!("index refresh timed out"); }
}
```

The 1s timeout is well within default 10s renew interval. Failure logs warning only; never fails renewal.

#### 3. Per-Surface Error Handling for Remote Reads

**Key lesson:** "Graceful degradation" must distinguish *no data* from *couldn't fetch*, and the right way to surface a failure depends on the calling surface. Errors should flow through return values so each surface delivers them appropriately.

**Contract (refined post-review):** `list_remote_sessions_with_meta(cluster)` returns:
- `Ok(vec![])` for genuinely-empty cases: empty bucket OR bucket-not-found (no sessions indexed yet)
- `Err` for real failures: connection/auth/permissions/transport errors, or `list_records` failure on an existing bucket

**Why this matters:** Silently returning empty on *all* failures was wrong UX. When a user explicitly lists remote sessions and the cluster is unreachable or auth fails, a silent empty list looks like "no sessions exist" — misleading and frustrating. The refined contract surfaces real failures while still treating "no index yet" as valid-empty.

```rust
pub async fn list_remote_sessions_with_meta(&self, cluster: &str) -> Result<Vec<SessionMeta>> {
    let jetstream = match self.nats_jetstream(cluster).await {
        Ok(js) => js,
        Err(e) => {
            // Connection/auth failure → Err (surface to user)
            return Err(anyhow::anyhow!("remote sessions unavailable: {e:#}"));
        }
    };
    let store = match jetstream.get_key_value(BUCKET).await {
        Ok(s) => s,
        Err(e) if kv_bucket_missing(&e) => {
            // Bucket not found → Ok(empty) (no sessions indexed yet)
            return Ok(vec![]);
        }
        Err(e) => {
            // Other get_key_value failure (perms, transport) → Err
            return Err(anyhow::anyhow!("remote sessions unavailable: {e:#}"));
        }
    };
    let records = match list_records(&store).await {
        Ok(r) => r,
        Err(e) => {
            // Listing failure on existing bucket → Err
            return Err(anyhow::anyhow!("remote sessions unavailable: {e:#}"));
        }
    };
    Ok(records.into_iter().map(record_to_meta).collect())
}
```

**Per-surface handling:**

| Surface | Behavior | Rationale |
|---------|----------|-----------|
| **CLI** (`--list-sessions`) | On `Err`, print to stderr + non-zero exit | Explicit user intent; silently showing empty is misleading |
| **TUI** (session picker modal) | Store error in `error: Option<String>` field on `ModalState::SessionPicker`; render as visible ⚠ line | Logs invisible/corrupting in TUI; error must flow through return value into UI state |
| **Completion** (tab-complete) | Degrade to empty on `Err` | Ambient/non-blocking; can't show errors, must stay snappy (500ms timeout) |

**Bucket-not-found detection:**

The `kv_bucket_missing` helper detects `STREAM_NOT_FOUND` in the async_nats error chain:

```rust
pub fn kv_bucket_missing(error: &Error) -> bool {
    error
        .downcast_ref::<async_nats::Error>()
        .map(|e| e.to_string().contains("STREAM_NOT_FOUND"))
        .unwrap_or(false)
}
```

**Fragility:** This depends on the current async_nats error-chain shape. If async_nats changes how it reports missing buckets, this detection may break.

#### 4. Completion Path Graceful Degradation

Completion still degrades to empty on any failure (ambient, non-blocking surface):

```rust
pub async fn list_sessions_for_completion(&self, cluster: Option<&str>) -> Vec<String> {
    match cluster {
        Some(cluster) => {
            match tokio::time::timeout(
                Duration::from_millis(500),
                self.list_remote_sessions_with_meta(cluster),
            ).await {
                Ok(Ok(sessions)) => sessions.into_iter().map(|s| s.id).collect(),
                _ => vec![],
            }
        }
        None => self.list_sessions(),
    }
}
```

#### 5. Resume-Format Constraint

Picker yields **bare `session_id`** (no prefix/decoration) to match existing remote resume path:

```rust
fn session_index_record_to_meta(record: &SessionIndexRecord) -> SessionMeta {
    SessionMeta {
        id: record.session_id.clone(), // bare session_id
        // ... other fields
    }
}

// In TUI picker selection:
let session_name = sessions[selected - 1].id.clone();
self.config.write().use_session(Some(&session_name));
```

## Deferrals

Explicitly **not** implemented (tracked by follow-up issues):

- **No backfill**: Index covers sessions active after deploy; pre-existing sessions appear on next activation
- **No tombstone TTL/GC**: Stale records persist until manual cleanup. [#933](https://github.com/dobesv/harnx/issues/933) tracks stale cleanup
- **No rich metadata**: Picker shows ID + git branch/remote; title/summary deferred. [#934](https://github.com/dobesv/harnx/issues/934)

## Known Limitations

- **O(N) sequential fetches**: `list_records` performs one KV `entry()` call per key. Acceptable at single-agent scale (few sessions); latency concern for large counts
- **Stale records possible**: Without GC, deleted sessions may linger in enumeration until explicit index cleanup
- **Delete path lacks timeout**: `best_effort_delete_session_index_record` has no timeout; slow NATS may delay admin delete command
- **Bucket-not-found detection is fragile**: Depends on async_nats error-chain containing `STREAM_NOT_FOUND`; may break if async_nats changes error formatting

## Test Gating Convention

Integration tests use `HARNX_NATS_TEST_URL` environment variable:

```rust
#[tokio::test]
async fn live_kv_crud() {
    let url = match std::env::var("HARNX_NATS_TEST_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("skipping: HARNX_NATS_TEST_URL unset");
            return;
        }
    };
    // ... real NATS test against url
}
```

Keeps `cargo nextest` green in normal CI while enabling opt-in real-bucket testing.

## Related Issues

- **Plan:** `nats-remote-session-enumeration` (GitHub #914)
- **Prior Art:** `nats-ha-lease.md` — mirrors lease bucket pattern; `session-picker-multi-factor-sorting-2026-05-02.md` — local enumeration sorting
- **Follow-ups:** #933 (stale GC), #934 (richer picker metadata)
- **Code:** `crates/harnx-runtime/src/nats_session_index.rs`, `nats_lease.rs:spawn_renew_task`, `config/session_ops_split.rs:list_remote_sessions_with_meta`
