use super::{FetchedPackage, PackageFetcher};
use crate::semver_util::parse_semver_tag;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::sync::atomic::AtomicBool;

pub struct GitFetcher;

#[async_trait]
impl PackageFetcher for GitFetcher {
    async fn fetch(&self, url: &str, tag: &str, subpath: Option<&str>) -> Result<FetchedPackage> {
        // Validate semver tag upfront
        parse_semver_tag(tag)?;

        let url = url.to_string();
        let tag = tag.to_string();
        let subpath = subpath.map(str::to_string);

        tokio::task::spawn_blocking(move || {
            // Create temp dir for clone destination
            let clone_dir = tempfile::TempDir::new()?;

            // Clone the repo using gix
            let (mut checkout, _outcome) = gix::prepare_clone(url.as_str(), clone_dir.path())?
                .fetch_then_checkout(gix::progress::Discard, &AtomicBool::new(false))?;
            let (_repo, _outcome) =
                checkout.main_worktree(gix::progress::Discard, &AtomicBool::new(false))?;

            // Checkout the specific tag using git command (simpler than gix API)
            let status = std::process::Command::new("git")
                .args(["-C", &clone_dir.path().to_string_lossy(), "checkout", &tag])
                .status()
                .context("Failed to run git checkout")?;
            if !status.success() {
                bail!("git checkout {tag} failed — tag may not exist in the repository");
            }

            // Get the resolved commit SHA
            let output = std::process::Command::new("git")
                .args([
                    "-C",
                    &clone_dir.path().to_string_lossy(),
                    "rev-parse",
                    "HEAD",
                ])
                .output()
                .context("Failed to run git rev-parse")?;
            let resolved_id = String::from_utf8_lossy(&output.stdout).trim().to_string();

            // Handle subpath: if subpath is set, copy only that subtree to a new TempDir
            let final_dir = if let Some(ref sp) = subpath {
                let src = clone_dir.path().join(sp);
                if !src.is_dir() {
                    bail!("subpath '{sp}' does not exist in the repository");
                }
                let dest = tempfile::TempDir::new()?;
                copy_dir_recursive(&src, dest.path())?;
                dest
            } else {
                // Move the clone dir out as the final dir
                // We need a new TempDir since we can't transfer ownership of clone_dir cleanly
                // while keeping it alive. Instead copy everything.
                let dest = tempfile::TempDir::new()?;
                copy_dir_recursive(clone_dir.path(), dest.path())?;
                dest
            };

            Ok(FetchedPackage {
                dir: final_dir,
                resolved_id,
                tag,
            })
        })
        .await
        .context("spawn_blocking panicked")?
    }

    async fn list_tags(&self, url: &str) -> Result<Vec<semver::Version>> {
        let url = url.to_string();

        tokio::task::spawn_blocking(move || {
            // Shell out to git ls-remote --tags for simplicity and reliability
            let output = std::process::Command::new("git")
                .args(["ls-remote", "--tags", &url])
                .output()
                .context("Failed to run git ls-remote")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("git ls-remote failed: {stderr}");
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut versions = Vec::new();

            for line in stdout.lines() {
                // Lines look like: abc123\trefs/tags/v1.2.3
                // Skip peeled entries (refs/tags/v1.2.3^{})
                if line.contains("^{}") {
                    continue;
                }
                if let Some(refs) = line.split('\t').nth(1) {
                    if let Some(tag_name) = refs.strip_prefix("refs/tags/") {
                        if let Ok(v) = crate::semver_util::parse_semver_tag(tag_name) {
                            versions.push(v);
                        }
                    }
                }
            }

            versions.sort();
            Ok(versions)
        })
        .await
        .context("spawn_blocking panicked")?
    }
}

/// Recursively copy files from `src` to `dst`, skipping `.git` directories.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        // Skip .git directory
        if name == ".git" {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        if src_path.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
