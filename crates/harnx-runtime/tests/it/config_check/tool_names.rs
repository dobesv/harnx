//! Tool names agents and config refer to, checked against what the configured
//! tool servers list (`harnx-time-tools`, found next to the test binary the
//! way the worker finds it) and the names harnx knows offline.

use std::path::Path;
use std::time::Duration;

use harnx_runtime::config::{Config, ToolServerConfig};
use harnx_runtime::config_check::{
    check_config_dir, collect_tool_refs, list_servers, CheckOptions, Code, ConfigLayout, Report,
    ServerListing, Severity, KNOWN_TOOLSET_NAMES,
};

use super::{with_code, write, EnvVarGuard};

const TIME_SERVER: (&str, &str) = ("tool_servers/time.yaml", "command: harnx-time-tools\n");

/// Check a directory holding `files` with `options` adjusted by `adjust`.
async fn check_tools_with(
    files: &[(&str, &str)],
    adjust: impl FnOnce(&mut CheckOptions),
) -> Report {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        write(dir.path(), path, content);
    }
    let _config_dir = EnvVarGuard::set("HARNX_CONFIG_DIR", dir.path());
    let mut options = CheckOptions::new(dir.path().to_path_buf());
    adjust(&mut options);
    check_config_dir(&options).await.unwrap()
}

async fn check_tools(files: &[(&str, &str)]) -> Report {
    check_tools_with(files, |_| {}).await
}

fn agent(use_tools: &str) -> String {
    format!("---\nuse_tools: {use_tools}\n---\nhi\n")
}

/// Check a directory holding one tool server, `(path, yaml)`, and agent `a`
/// with `use_tools`.
async fn check_server_and_agent(server: (&str, &str), use_tools: &str) -> Report {
    let a = agent(use_tools);
    check_tools(&[server, ("agents/a.md", &a)]).await
}

const TOOL_CODES: [Code; 3] = [
    Code::UnknownTool,
    Code::ToolUnverified,
    Code::GlobMatchesNothing,
];

fn tool_findings(report: &Report) -> Vec<String> {
    report
        .findings
        .iter()
        .filter(|finding| TOOL_CODES.contains(&finding.code))
        .map(|finding| format!("{}: {}", finding.code.as_str(), finding.message))
        .collect()
}

/// An executable shell script `name` in `dir`, run as a tool
/// server command by its absolute path.
#[cfg(unix)]
fn script(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

/// Check tool server `name`, a script running `body`, with a 2 s listing
/// timeout, referred to as `<name>_x`; the check must finish well within
/// 10 s, whatever the script leaves running.
#[cfg(unix)]
async fn check_script_server_with_timeout(name: &str, body: &str) -> Report {
    let bin = tempfile::tempdir().unwrap();
    let command = script(bin.path(), name, body);
    let server_file = format!("tool_servers/{name}.yaml");
    let server = format!("command: {command}\n");
    let a = agent(&format!("{name}_x"));
    let started = std::time::Instant::now();

    let report = check_tools_with(&[(&server_file, &server), ("agents/a.md", &a)], |options| {
        options.list_timeout = Duration::from_secs(2)
    })
    .await;

    assert!(started.elapsed() < Duration::from_secs(10), "{report:#?}");
    report
}

#[tokio::test]
async fn known_tool_passes() {
    let report = check_server_and_agent(TIME_SERVER, "time_wait").await;

    assert!(tool_findings(&report).is_empty(), "{report:#?}");
    assert_eq!(report.summary.tools_unverified, 0, "{report:#?}");
}

#[tokio::test]
async fn misspelled_tool_is_error_with_hint() {
    let report = check_server_and_agent(TIME_SERVER, "time_wiat").await;

    let found = with_code(&report, Code::UnknownTool);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].severity, Severity::Error);
    assert_eq!(found[0].file.as_deref(), Some(Path::new("agents/a.md")));
    assert_eq!(found[0].key_path.as_deref(), Some("use_tools"));
    assert!(
        found[0].message.contains("did you mean time_wait"),
        "{}",
        found[0].message
    );
    assert!(found[0].message.contains("'time'"), "{}", found[0].message);
}

#[tokio::test]
async fn unknown_prefix_is_error() {
    let report = check_server_and_agent(TIME_SERVER, "nosuch_tool").await;

    let found = with_code(&report, Code::UnknownTool);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert!(
        found[0].message.contains("'nosuch_tool'"),
        "{}",
        found[0].message
    );
}

const CLOCK_SERVER: (&str, &str) = (
    "tool_servers/time.yaml",
    "command: harnx-time-tools\nargs: [--name, clock]\n",
);

