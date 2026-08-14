//! Proves that a SIGTERM sent directly to a running `harnx-time-server`
//! process (as Kubernetes would send to a pod) triggers deregistration, not
//! just that the plumbing between a `CancellationToken` and the serve loop
//! is wired correctly. `harnx-toolset-server`'s own tests cover the latter;
//! this covers the actual signal path end to end.

#![cfg(unix)]

use anyhow::{Context, Result};
use harnx_core::instance::{ServerScope, HARNX_SERVER_SCOPE};
use harnx_toolset::{HARNX_SERVER_CONFIG, HARNX_SERVER_PACKAGE};
use harnx_toolset_server::registration_key;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TOKEN: &str = "time-server-sigterm-test-token";
// harnx-time-server's `TimeToolset::name()` is "time", with no package or
// config-file stem set, giving the identity token `server_identity_token`
// would produce for `(None, "", "time")`.
const IDENTITY_TOKEN: &str = "____time";

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

struct TimeServerHandle(Child);

impl Drop for TimeServerHandle {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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

async fn spawn_nats_server() -> Result<Option<NatsServerHandle>> {
    let Some(binary) = nats_server_binary() else {
        eprintln!("skipping NATS integration test: nats-server binary not found");
        return Ok(None);
    };

    let store_dir = tempfile::tempdir().context("create NATS test store")?;
    let ports_dir = tempfile::tempdir().context("create NATS ports dir")?;
    let mut child = Command::new(binary)
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
    )
    .await?;

    match wait_for_nats_ready(&mut child, &url).await {
        Ok(()) => Ok(Some(NatsServerHandle {
            url,
            _store_dir: store_dir,
            _ports_dir: ports_dir,
            child,
        })),
        Err(error) => {
            // `wait_for_nats_ready` can fail with the child still running (a
            // flush error mid-connect, or the deadline passing without it
            // ever becoming ready) — dropping `Child` here would leak the
            // process and leave it holding its port and store directory
            // instead of killing it.
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

async fn wait_for_nats_ready(child: &mut Child, url: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match async_nats::ConnectOptions::new()
            .token(TOKEN.to_string())
            .connect(url)
            .await
        {
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

/// Poll the registration key until it matches `present`, or fail at
/// `deadline`. No blind sleeps: the assertion is only as slow as the actual
/// state change.
async fn wait_for_key_presence(
    jetstream: &async_nats::jetstream::Context,
    key: &str,
    present: bool,
    deadline: Instant,
) -> Result<()> {
    loop {
        if let Ok(store) = jetstream
            .get_key_value(harnx_toolset_server::TOOL_REGISTRY_BUCKET)
            .await
        {
            let found = store.get(key).await?.is_some();
            if found == present {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for registration '{key}' to become present={present}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_removes_the_registration() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };

    let instance_id = ServerScope::new();
    let child = Command::new(env!("CARGO_BIN_EXE_harnx-time-server"))
        .env(HARNX_SERVER_SCOPE, instance_id.as_str())
        .env("HARNX_NATS_URL", &server.url)
        .env("HARNX_NATS_TOKEN", TOKEN)
        // The child inherits this test process's environment by default; an
        // exported HARNX_SERVER_PACKAGE or HARNX_SERVER_CONFIG would change
        // the identity token the server registers under, so IDENTITY_TOKEN
        // above (computed for the empty case) would no longer match.
        .env_remove(HARNX_SERVER_PACKAGE)
        .env_remove(HARNX_SERVER_CONFIG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn harnx-time-server")?;
    let pid = child.id();
    let mut time_server = TimeServerHandle(child);

    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await
        .context("connect test NATS client")?;
    let jetstream = async_nats::jetstream::new(client);
    let key = registration_key(&instance_id, IDENTITY_TOKEN);

    wait_for_key_presence(
        &jetstream,
        &key,
        true,
        Instant::now() + Duration::from_secs(10),
    )
    .await?;

    // Send the process the same signal Kubernetes sends a pod being
    // terminated. No parent supervisor is involved here, unlike the worker's
    // `ToolServerSupervisor` (which only ever `abort()`s/SIGKILLs) — this is
    // the pod-lifecycle path this task exists for.
    let killed = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    assert_eq!(killed, 0, "failed to send SIGTERM to harnx-time-server");

    wait_for_key_presence(
        &jetstream,
        &key,
        false,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .context("registration should be gone soon after SIGTERM, not left to expire")?;

    let _ = time_server.0.wait();
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
async fn read_nats_ports_file(
    dir: &std::path::Path,
    child: &mut Child,
    deadline: Instant,
) -> Result<String> {
    loop {
        if let Some(url) = first_nats_client_url(dir) {
            return Ok(url);
        }
        // `std::process::Child` does not kill on drop, so every failure path here
        // has to reap the server or it keeps running after the test gives up.
        match child.try_wait() {
            Ok(Some(status)) => {
                anyhow::bail!("nats-server exited during startup: {status}")
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("poll nats-server during startup");
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "timed out waiting for the nats-server ports file in {}",
                dir.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
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
