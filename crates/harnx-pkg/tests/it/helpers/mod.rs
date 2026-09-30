use std::path::Path;
use tempfile::TempDir;

/// Create a non-bare git repo with given files committed, then create given tags.
/// Returns (TempDir holding the repo, file:// URL).
/// Uses std::process::Command("git") for simplicity.
pub fn create_test_git_repo(files: &[(&str, &str)], tags: &[&str]) -> (TempDir, String) {
    let repo_dir = TempDir::new().expect("create temp dir");
    let path = repo_dir.path();

    // Init repo
    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test"]);

    // Write files
    for (rel_path, content) in files {
        let full = path.join(rel_path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, content).unwrap();
    }

    // Commit
    run_git(path, &["add", "."]);
    run_git(path, &["commit", "-m", "Initial commit"]);

    // Create tags
    for tag in tags {
        run_git(path, &["tag", tag]);
    }

    let url = format!("file://{}", path.display());
    (repo_dir, url)
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git command failed");
    assert!(status.success(), "git {:?} failed in {:?}", args, dir);
}
