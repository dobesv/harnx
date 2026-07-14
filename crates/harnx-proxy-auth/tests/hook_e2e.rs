//! End-to-end tests for the harnx-proxy-auth persistent hook.
//!
//! Tests that the hook correctly injects `HTTPS_PROXY`, `SSL_CERT_FILE`, and
//! friends into `bash_exec` / `bash_spawn` tool input when wired up through
//! the real `PersistentHookManager` → `dispatch_hooks` → `eval_tool_calls`
//! stack.
//!
//! Both tests skip gracefully if the required binaries haven't been built yet.

// These tests spawn `harnx-proxy-auth` as a subprocess and use Unix-style
// single-quoted shell escaping. They require a Unix shell and are skipped on
// Windows where the binary also has known build issues.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use harnx_core::abort::create_abort_signal;
use harnx_core::hooks::{HookConfig, HookEvent, HookOutcome, HookResultControl, HooksConfig};
use harnx_core::tool::ToolCall;
use harnx_core::working_mode::WorkingMode;
use harnx_engine::tool::eval_tool_calls;
use harnx_hooks::{dispatch_hooks_with_count_and_manager, PersistentHookManager};
use harnx_mcp::McpServerConfig;
use harnx_runtime::config::{Config, GlobalConfig};
use harnx_runtime::tool::build_tool_eval_context;
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::Mutex;

// ── Test 1: hook dispatch layer ────────────────────────────────────────────

