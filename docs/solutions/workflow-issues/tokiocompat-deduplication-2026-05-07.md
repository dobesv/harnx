---
title: "Deduplicate TokioCompat adapter across workspace crates"
date: 2026-05-07
category: workflow-issues
problem_type: workflow_issue
component: harnx-acp
root_cause: "utility adapter struct duplicated in three locations across workspace"
resolution_type: code_fix
severity: low
tags:
  - deduplication
  - workspace-organization
  - doc-hidden
  - tokio
  - futures-io
plan_ref: issue-72-tokiocompat-dedup
---

## Problem

`TokioCompat<T>` adapter (bridging tokio's `AsyncRead`/`AsyncWrite` to `futures_io` traits) existed in three separate locations across `harnx-acp` and `harnx-acp-server` crates, causing maintenance burden and potential drift.

## Symptoms

```
crates/harnx-acp/src/client.rs         — local TokioCompat struct (+50 lines)
crates/harnx-acp-server/src/lib.rs      — local TokioCompat struct (+50 lines)
crates/harnx-acp-server/src/server_main.rs — local TokioCompat struct (+50 lines)
```

Each duplicate included:
- Identical struct definition
- `AsyncRead` implementation for `futures_util::io::AsyncRead`
- `AsyncWrite` implementation for `futures_util::io::AsyncWrite`
- Supporting imports: `Pin`, `Poll`, `TaskContext`, `TokioAsyncRead`, `TokioAsyncWrite`, `ReadBuf`

Risk: bug fixes or improvements applied to one copy but not others.

## Investigation Steps

1. Identified `harnx-acp` as common dependency of both `harnx-acp-server` and `harnx-acp` client code.
2. Confirmed `TokioCompat` is internal workspace utility — not part of public API.
3. Extracted to `crates/harnx-acp/src/compat.rs` with module comment explaining purpose.
4. Used `#[doc(hidden)] pub mod compat;` to expose cross-crate without cluttering docs.

## Root Cause

Utility adapter needed in multiple crates but no shared location existed. Each location independently implemented the same `futures_io` ↔ tokio bridge.

## Solution

**1. Create canonical module** in lowest-level crate:

```rust
// crates/harnx-acp/src/compat.rs
//! Adapter between tokio's `AsyncRead`/`AsyncWrite` and `futures_io`'s
//! `AsyncRead`/`AsyncWrite`.
//!
//! `agent-client-protocol` accepts the `futures_io` traits; tokio's
//! stdio/process streams implement the tokio traits.  `TokioCompat` bridges
//! the gap.

use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};

pub struct TokioCompat<T> {
    inner: T,
}

impl<T> TokioCompat<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: TokioAsyncRead + Unpin> futures_util::io::AsyncRead for TokioCompat<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut read_buf = ReadBuf::new(buf);
        match Pin::new(&mut self.inner).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: TokioAsyncWrite + Unpin> futures_util::io::AsyncWrite for TokioCompat<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
```

**2. Export as doc-hidden** from lib.rs:

```rust
// crates/harnx-acp/src/lib.rs
mod client;
#[doc(hidden)]
pub mod compat;
mod config;
mod event;
pub mod manager;
```

**3. Update consumers** to import from shared location:

```rust
// client.rs
use crate::compat::TokioCompat;
```

```rust
// server tests in lib.rs
use harnx_acp::compat::TokioCompat;
```

**4. Remove local definitions and dead imports**:

```rust
// Before (client.rs):
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};

struct TokioCompat<T> { inner: T }
// + 50 lines of impl
```

```rust
// After (client.rs):
use crate::compat::TokioCompat;
```

**5. Run `cargo fmt`** to fix module declaration order (alphabetical: `mod client;` before `pub mod compat;`).

## Why This Works

1. **Single canonical location**: `harnx-acp` is already a dependency of all consumers, so no new dependency edges needed.

2. **`#[doc(hidden)]`**: Prevents internal utility from appearing in public API docs while allowing `pub` visibility for workspace consumers.

3. **Dead import cleanup**: Removing local struct makes associated imports dead code — `cargo check` catches these.

4. **`cargo fmt` ordering**: Rustfmt enforces alphabetical `mod` declarations. Running after structural changes prevents CI failures.

## Prevention Strategies

**Code Review Checklist:**
- [ ] Check for duplicate utility structs before introducing new copies
- [ ] Identify lowest-level crate that can host shared utilities
- [ ] Use `#[doc(hidden)]` for internal workspace utilities

**Best Practices:**
- Extract shared utilities to crates already in dependency graph
- Run `cargo fmt` after adding/removing module declarations
- Clean up imports when removing local definitions (`cargo check -W unused_imports`)
- Document rationale for internal utilities with module-level comments

**Refactoring Pattern:**
When utility exists in 3+ locations:
1. Create shared module in lowest-level crate
2. Mark `#[doc(hidden)] pub` if internal
3. Update all consumers to import
4. Remove locals + dead imports
5. Run `cargo fmt && cargo check`

## Related Issues

- **GitHub:** [issue #72](https://github.com/dobesv/harnx/issues/72) — TokioCompat deduplication
- **Related Solution:** [acp-io-task-supervision-2026-05-07.md](../async-patterns/acp-io-task-supervision-2026-05-07.md) — Same ACP client refactoring context
