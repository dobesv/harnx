//! D1 acceptance through real rmcp clients, worker, and native tool servers.
//! Only model execution is injected, at the worker's existing AgentCallFn seam.
use super::*;
use harnx_core::{cid_url::CidUrl, session::SessionLogEntry};
use harnx_mcp_server::transport::run_http;
use harnx_runtime::{
    nats_session_log::NatsSessionLog, nats_session_metadata::SessionMetadataStore, AgentCallFn,
};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, ErrorCode, ProtocolVersion},
    service::{RoleClient, RunningService},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ClientHandler, ServiceExt,
};
use std::process::Stdio;
use tokio_util::sync::CancellationToken;

const SELECTORS: &str = "fs_*,attachments_*,reviewer_*,harnx_agent_session_history_read";

struct Worker {
    task: AbortOnDropHandle<Result<()>>,
    // Keep broker alive until the worker and its managed children have stopped.
    _broker: harnx_test_bins::NatsServerHandle,
    _env: Vec<EnvGuard>,
    bootstrap: Arc<Bootstrap>,
    client: async_nats::Client,
    prompts: Arc<parking_lot::Mutex<Vec<String>>>,
}

fn write_agent(config: &Path, name: &str) -> Result<()> {
    std::fs::create_dir_all(config.join("agents"))?;
    std::fs::write(
        config.join(format!("agents/{name}.md")),
        "---\nmodel: test:test-model\nstream: false\nuse_tools: []\n---\nIntegration test agent\n",
    )?;
    Ok(())
}

fn write_fs(config: &Path, root: &Path) -> Result<()> {
    write_server(
        config,
        "fs",
        "harnx-fs-tools",
        json!(["--name", "fs", "--allow-read", root]),
    )
}

fn write_server(config: &Path, name: &str, bin: &str, args: serde_json::Value) -> Result<()> {
    std::fs::create_dir_all(config.join("tool_servers"))?;
    std::fs::write(
        config.join(format!("tool_servers/{name}.yaml")),
        serde_json::to_vec(&json!({"name": name, "command": binary(bin)?, "args": args}))?,
    )?;
    Ok(())
}

fn prepare_worker_environment(config: &Path, root: &Path) -> Result<()> {
    std::fs::create_dir_all(config.join("clients"))?;
    std::fs::write(
        config.join("clients/test.yaml"),
        "type: openai-compatible\nname: test\napi_base: http://127.0.0.1:1/v1\napi_key: unused\nmodels:\n  - name: test-model\n    max_input_tokens: 32000\n    max_output_tokens: 1024\n",
    )?;
    if !config.join("packages").exists() {
        write_agent(config, "reviewer")?;
        write_fs(config, root)?;
    }
    write_agent(config, "unselected")?;
    write_server(
        config,
        "attachments",
        "harnx-attachment-tools",
        json!(["--name", "attachments"]),
    )?;
    Ok(())
}

impl Worker {
    async fn start(fixture: &Fixture) -> Result<Self> {
        let config = fixture.config_dir();
        prepare_worker_environment(&config, fixture.root.path())?;
        let broker = harnx_test_bins::spawn_nats_server()
            .await?
            .context("nats-server required")?;
        fixture.cluster("X", broker.url())?;
        // Managing workers pass their broker endpoint to native child servers.
        let env = vec![
            EnvGuard::set("HARNX_NATS_URL", Some(broker.url().as_ref())),
            EnvGuard::set("HARNX_NATS_TOKEN", Some("".as_ref())),
        ];
        let bootstrap = Arc::new(fixture.bootstrap(Some("X")).await?);
        let client = async_nats::connect(broker.url()).await?;
        let prompts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let call_fn: AgentCallFn = Arc::new({
            let prompts = prompts.clone();
            move |input, _config, _abort| {
                let message = input.text();
                prompts.lock().push(message.clone());
                Box::pin(async move {
                    Ok((
                        format!("mock reply: {message}"),
                        None,
                        vec![],
                        harnx_runtime::client::CompletionTokenUsage::default(),
                    ))
                })
            }
        });
        let readiness = harnx_healthz::Readiness::default();
        let mut task = AbortOnDropHandle::new(tokio::spawn({
            let config = Arc::new(parking_lot::RwLock::new(bootstrap.config().clone()));
            let readiness = readiness.clone();
            async move {
                run_worker_daemon(
                    config,
                    WorkerDaemonConfig::managing("X", "d1-worker"),
                    Some(call_fn),
                    Some(readiness),
                )
                .await
            }
        }));
        tokio::select! {
            stopped = &mut task => anyhow::bail!("worker stopped before readiness: {stopped:?}"),
            result = tokio::time::timeout(DEADLINE, async {
                while !readiness.is_ready() { tokio::time::sleep(Duration::from_millis(20)).await; }
            }) => result.context("worker readiness deadline")?,
        }
        Ok(Self {
            task,
            _broker: broker,
            _env: env,
            bootstrap,
            client,
            prompts,
        })
    }

