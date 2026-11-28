//! Runs the shipped publisher against disposable Git repositories. GitHub,
//! token lookup, and race injection are isolated behind fixture commands.
#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const URL: &str = "https://github.com/example/project";
const GIT_URL: &str = "https://github.com/example/project.git";
const REF: &str = "refs/plans/feature/docs";
const EXACT: &[u8] =
    b"# Snapshot\r\n\r\nCaf\xc3\xa9  \r\nContact: reader@example.org\r\nNo final newline";
const FLAGGED: &[u8] = b"github_pat_83alX20w0s9j23a7BydcZe55\r\n-----BEGIN OPENSSH PRIVATE KEY-----\neyJdemo.eyJtest.qwerty\nhttps://test:fixture-pass@localhost\nreader@fixture-company.net\n";
// The publisher authenticates with the ambient GitHub token route: the
// anonymous probe carries no token, proxy or CA; the authenticated
// confirmation carries the fixture token as a stdin header line, never argv.
const CURL_STUB: &str = r#"#!/usr/bin/env bash
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
[[ $1 == -q && ${!#} == https://api.github.com/repos/example/* ]]
repo=${!#}; repo=${repo##*/}
mode=$(cat "$root/visibility")
printf '%s\n' "curl $repo" >> "$root/calls"
case $mode in
  public|private|internal|wrong)
    if [[ " $* " == *' --proxy  --noproxy * '* ]]; then
      # The anonymous probe: no credentials of any kind reach it.
      [[ " $* " != *' Authorization:'* ]]
      [[ ! ${GH_TOKEN+x} && ! ${GITHUB_TOKEN+x} && ! ${HOME+x} && ! ${CURL_HOME+x} ]]
      [[ ! ${HTTP_PROXY+x} && ! ${HTTPS_PROXY+x} && ! ${ALL_PROXY+x} && ! ${NO_PROXY+x} ]]
      [[ ! ${http_proxy+x} && ! ${https_proxy+x} && ! ${all_proxy+x} && ! ${no_proxy+x} ]]
      [[ ! ${SSL_CERT_FILE+x} && ! ${CURL_CA_BUNDLE+x} && ! ${GIT_SSL_CAINFO+x} ]]
      if [[ $mode != public ]]; then
        printf '{"message":"Not Found"}\n404'
        exit
      fi
    else
      # The authenticated confirmation: the token rides stdin, never argv.
      data=$(cat)
      [[ $data == "Authorization: Bearer fixture-token"$'\r' ]]
      [[ " $* " != *'Authorization:'* && " $* " != *fixture-token* ]]
      [[ " $* " != *' --noproxy'* ]]
    fi
    visible=$mode; private=true; identity=example/$repo
    [[ $mode != public ]] || private=false
    [[ $mode != wrong ]] || { identity=example/not-the-origin; visible=private; }
    printf '{"full_name":"%s","html_url":"https://github.com/%s","private":%s,"visibility":"%s"}\n200' "$identity" "$identity" "$private" "$visible"
    ;;
  missing) printf '\n404' ;;
  redirect) printf '\n301' ;;
  network) exit 7 ;;
  *) exit 91 ;;
esac
"#;

// Stands in for `gh auth token --hostname github.com`; gh-mode selects the
// output framing so refusal branches stay covered.
const GH_STUB: &str = r#"#!/usr/bin/env bash
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
printf '%s\n' "gh $*" >> "$root/gh-calls"
case $(cat "$root/gh-mode") in
    ok) printf ' fixture-token \n' ;;
    empty) printf '\n' ;;
    fail) exit 1 ;;
esac
"#;

