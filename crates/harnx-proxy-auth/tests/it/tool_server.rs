//! Real tool-service lifecycle, stdio framing, filtering and temp-file ownership.
use super::mcp_stdio;

use anyhow::Result;
use serde_json::Value;
use tokio::{
    process::Command,
    time::{timeout, Duration},
};

const DEADLINE: Duration = Duration::from_secs(15);

async fn exchange(filtered: bool) -> Result<Value> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_harnx-proxy-auth"));
    command.args([
        "--tool-server",
        "--mcp-stdio",
        "--name=proxy_fixture",
        "--enable-tool",
        if filtered { "no-match" } else { "environment" },
        "--fs",
        r#"{"provisioned.txt":"fixture"}"#,
        "--env",
        r#"{"CONFIG_ROOT":"\($temp_file_root)"}"#,
    ]);
    let mut session = mcp_stdio::Session::start(&mut command, "proxy_fixture", filtered).await?;
    if filtered {
        session.shutdown().await?;
        return Ok(Value::Null);
    }
    let env = session.environment().await?;
    let ca = std::path::Path::new(env["filesystem"]["ca_cert_path"].as_str().unwrap());
    let root = std::path::Path::new(env["filesystem"]["temp_file_root"].as_str().unwrap());
    assert!(ca.is_file());
    assert_eq!(
        std::fs::read_to_string(root.join("provisioned.txt"))?,
        "fixture"
    );
    assert_eq!(env["env"]["CONFIG_ROOT"], root.to_string_lossy().as_ref());
    session.shutdown().await?;
    assert!(!ca.exists(), "CA is removed on graceful service exit");
    assert!(
        !root.exists(),
        "provisioned files are removed on graceful service exit"
    );
    Ok(env)
}

#[tokio::test]
async fn tool_server_mcp_stdio_owns_files_until_shutdown() -> Result<()> {
    timeout(DEADLINE, Box::pin(exchange(false))).await??;
    Ok(())
}

#[tokio::test]
async fn tool_server_mcp_stdio_honors_enable_tool() -> Result<()> {
    timeout(DEADLINE, Box::pin(exchange(true))).await??;
    Ok(())
}