    async fn stop(self) -> Result<()> {
        self.task.abort();
        assert!(self.task.await.unwrap_err().is_cancelled());
        Ok(())
    }

    async fn metadata(&self) -> Result<SessionMetadataStore> {
        SessionMetadataStore::ensure(&async_nats::jetstream::new(self.client.clone()), 1).await
    }
}

async fn stdio_client(
    fixture: &Fixture,
    selectors: &str,
    package: Option<&str>,
) -> Result<(
    tokio::process::Child,
    RunningService<RoleClient, ()>,
    harnx_core::child_process::ChildProcessManager,
)> {
    let manager = harnx_core::child_process::ChildProcessManager::new();
    let mut command = tokio::process::Command::new(binary("harnx-mcp-server")?);
    command
        .args([
            "--mcp-stdio",
            "--cluster",
            "X",
            "--use-tools",
            selectors,
            "--config-dir",
        ])
        .arg(fixture.config_dir());
    if let Some(package) = package {
        command.args(["--package", package]);
    }
    command
        .env_remove("HARNX_MCP_USE_TOOLS")
        .env_remove("HARNX_MCP_PACKAGE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = manager.spawn(command).await?;
    let client = tokio::time::timeout(
        DEADLINE,
        ().serve((child.stdout.take().unwrap(), child.stdin.take().unwrap())),
    )
    .await??;
    Ok((child, client, manager))
}

async fn close_stdio(
    mut child: tokio::process::Child,
    client: RunningService<RoleClient, ()>,
) -> Result<()> {
    client.cancel().await?;
    assert!(tokio::time::timeout(DEADLINE, child.wait())
        .await??
        .success());
    Ok(())
}

async fn catalog(
    client: &RunningService<RoleClient, ()>,
    expected: &[&str],
) -> Result<Vec<String>> {
    let mut last_names = Vec::new();
    tokio::time::timeout(DEADLINE, async {
        loop {
            let names: Vec<_> = client
                .list_tools(None)
                .await?
                .tools
                .into_iter()
                .map(|t| t.name.to_string())
                .collect();
            if expected.iter().all(|name| names.iter().any(|n| n == name)) {
                return Ok(names);
            }
            last_names = names;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("expected {expected:?}; last catalog {last_names:?}"))?
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    args: serde_json::Value,
) -> Result<CallToolResult> {
    let result = tokio::time::timeout(
        DEADLINE,
        client.call_tool(
            CallToolRequestParams::new(name.to_owned()).with_arguments(
                args.as_object()
                    .context("tool args must be object")?
                    .clone(),
            ),
        ),
    )
    .await??;
    anyhow::ensure!(result.is_error != Some(true), "{name} failed: {result:?}");
    Ok(result)
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn assert_read_mcp(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    fixture: &Fixture,
) -> Result<()> {
    let path = fixture.root.path().join("marker.txt");
    std::fs::write(&path, "real MCP filesystem marker")?;
    let result = call(client, name, json!({"path": path})).await?;
    assert!(
        text(&result).contains("real MCP filesystem marker"),
        "{result:?}"
    );
    Ok(())
}

async fn attachment(client: &RunningService<RoleClient, ()>) -> Result<CidUrl> {
    let result = call(
        client,
        "attachments_attachment_create",
        json!({"content": "real MCP caller identity", "mime_type": "text/plain"}),
    )
    .await?;
    let value = serde_json::to_value(&result)?;
    let uri = value["content"]
        .as_array()
        .context("content array")?
        .iter()
        .find_map(|c| c["uri"].as_str())
        .context("attachment resource link missing")?;
    let url = CidUrl::parse(uri)?;
    assert!(
        url.session().agent.is_none(),
        "backing session must be inline"
    );
    let read = call(client, "attachments_attachment_read", json!({"url": uri})).await?;
    assert!(text(&read).contains("real MCP caller identity"), "{read:?}");
    Ok(url)
}

async fn assert_unlisted(client: &RunningService<RoleClient, ()>, name: &str) -> Result<()> {
    let error = client
        .call_tool(
            CallToolRequestParams::new(name.to_owned()).with_arguments(
                json!({"message": "must not execute"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect_err("unlisted tool accepted");
    match error {
        rmcp::ServiceError::McpError(error) => assert_eq!(error.code, ErrorCode::INVALID_PARAMS),
        error => anyhow::bail!("expected invalid params, got {error:?}"),
    }
    Ok(())
}

async fn agent_continuation(
    client: &RunningService<RoleClient, ()>,
    worker: &Worker,
    tool: &str,
    agent: &str,
    owner: &CidUrl,
) -> Result<String> {
    let first = call(client, tool, json!({"message": "first prompt"})).await?;
    let first: serde_json::Value = serde_json::from_str(&text(&first))?;
    let session_id = first["session_id"]
        .as_str()
        .context("session_prompt session_id missing")?
        .to_owned();
    assert_eq!(first["response"], "mock reply: first prompt");
    let second = call(
        client,
        tool,
        json!({"message": "continued prompt", "session_id": session_id}),
    )
    .await?;
    let second: serde_json::Value = serde_json::from_str(&text(&second))?;
    assert_eq!(second["session_id"], session_id);
    assert_eq!(second["response"], "mock reply: continued prompt");
    let key = harnx_core::session_identity::session_key(Some(agent), &session_id);
    let metadata = worker
        .metadata()
        .await?
        .get(&key)
        .await?
        .context("child metadata missing")?;
    assert_eq!(metadata.metadata.agent.name(), Some(agent));
    assert_eq!(
        metadata
            .metadata
            .parent
            .context("caller parent link missing")?
            .session_id,
        owner.session().owner()
    );
    let events = NatsSessionLog::for_agent(
        async_nats::jetstream::new(worker.client.clone()),
        agent,
        &session_id,
    )
    .load_events_async()
    .await?;
    let messages: Vec<_> = events
        .into_iter()
        .filter_map(|(_, event)| match event {
            SessionLogEntry::Message { role, content, .. } => Some((role, content)),
            _ => None,
        })
        .collect();
    let messages: Vec<_> = messages
        .into_iter()
        .map(|(role, content)| (role, content.to_text()))
        .collect();
    assert_eq!(
        messages,
        vec![
            (
                harnx_core::message::MessageRole::User,
                "first prompt".to_owned()
            ),
            (
                harnx_core::message::MessageRole::Assistant,
                "mock reply: first prompt".to_owned()
            ),
            (
                harnx_core::message::MessageRole::User,
                "continued prompt".to_owned()
            ),
            (
                harnx_core::message::MessageRole::Assistant,
                "mock reply: continued prompt".to_owned()
            ),
        ],
        "continuation must preserve exactly one copy of each turn"
    );
    Ok(session_id)
}

async fn core_flow(
    client: &RunningService<RoleClient, ()>,
    fixture: &Fixture,
    worker: &Worker,
) -> Result<CidUrl> {
    let names = catalog(
        client,
        &[
            "fs_read",
            "attachments_attachment_create",
            "reviewer_session_prompt",
            "reviewer_session_new",
            "reviewer_session_load",
            "reviewer_session_cancel",
        ],
    )
    .await?;
    assert!(!names
        .iter()
        .any(|n| n.ends_with("_session_handoff") || n == "harnx_agent_session_history_read"));
    assert!(!names.iter().any(|n| n == "unselected_session_prompt"));
    assert_read_mcp(client, "fs_read", fixture).await?;
    let owner = attachment(client).await?;
    assert!(worker
        .metadata()
        .await?
        .get(&owner.session().owner())
        .await?
        .is_some());
    // Existing-only read fails if open created metadata but no transcript.
    assert!(NatsSessionLog::new(
        async_nats::jetstream::new(worker.client.clone()),
        owner.session().owner(),
    )
    .last_entry_async()
    .await?
    .is_none());
    assert_unlisted(client, "unselected_session_prompt").await?;
    assert_unlisted(client, "read").await?;
    assert_unlisted(client, "reviewer_session_handoff").await?;
    assert_unlisted(client, "harnx_agent_session_history_read").await?;
    agent_continuation(
        client,
        worker,
        "reviewer_session_prompt",
        "reviewer",
        &owner,
    )
    .await?;
    Ok(owner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdio_binary_real_tools_and_agent_continuation() -> Result<()> {
    let fixture = Fixture::new()?;
    let worker = Worker::start(&fixture).await?;
    let (child, client, _manager) = stdio_client(&fixture, SELECTORS, None).await?;
    let owner = core_flow(&client, &fixture, &worker).await?;
    assert_eq!(*worker.prompts.lock(), ["first prompt", "continued prompt"]);
    close_stdio(child, client).await?;
    assert!(
        worker
            .metadata()
            .await?
            .get(&owner.session().owner())
            .await?
            .is_some(),
        "disconnect deleted backing session"
    );
    worker.stop().await
}

async fn http_client(url: &str) -> Result<RunningService<RoleClient, ()>> {
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::builder().no_proxy().build()?,
        StreamableHttpClientTransportConfig::with_uri(url),
    );
    Ok(tokio::time::timeout(DEADLINE, ().serve(transport)).await??)
}

async fn spawn_http_test_server(
    worker: &Worker,
) -> Result<(String, CancellationToken, AbortOnDropHandle<Result<()>>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    let shutdown = CancellationToken::new();
    let server = AbortOnDropHandle::new(tokio::spawn(run_http(
        worker.bootstrap.clone(),
        ToolReservationView {
            package: None,
            use_tools: harnx_core::agent_config::split_tool_selectors(SELECTORS)
                .into_iter()
                .map(str::to_owned)
                .collect(),
        },
        harnx_mcp_server::transport::HttpOptions {
            listener,
            session_config: Default::default(),
            shutdown: shutdown.clone(),
        },
    )));
    Ok((url, shutdown, server))
}

async fn connect_and_assert_http_clients(
    url: &str,
) -> Result<(
    RunningService<RoleClient, ()>,
    RunningService<RoleClient, ()>,
)> {
    let (one, two) = tokio::join!(http_client(url), http_client(url));
    let one = one?;
    let two = two?;
    for client in [&one, &two] {
        assert_eq!(
            client
                .peer_info()
                .context("initialize result missing")?
                .protocol_version,
            ProtocolVersion::V_2025_11_25
        );
    }
    let (one_names, two_names) = tokio::join!(
        catalog(&one, &["fs_read", "reviewer_session_prompt"]),
        catalog(&two, &["fs_read", "reviewer_session_prompt"])
    );
    assert_eq!(one_names?, two_names?);
    Ok((one, two))
}

async fn verify_isolated_continuation_and_persistence(
    two: RunningService<RoleClient, ()>,
    worker: &Worker,
    fixture: &Fixture,
    second_owner: &CidUrl,
    first_owner: &CidUrl,
    shutdown: CancellationToken,
    server: AbortOnDropHandle<Result<()>>,
) -> Result<()> {
    assert_read_mcp(&two, "fs_read", fixture).await?;
    let again = attachment(&two).await?;
    assert_eq!(
        again.session(),
        second_owner.session(),
        "HTTP calls changed backing identity"
    );
    agent_continuation(
        &two,
        worker,
        "reviewer_session_prompt",
        "reviewer",
        second_owner,
    )
    .await?;
    assert_eq!(
        *worker.prompts.lock(),
        [
            "first prompt",
            "continued prompt",
            "first prompt",
            "continued prompt"
        ]
    );
    two.cancel().await?;
    shutdown.cancel();
    tokio::time::timeout(DEADLINE, server).await???;
    for owner in [first_owner, second_owner] {
        assert!(worker
            .metadata()
            .await?
            .get(&owner.session().owner())
            .await?
            .is_some());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_default_initialize_downgrades_and_real_sessions_are_isolated() -> Result<()> {
    let fixture = Fixture::new()?;
    let worker = Worker::start(&fixture).await?;
    // Exercise default Initialize, not a client pinned to an older protocol.
    assert_eq!(
        ().get_info().protocol_version,
        ProtocolVersion::V_2026_07_28
    );
    let (url, shutdown, server) = spawn_http_test_server(&worker).await?;
    let (one, two) = connect_and_assert_http_clients(&url).await?;

    let first_owner = core_flow(&one, &fixture, &worker).await?;
    let second_owner = attachment(&two).await?;
    assert_ne!(
        first_owner.session().session_id,
        second_owner.session().session_id
    );
    assert_ne!(
        first_owner.session().owner(),
        second_owner.session().owner()
    );
    one.cancel().await?;

    verify_isolated_continuation_and_persistence(
        two,
        &worker,
        &fixture,
        &second_owner,
        &first_owner,
        shutdown,
        server,
    )
    .await?;

    worker.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdio_package_cli_same_and_cross_package_names_are_callable() -> Result<()> {
    let fixture = Fixture::new()?;
    for package in ["own", "other"] {
        let config = fixture.config_dir().join("packages").join(package);
        let name = if package == "own" { "fs" } else { "crossfs" };
        write_server(
            &config,
            name,
            "harnx-fs-tools",
            json!(["--name", name, "--allow-read", fixture.root.path()]),
        )?;
        std::fs::create_dir_all(config.join("agents"))?;
        // Explicitly use the top-level client; package-relative test:test-model
        // would resolve to a package client that this fixture doesn't install.
        std::fs::write(
            config.join("agents/reviewer.md"),
            "---\nmodel: /test:test-model\nstream: false\nuse_tools: []\n---\nPackage integration agent\n",
        )?;
    }
    let worker = Worker::start(&fixture).await?;
    let (child, client, _manager) = stdio_client(
        &fixture,
        "fs_read,other__crossfs_read,reviewer_*,other__reviewer_*,attachments_*",
        Some("own"),
    )
    .await?;
    let names = catalog(
        &client,
        &[
            "fs_read",
            "other__crossfs_read",
            "reviewer_session_prompt",
            "other__reviewer_session_prompt",
        ],
    )
    .await?;
    for excluded in [
        "own__fs_read",
        "own__reviewer_session_prompt",
        "fs_write",
        "other__crossfs_write",
    ] {
        assert!(!names.iter().any(|n| n == excluded), "{names:?}");
        assert_unlisted(&client, excluded).await?;
    }
    for name in ["fs_read", "other__crossfs_read"] {
        assert_read_mcp(&client, name, &fixture).await?;
    }
    let owner = attachment(&client).await?;
    let own_id = agent_continuation(
        &client,
        &worker,
        "reviewer_session_prompt",
        "own/reviewer",
        &owner,
    )
    .await?;
    let other_id = agent_continuation(
        &client,
        &worker,
        "other__reviewer_session_prompt",
        "other/reviewer",
        &owner,
    )
    .await?;
    assert_ne!(own_id, other_id);
    assert_eq!(worker.prompts.lock().len(), 4);
    close_stdio(child, client).await?;
    worker.stop().await
}

#[test]
fn binary_missing_or_empty_selectors_fail_before_transport() -> Result<()> {
    let fixture = Fixture::new()?;
    for transport in ["--mcp-stdio", "--mcp-http"] {
        for selector in [None, Some(""), Some(" , ")] {
            let mut command = std::process::Command::new(binary("harnx-mcp-server")?);
            command
                .arg(transport)
                .arg("--config-dir")
                .arg(fixture.config_dir())
                .env_remove("HARNX_MCP_USE_TOOLS")
                .env_remove("HARNX_MCP_PACKAGE");
            if let Some(selector) = selector {
                command.args(["--use-tools", selector]);
            }
            let output = command.output()?;
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8(output.stderr)?
                .contains("no tool selectors specified; set --use-tools or HARNX_MCP_USE_TOOLS"));
        }
    }
    Ok(())
}
