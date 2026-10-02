#[cfg(unix)]
#[path = "signals.rs"]
mod signals;
#[path = "transports.rs"]
mod transports;

use super::*;
use harnx_core::instance::ServerScope;
use harnx_mcp_server::connection::Connection;
use harnx_mcp_server::handler::McpHandler;
use harnx_toolset::{
    server_identity_token, ControlMessage, Registration, ToolReply, ToolRequest, ToolSpec,
};
use harnx_toolset_server::{
    registration_key, TOOL_PROTOCOL_VERSION, TOOL_REGISTRY_BUCKET, TOOL_SCHEMA_VERSION,
};
use rmcp::{model::*, service::PeerRequestOptions, ServerHandler, ServiceExt};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;

async fn recv<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> Result<T> {
    tokio::time::timeout(DEADLINE, receiver.recv())
        .await?
        .context("test peer channel closed")
}

// Test-only protocol peer: controls recovery barriers and leaves calls pending
// so snapshot routing/cancel behavior can be asserted without timing guesses.
struct Peer {
    client: async_nats::Client,
    scopes: [ServerScope; 2],
    invalid: Arc<AtomicBool>,
    gate: Arc<Notify>,
    reserves: mpsc::UnboundedReceiver<Reserve>,
    calls: mpsc::UnboundedReceiver<(usize, ToolRequest, async_nats::Message)>,
    cancels: mpsc::UnboundedReceiver<ControlMessage>,
    releases: mpsc::UnboundedReceiver<Release>,
    controls: mpsc::UnboundedReceiver<ToolReservationControl>,
    _tasks: Vec<AbortOnDropHandle<()>>,
}

struct ReserveScript {
    scopes: [ServerScope; 2],
    control: String,
    requests: mpsc::UnboundedSender<Reserve>,
    gate: Arc<Notify>,
    recovery_gate: bool,
}

struct ControlEvents {
    controls: mpsc::UnboundedSender<ToolReservationControl>,
    releases: mpsc::UnboundedSender<Release>,
}

impl Peer {
    async fn new(client: async_nats::Client) -> Result<Self> {
        Self::with_recovery_gate(client, true).await
    }

    async fn with_recovery_gate(client: async_nats::Client, recovery_gate: bool) -> Result<Self> {
        let scopes = [ServerScope::new(), ServerScope::new()];
        let (reserve_tx, reserves) = mpsc::unbounded_channel();
        let (call_tx, calls) = mpsc::unbounded_channel();
        let (cancel_tx, cancels) = mpsc::unbounded_channel();
        let (release_tx, releases) = mpsc::unbounded_channel();
        let (control_tx, controls) = mpsc::unbounded_channel();
        let invalid = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Notify::new());
        let control = client.new_inbox();

        let mut tasks = Vec::new();
        tasks.push(
            Self::spawn_reserve_handler(
                &client,
                ReserveScript {
                    scopes: scopes.clone(),
                    control: control.clone(),
                    requests: reserve_tx,
                    gate: gate.clone(),
                    recovery_gate,
                },
            )
            .await?,
        );
        tasks.push(
            Self::spawn_control_handler(
                &client,
                &control,
                ControlEvents {
                    controls: control_tx,
                    releases: release_tx,
                },
                invalid.clone(),
            )
            .await?,
        );
        tasks.extend(
            Self::register_and_subscribe_scopes(&client, &scopes, call_tx, cancel_tx).await?,
        );
        client.flush().await?;

