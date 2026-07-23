//! Integration test: drive run_agent_loop with NATS-backed persistence.
//!
//! Validates end-to-end persistence of a full turn via NatsSessionLog.

mod common;

use anyhow::Result;
use common::spawn_nats_server;
use harnx_core::{
    message::MessageRole,
    require_nextest,
    session::SessionLogEntry,
    session_reconstruct::{reconstruct_state, reconstruct_state_from_nats, TurnStatus},
    tool::ToolCall,
};
use harnx_runtime::{
    client::CompletionTokenUsage,
    config::{Config, NatsServerConfig},
    nats_lease::NatsLeaseConfig,
    nats_session_log::NatsSessionLog,
    nats_worker::{
        publish_session_activate, run_agent_loop_with_nats, run_worker_daemon,
        NatsSessionLogBackend, RunAgentLoopArgs, SessionActivate, WorkerDaemonConfig,
    },
    utils::create_abort_signal,
};
use std::sync::LazyLock;

static MID_ROUND_APPEND_READY: LazyLock<Notify> = LazyLock::new(Notify::new);
static MID_ROUND_APPEND_DONE: LazyLock<Notify> = LazyLock::new(Notify::new);
static MID_ROUND_FINAL_CALLS: AtomicUsize = AtomicUsize::new(0);
static MID_ROUND_RELOAD_SEEN: AtomicUsize = AtomicUsize::new(0);
static END_TURN_APPEND_READY: LazyLock<Notify> = LazyLock::new(Notify::new);
static END_TURN_APPEND_DONE: LazyLock<Notify> = LazyLock::new(Notify::new);
static END_TURN_CALLS: AtomicUsize = AtomicUsize::new(0);
static RETRACTED_ORPHAN_ACTIVATION_CALLS: AtomicUsize = AtomicUsize::new(0);
use parking_lot::RwLock;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, Notify};

/// Stub LLM that returns: assistant with tool call on turn 1, final text on turn 2.
/// Description of a single NATS server entry for test configs.
struct NatsServerSpec<'a> {
    name: &'a str,
    url: &'a str,
    token: Option<&'a str>,
}

fn local_nats_config(spec: NatsServerSpec<'_>) -> Config {
    let mut config = Config {
        nats_servers: vec![NatsServerConfig {
            name: spec.name.to_string(),
            url: spec.url.to_string(),
            token: spec.token.map(str::to_string),
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: vec![],
        }],
        ..Default::default()
    };
    config.dry_run = false;
    config
}

fn local_nats_runtime_config(server_url: &str) -> Arc<RwLock<Config>> {
    Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url: server_url,
        token: None,
    })))
}

async fn require_nats_server() -> Result<Option<common::NatsServerHandle>> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("skipping: nats-server not available");
        return Ok(None);
    };
    Ok(Some(server))
}

async fn append_retracted_user_message(
    log: &NatsSessionLog,
    message_id: &str,
    content: &str,
) -> Result<u64> {
    let seq = log
        .append_event_async(&append_user_message_entry(message_id, content))
        .await?;
    log.append_event_async(&SessionLogEntry::EditEntries {
        from: seq as usize,
        to: seq as usize,
        replacements: vec![],
    })
    .await?;
    Ok(seq)
}

async fn assert_single_prompt(prompts: &Arc<AsyncMutex<Vec<String>>>, expected: &str) {
    let captured = prompts.lock().await.clone();
    assert_eq!(
        captured.len(),
        1,
        "worker should have made exactly one call"
    );
    assert_eq!(
        captured[0], expected,
        "worker input should match expected message"
    );
}

fn assert_single_assistant_contains(entries: &[(u64, SessionLogEntry)], needle: &str) {
    let assistant_texts = final_assistant_texts(entries);
    assert_eq!(
        assistant_texts.len(),
        1,
        "exactly one assistant message should be persisted"
    );
    assert!(
        assistant_texts[0].contains(needle),
        "assistant should contain {needle:?}, got: {:?}",
        assistant_texts
    );
}

fn count_tool_results_with_id(entries: &[(u64, SessionLogEntry)], call_id: &str) -> usize {
    entries
        .iter()
        .filter(|(_, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => results
                .iter()
                .any(|result| result.id.as_deref() == Some(call_id)),
            _ => false,
        })
        .count()
}

async fn wait_for_worker_daemon_idle(metrics_before_lease_acquisitions: u64) -> Result<()> {
    wait_until(Duration::from_secs(10), || {
        harnx_runtime::nats_metrics::snapshot().lease_acquisitions
            > metrics_before_lease_acquisitions
    })
    .await?;
    wait_until(Duration::from_secs(10), || {
        harnx_runtime::nats_metrics::snapshot().active_sessions_per_worker == 0
    })
    .await
}

fn assert_no_resume_or_interrupt_metric_delta(
    metrics_before: harnx_runtime::nats_metrics::NatsMetricsSnapshot,
    metrics_after: harnx_runtime::nats_metrics::NatsMetricsSnapshot,
) {
    assert_eq!(
        metrics_after.resumes, metrics_before.resumes,
        "retracted tool round must not trigger a resume/orphan repair"
    );
    assert_eq!(
        metrics_after.interrupt_errors_synthesized, metrics_before.interrupt_errors_synthesized,
        "retracted tool round must not synthesize an interrupt-error result"
    );
}

fn assert_retracted_orphan_absent(entries: &[(u64, SessionLogEntry)], call_id: &str) -> Result<()> {
    assert_eq!(
        count_tool_results_with_id(entries, call_id),
        0,
        "worker must not synthesize or replay ToolResults for retracted orphan tool call"
    );
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(entries)?;
    assert!(
        !effective.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::ToolCalls { calls, .. }
                if calls.iter().any(|call| call.id.as_deref() == Some(call_id))
        )),
        "effective log must not contain retracted tool call"
    );
    assert!(
        !effective.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::ToolResults { results, .. }
                if results.iter().any(|result| result.id.as_deref() == Some(call_id))
        )),
        "effective log must not contain resurrected ToolResults for retracted tool call"
    );
    Ok(())
}
fn session_header(session_id: &str) -> SessionLogEntry {
    SessionLogEntry::Header {
        model_id: "test-model".to_string(),
        temperature: None,
        top_p: None,
        use_tools: None,
        save_session: Some(true),
        compress_threshold: None,
        agent_name: None,
        session_id: Some(session_id.to_string()),
        working_dir: None,
        git_branch: None,
        git_remote: None,
        terminal_session_id: None,
        agent_variables: Default::default(),
        agent_instructions: String::new(),
        model_fallbacks: vec![],
        compaction_agent: None,
    }
}

