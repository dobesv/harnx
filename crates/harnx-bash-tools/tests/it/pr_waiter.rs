#![cfg(unix)]

use harnx_bash_tools::discover_tool_templates;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const PR_URL: &str = "https://github.com/example/project/pull/42";

struct WaiterFixture<'a> {
    fake_gh: &'a str,
    direct_pr: bool,
    stall_seconds: u64,
}

struct ExpectedWaiter<'a> {
    output: Output,
    stdout_fragments: &'a [&'a str],
}

struct ExpectedCheck<'a> {
    command: &'a str,
    stall_seconds: u64,
    stdout_fragments: &'a [&'a str],
}

fn waiter_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/pantheon/scripts/wait-for-pr-stable.sh")
}

fn templates_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/pantheon/bash_tools")
}

fn run_waiter(fixture: WaiterFixture<'_>) -> Output {
    let WaiterFixture {
        fake_gh,
        direct_pr,
        stall_seconds,
    } = fixture;
    let fixture = tempfile::tempdir().expect("fixture directory");
    let bin_dir = fixture.path().join("bin");
    let state_dir = fixture.path().join("state");
    fs::create_dir(&bin_dir).expect("fake bin directory");
    fs::create_dir(&state_dir).expect("fake state directory");

    let gh_path = bin_dir.join("gh");
    fs::write(&gh_path, fake_gh).expect("write fake gh");
    let mut permissions = fs::metadata(&gh_path)
        .expect("fake gh metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&gh_path, permissions).expect("make fake gh executable");

    let host_path = std::env::var_os("PATH").expect("PATH should exist");
    let joined_path = std::env::join_paths(
        std::iter::once(bin_dir.clone()).chain(std::env::split_paths(&host_path)),
    )
    .expect("build fixture PATH");

    let mut command = Command::new("bash");
    command
        .arg(waiter_path())
        .env("PATH", joined_path)
        .env("STATE_DIR", state_dir)
        .env("HARNX_WAIT_PR_POLL_SECONDS", "0")
        .env("HARNX_WAIT_PR_STALL_SECONDS", stall_seconds.to_string())
        .env("HARNX_WAIT_PR_SETTLE_SECONDS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if direct_pr {
        command.env("PR_URL", PR_URL);
    } else {
        command
            .env("REPO", "example/project")
            .env("BRANCH", "feature");
    }
    let mut child = command.spawn().expect("run waiter");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().expect("query waiter process").is_some() {
            return child.wait_with_output().expect("collect waiter output");
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill timed out waiter");
            let output = child.wait_with_output().expect("collect timed out waiter");
            let (stdout, stderr) = output_text(&output);
            panic!("waiter exceeded 5 seconds: stdout={stdout}\nstderr={stderr}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn output_text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn assert_successful_waiter(expected: ExpectedWaiter<'_>) -> (String, String) {
    let ExpectedWaiter {
        output,
        stdout_fragments,
    } = expected;
    let (stdout, stderr) = output_text(&output);
    assert!(output.status.success(), "stdout={stdout}\nstderr={stderr}");
    for expected in stdout_fragments {
        assert!(
            stdout.contains(expected),
            "stdout does not contain {expected:?}: {stdout}"
        );
    }
    (stdout, stderr)
}

fn assert_waiter_check(expected: ExpectedCheck<'_>) -> (String, String) {
    let ExpectedCheck {
        command,
        stall_seconds,
        stdout_fragments,
    } = expected;
    let fake_gh = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
case "$1 $2" in
  "pr view") printf 'OPEN\t%s\tsha-1\tupdated-1\tcomments-1\treviews-1\n' 'https://github.com/example/project/pull/42' ;;
  "pr checks") {command} ;;
  *) exit 91 ;;
esac
"#
    );
    assert_successful_waiter(ExpectedWaiter {
        output: run_waiter(WaiterFixture {
            fake_gh: &fake_gh,
            direct_pr: true,
            stall_seconds,
        }),
        stdout_fragments,
    })
}

