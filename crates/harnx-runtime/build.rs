//! Stamp the git commit into the binary so the startup log line can prove
//! which build is running (see `bootstrap::setup_logger`). Best-effort: falls
//! back to "unknown" when git or the `.git` directory is unavailable (e.g.
//! building from a packaged crate).
use std::path::Path;
use std::process::Command;

fn main() {
    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    // `--no-optional-locks` keeps `status` from refreshing and rewriting the
    // index. Without it this build script bumped the index mtime every time it
    // ran, and — while the index was also a `rerun-if-changed` input — that made
    // it dirty its own fingerprint, so every cargo invocation rebuilt
    // harnx-runtime and everything downstream.
    let dirty = git(&["--no-optional-locks", "status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let stamp = if dirty { format!("{sha}-dirty") } else { sha };
    println!("cargo:rustc-env=HARNX_BUILD_SHA={stamp}");
    // Re-stamp when the checked-out commit changes. Ask git for these paths
    // rather than assuming `../../.git/`: in a linked worktree `.git` is a file
    // pointing at `.git/worktrees/<name>/`, so a hardcoded path does not exist —
    // and cargo treats a missing `rerun-if-changed` file as changed, which is a
    // second way to keep this script permanently dirty.
    //
    // Three paths, because no single one covers every way the commit moves.
    // `git commit` does not touch HEAD at all — HEAD holds `ref: refs/heads/x`,
    // and only the ref file underneath it moves — so HEAD alone catches checkout
    // and branch switches but silently misses commits on the current branch.
    watch_git_path("HEAD");
    watch_head_ref();
    // A ref that lives in packed-refs has no loose file to watch.
    watch_git_path("packed-refs");
    // The index is deliberately not watched: that tied a full rebuild of every
    // downstream crate to `git add` and to anything else that refreshes the
    // index, which costs minutes for a marker used in one log line. So `-dirty`
    // can lag — it reflects the tree as of the last time this crate compiled.
}

/// Watch the ref HEAD points at, which is what actually moves on `git commit`.
fn watch_head_ref() {
    // Fails on a detached HEAD, where the sha lives in HEAD itself and the watch
    // on HEAD already covers it.
    let Some(reference) = git(&["symbolic-ref", "--quiet", "HEAD"]) else {
        return;
    };
    watch_git_path(&reference);
}

fn watch_git_path(name: &str) {
    let Some(path) = git(&["rev-parse", "--git-path", name]) else {
        return;
    };
    // Same trap as above: only watch it if it's really there.
    if Path::new(&path).exists() {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