#[tokio::test]
async fn renamed_server_uses_reported_name() {
    let report = check_server_and_agent(CLOCK_SERVER, "clock_wait").await;

    assert!(tool_findings(&report).is_empty(), "{report:#?}");
    assert_eq!(report.summary.tools_unverified, 0, "{report:#?}");
}

#[tokio::test]
async fn renamed_server_file_stem_is_not_a_prefix() {
    let report = check_server_and_agent(CLOCK_SERVER, "time_wait").await;

    let found = with_code(&report, Code::UnknownTool);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert!(
        found[0].message.contains("'time_wait'"),
        "{}",
        found[0].message
    );
    assert_eq!(tool_findings(&report).len(), 1, "{report:#?}");
}

#[tokio::test]
async fn same_and_cross_package_names() {
    let a = agent("\"time_wait,q__time_wait\"");
    let report = check_tools(&[
        (
            "packages/p/tool_servers/time.yaml",
            "command: harnx-time-tools\n",
        ),
        (
            "packages/q/tool_servers/time.yaml",
            "command: harnx-time-tools\n",
        ),
        ("packages/p/agents/a.md", &a),
    ])
    .await;

    assert!(tool_findings(&report).is_empty(), "{report:#?}");
}

#[tokio::test]
async fn glob_matching_nothing_warns() {
    let report = check_server_and_agent(TIME_SERVER, "\"time_zz*\"").await;

    let found = with_code(&report, Code::GlobMatchesNothing);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].severity, Severity::Warning);
    assert!(
        found[0].message.contains("'time_zz*'"),
        "{}",
        found[0].message
    );
    assert_eq!(tool_findings(&report).len(), 1, "{report:#?}");
}

#[tokio::test]
async fn ignored_tool_is_skipped() {
    let a = agent("dyn_x");
    let report = check_tools_with(&[("agents/a.md", &a)], |options| {
        options.ignore_tools = vec!["dyn_*".to_string()]
    })
    .await;

    assert!(tool_findings(&report).is_empty(), "{report:#?}");
    assert_eq!(report.summary.tools_ignored, 1, "{report:#?}");
}

#[cfg(unix)]
#[tokio::test]
async fn hanging_server_times_out_unverified() {
    let report = check_script_server_with_timeout("hang", "sleep 60").await;

    let found = with_code(&report, Code::ToolUnverified);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].severity, Severity::Warning);
    assert_eq!(
        found[0].file.as_deref(),
        Some(Path::new("tool_servers/hang.yaml"))
    );
    assert!(
        found[0].message.contains("timed out"),
        "{}",
        found[0].message
    );
    assert_eq!(report.summary.tools_unverified, 1, "{report:#?}");
    assert!(
        with_code(&report, Code::UnknownTool).is_empty(),
        "{report:#?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn failing_server_is_unverified() {
    let bin = tempfile::tempdir().unwrap();
    let fail = script(
        bin.path(),
        "fail",
        "echo 'cannot start: no token' >&2\nexit 1",
    );
    let garbage = script(bin.path(), "garbage", "echo 'not json'");
    let fail_server = format!("command: {fail}\n");
    let garbage_server = format!("command: {garbage}\n");
    let a = agent("\"fail_x,garbage_x,garbage_y\"");

    let report = check_tools(&[
        ("tool_servers/fail.yaml", &fail_server),
        ("tool_servers/garbage.yaml", &garbage_server),
        ("agents/a.md", &a),
    ])
    .await;

    let found = with_code(&report, Code::ToolUnverified);
    assert_eq!(found.len(), 2, "{report:#?}");
    let fail = found
        .iter()
        .find(|f| f.file.as_deref() == Some(Path::new("tool_servers/fail.yaml")))
        .expect("fail server finding");
    assert!(
        fail.message.contains("cannot start: no token"),
        "{}",
        fail.message
    );
    let garbage = found
        .iter()
        .find(|f| f.file.as_deref() == Some(Path::new("tool_servers/garbage.yaml")))
        .expect("garbage server finding");
    assert!(garbage.message.contains("2 "), "{}", garbage.message);
    assert_eq!(report.summary.tools_unverified, 3, "{report:#?}");
    assert!(
        with_code(&report, Code::UnknownTool).is_empty(),
        "{report:#?}"
    );
}

