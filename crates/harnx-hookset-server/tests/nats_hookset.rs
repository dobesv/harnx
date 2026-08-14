use anyhow::{Context, Result};
use async_trait::async_trait;
use harnx_core::hooks::{HookEvent, HookOutcome, HookPayload, HookResult, HookResultControl};
use harnx_core::instance::ServerScope;
use harnx_hookset::{
    FailPolicy, Hook, HookRegistration, HookSpec, HOOK_PROTOCOL_VERSION, HOOK_SCHEMA_VERSION,
};
use harnx_hookset_server::{
    hook_registration_key, serve_over_nats, serve_with_shutdown, HOOK_REGISTRY_BUCKET,
};
use harnx_nats_common::connect::NatsConnection;
use serde_json::json;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "hookset-server-test-token";

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
        )
        .await?;

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

async fn wait_for_registration(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<HookRegistration> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await {
            let key = hook_registration_key(instance_id, "echo");
            if let Some(value) = store.get(&key).await? {
                return serde_json::from_slice(&value).context("decode hook registration");
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for echo hook registration");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct EchoHook;

#[async_trait]
impl Hook for EchoHook {
    fn name(&self) -> &str {
        "echo"
    }

    fn hooks(&self) -> Vec<HookSpec> {
        vec![HookSpec {
            event: "PreToolUse".to_string(),
            matcher: None,
            priority: 0,
            timeout_secs: None,
            fail_policy: FailPolicy::Closed,
        }]
    }

    async fn handle_hook(&self, _payload: HookPayload) -> HookOutcome {
        HookOutcome {
            control: HookResultControl::Continue,
            result: HookResult {
                mutated_tool_input: Some(json!({"seen": true})),
                ..Default::default()
            },
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hookset_registers_and_serves_hook_over_nats() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let instance_id = ServerScope::new();
    let server_instance_id = instance_id.clone();
    let server_url = server.url.clone();
    let server_task = tokio::spawn(async move {
        serve_over_nats(EchoHook, server_instance_id, &server_url, TOKEN).await
    });
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;

    let registration = match wait_for_registration(&client, &instance_id).await {
        Ok(registration) => registration,
        Err(error) if server_task.is_finished() => {
            return Err(anyhow::anyhow!(
                "hook server exited before registration ({error:#}): {:?}",
                server_task.await
            ));
        }
        Err(error) => return Err(error),
    };
    assert_eq!(registration.server, "echo");
    assert_eq!(registration.hooks.len(), 1);
    assert_eq!(registration.hooks[0].event, "PreToolUse");
    assert_eq!(registration.schema_version, HOOK_SCHEMA_VERSION);
    assert_eq!(registration.proto_version, HOOK_PROTOCOL_VERSION);

    let payload = HookPayload {
        session_id: "test-session".to_string(),
        cwd: std::env::current_dir()?,
        resume_count: 0,
        hook_event: HookEvent::PreToolUse {
            tool_name: "example".to_string(),
            tool_input: json!({"input": true}),
            tool_use_id: "tool-use-1".to_string(),
        },
    };
    let message = client
        .request(
            instance_id.hook_subject("echo", "PreToolUse"),
            serde_json::to_vec(&payload)?.into(),
        )
        .await?;
    let outcome: HookOutcome =
        serde_json::from_slice(&message.payload).context("decode hook outcome")?;
    assert_eq!(outcome.control, HookResultControl::Continue);
    assert_eq!(
        outcome.result.mutated_tool_input,
        Some(json!({"seen": true}))
    );

    server_task.abort();
    drop(server);
    Ok(())
}

async fn registration_revision(client: &async_nats::Client, key: &str) -> Result<Option<u64>> {
    let jetstream = async_nats::jetstream::new(client.clone());
    // The bucket may not exist yet if no hook server has published to this
    // scope. That is "no revision yet", not a failure worth propagating here.
    let Ok(store) = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await else {
        return Ok(None);
    };
    Ok(store.entry(key).await?.map(|entry| entry.revision))
}

async fn wait_for_revision_beyond(
    client: &async_nats::Client,
    key: &str,
    previous: u64,
) -> Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(revision) = registration_revision(client, key).await? {
            if revision > previous {
                return Ok(revision);
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for registration revision to advance past {previous}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn connect_test_client(url: &str) -> Result<async_nats::Client> {
    async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(url)
        .await
        .context("connect test NATS client")
}

/// A rolling deploy publishes the replacement's registration under the same
/// `{scope}.{server}` key before the old instance finishes shutting down
/// (new pod ready before old pod terminates is Kubernetes' normal sequence).
/// The old instance's shutdown must delete only its own registration, never
/// the replacement's.
#[tokio::test(flavor = "multi_thread")]
async fn old_instances_shutdown_does_not_delete_a_replacements_registration() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let instance_id = ServerScope::new();
    let key = hook_registration_key(&instance_id, "echo");
    let client = connect_test_client(&server.url).await?;

    let old_shutdown = CancellationToken::new();
    let old_connection = NatsConnection {
        client: connect_test_client(&server.url).await?,
        replicas: 1,
    };
    let old_task = {
        let instance_id = instance_id.clone();
        let shutdown = old_shutdown.clone();
        tokio::spawn(async move {
            serve_with_shutdown(Arc::new(EchoHook), instance_id, old_connection, shutdown).await
        })
    };
    let old_revision = wait_for_revision_beyond(&client, &key, 0).await?;

    // Start the replacement under the SAME scope while the old instance is
    // still running, publishing over the same key with a newer revision.
    let new_shutdown = CancellationToken::new();
    let new_connection = NatsConnection {
        client: connect_test_client(&server.url).await?,
        replicas: 1,
    };
    let new_task = {
        let instance_id = instance_id.clone();
        let shutdown = new_shutdown.clone();
        tokio::spawn(async move {
            serve_with_shutdown(Arc::new(EchoHook), instance_id, new_connection, shutdown).await
        })
    };
    let new_revision = wait_for_revision_beyond(&client, &key, old_revision).await?;
    assert!(new_revision > old_revision);

    // Now tell the OLD instance to shut down. Its unconditional delete used
    // to remove whatever is currently at `key` -- the replacement's entry.
    old_shutdown.cancel();
    old_task
        .await
        .context("join old instance task")?
        .context("old instance exited with an error")?;

    let jetstream = async_nats::jetstream::new(client.clone());
    let store = jetstream.get_key_value(HOOK_REGISTRY_BUCKET).await?;
    assert!(
        store.get(&key).await?.is_some(),
        "old instance's shutdown deleted the replacement's registration"
    );
    let revision_after_old_shutdown = registration_revision(&client, &key)
        .await?
        .expect("registration entry should still exist");
    assert_eq!(
        revision_after_old_shutdown, new_revision,
        "the surviving registration must be the replacement's, not a re-published old one"
    );

    // The replacement's own shutdown should still delete its own, current
    // registration -- the conditional delete must not become a no-op.
    new_shutdown.cancel();
    new_task
        .await
        .context("join replacement task")?
        .context("replacement exited with an error")?;
    assert!(
        store.get(&key).await?.is_none(),
        "the replacement's own shutdown must delete its own current registration"
    );

    drop(server);
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