// Forward normal operations to Git; redirect origin transport and inject controlled races.
const GIT_SHIM: &str = r#"#!/usr/bin/env bash
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
export PATH=${PATH#*:}
ref=refs/plans/feature/docs
mode=$(cat "$root/mode")
replace=()
verb=
for arg in "$@"; do
    case $arg in
        fetch|push|ls-remote|update-ref) verb=$arg ;;
        https://github.com/example/*.git)
            repo=${arg%.git}; repo=${repo##*/}
            printf '%s %s\n' "$verb" "$repo" >> "$root/calls"
            [[ $GIT_ALLOW_PROTOCOL == https ]]
            [[ $GIT_CONFIG_VALUE_0 == "Authorization: Basic $(printf 'x-access-token:fixture-token' | base64 | tr -d '\n')" ]]
            [[ $GIT_CONFIG_VALUE_1 == false ]]
            [[ $PWD != "$root/work" && $HOME == "$PWD" ]]
            arg="$root/remote.git" ;;
        https://*) exit 91 ;; # Never fall through to network.
    esac
    replace+=("$arg")
done
external_git() (
    unset GIT_OBJECT_DIRECTORY
    git -c core.hooksPath=/dev/null "$@"
)
case "$verb:$mode" in
    update-ref:cas-race)
        external_git -C "$root/work" update-ref --no-deref "$ref" HEAD
        ;;
    push:swap-ref)
        external_git -C "$root/work" update-ref --no-deref "$ref" HEAD
        ;;
    push:remote-advance|push:remote-create)
        printf 'normal\n' > "$root/mode"
        external_git --git-dir="$root/remote.git" update-ref "$ref" "$(cat "$root/winner")"
        ;;
    push:deny-push) echo 'remote secret fixture-diagnostic' >&2; exit 1 ;;
    ls-remote:fail-lookup) exit 128 ;;
    ls-remote:swap-source)
        printf 'reader@fixture-company.net' > "$root/work/snapshot.md"
        ;;