#[tokio::test]
async fn remote_server_is_unverified_without_spawning() {
    let report = check_server_and_agent(
        (
            "tool_servers/remote.yaml",
            "command: harnx-mcp-remote\nargs: [--url, \"https://example.invalid/mcp\"]\n",
        ),
        "remote_x",
    )
    .await;

    let found = with_code(&report, Code::ToolUnverified);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert!(
        found[0].message.contains("needs user credentials"),
        "{}",
        found[0].message
    );
    assert_eq!(report.summary.tools_unverified, 1, "{report:#?}");
}

#[tokio::test]
async fn subagent_and_handoff_tools_are_known() {
    let a = agent("\"b_session_prompt,b_session_handoff\"");
    let report = check_tools(&[("agents/a.md", &a), ("agents/b.md", "hello\n")]).await;

    assert!(tool_findings(&report).is_empty(), "{report:#?}");
}

#[tokio::test]
async fn toolset_name_expands() {
    let a = agent("timey");
    let report = check_tools(&[
        TIME_SERVER,
        ("config.yaml", "toolsets:\n  timey: [time_wait]\n"),
        ("agents/a.md", &a),
    ])
    .await;

    assert!(tool_findings(&report).is_empty(), "{report:#?}");
}

#[tokio::test]
async fn toolset_member_is_checked() {
    let report = check_tools(&[
        TIME_SERVER,
        ("config.yaml", "toolsets:\n  timey: [time_wiat]\n"),
    ])
    .await;

    let found = with_code(&report, Code::UnknownTool);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].file.as_deref(), Some(Path::new("config.yaml")));
    assert_eq!(found[0].key_path.as_deref(), Some("toolsets"));
}

#[cfg(unix)]
#[tokio::test]
async fn tools_skip_mode_spawns_nothing() {
    let bin = tempfile::tempdir().unwrap();
    let hang = script(bin.path(), "hang", "sleep 60");
    let server = format!("command: {hang}\n");
    let a = agent("hang_x");
    let started = std::time::Instant::now();

    let report = check_tools_with(
        &[("tool_servers/hang.yaml", &server), ("agents/a.md", &a)],
        |options| options.check_tools = false,
    )
    .await;

    assert!(started.elapsed() < Duration::from_secs(5), "{report:#?}");
    assert!(tool_findings(&report).is_empty(), "{report:#?}");
    assert!(report.summary.tools_unverified > 0, "{report:#?}");
}

#[tokio::test]
async fn missing_command_counts_without_a_second_warning() {
    let report = check_server_and_agent(
        (
            "tool_servers/gone.yaml",
            "command: definitely-not-installed-xyz\n",
        ),
        "gone_x",
    )
    .await;

    assert_eq!(
        with_code(&report, Code::CommandNotFound).len(),
        1,
        "{report:#?}"
    );
    assert!(tool_findings(&report).is_empty(), "{report:#?}");
    assert_eq!(report.summary.tools_unverified, 1, "{report:#?}");
}

#[tokio::test]
async fn middleware_helper_tools_are_checked() {
    let report = check_tools(&[(
        "tool_servers/time.yaml",
        "command: harnx-time-tools\nmiddleware:\n  - name: m\n    match: '.*'\n    bash: 'true'\n    tools: [time_wiat]\n",
    )])
    .await;

    let found = with_code(&report, Code::UnknownTool);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(
        found[0].file.as_deref(),
        Some(Path::new("tool_servers/time.yaml"))
    );
    assert_eq!(found[0].key_path.as_deref(), Some("middleware.tools"));
}

#[tokio::test]
async fn renamed_remote_server_is_unverified_by_its_configured_name() {
    let report = check_server_and_agent(
        (
            "tool_servers/github-token.yaml",
            "command: harnx-mcp-remote\nargs: [--url, \"https://example.invalid/mcp\", --name, github]\n",
        ),
        "github_x",
    )
    .await;

    let found = with_code(&report, Code::ToolUnverified);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(
        found[0].file.as_deref(),
        Some(Path::new("tool_servers/github-token.yaml"))
    );
    assert!(
        with_code(&report, Code::UnknownTool).is_empty(),
        "{report:#?}"
    );
    assert_eq!(report.summary.tools_unverified, 1, "{report:#?}");
}

#[cfg(unix)]
#[tokio::test]
async fn server_leaving_a_child_on_its_stdout_times_out() {
    let report = check_script_server_with_timeout(
        "linger",
        "sleep 60 &\necho '{\"server\":\"linger\",\"tools\":[]}'\nexit 0",
    )
    .await;

    let found = with_code(&report, Code::ToolUnverified);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert!(
        found[0].message.contains("timed out"),
        "{}",
        found[0].message
    );
}

