use anyhow::{Context, Result};
use async_nats::jetstream;
use harnx_nats_common::registry::{
    ensure_bucket_with_ttl, reconcile_bucket_replicas, REGISTRATION_TTL,
};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TOKEN: &str = "nats-common-test-token";

struct NatsServerHandle {
    url: String,
    _store_dir: TempDir,
    _ports_dir: TempDir,
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
        let store_dir = tempfile::tempdir().context("create NATS test store")?;
        let ports_dir = tempfile::tempdir().context("create NATS ports dir")?;
        let mut child = Command::new(&binary)
            .arg("-js")
            .arg("-sd")
            .arg(store_dir.path())
            .arg("-a")
            .arg("127.0.0.1")
            .arg("-p")
            .arg("-1")
            .arg("--auth")
            .arg(TOKEN)
            .arg("--ports_file_dir")
            .arg(ports_dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn nats-server")?;
        let url = read_nats_ports_file(
            ports_dir.path(),
            &mut child,
            Instant::now() + Duration::from_secs(15),
        )?;

        match wait_for_nats_ready(&mut child, &url).await {
            Ok(()) => {
                return Ok(Some(NatsServerHandle {
                    url,
                    _store_dir: store_dir,
                    _ports_dir: ports_dir,
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

/// Connect to `server` and wrap it as a JetStream context, the way every
/// test in this file needs its NATS connection.
async fn test_jetstream(server: &NatsServerHandle) -> Result<jetstream::Context> {
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await
        .context("connect test NATS client")?;
    Ok(jetstream::new(client))
}

/// The live replica count on a bucket's backing stream, read back from the
/// server rather than assumed — the assertion every test in this file that
/// claims to check a reconcile outcome actually needs.
async fn live_num_replicas(jetstream: &jetstream::Context, bucket: &str) -> Result<usize> {
    Ok(jetstream
        .get_stream(format!("KV_{bucket}"))
        .await
        .with_context(|| format!("get backing stream for '{bucket}'"))?
        .info()
        .await
        .with_context(|| format!("stream info for '{bucket}'"))?
        .config
        .num_replicas)
}

#[tokio::test]
async fn ensure_bucket_sets_ttl_and_reconciles_existing_bucket() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: no nats-server binary available");
        return Ok(());
    };
    let jetstream = test_jetstream(&server).await?;

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

/// Re-requesting the replica count a bucket already has is a no-op: no error,
/// and the stream still reports the same count.
#[tokio::test]
async fn ensure_bucket_reconciles_replicas_noop_when_already_matching() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: no nats-server binary available");
        return Ok(());
    };
    let jetstream = test_jetstream(&server).await?;

    ensure_bucket_with_ttl(&jetstream, "replicas_noop", REGISTRATION_TTL, 1)
        .await
        .context("create bucket at replicas=1")?;
    // Same request again: nothing to reconcile, must still succeed.
    ensure_bucket_with_ttl(&jetstream, "replicas_noop", REGISTRATION_TTL, 1)
        .await
        .context("re-ensure bucket at replicas=1")?;

    assert_eq!(live_num_replicas(&jetstream, "replicas_noop").await?, 1);
    Ok(())
}

/// Raising the replica count on a bucket that already exists must never stop
/// harnx starting, even on a dev server that cannot really host 3 copies.
///
/// This does not exercise a genuine "the cluster refused the raise" rejection:
/// against a single, non-clustered `nats-server` (verified against 2.11.6),
/// `update_stream` accepts any `num_replicas` value unconditionally --
/// unlike creating a brand-new stream, which does reject `num_replicas > 1`
/// with "replicas > 1 not supported in non-clustered mode". So on this
/// harness the raise actually succeeds; what this test pins down is that
/// `ensure_bucket_with_ttl` returns `Ok` either way, that the raise actually
/// lands on the live stream, and that the `if let Err(..) =
/// reconcile_bucket_config(..)` wrapping around the update in
/// `ensure_bucket_with_ttl` is still in place and non-fatal. Demonstrating an
/// actual rejection would need a real under-provisioned cluster (e.g. 2 nodes
/// asked for `replicas: 3`), which this test suite does not stand up.
#[tokio::test]
async fn ensure_bucket_raising_replicas_on_existing_bucket_does_not_fail_startup() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: no nats-server binary available");
        return Ok(());
    };
    let jetstream = test_jetstream(&server).await?;

    ensure_bucket_with_ttl(&jetstream, "raise_replicas", REGISTRATION_TTL, 1)
        .await
        .context("create bucket at replicas=1")?;
    ensure_bucket_with_ttl(&jetstream, "raise_replicas", REGISTRATION_TTL, 3)
        .await
        .context("raising replicas on an existing bucket must not fail startup")?;

    assert_eq!(
        live_num_replicas(&jetstream, "raise_replicas").await?,
        3,
        "the raise must actually apply"
    );
    Ok(())
}

/// Reconcile never lowers replicas: a caller that only knows a smaller value
/// than a bucket already has (e.g. a stale default, or a caller that never
/// resolved the cluster's real config) must not be able to downgrade its
/// actual fault tolerance. See `reconcile_bucket_replicas`'s doc comment for
/// why — this is exactly the bug the hourly remote-session GC lease hit
/// before it resolved its cluster's real `replicas`.
#[tokio::test]
async fn reconcile_bucket_replicas_never_lowers_an_existing_higher_count() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: no nats-server binary available");
        return Ok(());
    };
    let jetstream = test_jetstream(&server).await?;