        Ok(Self {
            client,
            scopes,
            invalid,
            gate,
            reserves,
            calls,
            cancels,
            releases,
            controls,
            _tasks: tasks,
        })
    }

    async fn spawn_reserve_handler(
        client: &async_nats::Client,
        script: ReserveScript,
    ) -> Result<AbortOnDropHandle<()>> {
        let mut reserve_sub = client.subscribe(reserve_subject("X")).await?;
        let client = client.clone();
        let ReserveScript {
            scopes,
            control,
            requests: reserve_tx,
            gate,
            recovery_gate,
        } = script;
        let next = AtomicUsize::new(0);
        Ok(AbortOnDropHandle::new(tokio::spawn(async move {
            while let Some(message) = reserve_sub.next().await {
                let request: Reserve = serde_json::from_slice(&message.payload).unwrap();
                reserve_tx.send(request.clone()).unwrap();
                let reservation_index = next.fetch_add(1, Ordering::SeqCst);
                let index = reservation_index.min(1);
                if recovery_gate && index == 1 {
                    gate.notified().await;
                }
                let reply = Reserved {
                    protocol_version: TOOL_RESERVATION_PROTOCOL_VERSION,
                    attempt_id: request.attempt_id,
                    reservation_id: format!("reservation-{reservation_index}"),
                    worker_id: format!("worker-{index}"),
                    server_scope: scopes[index].to_string(),
                    control_subject: control.clone(),
                    ttl_ms: 3000,
                    renew_after_ms: 100,
                };
                client
                    .publish(
                        message.reply.unwrap(),
                        serde_json::to_vec(&reply).unwrap().into(),
                    )
                    .await
                    .unwrap();
            }
        })))
    }

    async fn spawn_control_handler(
        client: &async_nats::Client,
        control: &str,
        events: ControlEvents,
        invalid: Arc<AtomicBool>,
    ) -> Result<AbortOnDropHandle<()>> {
        let mut control_sub = client.subscribe(control.to_owned()).await?;
        let client = client.clone();
        let ControlEvents {
            controls: control_tx,
            releases: release_tx,
        } = events;
        Ok(AbortOnDropHandle::new(tokio::spawn(async move {
            while let Some(message) = control_sub.next().await {
                let request: ToolReservationControl =
                    serde_json::from_slice(&message.payload).unwrap();
                control_tx.send(request.clone()).unwrap();
                let reply = match request {
                    ToolReservationControl::Release(release) => {
                        release_tx.send(release).unwrap();
                        ToolReservationControlReply::Ok(ToolReservationOk::Ok)
                    }
                    ToolReservationControl::Renew(_) if invalid.load(Ordering::SeqCst) => {
                        ToolReservationControlReply::Error(ToolReservationError {
                            code: ToolReservationErrorCode::UnknownOrExpired,
                            message: "test scope invalidated".to_owned(),
                        })
                    }
                    _ => ToolReservationControlReply::Ok(ToolReservationOk::Ok),
                };
                client
                    .publish(
                        message.reply.unwrap(),
                        serde_json::to_vec(&reply).unwrap().into(),
                    )
                    .await
                    .unwrap();
            }
        })))
    }

    async fn register_probe_package(
        store: &async_nats::jetstream::kv::Store,
        scope: &ServerScope,
        package: Option<&str>,
        index: usize,
    ) -> Result<String> {
        let server = "probe";
        let registration = Registration {
            package: package.map(str::to_owned),
            config: "probe".to_owned(),
            server: server.to_owned(),
            schema_version: TOOL_SCHEMA_VERSION,
            proto_version: TOOL_PROTOCOL_VERSION,
            tools: vec![ToolSpec {
                name: "echo".to_owned(),
                description: format!("scope {index}"),
                input_schema: json!({"type":"object", "properties":{}}),
                cancellation_guarantee: Default::default(),
                idempotent_hint: false,
                read_only_hint: true,
                timeout_secs: Some(0),
                meta: None,
            }],
        };
        let identity = server_identity_token(package, "probe", server);
        store
            .put(
                registration_key(scope, &identity),
                serde_json::to_vec(&registration)?.into(),
            )
            .await?;
        Ok(identity)
    }

    async fn register_and_subscribe_scopes(
        client: &async_nats::Client,
        scopes: &[ServerScope; 2],
        call_tx: mpsc::UnboundedSender<(usize, ToolRequest, async_nats::Message)>,
        cancel_tx: mpsc::UnboundedSender<ControlMessage>,
    ) -> Result<Vec<AbortOnDropHandle<()>>> {
        let mut tasks = Vec::new();
        let store = async_nats::jetstream::new(client.clone())
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: TOOL_REGISTRY_BUCKET.to_owned(),
                ..Default::default()
            })
            .await?;
        for (index, scope) in scopes.iter().enumerate() {
            for package in [Some("pkg"), Some("other")] {
                let identity = Self::register_probe_package(&store, scope, package, index).await?;
                let mut sub = client
                    .subscribe(scope.tool_subject(&identity, "echo"))
                    .await?;
                let tx = call_tx.clone();
                tasks.push(AbortOnDropHandle::new(tokio::spawn(async move {
                    while let Some(message) = sub.next().await {
                        let request = serde_json::from_slice(&message.payload).unwrap();
                        tx.send((index, request, message)).unwrap();
                    }
                })));
            }
            let mut sub = client.subscribe(scope.control_subject()).await?;
            let tx = cancel_tx.clone();
            tasks.push(AbortOnDropHandle::new(tokio::spawn(async move {
                while let Some(message) = sub.next().await {
                    let control = serde_json::from_slice(&message.payload).unwrap();
                    tx.send(control).unwrap();
                }
            })));
        }
        Ok(tasks)
    }

    async fn reply(
        &self,
        request: &ToolRequest,
        message: async_nats::Message,
        marker: &str,
    ) -> Result<()> {
        let reply = ToolReply {
            call_id: request.call_id.clone(),
            result: Ok(json!({"content":[{"type":"text", "text":marker}], "isError":false})),
            final_progress: None,
        };
        self.client
            .publish(
                message.reply.context("call reply missing")?,
                serde_json::to_vec(&reply)?.into(),
            )
            .await?;
        Ok(())
    }
}

