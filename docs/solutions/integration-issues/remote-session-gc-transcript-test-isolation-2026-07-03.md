---
title: "Remote Session GC: All-or-Nothing Transcript Deletion + Shared NATS Test Isolation"
date: 2026-07-03
category: integration-issues
problem_type: integration_issue
component: "remote-session-cleanup"
root_cause: "Per-message stream truncation corrupts replay; shared global buckets cause test flake under parallel execution"
resolution_type: code_fix
severity: high
tags:
  - nats
  - jetstream
  - kv
  - gc
  - test-isolation
  - nextest
  - transcript
  - all-or-nothing
  - shared-state
plan_ref: "remote-session-gc-933"
---
# Solution: Remote Session GC with All-or-Nothing Transcript Deletion + Shared NATS Test Isolation

## Problem

Remote session GC needed to delete idle sessions' index records + full transcript streams after a configurable period, but two subtleties caused production correctness risk and severe test flake:
1. **Transcript deletion must be all-or-nothing** — per-message purge corrupts replay
2. **Shared NATS bucket names cause parallel-test contention** — fixed protocol constants mean all tests share physical buckets

## Symptoms

**Production Risk:**
- Setting `max_age`/`max_msgs` on `SESSION_<ID>` streams would truncate long-lived sessions, violating all-or-nothing invariant
- Per-message purge breaks replay path that reads `first_sequence..last_sequence`

**Test Flake (burned 4 review cycles):**
- `leased_session_is_skipped` panicked: `Failed to open NATS KV bucket 'harnx_leases'` (non-deterministic)
- `missing_index_bucket_returns_zero_stats` assertion failure: `left: 2, right: 0` (foreign records contaminated)
- Failures only surfaced under full-suite `--stress-count`, not module-only runs
- `stale_session_is_deleted_when_lease_bucket_is_missing` deleting global bucket raced with other tests

## Root Cause

### Transcript Deletion
JetStream stream replay reads sequential messages. Per-message purge via `max_age`/`max_msgs` truncates the stream, leaving gaps in sequence numbers. Replay code expects continuous `first_sequence..last_sequence` — gaps corrupt replay. Whole-stream `delete_stream` is the only correct mechanism.

### Shared NATS Test Isolation
Under `cargo-nextest` (process-per-test, parallel) against ONE shared NATS server:
- Bucket names (`harnx_sessions`, `harnx_leases`) are fixed protocol constants
- All tests share the same physical buckets regardless of "cluster name" parameter
- Bucket deletion races with every parallel test using it
- Aggregate count assertions contaminated by foreign test records
- Hardcoded leader-lease ID (`session_index_gc`) caused cross-test leader contention

## Solution

### Production: All-or-Nothing Stream Deletion

**Hard MUST NOT:**
- Never set `max_age` or `max_msgs` on `SESSION_<ID>` streams
- Never use per-message purge/truncation

**Correct pattern:**
```rust
// NO: corrupts replay
stream.set_max_age(duration);  // NEVER
stream.purge_messages_older_than(...);  // NEVER

// YES: whole-stream delete
nats_admin::delete_remote_session(&config, &cluster, &session_id).await?;
// Internally: delete_stream → lease delete → index delete (order matters)
```

Deletion order (existing primitive): stream → lease → index. "Not found" tolerance built in.

### GC Loop Pattern

```rust
// Public entry returns owned stats (errors are counted, not propagated);
// the fallible scan lives in an inner helper returning anyhow::Result.
pub async fn run_remote_cleanup(
    config: &Config,
    days: u64,
    cluster: &str,
) -> RemoteCleanupStats {
    if days == 0 {
        return RemoteCleanupStats::default();  // disabled / guard direct callers
    }
    // ... acquire leader lease, then run_remote_cleanup_inner(...).await ...
}

async fn run_remote_cleanup_inner(
    config: &Config,
    days: u64,
    cluster: &str,
) -> anyhow::Result<RemoteCleanupStats> {
    // Leader-election: only one worker scans per cycle
    let lease_id = "session_index_gc";  // unique per NATS cluster
    let lease = NatsSessionLease::acquire(config, cluster, lease_id).await?;
    if lease.is_none() { return Ok(RemoteCleanupStats::default()); }

    // Scan index bucket for idle sessions
    let threshold = now() - Duration::from_days(days);
    for record in list_records(&sessions_bucket).await? {
        if record.last_activity < threshold {
            // Reactivation race guard: re-fetch before delete
            let fresh = fetch_record(&sessions_bucket, &record.session_id).await?;
            if fresh.last_activity >= threshold { continue; }
            
            // Lease-absent check
            if lease_present(&leases_bucket, &record.session_id).await? { continue; }
            
            // Whole-stream delete (all-or-nothing)
            delete_remote_session(config, cluster, &record.session_id).await?;
        }
    }
}
```

### Test Isolation Rules (for shared NATS server)