async fn seed_session_and_attach_runtime(
    global_config: &Arc<RwLock<Config>>,
    jetstream: async_nats::jetstream::Context,
    session_id: &str,
) -> Result<NatsSessionLog> {
    let backend = NatsSessionLogBackend::new(jetstream.clone(), session_id);
    backend.append_event_blocking(&session_header(session_id))?;

    let log = NatsSessionLog::new(jetstream, session_id);
    let entries = log.load_events_async().await?;
    let mut session =
        harnx_runtime::nats_session_log::load_session_from_entries(&entries, session_id)?;
    let runtime = std::sync::Arc::new(backend.clone())
        as std::sync::Arc<dyn harnx_runtime::config::session::SessionAppendSink>;
    session.runtime = Some(std::sync::Arc::new(runtime));
    global_config.write().session = Some(session);
    Ok(log)
}

struct WorkerTurnLabels<'a> {
    cluster_key: &'a str,
    session_id: &'a str,
    prompt: &'a str,
}

struct WorkerTurnParams<'a> {
    global_config: Arc<RwLock<Config>>,
    labels: WorkerTurnLabels<'a>,
    call_fn: harnx_runtime::agent_loop::AgentCallFn,
    lease: Option<Arc<harnx_runtime::nats_lease::NatsSessionLease>>,
}

async fn run_worker_turn(params: WorkerTurnParams<'_>) -> Result<()> {
    let WorkerTurnParams {
        global_config,
        labels:
            WorkerTurnLabels {
                cluster_key,
                session_id,
                prompt,
            },
        call_fn,
        lease,
    } = params;
    let input = harnx_runtime::config::input::from_str(&global_config, prompt, None);
    run_agent_loop_with_nats(RunAgentLoopArgs {
        cluster_key,
        session_id,
        config: global_config,
        initial_input: input,
        abort_signal: create_abort_signal(),
        call_fn: Some(call_fn),
        lease,
        lease_config: NatsLeaseConfig::default(),
        after_seq_observer: None,
        header_insert_observer: None,
        session_index: None,
        on_tool_round: None,
        working_dir: None,
    })
    .await
}

fn make_stub_llm_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    let call_count = Arc::new(AtomicUsize::new(0));
    Arc::new(move |_input, _config, _abort| {
        let cc = call_count.clone();
        Box::pin(async move {
            let n = cc.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // Turn 1: assistant calls echo tool
                Ok((
                    "let me help you".to_string(),
                    None,
                    vec![ToolCall::new(
                        "echo".to_string(),
                        json!({"message": "hello"}),
                        Some("call-echo-1".to_string()),
                        None,
                    )],
                    harnx_runtime::client::CompletionTokenUsage::default(),
                ))
            } else {
                // Turn 2: final text response
                Ok((
                    "done!".to_string(),
                    None,
                    vec![],
                    harnx_runtime::client::CompletionTokenUsage::default(),
                ))
            }
        })
    })
}

fn append_user_message_entry(message_id: &str, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some(message_id.to_string()),
        role: MessageRole::User,
        content: harnx_core::message::MessageContent::Text(text.to_string()),
        timestamp: None,
        fence_token: None,
    }
}

fn make_handoff_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    let call_count = Arc::new(AtomicUsize::new(0));
    Arc::new(move |_input, _config, _abort| {
        let cc = call_count.clone();
        Box::pin(async move {
            let n = cc.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok((
                    "handoff requested".to_string(),
                    None,
                    vec![ToolCall::new(
                        "delegate-agent_session_handoff".to_string(),
                        json!({
                            "prompt": "finish delegated work",
                            "session_id": "handoff-remote-session"
                        }),
                        Some("handoff-call-1".to_string()),
                        None,
                    )],
                    CompletionTokenUsage::default(),
                ))
            } else {
                Ok((
                    "handoff completed".to_string(),
                    None,
                    vec![],
                    CompletionTokenUsage::default(),
                ))
            }
        })
    })
}

fn write_test_agent(config_dir: &Path, agent_name: &str, body: &str) -> Result<()> {
    let agents_dir = config_dir.join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::write(agents_dir.join(format!("{agent_name}.md")), body)?;
    Ok(())
}

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

async fn spawn_worker_daemon_with_call_fn(
    config: Arc<RwLock<Config>>,
    worker_id: &str,
    call_fn: harnx_runtime::agent_loop::AgentCallFn,
) -> tokio::task::JoinHandle<Result<()>> {
    let worker_config = WorkerDaemonConfig::new("local", worker_id);
    let daemon = tokio::spawn({
        let cfg = config.clone();
        async move { run_worker_daemon(cfg, worker_config, Some(call_fn)).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    daemon
}

async fn local_test_nats(server_url: &str) -> Result<async_nats::jetstream::Context> {
    Ok(async_nats::jetstream::new(
        async_nats::connect(server_url).await?,
    ))
}

async fn activate_session(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<()> {
    publish_session_activate(
        jetstream,
        "local",
        &SessionActivate::new(session_id, "ignored-agent"),
    )
    .await?;
    Ok(())
}

fn mid_round_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |input, _config, _abort| {
        // The mid-turn injection is delivered via `input.injected_user_text`
        // (set by the worker's `on_tool_round` callback between tool rounds).
        // `build_messages` appends it to the wire request, so the LLM sees it,
        // but it is only persisted to `session.messages` AFTER this call. Detect
        // it directly on the input.
        let injected_arrived = input
            .injected_user_text()
            .map(|t| t.contains("late message"))
            .unwrap_or(false);
        Box::pin(async move {
            if !injected_arrived {
                // First round: signal readiness, block until the test appends the
                // late message, then emit a tool call so the loop runs another round
                // (the `on_tool_round` seam fires between rounds and injects it).
                MID_ROUND_APPEND_READY.notify_one();
                MID_ROUND_APPEND_DONE.notified().await;
                MID_ROUND_RELOAD_SEEN.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "round-one".to_string(),
                    None,
                    vec![ToolCall::new(
                        "echo".to_string(),
                        json!({}),
                        Some("call-mid".to_string()),
                        None,
                    )],
                    CompletionTokenUsage::default(),
                ))
            } else {
                // Subsequent round: the injected message is now visible — emit a
                // final assistant text echoing it so the test can assert it was
                // delivered into the SAME turn exactly once.
                MID_ROUND_FINAL_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "final: saw late message".to_string(),
                    None,
                    vec![],
                    CompletionTokenUsage::default(),
                ))
            }
        })
    })
}

