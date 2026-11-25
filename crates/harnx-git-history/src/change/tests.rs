use super::*;
use std::fs;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).expect("utf8")
}

fn init(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir");
    git(dir, &["init"]);
    git(dir, &["config", "user.name", "Test User"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "core.autocrlf", "false"]);
}

fn commit(dir: &Path, files: &[(&str, &str)]) -> gix::ObjectId {
    for (name, body) in files {
        fs::write(dir.join(name), body).expect("write");
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "snap", "--allow-empty"]);
    let id = git(dir, &["rev-parse", "HEAD"]);
    gix::ObjectId::from_hex(id.trim().as_bytes()).expect("oid")
}

/// Two commits in a fresh repository: a.txt and b.txt change.
fn pair(dir: &Path) -> (gix::ObjectId, gix::ObjectId) {
    init(dir);
    let before = commit(dir, &[("a.txt", "one\n"), ("b.txt", "x\n")]);
    let after = commit(dir, &[("a.txt", "one\ntwo\n"), ("b.txt", "y\n")]);
    (before, after)
}

fn render(dir: &Path, b: gix::ObjectId, a: gix::ObjectId, level: DiffLevel) -> String {
    let repo = gix::open(dir).expect("open");
    render_change_blocking(&repo, b, a, level).expect("renders")
}

fn short(id: gix::ObjectId) -> String {
    id.to_hex().to_string()[..12].to_owned()
}

#[test]
fn summary_is_one_line_with_a_command_that_reproduces_the_change() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (before, after) = pair(temp.path());
    let text = render(temp.path(), before, after, DiffLevel::Summary);
    assert_eq!(text.matches('\n').count(), 1, "{text}");
    let (stat, command) = text.trim_end().split_once(": ").expect("colon");
    assert_eq!(stat, "2 files changed, 2 insertions(+), 1 deletion(-)");
    let words = shell_words::split(command).expect("splits");
    assert_eq!(words[0], "git");
    assert_eq!(words[1], "-C");
    assert_eq!(words[4], short(before));
    assert_eq!(words[5], short(after));
    let other = tempfile::tempdir().expect("other dir");
    let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
    let rerun = git(other.path(), &args);
    let expected = git(
        temp.path(),
        &[
            "diff",
            &before.to_hex().to_string(),
            &after.to_hex().to_string(),
        ],
    );
    assert_eq!(rerun, expected);
}

#[test]
fn summary_quotes_a_repository_path_with_a_space() {
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = temp.path().join("my repo");
    let (before, after) = pair(&dir);
    let text = render(&dir, before, after, DiffLevel::Summary);
    let command = text.trim_end().split_once(": ").expect("colon").1;
    let words = shell_words::split(command).expect("splits");
    assert_eq!(words.len(), 6, "{text}");
    assert!(
        words[3] == "diff" && words[2].ends_with("my repo"),
        "{words:?}"
    );
}

#[test]
fn files_adds_one_stat_line_per_file_and_no_second_summary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (before, after) = pair(temp.path());
    let text = render(temp.path(), before, after, DiffLevel::Files);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines[0].starts_with("2 files changed"));
    assert!(lines[1].starts_with(" a.txt"), "{text}");
    assert!(lines[2].starts_with(" b.txt"), "{text}");
    assert!(text.ends_with('\n'));
    assert_eq!(text.matches("files changed").count(), 1);
}

#[test]
fn files_stops_at_fifty_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    init(temp.path());
    let before = commit(temp.path(), &[("seed.txt", "s\n")]);
    let names: Vec<String> = (0..60).map(|i| format!("f{i:02}.txt")).collect();
    let files: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "x\n")).collect();
    let after = commit(temp.path(), &files);
    let text = render(temp.path(), before, after, DiffLevel::Files);
    let stat_lines = text.lines().count() - 1;
    assert!(stat_lines <= 51, "{stat_lines} lines");
    assert!(text.contains("..."), "{text}");
    assert!(text.starts_with("60 files changed"));
}

#[test]
fn an_unchanged_pair_renders_nothing_at_every_level() {
    let temp = tempfile::tempdir().expect("tempdir");
    init(temp.path());
    let before = commit(temp.path(), &[("a.txt", "one\n")]);
    let after = commit(temp.path(), &[]);
    for level in [DiffLevel::Summary, DiffLevel::Files, DiffLevel::Full] {
        assert_eq!(render(temp.path(), before, after, level), "", "{level}");
    }
}

#[test]
fn two_snapshots_of_one_tree_with_different_labels_report_no_change() {
    let temp = tempfile::tempdir().expect("tempdir");
    init(temp.path());
    let before = commit(temp.path(), &[("a.txt", "one\n")]);
    git(
        temp.path(),
        &["commit", "-m", "another label", "--allow-empty"],
    );
    let after = gix::ObjectId::from_hex(git(temp.path(), &["rev-parse", "HEAD"]).trim().as_bytes())
        .expect("oid");
    assert_ne!(before, after);
    for level in [DiffLevel::Summary, DiffLevel::Files, DiffLevel::Full] {
        assert_eq!(render(temp.path(), before, after, level), "", "{level}");
    }
}

#[test]
fn none_renders_nothing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (before, after) = pair(temp.path());
    assert_eq!(render(temp.path(), before, after, DiffLevel::None), "");
}

