//! WebSocket transport against a real `nats-server`.
//!
//! An AWS Application Load Balancer can require an x509 client certificate
//! from every client, which makes it a single gatekeeper in front of a NATS
//! cluster — but it only speaks HTTP, so harnx's `nats://`/`tls://` TCP
//! connections cannot traverse one. WebSocket is the transport that can, and
//! `nats-server` terminates it natively. These tests pin the part that broke
//! silently before: the `websockets` feature has to stay enabled on
//! `async-nats`, or a `ws://` URL falls through to the raw-TCP path and fails
//! at a handshake nobody is speaking.
//!
//! The mTLS leg itself is the load balancer's, not the broker's, so it isn't
//! reproducible here; `connect_options.rs` covers the configuration rules that
//! guard it.

use anyhow::{Context, Result};
use harnx_nats_common::connect::NatsEndpoint;
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

const TOKEN: &str = "websocket-probe-token";

struct WebSocketNatsServer {
    /// The `ws://` URL of the broker's WebSocket listener.
    websocket_url: String,
    _dir: TempDir,
    child: Child,
}

impl Drop for WebSocketNatsServer {
    /// A failing assertion unwinds past any explicit kill, and
    /// `std::process::Child` does not reap on drop — a stranded broker would
    /// then compete with every later broker test in the same nextest run.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn nats_server_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NATS_SERVER_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }
    which::which("nats-server").ok()
}

/// Start a broker with a WebSocket listener beside the usual TCP one.
///
/// The WebSocket listener has no CLI flag, so it needs a config file. Both
/// listeners take port `-1` and the ports file reports what the kernel handed
/// them, which avoids the probe-then-bind race a pre-chosen port would carry.
async fn spawn_websocket_nats_server() -> Result<Option<WebSocketNatsServer>> {
    let Some(binary) = nats_server_binary() else {
        eprintln!("skipping NATS WebSocket test: nats-server binary not found");
        return Ok(None);
    };

    let dir = tempfile::tempdir().context("create temp dir for the WebSocket broker")?;
    let store_dir = dir.path().join("store");
    let ports_dir = dir.path().join("ports");
    std::fs::create_dir_all(&store_dir).context("create JetStream store dir")?;
    std::fs::create_dir_all(&ports_dir).context("create ports dir")?;

    let config_path = dir.path().join("nats.conf");
    std::fs::write(
        &config_path,
        format!(
            "listen: 127.0.0.1:-1\n\
             jetstream {{ store_dir: {store:?} }}\n\
             authorization {{ token: {TOKEN:?} }}\n\
             websocket {{\n  listen: 127.0.0.1:-1\n  no_tls: true\n}}\n",
            store = store_dir,
        ),
    )
    .context("write nats-server config")?;

    let mut child = Command::new(&binary)
        .arg("-c")
        .arg(&config_path)
        .arg("--ports_file_dir")
        .arg(&ports_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {}", binary.display()))?;

    let websocket_url = match read_websocket_url(
        &ports_dir,
        &mut child,
        Instant::now() + Duration::from_secs(15),
    )
    .await
    {
        Ok(url) => url,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };

    Ok(Some(WebSocketNatsServer {
        websocket_url,
        _dir: dir,
        child,
    }))
}

/// Poll the ports file until the broker publishes its WebSocket listener.
async fn read_websocket_url(
    ports_dir: &std::path::Path,
    child: &mut Child,
    deadline: Instant,
) -> Result<String> {
    loop {
        if let Some(url) = read_websocket_url_once(ports_dir)? {
            return Ok(url);
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("nats-server exited during startup with {status}");
        }
        if Instant::now() >= deadline {
            anyhow::bail!("nats-server did not publish a WebSocket port before the deadline");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn read_websocket_url_once(ports_dir: &std::path::Path) -> Result<Option<String>> {
    let Some(entry) = std::fs::read_dir(ports_dir)
        .context("read ports dir")?
        .filter_map(Result::ok)
        .find(|entry| entry.path().extension().is_some_and(|ext| ext == "ports"))
    else {
        return Ok(None);
    };
    let Ok(contents) = std::fs::read_to_string(entry.path()) else {
        // The broker writes the file in one go, but a read that catches it
        // mid-write is not an error worth failing the test over.
        return Ok(None);
    };
    let Ok(ports): Result<serde_json::Value, _> = serde_json::from_str(&contents) else {
        return Ok(None);
    };
    Ok(ports
        .get("websocket")
        .and_then(|urls| urls.get(0))
        .and_then(|url| url.as_str())
        .map(str::to_string))
}

fn websocket_endpoint(url: &str) -> NatsEndpoint {
    NatsEndpoint {
        name: "websocket-probe".into(),
        url: url.to_string(),
        token: Some(TOKEN.to_string()),
        ..Default::default()
    }
}

/// The whole point of the feature: a `ws://` URL has to produce a working
/// connection. Without the `websockets` feature on `async-nats`, the scheme
/// still parses but the connector falls through to its raw-TCP arm and this
/// fails.
#[tokio::test]
async fn connects_over_a_websocket_url() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_websocket_nats_server().await? else {
        return Ok(());
    };

    let client = websocket_endpoint(&server.websocket_url)
        .connect()
        .await
        .context("connect over ws://")?;
    client.flush().await.context("flush the WebSocket client")?;

    let mut subscriber = client
        .subscribe("websocket.probe")
        .await
        .context("subscribe over ws://")?;
    client
        .publish("websocket.probe", "hello".into())
        .await
        .context("publish over ws://")?;
    client.flush().await.context("flush the publish")?;

    let message = tokio::time::timeout(
        Duration::from_secs(10),
        futures_util::StreamExt::next(&mut subscriber),
    )
    .await
    .context("wait for the round-tripped message")?
    .context("subscription closed before delivering the message")?;
    assert_eq!(message.payload.as_ref(), b"hello");
    Ok(())
}

/// JetStream is what harnx actually runs over this connection — session logs,
/// KV buckets and object stores all live there — so a Core round trip alone
/// would not prove the transport usable.
#[tokio::test]
async fn jetstream_works_over_a_websocket_url() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_websocket_nats_server().await? else {
        return Ok(());
    };

    let client = websocket_endpoint(&server.websocket_url)
        .connect()
        .await
        .context("connect over ws://")?;
    let jetstream = async_nats::jetstream::new(client);
    let bucket = jetstream
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: "websocket_probe".to_string(),
            history: 1,
            num_replicas: 1,
            ..Default::default()
        })
        .await
        .context("create a KV bucket over ws://")?;

    bucket
        .put("key", "value".into())
        .await
        .context("put over ws://")?;
    let stored = bucket
        .get("key")
        .await
        .context("get over ws://")?
        .context("the key just written must be readable")?;
    assert_eq!(stored.as_ref(), b"value");
    Ok(())
}
