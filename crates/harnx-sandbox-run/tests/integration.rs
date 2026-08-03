//! Integration tests for harnx-sandbox-run binary.
//!
//! These tests run the actual sandbox binary as a subprocess. They are
//! `#[cfg(unix)]` gated since birdcage is Unix-only.
//!
//! On environments where birdcage cannot initialize (e.g., restricted
//! containers without unprivileged user namespaces), tests gracefully
//! skip rather than fail.

#[cfg(unix)]
use std::fs;
use std::process::Command;
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

/// Path to the sandbox binary, set by cargo's CARGO_BIN_EXE_ env var.
fn binary_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_harnx-sandbox-run"))
}

#[cfg(unix)]
fn temp_test_dir(name: &str) -> std::path::PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("harnx-sandbox-run-{name}-{suffix}"));
    fs::create_dir_all(&dir).expect("create temp test dir");
    dir
}

#[cfg(unix)]
fn make_unix_script(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, body).expect("write script");
    let mut perms = fs::metadata(path).expect("script metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod script");
}

/// Probe whether the birdcage sandbox can actually initialize in the
/// current environment.
///
/// GitHub Actions Ubuntu runners and other restricted Linux environments
/// commonly disallow unprivileged user namespaces, which causes
/// `Sandbox::spawn()` to fail with EPERM at runtime.
///
/// If this returns false, tests should skip gracefully.
#[cfg(unix)]
fn sandbox_runtime_works() -> bool {
    // Try a minimal sandbox invocation
    let result = Command::new(binary_path())
        .args([
            "--exec",
            "/bin",
            "--exec",
            "/usr",
            "--exec",
            "/lib",
            "--exec",
            "/lib64",
            "--exec",
            "/usr/lib",
            "--exec",
            "/usr/lib64",
            "--exec",
            "/tmp",
            "--working-dir",
            "/tmp",
            "--",
            "true",
        ])
        .status();

    match result {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!(
                "sandbox runtime probe: birdcage cannot initialize here (exit={:?}) — skipping",
                status.code()
            );
            false
        }
        Err(err) => {
            eprintln!("sandbox runtime probe: failed to spawn binary: {err} — skipping");
            false
        }
    }
}

// === Unix-only sandbox tests ===
// These require birdcage to initialize successfully

#[cfg(unix)]
#[test]
fn test_basic_echo() {
    if !sandbox_runtime_works() {
        eprintln!("skipping: sandbox runtime not available");
        return;
    }

    let output = Command::new(binary_path())
        .args([
            "--exec",
            "/bin",
            "--exec",
            "/usr",
            "--working-dir",
            "/tmp",
            "--",
            "echo",
            "hello",
        ])
        .output()
        .expect("failed to spawn sandbox");

    assert!(output.status.success(), "sandbox should exit successfully");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello"),
        "output should contain 'hello', got: {stdout:?}"
    );
}

#[cfg(unix)]
#[test]
fn test_exit_code_propagation() {
    if !sandbox_runtime_works() {
        eprintln!("skipping: sandbox runtime not available");
        return;
    }

    let output = Command::new(binary_path())
        .args([
            "--exec",
            "/bin",
            "--exec",
            "/usr",
            "--working-dir",
            "/tmp",
            "--",
            "bash",
            "-c",
            "exit 42",
        ])
        .output()
        .expect("failed to spawn sandbox");

    let code = output.status.code().unwrap_or(-1);
    assert_eq!(
        code, 42,
        "sandbox should propagate exit code 42, got: {code}"
    );
}

#[cfg(unix)]
#[test]
fn test_env_var_passthrough() {
    if !sandbox_runtime_works() {
        eprintln!("skipping: sandbox runtime not available");
        return;
    }

    let output = Command::new(binary_path())
        .args([
            "--exec",
            "/bin",
            "--exec",
            "/usr",
            "--working-dir",
            "/tmp",
            "--env",
            "TEST_VAR=hello_sandbox",
            "--",
            "bash",
            "-c",
            "echo \"$TEST_VAR\"",
        ])
        .output()
        .expect("failed to spawn sandbox");

    assert!(output.status.success(), "sandbox should exit successfully");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello_sandbox"),
        "output should contain 'hello_sandbox', got: {stdout:?}"
    );
}

#[cfg(unix)]
#[test]
fn test_no_network_flag() {
    if !sandbox_runtime_works() {
        eprintln!("skipping: sandbox runtime not available");
        return;
    }

    // With --no-network, network access should be blocked
    // We test that the flag is accepted and the binary runs
    let output = Command::new(binary_path())
        .args([
            "--no-network",
            "--exec",
            "/bin",
            "--exec",
            "/usr",
            "--working-dir",
            "/tmp",
            "--",
            "echo",
            "isolated",
        ])
        .output()
        .expect("failed to spawn sandbox");

    assert!(
        output.status.success(),
        "sandbox should exit successfully with --no-network"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("isolated"),
        "output should contain 'isolated', got: {stdout:?}"
    );
}

