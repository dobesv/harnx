//! The private byte transport must never be registered with a NATS host.
use std::process::Stdio;
use std::time::Duration;

async fn expect_http_required(args: &[&str], enabled_env: bool) {
    let bin = std::env::var("CARGO_BIN_EXE_harnx-fs-tools")
        .expect("CARGO_BIN_EXE_harnx-fs-tools must be set");
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(bin)
            .args(args)
            .env(
                "HARNX_FS_SANDBOX_TRANSPORT",
                if enabled_env { "1" } else { "0" },
            )
            .env("HARNX_NATS_URL", "invalid NATS URL")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("invalid transport must exit without serving or connecting to NATS")
    .expect("run fs-tools");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("requires --mcp-http"), "{stderr}");
    assert!(stderr.contains("HARNX_FS_SANDBOX_TRANSPORT"), "{stderr}");
    assert!(stderr.contains("not NATS or MCP stdio"), "{stderr}");
}

#[tokio::test]
async fn sandbox_transport_flag_rejects_nats_and_stdio_at_startup() {
    expect_http_required(&["--sandbox-transport"], false).await;
    expect_http_required(&["--mcp-stdio", "--sandbox-transport"], false).await;
}

#[tokio::test]
async fn sandbox_transport_env_rejects_nats_and_stdio_at_startup() {
    expect_http_required(&[], true).await;
    expect_http_required(&["--mcp-stdio"], true).await;
}

#[tokio::test]
async fn sandbox_transport_does_not_treat_option_values_as_http_mode() {
    for args in [
        ["--sandbox-transport", "--name", "--mcp-http"],
        ["--sandbox-transport", "--allow-write", "--mcp-http"],
    ] {
        expect_http_required(&args, false).await;
    }
}
