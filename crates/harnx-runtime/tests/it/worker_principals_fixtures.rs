//! Shared fixtures for the caller-principals worker tests: a parent agent
//! (`alpha`) delegating to a child agent (`beta`) on an isolated broker.

use anyhow::{Context, Result};
use harnx_core::tool::ToolCall;
use harnx_core::user_principals::{from_resolved, UserPrincipals};
use harnx_runtime::client::CompletionTokenUsage;
use harnx_runtime::config::{Config, ConfigLock};
use std::path::Path;
use std::sync::Arc;

/// The snapshot a trusted server stamps for alice: alias expansion plus
/// memberships, in resolver order.
pub fn expected_principals() -> UserPrincipals {
    from_resolved(
        Some("alice"),
        Some(&["alice".into(), "bob".into()]),
        &["engineering".into()],
        &["reviewer".into()],
    )
}

/// `alpha` delegates to `beta` through `beta_tools`; `beta` answers with
/// plain text.
pub fn write_delegation_agents(config_dir: &Path, alpha_tools: &str) -> Result<()> {
    let agents_dir = config_dir.join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::write(
        agents_dir.join("alpha.md"),
        format!(
            "---\nmodel: openai:test-model\nuse_tools: [{alpha_tools}]\n---\nAlpha agent instructions\n"
        ),
    )?;
    std::fs::write(
        agents_dir.join("beta.md"),
        "---\nmodel: openai:test-model\n---\nBeta agent instructions\n",
    )?;
    Ok(())
}

/// Fixture directories plus the config files that point this worker at the
/// test broker. The worker daemon resolves agent files through the global
/// `HARNX_CONFIG_DIR` path, so the returned guards must stay alive for the
/// whole test: they set config/data/state dirs and restore them on drop.
pub fn setup_worker_env(server_url: &str, root: &Path) -> Result<Vec<crate::worker::EnvGuard>> {
    use crate::worker::EnvGuard;
    let config_dir = root.join("config");
    std::fs::create_dir_all(config_dir.join("clients"))?;
    std::fs::create_dir_all(config_dir.join("nats_servers"))?;
    std::fs::create_dir_all(config_dir.join("agents"))?;
    std::fs::create_dir_all(root.join("data"))?;
    std::fs::create_dir_all(root.join("state"))?;
    std::fs::write(config_dir.join("config.yaml"), "model: openai:test-model\n")?;
    std::fs::write(
        config_dir.join("nats_servers").join("local.yaml"),
        format!("url: {server_url}\n"),
    )?;
    std::fs::write(
        config_dir.join("clients").join("openai.yaml"),
        "type: openai\napi_key: sk-test\nmodels:\n  - name: test-model\n    type: chat\n    max_input_tokens: 4096\n",
    )?;
    let guards = vec![
        EnvGuard::set("HARNX_CONFIG_DIR", config_dir.to_str().unwrap()),
        EnvGuard::set("HARNX_DATA_DIR", root.join("data").to_str().unwrap()),
        EnvGuard::set("HARNX_STATE_DIR", root.join("state").to_str().unwrap()),
        // `HARNX_NATS_URL` is what gates NATS tool discovery in this process
        // (`has_broker_address`): without it the session's registry snapshot
        // stays empty and sub-agent delegation tools are never found.
        EnvGuard::set("HARNX_NATS_URL", server_url),
        EnvGuard::set("HARNX_NATS_TOKEN", "test-token"),
        EnvGuard::set(
            harnx_core::instance::HARNX_SERVER_SCOPE,
            harnx_core::instance::ServerScope::new().as_str(),
        ),
    ];
    Ok(guards)
}

/// Load the fixture config and connect to its broker.
pub async fn worker_backend(
    root: &Path,
) -> Result<(
    Arc<ConfigLock>,
    async_nats::Client,
    async_nats::jetstream::Context,
)> {
    let config_path = root.join("config").join("config.yaml");
    let config = Arc::new(ConfigLock::new(Config::load_from_file(&config_path)?));
    let (client, js) = {
        let cfg = config.read().clone();
        (
            cfg.nats_client("local").await?,
            cfg.nats_jetstream("local").await?,
        )
    };
    Ok((config, client, js))
}

/// Scripted model: `agent`'s first invocation returns one tool call
/// (`tool` with `arguments`, id `tool_call_id`); every later invocation
/// answers with `text` and no tools.
pub fn one_tool_then_text_call_fn(
    agent: &'static str,
    tool: &'static str,
    arguments: serde_json::Value,
    tool_call_id: &'static str,
    text: &'static str,
) -> harnx_runtime::agent_loop::AgentCallFn {
    let made_call = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Arc::new(move |_input, config, _abort| {
        let current = config.read().extract_agent().name().to_string();
        let made_call = Arc::clone(&made_call);
        // Fresh per invocation: the async block owns its copy, so the outer
        // closure stays `Fn`.
        let arguments = arguments.clone();
        Box::pin(async move {
            let tool_calls =
                if current == agent && !made_call.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    vec![ToolCall::new(
                        tool.to_string(),
                        arguments.clone(),
                        Some(tool_call_id.to_string()),
                        None,
                    )]
                } else {
                    Vec::new()
                };
            Ok((
                text.to_string(),
                None,
                tool_calls,
                CompletionTokenUsage::default(),
            ))
        })
    })
}

