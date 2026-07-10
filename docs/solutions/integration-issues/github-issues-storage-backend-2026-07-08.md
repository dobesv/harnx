---
title: "GitHub Issues storage backend for plans MCP"
date: 2026-07-08
category: "integration-issues"
problem_type: integration_issue
component: "harnx-mcp-plans-github"
root_cause: "monolithic filesystem MCP server needed storage abstraction for multi-backend support"
resolution_type: code_fix
severity: high
tags:
  - mcp
  - github
  - plans
  - storage-backend
  - rust
  - refactoring
  - concurrency
  - toctou
  - dns-rebinding
  - git
  - subprocess
  - config
plan_ref: "harnx-mcp-plans-github"
last_updated: 2026-07-10
---

## Problem

`harnx-mcp-plans` was a monolithic filesystem-only MCP server with 15 tools (list/get/add/update/delete × plan/task/note). No storage abstraction existed — handlers called `store.rs` (std/tokio fs) directly. Needed to support GitHub Issues as an alternate backend while preserving the existing 54-test filesystem suite unchanged.

## Symptoms

- Storage logic baked into MCP handler layer
- No way to add new backends without duplicating handler/schema/tool code
- Filesystem-specific assumptions (client-provided IDs, hard delete) throughout

## Investigation Steps

1. Analyzed existing `harnx-mcp-plans` crate structure: `handlers.rs` called `store.rs` directly, domain types mixed with filesystem concerns
2. Identified trait boundary: hybrid approach (Oracle decision) — coarse tool-level ops returning domain structs + fine-grained body read/write primitives so shared handler owns `apply_body_edit` + `similar` diff generation
3. Extracted shared library crate `harnx-mcp-plans-core` with `PlanStore` trait, domain types, shared `PlansServer<S: PlanStore>`
4. Implemented `FilePlanStore` and verified 54 existing tests pass unchanged (behavior parity gate)
5. Built `GitHubPlanStore` mapping: plan=issue, task=sub-issue, note=comment, metadata in YAML front-matter
6. Discovered critical GitHub API nuance: `sub_issue_id` uses top-level integer `id`, not `node_id` (see Key Learnings)

## Root Cause

Single-backend architecture with no trait abstraction. MCP protocol layer tightly coupled to filesystem implementation.

## Solution

### 1. Extract `PlanStore` Trait (Hybrid Boundary)

```rust
// crates/harnx-mcp-plans-core/src/store.rs
#[async_trait]
pub trait PlanStore: Send + Sync {
    // Coarse tool-level ops (return domain structs)
    async fn list_plans(&self, page: Option<PageToken>) -> Result<Page<Plan>, StoreError>;
    async fn get_plan(&self, plan: &PlanId) -> Result<Plan, StoreError>;
    async fn add_plan(&self, new_plan: NewPlan) -> Result<Plan, StoreError>;
    async fn update_plan_meta(&self, plan: &PlanId, update: PlanMetaUpdate) -> Result<Plan, StoreError>;
    async fn delete_plan(&self, plan: &PlanId) -> Result<(), StoreError>;
    
    // Fine-grained body primitives (handler owns apply_body_edit + diff)
    async fn read_plan_body(&self, plan: &PlanId) -> Result<String, StoreError>;
    async fn write_plan_body(&self, plan: &PlanId, body: &str) -> Result<(), StoreError>;
    
    // ... analogous methods for tasks and notes
}
```

IDs = opaque `String` across trait (no associated types). GitHub issue numbers stringified; filesystem uses client-provided IDs.

### 2. Shared `PlansServer<S: PlanStore>` + Configurable `ServerMeta`

```rust
// crates/harnx-mcp-plans-core/src/server/handler.rs
pub struct PlansServer<S: PlanStore> {
    store: Arc<S>,
    meta: ServerMeta,  // name, instructions per backend
}

pub struct ServerMeta {
    pub name: &'static str,
    pub instructions: &'static str,
}
```

Two binaries share entire handler/schema/tool layer:
- `harnx-mcp-plans` — `FilePlanStore`, meta = "harnx-mcp-plans" + "File-based..."
- `harnx-mcp-plans-github` — `GitHubPlanStore`, meta = "harnx-mcp-plans-github" + "GitHub Issues..."

### 3. GitHub Issues Storage Backend Mapping

