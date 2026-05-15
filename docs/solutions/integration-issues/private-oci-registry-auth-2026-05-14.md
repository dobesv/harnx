---
title: "Private OCI registry authentication with per-registry credentials"
date: 2026-05-14
category: integration-issues
problem_type: integration_issue
component: harnx-pkg
root_cause: OCI fetcher lacked authentication support for private registries
resolution_type: code_fix
severity: medium
tags:
  - oci
  - authentication
  - serde
  - axum
  - rust
  - credentials
plan_ref: harnx-536-private-registry-auth
---

## Problem

harnx-pkg's OCI fetcher had no mechanism to authenticate against private OCI registries. Packages could only be fetched from public registries, blocking users from referencing private GitHub Container Registry or other authenticated registries.

## Symptoms

- Private registry package pulls failed with anonymous auth errors
- No way to configure per-registry credentials
- No integration test coverage for authenticated OCI fetches

## Investigation Steps

Implemented per-registry credential configuration stored in `~/.config/harnx/package_repos/*.yaml`. Each YAML file specifies a registry URL prefix and optional `username` and `password` fields. Credentials can be sourced from:
- `env: VAR_NAME` — read from environment variable
- `command: "..."` — execute command directly (e.g., `gh auth token`)
- `value: "literal"` — hardcoded value

Created `credentials` module with `resolve_oci_auth(url)` that finds matching config and constructs `RegistryAuth::Basic` or `Anonymous`.

Added mock OCI registry with Basic auth middleware for integration testing.

## Root Cause

The `OciFetcher` was stateless and always used `RegistryAuth::Anonymous`. No credential resolution logic existed. Test infrastructure had no way to simulate authenticated registries.

## Solution

### 1. CredentialSource enum with `#[serde(untagged)]`

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CredentialSource {
    Env { env: String },
    Command { command: String },
    Value { value: String },
}
```

**Key insight**: `#[serde(untagged)]` is required for natural YAML like `{ env: GH_TOKEN }`. Using `rename_all` would NOT work — serde needs to try variants in order without a `type` discriminator field.

### 2. Axum middleware State extraction

```rust
.layer(middleware::from_fn_with_state(
    Arc::clone(&state),
    auth_middleware,
))
```

**Key insight**: `middleware::from_fn` does NOT support State extraction. Must use `from_fn_with_state` when middleware needs router state (like `expected_auth`).

### 3. Registry URL prefix matching with boundary awareness

```rust
fn is_repo_prefix_match(target: &str, config: &str) -> bool {
    let (target_host, target_path) = split_host_and_path(target);
    let (config_host, config_path) = split_host_and_path(config);

    if target_host != config_host {
        return false;
    }

    if config_path.is_empty() {
        return true;
    }

    if !target_path.starts_with(config_path) {
        return false;
    }

    // CRITICAL: check next byte is '/' or end-of-string
    target_path
        .as_bytes()
        .get(config_path.len())
        .is_none_or(|byte| *byte == b'/')
}
```

**Key insight**: Raw `starts_with` would leak credentials. `ghcr.io/myorg` MUST NOT match `ghcr.io/myorg-evil/pkg`. Must check that the character after the matched prefix is `/` or absent.

### 4. Rust binary crate lib/main module visibility

When splitting `main.rs` and `lib.rs`, modules used by both must be declared in both:

```rust
// main.rs
mod credentials;
mod commands;

// lib.rs
pub mod credentials;
pub mod commands;
```

If `commands/add.rs` calls `crate::credentials::resolve_oci_auth`, the `mod credentials` declaration must be in `main.rs`, not just `lib.rs`. The `main.rs` sees `crate::` as itself.

### 5. Avoid `block_on` inside async Tokio tests

```rust
// WRONG — panics in #[tokio::test]
let auth = tokio::runtime::Handle::current().block_on(resolve_oci_auth(url))?;

// CORRECT — just await
let auth = resolve_oci_auth(url).await?;
```

`block_on` cannot be called inside an existing Tokio runtime. Use `.await` directly in `#[tokio::test]` functions.

## Why This Works

- `#[serde(untagged)]` allows YAML without type tags while maintaining variant disambiguation via distinct field names
- `from_fn_with_state` passes router state through middleware extraction layers
- Host equality + path boundary check prevents credential exfiltration to lookalike registries
- Declaration in both `main.rs` and `lib.rs` ensures `crate::` paths resolve from either entry point
- Direct `.await` in Tokio tests avoids nested runtime panic

## Prevention Strategies

**Test Cases:**
- Negative tests for prefix matching: `ghcr.io/myorg` must not match `ghcr.io/myorg-evil/pkg`
- Negative tests for host suffix: `registry.internal` must not match `registry.internal.attacker.com/pkg`
- Command source success/failure paths
- Token-only auth (empty username, non-empty password)
- Missing password fallback to anonymous

**Code Review Checklist:**
- [ ] Credential matching is host+path-boundary aware, not raw string prefix
- [ ] Serde enums with variant-distinguishing fields use `untagged`
- [ ] Axum middleware needing State uses `from_fn_with_state`
- [ ] Binary crates declare shared modules in both main.rs and lib.rs

**Best Practices:**
- Always use `from_fn_with_state` when middleware extracts State
- Validate URL prefix matching includes boundary checks to prevent credential leakage
- Test command-based credential sources with success and failure scenarios
- Use dedicated test mutex for env var mutations in parallel tests

## Related Issues

- **Plan:** harnx-536-private-registry-auth
- **Files:** `crates/harnx-pkg/src/credentials.rs`, `crates/harnx-pkg/tests/oci_fetcher.rs`
- **Docs:** `docs/private-registries.md`
