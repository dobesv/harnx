use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::diff::diff_commits_blocking;
use crate::DiffLevel;

const SHORT_ID_LEN: usize = 12;
const MIN_ID_LEN: usize = 7;
const MAX_ID_LEN: usize = 40;
const STAT_ARGS: [&str; 2] = ["--stat=160", "--stat-count=50"];

/// Runs git in `dir` with a fixed locale and no colour or external diff
/// drivers, so the text does not depend on the user's configuration.
fn run_git(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))
}

fn git_diff_text(dir: &Path, flags: &[&str], before: &str, after: &str) -> Result<String> {
    let mut args = vec!["diff", "--no-color", "--no-ext-diff"];
    args.extend_from_slice(flags);
    args.extend([before, after]);
    let output = run_git(dir, &args)?;
    if !output.status.success() {
        anyhow::bail!(
            "git diff failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).context("git diff output not utf-8")
}

fn short_id(id: gix::ObjectId) -> String {
    id.to_hex().to_string()[..SHORT_ID_LEN].to_owned()
}

/// The stat lines without git's closing summary line.
fn stat_lines(stat: &str) -> String {
    let mut lines: Vec<&str> = stat.lines().collect();
    lines.pop();
    lines.iter().map(|line| format!("{line}\n")).collect()
}

fn summary_line(
    workdir: &Path,
    shortstat: &str,
    before: gix::ObjectId,
    after: gix::ObjectId,
) -> String {
    let repo = shell_words::quote(&workdir.to_string_lossy()).into_owned();
    format!(
        "{}: git -C {repo} diff {} {}\n",
        shortstat.trim(),
        short_id(before),
        short_id(after)
    )
}

fn render_stat_levels(
    workdir: &Path,
    before: gix::ObjectId,
    after: gix::ObjectId,
    level: DiffLevel,
) -> Result<String> {
    let (b, a) = (before.to_hex().to_string(), after.to_hex().to_string());
    let shortstat = git_diff_text(workdir, &["--shortstat"], &b, &a)?;
    if shortstat.trim().is_empty() {
        return Ok(String::new());
    }
    let mut text = summary_line(workdir, &shortstat, before, after);
    if level == DiffLevel::Files {
        text.push_str(&stat_lines(&git_diff_text(workdir, &STAT_ARGS, &b, &a)?));
    }
    Ok(text)
}

fn tree_id(repo: &gix::Repository, commit: gix::ObjectId) -> Result<gix::ObjectId> {
    let tree = repo
        .find_object(commit)
        .with_context(|| format!("find commit {commit}"))?
        .peel_to_tree()
        .with_context(|| format!("peel commit {commit} to tree"))?;
    Ok(tree.id)
}

pub(crate) fn render_change_blocking(
    repo: &gix::Repository,
    before: gix::ObjectId,
    after: gix::ObjectId,
    level: DiffLevel,
) -> Result<String> {
    // Snapshot commits differ even when nothing changed, so equal trees are
    // the cheap way to skip running git at all.
    if level != DiffLevel::None && tree_id(repo, before)? == tree_id(repo, after)? {
        return Ok(String::new());
    }
    match level {
        DiffLevel::None => Ok(String::new()),
        DiffLevel::Full => diff_commits_blocking(repo, before, after),
        DiffLevel::Summary | DiffLevel::Files => {
            let workdir = repo.workdir().unwrap_or_else(|| repo.path());
            render_stat_levels(workdir, before, after, level)
        }
    }
}

fn check_id_shape(id: &str) -> Result<()> {
    let hex = id.chars().all(|c| c.is_ascii_hexdigit());
    if hex && (MIN_ID_LEN..=MAX_ID_LEN).contains(&id.len()) {
        return Ok(());
    }
    anyhow::bail!("commit id must be {MIN_ID_LEN} to {MAX_ID_LEN} hex digits: {id}")
}

/// The resolved id, only when it starts with what the caller typed. Git
/// resolves ref names before object ids, so a branch called `deadbeef` would
/// otherwise stand in for a commit id.
fn named_by_prefix(stdout: &[u8], id: &str) -> Option<gix::ObjectId> {
    let resolved =
        gix::ObjectId::from_hex(String::from_utf8_lossy(stdout).trim().as_bytes()).ok()?;
    let full = resolved.to_hex().to_string();
    full.starts_with(&id.to_ascii_lowercase())
        .then_some(resolved)
}

/// A snapshot commit from its full id or a unique prefix of 7 or more hex digits.
pub fn resolve_commit_id(repo_workdir: &Path, id: &str) -> Result<gix::ObjectId> {
    check_id_shape(id)?;
    let spec = format!("{id}^{{commit}}");
    // No `--quiet`: it hides the "ambiguous" message that tells the two
    // failures apart.
    let output = run_git(repo_workdir, &["rev-parse", "--verify", &spec])?;
    let repo = repo_workdir.display();
    if output.status.success() {
        return named_by_prefix(&output.stdout, id)
            .with_context(|| format!("no snapshot {id} in {repo}"));
    }
    if String::from_utf8_lossy(&output.stderr).contains("ambiguous") {
        anyhow::bail!("{id} names more than one object in {repo}");
    }
    anyhow::bail!("no snapshot {id} in {repo}")
}

#[cfg(test)]
mod tests;