esac
if [[ $verb == push ]]; then
    [[ ${!#} =~ ^[0-9a-f]{40}:refs/plans/ ]] || exit 91
    printf '%s\n' "${!#}" >> "$root/pushed"
fi
if [[ $verb =~ ^(push|fetch|ls-remote)$ ]]; then
    export GIT_ALLOW_PROTOCOL=file
fi
exec git "${replace[@]}"
"#;

struct Fixture {
    root: tempfile::TempDir,
    path: std::ffi::OsString,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("publish-plan-test-")
            .tempdir_in(fs::canonicalize("/tmp").expect("system temp root"))
            .expect("fixture root");
        for dir in ["bin", "home", "work", "remote.git"] {
            fs::create_dir(root.path().join(dir)).expect("fixture directory");
        }
        install(&root.path().join("bin/git"), GIT_SHIM);
        install(&root.path().join("bin/curl"), CURL_STUB);
        install(&root.path().join("bin/gh"), GH_STUB);
        let inherited_path = std::env::var_os("PATH").expect("PATH");
        let path = std::env::join_paths(
            std::iter::once(root.path().join("bin")).chain(std::env::split_paths(&inherited_path)),
        )
        .expect("fixture PATH");
        let fixture = Self { root, path };
        fixture.write("visibility", "private");
        fixture.write("mode", "normal");
        fixture.write("gh-mode", "ok");
        fixture.git(&["init", "-q", "--initial-branch=main"]);
        fixture.remote(&["init", "--bare", "-q"]);
        fixture.git(&["remote", "add", "origin", GIT_URL]);
        fixture.write("work/tracked", "committed\n");
        fixture.git(&["add", "tracked"]);
        fixture.git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=f@example.invalid",
            "commit",
            "-qm",
            "initial",
        ]);
        fixture.write("work/snapshot.md", EXACT);
        fixture
    }

    fn path(&self, path: &str) -> PathBuf {
        self.root.path().join(path)
    }

    fn write(&self, path: &str, data: impl AsRef<[u8]>) {
        fs::write(self.path(path), data).expect("write fixture file");
    }

    fn text(&self, path: &str) -> String {
        fs::read_to_string(self.path(path)).expect("read fixture text")
    }

    fn command(&self, binary: &str) -> Command {
        let mut command = Command::new(binary);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("PATH"))
            .env("HOME", self.path("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        command
    }

    fn git_at(&self, dir: &str, args: &[&str]) -> Vec<u8> {
        checked(
            self.command("git")
                .current_dir(self.path(dir))
                .args(args)
                .output()
                .expect("git"),
        )
    }

    fn git(&self, args: &[&str]) -> Vec<u8> {
        self.git_at("work", args)
    }

    fn remote(&self, args: &[&str]) -> Vec<u8> {
        self.git_at("remote.git", args)
    }

    fn publish(&self, extra: &[&str]) -> Output {
        self.invocation()
            .args(["--branch", "feature/docs", "--source", "snapshot.md"])
            .args(extra)
            .output()
            .expect("run publisher")
    }

    fn invocation(&self) -> Command {
        // Exercise the shipped implementation; PATH substitutes only external boundaries.
        let helper = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/pantheon/scripts/publish-plan.sh");
        let mut command = self.command("bash");
        command
            .current_dir(self.path("work"))
            .env("PATH", &self.path)
            .env("GH_TOKEN", "fixture-token")
            .arg(helper);
        command
    }

    fn local_objects(&self) -> String {
        self.git_text(&["count-objects", "-v"])
    }

    fn git_text(&self, args: &[&str]) -> String {
        String::from_utf8(self.git(args))
            .expect("Git output")
            .trim_end()
            .to_owned()
    }

    fn assert_empty_publication(&self, before: &str) {
        assert_eq!(self.local_objects(), before, "unexpected object creation");
        assert!(self.git(&["for-each-ref", "refs/plans"]).is_empty());
        assert!(self.remote(&["for-each-ref"]).is_empty());
    }

    fn assert_remote(&self, result: &Value, bytes: &[u8]) {
        let sha = field(result, "sha");
        let file = field(result, "file");
        assert_eq!(result["ref"], REF);
        assert_eq!(
            self.remote(&["rev-parse", REF]),
            format!("{sha}\n").as_bytes()
        );
        assert_eq!(
            self.remote(&["ls-tree", "--name-only", sha]),
            format!("{file}\n").as_bytes()
        );
        assert_eq!(self.remote(&["show", &format!("{sha}:{file}")]), bytes);
        assert_eq!(result["plan_url"], format!("{URL}/blob/{sha}/{file}"));
    }

    fn seed_remote_winner(&self, parent: Option<&str>, file: &str) -> String {
        let winner = self.path("remote.git");
        if let Some(parent) = parent {
            // Copy already-published history into a separate ref for preparing the rival.
            assert_eq!(self.remote(&["cat-file", "-t", parent]), b"commit\n");
        }
        self.write("remote-input", "rival writer\n");
        let blob = self.remote(&[
            "hash-object",
            "-w",
            self.path("remote-input").to_str().expect("path"),
        ]);
        let blob = std::str::from_utf8(&blob).expect("blob ID").trim_end();
        self.write("tree-input", format!("100644 blob {blob}\t{file}\n"));
        let mut mktree = self.command("git");
        let tree = checked(
            mktree
                .current_dir(&winner)
                .arg("mktree")
                .stdin(fs::File::open(self.path("tree-input")).expect("tree input"))
                .output()
                .expect("remote mktree"),
        );
        let tree = std::str::from_utf8(&tree).expect("tree ID").trim_end();
        let mut args = vec![
            "-c",
            "user.name=Rival",
            "-c",
            "user.email=rival@example.invalid",
            "commit-tree",
            tree,
            "-m",
            "Publish plan snapshot",
        ];
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        let commit = String::from_utf8(self.remote(&args))
            .expect("commit ID")
            .trim_end()
            .to_owned();
        self.write("winner", &commit);
        commit
    }
}

fn install(path: &Path, content: &str) {
    fs::write(path, content).expect("write fixture binary");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make fixture executable");
}

fn checked(output: Output) -> Vec<u8> {
    assert!(output.status.success(), "{output:?}");
    output.stdout
}

fn metadata(output: Output) -> Value {
    serde_json::from_slice(&checked(output)).expect("JSON success record")
}

fn field<'a>(metadata: &'a Value, key: &str) -> &'a str {
    metadata[key].as_str().expect("string field")
}

fn refuses(output: &Output, expected: &str) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(expected), "{stderr}");
    assert!(!stderr.contains("fixture-diagnostic"), "{stderr}");
}