async fn setup(
    fixture: &Fixture,
) -> Result<(harnx_test_bins::NatsServerHandle, Arc<Bootstrap>, Peer)> {
    let broker = harnx_test_bins::spawn_nats_server()
        .await?
        .context("nats-server required for MCP handler tests")?;
    fixture.cluster("X", broker.url())?;
    let bootstrap = Arc::new(fixture.bootstrap(Some("X")).await?);
    let peer = Peer::new(async_nats::connect(broker.url()).await?).await?;
    Ok((broker, bootstrap, peer))
}

fn probe_view() -> ToolReservationView {
    ToolReservationView {
        package: Some("pkg".to_owned()),
        use_tools: vec!["probe_*".to_owned(), "other__probe_*".to_owned()],
    }
}

async fn assert_whitelist_and_initial_tools(connection: &Connection) -> Result<()> {
    let tools = connection.list_tools().await?;
    assert_eq!(
        tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>(),
        ["other__probe_echo", "probe_echo"]
    );
    assert_eq!(tools[1].description.as_deref(), Some("scope 0"));
    for forbidden in [
        "echo",
        "probe",
        "pkg__probe_echo",
        "session_history",
        "agent_session_handoff",
    ] {
        let error = connection
            .call_tool(forbidden.to_owned(), json!({}), CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
    }
    Ok(())
}

async fn verify_recovery_replacement_and_no_replay(
    connection: Arc<Connection>,
    peer: &mut Peer,
    old: tokio::task::JoinHandle<std::result::Result<CallToolResult, ErrorData>>,
    old_request: ToolRequest,
    old_message: async_nats::Message,
) -> Result<()> {
    let fresh = tokio::spawn({
        let connection = connection.clone();
        async move {
            tokio::time::timeout(DEADLINE, async {
                loop {
                    match connection
                        .call_tool(
                            "other__probe_echo".to_owned(),
                            json!({}),
                            CancellationToken::new(),
                        )
                        .await
                    {
                        Err(error) if error.message.contains("reservation unavailable") => {
                            tokio::time::sleep(Duration::from_millis(10)).await
                        }
                        result => return result,
                    }
                }
            })
            .await
            .unwrap()
        }
    });
    let (index, new_request, new_message) = recv(&mut peer.calls).await?;
    assert_eq!(index, 1, "stale provider used after generation change");
    assert_ne!(old_request.call_id, new_request.call_id);
    assert_ne!(old_request.tool_call_id, new_request.tool_call_id);
    peer.reply(&new_request, new_message, "new scope").await?;
    assert_eq!(fresh.await??.is_error, Some(false));
    assert_eq!(
        recv(&mut peer.releases).await?.reservation_id,
        "reservation-0"
    );
    peer.reply(&old_request, old_message, "old scope completed once")
        .await?;
    assert_eq!(
        serde_json::to_value(old.await??)?["content"][0]["text"],
        "old scope completed once"
    );
    assert!(peer.calls.try_recv().is_err(), "in-flight call replayed");
    assert_ne!(peer.scopes[0], peer.scopes[1]);
    Ok(())
}

async fn verify_dynamic_unregistration_and_teardown(
    connection: &Connection,
    handler: &McpHandler,
    bootstrap: &Bootstrap,
    peer: &mut Peer,
) -> Result<()> {
    let store = async_nats::jetstream::new(peer.client.clone())
        .get_key_value(TOOL_REGISTRY_BUCKET)
        .await?;
    store
        .delete(registration_key(
            &peer.scopes[1],
            &server_identity_token(Some("other"), "probe", "probe"),
        ))
        .await?;
    let tools = connection.list_tools().await?;
    assert_eq!(
        tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>(),
        ["probe_echo"]
    );
    assert!(connection
        .call_tool(
            "other__probe_echo".to_owned(),
            json!({}),
            CancellationToken::new()
        )
        .await
        .is_err());
    handler.close().await?;
    assert_eq!(
        recv(&mut peer.releases).await?.reservation_id,
        "reservation-1"
    );
    handler.close().await?;
    assert!(connection.list_tools().await.is_err());
    assert!(bootstrap.config().session.is_none());
    assert!(bootstrap.config().nats_tool_declarations.read().is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broker_snapshot_whitelist_scope_recovery_never_replays_in_flight() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_broker, bootstrap, mut peer) = setup(&fixture).await?;
    let handler = McpHandler::new(bootstrap.clone(), probe_view());
    let connection = handler.connection();
    assert!(handler.get_info().capabilities.tools.is_some());
    assert!(connection.session_id().await.is_none());
    assert_whitelist_and_initial_tools(&connection).await?;
    let first_reserve = recv(&mut peer.reserves).await?;
    assert_eq!(first_reserve.view, probe_view());
    assert!(peer.calls.try_recv().is_err());

    let old = tokio::spawn({
        let connection = connection.clone();
        async move {
            connection
                .call_tool(
                    "probe_echo".to_owned(),
                    json!({"marker":"old"}),
                    CancellationToken::new(),
                )
                .await
        }
    });
    let (index, old_request, old_message) = recv(&mut peer.calls).await?;
    assert_eq!(index, 0);
    assert_eq!(old_request.args, json!({"marker":"old"}));
    assert_eq!(
        old_request.parent_local_session_id.as_deref(),
        connection.session_id().await.as_deref()
    );
    assert_eq!(
        old_request.parent_session_id.as_deref(),
        Some(first_reserve.session_storage_key.as_str())
    );
    assert!(old_request.tool_call_id.is_some());
    peer.invalid.store(true, Ordering::SeqCst);
    let second_reserve = recv(&mut peer.reserves).await?;
    assert_ne!(first_reserve.attempt_id, second_reserve.attempt_id);
    assert_eq!(
        first_reserve.session_storage_key,
        second_reserve.session_storage_key
    );
    let unavailable = connection
        .call_tool("probe_echo".to_owned(), json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        unavailable.message.contains("reservation unavailable"),
        "{unavailable}"
    );
    assert!(
        peer.calls.try_recv().is_err(),
        "invalid scope admitted a call"
    );
    peer.invalid.store(false, Ordering::SeqCst);
    peer.gate.notify_one();

    verify_recovery_replacement_and_no_replay(
        connection.clone(),
        &mut peer,
        old,
        old_request,
        old_message,
    )
    .await?;
    verify_dynamic_unregistration_and_teardown(&connection, &handler, &bootstrap, &mut peer)
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broker_mcp_cancel_and_close_reach_unique_call_ids() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_broker, bootstrap, mut peer) = setup(&fixture).await?;
    let handler = McpHandler::new(bootstrap, probe_view());
    let connection = handler.connection();
    let (client_io, server_io) = tokio::io::duplex(65536);
    let server = tokio::spawn(async move { handler.serve(server_io).await.unwrap() });
    let client = ().serve(client_io).await?;
    let server = server.await?;
    assert!(
        connection.session_id().await.is_none(),
        "initialize opened reservation"
    );
    let tools = client.list_all_tools().await?;
    assert_eq!(tools.len(), 2);
    let pending = client
        .send_request_with_option(
            ClientRequest::CallToolRequest(CallToolRequest::new(
                CallToolRequestParams::new("probe_echo")
                    .with_arguments(json!({}).as_object().unwrap().clone()),
            )),
            PeerRequestOptions::no_options(),
        )
        .await?;
    let (_, request, _) = recv(&mut peer.calls).await?;
    client
        .notify_cancelled(CancelledNotificationParam::new(
            Some(pending.id.clone()),
            Some("test cancel".to_owned()),
        ))
        .await?;
    let cancel = recv(&mut peer.cancels).await?;
    let wire = serde_json::to_value(cancel)?;
    assert_eq!(wire["call_id"], request.call_id);
    assert_eq!(wire["session_id"], request.parent_session_id.unwrap());
    let _ = pending.await_response().await;

    let pending = client
        .send_request_with_option(
            ClientRequest::CallToolRequest(CallToolRequest::new(CallToolRequestParams::new(
                "probe_echo",
            ))),
            PeerRequestOptions::no_options(),
        )
        .await?;
    let (_, next_request, _) = recv(&mut peer.calls).await?;
    assert_ne!(request.call_id, next_request.call_id);
    connection.close().await?;
    let cancel = serde_json::to_value(recv(&mut peer.cancels).await?)?;
    assert_eq!(cancel["call_id"], next_request.call_id);
    assert_eq!(
        recv(&mut peer.releases).await?.reservation_id,
        "reservation-0"
    );
    let result = pending.await_response().await?;
    let ServerResult::CallToolResult(result) = result else {
        anyhow::bail!("unexpected result: {result:?}")
    };
    assert_eq!(result.is_error, Some(true));
    client.cancel().await?;
    server.cancel().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broker_dropped_request_cancels_and_last_handler_drop_releases() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_broker, bootstrap, mut peer) = setup(&fixture).await?;
    let handler = McpHandler::new(bootstrap, probe_view());
    let connection = handler.connection();
    let task = tokio::spawn({
        let connection = connection.clone();
        async move {
            connection
                .call_tool("probe_echo".to_owned(), json!({}), CancellationToken::new())
                .await
        }
    });
    let (_, request, _) = recv(&mut peer.calls).await?;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let cancel = serde_json::to_value(recv(&mut peer.cancels).await?)?;
    assert_eq!(cancel["call_id"], request.call_id);
    drop(connection);
    drop(handler);
    assert_eq!(
        recv(&mut peer.releases).await?.reservation_id,
        "reservation-0"
    );
    Ok(())
}