#[cfg(unix)]
#[test]
fn test_working_dir_option() {
    if !sandbox_runtime_works() {
        eprintln!("skipping: sandbox runtime not available");
        return;
    }

    let output = Command::new(binary_path())
        .args([
            "--exec",
            "/bin",
            "--exec",
            "/usr",
            "--working-dir",
            "/tmp",
            "--",
            "bash",
            "-c",
            "pwd",
        ])
        .output()
        .expect("failed to spawn sandbox");

    assert!(output.status.success(), "sandbox should exit successfully");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("/tmp"),
        "working dir should be /tmp, got: {stdout:?}"
    );
}

#[cfg(unix)]
#[test]
fn test_hook_cli_injects_env() {
    if !sandbox_runtime_works() {
        eprintln!("skipping: sandbox runtime not available");
        return;
    }

    let dir = temp_test_dir("hook-env");
    let hook_path = dir.join("hook.sh");
    make_unix_script(
        &hook_path,
        r#"#!/bin/sh
cat >/dev/null
printf '%s' '{"hookSpecificOutput":{"toolInput":{"command":"env","env":{"HOOK_TEST_VAR":"from_hook"}}}}'
"#,
    );

    let output = Command::new(binary_path())
        .args([
            "--hook",
            "claude-command",
            hook_path.to_str().expect("utf8 hook path"),
            ";",
            "--exec",
            "/bin",
            "--exec",
            "/usr",
            "--working-dir",
            "/tmp",
            "--",
            "env",
        ])
        .output()
        .expect("failed to spawn sandbox");

    assert!(
        output.status.success(),
        "sandbox should exit successfully with hook, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line == "HOOK_TEST_VAR=from_hook"),
        "hook-injected env var missing from output: {stdout:?}"
    );
}

// === Tests that don't require sandbox runtime ===

#[test]
fn test_help_flag() {
    // --help should work without sandbox
    let output = Command::new(binary_path())
        .arg("--help")
        .output()
        .expect("failed to spawn sandbox --help");

    assert!(output.status.success(), "--help should exit successfully");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("sandbox"),
        "help output should mention 'sandbox', got: {stdout:?}"
    );
}

#[test]
fn test_missing_command_errors() {
    // Running without a command should error
    let output = Command::new(binary_path())
        .output()
        .expect("failed to spawn sandbox");

    // Should not succeed - clap requires the command
    assert!(
        !output.status.success(),
        "missing command should cause non-zero exit"
    );
}

#[test]
fn test_invalid_flag_errors() {
    // Invalid flag should error
    let output = Command::new(binary_path())
        .args(["--invalid-flag-that-does-not-exist", "--", "cmd"])
        .output()
        .expect("failed to spawn sandbox");

    assert_eq!(
        output.status.code(),
        Some(1),
        "invalid flag should fail startup"
    );
}

// === Additional Unix-specific tests ===

#[cfg(unix)]
#[test]
fn test_path_exceptions() {
    if !sandbox_runtime_works() {
        eprintln!("skipping: sandbox runtime not available");
        return;
    }

    // Test that --read, --write, --exec flags are accepted
    let output = Command::new(binary_path())
        .args([
            "--read",
            "/tmp",
            "--write",
            "/tmp",
            "--exec",
            "/bin",
            "--exec",
            "/usr",
            "--working-dir",
            "/tmp",
            "--",
            "true",
        ])
        .output()
        .expect("failed to spawn sandbox");

    assert!(
        output.status.success(),
        "sandbox should exit successfully with path exceptions"
    );
}

#[cfg(unix)]
#[test]
fn test_allow_rwx_flag() {
    if !sandbox_runtime_works() {
        eprintln!("skipping: sandbox runtime not available");
        return;
    }

    // Test that --allow-rwx flag is accepted
    let output = Command::new(binary_path())
        .args(["--allow-rwx", "/tmp", "--working-dir", "/tmp", "--", "true"])
        .output()
        .expect("failed to spawn sandbox");

    assert!(
        output.status.success(),
        "sandbox should exit successfully with --allow-rwx"
    );
}

#[cfg(unix)]
#[test]
fn test_no_defaults_flag() {
    if !sandbox_runtime_works() {
        eprintln!("skipping: sandbox runtime not available");
        return;
    }

    // Test that --no-defaults flag is accepted (will fail without explicit paths)
    // but we give it explicit paths for execution
    let output = Command::new(binary_path())
        .args([
            "--no-defaults",
            "--exec",
            "/bin",
            "--exec",
            "/usr",
            "--exec",
            "/lib",
            "--read",
            "/tmp",
            "--write",
            "/tmp",
            "--working-dir",
            "/tmp",
            "--",
            "true",
        ])
        .output()
        .expect("failed to spawn sandbox");

    assert!(
        output.status.success(),
        "sandbox should exit successfully with --no-defaults and explicit paths"
    );
}