#[test]
fn exact_orphan_reuse_and_changed_child_preserve_worktree() {
    let fixture = Fixture::new();
    fixture.write("work/tracked", "staged\n");
    fixture.git(&["add", "tracked"]);
    fixture.write("work/tracked", "working tree\n");
    fixture.write("work/.gitattributes", "*.md text eol=lf filter=fixture\n");
    fixture.write("work/.git/FETCH_HEAD", "unrelated fetch state\n");
    fixture.git(&["config", "filter.fixture.clean", "false"]);
    let head = fixture.text("work/.git/HEAD");
    let index = fs::read(fixture.path("work/.git/index")).expect("index bytes");
    let code_sha = fixture.git_text(&["rev-parse", "HEAD"]);
    let initial = metadata(fixture.publish(&[]));
    fixture.assert_remote(&initial, EXACT);
    let first_sha = field(&initial, "sha");
    assert_eq!(
        fixture.remote(&["rev-list", "--parents", "-n1", REF]),
        format!("{first_sha}\n").as_bytes()
    );
    assert!(field(&initial, "file").ends_with("-feature-docs.md"));
    assert_eq!(initial["reused"], false);
    let same = metadata(fixture.publish(&[]));
    assert_eq!(same["sha"], initial["sha"]);
    assert_eq!(same["reused"], true);
    assert_eq!(
        fixture.text("pushed").lines().count(),
        2,
        "reuse must still push"
    );
    fixture.write("work/snapshot.md", b"# Changed\r\n");
    let next = metadata(fixture.publish(&[]));
    fixture.assert_remote(&next, b"# Changed\r\n");
    assert_eq!(next["file"], initial["file"]);
    let next_sha = field(&next, "sha");
    assert_eq!(
        fixture.remote(&["rev-list", "--parents", "-n1", REF]),
        format!("{next_sha} {first_sha}\n").as_bytes()
    );
    assert_eq!(fixture.git_text(&["rev-parse", REF]), next_sha);
    assert_eq!(fixture.git_text(&["rev-parse", "HEAD"]), code_sha);
    assert_eq!(fixture.text("work/.git/HEAD"), head);
    assert_eq!(
        fixture.text("work/.git/FETCH_HEAD"),
        "unrelated fetch state\n"
    );
    assert_eq!(
        fs::read(fixture.path("work/.git/index")).expect("index bytes"),
        index
    );
    assert_eq!(fixture.text("work/tracked"), "working tree\n");
    assert_eq!(fixture.text("work/snapshot.md"), "# Changed\r\n");
    assert_eq!(
        fixture.text("work/.gitattributes"),
        "*.md text eol=lf filter=fixture\n"
    );
}

#[test]
fn ignores_hooks_signing_mirror_and_push_configuration() {
    let fixture = Fixture::new();
    for hook in ["reference-transaction", "pre-push", "signer"] {
        install(
            &fixture.path(&format!("work/.git/hooks/{hook}")),
            "#!/bin/sh\necho CALLED >&2\nexit 42\n",
        );
    }
    for (key, val) in [
        ("commit.gpgSign", "true"),
        ("push.gpgSign", "true"),
        ("push.followTags", "true"),
        ("remote.origin.mirror", "true"),
        ("remote.origin.push", "refs/heads/main:refs/heads/wrong"),
        ("push.pushOption", "must-not-leak"),
        ("gpg.program", ".git/hooks/signer"),
    ] {
        fixture.git(&["config", key, val]);
    }
    let result = metadata(fixture.publish(&[]));
    fixture.assert_remote(&result, EXACT);
    assert_eq!(
        fixture.remote(&["for-each-ref", "--format=%(refname)"]),
        format!("{REF}\n").as_bytes()
    );
    let commit = fixture.remote(&["cat-file", "commit", REF]);
    let commit = std::str::from_utf8(&commit).expect("commit text");
    assert!(!commit.contains("gpgsig"));
    assert!(commit.contains("author Pantheon <pantheon@plan-snapshot.invalid>"));
}

#[test]
fn pinned_push_survives_local_ref_change_after_cas() {
    let fixture = Fixture::new();
    fixture.write("mode", "swap-ref");
    let result = metadata(fixture.publish(&[]));
    fixture.assert_remote(&result, EXACT);
    assert_eq!(
        fixture.git_text(&["rev-parse", REF]),
        fixture.git_text(&["rev-parse", "HEAD"])
    );
    assert_eq!(
        fixture.text("pushed").trim_end(),
        format!("{}:{REF}", field(&result, "sha"))
    );
}

#[test]
fn failed_local_cas_preserves_winner_and_never_pushes() {
    let fixture = Fixture::new();
    fixture.write("mode", "cas-race");
    refuses(&fixture.publish(&[]), "concurrent ref left intact");
    assert_eq!(
        fixture.git_text(&["rev-parse", REF]),
        fixture.git_text(&["rev-parse", "HEAD"])
    );
    assert!(fixture.remote(&["for-each-ref"]).is_empty());
    assert!(!fixture.path("pushed").exists());
}

