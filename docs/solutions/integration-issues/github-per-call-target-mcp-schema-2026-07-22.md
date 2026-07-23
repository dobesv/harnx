---
title: "Per-call repository targeting and dynamic MCP schema requiredness for GitHub plans backend"
date: 2026-07-22
category: "integration-issues"
problem_type: integration_issue
component: "harnx-mcp-plans-github"
root_cause: "Fixed startup detection and single-repo constraint prevented operation outside git repos and per-call repository targeting"
resolution_type: code_fix
severity: high
tags:
  - mcp
  - github
  - plans
  - storage-backend
  - rust
  - json-schema
  - security
  - path-traversal
  - validation
plan_ref: "harnx-1137-github-plans-startup-and-targeting"
---

## Problem

GitHub plans MCP server required startup inside a git repo with GitHub origin and bound all operations to a single fixed repository. Per-call repository selection was impossible, and startup failed outside git repos or without GitHub origin. Label validation at startup blocked server initialization even when labels were optional.

## Symptoms

```
Error: current working directory is not inside a git repository
```

```
startup validation failed: cannot ensure label 'harnx-plan'
```

- MCP tool callers could not specify target repository per call
- Server unusable outside git repos or in repos without GitHub remote
- Label validation blocked startup even though labels are best-effort metadata
- Schema showed `owner`/`repo` as required or absent based on compile-time config, not runtime detection

## Investigation Steps

Reviewed existing `PlanStore` trait (21 methods across plans/tasks/notes) and `GitHubPlanStore` design. Found single `GitHubClient` owner/repo bound at startup. Evaluated two options:

1. **StoreManager layer above server** — duplicate handlers for validation, error mapping, body diffs
2. **Target parameter on trait methods** — clean seam, factory produces per-target clients

Chose option 2. Traced proposed `owner`/`repo` params through handlers → store methods → client construction. Identified security gap: caller-controlled path segments concatenated into GitHub API URLs without validation. `url` crate performs RFC 3986 dot-segment normalization — `repo="../../../user"` escapes `/repos/{owner}/{repo}` prefix.

Pre-diff, owner/repo were fixed at startup from git-origin detection. This diff introduces caller-controlled targeting, creating new attack surface requiring validation at trust boundary.

## Root Cause

### Architecture Constraint

Single `GitHubClient` instance bound to one repo at construction. No per-call target parameter on `PlanStore` trait. Handlers had no mechanism to resolve or pass target context.

### Security Vulnerability

Caller-supplied `owner`/`repo` interpolated into URL paths via `format!("/repos/{}/{}/...", self.owner, self.repo)`. No validation at `resolve_target()`. `url` 2.5.8 normalizes `../` segments per RFC 3986, escaping `/repos/{owner}/{repo}` prefix. Attacker could reach arbitrary GitHub API endpoints (`/user`, `/orgs/{org}/members`, `/app/installations`) using server's credential.

Example exploit: `owner="foo", repo="../../../user"` → `https://api.github.com/user` (escapes repo path).

### Schema Static Generation

`impl_json_schema!` macro produced static schemas at compile time. MCP `tools/list` had no mechanism to mark `owner`/`repo` as required only when no default repo detected at runtime.

## Solution

### 1. Target Threading Through Shared Trait

Added `target: &Target` as first argument after `&self` on all 21 `PlanStore` methods:

```rust
pub enum Target {
    Local,
    GitHub(RepoTarget),
}

pub struct RepoTarget {
    pub owner: String,
    pub repo: String,
}

pub trait PlanStore: Send + Sync {
    async fn list_plans(&self, target: &Target, page: Option<PageToken>) -> Result<...>;
    async fn get_plan(&self, target: &Target, name: &str) -> Result<...>;
    // ... 19 more methods
}
```

Filesystem backend ignores `_target`. GitHub backend uses factory to produce repo-bound clients:

```rust
pub struct GitHubClientFactory {
    auth: GitHubAuth,              // Arc-wrapped token cache
    raw_http: Client,              // Arc-wrapped connection pool
    ratelimit: Arc<RateLimitExecutor>, // Shared rate limiter
    base_url: String,
    default_repo: Option<RepoTarget>,
}

impl GitHubClientFactory {
    pub fn client_for(&self, target: &RepoTarget) -> GitHubClient {
        GitHubClient {
            auth: self.auth.clone(),
            owner: target.owner.clone(),
            repo: target.repo.clone(),
            raw_http: self.raw_http.clone(),
            ratelimit: self.ratelimit.clone(),
            base_url: self.base_url.clone(),
        }
    }
}
```

Client construction is cheap (Arc clones + string clones). No HTTP client rebuild, no token re-fetch.

### 2. Target Validation at Trust Boundary

Critical: validate caller-controlled `owner`/`repo` before constructing URLs.

**Validation rule** (both owner and repo):
- Non-empty
- Explicit rejection of `.` and `..` (equality check, not substring)
- No `/` or `\`
- No whitespace or control characters
- ASCII allowlist: `[A-Za-z0-9._-]`

```rust
impl RepoTarget {
    pub fn validate(value: &str) -> Result<(), String> {
        if value.is_empty() {
            return Err("cannot be empty".into());
        }
        if value == "." || value == ".." {
            return Err("cannot be '.' or '..'".into());
        }
        if value.contains('/') || value.contains('\\') {
            return Err("cannot contain path separators".into());
        }
        if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("cannot contain whitespace or control characters".into());
        }
        if !value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err("must contain only alphanumeric, '.', '_', or '-'".into());
        }
        Ok(())
    }
}
```

**Why explicit `.`/`..` rejection is necessary**: The allowlist includes `.`. A naive `^[A-Za-z0-9._-]+$` regex would accept `..` because each `.` is individually valid. The explicit equality check closes this gap.

**Defense in depth**:
1. Core `resolve_target()` validates explicit params and default-repo fallback
2. GitHub store `client_for_store_target()` re-validates before client creation
3. Client `repo_endpoint()` percent-encodes owner/repo path segments

```rust
fn repo_endpoint(&self, suffix: &str) -> String {
    format!("/repos/{}/{}/{}",
        encode_path_segment(&self.owner),
        encode_path_segment(&self.repo),
        suffix
    )
}

