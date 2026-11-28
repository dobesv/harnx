//! Declared ambient passthrough, the fail-closed `env` rejection, and the
//! `snapshots:` policy on template invocations — the core support
//! `bash_publish_plan` runs on.
use harnx_toolset::Toolset;
use serde_json::{json, Value};
use std::path::Path;
use tokio_util::sync::CancellationToken;

use crate::template_support::toolset_for;

fn write_template(path: &Path, yaml: &str) {
    std::fs::write(path, yaml).expect("write template");
}

/// A template call may not smuggle environment variables: without an
/// author-declared way in, `env` stays an unknown argument. The publisher
/// relies on this fail-closed default — its credentials arrive as declared
/// ambient passthrough, never as call arguments.
#[tokio::test]
async fn template_calls_reject_env_arguments() {
    let root = tempfile::tempdir().expect("grant root");
    let template_path = root.path().join("plain.yaml");
    write_template(&template_path, "name: plain\nscript: |\n  printf 'RAN'");
    let toolset = toolset_for(root.path(), &[template_path]).await;

    let error = toolset
        .invoke(
            "plain",
            json!({ "env": { "GH_TOKEN": "aaa" } }),
            CancellationToken::new(),
        )
        .await
        .expect_err("env must stay an unknown argument without a contract");
    assert!(
        error.to_string().contains("unknown argument `env`"),
        "unexpected error: {error}"
    );
    let _ = toolset.cleanup_log_dir().await;
}

/// Sets an ambient variable a template invocation test competes with or
/// depends on.
// SAFETY: nextest isolates each test in its own process; no concurrent test
// reads these variables, and the template server runs in-process.
fn set_ambient(key: &str, value: &str) {
    unsafe { std::env::set_var(key, value) };
}

/// Declared ambient passthrough (`sandbox.env`) reaches template scripts in
/// non-sandboxed runs too, so a template's env needs do not depend on the
/// sandbox mode. This is how `bash_publish_plan` finds its packaged script
/// and the GitHub credential route (`GH_TOKEN`/`GITHUB_TOKEN`).
#[tokio::test]
async fn declared_pass_env_reaches_unsandboxed_templates() {
    let root = tempfile::tempdir().expect("grant root");
    let template_path = root.path().join("ambient.yaml");
    write_template(
        &template_path,
        "name: ambient\nsandbox:\n  enabled: false\n  env: [\"HARNX_TEST_AMBIENT\"]\nscript: |\n  printf 'AMBIENT=%s' \"${HARNX_TEST_AMBIENT:-unset}\"\n",
    );
    set_ambient("HARNX_TEST_AMBIENT", "from-server-env");
    let toolset = toolset_for(root.path(), &[template_path]).await;

    let result: Value = toolset
        .invoke("ambient", json!({}), CancellationToken::new())
        .await
        .expect("invoke ambient template");
    assert!(
        result.to_string().contains("AMBIENT=from-server-env"),
        "declared ambient env must reach the unsandboxed script: {result}"
    );
    let _ = toolset.cleanup_log_dir().await;
}

/// Ambient passthrough is the lowest-precedence source on the unsandboxed
/// path: author-supplied `env:` and a parameter binding must each win over a
/// same-named ambient value. The probe prints every source, so an ambient
/// overwrite instead of `or_insert` fails this test.
const PRECEDENCE: &str = "\
name: precedence
description: Report competing env sources
parameters:
  choice: { type: string }
env:
  AUTHOR_VAR: author-fixed
