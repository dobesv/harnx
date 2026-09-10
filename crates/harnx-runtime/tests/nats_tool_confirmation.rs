//! End-to-end coverage for interactive tool confirmation across a NATS worker.
//!
//! The frontend and worker run in different processes, so a hook decision of
//! `ask` must cross NATS before the existing TUI modal can answer it. These
//! tests exercise the real handoff tool and hook server for both answers.

mod common;
#[path = "nats_tool_confirmation/multi_client.rs"]
mod multi_client;

use anyhow::{Context, Result};
use harnx_core::{
    event::NullSink,
    message::{MessageContent, MessageRole},
    require_nextest,
    session::SessionLogEntry,
    tool::ToolCall,
};
use harnx_runtime::{
    client::CompletionTokenUsage,
    config::Config,
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease},
    nats_session::{NatsSession, NatsSessionConfig},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::SessionInitializer,
    nats_tool_confirmation::{ToolConfirmationHandler, ToolConfirmationRequest},
    nats_worker::{
        publish_session_activate, run_worker_daemon, SessionActivate, WorkerDaemonConfig,
    },
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
const SECOND_TARGET_SESSION_ID: &str = "hook-approval-target-second";

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

fn make_reused_tool_call_id_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    let source_calls = Arc::new(AtomicUsize::new(0));
    Arc::new(move |_input, config, _abort| {
        let source_call = (config.read().extract_agent().name() == "approval-gated")
            .then(|| source_calls.fetch_add(1, Ordering::SeqCst));
        Box::pin(async move {
            let target_session_id = match source_call {
                Some(0) => Some(TARGET_SESSION_ID),
                Some(1) => Some(SECOND_TARGET_SESSION_ID),
                _ => None,
            };
            let calls = target_session_id
                .map(|session_id| {
                    ToolCall::new(
                        "target_session_handoff".to_string(),
                        json!({
                            "prompt": "finish after fresh approval",
                            "session_id": session_id,
                        }),
                        Some("provider-reused-call-id".to_string()),
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

async fn activate_durable_prompt(jetstream: &async_nats::jetstream::Context) -> Result<()> {
    activate_durable_text(jetstream, "start handoff").await
}

async fn activate_durable_text(
    jetstream: &async_nats::jetstream::Context,
    text: &str,
) -> Result<()> {
    let log = NatsSessionLog::new(jetstream.clone(), SOURCE_SESSION_ID);
    log.append_event_async(&SessionLogEntry::Message {
        id: Some(uuid::Uuid::new_v4().to_string()),
        role: MessageRole::User,
        content: MessageContent::Text(text.to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    publish_session_activate(jetstream, "local", &SessionActivate::new(SOURCE_SESSION_ID)).await?;
    Ok(())
}

async fn wait_for_source_entry<F>(
    jetstream: &async_nats::jetstream::Context,
    mut predicate: F,
) -> Result<Vec<(u64, SessionLogEntry)>>
where
    F: FnMut(&SessionLogEntry) -> bool,
{
    let log = NatsSessionLog::new(jetstream.clone(), SOURCE_SESSION_ID);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let entries = log.load_events_async().await?;
        if entries.iter().any(|(_, entry)| predicate(entry)) {
            return Ok(entries);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for source entry: {entries:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn wait_for_source_count<F>(
    jetstream: &async_nats::jetstream::Context,
    mut predicate: F,
    expected: usize,
) -> Result<Vec<(u64, SessionLogEntry)>>
where
    F: FnMut(&SessionLogEntry) -> bool,
{
    let log = NatsSessionLog::new(jetstream.clone(), SOURCE_SESSION_ID);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let entries = log.load_events_async().await?;
        if entries.iter().filter(|(_, entry)| predicate(entry)).count() >= expected {
            return Ok(entries);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expected} source entries: {entries:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_hitl_request_ends_activation_without_turn_end() -> Result<()> {
    let Some(harness) = ConfirmationHarness::start().await? else {
        return Ok(());
    };
    activate_durable_prompt(&harness.jetstream).await?;
    let entries = wait_for_source_entry(&harness.jetstream, |entry| {
        matches!(entry, SessionLogEntry::HitlApprovalRequested { .. })
    })
    .await?;
    assert!(entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::HitlApprovalRequested {
            tool_call_id,
            summary,
            fence_token,
        } if tool_call_id == "hook-approval-handoff"
            && summary == "Approve the handoff?"
            && *fence_token > 0
    )));
    assert!(!entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::TurnEnd { .. }
            | SessionLogEntry::ToolResults { .. }
            | SessionLogEntry::HitlApprovalDecision { .. }
    )));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_approval_does_not_authorize_reused_tool_call_id() -> Result<()> {
    let Some(harness) =
        ConfirmationHarness::start_with_call_fn(make_reused_tool_call_id_call_fn()).await?
    else {
        return Ok(());
    };
    activate_durable_text(&harness.jetstream, "first reused-id handoff").await?;
    wait_for_source_count(
        &harness.jetstream,
        |entry| matches!(entry, SessionLogEntry::HitlApprovalRequested { .. }),
        1,
    )
    .await?;
    assert!(
        harness
            .source
            .decide_hitl_approval("provider-reused-call-id", true, None)
            .await?
    );
    wait_for_source_count(
        &harness.jetstream,
        |entry| matches!(entry, SessionLogEntry::TurnEnd { .. }),
        1,
    )
    .await?;
    wait_for_target_turn(&NatsSessionLog::new(
        harness.jetstream.clone(),
        TARGET_SESSION_ID,
    ))
    .await?;

    activate_durable_text(&harness.jetstream, "second reused-id handoff").await?;
    let before_fresh_decision = wait_for_source_count(
        &harness.jetstream,
        |entry| matches!(entry, SessionLogEntry::HitlApprovalRequested { .. }),
        2,
    )
    .await?;
    assert_eq!(
        before_fresh_decision
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        1,
        "historical approval must not auto-decide reused provider ID"
    );
    assert_eq!(
        before_fresh_decision
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { .. }))
            .count(),
        1,
        "second tool call must not execute before fresh approval"
    );
    assert!(
        NatsSessionLog::new(harness.jetstream.clone(), SECOND_TARGET_SESSION_ID)
            .load_events_async()
            .await?
            .is_empty(),
        "second handoff target must remain untouched before fresh approval"
    );

    assert!(
        harness
            .source
            .decide_hitl_approval("provider-reused-call-id", true, None)
            .await?,
        "reused provider ID must accept a fresh decision for current request"
    );
    wait_for_target_turn(&NatsSessionLog::new(
        harness.jetstream.clone(),
        SECOND_TARGET_SESSION_ID,
    ))
    .await?;
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_hitl_duplicate_approval_has_one_decision_and_executes_after_it() -> Result<()> {
    let Some(harness) = ConfirmationHarness::start().await? else {
        return Ok(());
    };
    activate_durable_prompt(&harness.jetstream).await?;
    wait_for_source_entry(&harness.jetstream, |entry| {
        matches!(entry, SessionLogEntry::HitlApprovalRequested { .. })
    })
    .await?;
    let (first, second) = tokio::join!(
        harness
            .source
            .decide_hitl_approval("hook-approval-handoff", true, None),
        harness
            .source
            .decide_hitl_approval("hook-approval-handoff", true, None)
    );
    let first = first?;
    let second = second?;
    assert_ne!(
        first, second,
        "exactly one concurrent approval must report that it applied"
    );
    let entries = wait_for_source_entry(&harness.jetstream, |entry| {
        matches!(entry, SessionLogEntry::ToolResults { .. })
    })
    .await?;
    let decision_seqs: Vec<_> = entries
        .iter()
        .filter_map(|(seq, entry)| {
            matches!(entry, SessionLogEntry::HitlApprovalDecision { .. }).then_some(*seq)
        })
        .collect();
    assert_eq!(decision_seqs.len(), 1);
    let result_seq = entries
        .iter()
        .find_map(|(seq, entry)| {
            matches!(entry, SessionLogEntry::ToolResults { .. }).then_some(*seq)
        })
        .expect("tool results");
    assert!(decision_seqs[0] < result_seq);
    wait_for_target_turn(&NatsSessionLog::new(
        harness.jetstream.clone(),
        TARGET_SESSION_ID,
    ))
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_hitl_denial_writes_audit_and_model_visible_tool_result() -> Result<()> {
    let Some(harness) = ConfirmationHarness::start().await? else {
        return Ok(());
    };
    activate_durable_prompt(&harness.jetstream).await?;
    wait_for_source_entry(&harness.jetstream, |entry| {
        matches!(entry, SessionLogEntry::HitlApprovalRequested { .. })
    })
    .await?;
    assert!(
        harness
            .source
            .decide_hitl_approval(
                "hook-approval-handoff",
                false,
                Some("Denied in durable test".to_string()),
            )
            .await?
    );
    let entries = wait_for_source_entry(&harness.jetstream, |entry| {
        matches!(entry, SessionLogEntry::ToolResults { .. })
    })
    .await?;
    assert!(entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::HitlApprovalDecision {
            approved: false,
            note: Some(note),
            ..
        } if note == "Denied in durable test"
    )));
    assert!(entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::ToolResults { results, .. }
            if results.iter().any(|result| {
                result.id.as_deref() == Some("hook-approval-handoff")
                    && result.output["blocked_by_hook"] == json!(true)
                    && result.output["error"] == json!("Denied in durable test")
            })
    )));
    let target_entries = NatsSessionLog::new(harness.jetstream.clone(), TARGET_SESSION_ID)
        .load_events_async()
        .await?;
    assert!(target_entries.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_hitl_restart_recovers_pending_and_executes_once() -> Result<()> {
    let Some(mut harness) = ConfirmationHarness::start().await? else {
        return Ok(());
    };
    harness.daemon.abort();
    let _ = (&mut harness.daemon).await;

    let log = NatsSessionLog::new(harness.jetstream.clone(), SOURCE_SESSION_ID);
    log.append_event_async(&SessionLogEntry::Message {
        id: Some(uuid::Uuid::new_v4().to_string()),
        role: MessageRole::User,
        content: MessageContent::Text("start handoff".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: "activation completed".to_string(),
        thought: None,
        calls: vec![ToolCall::new(
            "target_session_handoff".to_string(),
            json!({
                "prompt": "finish after approval",
                "session_id": TARGET_SESSION_ID,
            }),
            Some("hook-approval-handoff".to_string()),
            None,
        )],
        timestamp: None,
        fence_token: Some(1),
    })
    .await?;
    log.append_event_async(&SessionLogEntry::HitlApprovalRequested {
        tool_call_id: "hook-approval-handoff".to_string(),
        summary: "Approve the handoff?".to_string(),
        fence_token: 1,
    })
    .await?;

    let (config, _, _) = harness._environment.load().await?;
    let mut replacement = tokio::spawn(run_worker_daemon(
        config,
        WorkerDaemonConfig::managing("local", "worker-tool-confirmation"),
        Some(make_handoff_call_fn()),
        None,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let decision = harness
        .source
        .decide_hitl_approval("hook-approval-handoff", true, None);
    tokio::select! {
        result = decision => assert!(result?),
        result = &mut replacement => anyhow::bail!("replacement worker exited early: {result:?}"),
    }
    let entries = wait_for_source_entry(&harness.jetstream, |entry| {
        matches!(entry, SessionLogEntry::ToolResults { .. })
    })
    .await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { .. }))
            .count(),
        1
    );
    let target_entries = wait_for_target_turn(&NatsSessionLog::new(
        harness.jetstream.clone(),
        TARGET_SESSION_ID,
    ))
    .await?;
    assert_eq!(
        target_entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::Message { role, .. } if role.is_assistant()))
            .count(),
        1,
        "restart handoff target must execute exactly once"
    );
    replacement.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_handoff_stale_worker_loses_decision_and_execution_race() -> Result<()> {
    require_nextest();
    let Some(server) = common::spawn_nats_server().await? else {
        return Ok(());
    };
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let lease_config = NatsLeaseConfig {
        ttl: std::time::Duration::from_secs(1),
        renew_interval: std::time::Duration::from_millis(200),
        replicas: 1,
        tombstone_ttl: std::time::Duration::from_secs(10),
        ..Default::default()
    };
    let stale_lease = Arc::new(
        NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: jetstream.clone(),
            session_id: SOURCE_SESSION_ID,
            worker_id: "stale-hitl-worker".to_string(),
            generation: 1,
            config: lease_config.clone(),
            session_metadata: None,
        })
        .await?
        .context("stale worker acquires lease")?,
    );
    let log = NatsSessionLog::new(jetstream.clone(), SOURCE_SESSION_ID);
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: "pending handoff".to_string(),
        thought: None,
        calls: vec![ToolCall::new(
            "target_session_handoff".to_string(),
            json!({"session_id": TARGET_SESSION_ID, "prompt": "run once"}),
            Some("handoff-race-call".to_string()),
            None,
        )],
        timestamp: None,
        fence_token: Some(stale_lease.fence_token()),
    })
    .await?;
    log.append_event_async(&SessionLogEntry::HitlApprovalRequested {
        tool_call_id: "handoff-race-call".to_string(),
        summary: "Approve handoff".to_string(),
        fence_token: stale_lease.fence_token(),
    })
    .await?;
    let stale_snapshot = log.load_events_async().await?;
    let stale_expected = stale_snapshot.last().expect("request tail").0;

    stale_lease.stop_renewal_for_test().await;
    let replacement_lease = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(lease) = NatsSessionLease::acquire(NatsLeaseAcquireParams {
                jetstream: jetstream.clone(),
                session_id: SOURCE_SESSION_ID,
                worker_id: "replacement-hitl-worker".to_string(),
                generation: 1,
                config: lease_config.clone(),
                session_metadata: None,
            })
            .await?
            {
                return Result::<_>::Ok(Arc::new(lease));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .context("replacement worker did not acquire expired lease")??;
    assert!(
        stale_lease.is_held(),
        "stale worker must still believe it owns lease during handoff race"
    );

    let replacement_sink = harnx_runtime::nats_worker::FencedSessionLogSink::new(
        harnx_runtime::nats_worker::NatsSessionLogBackend::new(
            jetstream.clone(),
            SOURCE_SESSION_ID,
        ),
        Arc::clone(&replacement_lease),
    );
    let replacement_entry = SessionLogEntry::HitlApprovalDecision {
        tool_call_id: "handoff-race-call".to_string(),
        approved: true,
        note: None,
        fence_token: replacement_lease.fence_token(),
    };
    assert!(
        replacement_sink
            .append_hitl_event_cas(&replacement_entry, stale_expected)
            .await?
            .is_some(),
        "replacement worker must win decision CAS"
    );

    let stale_sink = harnx_runtime::nats_worker::FencedSessionLogSink::new(
        harnx_runtime::nats_worker::NatsSessionLogBackend::new(
            jetstream.clone(),
            SOURCE_SESSION_ID,
        ),
        Arc::clone(&stale_lease),
    );
    let stale_entry = SessionLogEntry::HitlApprovalDecision {
        tool_call_id: "handoff-race-call".to_string(),
        approved: false,
        note: Some("stale denial".to_string()),
        fence_token: stale_lease.fence_token(),
    };
    assert!(
        stale_sink
            .append_hitl_event_cas(&stale_entry, stale_expected)
            .await?
            .is_none(),
        "stale worker decision must lose stream-tail CAS"
    );

    let execution_count = AtomicUsize::new(0);
    if stale_lease.revalidate_ownership().await? {
        execution_count.fetch_add(1, Ordering::SeqCst);
    }
    if replacement_lease.revalidate_ownership().await? {
        execution_count.fetch_add(1, Ordering::SeqCst);
    }
    assert_eq!(
        execution_count.load(Ordering::SeqCst),
        1,
        "only current lease holder may cross tool execution boundary"
    );

    let entries = log.load_events_async().await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::HitlApprovalDecision { .. }))
            .count(),
        1,
        "handoff race must durably apply exactly one decision"
    );
    replacement_lease.release().await?;
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