**Rule 1: NEVER delete global singleton buckets**
```rust
// WRONG: races with parallel tests
bucket.delete_key("harnx_leases").await?;  // NEVER
```

Bucket names are fixed constants — deleting one races with every test using it.

**Rule 2: Assert only on own artifacts + lower-bound counts**
```rust
// WRONG: foreign records contaminate
assert_eq!(stats.deleted, 0);  // Flaky

// CORRECT: own session + lower bounds
assert!(session_exists(&my_session_id).await? == false);
assert!(stats.errors == 0);
assert!(stats.skipped_active >= 0);  // lower bound OK
```

**Rule 3: Unique leader-lease ID per test**
```rust
// Test seam: unique GC lease ID
let gc_id = format!("session_index_gc_test_{}", Uuid::new_v4());
run_remote_cleanup_with_gc_id(&config, days, &cluster, &gc_id).await?;
```

Production API unchanged; test-only seam injects unique lease ID.

**Rule 4: Pure unit test for "missing bucket => Ok(None)"**
```rust
// WRONG: integration test deletes live bucket
delete_bucket(&leases_bucket).await?;
let result = run_remote_cleanup(...).await;  // race-prone

// CORRECT: unit test of decision branch
#[tokio::test]
async fn lease_present_returns_false_when_store_missing() -> anyhow::Result<()> {
    let result = lease_present(None, "any_session").await?;
    assert!(!result);  // pure logic, no NATS
    Ok(())
}
```

Don't delete live shared bucket to test missing-bucket branch; unit-test the predicate.

**Rule 5: Handle missing buckets gracefully in production**
```rust
async fn load_optional_lease_store(bucket: &str) -> Result<Option<Store>> {
    match get_kv_bucket(bucket).await {
        Ok(store) => Ok(Some(store)),
        Err(e) if kv_bucket_missing(&e) => Ok(None),  // tolerate
        Err(e) => Err(e),
    }
}
```

### Missing Bucket Tolerance

GC loop must proceed when `harnx_leases` bucket doesn't exist yet:

```rust
let lease_store = load_optional_lease_store("harnx_leases").await?;
// lease_store: Option<&Store>

async fn lease_present(store: Option<&Store>, session_id: &str) -> Result<bool> {
    match store {
        None => Ok(false),  // bucket missing => no lease
        Some(s) => s.get(&lease_key(session_id)).await?.is_some(),
    }
}
```

## Why This Works

**All-or-nothing deletion:** Whole-stream `delete_stream` removes atomically; no partial truncation; replay path sees either complete transcript or nothing (correct for deleted session). No sequence gaps.

**Test isolation rules:** Unique leader IDs prevent contention. Asserting on own artifacts avoids foreign contamination. No bucket deletion eliminates race conditions. Pure unit tests for edge cases (missing bucket) avoid fragile integration paths.

**Missing bucket tolerance:** New clusters may not have `harnx_leases` initialized; GC must still run (checking index `last_activity` + reactivation guard). `Ok(None)` → proceed with scan.

## Prevention Strategies

**Code Review Checklist:**
- [ ] No `max_age` or `max_msgs` on `SESSION_<ID>` streams (grep for both)
- [ ] Deletion flows through `delete_remote_session` or equivalent whole-stream primitive
- [ ] Tests assert on unique session IDs, not aggregate counts
- [ ] Tests never delete global singleton buckets (`harnx_sessions`, `harnx_leases`)
- [ ] Leader-lease IDs unique per test (test seam injection)
- [ ] Missing bucket branches tested via unit test, not integration

**Test Patterns:**
- Unique per-test UUIDs for session IDs
- `errors == 0` + own-artifact presence/absence
- Lower-bound counts (`>= 0`, `>= 1`) where aggregates needed
- `--stress-count` validation before merge

**Monitoring:**
- GC cycle logs: `scanned`, `deleted`, `skipped_active`, `errors`
- Alert on `errors > 0` trend
- Track transcript stream count vs index record count (drift indicates orphan streams)

## Known Limitations

**Orphan streams:** `SESSION_<ID>` streams with no index entry aren't reclaimed by this index-driven scan. Possible leak class if index deleted while stream survives. Mitigated by deletion order (stream → lease → index). Future follow-up: enumerate `SESSION_*` streams and reconcile.

**GC interval:** Hourly scan; sessions idle slightly beyond threshold may persist up to +1 hour. Acceptable for cleanup use case.

## Related Issues

- **GitHub:** [#933](https://github.com/dobesv/harnx/issues/933) — Remote session index: stale entry cleanup (TTL/GC)
- **Prior Art:** `docs/solutions/integration-issues/nats-kv-session-index-enumeration-2026-06-27.md` — index architecture
- **Prior Art:** `docs/solutions/nats-ha-lease.md` — lease TTL / leader-election pattern
- **Prior Art:** `docs/solutions/async-patterns/tokio-interval-spawn-single-point-2026-06-17.md` — spawn pattern