/// Verify that the persistent hook mutates `tool_input` for `bash_exec` by
/// adding the proxy env vars. Uses `dispatch_hooks_with_count_and_manager`
/// directly — does NOT require `harnx-mcp-bash`.
#[tokio::test(flavor = "multi_thread")]
async fn hook_dispatch_injects_proxy_env_into_bash_exec_input() {
    let Some(proxy_bin) = proxy_binary_path() else {
        eprintln!("SKIP hook_dispatch_injects_proxy_env: harnx-proxy-auth binary not found");
        return;
    };

    let outcome = dispatch_one_hook(
        &proxy_bin,
        HookEvent::PreToolUse {
            tool_name: "bash_exec".to_string(),
            tool_input: json!({ "command": "echo hello" }),
            tool_use_id: "toolu_hook_e2e_1".to_string(),
        },
    )
    .await;

    assert!(
        matches!(outcome.control, HookResultControl::Continue),
        "hook should continue, not block"
    );

    let mutated = outcome
        .result
        .mutated_tool_input
        .expect("persistent hook must mutate tool_input for bash_exec");
    let env = mutated
        .get("env")
        .and_then(Value::as_object)
        .expect("mutated tool_input must contain an 'env' object");

    let https_proxy = env
        .get("HTTPS_PROXY")
        .and_then(Value::as_str)
        .expect("HTTPS_PROXY must be injected");
    assert!(
        https_proxy.starts_with("http://127.0.0.1:"),
        "HTTPS_PROXY should point to localhost proxy, got: {https_proxy}"
    );

    let ssl_cert_file = env
        .get("SSL_CERT_FILE")
        .and_then(Value::as_str)
        .expect("SSL_CERT_FILE must be injected");
    assert!(
        ssl_cert_file.ends_with("ca.pem"),
        "SSL_CERT_FILE should point to CA cert, got: {ssl_cert_file}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hook_dispatch_does_not_mutate_non_bash_tools() {
    let Some(proxy_bin) = proxy_binary_path() else {
        eprintln!("SKIP hook_dispatch_does_not_mutate_non_bash_tools: harnx-proxy-auth not found");
        return;
    };

    let outcome = dispatch_one_hook(
        &proxy_bin,
        HookEvent::PreToolUse {
            tool_name: "read_file".to_string(),
            tool_input: json!({ "path": "/tmp/test" }),
            tool_use_id: "toolu_hook_e2e_2".to_string(),
        },
    )
    .await;

    assert!(
        outcome.result.mutated_tool_input.is_none(),
        "non-bash tool must not be mutated"
    );
}

/// Verify that `--env` sentinel variables are injected into bash tool call env maps.
///
/// This test uses the `--env` flag to pass a custom environment variable template
/// with a sentinel value, and verifies that the resulting env map contains the
/// resolved sentinel variable.
#[tokio::test(flavor = "multi_thread")]
async fn hook_env_sentinel_is_injected_into_bash_tool_input() {
    let Some(proxy_bin) = proxy_binary_path() else {
        eprintln!(
            "SKIP hook_env_sentinel_is_injected_into_bash_tool_input: harnx-proxy-auth not found"
        );
        return;
    };

    // The --env arg contains JSON with a sentinel variable using jaq string interpolation.
    // The jaq `\($fake_base64_key)` is literal inside the shell single-quoted string.
    let env_arg = r#"{"FAKE_TOKEN": "ghs_\($fake_base64_key)"}"#;

    let outcome = dispatch_one_hook_with_env(
        &proxy_bin,
        Some(env_arg),
        HookEvent::PreToolUse {
            tool_name: "bash_exec".to_string(),
            tool_input: json!({ "command": "echo hi" }),
            tool_use_id: "toolu_hook_e2e_env".to_string(),
        },
    )
    .await;

    assert!(
        matches!(outcome.control, HookResultControl::Continue),
        "hook should continue, not block"
    );

    let mutated = outcome
        .result
        .mutated_tool_input
        .expect("persistent hook must mutate tool_input for bash_exec");
    let env = mutated
        .get("env")
        .and_then(Value::as_object)
        .expect("mutated tool_input must contain an 'env' object");

    let fake_token = env
        .get("FAKE_TOKEN")
        .and_then(Value::as_str)
        .expect("FAKE_TOKEN must be injected");

    assert!(
        fake_token.starts_with("ghs_"),
        "FAKE_TOKEN should start with sentinel prefix 'ghs_', got: {fake_token}"
    );
    // Verify jaq interpolation actually resolved: no raw template markers should remain.
    assert!(
        !fake_token.contains("($") && !fake_token.contains('$'),
        "FAKE_TOKEN contains unresolved jaq template marker, interpolation failed: {fake_token}"
    );
    // The suffix should be base64 characters only (the fake_base64_key encoding).
    let suffix = fake_token.trim_start_matches("ghs_");
    assert!(
        !suffix.is_empty()
            && suffix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
        "FAKE_TOKEN suffix is not valid base64, got: {suffix}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hook_env_proxy_port_is_injected_into_bash_tool_input() {
    let Some(proxy_bin) = proxy_binary_path() else {
        eprintln!(
            "SKIP hook_env_proxy_port_is_injected_into_bash_tool_input: harnx-proxy-auth not found"
        );
        return;
    };

    let env_arg = r#"{"GCE_METADATA_HOST": "127.0.0.1:\($proxy_port)"}"#;
    let outcome = dispatch_one_hook_with_env(
        &proxy_bin,
        Some(env_arg),
        HookEvent::PreToolUse {
            tool_name: "bash_exec".to_string(),
            tool_input: json!({
                "command": "env",
            }),
            tool_use_id: "toolu_hook_e2e_proxy_port".to_string(),
        },
    )
    .await;

    assert!(matches!(outcome.control, HookResultControl::Continue));

    let mutated = outcome
        .result
        .mutated_tool_input
        .expect("persistent hook must mutate tool_input for bash_exec");
    let env = mutated
        .get("env")
        .and_then(Value::as_object)
        .expect("mutated tool_input must contain an 'env' object");
    let host = env
        .get("GCE_METADATA_HOST")
        .and_then(Value::as_str)
        .expect("GCE_METADATA_HOST must be injected");

    let port = host
        .strip_prefix("127.0.0.1:")
        .expect("metadata host should use localhost proxy");
    let port_num = port.parse::<u16>().expect("proxy port should parse");
    assert!(port_num > 0, "proxy port should be non-zero");
}

async fn dispatch_one_hook(proxy_bin: &Path, event: HookEvent) -> HookOutcome {
    dispatch_one_hook_with_env(proxy_bin, None, event).await
}

async fn dispatch_one_hook_with_env(
    proxy_bin: &Path,
    env_arg: Option<&str>,
    event: HookEvent,
) -> HookOutcome {
    let command = match env_arg {
        Some(env) => format!(
            r#"{} --env '{}' --hook '.'"#,
            shell_escape_path(proxy_bin),
            env
        ),
        None => format!("{} --hook '.'", shell_escape_path(proxy_bin)),
    };
    let hooks = vec![HookConfig {
        event: "PreToolUse".to_string(),
        matcher: Some("bash_exec".to_string()),
        command,
        timeout: None, // use framework default (30s); proxy startup can be slow on CI
        status_message: None,
        async_hook: None,
        hook_type: "claude-command-persistent".to_string(),
        package_dir: None,
    }];
    let persistent_manager = Arc::new(Mutex::new(PersistentHookManager::new()));
    let outcome = dispatch_hooks_with_count_and_manager(
        &event,
        &hooks,
        "hook-e2e-session",
        Path::new(env!("CARGO_MANIFEST_DIR")),
        0,
        None,
        Some(&persistent_manager),
    )
    .await;
    persistent_manager.lock().await.shutdown();
    outcome
}

// ── Test 2: full stack through bash execution ──────────────────────────────

/// Verify that the injected env vars actually reach the shell command via the
/// full `eval_tool_calls` path, with the persistent manager threaded through
/// `build_tool_eval_context` so `claude-command-persistent` hooks fire.
///
/// Requires both `harnx-proxy-auth` and `harnx-mcp-bash` to be built.
#[tokio::test(flavor = "multi_thread")]
async fn hook_injected_env_reaches_bash_exec_command() {
    let Some(proxy_bin) = proxy_binary_path() else {
        eprintln!("SKIP hook_injected_env_reaches_bash_exec_command: harnx-proxy-auth not found");
        return;
    };

    let bash_bin = mcp_bash_binary_path();
    if !bash_bin.is_file() {
        eprintln!(
            "SKIP hook_injected_env_reaches_bash_exec_command: harnx-mcp-bash not found at {}",
            bash_bin.display()
        );
        return;
    }

    let config = make_config(proxy_bin, bash_bin);
    let persistent_manager = Arc::new(Mutex::new(PersistentHookManager::new()));
    // Pass Some("*") so tool_declarations_for_use_tools connects to MCP servers
    // and populates their tool lists; None skips MCP entirely.
    let ctx = build_tool_eval_context(&config, Some("*"), None, &persistent_manager, None);

    // Skip if the MCP server failed to connect (e.g. sandbox execution restrictions).
    if !ctx.allowed_tool_names.contains("bash_exec") {
        eprintln!("SKIP hook_injected_env_reaches_bash_exec_command: harnx-mcp-bash did not register bash_exec (MCP connect failed)");
        persistent_manager.lock().await.shutdown();
        return;
    }

    let tool_call = ToolCall::new(
        "bash_exec".to_string(),
        json!({
            "command": "printf 'HTTPS_PROXY=%s\\nSSL_CERT_FILE=%s\\n' \"$HTTPS_PROXY\" \"$SSL_CERT_FILE\""
        }),
        Some("toolu_e2e_bash".to_string()),
        None,
    );

    let abort_signal = create_abort_signal();
    let result = eval_tool_calls(&ctx, vec![tool_call], &abort_signal)
        .await
        .expect("tool call must execute without error");

    persistent_manager.lock().await.shutdown();

    assert_eq!(result.len(), 1, "expected one tool result");

    let output = &result[0].output;

    // harnx-mcp-bash returns a JSON object; look for stdout in common keys.
    let stdout = output
        .get("stdout")
        .or_else(|| output.get("output"))
        .or_else(|| output.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!("bash_exec output did not contain recognisable stdout; output={output}")
        });

    assert!(
        stdout.contains("HTTPS_PROXY=http://127.0.0.1:"),
        "shell command must see injected HTTPS_PROXY; stdout={stdout}"
    );
    assert!(
        stdout.contains("SSL_CERT_FILE=") && stdout.contains("ca.pem"),
        "shell command must see injected SSL_CERT_FILE; stdout={stdout}"
    );
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn make_config(proxy_bin: PathBuf, bash_bin: PathBuf) -> GlobalConfig {
    let mut config = Config {
        working_mode: WorkingMode::Cmd,
        ..Config::default()
    };

    config.hooks = Some(HooksConfig {
        max_resume: None,
        entries: vec![HookConfig {
            event: "PreToolUse".to_string(),
            matcher: Some("bash_exec".to_string()),
            command: format!("{} --hook '.'", shell_escape_path(&proxy_bin)),
            timeout: None, // use framework default (30s)
            status_message: None,
            async_hook: None,
            hook_type: "claude-command-persistent".to_string(),
            package_dir: None,
        }],
    });

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    config.mcp_servers = vec![McpServerConfig {
        name: "bash".to_string(),
        command: bash_bin.to_string_lossy().into_owned(),
        args: vec![repo_root.to_string_lossy().into_owned()],
        env: HashMap::new(),
        roots: vec![repo_root.to_string_lossy().into_owned()],
        enabled: true,
        description: Some("bash test server".to_string()),
        rename_tools: HashMap::from([("exec".to_string(), "bash_exec".to_string())]),
        tool_templates: HashMap::new(),
        package: None,
        hooks: None,
    }];

    config.init_mcp_manager();

    Arc::new(RwLock::new(config))
}

fn proxy_binary_path() -> Option<PathBuf> {
    // When tests run via `cargo test`, CARGO_BIN_EXE_* is set for binaries in
    // the same package.
    if let Some(path) = std::option_env!("CARGO_BIN_EXE_harnx-proxy-auth") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    let candidate = target_dir().join(binary_name("harnx-proxy-auth"));
    candidate.is_file().then_some(candidate)
}

fn mcp_bash_binary_path() -> PathBuf {
    if let Some(path) = std::option_env!("CARGO_BIN_EXE_harnx-mcp-bash") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return p;
        }
    }
    target_dir().join(binary_name("harnx-mcp-bash"))
}

/// Returns the `target/debug` (or `target/release`) directory by walking up
/// from the current test executable path.
fn target_dir() -> PathBuf {
    let mut exe = std::env::current_exe().expect("current_exe");
    // Strip `<binary>` and possibly `deps/`
    exe.pop();
    if exe.ends_with("deps") {
        exe.pop();
    }
    exe
}

fn binary_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn shell_escape_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    // Single-quote the path and escape any embedded single quotes.
    format!("'{}'", s.replace('\'', "'\\''"))
}
