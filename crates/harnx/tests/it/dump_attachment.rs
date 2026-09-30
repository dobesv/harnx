#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result};
use harnx_blob_store::{media_cid_url, put_media};
use harnx_core::cid_url::SessionRef;

const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn harnx_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harnx"))
}

fn write_cluster_config(root: &Path, url: &str) -> Result<()> {
    let servers = root.join("config/nats_servers");
    std::fs::create_dir_all(&servers).context("create NATS server config directory")?;
    std::fs::write(servers.join("test.yaml"), format!("url: {url}\n"))
        .context("write NATS server config")?;
    Ok(())
}

fn run_dump(root: &Path, url: &str, output_path: Option<&Path>) -> Result<Output> {
    let mut command = Command::new(harnx_bin());
    command.args(["dump", "attachment", url]);
    if let Some(path) = output_path {
        command.arg("--output").arg(path);
    }
    command
        .env("HARNX_CONFIG_DIR", root.join("config"))
        .env("HARNX_DATA_DIR", root.join("data"))
        .env("HARNX_STATE_DIR", root.join("state"))
        .env("HARNX_NATS_SERVER", "test")
        .env_remove("HARNX_NATS_URL")
        .env_remove("HARNX_NATS_TOKEN")
        .output()
        .context("run harnx dump attachment")
}

async fn seed_attachment(url: &str, bytes: &[u8], mime_type: &str) -> Result<String> {
    let client = async_nats::connect(url).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = harnx_blob_store::media::ensure_attachments_bucket(&jetstream, 1).await?;
    let session = SessionRef::new(Some("pantheon/atlas".to_string()), "dump01".to_string())?;
    let url = media_cid_url(&session, HASH);
    put_media(&store, &url, bytes, mime_type).await?;
    Ok(url.to_string())
}

async fn seed_and_run_dump(
    server_url: &str,
    bytes: &[u8],
    mime: &str,
    output_path: Option<&Path>,
) -> Result<(tempfile::TempDir, Output)> {
    let url = seed_attachment(server_url, bytes, mime).await?;
    let root = tempfile::tempdir()?;
    write_cluster_config(root.path(), server_url)?;
    let output = run_dump(root.path(), &url, output_path)?;
    Ok((root, output))
}

#[cfg(target_os = "linux")]
fn run_open(root: &Path, url: &str, opener_dir: &Path, marker: &Path) -> Result<Output> {
    let path = format!(
        "{}:{}",
        opener_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(harnx_bin())
        .args(["open", "attachment", url])
        .env("HARNX_CONFIG_DIR", root.join("config"))
        .env("HARNX_DATA_DIR", root.join("data"))
        .env("HARNX_STATE_DIR", root.join("state"))
        .env("HARNX_NATS_SERVER", "test")
        .env("OPEN_MARKER", marker)
        .env("PATH", path)
        .env_remove("HARNX_NATS_URL")
        .env_remove("HARNX_NATS_TOKEN")
        .output()
        .context("run harnx open attachment")
}

#[cfg(target_os = "linux")]
async fn wait_for_opened_path(marker: &Path) -> Result<PathBuf> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(path) = std::fs::read_to_string(marker) {
            return Ok(PathBuf::from(path));
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "system opener was not invoked"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dump_attachment_prints_text_from_nats() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let (_root, output) =
        seed_and_run_dump(server.url(), b"attachment text\n", "text/plain", None).await?;

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"attachment text\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dump_attachment_requires_output_for_binary_data() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let (_root, output) = seed_and_run_dump(
        server.url(),
        &[0, 1, 2, 3],
        "application/octet-stream",
        None,
    )
    .await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("binary attachment (application/octet-stream, 4B)"));
    assert!(stderr.contains("harnx open attachment <url>"));
    assert!(stderr.contains("--output <path>"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dump_attachment_writes_binary_output_file() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let destination_root = tempfile::tempdir()?;
    let destination = destination_root.path().join("attachment.bin");
    let (_root, output) = seed_and_run_dump(
        server.url(),
        &[0, 1, 2, 3],
        "application/octet-stream",
        Some(&destination),
    )
    .await?;

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(destination)?, [0, 1, 2, 3]);
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn open_attachment_launches_system_opener_with_mime_extension() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    harnx_core::require_nextest();
    let Some(server) = harnx_test_bins::spawn_nats_server().await? else {
        return Ok(());
    };
    let url = seed_attachment(server.url(), b"open me", "text/plain").await?;
    let root = tempfile::tempdir()?;
    let opener_dir = root.path().join("bin");
    let opener = opener_dir.join("xdg-open");
    let marker = root.path().join("opened-path");
    std::fs::create_dir_all(&opener_dir)?;
    std::fs::write(
        &opener,
        "#!/bin/sh\nprintf '%s' \"$1\" > \"$OPEN_MARKER\"\n",
    )?;
    std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755))?;
    write_cluster_config(root.path(), server.url())?;

    let output = run_open(root.path(), &url, &opener_dir, &marker)?;
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let opened = wait_for_opened_path(&marker).await?;
    assert_eq!(
        opened.extension().and_then(|value| value.to_str()),
        Some("txt")
    );
    assert_eq!(std::fs::read(&opened)?, b"open me");
    std::fs::remove_file(opened)?;
    Ok(())
}
