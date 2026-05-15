---
title: "AWS credential chain with async caching for Bedrock client"
date: 2026-05-15
category: integration-issues
problem_type: integration_issue
component: harnx-client
root_cause: Bedrock client only supported static credentials, blocking SSO/IAM role/instance profile auth
resolution_type: code_fix
severity: high
tags:
  - aws
  - credentials
  - caching
  - async
  - bedrock
  - rust
plan_ref: bedrock-aws-credential-chain
---

## Problem

Bedrock client only accepted static `access_key_id`/`secret_access_key` credentials. Users with AWS SSO, IAM roles, EC2 instance profiles, or named profiles in `~/.aws/config` had no way to use standard AWS credential resolution — they had to copy static keys into harnx config.

## Symptoms

- SSO users couldn't use `aws sso login` profiles
- EC2 instances couldn't use IAM instance profiles
- Environment variable credentials (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) were ignored
- Named profiles from `~/.aws/config` were inaccessible
- Interactive setup forced users to enter static credentials even when chain would work

## Investigation Steps

1. Analyzed `vertexai.rs` to understand existing async credential caching pattern — found `LazyLock<RwLock<IndexMap<String, (Token, i64)>>>` module-level static with `prepare_*` + `get_cached_*` fn pair.
2. Tested `aws_config::defaults().load().await` to understand SDK behavior — discovered it can hang indefinitely on IMDSv2/SSO endpoints.
3. Traced `PROMPTS` validation in `harnx-runtime/src/client/common.rs` — found `required = env::var(&env_name).is_err()` marks fields required when env vars absent, blocking blank input.
4. Explored `aws_credential_types::provider::ProvideCredentials` — found `expiry()` returns `Option<SystemTime>` requiring conversion to Unix epoch.

## Root Cause

Bedrock client used `get_access_key_id()?` and `get_secret_access_key()?` directly in builder methods, failing immediately when static credentials weren't configured. No fallback to AWS SDK's `aws_config::defaults()` credential chain existed. The `PROMPTS` array required both fields during interactive setup, making chain-based auth impossible to configure.

## Solution

### 1. Module-Level Credential Cache (Mirroring VertexAI)

```rust
static AWS_CREDENTIALS: LazyLock<RwLock<IndexMap<String, (AwsCredentials, i64)>>> =
    LazyLock::new(|| RwLock::new(IndexMap::new()));
```

Pattern matches `ACCESS_TOKENS` in `access_token.rs` — global, in-memory, keyed by `client_name`.

### 2. Async Credential Preparation with Timeout Guard

```rust
async fn prepare_aws_credentials(
    client_name: &str,
    region: &str,
    profile: Option<&str>,
) -> Result<()> {
    // Check cache first (read lock)
    {
        let cache = AWS_CREDENTIALS.read();
        if let Some((_, expires_at)) = cache.get(client_name) {
            if chrono::Utc::now().timestamp() < *expires_at {
                return Ok(()); // Cache hit, unexpired
            }
        }
    }

    // Load AWS config with 30s timeout
    let sdk_config = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        loader.load(),
    )
    .await
    .map_err(|_| anyhow!("AWS credential resolution timed out after 30 seconds"))?;

    // Resolve credentials from provider chain
    let creds = provider.provide_credentials().await?;

    // Convert expiry: Option<SystemTime> -> i64
    let expires_at = creds
        .expiry()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(|| chrono::Utc::now().timestamp() + 3600); // Default 1h

    // Cache write
    AWS_CREDENTIALS.write().insert(client_name.to_string(), (aws_creds, expires_at));
    Ok(())
}
```

**Key decisions:**
- 30-second timeout prevents TUI/serve hangs on unreachable AWS endpoints
- Default 1-hour expiry for providers that don't return expiry
- Early return on cache hit avoids network calls

### 3. Static Credential Priority