#[test]
fn remote_advancement_rebuilds_snapshot_on_the_published_winner() {
    let fixture = Fixture::new();
    let initial = metadata(fixture.publish(&[]));
    let winner = fixture.seed_remote_winner(Some(field(&initial, "sha")), field(&initial, "file"));
    fixture.write("mode", "remote-advance");
    fixture.write("work/snapshot.md", b"new checked bytes\r\n");
    fixture.write("work/.git/FETCH_HEAD", "keep\n");
    let result = metadata(fixture.publish(&[]));
    fixture.assert_remote(&result, b"new checked bytes\r\n");
    assert_eq!(result["file"], initial["file"]);
    assert_eq!(
        fixture.remote(&["rev-parse", &format!("{REF}^")]),
        format!("{winner}\n").as_bytes()
    );
    assert_eq!(fixture.text("pushed").lines().count(), 3);
    assert_eq!(fixture.text("work/.git/FETCH_HEAD"), "keep\n");
}

#[test]
fn remote_creation_race_uses_winners_filename() {
    let fixture = Fixture::new();
    let winner = fixture.seed_remote_winner(None, "2020-01-01-remote-choice.md");
    fixture.write("mode", "remote-create");
    let result = metadata(fixture.publish(&[]));
    fixture.assert_remote(&result, EXACT);
    assert_eq!(result["file"], "2020-01-01-remote-choice.md");
    assert_eq!(
        fixture.remote(&["rev-parse", &format!("{REF}^")]),
        format!("{winner}\n").as_bytes()
    );
    assert_eq!(fixture.text("pushed").lines().count(), 2);
}

#[test]
fn push_failure_is_not_success_and_different_local_only_history_is_retained() {
    let fixture = Fixture::new();
    fixture.write("mode", "deny-push");
    refuses(&fixture.publish(&[]), "plan push failed");
    let local = fixture.git_text(&["rev-parse", REF]);
    assert!(fixture.remote(&["for-each-ref"]).is_empty());
    fixture.write("mode", "normal");
    fixture.write("work/snapshot.md", "other content");
    let before = fixture.local_objects();
    refuses(&fixture.publish(&[]), "local-only plan history conflicts");
    assert_eq!(fixture.local_objects(), before);
    assert_eq!(fixture.git_text(&["rev-parse", REF]), local);
    fixture.write("work/snapshot.md", EXACT);
    fixture.assert_remote(&metadata(fixture.publish(&[])), EXACT);
}

#[test]
fn unknown_visibility_and_failed_ref_lookup_create_no_objects() {
    let fixture = Fixture::new();
    let before = fixture.local_objects();
    for mode in ["missing", "internal", "wrong", "redirect", "network"] {
        fixture.write("visibility", mode);
        let output = fixture.publish(&[]);
        let reason = if matches!(mode, "internal" | "wrong") {
            "inconsistent"
        } else {
            "visibility is unknown"
        };
        refuses(&output, reason);
        fixture.assert_empty_publication(&before);
    }
    assert!(!fixture.text("calls").contains("ls-remote"));
    fixture.write("visibility", "private");
    fixture.write("mode", "fail-lookup");
    refuses(&fixture.publish(&[]), "could not look up remote plan ref");
    fixture.assert_empty_publication(&before);
}

#[test]
fn refuses_unsafe_paths_and_uses_only_checked_bytes_after_source_changes() {
    let fixture = Fixture::new();
    let before = fixture.local_objects();
    symlink("snapshot.md", fixture.path("work/link.md")).expect("file symlink");
    symlink(fixture.path("work"), fixture.path("work/link-dir")).expect("dir symlink");
    fs::hard_link(
        fixture.path("work/snapshot.md"),
        fixture.path("work/hard.md"),
    )
    .expect("hard link");
    for (source, reason) in [
        ("link.md", "could not open source safely"),
        ("link-dir/snapshot.md", "could not open source safely"),
        ("hard.md", "regular, singly linked file"),
        ("/dev/null", "source must be inside"),
        ("../work/snapshot.md", "without parent traversal"),
        (".git", "regular, singly linked file"),
    ] {
        refuses(&fixture.publish(&["--source", source]), reason);
        fixture.assert_empty_publication(&before);
    }
    assert!(!fixture.path("calls").exists());
    fs::remove_file(fixture.path("work/hard.md")).expect("unlink hard link");
    fixture.write("mode", "swap-source");
    let result = metadata(fixture.publish(&[]));
    assert_eq!(
        fixture.text("work/snapshot.md"),
        "reader@fixture-company.net"
    );
    fixture.assert_remote(&result, EXACT);
}

