//! Broker-backed worker tests for `patches`: two sessions of the same agent
//! with different caller snapshots get different effective configs on one
//! worker, a sub-agent child applies its own patches against the inherited
//! snapshot, and a broken patch fails the activation without ever running the
//! unpatched config.

use crate::worker_principals_fixtures as fixtures;

use anyhow::{Context, Result};
use harnx_core::require_nextest;
use harnx_core::tool::ToolCall;
use harnx_core::user_principals::{from_resolved, UserPrincipals};
use harnx_runtime::client::CompletionTokenUsage;
use harnx_runtime::config::GlobalConfig;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the scripted model saw for one invocation: the agent whose turn it
/// was, the effective selectors, the declarations handed to the model, and
/// the snapshot the effective agent carries.
#[derive(Clone, Debug)]
struct ModelView {
    agent: String,
    model: String,
    use_tools: Option<Vec<String>>,
    bash_tools: Option<Vec<String>>,
    declarations: Vec<String>,
    groups: Vec<String>,
    system_prompt: String,
}

fn developer_snapshot() -> UserPrincipals {
    from_resolved(
        Some("alice"),
        None,
        &["developers".into(), "engineering".into()],
        &[],
    )
}

fn outsider_snapshot() -> UserPrincipals {
    from_resolved(Some("mallory"), None, &["finance".into()], &[])
}

/// Every invocation records its view of the config the worker installed;
/// the first invocation of `scripted.0` also makes the one scripted tool
/// call, and every invocation answers with `text`.
fn recording_call_fn(
    views: Arc<Mutex<Vec<ModelView>>>,
    scripted: Option<(&'static str, &'static str, serde_json::Value)>,
    text: &'static str,
) -> harnx_runtime::agent_loop::AgentCallFn {
    let made_call = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Arc::new(move |input, config: &GlobalConfig, _abort| {
        // Exercise the same per-turn system-prompt injection as the real model path.
        let messages = harnx_runtime::config::input::build_messages(input, config)
            .expect("build model messages");
        let system_prompt = messages
            .iter()
            .find(|message| message.role == harnx_core::message::MessageRole::System)
            .map(|message| message.content.to_text())
            .unwrap_or_default();
        let view = {
            let cfg = config.read();
            let agent = cfg.extract_agent();
            ModelView {
                agent: agent.name().to_string(),
                model: agent.model().id(),
                use_tools: agent.use_tools(),
                bash_tools: agent.bash_tools(),
                // The scripted call-fn path skips `prepare_completion_data`,
                // so compute the model-visible declarations the same way it
                // does: `select_tools` over the effective agent.
                declarations: cfg
                    .select_tools(&agent)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect(),
                groups: input
                    .agent()
                    .user_principals()
                    .map(|user| user.groups.clone())
                    .unwrap_or_default(),
                system_prompt,
            }
        };
        views.lock().expect("views lock").push(view.clone());
        let tool_calls = match &scripted {
            Some((agent, tool, arguments))
                if view.agent == *agent
                    && !made_call.swap(true, std::sync::atomic::Ordering::SeqCst) =>
            {
                vec![ToolCall::new(
                    tool.to_string(),
                    arguments.clone(),
                    Some("patched-worker-call-1".to_string()),
                    None,
                )]
            }
            _ => Vec::new(),
        };
        Box::pin(async move {
            Ok((
                text.to_string(),
                None,
                tool_calls,
                CompletionTokenUsage::default(),
            ))
        })
    })
}

/// `alpha` keeps its coding tools only for developers-group callers; `beta`
/// trims its own tools for engineering-group callers — the snapshot a child
/// inherits from an `alice` parent.
fn write_patch_agents(config_dir: &std::path::Path) -> Result<()> {
    let agents_dir = config_dir.join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::write(
        agents_dir.join("alpha.md"),
        "---\n\
         model: openai:test-model\n\
         use_tools: [beta_session_*, harnx_agent_session_history_read]\n\
         bash_tools: [fs_*]\n\
         patches:\n\
         \x20 - 'if ($user.groups | index(\"developers\")) then . else .use_tools = [] | .bash_tools = [] end'\n\
         ---\n\
         Alpha instructions\n\
         {% if \"developers\" in __user__.groups %}You may delegate coding work to coder.{% endif %}\n",
    )?;
    std::fs::write(
        agents_dir.join("beta.md"),
        "---\n\
         model: openai:test-model\n\
         use_tools: [alpha_session_*]\n\
         patches:\n\
         \x20 - 'if ($user.groups | index(\"engineering\")) then .use_tools = [] else . end'\n\
         ---\n\
         Beta instructions\n",
    )?;
    Ok(())
}