```rust
fn has_static_credentials(&self) -> bool {
    self.get_access_key_id().is_ok() && self.get_secret_access_key().is_ok()
}

async fn resolve_credentials(&self) -> Result<AwsCredentials> {
    if self.has_static_credentials() {
        // Static path: no network, no caching
        Ok(AwsCredentials { ... })
    } else {
        // Chain path: async resolution with caching
        let region = self.get_region()?;
        prepare_aws_credentials(self.name(), &region, self.config.profile.as_deref()).await?;
        get_cached_aws_credentials(self.name())
    }
}
```

Static credentials skip the entire chain mechanism — backwards-compatible, zero overhead.

### 4. PROMPTS Reduction (Region-Only)

```rust
pub const PROMPTS: [PromptAction<'static>; 1] = [
    ("region", "AWS Region", None),
];
```

Removed `access_key_id` and `secret_access_key` from interactive setup. Users configure static creds via:
- YAML config file
- Environment variables (`BEDROCK_ACCESS_KEY_ID`, `BEDROCK_SECRET_ACCESS_KEY`)

This allows chain-based auth (SSO, IAM roles) to work without wizard blocking.

### 5. Profile Configuration

Added `profile: Option<String>` to `BedrockConfig`:

```yaml
clients:
  bedrock:
    profile: my-sso-profile
    region: us-east-1
```

Enables named profile selection from `~/.aws/config` and `~/.aws/credentials`.

## Why This Works

1. **Timeout guard prevents hangs**: `tokio::time::timeout(30s, loader.load())` ensures TUI doesn't freeze on broken IMDSv2/SSO endpoints — user gets actionable error message instead of frozen interface.

2. **Static priority preserves existing behavior**: Users with static keys see no change — no async overhead, no caching, immediate resolution.

3. **Cache eliminates repeated resolution**: Once credentials resolve, subsequent requests read from in-memory cache until expiry. First request pays network cost, remaining requests are instant.

4. **Env-var provider enables unit testing**: Tests can set `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` and call `prepare_aws_credentials` — env-var provider resolves in-process without network calls.

5. **Region-only prompts unblock chain auth**: Removing credential prompts allows users with valid AWS config to complete setup without entering fake static keys.

## Prevention Strategies

**Test Cases:**
```rust
#[tokio::test]
async fn prepare_aws_credentials_populates_cache_from_env() {
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIAENVTEST");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "envsecret");
    
    let result = prepare_aws_credentials("test-client", "us-east-1", None).await;
    assert!(result.is_ok());
    
    let creds = get_cached_aws_credentials("test-client").unwrap();
    assert_eq!(creds.access_key_id, "AKIAENVTEST");
}

#[test]
fn has_static_credentials_true_when_both_set() {
    let config = BedrockConfig {
        access_key_id: Some("key".into()),
        secret_access_key: Some("secret".into()),
        ..Default::default()
    };
    let client = BedrockClient::new_for_test(config);
    assert!(client.has_static_credentials());
}
```

**Best Practices:**
- Always wrap AWS SDK async credential resolution in `tokio::time::timeout`
- Use module-level `LazyLock<RwLock<IndexMap>>` for credential caching, matching existing patterns
- Check static credentials first (`has_static_credentials`) before invoking async chain
- Remove credential prompts from interactive setup when chain auth is primary path

**Code Review Checklist:**
- [ ] Timeout wrapper on all external credential resolution?
- [ ] Static credential path returns immediately (no async)?
- [ ] Cache populated with fixed-size types (avoid cloning large structs)?
- [ ] Expiry fallback reasonable (default 1h for providers without expiry)?
- [ ] Interactive setup allows chain auth (no required credential prompts)?

## Related Issues

- **Issue:** [GH-171](https://github.com/example/harnx/issues/171) — Bedrock missing AWS SSO/credential chain support
- **Pattern Source:** `crates/harnx-client/src/access_token.rs` — VertexAI token caching precedent
- **Related Solution:** [private-oci-registry-auth-2026-05-14.md](./private-oci-registry-auth-2026-05-14.md) — Credential resolution for OCI registries
