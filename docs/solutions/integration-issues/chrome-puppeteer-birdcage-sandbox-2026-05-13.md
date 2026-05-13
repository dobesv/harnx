---
title: "Chrome/Puppeteer crashes inside birdcage Linux sandbox"
date: 2026-05-13
category: "integration-issues"
problem_type: integration_issue
component: "birdcage-sandbox"
root_cause: "missing bind mount for /dev/shm submount, nested user namespace blocked by ptrace_scope"
resolution_type: code_fix
severity: high
tags:
  - chrome
  - puppeteer
  - birdcage
  - sandbox
  - user-namespaces
  - dev-shm
  - container
plan_ref: "dobesv/harnx#528"
---

## Problem

Chrome (and Puppeteer) crashes with two fatal errors when run inside the harnx birdcage sandbox:
1. `FATAL sandbox/linux/services/credentials.cc:131` — Chrome's sub-process sandbox fails because birdcage uses `CLONE_NEWUSER` (user namespaces), and the kernel's `ptrace_scope=1` blocks nested user namespace creation
2. `/dev/shm` is mounted read-only inside birdcage's mount namespace — Chrome uses this tmpfs for IPC shared memory and crashes on first write

## Symptoms

```
FATAL:sandbox/linux/services/credentials.cc(131)] setresuid: Operation not permitted
(trace/bp trap by Chrome subprocess trying to create nested user namespace)

# Additionally:
Error: EROFS: read-only file system, open '/dev/shm/.com.google.Chrome.xxx'
```

- Chrome processes exit immediately when launched inside birdcage sandbox
- Puppeteer connection fails with "Target closed" errors
- Reproducible in any container-like environment with user namespaces

## Investigation Steps

1. Analyzed Chrome sandbox architecture — Chrome creates nested user namespaces for process isolation
2. Checked kernel Yama security settings — `ptrace_scope=1` (default in containers) blocks unprivileged processes from creating nested user namespaces
3. Inspected birdcage mount namespace — discovered `/dev/shm` is a separate tmpfs mount, NOT included in bind mount of `/dev`
4. Tested Chrome with `--no-sandbox --disable-dev-shm-usage` — confirmed workaround works
5. Verified birdcage's `system_writable_paths()` mechanism — only `/tmp` was included, `/dev/shm` missing

## Root Cause

**Issue 1: `/dev/shm` not bind-mounted**

Birdcage creates a new mount namespace and bind-mounts `/dev` into it. However, `/dev/shm` is a separate tmpfs mount point. Bind mounting `/dev` with `MS_BIND` (without `MS_REC`) does NOT recursively include submounts. Without an explicit exception in `system_writable_paths()`, `/dev/shm` is either invisible or read-only inside the sandbox.

**Issue 2: Nested user namespaces blocked**

Birdcage uses `CLONE_NEWUSER` to create a user namespace. Inside this namespace, Chrome attempts to create a nested user namespace for its sandbox. The kernel's Yama security module (`/proc/sys/kernel/yama/ptrace_scope ≥ 1`) blocks unprivileged creation of nested user namespaces in container environments. This is by design and cannot be bypassed by birdcage.

## Solution

**Fix: Add `/dev/shm` to system writable paths**

Modified `system_writable_paths()` in `crates/harnx-mcp-bash/src/server.rs`:

```rust
// Before:
#[cfg(target_os = "linux")]
fn system_writable_paths() -> Vec<&'static str> {
    vec!["/tmp"]
}

// After:
#[cfg(target_os = "linux")]
fn system_writable_paths() -> Vec<&'static str> {
    vec!["/tmp", "/dev/shm"]
}
```

This causes birdcage to create a writable bind mount for `/dev/shm` in the sandboxed mount namespace.

**Workaround for nested user namespace issue:**

Launch Chrome/Puppeteer with:
```
--no-sandbox --disable-dev-shm-usage
```

This is standard practice for container environments (Docker, CI, birdcage). No code fix possible — kernel security policy blocks nested user namespaces.

## Why This Works

Adding `/dev/shm` to `system_writable_paths()` tells birdcage to create a separate writable bind mount for that path. Chrome's IPC shared memory operations succeed because the tmpfs is now writable inside the sandbox.

The nested user namespace issue is a kernel-level security restriction. Using `--no-sandbox` disables Chrome's own sandboxing (which requires nested namespaces) and is safe inside birdcage since birdcage already provides sandbox isolation.

## Prevention Strategies

**Checklist for adding new sandbox-aware applications:**

- [ ] Identify tmpfs mounts the application uses (`findmnt -t tmpfs`)
- [ ] Add required paths to `system_writable_paths()` for Linux
- [ ] Document required application flags for container environments
- [ ] Test in sandbox before merging

**Code review items:**

- [ ] Does the app use `/dev/shm`, `/run`, or other tmpfs mounts?
- [ ] Does the app create its own sandbox (browsers, Electron apps)?
- [ ] Add recipe to `docs/bash-mcp-server.md` for future reference

## Related Issues

- **Reference:** [dobesv/harnx#528](https://github.com/dobesv/harnx/issues/528)
- **Puppeteer docs:** [Running Puppeteer in Docker](https://pptr.dev/guides/docker) — same flags required
- **Chrome Linux sandbox:** `chrome_sandbox_linux_services_credentials.cc`