    ensure_bucket_with_ttl(&jetstream, "no_lower", REGISTRATION_TTL, 1)
        .await
        .context("create bucket at replicas=1")?;

    // Raise it out-of-band (bypassing our reconcile function) to stand in
    // for a bucket a real cluster already correctly reconciled up to 3;
    // `update_stream` accepts this unconditionally even on this
    // non-clustered test server (see the doc comment two tests up).
    let mut config = jetstream
        .get_stream("KV_no_lower")
        .await
        .context("get backing stream")?
        .info()
        .await
        .context("stream info")?
        .config
        .clone();
    config.num_replicas = 3;
    jetstream
        .update_stream(config)
        .await
        .context("raise replicas out of band to simulate an already-reconciled bucket")?;

    // A caller that only knows `1` (a stale default, or one that never
    // resolved the cluster's actual configured value) must not drag this
    // back down.
    reconcile_bucket_replicas(&jetstream, "no_lower", 1)
        .await
        .context("reconcile with a lower request must not error")?;

    assert_eq!(
        live_num_replicas(&jetstream, "no_lower").await?,
        3,
        "reconcile must never lower an existing bucket's replicas"
    );
    Ok(())
}

/// Read the client URL out of the ports file nats-server writes once it has
/// bound its listeners.
///
/// The file is named `<executable_name>_<pid>.ports`, so it's found by scanning
/// the (private) directory rather than by rebuilding the name — `NATS_SERVER_BIN`
/// can point at a differently named binary. nats-server writes into the file
/// directly rather than renaming it into place, so a partial read is possible;
/// failing to parse is treated the same as not-yet-written.
fn read_nats_ports_file(
    dir: &std::path::Path,
    child: &mut Child,
    deadline: Instant,
) -> Result<String> {
    loop {
        if let Some(url) = first_nats_client_url(dir) {
            return Ok(url);
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("nats-server exited during startup: {status}");
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for the nats-server ports file in {}",
                dir.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn first_nats_client_url(dir: &std::path::Path) -> Option<String> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.extension().is_some_and(|ext| ext == "ports") {
            let contents = std::fs::read_to_string(&path).ok()?;
            let parsed: serde_json::Value = serde_json::from_str(&contents).ok()?;
            let url = parsed.get("nats")?.get(0)?.as_str()?;
            return Some(url.to_string());
        }
    }
    None
}
