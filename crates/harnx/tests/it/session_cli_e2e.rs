use std::path::PathBuf;
use std::process::Command;

fn harnx_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harnx"))
}

fn session_help(command: &str, subject: &str) -> String {
    let output = Command::new(harnx_bin())
        .args([command, subject, "--help"])
        .output()
        .expect("failed to run harnx session help");
    assert!(output.status.success(), "{command} {subject}: {output:?}");
    String::from_utf8(output.stdout).expect("help must be UTF-8")
}

#[test]
fn cli_dump_session_help_shows_format_and_follow() {
    let stdout = session_help("dump", "session");
    assert!(stdout.contains("--format"), "stdout: {stdout}");
    assert!(stdout.contains("--follow"), "stdout: {stdout}");
}

#[test]
fn cli_info_session_help_shows_format() {
    let stdout = session_help("info", "session");
    assert!(stdout.contains("--format"), "stdout: {stdout}");
}

#[test]
fn cli_delete_session_help_shows_cluster() {
    let stdout = session_help("delete", "session");
    assert!(stdout.contains("--cluster"), "stdout: {stdout}");
}

#[test]
fn cli_list_sessions_help() {
    session_help("list", "sessions");
}

// ---------------------------------------------------------------------------
// --list-assistant-agents display normalization tests
// ---------------------------------------------------------------------------

/// Test --list-assistant-agents in cluster-client mode: local agents omitted,
/// default-cluster remotes bare, non-default cluster remotes suffixed.
#[test]
fn cli_list_assistant_agents_cluster_mode_shows_bare_default_cluster() {
    let tmp = tempfile::tempdir().unwrap();
    let agents_dir = tmp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();

    // Create a local agent (should be omitted in cluster mode)
    std::fs::write(agents_dir.join("local-agent.md"), "# local-agent").unwrap();

    // Create remote cluster config
    let nats_servers_dir = tmp.path().join("nats_servers");
    std::fs::create_dir_all(&nats_servers_dir).unwrap();
    std::fs::write(
        nats_servers_dir.join("mycluster.yaml"),
        r#"url: nats://localhost:4222
agents:
  - name: remote-assistant
    role: assistant
    description: Remote assistant
  - name: subagent-tool
    role: subagent
"#,
    )
    .unwrap();
    std::fs::write(
        nats_servers_dir.join("other.yaml"),
        r#"url: nats://localhost:4223
agents:
  - name: other-assistant
    role: assistant
    description: Other cluster assistant
"#,
    )
    .unwrap();

    let output = Command::new(harnx_bin())
        .args(["--list-assistant-agents"])
        .env("HARNX_CONFIG_DIR", tmp.path())
        .env("HARNX_STATE_DIR", tmp.path())
        .env("HARNX_NATS_SERVER", "mycluster")
        .output()
        .expect("failed to run harnx --list-assistant-agents");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Default-cluster remote should be bare
    assert!(
        stdout.contains("remote-assistant\n"),
        "expected remote-assistant bare, got: {}",
        stdout
    );
    // Non-default cluster should keep suffix
    assert!(
        stdout.contains("other-assistant@other\n"),
        "expected other-assistant@other suffixed, got: {}",
        stdout
    );
    // Local agent should NOT appear
    assert!(
        !stdout.contains("local-agent"),
        "local-agent should be omitted, got: {}",
        stdout
    );
    // Subagent should NOT appear
    assert!(
        !stdout.contains("subagent-tool"),
        "subagent-tool should be excluded, got: {}",
        stdout
    );
    // Should not have @mycluster suffix
    assert!(
        !stdout.contains("@mycluster"),
        "should not have @mycluster suffix, got: {}",
        stdout
    );
}

/// Test --list-assistant-agents in default mode: local agents bare, remotes suffixed.
#[test]
fn cli_list_assistant_agents_default_mode_shows_local_bare() {
    let tmp = tempfile::tempdir().unwrap();
    let agents_dir = tmp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();

    // Create a local agent
    std::fs::write(
        agents_dir.join("local-agent.md"),
        "---\nrole: assistant\nmodel: openai:gpt-4o\n---\nLocal agent",
    )
    .unwrap();

    // Create remote cluster config
    let nats_servers_dir = tmp.path().join("nats_servers");
    std::fs::create_dir_all(&nats_servers_dir).unwrap();
    std::fs::write(
        nats_servers_dir.join("shared.yaml"),
        r#"url: nats://localhost:4222
agents:
  - name: remote-assistant
    role: assistant
    description: Remote assistant
"#,
    )
    .unwrap();

    let output = Command::new(harnx_bin())
        .args(["--list-assistant-agents"])
        .env("HARNX_CONFIG_DIR", tmp.path())
        .env("HARNX_STATE_DIR", tmp.path())
        // NO HARNX_NATS_SERVER - default mode
        .output()
        .expect("failed to run harnx --list-assistant-agents");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Local agent should be bare
    assert!(
        stdout.contains("local-agent\n"),
        "expected local-agent bare, got: {}",
        stdout
    );
    // Remote agent should have suffix
    assert!(
        stdout.contains("remote-assistant@shared\n"),
        "expected remote-assistant@shared suffixed, got: {}",
        stdout
    );
}
