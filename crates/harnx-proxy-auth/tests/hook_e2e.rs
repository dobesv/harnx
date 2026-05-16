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
use harnx_core::hooks::{HookConfig, HookEvent, HookResultControl, HooksConfig};
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

    let hook_command = format!("{} --hook '.'", shell_escape_path(&proxy_bin));
    let hooks = vec![HookConfig {
        event: "PreToolUse".to_string(),
        matcher: Some("bash_exec".to_string()),
        command: hook_command,
        timeout: None, // use framework default (30s); proxy startup can be slow on CI
        status_message: None,
        async_hook: None,
        hook_type: "claude-command-persistent".to_string(),
    }];

    let event = HookEvent::PreToolUse {
        tool_name: "bash_exec".to_string(),
        tool_input: json!({ "command": "echo hello" }),
        tool_use_id: "toolu_hook_e2e_1".to_string(),
    };

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

    // Non-bash tools must NOT be mutated.
    let event_other = HookEvent::PreToolUse {
        tool_name: "read_file".to_string(),
        tool_input: json!({ "path": "/tmp/test" }),
        tool_use_id: "toolu_hook_e2e_2".to_string(),
    };
    let persistent_manager2 = Arc::new(Mutex::new(PersistentHookManager::new()));
    let outcome_other = dispatch_hooks_with_count_and_manager(
        &event_other,
        &hooks,
        "hook-e2e-session",
        Path::new(env!("CARGO_MANIFEST_DIR")),
        0,
        None,
        Some(&persistent_manager2),
    )
    .await;
    persistent_manager2.lock().await.shutdown();
    assert!(
        outcome_other.result.mutated_tool_input.is_none(),
        "non-bash tool must not be mutated"
    );
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
    let ctx = build_tool_eval_context(&config, None, &persistent_manager);

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
        rename_tools: HashMap::new(),
        tool_templates: HashMap::new(),
        package: None,
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
