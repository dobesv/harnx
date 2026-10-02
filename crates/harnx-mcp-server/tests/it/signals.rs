//! OS signals must reach the real binary's HTTP shutdown coordinator, not just
//! the library shutdown token. The broker peer leaves a real MCP call pending.
use super::*;
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
};
use std::process::Stdio;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nats_http_binary_sigterm_cancels_and_releases_and_exits_zero() -> Result<()> {
    signal_shutdown("TERM").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nats_http_binary_sigint_cancels_and_releases_and_exits_zero() -> Result<()> {
    signal_shutdown("INT").await
}

fn spawn_server_process(fixture: &Fixture, port: u16) -> Result<tokio::process::Child> {
    Ok(tokio::process::Command::new(binary("harnx-mcp-server")?)
        .args([
            "--mcp-http",
            "--host",
            "127.0.0.1",
            "--cluster",
            "X",
            "--port",
        ])
        .arg(port.to_string())
        .args(["--package", "pkg", "--use-tools", "probe_*"])
        .arg("--config-dir")
        .arg(fixture.config_dir())
        .env_remove("HARNX_MCP_USE_TOOLS")
        .env_remove("HARNX_MCP_PACKAGE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?)
}

async fn wait_for_server_ready(
    child: &mut tokio::process::Child,
    address: std::net::SocketAddr,
) -> Result<()> {
    while tokio::net::TcpStream::connect(address).await.is_err() {
        if child.try_wait()?.is_some() {
            anyhow::bail!("HTTP binary exited before listening");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

async fn await_reserved_event(
    events: &mut async_nats::Subscriber,
    expected_attempt_id: &str,
) -> Result<Reserved> {
    while let Some(message) = events.next().await {
        match serde_json::from_slice::<Reserved>(&message.payload) {
            Ok(reserved) if reserved.attempt_id == expected_attempt_id => return Ok(reserved),
            _ => continue,
        }
    }
    anyhow::bail!("NATS observer closed")
}

async fn deliver_signal(child: &mut tokio::process::Child, signal: &str) -> Result<()> {
    let pid = child.id().context("HTTP binary exited before signal")?;
    let delivered = tokio::process::Command::new("kill")
        .args(["-s", signal, &pid.to_string()])
        .status()
        .await?;
    anyhow::ensure!(
        delivered.success(),
        "failed to send SIG{signal}: {delivered}"
    );
    Ok(())
}

struct ExpectedShutdown<'a> {
    control_subject: &'a str,
    reserved_control_subject: &'a str,
    expected_call_id: &'a str,
    expected_session_id: &'a str,
    expected_reservation_id: &'a str,
}

struct ShutdownEventTracker {
    cancelled: bool,
    released: bool,
}

impl ShutdownEventTracker {
    fn handle_message(
        &mut self,
        message: &async_nats::Message,
        expected: &ExpectedShutdown<'_>,
    ) -> Result<()> {
        let subject = message.subject.as_str();
        if subject == expected.control_subject {
            let cancel: ControlMessage = serde_json::from_slice(&message.payload)?;
            anyhow::ensure!(
                cancel.call_id == expected.expected_call_id,
                "shutdown cancelled wrong call: {cancel:?}"
            );
            anyhow::ensure!(
                cancel.session_id == expected.expected_session_id,
                "shutdown cancelled wrong session: {cancel:?}"
            );
            self.cancelled = true;
        } else if subject == expected.reserved_control_subject {
            anyhow::ensure!(
                !self.released,
                "reservation control published after release"
            );
            if let ToolReservationControl::Release(release) =
                serde_json::from_slice(&message.payload)?
            {
                anyhow::ensure!(
                    release.reservation_id == expected.expected_reservation_id,
                    "shutdown released wrong reservation"
                );
                self.released = true;
            }
        }
        Ok(())
    }
}

async fn await_shutdown_events(
    events: &mut async_nats::Subscriber,
    expected: &ExpectedShutdown<'_>,
) -> Result<()> {
    let mut tracker = ShutdownEventTracker {
        cancelled: false,
        released: false,
    };
    while !tracker.cancelled || !tracker.released {
        let message = events
            .next()
            .await
            .context("NATS observer closed during shutdown")?;
        tracker.handle_message(&message, expected)?;
    }
    Ok(())
}

async fn verify_post_shutdown_events(
    observer: &async_nats::Client,
    events: &mut async_nats::Subscriber,
    reserved_control_subject: &str,
) -> Result<()> {
    let barrier = observer.new_inbox();
    observer
        .publish(barrier.clone(), "shutdown-observed".into())
        .await?;
    observer.flush().await?;
    while let Some(message) = events.next().await {
        if message.subject.as_str() == barrier {
            return Ok(());
        }
        anyhow::ensure!(
            message.subject.as_str() != reserved_control_subject,
            "reservation control published after release: {message:?}"
        );
    }
    anyhow::bail!("NATS observer closed after exit")
}

struct PendingCall {
    client: rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    reserve: Reserve,
    reserved: Reserved,
    request: ToolRequest,
    pending: AbortOnDropHandle<std::result::Result<CallToolResult, rmcp::ServiceError>>,
}

async fn open_pending_call(
    address: std::net::SocketAddr,
    peer: &mut Peer,
    events: &mut async_nats::Subscriber,
) -> Result<PendingCall> {
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::builder().no_proxy().build()?,
        StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp")),
    );
    let client = ().serve(transport).await?;
    let tools = client.list_tools(None).await?;
    anyhow::ensure!(
        tools.tools.len() == 1 && tools.tools[0].name == "probe_echo",
        "unexpected catalog: {tools:?}"
    );
    let reserve = recv(&mut peer.reserves).await?;
    let reserved = await_reserved_event(events, &reserve.attempt_id).await?;

    let pending = AbortOnDropHandle::new(tokio::spawn({
        let client = client.peer().clone();
        async move {
            client
                .call_tool(CallToolRequestParams::new("probe_echo"))
                .await
        }
    }));
    let (_, request, _unanswered) = recv(&mut peer.calls).await?;
    anyhow::ensure!(
        request.parent_session_id.as_deref() == Some(reserve.session_storage_key.as_str()),
        "pending call used wrong backing session"
    );
    anyhow::ensure!(!pending.is_finished(), "call completed before signal");
    anyhow::ensure!(
        peer.cancels.try_recv().is_err() && peer.releases.try_recv().is_err(),
        "cleanup started before signal"
    );

    Ok(PendingCall {
        client,
        reserve,
        reserved,
        request,
        pending,
    })
}

async fn signal_shutdown(signal: &str) -> Result<()> {
    let fixture = Fixture::new()?;
    let broker = harnx_test_bins::spawn_nats_server()
        .await?
        .context("nats-server required for OS signal tests")?;
    fixture.cluster("X", broker.url())?;
    let mut peer =
        Peer::with_recovery_gate(async_nats::connect(broker.url()).await?, false).await?;
    let observer = async_nats::connect(broker.url()).await?;
    let mut events = observer.subscribe(">").await?;
    observer.flush().await?;

    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    let mut child = spawn_server_process(&fixture, address.port())?;

    let result = tokio::time::timeout(DEADLINE, async {
        wait_for_server_ready(&mut child, address).await?;
        let PendingCall {
            client,
            reserve,
            reserved,
            request,
            pending,
        } = open_pending_call(address, &mut peer, &mut events).await?;

        deliver_signal(&mut child, signal).await?;

        let expected = ExpectedShutdown {
            control_subject: &peer.scopes[0].control_subject(),
            reserved_control_subject: &reserved.control_subject,
            expected_call_id: &request.call_id,
            expected_session_id: &reserve.session_storage_key,
            expected_reservation_id: &reserved.reservation_id,
        };
        await_shutdown_events(&mut events, &expected).await?;

        let release = recv(&mut peer.releases).await?;
        anyhow::ensure!(
            release.reservation_id == reserved.reservation_id,
            "peer did not receive release"
        );
        let status = child.wait().await?;
        anyhow::ensure!(
            status.code() == Some(0),
            "SIG{signal} shutdown should exit 0: {status}"
        );

        verify_post_shutdown_events(&observer, &mut events, &reserved.control_subject).await?;
        anyhow::ensure!(
            peer.reserves.try_recv().is_err(),
            "shutdown opened another reservation"
        );
        client.cancel().await?;
        let _ = pending.await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("OS signal shutdown exceeded deadline")
    .and_then(|result| result);

    if result.is_err() && child.try_wait()?.is_none() {
        child
            .kill()
            .await
            .context("kill HTTP binary after failed signal test")?;
    }
    result.with_context(|| format!("actual HTTP binary SIG{signal} shutdown"))
}
