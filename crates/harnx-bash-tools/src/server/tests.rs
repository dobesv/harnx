use super::*;

#[cfg(unix)]
use std::ffi::OsString;

use rmcp::handler::client::ClientHandler;
use rmcp::model::{ClientCapabilities, InitializeRequestParams};
use rmcp::service::{serve_client, serve_server, RoleClient, RoleServer, RunningService};
use tokio::io::duplex;

#[cfg(target_os = "linux")]
use harnx_toolset::Toolset;
#[cfg(target_os = "linux")]
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harnx-bash-tools-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Default)]
struct TestClientHandler;

impl ClientHandler for TestClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("test", "0.1"),
        )
    }
}

#[allow(dead_code)]
struct TestConnection {
    _server_service: RunningService<RoleServer, BashServer>,
    client_service: RunningService<RoleClient, TestClientHandler>,
}

#[allow(dead_code)]
async fn connect_server(server: BashServer, _roots: Vec<PathBuf>) -> TestConnection {
    let (client_transport, server_transport) = duplex(65_536);
    let server_fut = serve_server(server, server_transport);
    let client_fut = serve_client(TestClientHandler, client_transport);
    let (server_res, client_res) = tokio::join!(server_fut, client_fut);

    TestConnection {
        _server_service: server_res.unwrap(),
        client_service: client_res.unwrap(),
    }
}

