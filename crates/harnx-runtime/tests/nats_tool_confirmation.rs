//! End-to-end coverage for interactive tool confirmation across a NATS worker.
//!
//! The frontend and worker run in different processes, so a hook decision of
//! `ask` must cross NATS before the existing TUI modal can answer it. These
//! tests exercise the real handoff tool and hook server for both answers.

mod common;
#[path = "nats_tool_confirmation/multi_client.rs"]
mod multi_client;

use anyhow::{Context, Result};
use harnx_core::{event::NullSink, require_nextest, session::SessionLogEntry, tool::ToolCall};
use harnx_runtime::{
    client::CompletionTokenUsage,
    config::Config,
    nats_session::{NatsSession, NatsSessionConfig},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::SessionInitializer,
    nats_tool_confirmation::{ToolConfirmationHandler, ToolConfirmationRequest},
    nats_worker::{run_worker_daemon, WorkerDaemonConfig},
    utils::create_abort_signal,
};
use parking_lot::{Mutex, RwLock};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

const SOURCE_SESSION_ID: &str = "hook-approval-source";
const TARGET_SESSION_ID: &str = "hook-approval-target";

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

struct TestEnvironment {
    config_dir: PathBuf,
    _root: tempfile::TempDir,
    _guards: Vec<EnvVarGuard>,
}

impl TestEnvironment {
    fn new(server_url: &str) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let config_dir = root.path().join("config");
        let data_dir = root.path().join("data");
        let state_dir = root.path().join("state");
        for dir in [
            config_dir.clone(),
            config_dir.join("agents"),
            config_dir.join("clients"),
            config_dir.join("nats_servers"),
            data_dir.clone(),
            state_dir.clone(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        write_test_config(&config_dir, server_url)?;
        let guards = vec![
            EnvVarGuard::set_path("HARNX_CONFIG_DIR", &config_dir),
            EnvVarGuard::set_path("HARNX_DATA_DIR", &data_dir),
            EnvVarGuard::set_path("HARNX_STATE_DIR", &state_dir),
            EnvVarGuard::set_path("HARNX_NATS_URL", Path::new(server_url)),
            EnvVarGuard::set_path("HARNX_NATS_TOKEN", Path::new("test-token")),
        ];
        Ok(Self {
            config_dir,
            _root: root,
            _guards: guards,
        })
    }

    async fn load(
        &self,
    ) -> Result<(
        Arc<RwLock<Config>>,
        async_nats::Client,
        async_nats::jetstream::Context,
    )> {
        let config = Arc::new(RwLock::new(Config::load_from_file(
            &self.config_dir.join("config.yaml"),
        )?));
        let cfg = config.read().clone();
        let client = cfg.nats_client("local").await?;
        let jetstream = cfg.nats_jetstream("local").await?;
        Ok((config, client, jetstream))
    }
}

struct ConfirmationHarness {
    source: NatsSession,
    client: async_nats::Client,
    jetstream: async_nats::jetstream::Context,
    daemon: tokio::task::JoinHandle<Result<()>>,
    _server: common::NatsServerHandle,
    _environment: TestEnvironment,
}

impl ConfirmationHarness {
    async fn start() -> Result<Option<Self>> {
        Self::start_with_call_fn(make_handoff_call_fn()).await
    }

