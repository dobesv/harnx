use anyhow::{Context, Result};
use harnx_nats_common::registry::{ensure_bucket_with_ttl, REGISTRATION_TTL};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TOKEN: &str = "nats-common-test-token";

struct NatsServerHandle {
    url: String,
    _store_dir: TempDir,
    child: Child,
}

impl Drop for NatsServerHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn spawn_nats_server() -> Result<Option<NatsServerHandle>> {
    let Some(binary) = nats_server_binary() else {
        eprintln!("skipping NATS integration test: nats-server binary not found");
        return Ok(None);
    };

    const MAX_START_ATTEMPTS: usize = 5;
    for attempt in 1..=MAX_START_ATTEMPTS {
        let listener = TcpListener::bind("127.0.0.1:0").context("allocate NATS test port")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let store_dir = tempfile::tempdir().context("create NATS test store")?;
        let mut child = Command::new(&binary)
            .arg("-js")
            .arg("-sd")
            .arg(store_dir.path())
            .arg("-p")
            .arg(port.to_string())
            .arg("--auth")
            .arg(TOKEN)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn nats-server")?;
        let url = format!("nats://127.0.0.1:{port}");

        match wait_for_nats_ready(&mut child, &url).await {
            Ok(()) => {
                return Ok(Some(NatsServerHandle {
                    url,
                    _store_dir: store_dir,
                    child,
                }));
            }
            Err(error) => {
                let exited_during_startup = child.try_wait()?.is_some();
                let _ = child.kill();
                let _ = child.wait();
                if exited_during_startup && attempt < MAX_START_ATTEMPTS {
                    eprintln!(
                        "nats-server exited during startup attempt {attempt}; retrying with a new port: {error:#}"
                    );
                    continue;
                }
                return Err(error).context(format!(
                    "start nats-server after {attempt} attempt{}",
                    if attempt == 1 { "" } else { "s" }
                ));
            }
        }
    }

    unreachable!("NATS startup loop always returns")
}

async fn wait_for_nats_ready(child: &mut Child, url: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let result = async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(url)
            .await;
        match result {
            Ok(client) => return client.flush().await.context("flush NATS test client"),
            Err(error) if child.try_wait()?.is_some() => {
                anyhow::bail!("nats-server exited during startup: {error}");
            }
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error).context("wait for nats-server readiness"),
        }
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

#[tokio::test]
async fn ensure_bucket_sets_ttl_and_reconciles_existing_bucket() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: no nats-server binary available");
        return Ok(());
    };
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await
        .context("connect test NATS client")?;
    let jetstream = async_nats::jetstream::new(client);

    // Pre-create the bucket with no TTL, standing in for a bucket made by an
    // older build.
    jetstream
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: "ttl_probe".to_string(),
            history: 1,
            num_replicas: 1,
            max_age: Duration::ZERO,
            storage: async_nats::jetstream::stream::StorageType::File,
            ..Default::default()
        })
        .await
        .context("pre-create bucket")?;

    ensure_bucket_with_ttl(&jetstream, "ttl_probe", REGISTRATION_TTL, 1)
        .await
        .context("ensure bucket")?;

    let info = jetstream
        .get_stream("KV_ttl_probe")
        .await
        .context("get backing stream")?
        .info()
        .await
        .context("stream info")?
        .clone();
    assert_eq!(info.config.max_age, REGISTRATION_TTL);
    Ok(())
}