fn text_content(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            rmcp::model::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn bash_tools_advertise_call_template_only() {
    // Each tool ships a `_meta.call_template` for the TUI's call header.
    // We deliberately omit `result_template` so the MCP client falls
    // back to its audience-aware generic renderer — that's what surfaces
    // the history diff content blocks (issue #398).
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let server = server_with_sandbox(
        vec![temp_dir.path().to_path_buf()],
        disabled_sandbox_config(),
    );
    let TestConnection {
        _server_service,
        client_service,
    } = connect_server(server, vec![temp_dir.path().to_path_buf()]).await;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    let tools = peer.list_tools(Default::default()).await.unwrap().tools;
    assert!(!tools.is_empty(), "server should expose at least one tool");
    for tool in &tools {
        let meta = tool
            .meta
            .as_ref()
            .unwrap_or_else(|| panic!("tool '{}' has no _meta", tool.name));
        assert!(
            meta.0.contains_key("call_template"),
            "tool '{}' missing call_template in _meta",
            tool.name
        );
        assert!(
                !meta.0.contains_key("result_template"),
                "tool '{}' must not pin result_template — let the client fall back to its generic audience-aware renderer",
                tool.name
            );
    }
}

#[cfg(unix)]
fn collect_arg_pairs(args: &[OsString]) -> Vec<(String, String)> {
    args.chunks(2)
        .filter_map(|w| {
            if w.len() == 2 {
                Some((
                    w[0].to_string_lossy().into_owned(),
                    w[1].to_string_lossy().into_owned(),
                ))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(unix)]
use crate::test_support::{env_lock, EnvVar};

fn allowlist_for_paths(paths: Vec<PathBuf>) -> Arc<ResolvedAllowlist> {
    let inputs = harnx_tool_allow::AllowInputs {
        rwx: paths,
        exec: system_exec_paths(),
        read: system_read_paths(),
        ..harnx_tool_allow::AllowInputs::default()
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Arc::new(harnx_tool_allow::resolve_allowlist(
        &inputs,
        &cwd,
        &harnx_tool_allow::AllowEnv::from_current_process(),
    ))
}

fn system_exec_paths() -> Vec<PathBuf> {
    harnx_sandbox_common::SYSTEM_EXEC_PATHS
        .iter()
        .filter(|path| **path != "/tmp")
        .map(PathBuf::from)
        .collect()
}

fn system_read_paths() -> Vec<PathBuf> {
    harnx_sandbox_common::SYSTEM_READ_PATHS
        .iter()
        .map(PathBuf::from)
        .collect()
}

fn server_with_paths(paths: Vec<PathBuf>) -> BashServer {
    BashServer::new((*allowlist_for_paths(paths)).clone())
}

fn server_with_sandbox(paths: Vec<PathBuf>, mut config: SandboxConfig) -> BashServer {
    config.allowlist = allowlist_for_paths(paths);
    BashServer::new_with_sandbox(config)
}

#[cfg(unix)]
fn enabled_sandbox_config() -> SandboxConfig {
    SandboxConfig {
        enabled: true,
        allowlist: Arc::new(ResolvedAllowlist::new()),
        extra_env_passthrough: vec![],
        env_overrides: vec![],
        sandbox_run_path: PathBuf::from("harnx-sandbox-exec"),
    }
}

#[tokio::test]
async fn rollback_rejects_repo_root_outside_write_grant() {
    let temp = TestDir::new();
    let base = temp.path().canonicalize().expect("canonical tempdir");
    let repo = base.join("repo");
    let allowed = repo.join("allowed");
    std::fs::create_dir_all(&allowed).expect("create allowed subdirectory");
    let init = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&repo)
        .status()
        .expect("run git init");
    assert!(init.success());
    let server = server_with_paths(vec![allowed.clone()]);

    let denied = server
        .rollback_file_impl(RollbackParams {
            commit_id: "0000000000000000000000000000000000000000".to_string(),
            repo_path: allowed.to_string_lossy().into_owned(),
        })
        .await
        .unwrap_err();

    assert!(
        denied.message.contains("outside allowed write paths"),
        "unexpected error: {}",
        denied.message
    );
    assert!(denied.message.contains(&repo.to_string_lossy().to_string()));
}

#[tokio::test]
async fn default_working_dir_skips_file_grants() {
    let temp = TestDir::new();
    let base = temp.path().canonicalize().expect("canonical tempdir");
    let file = base.join("allowed-file");
    std::fs::write(&file, "content").expect("write file grant");
    let mut allowlist = ResolvedAllowlist::new();
    allowlist.insert_read(&file);
    let server = BashServer::new(allowlist);

    let denied = server.resolve_working_dir(None).await.unwrap_err();

    assert!(denied
        .message
        .contains("no allowed paths are configured for a working directory"));
}

#[cfg(unix)]
fn disabled_sandbox_config() -> SandboxConfig {
    SandboxConfig {
        enabled: false,
        ..enabled_sandbox_config()
    }
}

#[cfg(target_os = "linux")]
fn sandboxed_server(roots: Vec<PathBuf>) -> BashServer {
    server_with_sandbox(
        roots,
        SandboxConfig {
            enabled: true,
            allowlist: Arc::new(ResolvedAllowlist::new()),
            extra_env_passthrough: vec![],
            env_overrides: vec![],
            sandbox_run_path: sandbox_run_test_path(),
        },
    )
}

#[cfg(target_os = "linux")]
fn sandboxed_read_exec_server(root: &Path) -> BashServer {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut inputs = harnx_tool_allow::AllowInputs {
        read: vec![root.to_path_buf()],
        exec: vec![root.to_path_buf()],
        ..harnx_tool_allow::AllowInputs::default()
    };
    inputs.exec.extend(system_exec_paths());
    inputs.read.extend(system_read_paths());
    BashServer::new_with_sandbox(SandboxConfig {
        allowlist: Arc::new(harnx_tool_allow::resolve_allowlist(
            &inputs,
            &cwd,
            &harnx_tool_allow::AllowEnv::from_current_process(),
        )),
        ..enabled_sandbox_config()
    })
}

/// Probe whether the sandbox helper can actually initialize in the current
/// environment. GitHub Actions Ubuntu runners and other restricted Linux
/// environments commonly disallow unprivileged user namespaces, which
/// causes `Sandbox::spawn()` to fail with EPERM at runtime. The
/// sandbox-runtime tests below short-circuit and log a "skipping" message
/// when this returns false, instead of failing the build.
#[cfg(target_os = "linux")]
fn sandbox_runtime_works() -> bool {
    let helper = sandbox_run_test_path();
    if !helper.exists() {
        eprintln!(
            "sandbox runtime probe: helper not built at {} — skipping",
            helper.display()
        );
        return false;
    }
    let output = std::process::Command::new(&helper)
        .args([
            "--exec",
            "/usr/bin",
            "--exec",
            "/bin",
            "--exec",
            "/lib",
            "--exec",
            "/lib64",
            "--exec",
            "/usr/lib",
            "--exec",
            "/usr/lib64",
            "--exec",
            "/usr/lib/x86_64-linux-gnu",
            "--exec",
            "/etc",
            "--exec",
            "/proc",
            "--exec",
            "/dev",
            "--exec",
            "/tmp",
            "--exec",
            "/usr/share",
            "--working-dir",
            "/tmp",
            "--",
            "bash",
            "-c",
            "exit 0",
        ])
        .output();
    match output {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            eprintln!(
                    "sandbox runtime probe: sandbox helper cannot initialize here (exit={:?}, stderr={:?}) — skipping",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            false
        }
        Err(err) => {
            eprintln!("sandbox runtime probe: failed to spawn helper: {err} — skipping");
            false
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn tool_template_sandbox_grants_control_real_file_access() {
    if !sandbox_runtime_works() {
        return;
    }

    let working_dir = TestDir::new();
    let secret_dir = TestDir::new();
    let secret_file = secret_dir.path().join("capability-marker.txt");
    let marker = "template-sandbox-capability-marker";
    std::fs::write(&secret_file, marker).expect("write capability marker");

    let secret_dir_yaml =
        serde_json::to_string(&secret_dir.path().to_string_lossy()).expect("quote grant path");
    let cat_script = format!(
        "cat {}",
        serde_json::to_string(&secret_file.to_string_lossy()).expect("quote marker path")
    );
    let granted = crate::tool_template::parse_template_str(
        &format!(
            "name: read_granted\nsandbox:\n  read:\n    - {secret_dir_yaml}\nscript: |\n  {cat_script}\n"
        ),
        Path::new("read_granted.yaml"),
    )
    .expect("parse granted template");
    let denied = crate::tool_template::parse_template_str(
        &format!("name: read_denied\nscript: |\n  {cat_script}\n"),
        Path::new("read_denied.yaml"),
    )
    .expect("parse denied template");
    let unsandboxed = crate::tool_template::parse_template_str(
        &format!("name: read_unsandboxed\nsandbox:\n  enabled: false\nscript: |\n  {cat_script}\n"),
        Path::new("read_unsandboxed.yaml"),
    )
    .expect("parse unsandboxed template");

    let mut sandbox_config = enabled_sandbox_config();
    sandbox_config.allowlist = allowlist_for_paths(vec![working_dir.path().to_path_buf()]);
    sandbox_config.sandbox_run_path = sandbox_run_test_path();
    let toolset = crate::BashToolset::new(sandbox_config, vec![granted, denied, unsandboxed])
        .await
        .expect("build template toolset");

    let granted: CallToolResult = serde_json::from_value(
        toolset
            .invoke(
                "read_granted",
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .expect("invoke granted template"),
    )
    .expect("deserialize granted result");
    let granted_text = text_content(&granted);
    assert_eq!(extract_field(&granted_text, "exit_code"), "0");
    assert!(
        granted_text.contains(marker),
        "granted template did not read marker:\n{granted_text}"
    );

    let denied: CallToolResult = serde_json::from_value(
        toolset
            .invoke(
                "read_denied",
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .expect("invoke denied template"),
    )
    .expect("deserialize denied result");
    let denied_text = text_content(&denied);
    let denied_exit = extract_field(&denied_text, "exit_code")
        .parse::<i32>()
        .expect("numeric denied exit code");
    let denied_stderr = denied_text
        .split_once("<!-- start stderr -->")
        .and_then(|(_, rest)| rest.split_once("<!-- end stderr -->"))
        .map(|(stderr, _)| stderr)
        .expect("denied result stderr block")
        .to_ascii_lowercase();
    assert_ne!(
        denied_exit, 0,
        "ungranted template unexpectedly succeeded:\n{denied_text}"
    );
    assert!(
        denied_stderr.contains("permission") || denied_stderr.contains("denied"),
        "ungranted template stderr lacked permission denial:\n{denied_text}"
    );
    assert!(
        !denied_text.contains(marker),
        "ungranted template exposed marker:\n{denied_text}"
    );

    let unsandboxed: CallToolResult = serde_json::from_value(
        toolset
            .invoke(
                "read_unsandboxed",
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .expect("invoke unsandboxed template"),
    )
    .expect("deserialize unsandboxed result");
    let unsandboxed_text = text_content(&unsandboxed);
    assert_eq!(extract_field(&unsandboxed_text, "exit_code"), "0");
    assert!(
        unsandboxed_text.contains(marker),
        "unsandboxed template did not read marker:\n{unsandboxed_text}"
    );

    let _ = toolset.cleanup_log_dir();
}

#[cfg(unix)]
fn sandbox_server(root: impl Into<PathBuf>) -> BashServer {
    server_with_sandbox(vec![root.into()], enabled_sandbox_config())
}

fn exec_params(command: impl Into<String>, working_dir: &Path) -> ExecCommandParams {
    ExecCommandParams {
        command: command.into(),
        working_dir: Some(working_dir.to_string_lossy().to_string()),
        timeout_secs: Some(15),
        head_lines: None,
        tail_lines: None,
        max_output_bytes: None,
        env: None,
    }
}

fn spawn_params(command: impl Into<String>, working_dir: &Path) -> SpawnCommandParams {
    SpawnCommandParams {
        command: command.into(),
        working_dir: Some(working_dir.to_string_lossy().to_string()),
        env: None,
    }
}

fn wait_params(execution_id: impl Into<String>) -> WaitParams {
    WaitParams {
        execution_id: execution_id.into(),
        timeout_secs: Some(5),
        head_lines: None,
        tail_lines: None,
        max_output_bytes: None,
        grep: None,
    }
}

fn read_exec_log_params(execution_id: impl Into<String>, stream: &str) -> ReadExecLogParams {
    ReadExecLogParams {
        execution_id: execution_id.into(),
        stream: stream.to_string(),
        offset: None,
        limit: None,
        tail: None,
        grep: None,
        head_lines: None,
        tail_lines: None,
        max_output_bytes: None,
    }
}

fn assert_text_contains_all(text: &str, needles: &[&str]) {
    for needle in needles {
        assert!(text.contains(needle), "missing {needle:?} in text: {text}");
    }
}

#[test]
fn test_exec_params_ignores_legacy_inputs_outputs() {
    let json = r#"{"command":"echo hi","inputs":["/tmp/in"],"outputs":["/tmp/out"]}"#;
    let parsed: ExecCommandParams =
        serde_json::from_str(json).expect("legacy payload must deserialize");
    assert_eq!(parsed.command, "echo hi");
}

fn assert_execution_metadata_fields(text: &str) {
    assert_text_contains_all(
        text,
        &[
            "execution_id:",
            "command:",
            "working_dir:",
            "stdout_log_path:",
            "stderr_log_path:",
        ],
    );
}

#[cfg(unix)]
fn assert_child_env_contains(child_env: &[(String, String)], key: &str, value: &str) {
    assert!(
        child_env
            .iter()
            .any(|(env_key, env_value)| env_key == key && env_value == value),
        "missing env {key}={value} in child env: {child_env:?}"
    );
}

#[cfg(unix)]
fn assert_child_env_absent(child_env: &[(String, String)], key: &str) {
    assert!(
        !child_env.iter().any(|(env_key, _)| env_key == key),
        "unexpected env {key} in child env: {child_env:?}"
    );
}

fn extract_field(text: &str, field: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{field}: ")))
        .unwrap()
        .to_string()
}

mod sandbox_args {
    use super::*;

    #[test]
    fn empty_allowlist_emits_no_filesystem_flags() {
        let server = BashServer::new_with_sandbox(enabled_sandbox_config());
        let args = server.build_sandbox_args(Path::new("/tmp"));
        let pairs = collect_arg_pairs(&args);
        assert!(!pairs
            .iter()
            .any(|(flag, _)| matches!(flag.as_str(), "--read" | "--write" | "--exec")));
    }

    #[test]
    fn explicit_rwx_maps_to_all_sandbox_permissions() {
        let root = PathBuf::from("/test/root");
        let server = server_with_sandbox(vec![root], enabled_sandbox_config());
        let pairs = collect_arg_pairs(&server.build_sandbox_args(Path::new("/test/root")));
        assert!(pairs.contains(&("--read".into(), "/test/root".into())));
        assert!(pairs.contains(&("--write".into(), "/test/root".into())));
        assert!(pairs.contains(&("--exec".into(), "/test/root".into())));
    }

    #[test]
    fn common_default_batch_emits_system_grants() {
        let inputs = harnx_tool_allow::AllowInputs {
            common_default: true,
            ..harnx_tool_allow::AllowInputs::default()
        };
        let allowlist = harnx_tool_allow::resolve_allowlist(
            &inputs,
            Path::new("/workspace"),
            &harnx_tool_allow::AllowEnv::default(),
        );
        let mut config = enabled_sandbox_config();
        config.allowlist = Arc::new(allowlist);
        let pairs = collect_arg_pairs(
            &BashServer::new_with_sandbox(config).build_sandbox_args(Path::new("/workspace")),
        );
        assert!(pairs.contains(&("--read".into(), "/usr/bin".into())));
        assert!(pairs.contains(&("--exec".into(), "/usr/bin".into())));
    }
}

#[cfg(unix)]
#[test]
fn env_default_allowlist_vars_passed_through() {
    let _env_guard = env_lock();
    let _home = EnvVar::set("HOME", "/tmp/harnx-home-4-1");
    let _path = EnvVar::set("PATH", "/tmp/harnx-bin-4-1");
    let _secret = EnvVar::set("HARNX_TEST_SECRET_4_1", "hunter2");
    let _config_dir = EnvVar::unset("HARNX_CONFIG_DIR");

    let server = server_with_sandbox(vec![], enabled_sandbox_config());
    let child_env = server.build_child_env();

    assert_child_env_contains(&child_env, "HOME", "/tmp/harnx-home-4-1");
    assert_child_env_contains(&child_env, "PATH", "/tmp/harnx-bin-4-1");
    assert_child_env_absent(&child_env, "HARNX_TEST_SECRET_4_1");
}

#[cfg(unix)]
#[test]
fn env_overrides_and_passthrough() {
    let _env_guard = env_lock();
    let _host_value = EnvVar::set("HARNX_TEST_CUSTOM_4_2", "from_host");
    let _config_dir = EnvVar::unset("HARNX_CONFIG_DIR");

    let mut passthrough_config = enabled_sandbox_config();
    passthrough_config.extra_env_passthrough = vec!["HARNX_TEST_CUSTOM_4_2".to_string()];
    let passthrough_server = server_with_sandbox(vec![], passthrough_config);
    let passthrough_env = passthrough_server.build_child_env();
    assert!(passthrough_env
        .iter()
        .any(|(key, value)| { key == "HARNX_TEST_CUSTOM_4_2" && value == "from_host" }));

    let mut override_config = enabled_sandbox_config();
    override_config.extra_env_passthrough = vec!["HARNX_TEST_CUSTOM_4_2".to_string()];
    override_config.env_overrides = vec![(
        "HARNX_TEST_CUSTOM_4_2".to_string(),
        "overridden".to_string(),
    )];
    let override_server = server_with_sandbox(vec![], override_config);
    let override_env = override_server.build_child_env();
    assert!(override_env
        .iter()
        .any(|(key, value)| { key == "HARNX_TEST_CUSTOM_4_2" && value == "overridden" }));
}

/// Non-interactive safe defaults (fallbacks) are present when neither the
/// host environment nor user config has set them, preventing programs from
/// corrupting the TUI, hanging on interactive pagers, or emitting ANSI
/// escapes.
#[cfg(unix)]
#[test]
fn env_non_interactive_defaults_applied_when_not_configured() {
    let _env_guard = env_lock();
    let _config_dir = EnvVar::unset("HARNX_CONFIG_DIR");

    // Unset all vars that appear in NON_INTERACTIVE_ENV_DEFAULTS from the
    // host environment so the fallback values are used.
    let _unset: Vec<EnvVar> = BashServer::NON_INTERACTIVE_ENV_DEFAULTS
        .iter()
        .map(|(k, _)| EnvVar::unset(k))
        .collect();

    let server = server_with_sandbox(vec![], enabled_sandbox_config());
    let child_env = server.build_child_env();

    let find = |key: &str, expected: &str| child_env.iter().any(|(k, v)| k == key && v == expected);

    for (key, fallback) in BashServer::NON_INTERACTIVE_ENV_DEFAULTS {
        assert!(
            find(key, fallback),
            "{key} fallback must be {fallback} when host env is unset"
        );
    }
}

/// Non-interactive safe defaults can be overridden by the host environment,
/// .env.bash, or env_overrides.
#[cfg(unix)]
#[test]
fn env_non_interactive_defaults_overridable() {
    let _env_guard = env_lock();

    // Host env wins over the fallback for vars it sets.
    let _host_pager = EnvVar::set("PAGER", "bat");
    let _host_no_color = EnvVar::unset("NO_COLOR"); // ensure unset so dotfile wins

    // .env.bash overrides NO_COLOR.
    let temp_dir = TestDir::new();
    let env_file_path = temp_dir.path().join(".env.bash");
    std::fs::write(&env_file_path, "NO_COLOR=0\n").unwrap();
    let _bash_env_file = EnvVar::set("HARNX_BASH_ENV_FILE", env_file_path.as_os_str());

    // env_overrides wins for GIT_PAGER.
    let mut cfg = enabled_sandbox_config();
    cfg.env_overrides = vec![("GIT_PAGER".to_string(), "delta".to_string())];
    let server = server_with_sandbox(vec![], cfg);
    let child_env = server.build_child_env();

    let find = |key: &str, expected: &str| child_env.iter().any(|(k, v)| k == key && v == expected);

    // host env beats fallback
    assert!(find("PAGER", "bat"), "host env must beat PAGER fallback");
    // .env.bash beats fallback
    assert!(
        find("NO_COLOR", "0"),
        ".env.bash must beat NO_COLOR fallback"
    );
    // env_overrides beats fallback
    assert!(
        find("GIT_PAGER", "delta"),
        "env_override must beat GIT_PAGER fallback"
    );
    // vars not overridden still have their fallbacks
    assert!(
        find("GIT_TERMINAL_PROMPT", "0"),
        "un-overridden GIT_TERMINAL_PROMPT must keep fallback"
    );
    assert!(
        find("CLICOLOR", "0"),
        "un-overridden CLICOLOR must keep fallback"
    );
}

#[cfg(unix)]
#[test]
fn env_precedence_cli_over_passthrough_over_dotfile() {
    let _env_guard = env_lock();

    // Set host value for the var so passthrough can pick it up.
    let _host_value = EnvVar::set("HARNX_TEST_PRECEDENCE_VAR", "from_host_passthrough");

    // Point dotfile at a tempdir whose .env.bash sets a different value.
    let temp_dir = TestDir::new();
    let env_file_path = temp_dir.path().join(".env.bash");
    std::fs::write(&env_file_path, "HARNX_TEST_PRECEDENCE_VAR=from_dotfile\n").unwrap();
    let _bash_env_file = EnvVar::set("HARNX_BASH_ENV_FILE", env_file_path.as_os_str());

    // Case 1: dotfile only (no passthrough, no override).
    // Expect dotfile value to win over (absent) default allowlist value.
    let dotfile_only = enabled_sandbox_config();
    let dotfile_server = server_with_sandbox(vec![], dotfile_only);
    let dotfile_env = dotfile_server.build_child_env();
    assert!(dotfile_env
        .iter()
        .any(|(k, v)| { k == "HARNX_TEST_PRECEDENCE_VAR" && v == "from_dotfile" }));

    // Case 2: dotfile + passthrough → passthrough beats dotfile.
    let mut passthrough_cfg = enabled_sandbox_config();
    passthrough_cfg.extra_env_passthrough = vec!["HARNX_TEST_PRECEDENCE_VAR".to_string()];
    let passthrough_server = server_with_sandbox(vec![], passthrough_cfg);
    let passthrough_env = passthrough_server.build_child_env();
    assert!(passthrough_env
        .iter()
        .any(|(k, v)| { k == "HARNX_TEST_PRECEDENCE_VAR" && v == "from_host_passthrough" }));

    // Case 3: dotfile + passthrough + override → override beats both.
    let mut override_cfg = enabled_sandbox_config();
    override_cfg.extra_env_passthrough = vec!["HARNX_TEST_PRECEDENCE_VAR".to_string()];
    override_cfg.env_overrides = vec![(
        "HARNX_TEST_PRECEDENCE_VAR".to_string(),
        "from_cli_override".to_string(),
    )];
    let override_server = server_with_sandbox(vec![], override_cfg);
    let override_env = override_server.build_child_env();
    assert!(override_env
        .iter()
        .any(|(k, v)| { k == "HARNX_TEST_PRECEDENCE_VAR" && v == "from_cli_override" }));
}

#[cfg(unix)]
#[test]
fn env_bash_dotfile_loaded() {
    let _env_guard = env_lock();
    let temp_dir = TestDir::new();
    let env_file_path = temp_dir.path().join(".env.bash");
    std::fs::write(
        &env_file_path,
        "# comment line\n\nHARNX_TEST_INJECT_4_3=s3cr3t\nHARNX_TEST_INJECT_KV_4_3=a=b\n",
    )
    .unwrap();
    let _bash_env_file = EnvVar::set("HARNX_BASH_ENV_FILE", env_file_path.as_os_str());

    let env_vars = load_bash_env_file();

    assert!(env_vars
        .iter()
        .any(|(key, value)| { key == "HARNX_TEST_INJECT_4_3" && value == "s3cr3t" }));
    assert!(env_vars
        .iter()
        .any(|(key, value)| { key == "HARNX_TEST_INJECT_KV_4_3" && value == "a=b" }));
    assert!(!env_vars.iter().any(|(key, _)| key == "# comment line"));
}

#[cfg(unix)]
#[cfg(unix)]
#[cfg(unix)]
#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_sandbox_exec_write_allowed() {
    if !sandbox_runtime_works() {
        return;
    }
    let root = TestDir::new();
    let server = sandboxed_server(vec![root.path().to_path_buf()]);

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "echo hi > out.txt".to_string(),
            working_dir: Some(root.path().to_string_lossy().to_string()),
            timeout_secs: Some(15),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    eprintln!(
        "sandbox write allowed output:
{text}"
    );
    assert_eq!(extract_field(&text, "exit_code"), "0");
    assert_eq!(
        std::fs::read_to_string(root.path().join("out.txt")).unwrap(),
        "hi
"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_sandbox_exec_write_denied_outside_root() {
    if !sandbox_runtime_works() {
        return;
    }
    let root = TestDir::new();
    let server = sandboxed_read_exec_server(root.path());

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "echo hi > out.txt".to_string(),
            working_dir: Some(root.path().to_string_lossy().to_string()),
            timeout_secs: Some(15),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    eprintln!(
        "sandbox write denied output:
{text}"
    );
    let exit_code = extract_field(&text, "exit_code").parse::<i32>().unwrap();
    let denied = exit_code != 0
        || text.contains("denied")
        || text.contains("Permission")
        || text.contains("permission");
    assert!(
        denied,
        "expected sandbox denial evidence, got:
{text}"
    );
    assert!(!root.path().join("out.txt").exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_working_dir_rejected_outside_allowlist() {
    let allowed = TestDir::new();
    let outside = TestDir::new();
    let server = server_with_paths(vec![allowed.path().to_path_buf()]);

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "pwd".to_string(),
            working_dir: Some(outside.path().to_string_lossy().to_string()),
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: None,
        })
        .await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .message
        .contains("outside allowed paths"));
}

#[tokio::test]
async fn test_exec_rejects_empty_command() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "   ".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: None,
        })
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_exec_rejects_invalid_env_keys() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    // Empty key
    let mut bad_env = std::collections::HashMap::new();
    bad_env.insert("".to_string(), "value".to_string());
    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "true".to_string(),
            working_dir: None,
            timeout_secs: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: Some(bad_env),
        })
        .await;
    assert!(result.is_err(), "empty key should be rejected");

    // Key with '='
    let mut bad_env2 = std::collections::HashMap::new();
    bad_env2.insert("FOO=BAR".to_string(), "value".to_string());
    let result2 = server
        .exec_command_impl(ExecCommandParams {
            command: "true".to_string(),
            working_dir: None,
            timeout_secs: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: Some(bad_env2),
        })
        .await;
    assert!(result2.is_err(), "key containing '=' should be rejected");

    // Key with NUL byte
    let mut bad_env3 = std::collections::HashMap::new();
    bad_env3.insert("FOO\0BAR".to_string(), "value".to_string());
    let result3 = server
        .exec_command_impl(ExecCommandParams {
            command: "true".to_string(),
            working_dir: None,
            timeout_secs: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: Some(bad_env3),
        })
        .await;
    assert!(result3.is_err(), "key containing NUL should be rejected");
}

#[tokio::test]
async fn test_exec_nonzero_exit_code() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .exec_command_impl(ExecCommandParams {
            timeout_secs: Some(5),
            ..exec_params("echo boom >&2; exit 1", temp_dir.path())
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert_eq!(result.is_error, Some(false));
    assert!(text.contains("exit_code: 1"));
}

#[tokio::test]
async fn test_exec_basic_command() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "echo test".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert_eq!(result.is_error, Some(false));
    assert!(text.contains("status: exited"));
    assert!(text.contains("exit_code: 0"));
    assert!(text.contains("command: echo test"));
    assert!(text.contains("working_dir:"));
    assert!(text.contains("test"));

    // Verify canonical field order: execution_id < status < exit_code < command < working_dir
    let pos_execution_id = text.find("execution_id:").unwrap();
    let pos_status = text.find("status:").unwrap();
    let pos_exit_code = text.find("exit_code:").unwrap();
    let pos_command = text.find("command:").unwrap();
    let pos_working_dir = text.find("working_dir:").unwrap();
    let pos_stdout_log = text.find("stdout_log_path:").unwrap();
    let pos_stderr_log = text.find("stderr_log_path:").unwrap();
    assert!(pos_execution_id < pos_status);
    assert!(pos_status < pos_exit_code);
    assert!(pos_exit_code < pos_command);
    assert!(pos_command < pos_working_dir);
    assert!(pos_working_dir < pos_stdout_log);
    assert!(pos_stdout_log < pos_stderr_log);

    let stdout_log_path = PathBuf::from(extract_field(&text, "stdout_log_path"));
    let stderr_log_path = PathBuf::from(extract_field(&text, "stderr_log_path"));
    assert!(stdout_log_path.exists());
    assert!(stderr_log_path.exists());
}

#[tokio::test]
async fn test_exec_per_call_env_vars() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let mut extra_env = std::collections::HashMap::new();
    extra_env.insert("MY_TEST_VAR".to_string(), "hello_from_env".to_string());

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "echo $MY_TEST_VAR".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: Some(extra_env),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(
        text.contains("hello_from_env"),
        "env var should be visible to command: {text}"
    );
}

#[tokio::test]
async fn test_spawn_per_call_env_vars() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let mut extra_env = std::collections::HashMap::new();
    extra_env.insert("MY_SPAWN_VAR".to_string(), "spawned_value".to_string());

    let result = server
        .spawn_impl(SpawnCommandParams {
            command: "echo $MY_SPAWN_VAR".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            env: Some(extra_env),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    let execution_id = extract_field(&text, "execution_id");

    let wait_result = server
        .wait_impl(WaitParams {
            execution_id,
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            grep: None,
        })
        .await
        .unwrap();

    let wait_text = text_content(&wait_result);
    assert!(
        wait_text.contains("spawned_value"),
        "env var should be visible to spawned command: {wait_text}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_sandbox_exec_per_call_env_vars() {
    if !sandbox_runtime_works() {
        return;
    }
    let temp_dir = TestDir::new();
    let server = sandboxed_server(vec![temp_dir.path().to_path_buf()]);

    let mut extra_env = std::collections::HashMap::new();
    extra_env.insert(
        "MY_SANDBOX_VAR".to_string(),
        "sandbox_exec_value".to_string(),
    );

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "echo $MY_SANDBOX_VAR".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(15),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: Some(extra_env),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(
        text.contains("sandbox_exec_value"),
        "env var should be visible to sandboxed command: {text}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_sandbox_spawn_per_call_env_vars() {
    if !sandbox_runtime_works() {
        return;
    }
    let temp_dir = TestDir::new();
    let server = sandboxed_server(vec![temp_dir.path().to_path_buf()]);

    let mut extra_env = std::collections::HashMap::new();
    extra_env.insert(
        "MY_SANDBOX_SPAWN_VAR".to_string(),
        "sandbox_spawn_value".to_string(),
    );

    let result = server
        .spawn_impl(SpawnCommandParams {
            command: "echo $MY_SANDBOX_SPAWN_VAR".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            env: Some(extra_env),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    let execution_id = extract_field(&text, "execution_id");

    let wait_result = server
        .wait_impl(WaitParams {
            execution_id,
            timeout_secs: Some(15),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            grep: None,
        })
        .await
        .unwrap();

    let wait_text = text_content(&wait_result);
    assert!(
        wait_text.contains("sandbox_spawn_value"),
        "env var should be visible to sandboxed spawned command: {wait_text}"
    );
}

#[cfg(unix)]
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn env_secret_not_leaked_to_child() {
    let _env_guard = env_lock();
    let _secret = EnvVar::set("AWS_SECRET_ACCESS_KEY", "hunter2_4_4");
    let _config_dir = EnvVar::unset("HARNX_CONFIG_DIR");
    let root = TestDir::new();
    let server = server_with_sandbox(vec![root.path().to_path_buf()], disabled_sandbox_config());

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "echo ${AWS_SECRET_ACCESS_KEY:-empty}".to_string(),
            working_dir: Some(root.path().to_string_lossy().to_string()),
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert_eq!(result.is_error, Some(false));
    assert!(text.contains("exit_code: 0"));
    assert!(text.contains("empty"), "unexpected exec output: {text}");
    assert!(
        !text.contains("hunter2_4_4"),
        "secret leaked into child output: {text}"
    );
}

#[tokio::test]
async fn test_exec_timeout() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "sleep 10".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(1),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert_eq!(result.is_error, Some(true));
    assert!(text.contains("command timed out after 1s and was terminated"));
    assert!(text.contains("execution_id:"));
    assert!(text.contains("status: timeout"));
    assert!(text.contains("command: sleep 10"));
    assert!(text.contains("working_dir:"));
    assert!(text.contains("stdout_log_path:"));
    assert!(text.contains("stderr_log_path:"));
}

#[tokio::test]
async fn test_exec_truncation_mentions_log_paths_and_read_exec_log_works() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "printf 'out1\nout2\nout3\n'; printf 'err1\nerr2\n' >&2".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(5),
            head_lines: Some(1),
            tail_lines: Some(1),
            max_output_bytes: Some(16),
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    let execution_id = extract_field(&text, "execution_id");
    assert!(text.contains("full log via read_exec_log"));
    assert!(text.contains(&execution_id));

    let stdout_read = server
        .read_exec_log_impl(ReadExecLogParams {
            execution_id: execution_id.clone(),
            stream: "stdout".to_string(),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();
    let stdout_text = text_content(&stdout_read);
    assert!(stdout_text.contains("1: out1"));
    assert!(stdout_text.contains("2: out2"));
    assert!(stdout_text.contains("3: out3"));

    let stderr_read = server
        .read_exec_log_impl(ReadExecLogParams {
            execution_id,
            stream: "stderr".to_string(),
            offset: None,
            limit: None,
            tail: Some(1),
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();
    let stderr_text = text_content(&stderr_read);
    assert!(stderr_text.contains("2: err2"));
    assert!(stderr_text.contains("showing last 1 of 2 matching lines"));
}

#[tokio::test]
async fn test_read_exec_log_rejects_invalid_stream() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .read_exec_log_impl(ReadExecLogParams {
            execution_id: "exec-test".to_string(),
            stream: "invalid".to_string(),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .message
        .contains("stream must be 'stdout' or 'stderr'"));
}

#[tokio::test]
async fn test_read_exec_log_offset_and_tail_combined() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "printf 'l1\\nl2\\nl3\\nl4\\nl5\\nl6\\n'".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: None,
        })
        .await
        .unwrap();

    let execution_id = extract_field(&text_content(&result), "execution_id");

    // Skip to line 3, then tail the last 2 lines of the remaining window
    // (lines 3..6) → lines 5 and 6.
    let read = server
        .read_exec_log_impl(ReadExecLogParams {
            execution_id,
            stream: "stdout".to_string(),
            offset: Some(3),
            limit: None,
            tail: Some(2),
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
        .await
        .unwrap();

    let text = text_content(&read);
    assert!(text.contains("5: l5"), "got: {text}");
    assert!(text.contains("6: l6"), "got: {text}");
    assert!(!text.contains("4: l4"), "got: {text}");
    // Window after offset is 4 lines (3..6), tail 2 of those.
    assert!(
        text.contains("showing last 2 of 4 matching lines"),
        "got: {text}"
    );
    // Tail is anchored to the end, so no "more matching lines" pagination notice.
    assert!(!text.contains("more matching lines"), "got: {text}");
}

#[test]
fn test_select_log_lines_offset_and_tail() {
    let lines: Vec<(usize, String)> = (1..=6).map(|n| (n, format!("line{n}"))).collect();

    // offset=3 (skip 2), tail=2 → last 2 of window [3..6] = lines 5,6.
    let (selected, notices) = BashServer::select_log_lines(lines.clone(), Some(3), 200, Some(2));
    assert_eq!(
        selected,
        vec![(5, "line5".to_string()), (6, "line6".to_string())]
    );
    assert!(notices
        .iter()
        .any(|n| n.contains("showing last 2 of 4 matching lines")));

    // tail larger than the post-offset window returns the whole window.
    let (selected, notices) = BashServer::select_log_lines(lines, Some(5), 200, Some(10));
    assert_eq!(
        selected,
        vec![(5, "line5".to_string()), (6, "line6".to_string())]
    );
    assert!(notices.is_empty());
}

#[tokio::test]
async fn test_cleanup_log_dir_removes_temp_logs() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "echo cleanup".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    let stdout_log_path = PathBuf::from(extract_field(&text, "stdout_log_path"));
    let log_dir = stdout_log_path.parent().unwrap().to_path_buf();
    assert!(log_dir.exists());

    server.cleanup_log_dir().unwrap();
    assert!(!log_dir.exists());
}

#[tokio::test]
async fn test_spawn_and_wait() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .spawn_impl(SpawnCommandParams {
            command: "echo background && sleep 1".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    // Verify spawn metadata
    assert!(text.contains("status: spawned"));
    assert!(text.contains("command: echo background && sleep 1"));
    assert!(text.contains("working_dir:"));
    assert!(text.contains("stdout_log_path:"));
    assert!(text.contains("stderr_log_path:"));
    let execution_id = extract_field(&text, "execution_id");

    let result = server
        .wait_impl(WaitParams {
            execution_id,
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: Some(10),
            max_output_bytes: None,
            grep: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("status: exited"));
    assert!(text.contains("command: echo background && sleep 1"));
    assert!(text.contains("working_dir:"));
    assert!(text.contains("background"));
}

#[tokio::test]
async fn test_spawn_wait_timeout() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .spawn_impl(SpawnCommandParams {
            command: "sleep 5".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    let execution_id = extract_field(&text, "execution_id");

    let result = server
        .wait_impl(WaitParams {
            timeout_secs: Some(1),
            tail_lines: Some(10),
            ..wait_params(execution_id)
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("status: running"));
}

#[tokio::test]
async fn test_spawn_and_terminate() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .spawn_impl(spawn_params("sleep 30", temp_dir.path()))
        .await
        .unwrap();

    let text = text_content(&result);
    let execution_id = extract_field(&text, "execution_id");

    let result = server
        .terminate_impl(TerminateParams {
            execution_id,
            signal: Some("SIGTERM".to_string()),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert_execution_metadata_fields(&text);
    assert_text_contains_all(
        &text,
        &["status: terminated", "command: sleep 30", "signal: SIGTERM"],
    );
    // The YAML metadata fence must close on its own line, followed by a blank
    // line, then the `signal:` line as plain text. Guards against the closing
    // fence and signal text being mashed onto the same line (e.g. "```signal:").
    assert!(
        text.contains("```\n\nsignal: SIGTERM"),
        "terminate output must separate the closing yaml fence from the signal line: {text:?}"
    );
    assert!(
        !text.contains("```signal:"),
        "signal line must not be mashed onto the closing yaml fence: {text:?}"
    );
}

#[tokio::test]
async fn test_wait_unknown_execution_id() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .wait_impl(WaitParams {
            execution_id: "exec-does-not-exist".to_string(),
            timeout_secs: Some(1),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            grep: None,
        })
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_terminate_unknown_execution_id() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .terminate_impl(TerminateParams {
            execution_id: "exec-does-not-exist".to_string(),
            signal: None,
        })
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn test_spawn_with_output() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .spawn_impl(SpawnCommandParams {
            command: "for i in 1 2 3; do echo line$i; done".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    let execution_id = extract_field(&text, "execution_id");

    let result = server
        .wait_impl(WaitParams {
            execution_id,
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: Some(10),
            max_output_bytes: None,
            grep: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(text.contains("exit_code: 0"));
    assert!(text.contains("line1"));
    assert!(text.contains("line2"));
    assert!(text.contains("line3"));
}

#[tokio::test]
async fn test_exec_env_special_chars_and_override() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let mut extra_env = std::collections::HashMap::new();
    extra_env.insert("VAR_WITH_EQUALS".to_string(), "key=value=more".to_string());
    extra_env.insert("VAR_WITH_NEWLINE".to_string(), "line1\nline2".to_string());
    extra_env.insert("PAGER".to_string(), "custom_pager".to_string());

    let command = r#"echo $VAR_WITH_EQUALS; echo "$VAR_WITH_NEWLINE"; echo $PAGER"#;

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: command.to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: Some(extra_env),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(
        text.contains("key=value=more"),
        "Should handle multiple equals: {text}"
    );
    assert!(
        text.contains("line1\nline2"),
        "Should handle newlines: {text}"
    );
    assert!(
        text.contains("custom_pager"),
        "Should override base env: {text}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_sandbox_exec_env_special_chars_and_override() {
    if !sandbox_runtime_works() {
        return;
    }
    let temp_dir = TestDir::new();
    let server = sandboxed_server(vec![temp_dir.path().to_path_buf()]);

    let mut extra_env = std::collections::HashMap::new();
    extra_env.insert("VAR_WITH_EQUALS".to_string(), "key=value=more".to_string());
    extra_env.insert("VAR_WITH_NEWLINE".to_string(), "line1\nline2".to_string());
    extra_env.insert("PAGER".to_string(), "custom_pager".to_string());

    let command = r#"echo $VAR_WITH_EQUALS; echo "$VAR_WITH_NEWLINE"; echo $PAGER"#;

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: command.to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(15),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: Some(extra_env),
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(
        text.contains("key=value=more"),
        "Should handle multiple equals: {text}"
    );
    assert!(
        text.contains("line1\nline2"),
        "Should handle newlines: {text}"
    );
    assert!(
        text.contains("custom_pager"),
        "Should override base env: {text}"
    );
}
// ── Tests for is_home_or_ancestor and build_sandbox_args HOME filtering ──

mod home_filtering {
    use super::*;

    #[test]
    fn home_rwx_is_downgraded_to_read_only() {
        let _env_guard = env_lock();
        let home = tempfile::tempdir().expect("home");
        let _home = EnvVar::set("HOME", home.path());
        let server = server_with_sandbox(vec![home.path().to_path_buf()], enabled_sandbox_config());
        let pairs = collect_arg_pairs(&server.build_sandbox_args(home.path()));
        // Grants are emitted as written, not resolved. On macOS a temp dir sits
        // under /var, a symlink to /private/var, so canonicalising here would
        // expect a path the allowlist deliberately no longer produces.
        let home = home.path().to_string_lossy().into_owned();

        assert!(pairs.contains(&("--read".into(), home.clone())));
        assert!(!pairs.contains(&("--write".into(), home.clone())));
        assert!(!pairs.contains(&("--exec".into(), home)));
    }

    #[test]
    fn home_subdirectory_keeps_rwx() {
        let _env_guard = env_lock();
        let home = tempfile::tempdir().expect("home");
        let project = home.path().join("project");
        std::fs::create_dir(&project).expect("project");
        let _home = EnvVar::set("HOME", home.path());
        let server = server_with_sandbox(vec![project.clone()], enabled_sandbox_config());
        let pairs = collect_arg_pairs(&server.build_sandbox_args(&project));
        // Emitted as written; see the note in home_rwx_is_downgraded_to_read_only.
        let project = project.to_string_lossy().into_owned();

        assert!(pairs.contains(&("--write".into(), project.clone())));
        assert!(pairs.contains(&("--exec".into(), project)));
    }
}
fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn test_exec_python_shebang() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "#!/usr/bin/env python3\nprint(\"hello from python\")".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(10),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(
        text.contains("hello from python"),
        "expected python output, got: {text:?}"
    );
    assert!(text.contains("exit_code: 0"), "expected success: {text:?}");
}

#[tokio::test]
async fn test_exec_node_shebang() {
    if !node_available() {
        eprintln!("skipping: node not available");
        return;
    }
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "#!/usr/bin/env node\nconsole.log(\"hello from node\")".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(10),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(
        text.contains("hello from node"),
        "expected node output, got: {text:?}"
    );
    assert!(text.contains("exit_code: 0"), "expected success: {text:?}");
}

#[tokio::test]
async fn test_spawn_python_shebang() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let spawn_result = server
        .spawn_impl(SpawnCommandParams {
            command: "#!/usr/bin/env python3\nprint(\"hello from spawn python\")".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            env: None,
        })
        .await
        .unwrap();

    let spawn_text = text_content(&spawn_result);
    assert!(
        spawn_text.contains("status: spawned"),
        "expected spawned status: {spawn_text:?}"
    );
    let execution_id = extract_field(&spawn_text, "execution_id");

    let wait_result = server
        .wait_impl(WaitParams {
            execution_id,
            timeout_secs: Some(10),
            head_lines: None,
            tail_lines: Some(20),
            max_output_bytes: None,
            grep: None,
        })
        .await
        .unwrap();

    let wait_text = text_content(&wait_result);
    assert!(
        wait_text.contains("hello from spawn python"),
        "expected python output in wait, got: {wait_text:?}"
    );
    assert!(
        wait_text.contains("exit_code: 0"),
        "expected success: {wait_text:?}"
    );
}

mod shebangs {
    use super::*;

    #[test]
    fn test_parse_shebang_none_for_plain_command() {
        assert_eq!(parse_shebang("echo hello"), None);
    }

    #[test]
    fn test_parse_shebang_env_python3() {
        assert_eq!(
            parse_shebang("#!/usr/bin/env python3\nprint(\"hi\")"),
            Some((PathBuf::from("python3"), vec![]))
        );
    }

    #[test]
    fn test_parse_shebang_direct_path() {
        assert_eq!(
            parse_shebang("#!/usr/bin/python3\nprint(\"hi\")"),
            Some((PathBuf::from("/usr/bin/python3"), vec![]))
        );
    }

    #[test]
    fn test_parse_shebang_with_args() {
        assert_eq!(
            parse_shebang("#!/usr/bin/python3 -u\nprint(\"hi\")"),
            Some((PathBuf::from("/usr/bin/python3"), vec!["-u".to_string()]))
        );
    }

    #[test]
    fn test_parse_shebang_node() {
        assert_eq!(
            parse_shebang("#!/usr/bin/env node\nconsole.log(\"hi\")"),
            Some((PathBuf::from("node"), vec![]))
        );
    }

    #[test]
    fn test_shebang_script_ext_values() {
        assert_eq!(shebang_script_ext("#!/usr/bin/env python3\n"), "py");
        assert_eq!(shebang_script_ext("#!/usr/bin/env node\n"), "js");
        assert_eq!(shebang_script_ext("#!/usr/bin/env ruby\n"), "rb");
        assert_eq!(shebang_script_ext("#!/usr/bin/env perl\n"), "pl");
        assert_eq!(shebang_script_ext("#!/usr/bin/env deno\n"), "ts");
        assert_eq!(shebang_script_ext("#!/usr/bin/env php\n"), "php");
        assert_eq!(shebang_script_ext("echo hello"), "sh");
    }

    #[test]
    fn test_parse_shebang_env_s_flag() {
        // #!/usr/bin/env -S python3 -u should skip -S and use python3 as interpreter
        assert_eq!(
            parse_shebang("#!/usr/bin/env -S python3 -u\nprint(\"hi\")"),
            Some((PathBuf::from("python3"), vec!["-u".to_string()]))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_absolute_shebang_interpreter_is_not_auto_granted() {
        let temp_dir = TestDir::new();
        let root = temp_dir.path().to_path_buf();
        let server = sandbox_server(root.clone());
        let command = server
            .build_sandbox_command(
                SandboxCommandSpec {
                    working_dir: &root,
                    exec_dir: &root,
                    command: "#!/opt/notallowed/x\nexit 0",
                    extra_env: None,
                    read_paths: Vec::new(),
                    write_paths: Vec::new(),
                    pass_env: Vec::new(),
                    no_network: false,
                },
                Stdio::null(),
                Stdio::null(),
            )
            .await
            .expect("build sandbox command");
        let args: Vec<_> = command
            .command()
            .as_std()
            .get_args()
            .map(OsString::from)
            .collect();
        let unexpected = [OsString::from("--exec"), OsString::from("/opt/notallowed")];

        assert!(
            !args.windows(2).any(|pair| pair == unexpected),
            "absolute shebang interpreter directory was auto-granted: {args:?}"
        );
        let separator = args
            .iter()
            .position(|arg| arg == "--")
            .expect("sandbox command separator");
        assert_eq!(
            args.get(separator + 1),
            Some(&OsString::from("/opt/notallowed/x"))
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_sandbox_exec_python_shebang() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let temp_dir = TestDir::new();
    let server = sandboxed_server(vec![temp_dir.path().to_path_buf()]);

    if !sandbox_runtime_works() {
        eprintln!("skipping: sandbox runtime not available");
        return;
    }

    let result = server
        .exec_command_impl(ExecCommandParams {
            command: "#!/usr/bin/env python3\nprint(\"hello from sandboxed python\")".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            timeout_secs: Some(10),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            env: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(
        text.contains("hello from sandboxed python"),
        "expected python output in sandbox, got: {text:?}"
    );
    assert!(
        text.contains("exit_code: 0"),
        "expected success in sandbox: {text:?}"
    );
}

// --- Issue #365: wait grep/truncation params + concurrent isolation ---

/// `wait` with `grep` filters each stream independently: stdout keeps only
/// matching lines, stderr keeps only its matching lines.
#[tokio::test]
async fn test_wait_grep_filters_per_stream() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .spawn_impl(SpawnCommandParams {
            command: "printf 'keep-out\\ndrop-out\\n'; printf 'keep-err\\ndrop-err\\n' >&2"
                .to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            env: None,
        })
        .await
        .unwrap();
    let execution_id = extract_field(&text_content(&result), "execution_id");

    let result = server
        .wait_impl(WaitParams {
            execution_id,
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            grep: Some("keep".to_string()),
        })
        .await
        .unwrap();

    // Inspect only the rendered stream blocks; the metadata header echoes
    // the literal command (which contains "drop-*"), so assert against the
    // stdout/stderr block bodies, not the full text.
    let text = text_content(&result);
    let blocks = &text[text.find("<!-- start stdout -->").expect("stdout marker")..];
    assert!(blocks.contains("keep-out"), "stdout match kept: {text}");
    assert!(blocks.contains("keep-err"), "stderr match kept: {text}");
    assert!(
        !blocks.contains("drop-out"),
        "stdout non-match removed: {text}"
    );
    assert!(
        !blocks.contains("drop-err"),
        "stderr non-match removed: {text}"
    );
}

/// `wait` truncation: a large output with `head_lines`/`tail_lines`/
/// `max_output_bytes` triggers actual truncation and a truncation hint.
#[tokio::test]
async fn test_wait_truncation_triggers_and_mentions_log_path() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .spawn_impl(SpawnCommandParams {
            command: "for i in $(seq 1 200); do echo \"stdout-line-$i\"; done".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            env: None,
        })
        .await
        .unwrap();
    let execution_id = extract_field(&text_content(&result), "execution_id");

    let result = server
        .wait_impl(WaitParams {
            execution_id: execution_id.clone(),
            timeout_secs: Some(5),
            head_lines: Some(2),
            tail_lines: Some(2),
            max_output_bytes: None,
            grep: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    // Truncation happened: hint present and points back at read_exec_log.
    assert!(
        text.contains("stdout truncated from"),
        "expected truncation hint: {text}"
    );
    assert!(text.contains("full log via read_exec_log"));
    assert!(text.contains(&execution_id));
    // Head/tail boundaries retained; middle dropped.
    assert!(text.contains("stdout-line-1"), "head kept: {text}");
    assert!(text.contains("stdout-line-200"), "tail kept: {text}");
    assert!(!text.contains("stdout-line-100"), "middle dropped: {text}");
}

/// `wait` with `max_output_bytes` truncates the stream to the byte budget.
#[tokio::test]
async fn test_wait_max_output_bytes_truncates() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .spawn_impl(SpawnCommandParams {
            command: "for i in $(seq 1 200); do echo \"line-$i\"; done".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            env: None,
        })
        .await
        .unwrap();
    let execution_id = extract_field(&text_content(&result), "execution_id");

    let result = server
        .wait_impl(WaitParams {
            execution_id,
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: Some(32),
            grep: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(
        text.contains("stdout truncated from"),
        "expected byte truncation hint: {text}"
    );
}

/// Stream markers appear in `wait` output: opening/closing HTML-comment markers
/// for streams. Empty streams emit start/end markers with an empty fenced code block.
#[tokio::test]
async fn test_wait_stream_markers() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    // stdout has content, stderr is empty.
    let result = server
        .spawn_impl(SpawnCommandParams {
            command: "echo only-stdout".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            env: None,
        })
        .await
        .unwrap();
    let execution_id = extract_field(&text_content(&result), "execution_id");

    let result = server
        .wait_impl(WaitParams {
            execution_id,
            timeout_secs: Some(5),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            grep: None,
        })
        .await
        .unwrap();

    let text = text_content(&result);
    assert!(
        text.contains("<!-- start stdout -->"),
        "stdout open marker: {text}"
    );
    assert!(
        text.contains("<!-- end stdout -->"),
        "stdout close marker: {text}"
    );
    assert!(text.contains("only-stdout"));
    assert!(
        text.contains("<!-- start stderr -->"),
        "empty stderr start marker: {text}"
    );
    assert!(
        text.contains("<!-- end stderr -->"),
        "empty stderr end marker: {text}"
    );
}

/// Concurrent executions get separate UUID log directories: two processes
/// spawned simultaneously must not collide, and each `read_exec_log`
/// returns only that execution's output.
#[tokio::test]
async fn test_concurrent_execution_log_isolation() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);
    let wd = temp_dir.path().to_string_lossy().to_string();

    let (res_a, res_b) = tokio::join!(
        server.spawn_impl(SpawnCommandParams {
            command: "echo alpha-out; echo alpha-err >&2; sleep 0.3".to_string(),
            working_dir: Some(wd.clone()),
            env: None,
        }),
        server.spawn_impl(SpawnCommandParams {
            command: "echo bravo-out; echo bravo-err >&2; sleep 0.3".to_string(),
            working_dir: Some(wd.clone()),
            env: None,
        }),
    );

    let text_a = text_content(&res_a.unwrap());
    let text_b = text_content(&res_b.unwrap());
    let id_a = extract_field(&text_a, "execution_id");
    let id_b = extract_field(&text_b, "execution_id");
    assert_ne!(id_a, id_b, "execution ids must differ");

    let log_a = PathBuf::from(extract_field(&text_a, "stdout_log_path"));
    let log_b = PathBuf::from(extract_field(&text_b, "stdout_log_path"));
    assert_ne!(
        log_a.parent(),
        log_b.parent(),
        "concurrent executions must use separate log dirs: {log_a:?} vs {log_b:?}"
    );

    // Drain both before inspecting logs.
    for id in [&id_a, &id_b] {
        server
            .wait_impl(WaitParams {
                execution_id: id.clone(),
                timeout_secs: Some(5),
                head_lines: None,
                tail_lines: None,
                max_output_bytes: None,
                grep: None,
            })
            .await
            .unwrap();
    }

    let read = |id: String, stream: &str| {
        server.read_exec_log_impl(ReadExecLogParams {
            execution_id: id,
            stream: stream.to_string(),
            offset: None,
            limit: None,
            tail: None,
            grep: None,
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
        })
    };

    let a_out = text_content(&read(id_a.clone(), "stdout").await.unwrap());
    let a_err = text_content(&read(id_a.clone(), "stderr").await.unwrap());
    let b_out = text_content(&read(id_b.clone(), "stdout").await.unwrap());
    let b_err = text_content(&read(id_b.clone(), "stderr").await.unwrap());

    assert!(a_out.contains("alpha-out") && !a_out.contains("bravo-out"));
    assert!(a_err.contains("alpha-err") && !a_err.contains("bravo-err"));
    assert!(b_out.contains("bravo-out") && !b_out.contains("alpha-out"));
    assert!(b_err.contains("bravo-err") && !b_err.contains("alpha-err"));
}

/// Parallel to the existing exec -> read_exec_log truncation test: spawn ->
/// wait (truncated) -> read_exec_log returns the full, untruncated log.
#[tokio::test]
async fn test_spawn_wait_read_exec_log_returns_full_output() {
    let temp_dir = TestDir::new();
    let server = server_with_paths(vec![temp_dir.path().to_path_buf()]);

    let result = server
        .spawn_impl(SpawnCommandParams {
            command: "printf 'out1\\nout2\\nout3\\n'; printf 'err1\\nerr2\\n' >&2".to_string(),
            working_dir: Some(temp_dir.path().to_string_lossy().to_string()),
            env: None,
        })
        .await
        .unwrap();
    let execution_id = extract_field(&text_content(&result), "execution_id");

    // Truncated wait: only boundaries visible.
    let result = server
        .wait_impl(WaitParams {
            execution_id: execution_id.clone(),
            timeout_secs: Some(5),
            head_lines: Some(1),
            tail_lines: Some(1),
            max_output_bytes: None,
            grep: None,
        })
        .await
        .unwrap();
    // Scope to the stdout block: the metadata header echoes the literal
    // command, which contains "out2".
    let wait_text = text_content(&result);
    let wait_blocks = &wait_text[wait_text
        .find("<!-- start stdout -->")
        .expect("stdout marker")..];
    assert!(wait_blocks.contains("out1"));
    assert!(
        !wait_blocks.contains("out2"),
        "middle truncated in wait: {wait_text}"
    );

    // read_exec_log returns the complete stdout log.
    let stdout_read = server
        .read_exec_log_impl(read_exec_log_params(execution_id.clone(), "stdout"))
        .await
        .unwrap();
    let stdout_text = text_content(&stdout_read);
    assert!(stdout_text.contains("1: out1"));
    assert!(stdout_text.contains("2: out2"));
    assert!(stdout_text.contains("3: out3"));

    let stderr_read = server
        .read_exec_log_impl(read_exec_log_params(execution_id, "stderr"))
        .await
        .unwrap();
    let stderr_text = text_content(&stderr_read);
    assert!(stderr_text.contains("1: err1"));
    assert!(stderr_text.contains("2: err2"));
}