    async fn start_with_call_fn(
        call_fn: harnx_runtime::agent_loop::AgentCallFn,
    ) -> Result<Option<Self>> {
        require_nextest();
        let Some(server) = common::spawn_nats_server().await? else {
            return Ok(None);
        };
        ensure_hook_server_binary().await?;
        let environment = TestEnvironment::new(server.url())?;
        let (config, client, jetstream) = environment.load().await?;
        let daemon = tokio::spawn(run_worker_daemon(
            config,
            WorkerDaemonConfig::managing("local", "worker-tool-confirmation"),
            Some(call_fn),
            None,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let source = NatsSession::new(
            NatsSessionConfig {
                cluster: "local".to_string(),
                initializer: SessionInitializer::named("approval-gated", Default::default()),
                session_id: Some(SOURCE_SESSION_ID.to_string()),
                activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
            },
            client.clone(),
            jetstream.clone(),
            create_abort_signal(),
        )
        .await?;
        Ok(Some(Self {
            source,
            client,
            jetstream,
            daemon,
            _server: server,
            _environment: environment,
        }))
    }

    async fn run(&self, approved: bool) -> Result<ConfirmationOutcome> {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_handler = Arc::clone(&requests);
        let handler: Arc<ToolConfirmationHandler> = Arc::new(move |request| {
            requests_for_handler.lock().push(request);
            Box::pin(async move { approved })
        });
        self.source
            .run_turn_with_tool_confirmation("start handoff", Arc::new(NullSink), None, handler)
            .await?;
        let source_entries = NatsSessionLog::new(self.jetstream.clone(), SOURCE_SESSION_ID)
            .load_events_async()
            .await?;
        let target_log = NatsSessionLog::new(self.jetstream.clone(), TARGET_SESSION_ID);
        let target_entries = if approved {
            wait_for_target_turn(&target_log).await?
        } else {
            target_log.load_events_async().await?
        };
        let requests = requests.lock().clone();
        Ok(ConfirmationOutcome {
            requests,
            source_entries,
            target_entries,
        })
    }
}

impl Drop for ConfirmationHarness {
    fn drop(&mut self) {
        self.daemon.abort();
    }
}

struct ConfirmationOutcome {
    requests: Vec<ToolConfirmationRequest>,
    source_entries: Vec<(u64, SessionLogEntry)>,
    target_entries: Vec<(u64, SessionLogEntry)>,
}

fn write_test_config(config_dir: &Path, server_url: &str) -> Result<()> {
    std::fs::write(config_dir.join("config.yaml"), "model: openai:test-model\n")?;
    std::fs::write(
        config_dir.join("nats_servers/local.yaml"),
        format!("url: {server_url}\n"),
    )?;
    std::fs::write(
        config_dir.join("clients/openai.yaml"),
        "type: openai\napi_key: sk-test\nmodels:\n  - name: test-model\n    type: chat\n    max_input_tokens: 4096\n",
    )?;
    std::fs::write(
        config_dir.join("agents/approval-gated.md"),
        "---\nmodel: openai:test-model\nuse_tools:\n- target_session_handoff\nhooks:\n  entries:\n    - command: >-\n        harnx-claude-compatible-hook-server\n        --event PreToolUse\n        --matcher '^target_session_handoff$'\n        --jaq '{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"Approve the handoff?\"}}'\n---\nApproval-gated agent instructions\n",
    )?;
    std::fs::write(
        config_dir.join("agents/target.md"),
        "---\nmodel: openai:test-model\n---\nTarget agent instructions\n",
    )?;
    Ok(())
}

fn make_handoff_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    let source_called = Arc::new(AtomicBool::new(false));
    Arc::new(move |_input, config, _abort| {
        let is_first_source_call = config.read().extract_agent().name() == "approval-gated"
            && !source_called.swap(true, Ordering::SeqCst);
        Box::pin(async move {
            let calls = is_first_source_call
                .then(|| {
                    ToolCall::new(
                        "target_session_handoff".to_string(),
                        json!({
                            "prompt": "finish after approval",
                            "session_id": TARGET_SESSION_ID,
                        }),
                        Some("hook-approval-handoff".to_string()),
                        None,
                    )
                })
                .into_iter()
                .collect();
            Ok((
                "activation completed".to_string(),
                None,
                calls,
                CompletionTokenUsage::default(),
            ))
        })
    })
}

fn make_queued_handoff_call_fn(
    first_call_started: Arc<tokio::sync::Notify>,
    release_first_call: Arc<tokio::sync::Notify>,
) -> harnx_runtime::agent_loop::AgentCallFn {
    let source_calls = Arc::new(AtomicUsize::new(0));
    Arc::new(move |_input, config, _abort| {
        let source_call = (config.read().extract_agent().name() == "approval-gated")
            .then(|| source_calls.fetch_add(1, Ordering::SeqCst));
        let first_call_started = Arc::clone(&first_call_started);
        let release_first_call = Arc::clone(&release_first_call);
        Box::pin(async move {
            if source_call == Some(0) {
                first_call_started.notify_one();
                release_first_call.notified().await;
            }
            let calls = (source_call == Some(1))
                .then(|| {
                    ToolCall::new(
                        "target_session_handoff".to_string(),
                        json!({
                            "prompt": "finish queued handoff after approval",
                            "session_id": TARGET_SESSION_ID,
                        }),
                        Some("queued-hook-approval-handoff".to_string()),
                        None,
                    )
                })
                .into_iter()
                .collect();
            Ok((
                "activation completed".to_string(),
                None,
                calls,
                CompletionTokenUsage::default(),
            ))
        })
    })
}

async fn ensure_hook_server_binary() -> Result<()> {
    let mut path = std::env::current_exe()?;
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    path.push(if cfg!(windows) {
        "harnx-claude-compatible-hook-server.exe"
    } else {
        "harnx-claude-compatible-hook-server"
    });
    if tokio::fs::try_exists(&path).await? {
        return Ok(());
    }
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .context("resolve workspace root")?
        .to_path_buf();
    let status =
        tokio::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(["build", "-p", "harnx-claude-compatible-hook-server"])
            .current_dir(workspace)
            .status()
            .await?;
    anyhow::ensure!(status.success(), "building hook server failed");
    Ok(())
}

async fn wait_for_target_turn(log: &NatsSessionLog) -> Result<Vec<(u64, SessionLogEntry)>> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let entries = log.load_events_async().await?;
        let completed = entries.iter().any(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::Message { role, content, .. }
                    if role.is_assistant() && content.to_text() == "activation completed"
            )
        }) && entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. }));
        if completed {
            return Ok(entries);
        }
        anyhow::ensure!(
            !entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. })),
            "approved handoff target failed: {entries:?}"
        );
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "approved handoff target was not activated: {entries:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Full regression path for the reported TUI failure. Before the confirmation
/// bridge, the headless worker read EOF from stdin and converted `ask` into a
/// `blocked_by_hook` result without showing the modal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoff_ask_reaches_frontend_and_approval_activates_target() -> Result<()> {
    let Some(harness) = ConfirmationHarness::start().await? else {
        return Ok(());
    };
    let outcome = harness.run(true).await?;
    assert_eq!(
        outcome.requests,
        vec![ToolConfirmationRequest {
            session_id: SOURCE_SESSION_ID.to_string(),
            tool_call_id: Some("hook-approval-handoff".to_string()),
            tool_name: "target_session_handoff".to_string(),
            arguments: json!({
                "prompt": "finish after approval",
                "session_id": TARGET_SESSION_ID,
            }),
            reason: Some("Approve the handoff?".to_string()),
        }]
    );
    assert!(outcome.target_entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::Message { role, .. } if role.is_assistant()
    )));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_handoff_ask_stays_blocked_and_does_not_activate_target() -> Result<()> {
    let Some(harness) = ConfirmationHarness::start().await? else {
        return Ok(());
    };
    let outcome = harness.run(false).await?;
    assert_eq!(outcome.requests.len(), 1);
    assert!(outcome.source_entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::ToolResults { results, .. }
            if results.iter().any(|result| {
                result.name == "target_session_handoff"
                    && result.output["blocked_by_hook"] == json!(true)
                    && result.switch_agent.is_none()
            })
    )));
    assert!(
        outcome.target_entries.is_empty(),
        "denied handoff must not create target transcript entries"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_turn_interrupts_pending_confirmation_request() -> Result<()> {
    let Some(harness) = ConfirmationHarness::start().await? else {
        return Ok(());
    };
    let confirmation_requested = Arc::new(tokio::sync::Notify::new());
    let requested_for_handler = Arc::clone(&confirmation_requested);
    let handler: Arc<ToolConfirmationHandler> = Arc::new(move |_request| {
        requested_for_handler.notify_one();
        Box::pin(std::future::pending())
    });
    let turn = harness.source.run_turn_with_tool_confirmation(
        "start handoff",
        Arc::new(NullSink),
        None,
        handler,
    );
    tokio::pin!(turn);

    tokio::select! {
        _ = confirmation_requested.notified() => {}
        result = &mut turn => {
            anyhow::bail!("turn ended before requesting confirmation: {result:?}");
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            anyhow::bail!("worker did not request tool confirmation");
        }
    }

    harnx_runtime::send_control_command(
        &harness.client,
        SOURCE_SESSION_ID,
        harnx_runtime::ControlCommand::Cancel,
    )
    .await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), &mut turn)
        .await
        .context("turn did not stop after cancellation interrupted confirmation")??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_continuation_handoff_reuses_live_frontend_confirmation_route() -> Result<()> {
    let first_call_started = Arc::new(tokio::sync::Notify::new());
    let release_first_call = Arc::new(tokio::sync::Notify::new());
    let Some(harness) = ConfirmationHarness::start_with_call_fn(make_queued_handoff_call_fn(
        Arc::clone(&first_call_started),
        Arc::clone(&release_first_call),
    ))
    .await?
    else {
        return Ok(());
    };

    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_for_handler = Arc::clone(&requests);
    let handler: Arc<ToolConfirmationHandler> = Arc::new(move |request| {
        requests_for_handler.lock().push(request);
        Box::pin(async { true })
    });
    let route = harness.source.tool_confirmation_route(handler).await?;
    let turn = harness.source.run_turn_with_tool_confirmation_route(
        "start slow turn",
        Arc::new(NullSink),
        None,
        &route,
    );
    tokio::pin!(turn);

    tokio::select! {
        _ = first_call_started.notified() => {}
        result = &mut turn => anyhow::bail!("first turn ended before queued input: {result:?}"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            anyhow::bail!("first source call did not start")
        }
    }
    harness
        .source
        .enqueue_text_with_tool_confirmation("perform queued handoff", &route)
        .await?
        .into_activation_result()?;
    release_first_call.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(30), &mut turn)
        .await
        .context("direct turn did not finish while worker drained continuation")??;

    let target_entries = wait_for_target_turn(&NatsSessionLog::new(
        harness.jetstream.clone(),
        TARGET_SESSION_ID,
    ))
    .await?;
    assert!(target_entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::Message { role, .. } if role.is_assistant()
    )));
    assert_eq!(
        requests.lock().as_slice(),
        &[ToolConfirmationRequest {
            session_id: SOURCE_SESSION_ID.to_string(),
            tool_call_id: Some("queued-hook-approval-handoff".to_string()),
            tool_name: "target_session_handoff".to_string(),
            arguments: json!({
                "prompt": "finish queued handoff after approval",
                "session_id": TARGET_SESSION_ID,
            }),
            reason: Some("Approve the handoff?".to_string()),
        }]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_continuation_handoff_denies_after_frontend_route_closes() -> Result<()> {
    let first_call_started = Arc::new(tokio::sync::Notify::new());
    let release_first_call = Arc::new(tokio::sync::Notify::new());
    let Some(harness) = ConfirmationHarness::start_with_call_fn(make_queued_handoff_call_fn(
        Arc::clone(&first_call_started),
        Arc::clone(&release_first_call),
    ))
    .await?
    else {
        return Ok(());
    };

    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_for_handler = Arc::clone(&request_count);
    let handler: Arc<ToolConfirmationHandler> = Arc::new(move |_| {
        request_count_for_handler.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { true })
    });
    let route = harness.source.tool_confirmation_route(handler).await?;
    harness
        .source
        .enqueue_text_with_tool_confirmation("start slow turn", &route)
        .await?
        .into_activation_result()?;
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        first_call_started.notified(),
    )
    .await
    .context("first source call did not start")?;
    harness
        .source
        .enqueue_text_with_tool_confirmation("perform queued handoff", &route)
        .await?
        .into_activation_result()?;
    route.close().await;
    release_first_call.notify_one();

    let source_entries = wait_for_blocked_handoff(&harness.jetstream).await?;
    assert!(source_entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::ToolResults { results, .. }
            if results.iter().any(|result| {
                result.name == "target_session_handoff"
                    && result.output["blocked_by_hook"] == json!(true)
                    && result.switch_agent.is_none()
            })
    )));
    let target_entries = NatsSessionLog::new(harness.jetstream.clone(), TARGET_SESSION_ID)
        .load_events_async()
        .await?;
    assert!(
        target_entries.is_empty(),
        "dead frontend route must deny without activating target"
    );
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        0,
        "closed responder must not surface a confirmation request"
    );
    Ok(())
}

async fn wait_for_blocked_handoff(
    jetstream: &async_nats::jetstream::Context,
) -> Result<Vec<(u64, SessionLogEntry)>> {
    let log = NatsSessionLog::new(jetstream.clone(), SOURCE_SESSION_ID);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let entries = log.load_events_async().await?;
        if entries.iter().any(|(_, entry)| {
            matches!(
                entry,
                SessionLogEntry::ToolResults { results, .. }
                    if results.iter().any(|result| {
                        result.name == "target_session_handoff"
                            && result.output["blocked_by_hook"] == json!(true)
                    })
            )
        }) {
            return Ok(entries);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "queued handoff did not fail closed: {entries:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