#[tokio::test]
async fn skip_mode_counts_references_to_renamed_servers() {
    let report = check_tools_with(
        &[CLOCK_SERVER, ("agents/a.md", &agent("clock_wait"))],
        |options| options.check_tools = false,
    )
    .await;

    assert!(tool_findings(&report).is_empty(), "{report:#?}");
    assert_eq!(report.summary.tools_unverified, 1, "{report:#?}");
}

/// Check tool server `name`, a script whose stderr is `stderr` and which then
/// exits 1, referred to as `<name>_x`, and return its `tool-unverified`
/// message.
#[cfg(unix)]
async fn failure_message(name: &str, stderr: &str) -> String {
    let report =
        check_script_server_with_timeout(name, &format!("cat >&2 <<'EOF'\n{stderr}\nEOF\nexit 1"))
            .await;
    let found = with_code(&report, Code::ToolUnverified);
    assert_eq!(found.len(), 1, "{report:#?}");
    found[0].message.clone()
}

#[cfg(unix)]
#[tokio::test]
async fn failure_quotes_only_the_error_line() {
    let message = failure_message(
        "noisy",
        "2026-11-25T10:00:00.000Z [INFO ] 4242 harnx_mcp_bridge: inherited HTTP_PROXY=http://user:password@127.0.0.1:3128\nstarting up\n2026-11-25T10:00:00.100Z [WARN ] 4242 harnx_mcp_bridge: retrying\nError: boom\nCaused by: something",
    )
    .await;

    assert!(message.contains("boom"), "{message}");
    assert!(!message.contains("password"), "{message}");
    assert!(!message.contains("retrying"), "{message}");
    assert!(!message.contains("starting up"), "{message}");
}

#[cfg(unix)]
#[tokio::test]
async fn failure_redacts_credentials_in_the_quoted_line() {
    let message = failure_message(
        "proxied",
        "2026-11-25T10:00:00.000Z [INFO ] 4242 harnx_mcp_bridge: starting\ncannot reach https://alice:hunter2@proxy.example:3128/path",
    )
    .await;

    assert!(message.contains("cannot reach"), "{message}");
    assert!(!message.contains("hunter2"), "{message}");
    assert!(!message.contains("alice"), "{message}");
    assert!(
        message.contains("https://***@proxy.example:3128/path"),
        "{message}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn failure_with_only_a_backtrace_quotes_nothing() {
    let message = failure_message(
        "crashy",
        "stack backtrace:\n   0: rust_begin_unwind\n             at /rustc/abc/library/std/src/panicking.rs:665:5\n   1: crashy::main\nnote: Some details are omitted, run with `RUST_BACKTRACE=full` for a verbose backtrace.",
    )
    .await;

    assert!(message.contains("exited with code 1"), "{message}");
    assert!(!message.contains("rust_begin_unwind"), "{message}");
    assert!(!message.contains("panicking.rs"), "{message}");
    assert!(!message.contains("crashy::main"), "{message}");
    assert!(!message.contains("RUST_BACKTRACE"), "{message}");
}

/// `harnx-grep-tools` with an argument it rejects, so its listing fails.
const BROKEN_GREP_SERVER: (&str, &str) = (
    "tool_servers/grep.yaml",
    "command: harnx-grep-tools\nargs: [--bogus]\n",
);

#[tokio::test]
async fn unlistable_native_server_leaves_other_prefixes_unknown() {
    let a = agent("github_create_issue");
    let report = check_tools(&[TIME_SERVER, BROKEN_GREP_SERVER, ("agents/a.md", &a)]).await;

    let found = with_code(&report, Code::UnknownTool);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert!(
        with_code(&report, Code::ToolUnverified).is_empty(),
        "{report:#?}"
    );
    assert_eq!(report.summary.tools_unverified, 0, "{report:#?}");
}

#[tokio::test]
async fn unlistable_native_server_leaves_its_own_prefix_unverified() {
    let a = agent("\"grep_search,github_create_issue\"");
    let report = check_tools(&[BROKEN_GREP_SERVER, ("agents/a.md", &a)]).await;

    let found = with_code(&report, Code::ToolUnverified);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(
        found[0].file.as_deref(),
        Some(Path::new("tool_servers/grep.yaml"))
    );
    assert_eq!(
        with_code(&report, Code::UnknownTool).len(),
        1,
        "{report:#?}"
    );
    assert_eq!(report.summary.tools_unverified, 1, "{report:#?}");
}

#[tokio::test]
async fn k8s_sandbox_prefixes_are_unverified() {
    let report = check_server_and_agent(
        (
            "tool_servers/gateway.yaml",
            "command: harnx-k8s-sandbox-tools\nargs: [--fs-name=workspace]\nenv:\n  BASH_TOOLSET_NAME: shell\n",
        ),
        "\"sandbox_connect,workspace_read,shell_exec,fs_read,zzz_x\"",
    )
    .await;

    let unverified = with_code(&report, Code::ToolUnverified);
    assert_eq!(unverified.len(), 1, "{report:#?}");
    assert!(
        unverified[0].message.contains("3 tool references"),
        "{}",
        unverified[0].message
    );
    let unknown: Vec<&str> = with_code(&report, Code::UnknownTool)
        .iter()
        .map(|finding| finding.message.as_str())
        .collect();
    assert_eq!(unknown.len(), 2, "{report:#?}");
    assert!(
        unknown.iter().any(|m| m.contains("'fs_read'")),
        "{unknown:?}"
    );
    assert!(unknown.iter().any(|m| m.contains("'zzz_x'")), "{unknown:?}");
}

#[tokio::test]
async fn skip_mode_counts_references_judged_offline() {
    let report = check_tools_with(
        &[("agents/a.md", &agent("\"time_wait,github_create_issue\""))],
        |options| options.check_tools = false,
    )
    .await;

    assert!(tool_findings(&report).is_empty(), "{report:#?}");
    assert_eq!(report.summary.tools_unverified, 2, "{report:#?}");
}

#[tokio::test]
async fn middleware_of_a_server_without_a_file_names_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let layout = ConfigLayout {
        root: dir.path().to_path_buf(),
        ..ConfigLayout::default()
    };
    let mut server: ToolServerConfig = serde_yaml::from_str(
        "command: harnx-time-tools\nmiddleware:\n  - name: m\n    match: '.*'\n    bash: 'true'\n    tools: [time_wiat]\n",
    )
    .unwrap();
    server.name = "time".to_string();
    let config = Config {
        tool_servers: vec![server],
        ..Config::default()
    };

    let refs = collect_tool_refs(&layout, &config, &[]);

    let middleware: Vec<_> = refs
        .iter()
        .filter(|r| r.key == "middleware.tools")
        .collect();
    assert_eq!(middleware.len(), 1, "{refs:#?}");
    assert_eq!(middleware[0].source, dir.path().join("tool_servers"));
}

