//! Real rmcp transports. Test protocol peer exposes release/cancellation barriers;
//! real local-worker/tool semantics remain covered by handler tests and D1.
use super::*;
use harnx_mcp_server::transport::run_http;
use rmcp::{
    service::{RoleClient, RunningService},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ClientHandler,
};
use std::process::Stdio;

struct Client;
impl ClientHandler for Client {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("transport-test", "1"),
        )
        .with_protocol_version(ProtocolVersion::V_2025_11_25)
    }
}

async fn transport_setup(
    fixture: &Fixture,
) -> Result<(harnx_test_bins::NatsServerHandle, Arc<Bootstrap>, Peer)> {
    let broker = harnx_test_bins::spawn_nats_server()
        .await?
        .context("nats-server required")?;
    fixture.cluster("X", broker.url())?;
    let bootstrap = Arc::new(fixture.bootstrap(Some("X")).await?);
    let peer = Peer::with_recovery_gate(async_nats::connect(broker.url()).await?, false).await?;
    Ok((broker, bootstrap, peer))
}

async fn http_client(url: &str) -> Result<RunningService<RoleClient, Client>> {
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::builder().no_proxy().build()?,
        StreamableHttpClientTransportConfig::with_uri(url),
    );
    Ok(tokio::time::timeout(DEADLINE, Client.serve(transport)).await??)
}

async fn start_http(
    bootstrap: Arc<Bootstrap>,
    selectors: ToolReservationView,
    idle: Option<Duration>,
) -> Result<(String, CancellationToken, AbortOnDropHandle<Result<()>>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    let shutdown = CancellationToken::new();
    let mut config =
        rmcp::transport::streamable_http_server::session::local::SessionConfig::default();
    if let Some(idle) = idle {
        config.keep_alive = Some(idle);
    }
    let task = AbortOnDropHandle::new(tokio::spawn(run_http(
        bootstrap,
        selectors,
        harnx_mcp_server::transport::HttpOptions {
            listener,
            session_config: config,
            shutdown: shutdown.clone(),
        },
    )));
    Ok((url, shutdown, task))
}

async fn wait_for_release_control(peer: &mut Peer, expected_reservation_id: &str) -> Result<()> {
    loop {
        match recv(&mut peer.controls).await? {
            ToolReservationControl::Release(release)
                if release.reservation_id == expected_reservation_id =>
            {
                return Ok(());
            }
            _ => {}
        }
    }
}

fn assert_control_not_renewed(control: &ToolReservationControl, reservation_id: &str) {
    if let ToolReservationControl::Renew(renew) = control {
        assert_ne!(
            renew.reservation_id, reservation_id,
            "renewal after release"
        );
    }
}

async fn no_renew_after_release(peer: &mut Peer, reservation_id: &str) -> Result<()> {
    wait_for_release_control(peer, reservation_id).await?;
    // Three renewal periods. Other still-live sessions may continue renewing.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(350);
    while let Ok(Some(control)) = tokio::time::timeout_at(deadline, peer.controls.recv()).await {
        assert_control_not_renewed(&control, reservation_id);
    }
    Ok(())
}