fn end_turn_call_fn() -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |input, _config, _abort| {
        let prompt = input.raw.0.clone();
        Box::pin(async move {
            let call_no = END_TURN_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
            // Signal readiness using notify_one() (stores a permit if no waiter yet).
            // This avoids the lost-wakeup race where notify_waiters fires before
            // the test registers its notified() future.
            if call_no == 1 {
                END_TURN_APPEND_READY.notify_one();
                // Block until test signals DONE - this prevents turn 1 from completing
                // before the test appends "second", ensuring the daemon's drain loop
                // sees the new message when it re-reads the tail.
                END_TURN_APPEND_DONE.notified().await;
            }
            Ok((
                format!("turn:{call_no} prompt:{prompt}"),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    })
}

fn fold_capture_call_fn(
    calls: Arc<AtomicUsize>,
    prompts: Arc<AsyncMutex<Vec<String>>>,
) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |input, _config, _abort| {
        let calls = calls.clone();
        let prompts = prompts.clone();
        let prompt = input.raw.0.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            prompts.lock().await.push(prompt.clone());
            Ok((
                format!("folded:{prompt}"),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    })
}

fn final_assistant_texts(entries: &[(u64, SessionLogEntry)]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message {
                role,
                content,
                // Include all assistant messages, fenced or not (fence_token ignored)
                ..
            } if role.is_assistant() => Some(content.to_text()),
            _ => None,
        })
        .collect()
}

fn user_message_texts(entries: &[(u64, SessionLogEntry)]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message { role, content, .. } if role.is_user() => {
                Some(content.to_text())
            }
            _ => None,
        })
        .collect()
}
// Reset static state between tests
fn reset_test_state() {
    // These must be reset in case tests are run in the same process
    // Note: We can't truly clear Notify state across tests without async runtime
    // Just reset the counters - tests may flake if run in same process
    MID_ROUND_FINAL_CALLS.store(0, Ordering::SeqCst);
    END_TURN_CALLS.store(0, Ordering::SeqCst);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_tool_round_user_message_is_injected_once_into_same_turn() -> Result<()> {
    reset_test_state();
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let config = local_nats_runtime_config(server.url());
    let worker_config = WorkerDaemonConfig::new("local", "worker-mid-round");
    let daemon = tokio::spawn({
        let cfg = config.clone();
        async move { run_worker_daemon(cfg, worker_config, Some(mid_round_call_fn())).await }
    });
    // Give daemon time to initialize consumer
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "mid-round-injection";
    let log = NatsSessionLog::new(js.clone(), session_id);

    // CRITICAL: Create the notified future BEFORE publishing the activate
    // to avoid lost wakeup race between notify_one() and notified().await
    let ready_fut = MID_ROUND_APPEND_READY.notified();

    log.append_event_async(&append_user_message_entry("user-1", "seed message"))
        .await?;
    publish_session_activate(
        &js,
        "local",
        &SessionActivate::new(session_id, "ignored-agent"),
    )
    .await?;

    // Now await the ready signal - the permit was stored by notify_one()
    ready_fut.await;

    log.append_event_async(&append_user_message_entry("user-2", "late message"))
        .await?;
    // Use notify_one() to signal completion - matches the notify_one() in call_fn
    MID_ROUND_APPEND_DONE.notify_one();

    wait_until(Duration::from_secs(10), || {
        MID_ROUND_FINAL_CALLS.load(Ordering::SeqCst) >= 1
    })
    .await?;

    tokio::time::sleep(Duration::from_millis(1500)).await;

    let entries = log.load_events_async().await?;
    let assistants = final_assistant_texts(&entries);
    assert!(assistants.iter().any(|text| text.contains("late message")));
    assert_eq!(
        assistants
            .iter()
            .filter(|text| text.contains("late message"))
            .count(),
        1
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_of_turn_reread_runs_continuation_turn_with_same_activation() -> Result<()> {
    reset_test_state();
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let config = Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url: server.url(),
        token: None,
    })));
    let worker_config = WorkerDaemonConfig::new("local", "worker-reread");
    let daemon = tokio::spawn({
        let cfg = config.clone();
        async move { run_worker_daemon(cfg, worker_config, Some(end_turn_call_fn())).await }
    });
    // Give daemon time to initialize consumer
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "end-turn-reread";
    let log = NatsSessionLog::new(js.clone(), session_id);

    // CRITICAL: Create the notified future BEFORE publishing the activate
    // to avoid lost wakeup race between notify_one() and notified().await
    let ready_fut = END_TURN_APPEND_READY.notified();

    log.append_event_async(&append_user_message_entry("user-1", "first"))
        .await?;
    publish_session_activate(
        &js,
        "local",
        &SessionActivate::new(session_id, "ignored-agent"),
    )
    .await?;

    // Now await the ready signal - the permit was stored by notify_one()
    ready_fut.await;

    // Give a moment for the NATS message to propagate
    tokio::time::sleep(Duration::from_millis(100)).await;

    log.append_event_async(&append_user_message_entry("user-2", "second"))
        .await?;
    // Signal to turn 1 that it can complete now
    END_TURN_APPEND_DONE.notify_one();

    // Poll the DURABLE log for two persisted `turn:` assistant messages.
    // `END_TURN_CALLS` is bumped at the START of each LLM call, so waiting on it
    // races turn 2's persistence; assert on the committed log instead.
    let mut entries;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        entries = log.load_events_async().await?;
        let count = final_assistant_texts(&entries)
            .iter()
            .filter(|text| text.contains("turn:"))
            .count();
        if count >= 2 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "continuation turn never persisted a second assistant (got {count} turn: messages)"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let assistants = final_assistant_texts(&entries);
    assert_eq!(
        assistants
            .iter()
            .filter(|text| text.contains("turn:"))
            .count(),
        2
    );
    // Turn 1 consumed "first" (its assistant barrier), so the continuation turn 2
    // folds only the post-barrier message "second".
    assert!(assistants
        .iter()
        .any(|text| text.contains("turn:1 prompt:first")));
    assert!(assistants
        .iter()
        .any(|text| text.contains("turn:2 prompt:second")));

    // `skip_user_log_append` invariant: the worker must NOT re-append the user
    // messages it reads from the log. The durable log must contain EXACTLY the
    // two client-appended user messages — "first" and "second" — with no
    // duplicates. A regression (re-appending the folded input) would both
    // duplicate the user message and reorder the assistant barrier past
    // concurrently-arrived messages.
    let users = user_message_texts(&entries);
    assert_eq!(
        users,
        vec!["first".to_string(), "second".to_string()],
        "durable log must contain exactly the two client user messages with no worker re-appends; got {users:?}"
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_concurrent_messages_fold_in_seq_order_into_single_turn() -> Result<()> {
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let config = Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url: server.url(),
        token: None,
    })));
    let calls = Arc::new(AtomicUsize::new(0));
    let prompts = Arc::new(AsyncMutex::new(Vec::<String>::new()));
    let worker_config = WorkerDaemonConfig::new("local", "worker-fold");
    let daemon = tokio::spawn({
        let cfg = config.clone();
        let calls = calls.clone();
        let prompts = prompts.clone();
        async move {
            run_worker_daemon(
                cfg,
                worker_config,
                Some(fold_capture_call_fn(calls, prompts)),
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "fold-order";
    let log = NatsSessionLog::new(js.clone(), session_id);
    log.append_event_async(&append_user_message_entry("user-1", "alpha"))
        .await?;
    log.append_event_async(&append_user_message_entry("user-2", "beta"))
        .await?;
    publish_session_activate(
        &js,
        "local",
        &SessionActivate::new(session_id, "ignored-agent"),
    )
    .await?;

    wait_until(Duration::from_secs(10), || {
        calls.load(Ordering::SeqCst) >= 1
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let prompts = prompts.lock().await.clone();
    assert_eq!(prompts, vec!["alpha\nbeta".to_string()]);

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_worker_persists_full_turn_end_to_end() -> Result<()> {
    require_nextest();

    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let global_config = Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url: server.url(),
        token: None,
    })));
    let session_id = "test-session-full-turn";

    let jetstream_ctx = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let log = seed_session_and_attach_runtime(&global_config, jetstream_ctx, session_id).await?;

    run_worker_turn(WorkerTurnParams {
        global_config: global_config.clone(),
        labels: WorkerTurnLabels {
            cluster_key: "local",
            session_id,
            prompt: "test prompt",
        },
        call_fn: make_stub_llm_call_fn(),
        lease: None,
    })
    .await?;

    // Verify: load entries from NATS
    let entries = log.load_events_async().await?;
    let entries_only: Vec<SessionLogEntry> = entries.iter().map(|(_, e)| e.clone()).collect();

    // Should have: Header, User message, ToolCalls, ToolResults, final assistant Message
    assert!(
        entries.len() >= 4,
        "expected at least 4 entries, got {}",
        entries.len()
    );

    // Verify reconstruction shows Idle at end
    let state = reconstruct_state(&entries_only);
    assert_eq!(
        state.turn_status,
        TurnStatus::Idle,
        "expected Idle, got {:?}",
        state.turn_status
    );

    Ok(())
}

/// Regression test for the Aristarchus blocker: the worker execution path must
/// connect through the config-driven `nats_jetstream`/`nats_client` (applying
/// token/TLS auth), NOT a bare `async_nats::connect(url)`. Run the worker
/// against a TOKEN-AUTH nats-server with the token configured: with the old
/// bare connect this fails (auth required); with the fix it persists the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_worker_honors_configured_token_auth() -> Result<()> {
    require_nextest();

    let token = "s3cr3t-worker-token";
    let Some(server) = common::spawn_nats_server_with_options(common::SpawnNatsServerOptions {
        auth_token: Some(token.to_string()),
    })
    .await?
    else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    // Config carries the token so the config-driven connect can authenticate.
    let global_config = Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "secure",
        url: server.url(),
        token: Some(token),
    })));
    let session_id = "test-session-token-auth";

    // Seed a header via an authenticated backend (proves auth works for setup).
    let auth_js = {
        let cfg = global_config.read().clone();
        cfg.nats_jetstream("secure").await?
    };
    let log = seed_session_and_attach_runtime(&global_config, auth_js, session_id).await?;

    // The worker connects internally via the config-driven path; this only
    // succeeds if it applies the configured token.
    run_worker_turn(WorkerTurnParams {
        global_config: global_config.clone(),
        labels: WorkerTurnLabels {
            cluster_key: "secure",
            session_id,
            prompt: "test prompt",
        },
        call_fn: make_stub_llm_call_fn(),
        lease: None,
    })
    .await?;

    let entries = log.load_events_async().await?;
    assert!(
        entries.len() >= 4,
        "expected the authenticated worker to persist a full turn, got {} entries",
        entries.len()
    );
    Ok(())
}

