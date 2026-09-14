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