#[tokio::test]
async fn known_binaries_register_their_default_names() {
    let servers: Vec<ToolServerConfig> = KNOWN_TOOLSET_NAMES
        .iter()
        .map(|(binary, _)| {
            let mut server: ToolServerConfig =
                serde_yaml::from_str(&format!("command: {binary}\n")).unwrap();
            server.name = binary.to_string();
            server
        })
        .collect();

    let listings = list_servers(&servers, Duration::from_secs(30)).await;

    for (binary, name) in KNOWN_TOOLSET_NAMES {
        match listings.get(*binary) {
            Some(ServerListing::Listed(listing)) => {
                assert_eq!(listing.server, *name, "{binary} registers as");
            }
            other => panic!("{binary} didn't list its tools: {other:?}"),
        }
    }
}

#[tokio::test]
async fn handoff_tool_in_bash_tools_is_error() {
    let a = "---\nbash_tools: [b_session_handoff]\nuse_tools: b_session_handoff\n---\nhi\n";
    let report = check_tools(&[("agents/a.md", a), ("agents/b.md", "hello\n")]).await;

    let found = with_code(&report, Code::UnknownTool);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].key_path.as_deref(), Some("bash_tools"));
    assert!(
        found[0].message.contains("b_session_handoff"),
        "{}",
        found[0].message
    );
}

#[tokio::test]
async fn glob_toolset_member_is_error() {
    let report = check_tools(&[
        TIME_SERVER,
        ("config.yaml", "toolsets:\n  timey: ['time_*']\n"),
    ])
    .await;

    let found = with_code(&report, Code::UnknownTool);
    assert_eq!(found.len(), 1, "{report:#?}");
    assert_eq!(found[0].key_path.as_deref(), Some("toolsets"));
    assert!(found[0].message.contains("exact"), "{}", found[0].message);
}

#[tokio::test]
async fn package_view_of_a_glob_toolset_member_is_error() {
    let a = agent("timey");
    let report = check_tools(&[
        TIME_SERVER,
        ("config.yaml", "toolsets:\n  timey: ['time_*']\n"),
        ("packages/p/agents/a.md", &a),
    ])
    .await;

    let found = with_code(&report, Code::UnknownTool);
    let files: Vec<_> = found.iter().map(|f| f.file.as_deref()).collect();
    assert!(
        files.contains(&Some(Path::new("packages/p/agents/a.md"))),
        "{report:#?}"
    );
}