/// Stub call_fn that increments an external counter on each LLM call, then
/// returns a final text response (single-turn).
fn counting_stub_call_fn(counter: Arc<AtomicUsize>) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let counter = counter.clone();
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok((
                "done".to_string(),
                None,
                vec![],
                harnx_runtime::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

fn cluster_config(url: &str) -> Arc<RwLock<Config>> {
    Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url,
        token: None,
    })))
}

async fn wait_until(timeout: std::time::Duration, mut cond: impl FnMut() -> bool) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if cond() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!("condition not met within {:?}", timeout)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_runs_exactly_one_worker_per_activation_and_reactivation_is_noop() -> Result<()> {
    use harnx_runtime::nats_lease::NatsLeaseConfig;
    use harnx_runtime::nats_worker::{
        publish_session_activate, run_worker_daemon, SessionActivate, WorkerDaemonConfig,
    };
    use std::time::Duration;

    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let fast_lease = NatsLeaseConfig {
        ttl: Duration::from_secs(3),
        renew_interval: Duration::from_millis(500),
        replicas: 1,
        tombstone_ttl: Duration::from_secs(10),
        ..Default::default()
    };

    let counter_one = Arc::new(AtomicUsize::new(0));
    let counter_two = Arc::new(AtomicUsize::new(0));

    let mut cfg_one = WorkerDaemonConfig::new("local", "worker-one");
    cfg_one.lease = fast_lease.clone();
    let mut cfg_two = WorkerDaemonConfig::new("local", "worker-two");
    cfg_two.lease = fast_lease.clone();

    let config_one = cluster_config(server.url());
    let config_two = cluster_config(server.url());

    let h1 = tokio::spawn({
        let c = config_one.clone();
        let call = counting_stub_call_fn(counter_one.clone());
        async move { run_worker_daemon(c, cfg_one, Some(call)).await }
    });
    let h2 = tokio::spawn({
        let c = config_two.clone();
        let call = counting_stub_call_fn(counter_two.clone());
        async move { run_worker_daemon(c, cfg_two, Some(call)).await }
    });

    // Give the daemons a moment to subscribe.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);

    // Client appends a user message to the session log, then activates it.
    let session_id = "dispatch-session";
    let client_log = NatsSessionLog::new(js.clone(), session_id);
    client_log
        .append_event_async(&SessionLogEntry::Message {
            id: None,
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text("hello worker".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await?;

    let activation = SessionActivate::new(session_id, "ignored-agent");
    publish_session_activate(&js, "local", &activation).await?;
    // Duplicate publish is deduped by Nats-Msg-Id; still only one execution.
    publish_session_activate(&js, "local", &activation).await?;

    // Exactly one worker executes one turn.
    wait_until(Duration::from_secs(10), || {
        counter_one.load(Ordering::SeqCst) + counter_two.load(Ordering::SeqCst) >= 1
    })
    .await?;
    // Let any erroneous second executor surface.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let total = counter_one.load(Ordering::SeqCst) + counter_two.load(Ordering::SeqCst);
    assert_eq!(
        total, 1,
        "exactly one worker should execute the activation (got {total})"
    );

    h1.abort();
    h2.abort();
    let _ = h1.await;
    let _ = h2.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fenced_sink_rejects_append_when_lease_lost() -> Result<()> {
    use harnx_runtime::config::session::SessionAppendSink;
    use harnx_runtime::nats_lease::{NatsLeaseConfig, NatsSessionLease};
    use harnx_runtime::nats_worker::{FencedSessionLogSink, NatsSessionLogBackend};
    use std::time::Duration;

    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "fenced-session";

    let lease = Arc::new(
        NatsSessionLease::acquire(harnx_runtime::nats_lease::NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id,
            worker_id: "worker-a".to_string(),
            generation: 1,
            config: NatsLeaseConfig {
                ttl: Duration::from_secs(30),
                renew_interval: Duration::from_secs(10),
                ..Default::default()
            },
            session_index: None,
        })
        .await?
        .expect("acquire"),
    );

    let backend = NatsSessionLogBackend::new(js.clone(), session_id);
    let sink = FencedSessionLogSink::new(backend, Arc::clone(&lease));

    // Held lease: an assistant append succeeds and is fence-stamped.
    let entry = SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::Assistant,
        content: harnx_core::message::MessageContent::Text("hi".to_string()),
        timestamp: None,
        fence_token: None,
    };
    let fence_at_append = lease.fence_token();
    sink.append(&entry)
        .expect("append while held should succeed");

    // Verify the persisted entry carries the lease fence stamped at append time.
    let log = NatsSessionLog::new(js.clone(), session_id);
    let loaded = log.load_events_async().await?;
    let stamped = loaded
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::Message { fence_token: Some(f), .. } if *f == fence_at_append));
    assert!(
        stamped,
        "persisted assistant entry should carry the lease fence ({fence_at_append}); loaded={loaded:?}"
    );

    // Lose the lease: subsequent worker append must be rejected (fenced out).
    lease.mark_lost_for_test();
    let rejected = sink.append(&entry);
    assert!(
        rejected.is_err(),
        "append after lease loss must be rejected"
    );
    Ok(())
}