fn setup_local_tool_servers(config_dir: &Path, root: &Path) -> Result<()> {
    std::fs::create_dir_all(config_dir.join("tool_servers"))?;
    for (name, bin, args) in [
        (
            "fs",
            "harnx-fs-tools",
            vec!["--name", "fs", "--allow-read", root.to_str().unwrap()],
        ),
        (
            "attachments",
            "harnx-attachment-tools",
            vec!["--name", "attachments"],
        ),
    ] {
        std::fs::write(
            config_dir.join(format!("tool_servers/{name}.yaml")),
            serde_json::to_vec(&json!({"name":name,"command":binary(bin)?,"args":args}))?,
        )?;
    }
    Ok(())
}

struct ConnectionFixture<'a> {
    connection: &'a Connection,
    handler: &'a McpHandler,
}

async fn verify_isolated_connections(
    first: ConnectionFixture<'_>,
    second: ConnectionFixture<'_>,
    marker: &Path,
) -> Result<()> {
    let attachment = first
        .connection
        .call_tool(
            "attachments_attachment_create".to_owned(),
            json!({"content":"MCP caller identity", "mime_type":"text/plain"}),
            CancellationToken::new(),
        )
        .await?;
    assert_ne!(attachment.is_error, Some(true));
    assert!(serde_json::to_value(attachment)?
        .to_string()
        .contains("cid:"));
    first.handler.close().await?;
    std::fs::write(marker, "independent MCP session")?;
    let result = second
        .connection
        .call_tool(
            "fs_read".to_owned(),
            json!({"path":marker}),
            CancellationToken::new(),
        )
        .await?;
    assert_ne!(result.is_error, Some(true));
    assert!(serde_json::to_value(result)?
        .to_string()
        .contains("independent MCP session"));
    second.handler.close().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_local_worker_caller_identity_and_independent_connections() -> Result<()> {
    let fixture = Fixture::new()?;
    let config_dir = fixture.config_dir();
    setup_local_tool_servers(&config_dir, fixture.root.path())?;
    let _worker = EnvGuard::set(
        "HARNX_WORKER_BIN",
        Some(binary("harnx-worker")?.as_os_str()),
    );
    let bootstrap = Arc::new(fixture.bootstrap(None).await?);
    let first = McpHandler::new(
        bootstrap.clone(),
        ToolReservationView {
            package: None,
            use_tools: vec![
                "fs_read".to_owned(),
                "attachments_attachment_create".to_owned(),
            ],
        },
    );
    assert!(first.connection().session_id().await.is_none());
    let second = McpHandler::new(bootstrap.clone(), view("fs_read"));
    let one_connection = first.connection();
    let two_connection = second.connection();
    let (one, two) = tokio::time::timeout(DEADLINE, async {
        tokio::join!(one_connection.list_tools(), two_connection.list_tools())
    })
    .await?;
    let names: Vec<_> = one?.into_iter().map(|t| t.name.to_string()).collect();
    assert_eq!(names, ["attachments_attachment_create", "fs_read"]);
    assert_eq!(two?.len(), 1);
    assert_ne!(
        one_connection.session_id().await,
        two_connection.session_id().await
    );
    let marker = fixture.root.path().join("marker.txt");
    verify_isolated_connections(
        ConnectionFixture {
            connection: &one_connection,
            handler: &first,
        },
        ConnectionFixture {
            connection: &two_connection,
            handler: &second,
        },
        &marker,
    )
    .await?;
    assert!(bootstrap.config().session.is_none());
    Ok(())
}