#[test]
fn full_is_what_diff_commits_returns() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (before, after) = pair(temp.path());
    let repo = gix::open(temp.path()).expect("open");
    let expected = crate::diff::diff_commits_blocking(&repo, before, after).expect("diff");
    assert!(expected.starts_with("commit "));
    assert_eq!(
        render(temp.path(), before, after, DiffLevel::Full),
        expected
    );
}

#[test]
fn a_short_id_resolves() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (_, after) = pair(temp.path());
    let hex = after.to_hex().to_string();
    assert_eq!(
        resolve_commit_id(temp.path(), &hex[..12]).expect("short"),
        after
    );
    assert_eq!(resolve_commit_id(temp.path(), &hex).expect("full"), after);
}

#[test]
fn an_unknown_id_is_an_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    pair(temp.path());
    let err = resolve_commit_id(temp.path(), "deadbeef0").expect_err("unknown");
    assert!(
        err.to_string().starts_with("no snapshot deadbeef0 in "),
        "{err}"
    );
}

#[test]
fn a_too_short_or_non_hex_id_is_refused() {
    let temp = tempfile::tempdir().expect("tempdir");
    pair(temp.path());
    for id in ["abc123", "", "zzzzzzzz", "abc123g"] {
        let err = resolve_commit_id(temp.path(), id).expect_err("refused");
        assert_eq!(
            err.to_string(),
            format!("commit id must be 7 to 40 hex digits: {id}")
        );
    }
}

fn commit_body(tree: &str, n: u32) -> Vec<u8> {
    format!("tree {tree}\nauthor A <a@e> 0 +0000\ncommitter A <a@e> 0 +0000\n\nmsg {n}\n")
        .into_bytes()
}

fn write_commit(dir: &Path, body: &[u8]) {
    use std::io::Write;
    let mut child = Command::new("git")
        .args(["hash-object", "-t", "commit", "-w", "--stdin"])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(body)
        .expect("write");
    assert!(child.wait().expect("wait").success());
}

/// Two commit bodies whose ids share their first 7 hex digits.
fn colliding_commits(dir: &Path) -> (Vec<u8>, Vec<u8>, String) {
    let tree = git(dir, &["rev-parse", "HEAD^{tree}"]);
    let mut seen = std::collections::HashMap::new();
    (0..)
        .find_map(|n| {
            let body = commit_body(tree.trim(), n);
            let hash =
                gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Commit, &body)
                    .expect("hash");
            let prefix = hash.to_hex().to_string()[..7].to_owned();
            seen.insert(prefix.clone(), body.clone())
                .map(|earlier| (earlier, body, prefix))
        })
        .expect("a collision exists")
}

#[test]
fn an_ambiguous_prefix_is_an_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    pair(temp.path());
    let (first, second, prefix) = colliding_commits(temp.path());
    write_commit(temp.path(), &first);
    write_commit(temp.path(), &second);
    let err = resolve_commit_id(temp.path(), &prefix).expect_err("ambiguous");
    assert_eq!(
        err.to_string(),
        format!(
            "{prefix} names more than one object in {}",
            temp.path().display()
        )
    );
}

#[test]
fn a_tree_or_blob_id_is_not_a_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    pair(temp.path());
    for spec in ["HEAD^{tree}", "HEAD:a.txt"] {
        let id = git(temp.path(), &["rev-parse", spec]);
        let id = id.trim();
        let err = resolve_commit_id(temp.path(), id).expect_err("not a commit");
        assert!(
            err.to_string()
                .starts_with(&format!("no snapshot {id} in ")),
            "{err}"
        );
    }
}

#[test]
fn a_branch_named_like_an_id_is_not_a_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (_, after) = pair(temp.path());
    let name = if after.to_hex().to_string().starts_with("deadbeef") {
        "cafebabe"
    } else {
        "deadbeef"
    };
    git(temp.path(), &["branch", name]);
    let err = resolve_commit_id(temp.path(), name).expect_err("a ref name");
    assert!(
        err.to_string()
            .starts_with(&format!("no snapshot {name} in ")),
        "{err}"
    );
}

#[test]
fn a_branch_named_like_a_prefix_does_not_hide_the_commit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (before, after) = pair(temp.path());
    let prefix = after.to_hex().to_string()[..8].to_owned();
    git(
        temp.path(),
        &["branch", &prefix, &before.to_hex().to_string()],
    );
    assert_eq!(
        resolve_commit_id(temp.path(), &prefix).expect("the commit"),
        after
    );
}

#[test]
fn a_branch_named_like_a_shared_prefix_is_still_ambiguous() {
    let temp = tempfile::tempdir().expect("tempdir");
    pair(temp.path());
    let (first, second, prefix) = colliding_commits(temp.path());
    write_commit(temp.path(), &first);
    write_commit(temp.path(), &second);
    let first_id = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Commit, &first)
        .expect("hash");
    git(
        temp.path(),
        &["branch", &prefix, &first_id.to_hex().to_string()],
    );
    let err = resolve_commit_id(temp.path(), &prefix).expect_err("ambiguous");
    assert_eq!(
        err.to_string(),
        format!(
            "{prefix} names more than one object in {}",
            temp.path().display()
        )
    );
}