async fn append_resume_fence_seed(log: &NatsSessionLog) -> Result<()> {
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("go".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::Assistant,
        content: harnx_core::message::MessageContent::Text("from newer worker".to_string()),
        timestamp: None,
        fence_token: Some(u64::MAX),
    })
    .await?;
    Ok(())
}

async fn acquire_test_lease(
    js: async_nats::jetstream::Context,
    session_id: &str,
    worker_id: &str,
) -> Result<Arc<harnx_runtime::nats_lease::NatsSessionLease>> {
    use harnx_runtime::nats_lease::{NatsLeaseConfig, NatsSessionLease};
    use std::time::Duration;

    Ok(Arc::new(
        NatsSessionLease::acquire(harnx_runtime::nats_lease::NatsLeaseAcquireParams {
            jetstream: js,
            session_id,
            worker_id: worker_id.to_string(),
            generation: 1,
            config: NatsLeaseConfig {
                ttl: Duration::from_secs(30),
                renew_interval: Duration::from_secs(10),
                ..Default::default()
            },
            session_index: None,
        })
        .await?
        .expect("acquire"),
    ))
}

fn assert_resume_fenced(result: Result<()>) {
    assert!(
        result.is_err(),
        "resume must abort when tail fence exceeds held lease revision"
    );
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("fenced") || msg.contains("exceeds held lease"),
        "error should indicate fencing, got: {msg}"
    );
}

fn assert_handoff_original_session_entries(entries: &[(u64, SessionLogEntry)]) {
    assert!(
        entries.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::ToolResults { results, .. }
                if results.iter().any(|result| result.switch_agent.as_ref().is_some_and(|switch| {
                    switch.agent == "delegate-agent"
                        && switch.session_id.as_deref() == Some("handoff-remote-session")
                }))
        )),
        "expected original session to persist handoff tool result"
    );
}

fn assert_handoff_target_session(config: &Arc<RwLock<Config>>) -> String {
    assert!(
        config
            .read()
            .session
            .as_ref()
            .is_some_and(|session| session.id() == "handoff-remote-session"),
        "expected active config session to switch to handoff target"
    );
    config
        .read()
        .session
        .as_ref()
        .and_then(|session| session.session_id.clone())
        .expect("expected active handed-off session id")
}

