#[path = "../../../harnx-toolset-server/tests/support/mcp_stdio.rs"]
mod mcp_stdio;

use anyhow::Result;
use serde_json::json;
use std::time::Duration;
use tokio::process::Command;

async fn exchange(filtered: bool) -> Result<()> {
    let root = tempfile::tempdir()?;
    let host_config = root.path().join("host-config");
    std::fs::write(
        &host_config,
        serde_json::to_vec(&json!({
            "apiVersion":"v1", "kind":"Config", "current-context":"fixture",
            "clusters":[{"name":"fixture","cluster":{"server":"https://127.0.0.1:6443"}}],
            "contexts":[{"name":"fixture","context":{"cluster":"fixture","user":"fixture"}}],
            "users":[{"name":"fixture","user":{"token":"host-secret-token"}}],
        }))?,
    )?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_harnx-k8s-creds"));
    command
        .args([
            "--tool-server",
            "--mcp-stdio",
            "--name=k8s_fixture",
            "--enable-tool",
            if filtered { "none" } else { "environment" },
            "--context",
            "fixture",
            "--kubeconfig",
        ])
        .arg(&host_config);
    let mut session = mcp_stdio::Session::start(&mut command, "k8s_fixture", filtered).await?;
    let mut synthetic = None;
    if !filtered {
        let reply = session.environment().await?;
        let path = std::path::PathBuf::from(reply["env"]["KUBECONFIG"].as_str().unwrap());
        assert!(path.is_absolute() && path.is_file());
        assert_ne!(path, host_config);
        assert_eq!(
            reply["filesystem"]["kubeconfig_path"],
            reply["env"]["KUBECONFIG"]
        );
        let source = std::fs::read_to_string(&path)?;
        assert!(source.contains("command: curl"));
        assert!(!source.contains("host-secret-token"));
        synthetic = Some(path);
    }
    session.shutdown().await?;
    if let Some(path) = synthetic {
        assert!(
            !path.exists(),
            "synthetic config removed on service shutdown"
        );
    }
    Ok(())
}

#[tokio::test]
async fn tool_server_mcp_stdio_environment_lifetime() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), Box::pin(exchange(false))).await?
}

#[tokio::test]
async fn tool_server_mcp_stdio_filter() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), Box::pin(exchange(true))).await?
}