| Plan Resource | GitHub Entity | Key Details |
|---------------|---------------|------------|
| Plan | Issue | Issue number = canonical `plan.id` |
| Task | Sub-issue | REST sub-issues API (GA Apr 2025); issue number = `task.id` |
| Note | Comment | Comment ID = `note.id` |
| Metadata | YAML front-matter in issue body | `client_id`, `jira_key`, `dependencies`, `tags`, `status` |
| Dependencies | `#<n>` mentions in front-matter | NOT sub-issue links (reserved for plan→task parenting) |
| JIRA key | `[PROJ-123]` title prefix | Parsed on decode |

### 4. Rate-Limit + Auth Pattern

```rust
// crates/harnx-mcp-plans-github/src/ratelimit.rs
pub async fn send_rate_limited<F, Fut>(
    executor: &RateLimitExecutor,
    ctx: RequestContext,
    send: F,
) -> Result<reqwest::Response, StoreError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    // Handles: 403/429 retry-after, x-ratelimit-reset, bounded 5xx backoff
    // Threshold abort → StoreError::RateLimited { retry_after_secs }
}
```

Key design:
- Retry/backoff INSIDE impl (not decorator) — per-request token fetch ensures GitHub App installation-token refresh propagates to every call
- Honors `retry-after` + `x-ratelimit-reset`
- Configurable max-wait threshold via `GITHUB_MAX_WAIT_SECS` (default 30s)
- Injectable `Clock` + `Sleeper` for tests
- Serial bulk requests (no `join_all` bursts)

Dropped `octocrab` for raw `reqwest` because:
- `octocrab` baked token at construction — stale App tokens after ~1h
- Per-request token fetch inside `send_rate_limited` closure always fresh

### 5. Capability-Flag Conformance Suite

```rust
// crates/harnx-mcp-plans-core/src/conformance.rs
pub struct BackendCapabilities {
    pub preserves_client_id: bool,    // FS: true, GH: false
    pub deletes_permanently: bool,     // FS: true, GH: false (close)
    pub rejects_invalid_create_ids: bool,  // FS: true, GH: false
}

pub async fn run_conformance<S: PlanStore>(
    store: Arc<S>,
    caps: BackendCapabilities,
) {
    // Core round-trip: capture returned id from add_*, use for all ops
    if caps.preserves_client_id {
        // assert returned id == input client id (filesystem only)
    }
    if caps.deletes_permanently {
        // assert get returns NotFound after delete
    } else {
        // assert item absent from default list_*, but get still works (closed)
    }
}
```

One shared suite validates both backends honestly. FS: `preserves_client_id=true, deletes_permanently=true`. GitHub: `preserves_client_id=false, deletes_permanently=false`.

### 6. Correctness Gotchas Found in Review

**B1. Plan-label isolation on direct-id ops** — `get_plan`/`update_plan_meta`/`delete_plan`/`read_plan_body`/`write_plan_body` must verify the target issue carries `config.plan_label`. Otherwise, known numeric issue number lets server read/close/modify non-plan repo issues (IDOR).

Fix: `ensure_issue_is_plan(plan_number)` check (fetch issue, confirm label) at start of all direct-id plan ops. Return NotFound if not a plan.

**B2. MAX_SUB_ISSUES cap ineffective** — `list_sub_issues` returns only first page (~30), so cap never trips.

Fix: count sub-issues by paginating fully (follow `next` cursors) before comparing to 100.

**B3. Membership checks first-page only** — `ensure_task_belongs_to_plan`/`ensure_note_belongs_to_plan` checked only first page of sub-issues/comments.

Fix: paginate fully when verifying membership (tasks/notes beyond page 1 would incorrectly return NotFound).

**Dedupe tie-break** — equal `updated_at` needs stable secondary key (e.g., issue number) for determinism.

**Body re-encode** — `write_*_body` must re-encode via codec, not `string.replace(old, new)` — front-matter substring corruption risk.

## Why This Works

The hybrid trait boundary separates concerns cleanly:
- **Coarse ops**: Backend orchestrates GitHub-specific concerns (sub-issue parenting, delete=close, dedupe, retry)
- **Body primitives**: Shared handler owns diff generation (`similar`) and `apply_body_edit` uniform across backends
- **Opaque IDs**: No associated types prevents trait from "infecting" schema/handler layer

