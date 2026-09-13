use std::path::PathBuf;
use std::process::Command;

fn harnx_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harnx"))
}

#[test]
fn cli_dump_session_help_shows_format_and_follow() {
    let output = Command::new(harnx_bin())
        .args(["dump", "session", "--help"])
        .output()
        .expect("failed to run harnx dump session --help");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--format"), "stdout: {stdout}");
    assert!(stdout.contains("--follow"), "stdout: {stdout}");
}

#[test]
fn cli_info_session_help_shows_format() {
    let output = Command::new(harnx_bin())
        .args(["info", "session", "--help"])
        .output()
        .expect("failed to run harnx info session --help");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--format"), "stdout: {stdout}");
}

#[test]
fn cli_delete_session_help_shows_cluster() {
    let output = Command::new(harnx_bin())
        .args(["delete", "session", "--help"])
        .output()
        .expect("failed to run harnx delete session --help");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--cluster"), "stdout: {stdout}");
}

#[test]
fn cli_list_sessions_help() {
    let output = Command::new(harnx_bin())
        .args(["list", "sessions", "--help"])
        .output()
        .expect("failed to run harnx list sessions --help");

    assert!(output.status.success());
}

#[test]
fn cli_dump_session_execution_dispatch_fails_gracefully_for_missing_session() {
    let output = Command::new(harnx_bin())
        .args([
            "dump",
            "session",
            "test-agent",
            "nonexistent-sess-id",
            "--format",
            "json",
        ])
        .output()
        .expect("failed to run harnx dump session");

    // Execution proceeds into run_dump_command -> run_dump_session_once
    // and exits with non-zero status due to missing session
    assert!(!output.status.success());
}

#[test]
fn cli_info_session_execution_dispatch_fails_gracefully_for_missing_session() {
    let output = Command::new(harnx_bin())
        .args([
            "info",
            "session",
            "test-agent",
            "nonexistent-sess-id",
            "--format",
            "yaml",
        ])
        .output()
        .expect("failed to run harnx info session");

    // Execution proceeds into run_info_command -> run_info_session
    // and exits with non-zero status due to missing session
    assert!(!output.status.success());
}

#[test]
fn cli_dump_session_follow_execution_dispatch_fails_gracefully_for_missing_session() {
    let output = Command::new(harnx_bin())
        .args([
            "dump",
            "session",
            "test-agent",
            "nonexistent-sess-id",
            "--follow",
        ])
        .output()
        .expect("failed to run harnx dump session --follow");

    // Execution proceeds into run_dump_command -> run_dump_session_follow
    // and exits with non-zero status due to missing session
    assert!(!output.status.success());
}
