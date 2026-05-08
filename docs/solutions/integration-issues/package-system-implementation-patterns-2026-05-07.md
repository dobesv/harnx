---
title: "Package system implementation patterns: OCI registry testing, binary crate structure, and source classification"
date: 2026-05-07
category: integration-issues
problem_type: integration_issue
component: harnx-pkg
root_cause: test infrastructure gaps and architectural constraints requiring workarounds
resolution_type: code_fix
severity: medium
tags:
  - oci
  - testing
  - axum
  - binary-crate
  - spawn_blocking
  - gix
  - package-manager
plan_ref: harnx-package-system
---

## Problem

Building a package manager for harnx required solving several non-obvious integration challenges: testing OCI fetcher without a real registry, exposing binary crate modules to integration tests, blocking git operations in async context, and reliably classifying OCI vs git URLs for correct source persistence.

## Symptoms

- `registry-testkit` 0.1.3 fails with multi-segment repository names (e.g., `owner/repo`) due to axum path param limitations
- `registry-testkit` lacks `/tags/list` endpoint needed for version discovery
- Integration tests couldn't access modules in `harnx-pkg` binary crate
- Git clone via `gix` blocks async runtime, causing potential stalls
- OCI packages incorrectly saved as `PackageSource::Git`, breaking `update` and `check-for-updates`

## Investigation Steps

1. Attempted `registry-testkit` for OCI integration tests; hit axum path param limitation with `/v2/:name/:reference/manifests` routes — single `:name` param can't capture `owner/repo`
2. Checked `registry-testkit` source for `/tags/list` support; missing entirely
3. Binary crate `harnx-pkg` couldn't expose modules to `tests/` directory; Rust doesn't allow integration tests to access binary crate internals
4. Considered pure `gix` implementation for git operations; sparse checkout API too complex for required use case
5. URL classification by pattern matching; needed reliable heuristic to distinguish `PackageSource::Oci` vs `PackageSource::Git`

## Root Cause

- **OCI test registry**: Existing test infrastructure (`registry-testkit`) designed for single-segment image names and lacked required endpoints
- **Binary crate tests**: Rust restriction — integration tests in `tests/` can only access the crate's public library API, not internal modules of a binary-only crate
- **Blocking git operations**: `gix::prepare_clone` is synchronous and blocking; calling directly in async context blocks the tokio runtime
- **Source classification**: URLs like `ghcr.io/owner/repo` ambiguous — could be OCI or git; improper classification caused wrong `PackageSource` variant to be persisted

## Solution

### 1. In-process OCI registry shim

Built custom test registry directly in test file using axum:

```rust
// crates/harnx-pkg/tests/oci_fetcher.rs
use axum::{
    routing::{get, put, post},
    Router,
};

struct TestRegistry {
    port: u16,
    handle: JoinHandle<()>,
}

impl TestRegistry {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let app = Router::new()
            .route("/v2/:name/blobs/:digest", get(get_blob).put(put_blob))
            .route("/v2/:name/manifests/:reference", get(get_manifest).put(put_manifest))
            .route("/v2/:name/tags/list", get(list_tags));

        let handle = tokio::spawn(async {
            axum::serve(listener, app).await.unwrap();
        });

        Self { port, handle }
    }
}
```

Key routes implemented:
- Blob upload/download with proper digest headers
- Manifest put/get with `Docker-Content-Digest` header
- Tag listing via `/v2/:name/tags/list`

Seeding packages via `oci-client::Client::push()`:

```rust
let client = Client::new(ClientConfig {
    protocol: ClientProtocol::Http,  // Required for localhost
    use_monolithic_push: true,
    ..Default::default()
});
client.push(&reference, &layers, config, &RegistryAuth::Anonymous, Some(image_manifest)).await?;
```

### 2. Lib.rs in binary crate

Added `src/lib.rs` exposing public modules:

```rust
// crates/harnx-pkg/src/lib.rs
pub mod cli;
pub mod commands;
pub mod fetch;
pub mod install;
pub mod semver_util;
```

Now integration tests can import:

```rust
use harnx_pkg::fetch::{oci::OciFetcher, PackageFetcher};
use harnx_pkg::semver_util::parse_semver_tag;
```

### 3. Gix + spawn_blocking + shell git

Clone with `gix`, checkout with shell `git`:

```rust
// crates/harnx-pkg/src/fetch/git.rs
async fn fetch(&self, url: &str, tag: &str, subpath: Option<&str>) -> Result<FetchedPackage> {
    let url = url.to_string();
    let tag = tag.to_string();

    tokio::task::spawn_blocking(move || {
        // Clone with gix
        let (mut checkout, _) = gix::prepare_clone(&url, clone_dir.path())?
            .fetch_then_checkout(gix::progress::Discard, &AtomicBool::new(false))?;

        // Checkout specific tag via shell git (gix sparse checkout too complex)
        std::process::Command::new("git")
            .args(["checkout", &tag])
            .current_dir(clone_dir.path())
            .status()?;

        // Copy to final destination
        copy_dir_recursive(clone_dir.path(), final_dir.path())?;
        Ok(FetchedPackage { dir: final_dir, resolved_id, tag })
    })
    .await
    .context("spawn_blocking panicked")?
}
```

### 4. URL blacklist classification

Classify OCI vs git by blacklisting known git patterns:

```rust
// crates/harnx-pkg/src/commands/add.rs
pub fn is_oci_url(url: &str) -> bool {
    if url.starts_with("oci://") {
        return true;
    }
    // Blacklist known git indicators
    !url.ends_with(".git")
        && !url.contains("github.com")
        && !url.contains("gitlab.com")
        && !url.contains("bitbucket")
        && !url.starts_with("file://")
        && !url.starts_with("git://")
        && !url.starts_with("ssh://")
}

fn build_source(url: &str, tag: &str, fetched: &FetchedPackage, subpath: Option<&str>, is_oci: bool) -> PackageSource {
    if is_oci {
        PackageSource::Oci { url: url.to_string(), tag: tag.to_string(), digest: fetched.resolved_id.clone(), subpath: subpath.map(str::to_string) }
    } else {
        PackageSource::Git { url: url.to_string(), tag: tag.to_string(), commit: fetched.resolved_id.clone(), subpath: subpath.map(str::to_string) }
    }
}
```

Critical: `is_oci` boolean passed explicitly to `build_source()` ensures correct variant persisted.

## Why This Works

- **In-process registry**: Full control over endpoints and behavior; no external container dependency; supports multi-segment names via axum routing with proper path params
- **Lib.rs pattern**: Cargo treats crate as both library and binary; `lib.rs` exports modules, `main.rs` imports via `use harnx_pkg::cli;` and runs CLI
- **spawn_blocking**: Moves blocking operation to separate thread pool, freeing tokio runtime for other tasks; `.await` unwraps JoinHandle result
- **Blacklist classification**: Explicit is-better-than-implicit; URL heuristics reliable for known hosts; unknown URLs default to git with warning

## Prevention Strategies

**Test Cases:**
- OCI integration: `test_oci_fetch_basic`, `test_oci_list_tags` verify fetch and tag discovery
- Git integration: `test_git_fetch_basic`, `test_git_list_tags` (similar pattern)
- Source persistence: assert manifest contains correct `type: oci` vs `type: git`

**Best Practices:**
- Always wrap blocking operations in `spawn_blocking` for async code
- Build in-process test doubles when existing test infrastructure doesn't match requirements
- Expose `lib.rs` in binary crates for test access; import in `main.rs` to avoid duplication
- Classify variants via explicit parameter passing, not re-deriving from ambiguous data

**Code Review Checklist:**
- [ ] Does async code call blocking functions directly?
- [ ] Are test dependencies real external services or shims?
- [ ] Does binary crate have `lib.rs` for integration test access?
- [ ] Is classification logic explicit and underivable from persisted state?

## Related Issues

- **PR:** feat/package-system branch
- **Commit:** `9e920a06` — fix(pkg): address Aristarchus blockers - OCI source recording bug and OCI test coverage
- **Final Report Note:** Documented in `harnx-package-system` plan as blocker #1 and #2