fn assert_handoff_target_log(
    entries: &[(u64, SessionLogEntry)],
    local_session_path: &std::path::Path,
) {
    assert!(
        !entries.is_empty(),
        "expected handed-off session to persist to JetStream"
    );
    assert!(
        entries.iter().any(|(_, entry)| matches!(
            entry,
            SessionLogEntry::Message {
                role: MessageRole::Assistant,
                content: harnx_core::message::MessageContent::Text(text),
                ..
            } if text.contains("handoff completed")
        )),
        "expected handed-off JetStream log to contain delegated assistant reply"
    );
    assert!(
        !local_session_path.exists(),
        "handoff should stay NATS-backed; unexpected local session file at {}",
        local_session_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_aborts_when_tail_fence_exceeds_held_revision() -> Result<()> {
    use harnx_runtime::nats_session_log::NatsSessionLog;
    use harnx_runtime::nats_worker::run_agent_loop_with_nats_inner;

    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "resume-fence-session";

    let log = NatsSessionLog::new(js.clone(), session_id);
    append_resume_fence_seed(&log).await?;
    let lease = acquire_test_lease(js.clone(), session_id, "worker-stale").await?;

    let config = cluster_config(server.url());
    let input = harnx_runtime::config::input::from_str(&config, "go", None);
    let result = run_agent_loop_with_nats_inner(
        RunAgentLoopArgs {
            cluster_key: "local",
            session_id,
            config,
            initial_input: input,
            abort_signal: create_abort_signal(),
            call_fn: Some(counting_stub_call_fn(Arc::new(AtomicUsize::new(0)))),
            lease: None,
            lease_config: NatsLeaseConfig::default(),
            after_seq_observer: None,
            header_insert_observer: None,
            session_index: None,
            on_tool_round: None,
            working_dir: None,
        }
        .with_lease(lease),
    )
    .await;

    assert_resume_fenced(result);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nats_worker_handoff_persists_to_jetstream_not_file() -> Result<()> {
    require_nextest();

    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let root = tempfile::tempdir()?;
    let config_dir = root.path().join("config");
    let data_dir = root.path().join("data");
    let state_dir = root.path().join("state");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::create_dir_all(config_dir.join("clients"))?;
    std::fs::create_dir_all(config_dir.join("nats_servers"))?;
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(&state_dir)?;

    write_test_agent(
        &config_dir,
        "delegate-agent",
        "---\nmodel: openai:test-model\nuse_tools: delegate-agent_session_handoff\n---\nDelegate agent instructions\n",
    )?;

    let _config_guard = EnvVarGuard::set_path("HARNX_CONFIG_DIR", &config_dir);
    let _data_guard = EnvVarGuard::set_path("HARNX_DATA_DIR", &data_dir);
    let _state_guard = EnvVarGuard::set_path("HARNX_STATE_DIR", &state_dir);

    std::fs::write(config_dir.join("config.yaml"), "model: openai:test-model\n")?;
    std::fs::write(
        config_dir.join("nats_servers").join("local.yaml"),
        format!("url: {}\n", server.url()),
    )?;
    std::fs::write(
        config_dir.join("clients").join("openai.yaml"),
        "type: openai\napi_key: sk-test\nmodels:\n  - name: test-model\n    type: chat\n    max_input_tokens: 4096\n",
    )?;

    let config_path = config_dir.join("config.yaml");
    let base = {
        let _config_guard = EnvVarGuard::set_path("HARNX_CONFIG_DIR", &config_dir);
        Config::load_from_file(&config_path)?
    };

    let config = Arc::new(RwLock::new(base));
    harnx_runtime::config::Config::use_agent(
        &config,
        "delegate-agent",
        Some("nats-handoff-root"),
        create_abort_signal(),
    )
    .await?;

    let js = {
        let cfg = config.read().clone();
        cfg.nats_jetstream("local").await?
    };
    let original_log =
        seed_session_and_attach_runtime(&config, js.clone(), "nats-handoff-root").await?;
    let lease = Arc::new(
        harnx_runtime::nats_lease::NatsSessionLease::acquire(
            harnx_runtime::nats_lease::NatsLeaseAcquireParams {
                jetstream: js.clone(),
                session_id: "nats-handoff-root",
                worker_id: "worker-handoff".to_string(),
                generation: 1,
                config: NatsLeaseConfig::default(),
                session_index: None,
            },
        )
        .await?
        .expect("acquire"),
    );

    run_worker_turn(WorkerTurnParams {
        global_config: config.clone(),
        labels: WorkerTurnLabels {
            cluster_key: "local",
            session_id: "nats-handoff-root",
            prompt: "start handoff",
        },
        call_fn: make_handoff_call_fn(),
        lease: Some(lease),
    })
    .await?;

    let original_entries_after = original_log.load_events_async().await?;
    assert_handoff_original_session_entries(&original_entries_after);
    let actual_new_session_id = assert_handoff_target_session(&config);
    let new_log = NatsSessionLog::new(js.clone(), &actual_new_session_id);
    let new_entries = new_log.load_events_async().await?;

    let local_session_path = config.read().session_file(&actual_new_session_id);
    assert_handoff_target_log(&new_entries, &local_session_path);

    Ok(())
}

/// Read-your-writes seam (Step 2): a backend wired with an `after_seq_observer`
/// advances the observer to the durable ack sequence on every append, and
/// `load_events_consistent_async` waits until the stream reflects at least
/// that sequence before returning. This is the mechanism that lets the
/// end-of-turn drain re-read observe the worker's own just-persisted barrier
/// instead of a stale tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_seq_observer_advances_and_consistent_read_honors_it() -> Result<()> {
    use harnx_runtime::nats_worker::NatsSessionLogBackend;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "ryw-observer";

    let observer = Arc::new(AtomicU64::new(0));
    let backend = NatsSessionLogBackend::new(js.clone(), session_id)
        .with_after_seq_observer(Arc::clone(&observer));

    // Append two entries; the observer must advance to the durable ack seq of
    // the latest append (monotonic via fetch_max).
    let user = SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("hi".to_string()),
        timestamp: None,
        fence_token: None,
    };
    let assistant = SessionLogEntry::Message {
        id: None,
        role: harnx_core::message::MessageRole::Assistant,
        content: harnx_core::message::MessageContent::Text("there".to_string()),
        timestamp: None,
        fence_token: Some(7),
    };

    let seq1 = backend.append_event_blocking(&user)?;
    assert_eq!(
        observer.load(AtomicOrdering::SeqCst),
        seq1,
        "observer must advance to the first append's ack sequence"
    );
    let seq2 = backend.append_event_blocking(&assistant)?;
    assert!(seq2 > seq1, "second append must get a higher sequence");
    assert_eq!(
        observer.load(AtomicOrdering::SeqCst),
        seq2,
        "observer must advance to the latest append's ack sequence"
    );

    // The consistent read uses the observer's high-water mark; it must return a
    // tail that reflects at least seq2 (both entries visible), never a stale
    // read missing the worker's own latest barrier.
    let entries = backend.load_events_consistent_async().await?;
    assert!(
        entries.iter().any(|(s, _)| *s == seq2),
        "consistent read must include the latest appended entry (seq {seq2}); got {entries:?}"
    );
    assert_eq!(
        final_assistant_texts(&entries),
        vec!["there".to_string()],
        "consistent read must surface the just-persisted assistant barrier"
    );
    Ok(())
}

/// Worker-side retract: a user message that is retracted BEFORE the worker drains
/// must NOT be folded into worker input and NOT executed.
///
/// This test proves the worker path honors EditEntries mutations. Without the fix,
/// `fold_new_user_messages_since` would iterate raw entries and fold the retracted
/// message, causing unwanted worker execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retracted_mid_tool_round_message_is_not_injected() -> Result<()> {
    reset_test_state();
    MID_ROUND_RELOAD_SEEN.store(0, Ordering::SeqCst);
    require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        eprintln!("Skipping test: nats-server not available");
        return Ok(());
    };

    let config = Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url: server.url(),
        token: None,
    })));
    let daemon =
        spawn_worker_daemon_with_call_fn(config, "worker-mid-round-retract", mid_round_call_fn())
            .await;

    let js = local_test_nats(server.url()).await?;
    let session_id = "mid-round-retracted-injection";
    let log = NatsSessionLog::new(js.clone(), session_id);
    let ready_fut = MID_ROUND_APPEND_READY.notified();

    log.append_event_async(&append_user_message_entry("user-1", "seed message"))
        .await?;
    activate_session(&js, session_id).await?;
    ready_fut.await;

    let retracted_seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some("msg-retracted-before-injection".to_string()),
            role: MessageRole::User,
            content: harnx_core::message::MessageContent::Text("late message".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    let retracted_seq = usize::try_from(retracted_seq).expect("JetStream seq fits usize");
    log.append_event_async(&SessionLogEntry::EditEntries {
        from: retracted_seq,
        to: retracted_seq,
        replacements: vec![],
    })
    .await?;

    MID_ROUND_APPEND_DONE.notify_one();
    wait_until(Duration::from_secs(10), || {
        MID_ROUND_RELOAD_SEEN.load(Ordering::SeqCst) >= 1
    })
    .await?;

    let entries = log.load_events_async().await?;
    let assistants = final_assistant_texts(&entries);

    assert_eq!(
        MID_ROUND_FINAL_CALLS.load(Ordering::SeqCst),
        0,
        "retracted message should not trigger injected follow-up round"
    );
    assert!(
        assistants.is_empty(),
        "no final assistant turn should be persisted without injected message"
    );
    // The raw durable log still physically contains the retracted "late message"
    // entry plus its EditEntries tombstone (retract is a tombstone, never a
    // physical delete). The EFFECTIVE stream (mutations applied via
    // reconstruct_state_from_nats) is what the worker folds, and it must exclude
    // the retracted message from next_turn_messages.
    let effective_user_texts: Vec<String> = reconstruct_state_from_nats(&entries)
        .next_turn_messages
        .iter()
        .map(|m| m.content.to_text())
        .collect();
    assert!(
        !effective_user_texts.iter().any(|t| t == "late message"),
        "effective next-turn stream should exclude retracted mid-round message, got: {effective_user_texts:?}"
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rewind_truncates_worker_visible_tail_before_activation() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let counter = Arc::new(AtomicUsize::new(0));
    let prompts = Arc::new(AsyncMutex::new(Vec::<String>::new()));

    let config = local_nats_runtime_config(server.url());
    let daemon = spawn_worker_daemon_with_call_fn(
        config,
        "worker-rewind",
        fold_capture_call_fn(counter.clone(), prompts.clone()),
    )
    .await;

    let js = local_test_nats(server.url()).await?;
    let session_id = "rewind-worker-test";
    let log = NatsSessionLog::new(js.clone(), session_id);

    let first_seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: Some("msg-first".to_string()),
            role: MessageRole::User,
            content: harnx_core::message::MessageContent::Text("first prompt".to_string()),
            timestamp: None,
            fence_token: None,
        })
        .await?;
    log.append_event_async(&SessionLogEntry::Message {
        id: Some("msg-second".to_string()),
        role: MessageRole::User,
        content: harnx_core::message::MessageContent::Text("second prompt".to_string()),
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::Rewind {
        after_seq: usize::try_from(first_seq).expect("JetStream seq fits usize"),
    })
    .await?;

    activate_session(&js, session_id).await?;

    wait_until(Duration::from_secs(10), || {
        counter.load(Ordering::SeqCst) >= 1
    })
    .await?;

    assert_single_prompt(&prompts, "first prompt").await;

    let entries = log.load_events_async().await?;
    let assistant_texts = final_assistant_texts(&entries);
    assert_single_assistant_contains(&entries, "first prompt");
    assert!(
        assistant_texts
            .iter()
            .all(|text| !text.contains("second prompt")),
        "rewound tail must not leak into worker execution: {:?}",
        assistant_texts
    );

    let reconstructed = reconstruct_state_from_nats(&entries);
    assert_eq!(reconstructed.turn_status, TurnStatus::Idle);

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retracted_orphan_tool_call_is_not_repaired_by_worker() -> Result<()> {
    RETRACTED_ORPHAN_ACTIVATION_CALLS.store(0, Ordering::SeqCst);
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let config = Arc::new(RwLock::new(local_nats_config(NatsServerSpec {
        name: "local",
        url: server.url(),
        token: None,
    })));
    let tool_calls_seen = Arc::new(AtomicUsize::new(0));
    let seen_for_call_fn = Arc::clone(&tool_calls_seen);
    let call_fn: harnx_runtime::agent_loop::AgentCallFn =
        Arc::new(move |_input, _config, _abort| {
            let seen = seen_for_call_fn.clone();
            Box::pin(async move {
                RETRACTED_ORPHAN_ACTIVATION_CALLS.fetch_add(1, Ordering::SeqCst);
                seen.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "should not run".to_string(),
                    None,
                    vec![],
                    CompletionTokenUsage::default(),
                ))
            })
        });
    let daemon = spawn_worker_daemon_with_call_fn(config, "worker-retracted-orphan", call_fn).await;

    let js = local_test_nats(server.url()).await?;
    let session_id = "retracted-orphan-test";
    let log = NatsSessionLog::new(js.clone(), session_id);

    log.append_event_async(&session_header(session_id)).await?;
    // NOTE: deliberately NO unanswered user message in the seed. We want the log to
    // contain ONLY a retracted tool round, so that after mutations are applied the
    // effective log is idle and the worker has nothing to do. A pending user message
    // would (correctly) make the worker run a normal turn and invoke call_fn,
    // confounding the "orphan repair must not run" assertions below.
    let tool_calls_seq = log
        .append_event_async(&SessionLogEntry::ToolCalls {
            text: "working".to_string(),
            thought: Some("thinking".to_string()),
            calls: vec![ToolCall::new(
                "echo".to_string(),
                json!({"message": "ghost"}),
                Some("call-retracted-orphan".to_string()),
                None,
            )],
            timestamp: None,
            // fence_token MUST be None (or <= the worker's lease revision) so the
            // resume is NOT aborted by abort_resume_if_fenced before it reaches
            // load_or_repair_session. A stale fence here would make the test pass
            // vacuously (resume aborted before the orphan scan ever runs).
            fence_token: None,
        })
        .await?;
    let tool_calls_seq = usize::try_from(tool_calls_seq).expect("JetStream seq fits usize");
    log.append_event_async(&SessionLogEntry::EditEntries {
        from: tool_calls_seq,
        to: tool_calls_seq,
        replacements: vec![],
    })
    .await?;

    // Snapshot HA metrics before activation. With the bug (raw orphan scan) the
    // worker would detect the retracted ToolCalls as an orphan (resumes += 1) and
    // synthesize an interrupt-error ToolResults (interrupt_errors_synthesized += 1).
    // With the fix (effective scan) the retract is honored: neither increments.
    let metrics_before = harnx_runtime::nats_metrics::snapshot();

    activate_session(&js, session_id).await?;

    // Deterministic barrier: wait until the worker has actually CLAIMED the session
    // (lease_acquisitions increments) — which proves it reached load_or_repair_session
    // and ran the orphan scan — and then FINISHED (active_sessions back to 0). Without
    // this, the assertions could run before the worker did anything (a 0==0 check is
    // satisfied immediately), letting the test pass vacuously even with the bug present.
    wait_for_worker_daemon_idle(metrics_before.lease_acquisitions).await?;

    let entries = log.load_events_async().await?;
    assert_eq!(
        tool_calls_seen.load(Ordering::SeqCst),
        0,
        "orphan repair must not invoke call_fn for retracted tool call"
    );
    assert_no_resume_or_interrupt_metric_delta(
        metrics_before,
        harnx_runtime::nats_metrics::snapshot(),
    );
    assert_retracted_orphan_absent(&entries, "call-retracted-orphan")?;

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// Verifies `load_events_latest_async` reads leader-authoritative tail end-to-end.
///
/// On single-node NATS this cannot differ from old `stream.info()` path because
/// there is no replication lag, so this is a forward behavioral guarantee rather
/// than a fail-on-revert differential. Real #917 bug requires multi-node
/// STREAM.INFO-vs-leader divergence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_events_latest_async_reads_leader_authoritative_tail() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let js = local_test_nats(server.url()).await?;
    let session_id = "latest-tail-test";
    let log = NatsSessionLog::new(js, session_id);

    let user_seq = log
        .append_event_async(&append_user_message_entry(
            "msg-to-retract",
            "please ignore this",
        ))
        .await?;
    let retract_seq = log
        .append_event_async(&SessionLogEntry::EditEntries {
            from: user_seq as usize,
            to: user_seq as usize,
            replacements: vec![],
        })
        .await?;

    let entries = log.load_events_latest_async().await?;
    let max_seq = entries.iter().map(|(seq, _)| *seq).max().unwrap_or(0);
    assert!(
        max_seq >= retract_seq,
        "latest read must include retract seq {retract_seq}, got max seq {max_seq}"
    );
    assert!(
        entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::EditEntries { .. })),
        "latest read must include EditEntries retract entry"
    );

    let effective_messages = reconstruct_state_from_nats(&entries).next_turn_messages;
    assert!(
        !effective_messages
            .iter()
            .any(|message| message.content.to_text().contains("please ignore this")),
        "retracted user text must not survive reconstruct_state_from_nats fold"
    );

    Ok(())
}

