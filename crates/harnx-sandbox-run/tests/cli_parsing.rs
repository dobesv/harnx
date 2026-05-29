//! CLI parsing unit tests.
//!
//! These tests exercise `pre_parse_hooks` edge cases and `Cli` struct parsing
//! via clap's `try_parse_from`. They run without spawning the sandbox.

use std::path::PathBuf;

// Access Cli and pre_parse_hooks via the crate's public API.
// Since this is a binary crate, we need to test via the test helper module.
// For binary crates, integration tests in `tests/` cannot directly import from
// `main.rs`. We use a #[path] include to access the cli module directly.
#[path = "../src/cli.rs"]
mod cli;

use cli::{pre_parse_hooks, Cli};

// === pre_parse_hooks tests ===

#[test]
fn test_hook_passthrough_to_clap() {
    // Hook group is removed; remaining args have normal flags
    let raw = vec![
        "--hook".to_string(),
        "claude-command".to_string(),
        "echo".to_string(),
        ";".to_string(),
        "--no-network".to_string(),
        "--".to_string(),
        "ls".to_string(),
    ];
    let (hooks, remaining) = pre_parse_hooks(raw).expect("parse");
    assert_eq!(hooks.len(), 1);
    assert_eq!(remaining, vec!["--no-network", "--", "ls"]);
}

#[test]
fn test_handles_no_hooks_passthrough() {
    // No hooks, args pass through unchanged
    let raw = vec![
        "--no-network".to_string(),
        "--".to_string(),
        "echo".to_string(),
        "hello".to_string(),
    ];
    let (hooks, remaining) = pre_parse_hooks(raw).expect("parse");
    assert!(hooks.is_empty());
    assert_eq!(remaining, vec!["--no-network", "--", "echo", "hello"]);
}

// === Cli parsing tests via clap ===

#[test]
fn test_parse_env_var_format() {
    // --env VAR=value parses correctly
    let cli = <Cli as clap::Parser>::try_parse_from([
        "harnx-sandbox-run",
        "--env",
        "TEST_VAR=hello",
        "--",
        "echo",
        "test",
    ])
    .expect("parse should succeed");
    assert!(cli.env_vars.contains(&"TEST_VAR=hello".to_string()));
    assert_eq!(cli.command.len(), 2);
}

#[test]
fn test_parse_env_var_inherit() {
    // --env VAR (no value) parses correctly
    let cli =
        <Cli as clap::Parser>::try_parse_from(["harnx-sandbox-run", "--env", "HOME", "--", "bash"])
            .expect("parse should succeed");
    assert!(cli.env_vars.contains(&"HOME".to_string()));
}

#[test]
fn test_parse_no_network() {
    // --no-network flag parses correctly
    let cli = <Cli as clap::Parser>::try_parse_from([
        "harnx-sandbox-run",
        "--no-network",
        "--",
        "curl",
        "https://example.com",
    ])
    .expect("parse should succeed");
    assert!(cli.no_network);
}

#[test]
fn test_parse_working_dir() {
    // --working-dir /tmp parses correctly
    let cli = <Cli as clap::Parser>::try_parse_from([
        "harnx-sandbox-run",
        "--working-dir",
        "/tmp",
        "--",
        "ls",
    ])
    .expect("parse should succeed");
    assert_eq!(cli.working_dir, Some(PathBuf::from("/tmp")));
}

#[test]
fn test_parse_command_after_double_dash() {
    // -- echo hello becomes the command
    let cli = <Cli as clap::Parser>::try_parse_from([
        "harnx-sandbox-run",
        "--",
        "echo",
        "hello",
        "world",
    ])
    .expect("parse should succeed");
    assert_eq!(cli.command.len(), 3);
    assert_eq!(cli.command[0].to_string_lossy(), "echo");
    assert_eq!(cli.command[1].to_string_lossy(), "hello");
    assert_eq!(cli.command[2].to_string_lossy(), "world");
}

#[test]
fn test_parse_multiple_path_flags() {
    // --extra-read /a --extra-write /b --extra-exec /c all parsed
    let cli = <Cli as clap::Parser>::try_parse_from([
        "harnx-sandbox-run",
        "--extra-read",
        "/a",
        "--extra-write",
        "/b",
        "--extra-exec",
        "/c",
        "--",
        "cmd",
    ])
    .expect("parse should succeed");
    assert!(cli.extra_read.contains(&PathBuf::from("/a")));
    assert!(cli.extra_write.contains(&PathBuf::from("/b")));
    assert!(cli.extra_exec.contains(&PathBuf::from("/c")));
}

#[test]
fn test_parse_repeated_path_flags() {
    // Multiple --extra-read flags accumulate
    let cli = <Cli as clap::Parser>::try_parse_from([
        "harnx-sandbox-run",
        "--extra-read",
        "/a",
        "--extra-read",
        "/b",
        "--extra-read",
        "/c",
        "--",
        "cmd",
    ])
    .expect("parse should succeed");
    assert_eq!(cli.extra_read.len(), 3);
    assert!(cli.extra_read.contains(&PathBuf::from("/a")));
    assert!(cli.extra_read.contains(&PathBuf::from("/b")));
    assert!(cli.extra_read.contains(&PathBuf::from("/c")));
}

#[test]
fn test_parse_extra_rwx() {
    // --extra-rwx /tmp parses correctly
    let cli = <Cli as clap::Parser>::try_parse_from([
        "harnx-sandbox-run",
        "--extra-rwx",
        "/tmp",
        "--",
        "cmd",
    ])
    .expect("parse should succeed");
    assert!(cli.extra_rwx.contains(&PathBuf::from("/tmp")));
}

#[test]
fn test_parse_no_defaults() {
    // --no-defaults flag parses correctly
    let cli =
        <Cli as clap::Parser>::try_parse_from(["harnx-sandbox-run", "--no-defaults", "--", "cmd"])
            .expect("parse should succeed");
    assert!(cli.no_defaults);
}

#[test]
fn test_parse_missing_command_errors() {
    // Missing command after -- should error
    let result = <Cli as clap::Parser>::try_parse_from(["harnx-sandbox-run", "--"]);
    assert!(result.is_err(), "expected error when command is missing");
}

#[test]
fn test_parse_no_double_dash_errors() {
    // clap's `last = true` requires `--` before the command
    // Without it, the command is treated as an unknown argument
    let result = <Cli as clap::Parser>::try_parse_from(["harnx-sandbox-run", "echo", "hello"]);
    assert!(
        result.is_err(),
        "expected error when -- is missing before command"
    );
}

#[test]
fn test_parse_unknown_flag_errors() {
    // Unknown flag should cause clap error
    let result =
        <Cli as clap::Parser>::try_parse_from(["harnx-sandbox-run", "--unknown-flag", "--", "cmd"]);
    assert!(result.is_err(), "expected error for unknown flag");
}
