//! Registered `bash_publish_plan` tool through a real installed package and
//! Git HTTPS transport: typed inputs, worktree selection by the script's cd,
//! anonymous direct probe, authenticated confirmation and push over the
//! ambient GitHub credential route, no URL on failure.
use anyhow::Result;
use harnx_engine::tool::ToolEvalContext;
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

use super::proxy_auth::invoke;

const BRANCH: &str = "fixture/installed";
const REF: &str = "refs/plans/fixture/installed";
const DESTINATION: &str = "https://github.com/example/project";

// The only fake curl request is the direct/anonymous one: assert that it
// can't see proxy, CA, config or token env, and carries no Authorization.
// The authenticated confirmation uses real curl through the ambient egress
// route the bash server's environment provides.
const CURL: &str = r#"#!/usr/bin/env bash
set -euo pipefail
[[ $1 == -q ]]
case " $* " in
    *' --proxy  --noproxy * '*)
        [[ -z ${HTTPS_PROXY+x}${https_proxy+x}${NO_PROXY+x} ]]
        [[ -z ${GIT_SSL_CAINFO+x}${SSL_CERT_FILE+x}${CURL_HOME+x}${HOME+x} ]]
        [[ -z ${GH_TOKEN+x}${GITHUB_TOKEN+x} ]]
        [[ " $* " != *' Authorization:'* && " $* " != *' --netrc'* && " $* " != *' --cacert '* ]]
        [[ ${!#} == https://api.github.com/repos/example/project ]]
        printf '{"message":"Not Found"}\n404'
        ;;
    *) export PATH=${PATH#*:}; exec curl "$@" ;;
esac
"#;

fn packet(data: &str) -> String {
    format!("{:04x}{data}", data.len() + 4)
}

fn advertisement(service: &str, capabilities: &str) -> Value {
    let intro = packet(&format!("# service=git-{service}\n"));
    json!({"status": 200, "headers": {"content-type": format!("application/x-git-{service}-advertisement")},
           "body": format!("{intro}0000{capabilities}0000")})
}

pub(super) fn github_fixture_hook() -> String {
    // Terminal synthetic GitHub: never forward anything upstream. Real Git
    // drives upload/receive-pack discovery and the POST. The storage fixture
    // checks the candidate/bytes against a real disposable remote instead.
    let repo = json!({"status": 200, "body": json!({
        "full_name": "example/project", "html_url": DESTINATION, "private": true, "visibility": "private"
    }).to_string()});
    let upload = advertisement("upload-pack", "");
    let receive = advertisement(
        "receive-pack",
        &packet(&format!(
            "{} capabilities^{{}}\0report-status\n",
            "0".repeat(40)
        )),
    );
    let push = json!({"status": 200, "headers": {"content-type": "application/x-git-receive-pack-result"},
                      "body": format!("{}{}0000", packet("unpack ok\n"), packet(&format!("ok {REF}\n")))});
    format!(
        r#"
        if .method == "CONNECT" then .
        elif .host == "api.github.com" and .method == "GET" and .path == "/repos/example/project"
            and .headers.authorization == "Bearer fixture-github-token" then .respond = {repo}
        elif .host == "github.com" and .headers.authorization == ("x-access-token:fixture-github-token" | @base64 | "Basic " + .) then
            if .method == "GET" and .path == "/example/project.git/info/refs?service=git-upload-pack" then .respond = {upload}
            elif .method == "GET" and .path == "/example/project.git/info/refs?service=git-receive-pack" then .respond = {receive}
            elif .method == "POST" and .path == "/example/project.git/git-receive-pack" then .respond = {push}
            else .block = "unexpected Git request" end
        else .block = "unexpected or unauthenticated request" end
    "#
    )
}

pub(super) fn install_publisher_fixture(source: &Path, installed: &Path) -> Result<()> {
    for name in [
        "tool_servers/bash.yaml",
        "tool_servers/proxy_auth.yaml",
        "middleware/proxy-auth.py",
        "hooks/jira-auth-hook.py",
        "scripts/publish-plan.sh",
        "bash_tools/publish_plan.yaml",
    ] {
        let target = installed.join(name);
        fs::create_dir_all(target.parent().expect("package file parent"))?;
        fs::copy(source.join(name), target)?;
    }
    Ok(())
}

/// The delivery worktree the typed `worktree` argument must select: a real Git
/// repository with the fixture origin, never the package's source checkout.
fn init_worktree(work: &Path) -> Result<()> {
    for args in [
        ["init", "-q", "--object-format=sha1"],
        [
            "config",
            "remote.origin.url",
            "https://github.com/example/project.git",
        ],
    ] {
        let status = Command::new("git")
            .arg("-c")
            .arg("init.templateDir=")
            .args(args)
            .current_dir(work)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()?;
        if !status.success() {
            anyhow::bail!("fixture git {args:?} failed in {}", work.display());
        }
    }
    Ok(())
}

/// Runs `git` in `work` and returns its stdout.
fn git_out(work: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-c")
        .arg("init.templateDir=")
        .args(args)
        .current_dir(work)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "fixture git {args:?} failed in {}: {}",
            work.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Loose and packed object counts: the number that grows whenever execution
/// history captures the worktree into the local Git object store.
fn object_census(work: &Path) -> Result<(u64, u64)> {
    let counts = git_out(work, &["count-objects", "-v"])?;
    let mut census = (0, 0);
    for line in counts.lines() {
        if let Some(count) = line.strip_prefix("count: ") {
            census.0 = count.parse()?;
        } else if let Some(count) = line.strip_prefix("in-pack: ") {
            census.1 = count.parse()?;
        }
    }
    Ok(census)
}

/// Whether any object in the worktree's local Git store carries `needle`.
fn objects_contain(work: &Path, needle: &str) -> Result<bool> {
    Ok(git_out(work, &["cat-file", "--batch-all-objects", "--batch"])?.contains(needle))
}

async fn publish(ctx: &ToolEvalContext, work: &Path, source: &str, branch: &str) -> Value {
    invoke(
        ctx,
        "bash_publish_plan",
        json!({
            "source": source,
            "branch": branch,
            "worktree": work.to_string_lossy(),
            "timeout_secs": 60,
        }),
    )
    .await
}

pub(super) async fn check_installed(ctx: &ToolEvalContext, root: &Path) -> Result<()> {
    let installed = root.join("packages/pantheon");
    assert!(
        installed.join("bash_tools/publish_plan.yaml").is_file(),
        "registered publish tool template"
    );
    assert!(
        installed.join("scripts/publish-plan.sh").is_file(),
        "installed script"
    );
    let bin = root.join("bin");
    let work = root.join("work");
    fs::create_dir(&bin)?;
    fs::create_dir(&work)?;
    init_worktree(&work)?;
    let curl = bin.join("curl");
    fs::write(&curl, CURL)?;
    fs::set_permissions(&curl, fs::Permissions::from_mode(0o755))?;

    // A tracked file committed by the fixture itself: its bytes are
    // legitimately in the local object store, so later assertions can demand
    // that a refused publication adds no object beyond them.
    fs::write(work.join("delivery.txt"), "tracked fixture content\n")?;
    git_out(&work, &["add", "delivery.txt"])?;
    git_out(
        &work,
        &[
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "user.name=fixture",
            "commit",
            "-q",
            "-m",
            "fixture delivery",
        ],
    )?;

    // Success: metadata and the local ref identify the snapshot in this worktree.
    // Fixture curl checks that the anonymous probe has no proxy, CA, config, or token.
    fs::write(work.join("snapshot.md"), "# Installed fixture\r\n")?;
    let result = publish(ctx, &work, "snapshot.md", BRANCH).await;
    assert_eq!(result["exit_code"], 0, "{result}");
    let metadata: Value =
        serde_json::from_str(result["stdout"].as_str().expect("publisher stdout"))?;
    assert_eq!(metadata["ref"], format!("refs/plans/{BRANCH}"));
    assert_eq!(metadata["reused"], false);
    let sha = metadata["sha"].as_str().expect("snapshot SHA");
    let file = metadata["file"].as_str().expect("snapshot filename");
    assert_eq!(
        metadata["plan_url"],
        format!("{DESTINATION}/blob/{sha}/{file}")
    );
    assert_eq!(
        fs::read_to_string(work.join(".git").join(REF))?,
        format!("{sha}\n")
    );

    // Refuse the untracked secret before any Git object or ref write. A
    // host-side execution-history snapshot would copy it into the object store
    // before the publisher's scanner runs.
    fs::write(
        work.join("secret.md"),
        "token: ghp_0123456789abcdefghijKLM\n",
    )?;
    let secret_branch = format!("{BRANCH}-refused");
    let census_before = object_census(&work)?;
    let result = publish(ctx, &work, "secret.md", &secret_branch).await;
    assert_eq!(result["exit_code"], 1, "{result}");
    let diagnostic = result["diagnostic"].as_str().expect("full tool result");
    assert!(
        diagnostic.contains("snapshot contains potentially sensitive content"),
        "refusal diagnostic must name the scan refusal: {diagnostic}"
    );
    assert!(
        !diagnostic.contains("plan_url") && !diagnostic.contains(DESTINATION),
        "refused publication must not produce a URL: {diagnostic}"
    );
    assert!(
        !work
            .join(".git")
            .join(format!("refs/plans/{secret_branch}"))
            .exists(),
        "refused publication must not write a ref"
    );
    assert_eq!(
        object_census(&work)?,
        census_before,
        "refused publication must not add local Git objects (no host-side \
         pre-scan snapshot of the flagged worktree)"
    );
    assert!(
        !objects_contain(&work, "ghp_0123456789abcdefghijKLM")?,
        "flagged untracked bytes must not be retained in the worktree's \
         local Git objects"
    );

    // This transport fixture discards the push. Reuse and ancestry are covered
    // by publish_plan.rs's disposable-remote tests in harnx-bash-tools.
    Ok(())
}
