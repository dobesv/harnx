use super::*;
use crate::bridge_roundtrip::tool_request;
use harnx_core::instance::ServerScope;
use harnx_toolset::{server_identity_token, ToolReply, HDR_CALL_ID, HDR_IDEMPOTENCY_KEY};
use std::{path::Path, process::Stdio, time::Duration};

// Nextest kills stalled tests after four 60s slow-timeout periods; keep this backstop below that.
const PROCESS_TEST_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) fn start(
    url: &str,
    scope: &ServerScope,
    script: &Path,
) -> Result<tokio::process::Child> {
    let binary = Path::new(env!("CARGO_BIN_EXE_harnx-mcp-bridge"))
        .parent()
        .unwrap()
        .join("harnx-mock-mcp");
    Ok(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_harnx-mcp-bridge"))
            .args(["--name", "limits", "--max-result-bytes", "2048", "--"])
            .arg(binary)
            .arg("--script")
            .arg(script)
            .env("HARNX_SERVER_SCOPE", scope.as_str())
            .env("HARNX_NATS_URL", url)
            .env("HARNX_NATS_TOKEN", TOKEN)
            .env("HARNX_MCP_MAX_RESULT_BYTES", "4096")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()?,
    )
}

pub(super) async fn invoke(client: &async_nats::Client, scope: &ServerScope) -> Result<Value> {
    let call_id = "bounded-process-call";
    let mut request = tool_request(call_id, "large");
    request.parent_agent = session().agent;
    request.parent_local_session_id = Some(session().session_id);
    request.parent_session_id = Some(session().owner());
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(HDR_CALL_ID, call_id);
    headers.insert(HDR_IDEMPOTENCY_KEY, call_id);
    let identity = server_identity_token(None, "", "limits");
    let message = tokio::time::timeout(
        PROCESS_TEST_TIMEOUT,
        client.request_with_headers(
            scope.tool_subject(&identity, "large"),
            headers,
            serde_json::to_vec(&request)?.into(),
        ),
    )
    .await??;
    let reply: ToolReply = serde_json::from_slice(&message.payload)?;
    reply.result.map_err(|error| anyhow::anyhow!("{error:?}"))
}

pub(super) async fn stop(child: &mut tokio::process::Child) -> Result<()> {
    let pid = child.id().context("bridge child PID")?;
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) }, 0);
    assert!(tokio::time::timeout(PROCESS_TEST_TIMEOUT, child.wait())
        .await??
        .success());
    Ok(())
}
