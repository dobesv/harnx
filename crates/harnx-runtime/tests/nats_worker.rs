//! Integration test: drive run_agent_loop with NATS-backed persistence.
//!
//! Validates end-to-end persistence of a full turn via NatsSessionLog.

#[path = "nats_worker/cancellation.rs"]
mod cancellation;
mod common;
use harnx_runtime::nats_session_log::NatsSessionLog;
#[allow(dead_code)]
#[path = "common/generation.rs"]
mod generation;
#[path = "nats_worker/multi_round_resume.rs"]
mod multi_round_resume;
#[path = "nats_worker/session_completion.rs"]
mod session_completion;
use session_completion::{activate_session, local_test_nats, wait_for_worker_daemon_idle};
#[path = "nats_worker/session_metadata.rs"]
mod session_metadata;
#[allow(dead_code)]
#[path = "common/worker.rs"]
mod worker;
use worker::{
    acquire_worker_lease, counting_stub_call_fn, local_nats_config, local_nats_runtime_config,
    poll_until, require_nats_server, short_lease_config, spawn_worker_daemon_with_call_fn,
    storage_key, wait_for_worker_session_cleanup, wait_until, EnvGuard, NatsServerSpec,
    CI_SAFE_TIMEOUT,
};
#[path = "nats_worker/abort_signal.rs"]
mod abort_signal;
#[path = "nats_worker/dispatch_and_fencing.rs"]
mod dispatch_and_fencing;
#[path = "nats_worker/leader_reads.rs"]
mod leader_reads;
#[path = "nats_worker/orphan_repair.rs"]
mod orphan_repair;
#[path = "nats_worker/prompt_injection.rs"]
mod prompt_injection;
#[path = "nats_worker/rewind_and_retraction.rs"]
mod rewind_and_retraction;
#[path = "nats_worker/turn_continuation.rs"]
mod turn_continuation;
#[path = "nats_worker/turn_persistence.rs"]
mod turn_persistence;

use anyhow::Result;
use common::spawn_nats_server;
use harnx_core::{
    event::NullSink,
    message::MessageRole,
    require_nextest,
    session::SessionLogEntry,
    session_reconstruct::{reconstruct_state, reconstruct_state_from_nats, TurnStatus},
    tool::ToolCall,
};
use harnx_runtime::{
    client::CompletionTokenUsage,
    config::Config,
    nats_lease::{lease_holder_in, open_lease_bucket, NatsLeaseConfig},
    nats_session_metadata::{
        SessionInitializer, SessionMetadata, SessionMetadataStore, SessionOverrides,
    },
    nats_worker::{
        publish_session_activate, run_agent_loop_with_nats, run_worker_daemon,
        NatsSessionLogBackend, RunAgentLoopArgs, SessionActivate, WorkerDaemonConfig,
    },
    utils::create_abort_signal,
    ControlCommand, NatsSession, NatsSessionConfig,
};
use std::sync::LazyLock;

static MID_ROUND_APPEND_READY: LazyLock<Notify> = LazyLock::new(Notify::new);
static MID_ROUND_APPEND_DONE: LazyLock<Notify> = LazyLock::new(Notify::new);
static MID_ROUND_FINAL_CALLS: AtomicUsize = AtomicUsize::new(0);
static MID_ROUND_RELOAD_SEEN: AtomicUsize = AtomicUsize::new(0);
static END_TURN_CALLS: AtomicUsize = AtomicUsize::new(0);
use parking_lot::RwLock;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, Notify};

async fn seed_session_metadata(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<(SessionMetadataStore, SessionMetadata)> {
    let store = SessionMetadataStore::ensure(jetstream, 1).await?;
    let metadata = SessionMetadata::new(
        session_id,
        SessionInitializer::inline("", Default::default(), SessionOverrides::default()),
    );
    store.create(&metadata).await?;
    Ok((store, metadata))
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

fn abort_blocked_call_fn(
    entered: Arc<Notify>,
    model_dropped: tokio_util::sync::CancellationToken,
) -> harnx_runtime::agent_loop::AgentCallFn {
    Arc::new(move |_input, _config, _abort| {
        let entered = Arc::clone(&entered);
        let model_dropped = model_dropped.clone();
        Box::pin(async move {
            let _drop = model_dropped.drop_guard();
            entered.notify_one();
            std::future::pending().await
        })
    })
}

async fn acquire_test_lease(
    js: async_nats::jetstream::Context,
    session_id: &str,
    worker_id: &str,
) -> Result<Arc<harnx_runtime::nats_lease::NatsSessionLease>> {
    use harnx_runtime::nats_lease::{NatsLeaseConfig, NatsSessionLease};

    Ok(Arc::new(
        NatsSessionLease::acquire(harnx_runtime::nats_lease::NatsLeaseAcquireParams {
            jetstream: js,
            session_id: &storage_key(session_id),
            worker_id: worker_id.to_string(),
            generation: 1,
            config: NatsLeaseConfig {
                ttl: Duration::from_secs(30),
                renew_interval: Duration::from_secs(10),
                ..Default::default()
            },
            session_metadata: None,
        })
        .await?
        .expect("acquire"),
    ))
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

// Reset static state between tests
fn reset_test_state() {
    // These must be reset in case tests are run in the same process
    // Note: We can't truly clear Notify state across tests without async runtime
    // Just reset the counters - tests may flake if run in same process
    MID_ROUND_FINAL_CALLS.store(0, Ordering::SeqCst);
    END_TURN_CALLS.store(0, Ordering::SeqCst);
}