/// Extract the child session id the sub-agent tool allocated, from the
/// parent session's tool result for `tool_call_id`.
pub async fn delegated_child_session_id(
    js: &async_nats::jetstream::Context,
    tool_call_id: &str,
) -> Result<String> {
    let log = harnx_runtime::nats_session_log::NatsSessionLog::new(
        js.clone(),
        harnx_core::session_identity::session_key(Some("alpha"), "subagent-principals-parent"),
    );
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let entries = log.load_events_async().await?;
        if let Some(session_id) = child_session_id_in(&entries, tool_call_id)? {
            return Ok(session_id);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "parent session never recorded a result for tool call {tool_call_id}: {entries:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// The child session id recorded in a tool result, if the result has landed.
fn child_session_id_in(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
    tool_call_id: &str,
) -> Result<Option<String>> {
    use harnx_core::session::SessionLogEntry;
    for (_, entry) in entries {
        let SessionLogEntry::ToolResults { results, .. } = entry else {
            continue;
        };
        for result in results {
            if result.id.as_deref() == Some(tool_call_id) {
                let output = &result.output;
                anyhow::ensure!(
                    !output
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    "sub-agent tool result is an error: {output:?}"
                );
                let session_id = output["session_id"].as_str().with_context(|| {
                    format!("sub-agent tool result missing session_id: {output:?}")
                })?;
                return Ok(Some(session_id.to_string()));
            }
        }
    }
    Ok(None)
}

/// Wait until `session` on `agent` has canonical metadata, then return it.
/// Fails at `deadline_secs` instead of hanging when no child appears.
pub async fn wait_for_metadata(
    js: &async_nats::jetstream::Context,
    agent: &str,
    session: &str,
    deadline_secs: u64,
) -> Result<harnx_runtime::nats_session_metadata::SessionMetadata> {
    let store = harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(js, 1).await?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(deadline_secs);
    loop {
        if let Some(record) = store.get_for_agent(session, agent).await? {
            return Ok(record.metadata);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "session {agent}/{session} never got metadata"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Wait until `session` on `agent` records a finished turn.
pub async fn wait_for_turn_end(
    js: &async_nats::jetstream::Context,
    agent: &str,
    session: &str,
    deadline_secs: u64,
) -> Result<()> {
    let log = harnx_runtime::nats_session_log::NatsSessionLog::new_with_replicas(
        js.clone(),
        harnx_core::session_identity::session_key(Some(agent), session),
        1,
    );
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(deadline_secs);
    loop {
        let entries = log.load_events_async().await?;
        if entries
            .iter()
            .any(|(_, entry)| matches!(entry, harnx_core::session::SessionLogEntry::TurnEnd { .. }))
        {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "session {agent}/{session} turn did not finish: {entries:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Start the worker daemon for the fixture config with a scripted model, and
/// wait for its readiness announcement like the middleware-interaction
/// harness does. The worker does not manage its own servers
/// (`WorkerDaemonConfig::new`): it discovers them under the scope
/// `setup_worker_env` set.
pub async fn spawn_worker(
    config: Arc<ConfigLock>,
    worker_name: &'static str,
    call_fn: harnx_runtime::agent_loop::AgentCallFn,
    client: &async_nats::Client,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let mut ready = client
        .subscribe(harnx_runtime::nats_worker::worker_ready_subject("local"))
        .await?;
    client.flush().await?;
    let daemon_config = harnx_runtime::nats_worker::WorkerDaemonConfig::new("local", worker_name);
    let daemon = tokio::spawn(async move {
        harnx_runtime::nats_worker::run_worker_daemon(config, daemon_config, Some(call_fn), None)
            .await
    });
    use futures_util::StreamExt;
    tokio::time::timeout(std::time::Duration::from_secs(30), ready.next())
        .await
        .context("worker readiness subscription closed")?;
    Ok(daemon)
}

/// Create a session on `agent` the way trusted servers create one: with the
/// resolved identity stamped at creation.
pub async fn stamped_parent_session(
    client: async_nats::Client,
    js: async_nats::jetstream::Context,
    agent: &str,
    session_id: &str,
) -> Result<harnx_runtime::NatsSession> {
    let initializer = harnx_runtime::SessionInitializer::named(agent, Default::default())
        .with_user_id("alice")
        .with_user_principals(&expected_principals());
    harnx_runtime::NatsSession::new(
        harnx_runtime::NatsSessionConfig {
            cluster: "local".to_string(),
            initializer,
            session_id: Some(session_id.to_string()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        client,
        js,
        harnx_runtime::utils::create_abort_signal(),
    )
    .await
    .context("parent session")
}

/// Run one prompt on a freshly stamped parent session, through external
/// admission, and fail when the turn itself failed.
pub async fn stamped_parent_turn(
    client: async_nats::Client,
    js: async_nats::jetstream::Context,
    agent: &str,
    session_id: &str,
    prompt: &str,
) -> Result<()> {
    let session = stamped_parent_session(client, js, agent, session_id).await?;
    let turn = session
        .clone()
        .with_external_admission()
        .run_turn(prompt, Arc::new(harnx_core::event::NullSink), None)
        .await?;
    anyhow::ensure!(turn.error.is_none(), "parent turn failed: {turn:?}");
    Ok(())
}

/// The one scripted tool call the parent's model makes.
pub struct ScriptedCall {
    pub tool: &'static str,
    pub arguments: serde_json::Value,
    pub tool_call_id: &'static str,
    pub reply_text: &'static str,
}

/// Everything a principals test varies about the scenario it runs.
pub struct ScenarioPlan {
    pub worker_name: &'static str,
    pub call: ScriptedCall,
    pub parent_session_id: &'static str,
    pub prompt: &'static str,
}

/// Everything a principals test needs on the worker side: the fixture
/// environment (guards stay alive as long as the harness), agent files that
/// delegate `alpha` to `beta` through the plan's tool, a connection to the
/// broker, and a scripted worker daemon whose `alpha` makes exactly one call.
pub struct WorkerHarness {
    pub client: async_nats::Client,
    pub js: async_nats::jetstream::Context,
    pub daemon: tokio::task::JoinHandle<Result<()>>,
    /// Keeps the worker process-scoped environment set; restored on drop.
    _env: Vec<crate::worker::EnvGuard>,
}

/// Set up the broker-side fixtures and spawn the scripted worker for the
/// environment in `root`.
pub async fn scripted_worker_harness(
    server_url: &str,
    root: &Path,
    plan: &ScenarioPlan,
) -> Result<WorkerHarness> {
    let env = setup_worker_env(server_url, root)?;
    write_delegation_agents(&root.join("config"), plan.call.tool)?;
    let (config, client, js) = worker_backend(root).await?;
    let call_fn = one_tool_then_text_call_fn(
        "alpha",
        plan.call.tool,
        plan.call.arguments.clone(),
        plan.call.tool_call_id,
        plan.call.reply_text,
    );
    let daemon = spawn_worker(config, plan.worker_name, call_fn, &client).await?;
    Ok(WorkerHarness {
        client,
        js,
        daemon,
        _env: env,
    })
}

/// One running scenario: the scripted worker and the fixture directories and
/// broker that keep it alive.
pub struct Scenario {
    pub harness: WorkerHarness,
    /// Keeps the broker process alive; killed on drop.
    _server: crate::common::NatsServerHandle,
    /// Keeps the fixture directories alive for the whole test.
    _root: tempfile::TempDir,
}

impl Scenario {
    /// Stop the worker daemon.
    pub async fn finish(self) {
        self.harness.daemon.abort();
        let _ = self.harness.daemon.await;
    }
}

/// Spawn the broker, build the fixture environment, start the scripted
/// worker, and run the parent turn that triggers the plan's one tool call.
/// Returns None when nats-server is not available, so the test can skip.
pub async fn scenario_or_skip(plan: ScenarioPlan) -> Result<Option<Scenario>> {
    let Some(server) = crate::common::spawn_nats_server().await? else {
        return Ok(None);
    };
    let root = tempfile::tempdir()?;
    let harness = scripted_worker_harness(server.url(), root.path(), &plan).await?;
    stamped_parent_turn(
        harness.client.clone(),
        harness.js.clone(),
        "alpha",
        plan.parent_session_id,
        plan.prompt,
    )
    .await?;
    Ok(Some(Scenario {
        harness,
        _server: server,
        _root: root,
    }))
}

/// Wait until `session` on `agent` finishes its turn, then assert its recorded
/// snapshot matches the one the parent was stamped with, verbatim.
pub async fn assert_snapshot_inherited(
    js: &async_nats::jetstream::Context,
    agent: &str,
    session: &str,
) -> Result<harnx_runtime::nats_session_metadata::SessionMetadata> {
    wait_for_turn_end(js, agent, session, 60).await?;
    let metadata = wait_for_metadata(js, agent, session, 10).await?;
    assert_eq!(
        harnx_runtime::nats_session_metadata::user_principals(&metadata),
        expected_principals(),
        "the session carries the parent snapshot verbatim"
    );
    Ok(metadata)
}
