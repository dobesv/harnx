---
title: "Snapshot test normalization for non-deterministic IDs and paths"
date: 2026-05-03
category: "integration-issues"
problem_type: integration_issue
component: "harnx/tests, harnx-runtime/tests"
root_cause: "Short session IDs change every second; temp paths vary per run; snapshots fail spuriously"
resolution_type: test_fix
severity: low
tags:
  - testing
  - snapshot-tests
  - normalization
  - session-ids
plan_ref: "session-management-451-450-448-449"
---

## Problem

Snapshot tests failed spuriously after replacing UUID session IDs with 6-character base64url timestamp-based IDs. The IDs change every second, making snapshots non-deterministic. Additionally, temp paths (`/tmp/.tmpXXXXXX/`) and ISO timestamps varied between runs.

## Symptoms

- Snapshot tests fail with "unexpected session ID" in output
- False negatives: code is correct, but snapshot differs
- Tests pass when run in the same second, fail otherwise

## Root Cause

Three sources of non-determinism in test output:

1. **Short session IDs**: 6-char base64url encoding of Unix timestamp (e.g., `afgkKA`). Changes every second.
2. **Temp paths**: `/tmp/.tmpXXXXXX/` varies per test process.
3. **ISO timestamps**: `2026-05-03T14:32:15.123Z` differs between runs.

The existing `normalize_uuids()` function handled UUIDv4/v7 IDs, but not the new short format.

## Solution

Added three normalization functions for snapshot stability:

### `normalize_short_session_ids`

Replaces 6-char base64url session IDs with `[SID]` placeholder. Only matches after specific prefixes to avoid false positives on other 6-char strings:

```rust
// crates/harnx/tests/tmux_e2e.rs
/// Replace 6-char base64url session IDs (e.g. `afgkKA`) with `[SID]` so
/// snapshots are deterministic across runs. A valid short session ID consists
/// of exactly 6 characters from the URL-safe base64 alphabet `[A-Za-z0-9_-]`.
/// We only replace tokens that appear after ` ▸ ` or `session_id: ` to avoid
/// false positives on other 6-char alphanumeric strings.
fn normalize_short_session_ids(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let remaining: String = chars[i..].iter().collect();
        let prefix_len = if remaining.starts_with(" ▸ ") {
            Some(" ▸ ".chars().count())
        } else if remaining.starts_with("session_id: ") {
            Some("session_id: ".chars().count())
        } else {
            None
        };
        if let Some(pfx_len) = prefix_len {
            let pfx: String = chars[i..i + pfx_len].iter().collect();
            out.push_str(&pfx);
            i += pfx_len;
            // Check if next 6 chars are base64url followed by non-base64url or end
            if i + 6 <= n {
                let candidate: String = chars[i..i + 6].iter().collect();
                let is_b64 = candidate
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
                let after_ok = i + 6 >= n || {
                    let next = chars[i + 6];
                    !next.is_ascii_alphanumeric() && next != '-' && next != '_'
                };
                if is_b64 && after_ok {
                    out.push_str("[SID]");
                    i += 6;
                    continue;
                }
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}
```

### `normalize_temp_paths`

Replaces `/tmp/.tmpXXXXXX/` patterns with `[TMP]/` and ISO timestamps with `[TIMESTAMP]`:

```rust
fn normalize_temp_paths(text: &str) -> String {
    // Replace temp directory paths: /tmp/.tmpXXXXXX/
    let re = regex::Regex::new(r"/tmp/\.tmp[A-Za-z0-9]{6}/").unwrap();
    let text = re.replace(text, "[TMP]/").to_string();

    // Replace ISO timestamps: 2026-05-03T14:32:15.123Z
    let re = regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z?").unwrap();
    re.replace(&text, "[TIMESTAMP]").to_string()
}
```

### `normalize_uuids`

Existing function for UUIDv4/v7 patterns:

```rust
/// Replace any UUIDv4-looking substring with `[UUID]` placeholder.
fn normalize_uuids(text: &str) -> String {
    // UUID pattern: 8-4-4-4-12 hex chars separated by hyphens.
    // Matches both UUIDv4 and UUIDv7 formats.
    // ...
}
```

### Usage in Tests

Chain normalization functions in order:

```rust
let normalized = normalize_temp_paths(
    &normalize_short_session_ids(
        &normalize_uuids(&final_screen)
    )
);
insta::assert_snapshot!(normalized);
```

## Why This Works

1. **Prefix matching prevents false positives**: Only replace 6-char strings after known session ID contexts (` ▸ `, `session_id: `).
2. **Base64url alphabet validation**: Ensures candidate is valid session ID format.
3. **Boundary checking**: Next char after ID must not be base64url (prevents partial matches).
4. **Order matters**: Normalize UUIDs first (longer match), then short IDs, then temp paths/timestamps.

## Prevention Strategies

**Test Patterns:**

- Always use normalization helpers in snapshot tests
- Add new prefixes if session IDs appear in new contexts
- Keep normalization functions in shared test utility module

**Code Review Checklist:**

- [ ] Do snapshot tests use normalization for non-deterministic values?
- [ ] Are new session ID display locations covered by normalization prefixes?
- [ ] Is the normalization order correct (UUIDs before short IDs)?

## Related Issues

- **GitHub:** [Issue #449](https://github.com/dobesv/harnx/issues/449) — Shorter 6-char base64url session IDs
- **Related Solution:** [logic-errors/short-session-id-collision-claim-2026-05-03.md](../logic-errors/short-session-id-collision-claim-2026-05-03.md) — Short ID implementation
