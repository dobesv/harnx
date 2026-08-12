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
    // Re-stamp when the checked-out commit changes. Ask git where HEAD lives
    // instead of assuming `../../.git/HEAD`: in a linked worktree `.git` is a
    // file pointing at `.git/worktrees/<name>/`, so the hardcoded path did not
    // exist — and cargo treats a missing `rerun-if-changed` file as changed,
    // which was a second way to keep this script permanently dirty.
    //
    // The index is deliberately not watched. Doing so tied a full rebuild of
    // every downstream crate to `git add` (and to anything else that refreshes
    // the index), which costs minutes for a marker that is only used in one log
    // line. The tradeoff: `-dirty` reflects the tree as of the last time this
    // crate was compiled, not necessarily the current working tree.
    watch_git_path("HEAD");
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