#[test]
fn shipped_pantheon_templates_are_valid() {
    let templates = discover_tool_templates(None, &[], &[templates_path()])
        .expect("Pantheon command templates should load");
    assert_eq!(templates.len(), 8);
}

#[test]
fn shipped_waiter_uses_fifteen_minute_stall_threshold() {
    let script = fs::read_to_string(waiter_path()).expect("read shipped waiter");
    assert!(
        script.contains("HARNX_WAIT_PR_STALL_SECONDS:-900"),
        "waiter must default to the issue's 15-minute unchanged-status threshold"
    );
}

#[test]
fn waiter_discovers_pr_then_returns_after_terminal_checks_are_quiet() {
    let output = run_waiter(WaiterFixture {
        fake_gh: r#"#!/usr/bin/env bash
set -euo pipefail
case "$1 $2" in
  "pr list")
    count_file="$STATE_DIR/list-count"
    count=0
    [[ -f "$count_file" ]] && read -r count < "$count_file"
    count=$((count + 1))
    printf '%s\n' "$count" > "$count_file"
    if ((count >= 2)); then
      printf 'example\t%s\n' 'https://github.com/example/project/pull/42'
    fi
    ;;
  "pr view") printf 'OPEN\t%s\tsha-1\tupdated-1\tcomments-1\treviews-1\n' 'https://github.com/example/project/pull/42' ;;
  "pr checks") printf 'pass\tCI\tbuild\tSUCCESS\n' ;;
  *) exit 91 ;;
esac
"#,
        direct_pr: false,
        stall_seconds: 999,
    });
    assert_successful_waiter(ExpectedWaiter {
        output,
        stdout_fragments: &[
            "Waiting for an open pull request",
            "Monitoring https://github.com/example/project/pull/42",
            "reason=checks_terminal",
            "checks_pass=1",
        ],
    });
}

#[test]
fn waiter_resets_terminal_settlement_when_pr_activity_changes() {
    let output = run_waiter(WaiterFixture {
        fake_gh: r#"#!/usr/bin/env bash
set -euo pipefail
case "$1 $2" in
  "pr view")
    count_file="$STATE_DIR/view-count"
    count=0
    [[ -f "$count_file" ]] && read -r count < "$count_file"
    count=$((count + 1))
    printf '%s\n' "$count" > "$count_file"
    comment=comments-2
    ((count == 1)) && comment=comments-1
    printf 'OPEN\t%s\tsha-1\tupdated-1\t%s\treviews-1\n' 'https://github.com/example/project/pull/42' "$comment"
    ;;
  "pr checks") printf 'pass\tCI\tbuild\tSUCCESS\n' ;;
  *) exit 91 ;;
esac
"#,
        direct_pr: true,
        stall_seconds: 999,
    });
    let (stdout, _) = assert_successful_waiter(ExpectedWaiter {
        output,
        stdout_fragments: &["reason=checks_terminal"],
    });
    assert_eq!(
        stdout.matches("All 1 status checks are terminal").count(),
        2,
        "activity should restart the quiet period: {stdout}"
    );
}

#[test]
fn waiter_returns_when_pending_checks_stall() {
    assert_waiter_check(ExpectedCheck {
        command: r"printf 'pending\tCI\tbuild\tIN_PROGRESS\n'",
        stall_seconds: 0,
        stdout_fragments: &["reason=activity_stalled", "checks_pending=1"],
    });
}

#[test]
fn waiter_accepts_pending_rows_from_gh_exit_eight() {
    let (_, stderr) = assert_waiter_check(ExpectedCheck {
        command: "printf 'pending\\tCI\\tbuild\\tIN_PROGRESS\\n'; exit 8",
        stall_seconds: 0,
        stdout_fragments: &["reason=activity_stalled", "checks_pending=1"],
    });
    assert!(!stderr.contains("GitHub query failed"), "stderr={stderr}");
}