async fn session_with_snapshot(
    client: async_nats::Client,
    js: async_nats::jetstream::Context,
    agent: &str,
    session_id: &str,
    snapshot: &UserPrincipals,
) -> Result<harnx_runtime::NatsSession> {
    let mut initializer = harnx_runtime::SessionInitializer::named(agent, Default::default())
        .with_user_principals(snapshot);
    if let Some(user_id) = &snapshot.user_id {
        initializer = initializer.with_user_id(user_id.clone());
    }
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
    .with_context(|| format!("session {agent}/{session_id}"))
}

async fn run_turn(session: harnx_runtime::NatsSession, prompt: &str) -> Result<()> {
    let turn = session
        .clone()
        .with_external_admission()
        .run_turn(prompt, Arc::new(harnx_core::event::NullSink), None)
        .await?;
    anyhow::ensure!(turn.error.is_none(), "turn failed: {turn:?}");
    Ok(())
}

/// Extract the child session id the sub-agent tool allocated for
/// `tool_call_id`, from the parent session's tool result.
async fn delegated_child_session_id(
    js: &async_nats::jetstream::Context,
    parent_session: &str,
    tool_call_id: &str,
) -> Result<String> {
    let log = harnx_runtime::nats_session_log::NatsSessionLog::new(
        js.clone(),
        harnx_core::session_identity::session_key(Some("alpha"), parent_session),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let entries = log.load_events_async().await?;
        for (_, entry) in &entries {
            let harnx_core::session::SessionLogEntry::ToolResults { results, .. } = entry else {
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
                    return Ok(session_id.to_string());
                }
            }
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "parent session never recorded a result for tool call {tool_call_id}: {entries:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn skip_no_nats_server() {
    eprintln!("skipping: nats-server not available");
}

// qual:allow(srp, god_struct) reason: "one scenario handle owns the broker, client handles, recorded views, and the daemon task it must tear down"
struct PatchScenario {
    client: async_nats::Client,
    js: async_nats::jetstream::Context,
    views: Arc<Mutex<Vec<ModelView>>>,
    daemon: tokio::task::JoinHandle<Result<()>>,
    _env: Vec<crate::worker::EnvGuard>,
    _root: tempfile::TempDir,
    _server: crate::common::NatsServerHandle,
}

/// Spawn the broker and a scripted worker over the `alpha`/`beta` fixture
/// agents. Returns `None` when nats-server is not available, so the test can
/// skip.
async fn patch_scenario_or_skip(
    scripted: Option<(&'static str, &'static str, serde_json::Value)>,
) -> Result<Option<PatchScenario>> {
    let Some(server) = crate::common::spawn_nats_server().await? else {
        return Ok(None);
    };
    let root = tempfile::tempdir()?;
    let env = fixtures::setup_worker_env(server.url(), root.path())?;
    write_patch_agents(&root.path().join("config"))?;
    let (config, client, js) = fixtures::worker_backend(root.path()).await?;
    let views = Arc::new(Mutex::new(Vec::new()));
    let call_fn = recording_call_fn(Arc::clone(&views), scripted, "worker answer");
    let daemon = fixtures::spawn_worker(config, "worker-agent-patches", call_fn, &client).await?;
    Ok(Some(PatchScenario {
        client,
        js,
        views,
        daemon,
        _env: env,
        _root: root,
        _server: server,
    }))
}

impl PatchScenario {
    async fn finish(self) {
        self.daemon.abort();
        let _ = self.daemon.await;
    }

    fn views_of(&self, agent: &str) -> Vec<ModelView> {
        self.views
            .lock()
            .expect("views lock")
            .iter()
            .filter(|view| view.agent == agent)
            .cloned()
            .collect()
    }
}

/// Two sessions of the same agent on one worker, different snapshots: the
/// developer session keeps its tools and the outsider session gets the
/// patched, trimmed config — in the model tool declarations, the effective
/// selectors, and the snapshot the effective agent carries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_of_one_agent_get_different_effective_configs() -> Result<()> {
    require_nextest();
    let Some(scenario) = patch_scenario_or_skip(None).await? else {
        skip_no_nats_server();
        return Ok(());
    };

    let developer = session_with_snapshot(
        scenario.client.clone(),
        scenario.js.clone(),
        "alpha",
        "patch-alpha-developer",
        &developer_snapshot(),
    )
    .await?;
    run_turn(developer, "hello developer session").await?;

    let outsider = session_with_snapshot(
        scenario.client.clone(),
        scenario.js.clone(),
        "alpha",
        "patch-alpha-outsider",
        &outsider_snapshot(),
    )
    .await?;
    run_turn(outsider, "hello outsider session").await?;

    let views = scenario.views_of("alpha");
    assert_eq!(
        views.len(),
        2,
        "one model invocation per session: {views:?}"
    );
    let (developer_view, outsider_view) = (&views[0], &views[1]);
    assert_eq!(
        developer_view.system_prompt.trim(),
        "Alpha instructions\nYou may delegate coding work to coder."
    );
    assert_eq!(outsider_view.system_prompt.trim(), "Alpha instructions");
    assert_ne!(developer_view.system_prompt, outsider_view.system_prompt);

    // The developer session keeps the beta delegation tools.
    assert_eq!(
        developer_view.groups,
        vec!["developers".to_string(), "engineering".to_string()]
    );
    assert_eq!(
        developer_view.use_tools,
        Some(vec![
            "beta_session_*".to_string(),
            "harnx_agent_session_history_read".to_string(),
        ])
    );
    assert_eq!(developer_view.bash_tools, Some(vec!["fs_*".to_string()]));
    assert!(
        developer_view
            .declarations
            .iter()
            .any(|name| name == "harnx_agent_session_history_read"),
        "developer declarations must include the tools the patch kept: {:?}",
        developer_view.declarations
    );

    // The outsider session of the same agent got the trimmed config.
    assert_eq!(outsider_view.groups, vec!["finance".to_string()]);
    assert_eq!(outsider_view.use_tools, Some(Vec::new()));
    assert_eq!(outsider_view.bash_tools, Some(Vec::new()));
    assert!(
        outsider_view.declarations.is_empty(),
        "outsider declarations must be empty: {:?}",
        outsider_view.declarations
    );

    scenario.finish().await;
    Ok(())
}

