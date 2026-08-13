use anyhow::{Context, Result};
use harnx_core::instance::ServerScope;
use harnx_fs_tools::{FsServer, FsToolset, ListDirectoryParams, ReadFileParams};
use harnx_tool_allow::ResolvedAllowlist;
use harnx_toolset::{
    Registration, ToolErrorPayload, ToolReply, ToolRequest, HDR_CALL_ID, HDR_IDEMPOTENCY_KEY,
};
use harnx_toolset_server::{registration_key, serve_over_nats, TOOL_REGISTRY_BUCKET};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TOKEN: &str = "fs-toolset-test-token";

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

fn request_headers(call_id: &str, idempotency_key: &str) -> async_nats::HeaderMap {
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(HDR_CALL_ID, call_id);
    headers.insert(HDR_IDEMPOTENCY_KEY, idempotency_key);
    headers
}

async fn wait_for_registration(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<Registration> {
    let jetstream = async_nats::jetstream::new(client.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(store) = jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await {
            let key = registration_key(instance_id, "____fs");
            if let Some(value) = store.get(&key).await? {
                return serde_json::from_slice(&value).context("decode registration");
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for fs tool registration");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn invoke(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    tool: &str,
    args: Value,
) -> Result<ToolReply> {
    let request_id = uuid::Uuid::new_v4();
    let call_id = format!("fs-{tool}-{request_id}");
    let request = ToolRequest {
        call_id: call_id.clone(),
        tool: tool.to_string(),
        args,
        parent_session_id: None,
    };
    let message = client
        .request_with_headers(
            instance_id.tool_subject("____fs", tool),
            request_headers(&call_id, &format!("fs-{tool}-{request_id}-idempotency")),
            serde_json::to_vec(&request)?.into(),
        )
        .await?;
    serde_json::from_slice(&message.payload).context("decode tool reply")
}

fn assert_content_envelope(value: &Value) {
    assert!(value.is_object(), "tool result must be an object: {value}");
    assert!(
        value.get("content").and_then(Value::as_array).is_some(),
        "tool result must contain a content array: {value}"
    );
}

async fn assert_registration(client: &async_nats::Client, instance_id: &ServerScope) -> Result<()> {
    let registration = wait_for_registration(client, instance_id).await?;
    assert_eq!(registration.server, "fs");
    assert_eq!(registration.tools.len(), 9);
    let tool_hints: HashMap<_, _> = registration
        .tools
        .iter()
        .map(|tool| (tool.name.as_str(), tool.read_only_hint))
        .collect();
    assert_eq!(
        tool_hints,
        HashMap::from([
            ("read", true),
            ("write", false),
            ("edit", false),
            ("insert", false),
            ("re_replace", false),
            ("ls", true),
            ("grep", true),
            ("find", true),
            ("rollback_file", false),
        ])
    );
    Ok(())
}

async fn assert_write_read_round_trip(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    root_path: &Path,
) -> Result<Value> {
    let file_arg = root_path
        .join("round-trip.txt")
        .to_string_lossy()
        .into_owned();
    let content = "native NATS filesystem round trip\n";
    let write_reply = invoke(
        client,
        instance_id,
        "write",
        json!({"path": file_arg, "content": content}),
    )
    .await?;
    assert!(write_reply.result.is_ok(), "write failed: {write_reply:?}");

    let read_reply = invoke(client, instance_id, "read", json!({"path": file_arg})).await?;
    let read_value = read_reply
        .result
        .map_err(|error| anyhow::anyhow!("read failed: {error:?}"))?;
    assert_content_envelope(&read_value);
    assert!(
        read_value.to_string().contains(content.trim()),
        "read result did not contain written content: {read_value}"
    );
    Ok(read_value)
}

async fn assert_out_of_root_denied(
    client: &async_nats::Client,
    instance_id: &ServerScope,
) -> Result<()> {
    let outside = tempfile::tempdir().context("create out-of-root directory")?;
    let outside_file = outside.path().join("outside.txt");
    std::fs::write(&outside_file, "outside")?;
    let denied_reply = invoke(
        client,
        instance_id,
        "read",
        json!({"path": outside_file.to_string_lossy()}),
    )
    .await?;
    assert!(matches!(
        denied_reply.result,
        Err(ToolErrorPayload::Recoverable(_))
    ));
    Ok(())
}

async fn assert_envelope_parity(
    client: &async_nats::Client,
    instance_id: &ServerScope,
    root_path: &Path,
    read_value: Value,
) -> Result<()> {
    let file_arg = root_path
        .join("round-trip.txt")
        .to_string_lossy()
        .into_owned();
    let root_arg = root_path.to_string_lossy().into_owned();
    let mut direct_allowlist = ResolvedAllowlist::new();
    direct_allowlist.insert_rwx(root_path);
    let direct_server = FsServer::new(direct_allowlist);
    let direct_read = direct_server
        .read_file_impl(serde_json::from_value::<ReadFileParams>(json!({
            "path": file_arg
        }))?)
        .await?;
    let bridged_read_value = serde_json::to_value(direct_read)?;
    assert_eq!(read_value, bridged_read_value);

    let ls_reply = invoke(client, instance_id, "ls", json!({"path": root_arg})).await?;
    let ls_value = ls_reply
        .result
        .map_err(|error| anyhow::anyhow!("ls failed: {error:?}"))?;
    assert_content_envelope(&ls_value);
    let direct_ls = direct_server
        .list_directory_impl(serde_json::from_value::<ListDirectoryParams>(json!({
            "path": root_arg
        }))?)
        .await?;
    let bridged_ls_value = serde_json::to_value(direct_ls)?;
    assert_eq!(ls_value, bridged_ls_value);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_toolset_round_trips_over_nats_with_bridge_envelope_parity() -> Result<()> {
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let root = tempfile::tempdir().context("create filesystem root")?;
    let root_path = root.path().canonicalize()?;
    let mut allowlist = ResolvedAllowlist::new();
    allowlist.insert_rwx(&root_path);
    let toolset = FsToolset::new(allowlist);
    let instance_id = ServerScope::new();
    let server_url = server.url.clone();
    let server_instance_id = instance_id.clone();
    let server_task = tokio::spawn(async move {
        serve_over_nats(toolset, server_instance_id, &server_url, TOKEN).await
    });
    let client = async_nats::ConnectOptions::new()
        .token(TOKEN.to_string())
        .connect(&server.url)
        .await?;

    // If registration never lands, prefer the server's own error over a bare
    // timeout so the real failure cause isn't hidden.
    if let Err(error) = assert_registration(&client, &instance_id).await {
        if server_task.is_finished() {
            return Err(anyhow::anyhow!(
                "toolset server exited before registration: {:?}",
                server_task.await
            ));
        }
        return Err(error);
    }
    let read_value = assert_write_read_round_trip(&client, &instance_id, &root_path).await?;
    assert_out_of_root_denied(&client, &instance_id).await?;
    assert_envelope_parity(&client, &instance_id, &root_path, read_value).await?;

    server_task.abort();
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