`PlansServer<S>` generic over `PlanStore` lets two binaries share 100% of handler/schema/tool code. `ServerMeta` makes each backend self-describe correctly to MCP clients.

Capability-flagged conformance validates universal contract (returned id works for subsequent ops) without falsely asserting backend-specific semantics.

## Prevention Strategies

**Test Cases:**
- `add_task_sends_correct_internal_id_in_post_body` — verify sub-issue `sub_issue_id` = issue's top-level `id` field
- `list_tasks_dedupes_by_client_id_keeps_most_recent` — read-side dedupe tie-break
- `add_task_returns_error_when_cap_reached_without_creating_issue` — cap check BEFORE create
- `wrong_plan_*_returns_not_found` — cross-plan isolation
- Shared `run_conformance` against mock GitHub (Wiremock) + FilePlanStore

**Best Practices:**
- Always use GitHub REST issue's top-level `id` field for `sub_issue_id` — never decode `node_id` (modern node_ids are Base64+MessagePack+bitmask; undocumented hack)
- Re-encode bodies via codec, never blanket `string.replace`
- Pagination must follow `next` cursors fully for membership/cap checks
- Label-scope ALL direct-id plan ops (verify issue carries plan label)
- `jsonwebtoken` crate must use `aws_lc_rs` feature to avoid `rustls` CryptoProvider conflict panic

```toml
# Cargo.toml
jsonwebtoken = { version = "10", features = ["aws_lc_rs", "use_pem"] }
```

**Code Review Checklist:**
- [ ] Does this direct-id op verify plan-label ownership?
- [ ] Are pagination checks following full `next` cursor chains?
- [ ] Is body mutation re-encoding via codec?
- [ ] Does dedupe have stable secondary tie-break?
- [ ] Are GitHub API calls using issue's `id` (not `node_id`) for sub-issue linking?

## Known Limitations / Future Work

From Aristarchus final review (non-blocking):

1. **List ops skip label guard** — `list_tasks`/`list_notes` don't assert `ensure_issue_is_plan`. Domain-boundary leak confined to single configured repo. Add label guard for domain purity.

2. **`add_task` non-atomic** — If sub-issue linking fails AFTER issue creation, orphan issue remains (GitHub API has no transaction). Orphan visible/recoverable. Consider best-effort cleanup or reconcile pass.

3. **Cross-page dedupe** — Read-side `client_id` dedupe operates within single page; duplicate `client_id`s spanning pages won't dedupe. Edge case.

4. **chrono+jiff duality** — Auth layer uses `chrono`, domain uses `jiff`. Deliberate layer separation; optional unification.

5. **Deferred** — Docker image for new binary (binary-only release intentional); clap migration; runtime boilerplate dedup; auth token-exchange retry.

## PR #1023 Review Hardening (Commits 7f2d1bde + 893be954)

CodeRabbit review flagged three issues requiring fixes:

### 1. Filesystem `FilePlanStore` Concurrency (TOCTOU + Lost-Update Races)

Shared `Arc<FilePlanStore>` served on HTTP path has real race conditions:

**Problem A — TOCTOU on `add_plan`:** `create_dir_all` + check + write leaves window for concurrent creates. Two requests for same plan ID could both succeed, corrupting state.

**Fix (Commit 7f2d1bde):** Atomic exclusive create on leaf directory:

```rust
let dir = plan_dir(&self.dir, &name);
match std::fs::create_dir(&dir) {  // NOT create_dir_all!
    Ok(()) => {}
    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
        return Err(StoreError::AlreadyExists);
    }
    Err(err) => return Err(StoreError::Backend(err.into())),
}
// Parent dirs created separately with create_dir_all if needed
```

For task/note files, use `OpenOptions::create_new(true)` — fails if file exists.

**Problem B — Lost-update on meta updates:** Read-modify-write without locking allows interleaving: thread A reads, thread B reads, both write, last write wins.

**Fix — Per-entity async lock:** Guard across full RMW span:

```rust
pub struct FilePlanStore {
    dir: PathBuf,
    entity_locks: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

fn entity_lock_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

async fn lock_entity(&self, path: &Path) -> OwnedMutexGuard<()> {
    let key = Self::entity_lock_key(path);
    let lock = {
        let mut locks = self.entity_locks.lock().expect("entity lock map poisoned");
        locks
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    lock.lock_owned().await
}
```