/// A sub-agent child applies its own agent's patches against the snapshot it
/// inherited from the parent: `alice` is in `engineering`, so the child
/// `beta` session trims the tools its own patch removes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagent_child_applies_its_own_patches_with_the_inherited_snapshot() -> Result<()> {
    require_nextest();
    let Some(scenario) = patch_scenario_or_skip(Some((
        "alpha",
        "beta_session_prompt",
        serde_json::json!({"message": "finish delegated work"}),
    )))
    .await?
    else {
        skip_no_nats_server();
        return Ok(());
    };

    let parent = session_with_snapshot(
        scenario.client.clone(),
        scenario.js.clone(),
        "alpha",
        "patch-subagent-parent",
        &developer_snapshot(),
    )
    .await?;
    run_turn(parent, "delegate to the child").await?;

    let child_session_id = delegated_child_session_id(
        &scenario.js,
        "patch-subagent-parent",
        "patched-worker-call-1",
    )
    .await?;
    fixtures::wait_for_turn_end(&scenario.js, "beta", &child_session_id, 60).await?;

    let parent_views = scenario.views_of("alpha");
    assert_eq!(
        parent_views.len(),
        2,
        "parent ran a model call before and after the delegation: {parent_views:?}"
    );
    let child_views = scenario.views_of("beta");
    assert_eq!(
        child_views.len(),
        1,
        "child ran one model call: {child_views:?}"
    );
    let child_view = &child_views[0];

    // The child kept the inherited snapshot and applied beta's own patch to
    // it: engineering is in the inherited groups, so use_tools is trimmed.
    assert_eq!(
        child_view.groups,
        vec!["developers".to_string(), "engineering".to_string()],
        "the child must see the inherited snapshot"
    );
    assert_eq!(
        child_view.use_tools,
        Some(Vec::new()),
        "beta's own patch must have trimmed the child's tools"
    );
    assert!(child_view.declarations.is_empty());

    scenario.finish().await;
    Ok(())
}

