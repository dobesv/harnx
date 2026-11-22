//! End-to-end coverage for worker-dispatched in-process SessionStart hooks.

use super::tests::{
    env_lock, run_remote_round_trip_with_session_id_and_sink, seed_remote_config,
    spawn_metis_worker_with_hooks, spawn_test_nats, wait_for_condition, NoopEventSink,
    TestEnvGuard,
};
use crate::config::{HARNX_NATS_TOKEN_ENV, HARNX_NATS_URL_ENV, LOCAL_CLUSTER_KEY};
use crate::nats_worker::{new_remote_session_id, WorkerDaemonConfig};
use harnx_core::hooks::{
    EventHookDefinition, EventHookEntry, EventHookOnError, EventHooksList, HooksConfig,
};
use harnx_core::middleware::MiddlewareSourceConfig;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A SessionStart event hook that contributes one context line per run.
fn session_start_context_hook() -> HooksConfig {
    HooksConfig {
        session_start: EventHooksList {
            entries: vec![EventHookEntry::Full(EventHookDefinition {
                name: "greet".to_string(),
                source: MiddlewareSourceConfig::Bash {
                    bash: "add_context \"context from session start hook\"".to_string(),
                },
                tools: vec![],
                fetch: vec![],
                timeout: Duration::from_secs(30),
                on_error: EventHookOnError::Allow,
            })],
        },
        ..Default::default()
    }
}

/// Records each model call's `hook_context` — the queue-drained context the
/// loop attaches to the round's input.
fn hook_context_capturing_call_fn(
    captured: Arc<Mutex<Vec<String>>>,
) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |input, _config, _abort| {
        let captured = Arc::clone(&captured);
        let context = input.hook_context().unwrap_or_default().to_string();
        Box::pin(async move {
            captured.lock().expect("captured contexts").push(context);
            Ok((
                "stub remote reply over nats".to_string(),
                None,
                vec![],
                crate::client::CompletionTokenUsage::default(),
            ))
        })
    })
}

// This subprocess timing assertion is covered on Unix; Windows CI cannot
// observe the hook before its bounded worker shutdown deadline.
#[cfg_attr(
    windows,
    ignore = "worker hook observation exceeds deadline on Windows"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_hook_context_reaches_model_once_per_session() {
    let _env_guard = env_lock().await;
    let Some((url, mut child, _store_dir)) = spawn_test_nats().await else {
        return;
    };

    let seeded = seed_remote_config(&url);
    let _config_dir = TestEnvGuard::new("HARNX_CONFIG_DIR", seeded.config_dir());
    // The worker resolves the local NATS server from the environment.
    let _nats_url = TestEnvGuard::new(HARNX_NATS_URL_ENV, &url);
    let _nats_token = TestEnvGuard::new(HARNX_NATS_TOKEN_ENV, "session-start-token");

    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let worker = spawn_metis_worker_with_hooks(
        &url,
        hook_context_capturing_call_fn(Arc::clone(&captured)),
        WorkerDaemonConfig::managing(LOCAL_CLUSTER_KEY, "worker-metis"),
        Some(session_start_context_hook()),
    );

    let session_id = new_remote_session_id();
    run_remote_round_trip_with_session_id_and_sink(
        seeded.parent_config.clone(),
        session_id.clone(),
        Arc::new(NoopEventSink),
        LOCAL_CLUSTER_KEY,
    )
    .await
    .expect("first turn of a brand-new remote session");

    // SessionStart context must reach the first model call: the worker queues
    // it and the loop drains the queue into `hook_context` (issue #2198).
    let first_turn_has_context = |captured: &Arc<Mutex<Vec<String>>>| {
        let captured = captured.lock().expect("captured contexts");
        !captured.is_empty() && captured[0] == "context from session start hook"
    };
    assert!(
        wait_for_condition(Duration::from_secs(10), || first_turn_has_context(
            &captured
        ))
        .await,
        "SessionStart hook context must reach the first model call"
    );

    // A second turn re-activates the same session. SessionStart belongs to the
    // session, not the turn, so the context must not be re-added.
    run_remote_round_trip_with_session_id_and_sink(
        seeded.parent_config,
        session_id,
        Arc::new(NoopEventSink),
        LOCAL_CLUSTER_KEY,
    )
    .await
    .expect("second turn of the same remote session");

    let second_turn_is_clean = |captured: &Arc<Mutex<Vec<String>>>| {
        let captured = captured.lock().expect("captured contexts");
        captured.len() >= 2 && captured[1].is_empty()
    };
    assert!(
        wait_for_condition(Duration::from_secs(10), || second_turn_is_clean(&captured)).await,
        "SessionStart must not fire again for an existing session"
    );

    worker.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), worker).await;
    let _ = child.kill();
    let _ = child.wait();
}
