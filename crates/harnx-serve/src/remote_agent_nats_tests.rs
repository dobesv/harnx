use super::*;
use ag_ui_core::event::Event;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use harnx_runtime::{
    nats_worker::{run_worker_daemon, worker_ready_subject, WorkerDaemonConfig},
    AgentCallFn,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{broadcast, oneshot};

const CI_SAFE_TIMEOUT: Duration = Duration::from_secs(60);

async fn subscribe(handle: &crate::session_actor::SessionHandle) -> broadcast::Receiver<Event> {
    let (reply, result) = oneshot::channel();
    handle
        .tx
        .send(crate::session_actor::SessionCommand::Subscribe { reply })
        .await
        .expect("subscribe command");
    result.await.expect("subscribe response").events
}

async fn prompt(
    handle: &crate::session_actor::SessionHandle,
    text: &str,
) -> crate::session_actor::PromptResult {
    let (reply, result) = oneshot::channel();
    handle
        .tx
        .send(crate::session_actor::SessionCommand::Prompt {
            text: text.to_string(),
            options: Default::default(),
            reply,
        })
        .await
        .expect("prompt command");
    result.await.expect("prompt response")
}

fn counting_stub_call_fn(calls: Arc<AtomicUsize>) -> AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let calls = Arc::clone(&calls);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok((
                "done".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

async fn spawn_shared_worker(
    config: Config,
    calls: Arc<AtomicUsize>,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let client = config.nats_client("shared").await?;
    let mut ready = client.subscribe(worker_ready_subject("shared")).await?;
    client.flush().await?;

    let worker_config = Arc::new(RwLock::new(config));
    let mut daemon = tokio::spawn(run_worker_daemon(
        worker_config,
        WorkerDaemonConfig::managing("shared", "serve-remote-e2e"),
        Some(counting_stub_call_fn(calls)),
        None,
    ));

    tokio::select! {
        announced = tokio::time::timeout(CI_SAFE_TIMEOUT, ready.next()) => {
            announced
                .context("shared worker did not announce readiness")?
                .context("shared worker readiness subscription closed")?;
        }
        stopped = &mut daemon => {
            anyhow::bail!("shared worker stopped before readiness: {stopped:?}");
        }
    }
    Ok(daemon)
}

async fn shared_agent_config(sandbox: &crate::test_support::TestConfigSandbox) -> Result<Config> {
    sandbox.write_agent("sisyphus", "Complete the test turn.");
    let shared = harnx_runtime::config::resolve_local_nats_server_config().await?;
    let token = shared.token.as_deref().map_or_else(String::new, |token| {
        format!(
            "token: {}\n",
            serde_json::to_string(token).expect("serialize NATS token")
        )
    });
    sandbox.write_nats_server(
        "shared",
        &format!(
            "url: {}\n{token}agents:\n  - name: sisyphus\n    description: Remote test agent\n    role: assistant\n",
            serde_json::to_string(&shared.url).expect("serialize NATS URL")
        ),
    );
    let mut config = sandbox.config();
    config.dry_run = false;
    Ok(config)
}

async fn accepted_run_id(
    handle: &crate::session_actor::SessionHandle,
    text: &str,
    context: &str,
) -> Result<String> {
    match prompt(handle, text).await {
        crate::session_actor::PromptResult::Accepted { run_id } => Ok(run_id),
        other => anyhow::bail!("{context} was not accepted: {other:?}"),
    }
}

async fn wait_for_run_finished(
    events: &mut broadcast::Receiver<Event>,
    run_id: &str,
    context: &str,
) -> Result<()> {
    tokio::time::timeout(
        CI_SAFE_TIMEOUT,
        receive_run_finished(events, run_id, context),
    )
    .await
    .with_context(|| format!("{context} did not finish"))??;
    Ok(())
}

async fn receive_run_finished(
    events: &mut broadcast::Receiver<Event>,
    run_id: &str,
    context: &str,
) -> Result<()> {
    loop {
        match events.recv().await.context("remote event stream closed")? {
            Event::RunFinished(event) if event.run_id.to_string() == run_id => return Ok(()),
            Event::RunError(error) => anyhow::bail!("{context} failed: {}", error.message),
            _ => {}
        }
    }
}

async fn assert_remote_turns_persisted(
    config: &Config,
    target: &ResolvedAgentTarget,
    session_id: &str,
    calls: &AtomicUsize,
) -> Result<()> {
    let (session, entries) = crate::load_nats_session(config, target, session_id).await?;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "worker did not execute both turns; entries={entries:?}, messages={:?}",
        session.messages
    );
    assert_eq!(session.agent_name.as_deref(), Some("sisyphus"));
    assert!(
        !entries.is_empty(),
        "remote turn must persist a session log"
    );
    for expected in ["complete this remote turn", "resume this remote session"] {
        assert!(session
            .messages
            .iter()
            .any(|message| message.content.to_text().contains(expected)));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nats_remote_agent_turn_runs_on_shared_worker_and_persists_bare_agent() -> Result<()> {
    harnx_core::require_nextest();
    let sandbox = crate::test_support::TestConfigSandbox::new();
    if !crate::test_support::ensure_test_nats().await {
        return Ok(());
    }

    let config = shared_agent_config(&sandbox).await?;

    let missing_worker =
        std::env::temp_dir().join(format!("missing-harnx-worker-{}", uuid::Uuid::new_v4()));
    // SAFETY: TestConfigSandbox serializes process-environment tests and restores this variable.
    unsafe { std::env::set_var("HARNX_WORKER_BIN", &missing_worker) };

    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_shared_worker(config.clone(), Arc::clone(&calls)).await?;
    let global = Arc::new(RwLock::new(config.clone()));
    let server = Server::new(&global, PathBuf::from("web-assets"));
    let local_worker = server.session_registry.local_worker_for_tests();
    let session_id = format!("remote-turn-{}", uuid::Uuid::new_v4());
    let target = ResolvedAgentTarget::new("sisyphus", "shared");
    let handle = server
        .session_registry
        .get_or_spawn(crate::session_actor::SessionKey::new(
            target.clone(),
            &session_id,
        ));
    let mut events = subscribe(&handle).await;

    let run_id = accepted_run_id(&handle, "complete this remote turn", "remote turn").await?;
    wait_for_run_finished(&mut events, &run_id, "remote turn").await?;

    let resumed_run_id =
        accepted_run_id(&handle, "resume this remote session", "remote resume").await?;
    wait_for_run_finished(&mut events, &resumed_run_id, "remote resume").await?;

    assert!(local_worker.lock().await.is_none());
    assert!(!missing_worker.exists());

    assert_remote_turns_persisted(&config, &target, &session_id, &calls).await?;
    daemon.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_unreachable_remote_session_listing_hides_configured_url() -> Result<()> {
    harnx_core::require_nextest();
    let sandbox = crate::test_support::TestConfigSandbox::new();
    sandbox.write_nats_server(
        "down",
        "url: nats://127.0.0.1:1\nagents:\n  - name: agent\n    role: assistant\n",
    );
    let config = Arc::new(RwLock::new(sandbox.config()));
    let server = Server::new(&config, PathBuf::from("web-assets"));

    let error = server
        .list_sessions_json("agent@down")
        .await
        .expect_err("unreachable session listing must fail");
    let message = error.to_string();
    assert_eq!(message, "NATS cluster 'down' is unavailable");
    assert!(!message.contains("nats://127.0.0.1:1"), "{message}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_unreachable_declared_cluster_stays_listed_and_reports_transport_error() -> Result<()>
{
    harnx_core::require_nextest();
    let sandbox = crate::test_support::TestConfigSandbox::new();
    sandbox.write_nats_server(
        "down",
        "url: nats://127.0.0.1:1\nagents:\n  - name: agent\n    description: Declared but unavailable\n    role: assistant\n",
    );
    let config = sandbox.config();
    let global = Arc::new(RwLock::new(config));
    let server = Server::new(&global, PathBuf::from("web-assets"));

    let listed = server.filter_agents_by_role(None).await?;
    assert!(listed.iter().any(|agent| agent.name() == "agent@down"));

    let key = crate::session_actor::SessionKey::new(
        ResolvedAgentTarget::new("agent", "down"),
        format!("down-cluster-{}", uuid::Uuid::new_v4()),
    );
    let handle = server.session_registry.get_or_spawn(key);
    let error =
        match tokio::time::timeout(CI_SAFE_TIMEOUT, prompt(&handle, "try the declared cluster"))
            .await
            .context("unreachable cluster prompt did not return")?
        {
            crate::session_actor::PromptResult::Rejected { reason } => reason,
            other => anyhow::bail!("unreachable cluster prompt was not rejected: {other:?}"),
        };

    assert_eq!(error, "NATS cluster 'down' is unavailable");
    assert!(!error.contains("nats://127.0.0.1:1"), "{error}");
    assert!(!error.contains("Not Found"), "{error}");
    Ok(())
}