fn encode_path_segment(segment: &str) -> String {
    segment.chars().flat_map(|c| {
        if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
            vec![c]
        } else {
            format!("%{:02X}", c as u8).chars().collect()
        }
    }).collect()
}
```

### 3. Dynamic MCP Schema Requiredness

Post-process static schemars schemas in `list_tools` based on server-held `TargetPolicy`:

```rust
pub enum TargetPolicy {
    #[default]
    None,  // Filesystem — no owner/repo params
    GitHub { default_repo: Option<RepoTarget> },
}

impl TargetPolicy {
    pub fn apply_to_tool_schema(&self, tool: &mut Tool) {
        match self {
            TargetPolicy::None => {}
            TargetPolicy::GitHub { default_repo } => {
                // Inject owner/repo into properties
                // If default_repo is None, add to required array
            }
        }
    }
}
```

**With default repo detected**: `owner`/`repo` not in `required`, descriptions say "Optional. Defaults to `<owner>/<repo>` detected at startup."

**Without default repo**: `owner`/`repo` in `required`, descriptions say "Required. No default repository was detected at startup."

Test proves both cases:

```rust
#[test]
fn github_tool_schema_owner_repo_requiredness_tracks_default_repo() {
    // With default_repo → owner/repo NOT in required
    // Without default_repo → owner/repo IN required
}
```

### 4. Non-Fatal Startup Detection

Startup attempts cwd git-origin detection. On failure, logs warning and continues with `default_repo: None`. No GitHub API validation at startup. Label ensure moved to `add_plan()` with warning-only behavior.

```rust
let default_repo = match origin_provider().and_then(|origin| parse_github_origin(&origin)) {
    Ok(repo) => Some(repo),
    Err(err) => {
        eprintln!("warning: could not detect default GitHub repository: {err}");
        None
    }
};
```

## Why This Works

**Target threading** keeps `PlanStore` stateless relative to repos. Factory pattern avoids expensive per-call client reconstruction while enabling per-call target selection. Filesystem backend unchanged (zero diff).

**Validation at trust boundary** prevents path traversal. Explicit `.`/`..` check closes allowlist gap. Percent-encoding provides defense-in-depth. Regression test verifies zero HTTP requests for traversal payloads.

**Dynamic schema injection** occurs once per `tools/list` call (MCP protocol), not per `call_tool`. Schema reflects actual server state (detected repo or not).

**Non-fatal startup** gracefully degrades: auto-detect if available, explicit params otherwise. Label failures logged as warnings, operations proceed.

## Prevention Strategies

### Security

- **Validate at trust boundary**: Caller-supplied path segments must be validated before reaching URL construction
- **Explicit `.`/`..` rejection**: Allowlist alone is insufficient when `.` is a valid character
- **Regression test for path traversal**: Assert zero HTTP requests when traversal payloads provided

```rust
#[test]
fn explicit_target_rejects_path_traversal_without_request() {
    let mock = Wiremock::start();
    let store = build_store(mock.uri());
    
    // Attempt traversal payload
    let result = store.get_plan(
        &Target::GitHub(RepoTarget { owner: "../../../user".into(), repo: "repo".into() }),
        "plan-name"
    ).await;
    
    assert!(result.is_err());
    // Critical: assert ZERO requests made
    assert_eq!(mock.received_requests().await.len(), 0);
}
```

### Architecture

- **Thread target/context through shared trait**: Prefer single clean seam over manager layers duplicating handler logic
- **Factory for per-call resources**: Clone Arc-wrapped state (auth, HTTP client, rate limiter) rather than rebuild
- **Separate storage target from plan metadata**: `owner`/`repo` params for storage selection, `github_owner_repo` field for plan metadata — never conflate

### Schema

- **Post-process static schemas rather than generate dynamically**: Static `impl_json_schema!` for base, policy-driven injection for conditional fields
- **Test both branches**: Verify schema with and without default repo

### Test Coverage

- **Test coverage signal**: Modified test files in PR exercising changed code satisfies policy
- **Wiremock for HTTP assertions**: Mock server verifies request paths match expected targets
- **Zero-request assertion**: For security tests, verify malicious payloads never reach the network

## Related Issues

- **GitHub:** [#1137](https://github.com/dobesv/harnx/issues/1137) — Per-call repository targeting and non-fatal startup
- **Related Solution:** [github-issues-storage-backend-2026-07-08.md](github-issues-storage-backend-2026-07-08.md) — Original GitHub backend, superseded fail-fast startup
- **Related Solution:** [mcp-tool-template-design-guidelines-2026-05-08.md](mcp-tool-template-design-guidelines-2026-05-08.md) — MCP tool schema design
- **Related Solution:** [api-design/per-call-env-param-bash-mcp-2026-05-13.md](../api-design/per-call-env-param-bash-mcp-2026-05-13.md) — Per-call parameter pattern