Hold guard across entire RMW:

```rust
async fn update_plan_meta(&self, ...) {
    let dir = plan_dir(&self.dir, &name);
    let _entity_guard = self.lock_entity(&dir).await;  // acquired up front
    // read, modify, write...
}
```

**KEY INSIGHT (Commit 893be954):** Lock must cover ALL RMW paths for entity with SAME key. First pass only locked `update_*_meta`. This left races:
- `update_plan_meta` vs `write_plan_body` (meta-vs-body corruption)
- `update_plan_meta` vs `delete_plan` (delete-during-update)

Fix: `write_plan_body` and `delete_plan` acquire same lock (same key = plan_dir), `write_task_body`/`delete_task` lock on `task_file_path`, `write_note_body`/`delete_note` lock on `note_file_path`.

```rust
async fn write_plan_body(...) {
    let dir = plan_dir(&self.dir, &name);
    let _entity_guard = self.lock_entity(&dir).await;  // same key as update_plan_meta
    // ...
}

async fn delete_plan(...) {
    let dir = plan_dir(&self.dir, &name);
    let _entity_guard = self.lock_entity(&dir).await;  // same key
    // ...
}
```

**Known limitation:** `entity_locks` map grows unbounded (no eviction). Low priority since typical workloads have finite active entities.

### 2. rmcp Secure HTTP Default

Streaming HTTP server in rmcp (v1.7.0) has DNS-rebinding protection via host allowlist (default: localhost/127.0.0.1/::1). Code inappropriately called `.disable_allowed_hosts()`, disabling this guard.

**Fix (Commit 7f2d1bde):** Drop `disable_allowed_hosts()` entirely. Keep rmcp defaults. Default bind to loopback:

```rust
const DEFAULT_HOST: &str = "127.0.0.1";  // was "0.0.0.0"
```

Let operators explicitly opt into wider binds via `--host 0.0.0.0`.

```rust
// runtime.rs — BEFORE:
let server_config = StreamableHttpServerConfig::default()
    .with_stateful_mode(false)
    .disable_allowed_hosts();  // REMOVED

// AFTER:
let server_config = StreamableHttpServerConfig::default()
    .with_stateful_mode(false)
    .with_cancellation_token(ct.child_token());  // keep defaults
```

### 3. base_url Trailing Slash Normalization

Inconsistent handling of trailing slash on `--api-url` / `GITHUB_API_URL` could produce `//` in request URLs.

**Fix (Commit 7f2d1bde):** Normalize at parse time:

```rust
let base_url = first_non_empty(base_url_arg, env("GITHUB_API_URL"))
    .map(|url| url.trim_end_matches('/').to_string())
    .unwrap_or_else(|| DEFAULT_GITHUB_API_URL.to_string());
```

Applied to both `parse_from` (CLI args) and `from_env` paths.

### Verification

Concurrency tests added in `tests/concurrency_fs.rs`:
- `concurrent_add_plan_returns_one_already_exists` — TOCTOU race detection
- Deterministic tests for body-write/delete lock coverage

## PR #1023 Follow-up: Auto-detect repo from git origin

Removed `--repo` / `GITHUB_OWNER_REPO` config — server now auto-detects target GitHub repo from git `origin` remote, failing fast on misconfiguration.

### 1. Provider Injection for Testability

Config parsing accepts injected `origin_provider: impl Fn() -> Result<String>`:

```rust
pub fn parse_from_with(
    args: Args,
    env: &Env,
    origin_provider: impl Fn() -> Result<String>,
) -> Result<AppConfig> { ... }

// Production path shells out:
pub fn parse_from_env_and_args() -> Result<AppConfig> {
    parse_from_with(args, env, || detect_github_repo_from_git_origin())
}

// Tests supply fake origin URL — hermetic, no real git:
#[test]
fn parses_ssh_origin() {
    let config = parse_from_with(args, env, || Ok("git@github.com:owner/repo.git".into()));
    assert_eq!(config.owner, "owner");
}
```

### 2. Pure Parser `parse_github_origin(url)`

Handles SSH and HTTPS, rejects non-`github.com` hosts:

```rust
const GITHUB_HOST: &str = "github.com";

pub fn parse_github_origin(url: &str) -> Result<RepoConfig> {
    // SSH: git@github.com:owner/repo(.git)
    if url.starts_with("git@") {
        let rest = url.strip_prefix("git@").context("SSH prefix")?;
        let (host, path) = rest.split_once(':').context("SSH colon")?;
        ensure_host_is_github(host)?;
        return parse_owner_repo(path.trim_end_matches(".git"));
    }
    // HTTPS: use reqwest::Url::parse, allow trailing slash
    let parsed = Url::parse(url).context("URL parse")?;
    ensure_host_is_github(parsed.host_str().context("host")?)?;
    let mut segments: Vec<_> = parsed.path_segments()
        .context("path segments")?
        .filter(|s| !s.is_empty())
        .collect();
    if segments.last() == Some(&".git") { segments.pop(); }
    ensure!(segments.len() == 2, "expected owner/repo");
    Ok(RepoConfig { owner: segments[0].into(), repo: segments[1].into() })
}

fn ensure_host_is_github(host: &str) -> Result<()> {
    ensure!(host.eq_ignore_ascii_case(GITHUB_HOST), "non-github host: {}", host);
    Ok(())
}
```

### 3. Subprocess Safety + Timeout

Git invoked via `Command::new("git")` with fixed argv — no shell, no user input interpolation:

```rust
const GIT_DETECTION_TIMEOUT: Duration = Duration::from_secs(5);

fn run_git_with_timeout(dir: &Path) -> Result<Output> {
    let mut child = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn git")?;

    match child.wait_timeout(GIT_DETECTION_TIMEOUT).context("wait")? {
        Some(_) => child.wait_with_output().context("collect output"),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git origin detection timed out")
        }
    }
}
```

Dir-targeted helper enables real-subprocess tests with tempdir:

```rust
pub fn detect_github_repo_from_git_origin_in(dir: &Path) -> Result<RepoConfig> {
    // Verify inside git repo, then get origin URL
    run_git(["rev-parse", "--is-inside-work-tree"], dir)?;
    let output = run_git_with_timeout(dir)?;
    parse_github_origin(&stdout)
}

pub fn detect_github_repo_from_git_origin() -> Result<RepoConfig> {
    detect_github_repo_from_git_origin_in(std::env::current_dir()?)
}
```

### 4. Fail-Fast Startup Errors

Clear errors at config parse time:

| Condition | Error |
|-----------|-------|
| Not inside git repo | `"not inside a git repository"` |
| No `origin` remote | `"no 'origin' remote configured"` |
| Non-github.com origin | `"non-github host: <hostname>"` |
| Unparseable/empty origin | `"could not parse origin URL"` |

Repo access/permission errors surface via existing GitHub API startup validation.

### 5. Test Strategy

**Pure parser unit tests** (`config.rs`):
- SSH: `git@github.com:owner/repo.git` → OK
- HTTPS: `https://github.com/owner/repo` (with/without `.git`, trailing slash) → OK
- Non-github host → error names offending host
- Garbage/empty → parse error

**Real-git integration tests** (`config.rs`):
```rust
#[test]
fn detects_repo_from_real_git_origin() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    run_git(["init"], tmp.path())?;
    run_git(["remote", "add", "origin", "https://github.com/test-owner/test-repo"], tmp.path())?;
    let cfg = detect_github_repo_from_git_origin_in(tmp.path())?;
    assert_eq!(cfg.owner, "test-owner");
    assert_eq!(cfg.repo, "test-repo");
    Ok(())
}
```

Error paths: non-git-dir, repo-without-origin.

### Note

`wait-timeout` added as direct crate dep (not via `[workspace.dependencies]`) — candidate for future consistency cleanup.

## Related Issues

- **GitHub:** [#949](https://github.com/dobesv/harnx/issues/949) — Original issue
- **Related Solution:** [api-design/mcp-plans-surgical-edit-api-2026-05-11.md](../api-design/mcp-plans-surgical-edit-api-2026-05-11.md) — Body-edit mutual exclusion, diff output
- **Related Solution:** [logic-errors/mcp-plans-rewrite-patterns-2026-05-04.md](../logic-errors/mcp-plans-rewrite-patterns-2026-05-04.md) — Atomic writes, ID normalization
- **Related Solution:** [integration-issues/plan-note-file-storage-2026-05-03.md](../integration-issues/plan-note-file-storage-2026-05-03.md) — Per-file notes, JSON overview