#[test]
fn validates_effective_fetch_and_push_destinations_and_ignores_rewrites_in_transport() {
    let fixture = Fixture::new();
    let before = fixture.local_objects();
    fixture.git(&[
        "remote",
        "set-url",
        "--push",
        "origin",
        "https://github.com/example/other.git",
    ]);
    refuses(&fixture.publish(&[]), "destinations differ");
    fixture.git(&[
        "remote",
        "set-url",
        "--push",
        "origin",
        "https://fixture-password@github.com/example/project.git",
    ]);
    let output = fixture.publish(&[]);
    refuses(&output, "credential-free");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-password"));
    fixture.git(&["config", "--unset-all", "remote.origin.pushurl"]);
    fixture.git(&[
        "config",
        "url.https://github.com/example/other.git.pushInsteadOf",
        GIT_URL,
    ]);
    refuses(&fixture.publish(&[]), "destinations differ");
    fixture.git(&[
        "config",
        "--unset-all",
        "url.https://github.com/example/other.git.pushInsteadOf",
    ]);
    fixture.git(&[
        "remote",
        "set-url",
        "--add",
        "origin",
        "https://github.com/example/other.git",
    ]);
    refuses(&fixture.publish(&[]), "one credential-free");
    fixture.assert_empty_publication(&before);
    assert!(!fixture.path("calls").exists());
    fixture.git(&["config", "--unset-all", "remote.origin.url"]);
    fixture.git(&["config", "remote.origin.url", "fixture:project.git"]);
    fixture.git(&[
        "config",
        "url.https://GitHub.com/Example/.insteadOf",
        "fixture:",
    ]);
    fixture.git(&[
        "config",
        "url.https://github.com/example/no-transport.insteadOf",
        GIT_URL,
    ]);
    fixture.assert_remote(&metadata(fixture.publish(&[])), EXACT);
    assert!(fixture.text("calls").contains("push project\n"));
}

#[test]
fn flagged_snapshots_are_refused_until_sanitized_for_either_visibility() {
    for visibility in ["public", "private"] {
        let fixture = Fixture::new();
        fixture.write("visibility", visibility);
        let before = fixture.local_objects();
        fixture.write("work/snapshot.md", FLAGGED);
        let output = fixture.publish(&[]);
        refuses(&output, "sanitize and retry");
        // Exact diagnostic: categories and offsets, not even partial matched values.
        assert_eq!(
            output.stderr,
            b"publish-plan: snapshot contains potentially sensitive content; sanitize and retry: \
              token at line 1, byte column 1; private_key at line 2, byte column 1; \
              jwt at line 3, byte column 1; url_credentials at line 4, byte column 1; \
              email at line 5, byte column 1\n"
        );
        fixture.assert_empty_publication(&before);
        assert!(!fixture.path("calls").exists());
        fixture.write("work/snapshot.md", EXACT);
        fixture.assert_remote(&metadata(fixture.publish(&[])), EXACT);
    }
}

#[test]
fn retired_approval_flags_are_rejected() {
    let fixture = Fixture::new();
    fixture.write("visibility", "public");
    fixture.write("work/snapshot.md", FLAGGED);
    let before = fixture.local_objects();
    // A scope the former publisher accepted for these exact bytes must not help.
    let scope =
        format!("{URL}|{REF}|9f7e6317cd86b095f5348aefd615ceb9f688f7c9f59ea40fda0fe35ebd200161");
    for flag in ["--approve-public", "--approve-content"] {
        let output = fixture.publish(&[flag, &scope]);
        refuses(&output, "invalid arguments");
        assert_eq!(
            output.stderr,
            b"publish-plan: invalid arguments; see publish-plan.sh --help\n"
        );
        fixture.assert_empty_publication(&before);
    }
    assert!(!fixture.path("calls").exists());
}