/// A broken `patches` filter fails the session activation: the turn settles
/// with the activation error naming the agent, and the session never runs a
/// model call with the unpatched config.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broken_patch_fails_activation_and_never_runs_unpatched() -> Result<()> {
    require_nextest();
    let Some(server) = crate::common::spawn_nats_server().await? else {
        skip_no_nats_server();
        return Ok(());
    };
    let root = tempfile::tempdir()?;
    let env = fixtures::setup_worker_env(server.url(), root.path())?;
    let agents_dir = root.path().join("config").join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    // Compiles at load, fails at activation: it indexes a field on every
    // string in use_tools.
    std::fs::write(
        agents_dir.join("broken.md"),
        "---\nmodel: openai:test-model\nuse_tools: [beta_session_*]\npatches: '.use_tools |= map(.missing)'\n---\nBroken instructions\n",
    )?;
    std::fs::write(
        agents_dir.join("beta.md"),
        "---\nmodel: openai:test-model\n---\nBeta instructions\n",
    )?;
    let (config, client, js) = fixtures::worker_backend(root.path()).await?;
    let views = Arc::new(Mutex::new(Vec::new()));
    let call_fn = recording_call_fn(Arc::clone(&views), None, "never reached");
    let daemon = fixtures::spawn_worker(config, "worker-broken-patch", call_fn, &client).await?;
    let _env = env;

    let session = session_with_snapshot(
        client.clone(),
        js.clone(),
        "broken",
        "patch-broken-session",
        &outsider_snapshot(),
    )
    .await?;
    // The activation fails durably and the frontend settles with the worker's
    // error instead of waiting forever.
    let turn = tokio::time::timeout(
        Duration::from_secs(60),
        session.clone().with_external_admission().run_turn(
            "hello",
            Arc::new(harnx_core::event::NullSink),
            None,
        ),
    )
    .await
    .expect("turn settles")?;
    let error = turn
        .error
        .expect("a broken patch must fail the turn instead of running unpatched");
    assert!(
        error.contains("broken") && error.contains("patches"),
        "the turn error must name the agent and `patches`: {error}"
    );

    // Never ran unpatched: no model call, no completed turn.
    assert!(
        views.lock().expect("views lock").is_empty(),
        "the broken agent must never reach the model"
    );
    let log = harnx_runtime::nats_session_log::NatsSessionLog::new(
        js.clone(),
        harnx_core::session_identity::session_key(Some("broken"), "patch-broken-session"),
    );
    let entries = log.load_events_async().await?;
    assert!(
        !entries.iter().any(|(_, entry)| matches!(
            entry,
            harnx_core::session::SessionLogEntry::TurnEnd { .. }
        )),
        "the session must not finish a turn with the unpatched config: {entries:?}"
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patches_clearing_model_activate_with_the_global_default() -> Result<()> {
    require_nextest();
    let Some(server) = crate::common::spawn_nats_server().await? else {
        skip_no_nats_server();
        return Ok(());
    };
    let root = tempfile::tempdir()?;
    let _env = fixtures::setup_worker_env(server.url(), root.path())?;
    let agents_dir = root.path().join("config/agents");
    for (agent, filter) in [("alpha", ".model = null"), ("beta", "del(.model)")] {
        std::fs::write(
            agents_dir.join(format!("{agent}.md")),
            format!("---\nmodel: openai:test-model-mini\npatches: '{filter}'\n---\nInstructions\n"),
        )?;
    }
    std::fs::write(agents_dir.join("baseline.md"), "Instructions\n")?;
    let (config, client, js) = fixtures::worker_backend(root.path()).await?;
    config.write().set_model("openai:test-model")?;
    assert_eq!(
        config.read().retrieve_agent("alpha")?.model().id(),
        "openai:test-model-mini"
    );
    let views = Arc::new(Mutex::new(Vec::new()));
    let call_fn = recording_call_fn(Arc::clone(&views), None, "worker answer");
    let daemon = fixtures::spawn_worker(config, "worker-clear-model", call_fn, &client).await?;

    for agent in ["baseline", "alpha", "beta"] {
        let session = session_with_snapshot(
            client.clone(),
            js.clone(),
            agent,
            &format!("clear-model-{agent}"),
            &outsider_snapshot(),
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(60), run_turn(session, "hello"))
            .await
            .context("model-clearing activation must settle")??;
    }

    daemon.abort();
    let _ = daemon.await;
    let views = views.lock().expect("views lock");
    assert_eq!(views.len(), 3, "all three agents must reach the model");
    for view in views.iter() {
        assert_eq!(view.model, "openai:test-model", "{view:?}");
    }
    Ok(())
}
