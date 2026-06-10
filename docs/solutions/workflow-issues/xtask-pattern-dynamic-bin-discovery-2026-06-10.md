---
title: "xtask pattern with dynamic workspace binary discovery"
date: 2026-06-10
category: "workflow-issues"
problem_type: workflow_issue
component: "build-automation"
root_cause: "argc bash task runner lacked dynamic bin discovery and required hardcoded install lists"
resolution_type: code_fix
severity: medium
tags:
  - xtask
  - cargo
  - workspace
  - automation
  - binary-discovery
  - install
plan_ref: "xtask-install-792"
---

## Problem

Project automation used argc (bash task runner) for install tasks with hardcoded binary lists. Adding new workspace crates required manual updates to install scripts, risking missed binaries or stale entries.

## Symptoms

- Hardcoded lists of binary names in install scripts
- Manual coordination required when adding new workspace crates
- Risk of forgetting to update install task after adding crates
- Internal dev/test binaries (`harnx-test-bins`) incorrectly included when naively iterating all `kind: bin` targets

## Investigation Steps

1. Evaluated argc bash scripts for install task — found hardcoded bin list
2. Investigated `cargo metadata --format-version 1 --no-deps` output structure
3. Parsed metadata: `workspace_members` array + `packages[]` with targets
4. Discovered targets with `kind: ["bin"]` are binary crates
5. Found internal test-bin crate (`publish = false`) also matched bin filter
6. Tested cargo metadata semantics for `publish` field:
   - `"publish": []` (empty array) = `publish = false` in Cargo.toml
   - `"publish": null` or absent = publishable
7. Applied filter: skip packages where `publish` is empty array

## Root Cause

The argc-based install task had no mechanism for dynamic discovery. Naive bin discovery via cargo metadata iterated all `kind: ["bin"]` targets, but workspace includes `publish = false` crates (test binaries, xtask itself) that should not be installed. Cargo metadata represents `publish = false` as `"publish": []`, enabling a filter that excludes internal crates without hardcoding names.

## Solution

Migrated to Rust `xtask` crate with dynamic bin discovery:

### 1. Create xtask crate

```toml
# xtask/Cargo.toml
[package]
name = "xtask"
version.workspace = true
edition.workspace = true
publish = false

[dependencies]
anyhow.workspace = true
serde_json = { workspace = true }
```

Minimal deps (anyhow + serde_json) for fast compile. Hand-rolled arg parsing avoids clap overhead.

### 2. Add cargo alias

```toml
# .cargo/config.toml (append to existing)
[alias]
xtask = "run --package xtask --"
```

This enables `cargo xtask <task>` syntax. Append to existing config rather than overwriting cross-compile settings.

### 3. Dynamic bin discovery

```rust
fn discover_bins() -> Result<BTreeMap<String, PathBuf>> {
    let metadata = cargo_metadata()?;
    let workspace_members = metadata.workspace_members;
    let mut bins = BTreeMap::new();

    for package in metadata.packages {
        // Skip non-workspace and publish = false packages
        if !workspace_members.contains(package.id.as_str())
            || package.publish.as_ref().is_some_and(Vec::is_empty)
        {
            continue;
        }

        for target in package.targets {
            if target.kind.iter().any(|kind| kind == "bin") {
                bins.insert(target.name, PathBuf::from(&package.manifest_path));
            }
        }
    }

    Ok(bins)
}

fn cargo_metadata() -> Result<Metadata> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .context("failed to run cargo metadata")?;
    // ... parse with serde_json
}
```

**Key filter**: `package.publish.as_ref().is_some_and(Vec::is_empty)` excludes `publish = false` crates.

### 4. Install task

```rust
fn install(args: InstallArgs) -> Result<()> {
    let discovered_bins = discover_bins()?;
    let selected_bins = select_bins(&discovered_bins, &args.bins)?;
    
    let profile_dir = if args.debug { "debug" } else { "release" };
    let mut build = Command::new("cargo");
    build.arg("build").arg("--locked");
    if !args.debug {
        build.arg("--release");
    }
    for bin in &selected_bins {
        build.arg("--bin").arg(bin);
    }
    run_command(&mut build, "cargo build")?;
    
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"));
    let install_dir = cargo_install_root()?.join("bin");
    
    for bin in selected_bins {
        let source = target_dir.join(profile_dir).join(&bin);
        let destination = install_dir.join(&bin);
        fs::copy(&source, &destination)?;
    }
    Ok(())
}

fn cargo_install_root() -> Result<PathBuf> {
    env::var_os("CARGO_INSTALL_ROOT")
        .map(PathBuf::from)
        .or_else(|| env::var_os("CARGO_HOME").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .ok_or_else(|| anyhow!("HOME is not set"))
}
```

Supports: `cargo xtask install` (all bins), `cargo xtask install --debug`, `cargo xtask install harnx harnx-mcp`.

## Why This Works

- **Dynamic discovery**: `cargo metadata` provides full workspace structure; iterating `kind: ["bin"]` targets automatically includes new crates
- **Publish filter**: Excluding `publish: []` packages naturally filters out test utilities and xtask itself with no hardcoded crate names
- **Single build pass**: `cargo build --locked --bin <name> ...` builds all selected bins in one pass
- **Minimal deps**: anyhow + serde_json compile fast, hand-rolled args avoid clap overhead

## Prevention Strategies

**Best Practices:**
- Use `publish = false` consistently for internal/dev-only crates
- Append to `.cargo/config.toml` rather than overwriting
- Keep xtask deps minimal for fast iterative development

**Code Review Checklist:**
- [ ] New publishable crates have binaries automatically included in install
- [ ] Internal crates marked `publish = false` excluded from install
- [ ] `.cargo/config.toml` preserved cross-compile settings when adding alias

## Related Issues

- **Issue:** #792 — Migrate argc to xtask
- **Pattern source:** [cargo-xtask](https://github.com/matklad/cargo-xtask) pattern by matklad