#[test]
fn refuses_invalid_markdown_bytes_before_publication() {
    let fixture = Fixture::new();
    let before = fixture.local_objects();
    for bytes in [b"# Broken \xff".as_slice(), b"# Null\0 byte"] {
        fixture.write("work/snapshot.md", bytes);
        refuses(
            &fixture.publish(&[]),
            "source must be UTF-8 Markdown without NUL bytes",
        );
        fixture.assert_empty_publication(&before);
    }
    assert!(!fixture.path("calls").exists());
}

#[test]
fn shipped_wrapper_selects_physical_worktree_for_symlink_parent_path() {
    let fixture = Fixture::new();
    let physical_worktree = fixture.path("physical/work");
    fs::create_dir_all(fixture.path("physical/deep")).expect("physical symlink target");
    fs::create_dir_all(&physical_worktree).expect("physical worktree");
    symlink(fixture.path("physical/deep"), fixture.path("checkout-link"))
        .expect("worktree path symlink");

    let worktree = fixture.path("checkout-link/../work");
    assert_ne!(
        worktree.canonicalize().expect("physical worktree path"),
        fixture
            .path("work")
            .canonicalize()
            .expect("logical worktree path")
    );

    let package = fixture.path("package");
    fs::create_dir_all(package.join("scripts")).expect("package scripts directory");
    fs::write(
        package.join("scripts/publish-plan.sh"),
        r#"#!/usr/bin/env bash
printf 'PWD=%s' "$(pwd -P)"
"#,
    )
    .expect("publisher path probe");

    let template_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/pantheon/bash_tools/publish_plan.yaml");
    let template = fs::read_to_string(template_path).expect("shipped publisher template");
    let template: serde_yaml::Value = serde_yaml::from_str(&template).expect("valid template YAML");
    let script = template["script"]
        .as_str()
        .expect("template wrapper script");
    let output = fixture
        .command("bash")
        .current_dir(fixture.root.path())
        .arg("-c")
        .arg(script)
        .env("WORKTREE", &worktree)
        .env("SOURCE", "snapshot.md")
        .env("BRANCH", "feature/docs")
        .env("HARNX_PACKAGE_DIR", &package)
        .output()
        .expect("run shipped publisher wrapper");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("path probe output")
            .trim(),
        format!(
            "PWD={}",
            physical_worktree
                .canonicalize()
                .expect("canonical worktree")
                .display()
        )
    );
}

#[test]
fn gh_fallback_pins_github_com_and_trims_token() {
    let fixture = Fixture::new();
    let mut command = fixture.invocation();
    command.env_remove("GH_TOKEN");
    command.args(["--branch", "feature/docs", "--source", "snapshot.md"]);
    let result = metadata(command.output().expect("run publisher"));
    fixture.assert_remote(&result, EXACT);
    // The fallback lookup is pinned to github.com; the printed token's
    // framing (leading space, trailing newline) is trimmed before use.
    assert_eq!(
        fixture.text("gh-calls").trim_end(),
        "gh auth token --hostname github.com"
    );
    assert_eq!(
        fixture
            .text("calls")
            .lines()
            .filter(|line| line.starts_with("curl"))
            .count(),
        2,
        "anonymous probe then authenticated confirmation"
    );
}

#[test]
fn missing_gh_credential_refuses_without_network_or_writes() {
    for mode in ["fail", "empty"] {
        let fixture = Fixture::new();
        fixture.write("gh-mode", mode);
        let before = fixture.local_objects();
        let mut command = fixture.invocation();
        command.env_remove("GH_TOKEN");
        command.args(["--branch", "feature/docs", "--source", "snapshot.md"]);
        let output = command.output().expect("run publisher");
        refuses(&output, "a GitHub token is required");
        assert!(
            !fixture.path("calls").exists(),
            "no GitHub request may leave the host on refusal"
        );
        fixture.assert_empty_publication(&before);
    }
}

#[test]
fn control_byte_credential_refuses_without_network_or_writes() {
    let fixture = Fixture::new();
    let before = fixture.local_objects();
    let mut command = fixture.invocation();
    command.env("GH_TOKEN", "fixture-ev\ntoken");
    command.args(["--branch", "feature/docs", "--source", "snapshot.md"]);
    let output = command.output().expect("run publisher");
    refuses(&output, "contains control bytes");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("fixture-ev"), "{stderr}");
    assert!(!stderr.contains("Bearer"), "{stderr}");
    assert!(
        !fixture.path("calls").exists(),
        "no GitHub request may leave the host on refusal"
    );
    fixture.assert_empty_publication(&before);
}
