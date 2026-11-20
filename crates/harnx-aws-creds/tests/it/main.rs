#[path = "../../../harnx-toolset-server/tests/support/mcp_stdio.rs"]
mod mcp_stdio;

use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;

async fn exchange(filtered: bool) -> Result<()> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_harnx-aws-creds"));
    command
        .args([
            "--tool-server",
            "--mcp-stdio",
            "--name=aws_fixture",
            "--enable-tool",
            if filtered { "none" } else { "environment" },
        ])
        .env("AWS_REGION", "us-west-2")
        .env("AWS_ACCESS_KEY_ID", "fixture-access-key")
        .env("AWS_SECRET_ACCESS_KEY", "fixture-secret-key")
        .env("AWS_EC2_METADATA_DISABLED", "true");
    let mut session = mcp_stdio::Session::start(&mut command, "aws_fixture", filtered).await?;
    if !filtered {
        let reply = session.environment().await?;
        let text = reply.to_string();
        assert_eq!(reply["filesystem"], json!({}));
        assert_eq!(reply["env"]["AWS_REGION"], "us-west-2");
        assert!(!text.contains("fixture-access-key") && !text.contains("fixture-secret-key"));
        assert!(reply["env"]["AWS_CONTAINER_CREDENTIALS_FULL_URI"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:"));
        assert!(reply["env"]
            .as_object()
            .unwrap()
            .values()
            .all(Value::is_string));
    }
    session.shutdown().await
}

#[tokio::test]
async fn tool_server_mcp_stdio_environment() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), Box::pin(exchange(false))).await?
}

#[tokio::test]
async fn tool_server_mcp_stdio_filter() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), Box::pin(exchange(true))).await?
}
