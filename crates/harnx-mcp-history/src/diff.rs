use std::process::Command;

use anyhow::{Context, Result};

const MAX_DIFF_BYTES: usize = 50_000;

pub fn diff_commits_blocking(
    repo: &gix::Repository,
    before_id: gix::ObjectId,
    after_id: gix::ObjectId,
) -> Result<String> {
    let _ = repo
        .find_object(before_id)
        .context("find before commit")?
        .peel_to_tree()
        .context("peel before commit to tree")?;
    let _ = repo
        .find_object(after_id)
        .context("find after commit")?
        .peel_to_tree()
        .context("peel after commit to tree")?;

    let after_commit = repo
        .find_object(after_id)
        .context("find after commit for header")?
        .peel_to_commit()
        .context("peel after commit for header")?;
    let title = after_commit
        .message()
        .map(|m| m.title.to_string())
        .unwrap_or_else(|_| String::from("harnx snapshot"));
    let header = format!("commit {}\n    {}\n\n", after_id.to_hex(), title.trim());

    let workdir = repo.workdir().unwrap_or_else(|| repo.path());
    let output = Command::new("git")
        .arg("diff")
        .arg(before_id.to_hex().to_string())
        .arg(after_id.to_hex().to_string())
        .current_dir(workdir)
        .output()
        .context("run git diff")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {stderr}");
    }

    let mut diff = header;
    diff.push_str(&String::from_utf8(output.stdout).context("git diff output not utf-8")?);
    if diff.len() > MAX_DIFF_BYTES {
        // Truncate to a char boundary — String::truncate panics on a mid-char cut
        let mut cut = MAX_DIFF_BYTES;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
        let short = &after_id.to_hex().to_string()[..12];
        diff.push_str(&format!(
            "\n[ ... diff truncated, use 'git show {}' to view full diff ... ]",
            short
        ));
    }
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git command runs");
        assert!(status.success(), "git {:?} failed", args);
    }

    fn output_git(dir: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git output command runs");
        assert!(output.status.success(), "git {:?} failed", args);
        String::from_utf8(output.stdout).expect("utf8 git output")
    }

    #[test]
    fn test_diff_commits() {
        let temp = tempdir().expect("tempdir");
        run_git(temp.path(), &["init"]);
        run_git(temp.path(), &["config", "user.name", "Test User"]);
        run_git(temp.path(), &["config", "user.email", "test@example.com"]);

        let file = temp.path().join("note.txt");
        fs::write(&file, "before\n").expect("write before");
        run_git(temp.path(), &["add", "."]);
        run_git(temp.path(), &["commit", "-m", "before"]);
        let before = output_git(temp.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();

        fs::write(&file, "after\n").expect("write after");
        run_git(temp.path(), &["add", "."]);
        run_git(temp.path(), &["commit", "-m", "after"]);
        let after = output_git(temp.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();

        let repo = gix::open(temp.path()).expect("open repo");
        let diff = diff_commits_blocking(
            &repo,
            gix::ObjectId::from_hex(before.as_bytes()).expect("before oid"),
            gix::ObjectId::from_hex(after.as_bytes()).expect("after oid"),
        )
        .expect("diff works");

        assert!(diff.starts_with("commit "));
        assert!(diff.contains(&after));
        assert!(diff.contains("-before"));
        assert!(diff.contains("+after"));
    }
}