/// Structural regression guard for #917.
///
/// Runtime difference only appears under multi-node replication lag, which CI's
/// single-node NATS cannot reproduce. If production structure changes
/// legitimately, update this guard.
#[test]
fn injection_decision_points_use_leader_authoritative_read() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let agent_loop = std::fs::read_to_string(manifest_dir.join("src/nats_worker/agent_loop.rs"))
        .expect("agent_loop.rs must be readable");
    let daemon = std::fs::read_to_string(manifest_dir.join("src/nats_worker/daemon.rs"))
        .expect("daemon.rs must be readable");

    assert!(
        agent_loop.contains("build_mid_turn_injection_callback"),
        "agent_loop.rs must still define build_mid_turn_injection_callback"
    );
    assert!(
        agent_loop.contains("load_events_latest_async"),
        "mid-turn injection callback must use load_events_latest_async"
    );
    assert!(
        !agent_loop.contains("load_events_consistent_async"),
        "agent_loop.rs must not route mid-turn injection through load_events_consistent_async"
    );

    assert_eq!(
        daemon
            .lines()
            .filter(|line| line.contains("load_events_latest_async()"))
            .count(),
        3,
        "daemon.rs must use load_events_latest_async at exact 3 decision points"
    );
    assert_eq!(
        daemon.matches("load_events_consistent_async").count(),
        0,
        "daemon.rs must not use load_events_consistent_async at #917 decision points"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retracted_user_message_is_not_executed_by_worker() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let counter = Arc::new(AtomicUsize::new(0));
    let prompts = Arc::new(AsyncMutex::new(Vec::<String>::new()));

    let config = local_nats_runtime_config(server.url());
    let worker_config = WorkerDaemonConfig::new("local", "worker-retract");
    let daemon = tokio::spawn({
        let cfg = config.clone();
        let calls = counter.clone();
        let captured_prompts = prompts.clone();
        async move {
            run_worker_daemon(
                cfg,
                worker_config,
                Some(fold_capture_call_fn(calls, captured_prompts)),
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let session_id = "retract-test";
    let log = NatsSessionLog::new(js.clone(), session_id);

    // Append a user message and an EditEntries that retracts it.
    append_retracted_user_message(&log, "msg-to-retract", "please ignore this").await?;

    // Append a valid user message that SHOULD be processed.
    log.append_event_async(&append_user_message_entry("valid-msg", "hello world"))
        .await?;

    // Activate the session.
    publish_session_activate(
        &js,
        "local",
        &SessionActivate::new(session_id, "ignored-agent"),
    )
    .await?;

    // Wait for the worker to process.
    wait_until(Duration::from_secs(10), || {
        counter.load(Ordering::SeqCst) >= 1
    })
    .await?;

    // Verify: only ONE call was made, and the prompt is "hello world", NOT "please ignore this"
    // If the bug is present, the prompt would contain the retracted message.
    assert_single_prompt(&prompts, "hello world").await;

    // Verify the durable log: no assistant turn for the retracted message.
    let entries = log.load_events_async().await?;
    assert_single_assistant_contains(&entries, "hello world");

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// Exercises the NoMessageFound / empty-stream branch of load_events_latest_async (#917).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_events_latest_async_empty_stream_returns_empty() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };

    let js = local_test_nats(server.url()).await?;
    let session_id = "latest-tail-empty-stream-test";
    let log = NatsSessionLog::new(js, session_id);

    let entries = log.load_events_latest_async().await?;
    assert!(entries.is_empty(), "empty stream should return empty Vec");

    Ok(())
}