fn spawn_stdio_server(fixture: &Fixture) -> Result<tokio::process::Child> {
    Ok(tokio::process::Command::new(binary("harnx-mcp-server")?)
        .args([
            "--mcp-stdio",
            "--cluster",
            "X",
            "--package",
            "pkg",
            "--use-tools",
            "probe_*",
        ])
        .arg("--config-dir")
        .arg(fixture.config_dir())
        .env_remove("HARNX_MCP_USE_TOOLS")
        .env_remove("HARNX_MCP_PACKAGE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?)
}

async fn verify_stdio_successful_call(
    client: &RunningService<RoleClient, Client>,
    peer: &mut Peer,
    reserve: &Reserve,
) -> Result<()> {
    let call = tokio::spawn({
        let client = client.peer().clone();
        async move {
            client
                .call_tool(CallToolRequestParams::new("probe_echo"))
                .await
        }
    });
    let (_, request, message) = recv(&mut peer.calls).await?;
    assert_eq!(
        request.parent_session_id.as_deref(),
        Some(reserve.session_storage_key.as_str())
    );
    peer.reply(&request, message, "stdio success").await?;
    assert!(serde_json::to_value(call.await??)?
        .to_string()
        .contains("stdio success"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdio_binary_serves_and_eof_cancels_drains_releases() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_broker, _bootstrap, mut peer) = transport_setup(&fixture).await?;
    let mut child = spawn_stdio_server(&fixture)?;
    let client = tokio::time::timeout(
        DEADLINE,
        Client.serve((child.stdout.take().unwrap(), child.stdin.take().unwrap())),
    )
    .await??;
    assert!(client.peer_info().unwrap().capabilities.tools.is_some());
    let tools = client.list_tools(None).await?;
    assert_eq!(
        tools
            .tools
            .iter()
            .map(|t| t.name.as_ref())
            .collect::<Vec<_>>(),
        ["probe_echo"]
    );
    let reserve = recv(&mut peer.reserves).await?;
    verify_stdio_successful_call(&client, &mut peer, &reserve).await?;

    let pending = tokio::spawn({
        let client = client.peer().clone();
        async move {
            client
                .call_tool(CallToolRequestParams::new("probe_echo"))
                .await
        }
    });
    let (_, pending_request, _) = recv(&mut peer.calls).await?;
    // Closing the real stdin pipe is EOF, not process kill.
    client.cancel().await?;
    let cancel = serde_json::to_value(recv(&mut peer.cancels).await?)?;
    assert_eq!(cancel["call_id"], pending_request.call_id);
    assert!(pending.await?.is_err());
    assert_eq!(
        recv(&mut peer.releases).await?.reservation_id,
        "reservation-0"
    );
    let status = tokio::time::timeout(DEADLINE, child.wait()).await??;
    assert!(
        status.success(),
        "stdio EOF should exit successfully: {status}"
    );
    no_renew_after_release(&mut peer, "reservation-0").await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_two_sessions_isolated_delete_cancels_only_owner() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_broker, bootstrap, mut peer) = transport_setup(&fixture).await?;
    let (url, shutdown, server) = start_http(bootstrap.clone(), probe_view(), None).await?;
    let first = http_client(&url).await?;
    let second = http_client(&url).await?;
    assert_eq!(first.list_tools(None).await?.tools.len(), 2);
    let one = recv(&mut peer.reserves).await?;
    assert_eq!(second.list_tools(None).await?.tools.len(), 2);
    let two = recv(&mut peer.reserves).await?;
    assert_ne!(one.session_storage_key, two.session_storage_key);
    let pending = tokio::spawn({
        let client = first.peer().clone();
        async move {
            client
                .call_tool(CallToolRequestParams::new("probe_echo"))
                .await
        }
    });
    let (_, request, _) = recv(&mut peer.calls).await?;
    assert_eq!(
        request.parent_session_id.as_deref(),
        Some(one.session_storage_key.as_str())
    );
    // rmcp Streamable HTTP transport cleanup sends a real DELETE /mcp.
    first.cancel().await?;
    assert_eq!(
        serde_json::to_value(recv(&mut peer.cancels).await?)?["call_id"],
        request.call_id
    );
    assert!(pending.await?.is_err());
    assert_eq!(
        recv(&mut peer.releases).await?.reservation_id,
        "reservation-0"
    );
    no_renew_after_release(&mut peer, "reservation-0").await?;
    let live = tokio::spawn({
        let client = second.peer().clone();
        async move {
            client
                .call_tool(CallToolRequestParams::new("probe_echo"))
                .await
        }
    });
    let (_, request, message) = recv(&mut peer.calls).await?;
    assert_eq!(
        request.parent_session_id.as_deref(),
        Some(two.session_storage_key.as_str())
    );
    peer.reply(&request, message, "second session survives DELETE")
        .await?;
    assert!(serde_json::to_value(live.await??)?
        .to_string()
        .contains("survives DELETE"));
    second.cancel().await?;
    assert_eq!(
        recv(&mut peer.releases).await?.reservation_id,
        "reservation-1"
    );
    shutdown.cancel();
    tokio::time::timeout(DEADLINE, server).await???;
    assert!(bootstrap.config().session.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_idle_expiry_releases_and_process_bootstrap_survives() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_broker, bootstrap, mut peer) = transport_setup(&fixture).await?;
    let (url, shutdown, server) = start_http(
        bootstrap.clone(),
        probe_view(),
        Some(Duration::from_secs(1)),
    )
    .await?;
    let first = http_client(&url).await?;
    first.list_tools(None).await?;
    let one = recv(&mut peer.reserves).await?;
    let pending = tokio::spawn({
        let client = first.peer().clone();
        async move {
            client
                .call_tool(CallToolRequestParams::new("probe_echo"))
                .await
        }
    });
    let (_, request, _) = recv(&mut peer.calls).await?;
    // Leave the client alive without DELETE. LocalSessionManager inactivity
    // ends its worker, which must release despite handler/request Arc ownership.
    assert_eq!(
        recv(&mut peer.releases).await?.reservation_id,
        "reservation-0"
    );
    assert_eq!(
        serde_json::to_value(recv(&mut peer.cancels).await?)?["call_id"],
        request.call_id
    );
    no_renew_after_release(&mut peer, "reservation-0").await?;
    first.cancel().await?;
    assert!(pending.await?.is_err());
    let second = http_client(&url).await?;
    assert_eq!(second.list_tools(None).await?.tools.len(), 2);
    let two = recv(&mut peer.reserves).await?;
    assert_ne!(one.session_storage_key, two.session_storage_key);
    // Server shutdown also owns cleanup for sessions clients haven't deleted.
    shutdown.cancel();
    tokio::time::timeout(DEADLINE, server).await???;
    assert_eq!(
        recv(&mut peer.releases).await?.reservation_id,
        "reservation-1"
    );
    second.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdio_binary_nonmatching_selectors_serves_empty_catalog() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_broker, _bootstrap, mut peer) = transport_setup(&fixture).await?;
    let mut child = tokio::process::Command::new(binary("harnx-mcp-server")?)
        .args([
            "--mcp-stdio",
            "--cluster",
            "X",
            "--use-tools",
            "does_not_match_*",
        ])
        .arg("--config-dir")
        .arg(fixture.config_dir())
        .env_remove("HARNX_MCP_USE_TOOLS")
        .env_remove("HARNX_MCP_PACKAGE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let client = tokio::time::timeout(
        DEADLINE,
        Client.serve((child.stdout.take().unwrap(), child.stdin.take().unwrap())),
    )
    .await??;
    assert!(client.list_tools(None).await?.tools.is_empty());
    assert_eq!(
        recv(&mut peer.reserves).await?.view.use_tools,
        ["does_not_match_*"]
    );
    client.cancel().await?;
    assert_eq!(
        recv(&mut peer.releases).await?.reservation_id,
        "reservation-0"
    );
    assert!(tokio::time::timeout(DEADLINE, child.wait())
        .await??
        .success());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_rejects_stateless_protocol_without_opening_reservation() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_broker, bootstrap, mut peer) = transport_setup(&fixture).await?;
    let (url, shutdown, server) = start_http(bootstrap, probe_view(), None).await?;
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::builder().no_proxy().build()?,
        StreamableHttpClientTransportConfig::with_uri(url.as_str()),
    );
    assert!(
        tokio::time::timeout(
            DEADLINE,
            rmcp::serve_client_with_lifecycle(
                (),
                transport,
                rmcp::ClientLifecycleMode::Discover {
                    preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                }
            )
        )
        .await?
        .is_err(),
        "2026-07-28 cannot create a stateful session"
    );
    assert!(peer.reserves.try_recv().is_err());
    shutdown.cancel();
    tokio::time::timeout(DEADLINE, server).await???;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_serving_future_drop_cancels_and_releases() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_broker, bootstrap, mut peer) = transport_setup(&fixture).await?;
    let (url, _shutdown, server) = start_http(bootstrap.clone(), probe_view(), None).await?;
    let client = http_client(&url).await?;
    client.list_tools(None).await?;
    recv(&mut peer.reserves).await?;
    let pending = tokio::spawn({
        let peer = client.peer().clone();
        async move {
            peer.call_tool(CallToolRequestParams::new("probe_echo"))
                .await
        }
    });
    let (_, request, _) = recv(&mut peer.calls).await?;
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    assert_eq!(
        serde_json::to_value(recv(&mut peer.cancels).await?)?["call_id"],
        request.call_id
    );
    assert_eq!(
        recv(&mut peer.releases).await?.reservation_id,
        "reservation-0"
    );
    no_renew_after_release(&mut peer, "reservation-0").await?;
    client.cancel().await?;
    assert!(pending.await?.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_binary_nonmatching_selectors_serves_empty_catalog_delete_releases() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_broker, _bootstrap, mut peer) = transport_setup(&fixture).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    drop(listener);
    let mut child = tokio::process::Command::new(binary("harnx-mcp-server")?)
        .args(["--mcp-http", "--cluster", "X", "--port"])
        .arg(address.port().to_string())
        .args(["--use-tools", "does_not_match_*"])
        .arg("--config-dir")
        .arg(fixture.config_dir())
        .env_remove("HARNX_MCP_USE_TOOLS")
        .env_remove("HARNX_MCP_PACKAGE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    tokio::time::timeout(DEADLINE, async {
        while tokio::net::TcpStream::connect(address).await.is_err() {
            anyhow::ensure!(
                child.try_wait()?.is_none(),
                "HTTP binary exited before listening"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let client = http_client(&format!("http://{address}/mcp")).await?;
    assert!(client.list_tools(None).await?.tools.is_empty());
    recv(&mut peer.reserves).await?;
    client.cancel().await?;
    assert_eq!(
        recv(&mut peer.releases).await?.reservation_id,
        "reservation-0"
    );
    anyhow::ensure!(
        child.try_wait()?.is_none(),
        "HTTP server shouldn't exit after DELETE"
    );
    child.kill().await?;
    Ok(())
}