sandbox:
  enabled: false
  env: [\"AUTHOR_VAR\", \"CHOICE\"]
template: false
script: |
  set -euo pipefail
  printf 'AUTHOR_VAR=%s|CHOICE=%s\\n' \"${AUTHOR_VAR:-unset}\" \"${CHOICE:-unset}\"
";

#[tokio::test]
async fn author_and_bound_env_win_over_ambient_passthrough() {
    let root = tempfile::tempdir().expect("grant root");
    let template_path = root.path().join("precedence.yaml");
    write_template(&template_path, PRECEDENCE);
    set_ambient("AUTHOR_VAR", "ambient-author-loses");
    set_ambient("CHOICE", "ambient-binding-loses");
    let toolset = toolset_for(root.path(), &[template_path]).await;

    let result = toolset
        .invoke(
            "precedence",
            json!({ "choice": "binding-wins" }),
            CancellationToken::new(),
        )
        .await
        .expect("invoke precedence template");
    let text = result.to_string();
    assert!(
        text.contains("AUTHOR_VAR=author-fixed") && !text.contains("ambient-author-loses"),
        "author-supplied env must win over same-named ambient: {text}"
    );
    assert!(
        text.contains("CHOICE=binding-wins") && !text.contains("ambient-binding-loses"),
        "parameter binding must win over same-named ambient: {text}"
    );
    let _ = toolset.cleanup_log_dir().await;
}

/// Runs `git` in `work` and returns its stdout.
fn git_in(work: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(work)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run fixture git");
    assert!(
        output.status.success(),
        "fixture git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Loose and packed object counts, the number that grows whenever execution
/// history captures the worktree.
fn object_census(work: &Path) -> (u64, u64) {
    let counts = git_in(work, &["count-objects", "-v"]);
    let mut census = (0, 0);
    for line in counts.lines() {
        if let Some(count) = line.strip_prefix("count: ") {
            census.0 = count.parse().expect("loose object count");
        } else if let Some(count) = line.strip_prefix("in-pack: ") {
            census.1 = count.parse().expect("packed object count");
        }
    }
    census
}

/// Whether any object in the worktree's local Git store carries `needle`.
fn objects_contain(work: &Path, needle: &str) -> bool {
    git_in(work, &["cat-file", "--batch-all-objects", "--batch"]).contains(needle)
}

/// Inits a git repository with one committed tracked file, so its bytes are
/// legitimately in the local object store.
fn init_repo(work: &Path) {
    git_in(work, &["init", "-q", "--object-format=sha1"]);
    git_in(
        work,
        &[
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "user.name=fixture",
            "add",
            "delivery.txt",
        ],
    );
    git_in(
        work,
        &[
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "user.name=fixture",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
    );
}

/// A `snapshots: none` invocation must not capture the worktree into the
/// repository's local Git object store: a scanner-refusing tool (the plan
/// publisher's shape) would otherwise leave flagged tracked and untracked
/// bytes behind as unreferenced objects. The worktree is a nested repo of the
/// invocation's default directory — the real publisher's delivery worktree
/// sits under the server's working directory the same way — so a default
/// snapshot decision captures it. The control template, identical except for
/// the default snapshot policy, proves the snapshot machinery is active in
/// this fixture, so the policy assertions fail before the fix.
#[tokio::test]
async fn snapshots_none_invocation_writes_no_local_git_objects() {
    const MARKER: &str = "untracked-sensitive-marker-8675309";
    let root = tempfile::tempdir().expect("grant root");
    let scanner = root.path().join("scanner.sh");
    std::fs::write(
        &scanner,
        "#!/usr/bin/env bash\nprintf 'snapshot contains potentially sensitive content\\n' >&2\nexit 1\n",
    )
    .expect("write scanner script");

    let mut workdirs = Vec::new();
    for (name, yaml_extra) in [("snap_none", "\nsnapshots: none"), ("snap_auto", "")] {
        let work = root.path().join(name);
        std::fs::create_dir(&work).expect("work directory");
        std::fs::write(work.join("delivery.txt"), "tracked fixture content\n")
            .expect("tracked file");
        init_repo(&work);
        std::fs::write(work.join("secret.md"), format!("{MARKER}\n")).expect("untracked file");
        let template_path = root.path().join(format!("{name}.yaml"));
        write_template(
            &template_path,
            &format!(
                "name: {name}\nparameters:\n  worktree: {{ type: string, required: true, pattern: \"^/\" }}\n  scanner: {{ type: string, required: true }}\n{yaml_extra}\ntemplate: false\nscript: |\n  set -euo pipefail\n  cd \"$WORKTREE\"\n  exec bash \"$SCANNER\"\n"
            ),
        );
        workdirs.push((name, work, template_path));
    }

    for (name, work, template_path) in &workdirs {
        let toolset = toolset_for(root.path(), std::slice::from_ref(template_path)).await;
        let before = object_census(work);
        let result = toolset
            .invoke(
                name,
                json!({
                    "worktree": work.to_string_lossy(),
                    "scanner": scanner.to_string_lossy(),
                }),
                CancellationToken::new(),
            )
            .await
            .expect("invoke scanner template");
        let text = result.to_string();
        assert!(
            text.contains("sensitive content") && !text.contains(MARKER),
            "scanner refusal must report categories, not the flagged value: {text}"
        );
        let after = object_census(work);

        if *name == "snap_auto" {
            assert!(
                after > before,
                "default-policy control must snapshot (and thus write objects): \
                 {before:?} -> {after:?}"
            );
            assert!(
                objects_contain(work, MARKER),
                "control arm must prove the fixture detects retained untracked bytes"
            );
        } else {
            assert_eq!(
                after, before,
                "snapshots:none invocation must not write local Git objects: {before:?} -> {after:?}"
            );
            assert!(
                !objects_contain(work, MARKER),
                "snapshots:none invocation must not retain flagged untracked bytes in local Git objects"
            );
        }
        let _ = toolset.cleanup_log_dir().await;
    }
}