#[test]
fn waiter_treats_no_reported_checks_as_an_empty_set() {
    let (_, stderr) = assert_waiter_check(ExpectedCheck {
        command: "printf 'no checks reported\\n' >&2; exit 1",
        stall_seconds: 0,
        stdout_fragments: &["reason=activity_stalled", "checks_total=0"],
    });
    assert!(!stderr.contains("GitHub query failed"), "stderr={stderr}");
}

#[test]
fn waiter_keeps_successful_gh_stderr_out_of_parsed_values() {
    let output = run_waiter(WaiterFixture {
        fake_gh: r#"#!/usr/bin/env bash
set -euo pipefail
case "$1 $2" in
  "pr list")
    printf 'list warning\n' >&2
    printf 'example\t%s\n' 'https://github.com/example/project/pull/42'
    ;;
  "pr view")
    printf 'view warning\n' >&2
    printf 'OPEN\t%s\tsha-1\tupdated-1\tcomments-1\treviews-1\n' 'https://github.com/example/project/pull/42'
    ;;
  "pr checks")
    printf 'checks warning\n' >&2
    printf 'pass\tCI\tbuild\tSUCCESS\n'
    ;;
  *) exit 91 ;;
esac
"#,
        direct_pr: false,
        stall_seconds: 999,
    });
    let (_, stderr) = assert_successful_waiter(ExpectedWaiter {
        output,
        stdout_fragments: &["reason=checks_terminal", "checks_pass=1"],
    });
    assert!(
        stderr.is_empty(),
        "successful warnings must stay private: {stderr}"
    );
}

#[test]
fn waiter_treats_failed_checks_as_terminal() {
    assert_waiter_check(ExpectedCheck {
        command: r"printf 'fail\tCI\tbuild\tFAILURE\n'",
        stall_seconds: 999,
        stdout_fragments: &["reason=checks_terminal", "checks_fail=1"],
    });
}

#[test]
fn waiter_retries_transient_github_errors_and_limits_persistent_failures() {
    let transient = run_waiter(WaiterFixture {
        fake_gh: r#"#!/usr/bin/env bash
set -euo pipefail
case "$1 $2" in
  "pr view")
    count_file="$STATE_DIR/view-count"
    count=0
    [[ -f "$count_file" ]] && read -r count < "$count_file"
    count=$((count + 1))
    printf '%s\n' "$count" > "$count_file"
    if ((count <= 2)); then
      printf 'temporary outage\n' >&2
      exit 1
    fi
    printf 'OPEN\t%s\tsha-1\tupdated-1\tcomments-1\treviews-1\n' 'https://github.com/example/project/pull/42'
    ;;
  "pr checks") printf 'pass\tCI\tbuild\tSUCCESS\n' ;;
  *) exit 91 ;;
esac
"#,
        direct_pr: true,
        stall_seconds: 999,
    });
    let (stdout, stderr) = output_text(&transient);
    assert!(
        transient.status.success(),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(stderr.contains("attempt 1/3"));
    assert!(stderr.contains("attempt 2/3"));
    assert!(stdout.contains("reason=checks_terminal"));

    let persistent = run_waiter(WaiterFixture {
        fake_gh: r#"#!/usr/bin/env bash
set -euo pipefail
printf 'still unavailable\n' >&2
exit 1
"#,
        direct_pr: true,
        stall_seconds: 999,
    });
    let (stdout, stderr) = output_text(&persistent);
    assert!(
        !persistent.status.success(),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(stderr.contains("attempt 3/3"));
    assert!(stderr.contains("Stopping after 3 consecutive"));
}

#[test]
fn waiter_rejects_ambiguous_branch_matches() {
    let output = run_waiter(WaiterFixture {
        fake_gh: r#"#!/usr/bin/env bash
set -euo pipefail
case "$1 $2" in
  "pr list")
    printf 'alice\thttps://github.com/example/project/pull/41\n'
    printf 'bob\thttps://github.com/example/project/pull/42\n'
    ;;
  *) exit 91 ;;
esac
"#,
        direct_pr: false,
        stall_seconds: 999,
    });
    let (stdout, stderr) = output_text(&output);
    assert!(!output.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert!(stderr.contains("Multiple open pull requests"));
}
