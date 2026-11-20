//! NATS-worker seam coverage for in-process lifecycle event hooks (review
//! F2/F36, N1, F4, F37): hook-added context must survive the first tool
//! round's mid-turn injection callback, a UserPromptSubmit rewrite must reach
//! the admitted worker prompt, and a continuation after a saved legacy
//! approval denial must produce the synthetic denial result without
//! re-firing the prompt-hook chain.

use super::*;
use crate::worker::append_admitted_fixture_user;
use anyhow::Context as _;
use harnx_core::hooks::{
    EventHookDefinition, EventHookEntry, EventHookOnError, EventHooksList, HooksConfig,
};
use harnx_core::middleware::MiddlewareSourceConfig;

type RecordedRequests = Arc<AsyncMutex<Vec<Vec<String>>>>;

/// A one-entry UserPromptSubmit hook with the given bash source.
fn prompt_hook_config(bash: &str) -> HooksConfig {
    let list = EventHooksList {
        entries: vec![EventHookEntry::Full(EventHookDefinition {
            name: "seam-hook".to_string(),
            source: MiddlewareSourceConfig::Bash {
                bash: bash.to_string(),
            },
            fetch: vec![],
            timeout: Duration::from_secs(30),
            on_error: EventHookOnError::Allow,
        })],
    };
    HooksConfig {
        user_prompt_submit: list,
        ..HooksConfig::default()
    }
}

/// A model that records the user texts of every request it serves. With
/// `tool_in_first_round` it emits a tool call first (driving the worker's
/// mid-turn injection callback); later rounds answer with `final_text`.
fn recording_call_fn(
    requests: RecordedRequests,
    final_text: &'static str,
    tool_in_first_round: bool,
) -> harnx_runtime::agent_loop::AgentCallFn {
    let round = Arc::new(AtomicUsize::new(0));
    Arc::new(move |input, config, _abort| {
        let requests = Arc::clone(&requests);
        let round = Arc::clone(&round);
        Box::pin(async move {
            let users = harnx_runtime::config::input::build_messages(input, config)
                .context("worker request should build model messages")?
                .into_iter()
                .filter(|message| message.role.is_user())
                .map(|message| message.content.to_text())
                .collect::<Vec<_>>();
            requests.lock().await.push(users);
            if tool_in_first_round && round.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok((
                    "calling tool".to_string(),
                    None,
                    vec![ToolCall::new(
                        "echo".to_string(),
                        json!({}),
                        Some("call-seam-1".to_string()),
                        None,
                    )],
                    CompletionTokenUsage::default(),
                ))
            } else {
                Ok((
                    final_text.to_string(),
                    None,
                    vec![],
                    CompletionTokenUsage::default(),
                ))
            }
        })
    })
}

fn daemon_config_with_hooks(
    server_url: &str,
    hooks: HooksConfig,
) -> harnx_runtime::config::GlobalConfig {
    let mut config = local_nats_config(NatsServerSpec {
        name: "local",
        url: server_url,
        token: None,
    });
    config.data.hooks = Some(Box::new(hooks));
    Arc::new(ConfigLock::new(config))
}

async fn worker_session(
    server_url: &str,
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<NatsSession> {
    Ok(NatsSession::new(
        NatsSessionConfig {
            cluster: "local".to_string(),
            initializer: SessionInitializer::inline(
                "",
                Default::default(),
                SessionOverrides::default(),
            ),
            session_id: Some(session_id.to_string()),
            activation_route: harnx_runtime::SessionActivationRoute::ClusterShared,
        },
        async_nats::connect(server_url).await?,
        jetstream.clone(),
        create_abort_signal(),
    )
    .await?
    .with_external_admission())
}

async fn wait_for_requests(requests: &RecordedRequests, at_least: usize) -> Result<()> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if requests.lock().await.len() >= at_least {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("model requests never arrived")?
}

async fn wait_for_active_lease(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<()> {
    let session_key = storage_key(session_id);
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            if harnx_runtime::nats_lease::session_has_active_lease(jetstream, &session_key).await? {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("worker never took the session lease")?
}

fn user_texts(entries: &[(u64, SessionLogEntry)]) -> Vec<String> {
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

fn assistant_texts(entries: &[(u64, SessionLogEntry)]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message { role, content, .. } if role.is_assistant() => {
                Some(content.to_text())
            }
            _ => None,
        })
        .collect()
}

/// Hook-added context rows: the durable rows whose reserved id marks them as
/// runtime-authored context (review F2/F14).
fn hook_context_texts(entries: &[(u64, SessionLogEntry)]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message { id, content, .. }
                if id
                    .as_deref()
                    .is_some_and(harnx_core::runtime_note::is_hook_context_message_id) =>
            {
                Some(content.to_text())
            }
            _ => None,
        })
        .collect()
}

/// Review F2/F36: on a NATS worker, a UserPromptSubmit `add_context` rides
/// `Input::hook_context` into a durable row. The first tool round's mid-turn
/// injection callback used to fold that row in as an unadmitted user message
/// and fail the turn with "injected user row has no durable admission". The
/// reserved hook-context id keeps it out of the fold while it still reaches
/// the model and the transcript — exactly once per request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_hook_context_survives_first_tool_round_on_worker() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config = daemon_config_with_hooks(
        server.url(),
        prompt_hook_config("add_context \"hook context\""),
    );
    let requests: RecordedRequests = Arc::new(AsyncMutex::new(Vec::new()));
    let daemon = spawn_worker_daemon_with_call_fn(
        config,
        "worker-hook-context",
        recording_call_fn(Arc::clone(&requests), "all done", true),
    )
    .await?;

    let js = local_test_nats(server.url()).await?;
    let session_id = "event-hook-context-tool-round";
    let session = worker_session(server.url(), &js, session_id).await?;
    let metrics_before = harnx_runtime::nats_metrics::snapshot();
    session
        .clone()
        .enqueue_text("worker prompt")
        .await?
        .into_activation_result()?;
    wait_for_worker_daemon_idle(&js, session_id, metrics_before.lease_acquisitions).await?;
    wait_for_requests(&requests, 2).await?;

    let requests = requests.lock().await.clone();
    assert_eq!(
        requests.len(),
        2,
        "one prompt round and one post-tool round: {requests:?}"
    );
    for (round, users) in requests.iter().enumerate() {
        assert_eq!(
            users
                .iter()
                .filter(|text| text.as_str() == "hook context")
                .count(),
            1,
            "round {round} must carry the hook context exactly once: {users:?}"
        );
    }

    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);
    let entries = log.load_events_async().await?;
    assert_eq!(
        hook_context_texts(&entries),
        vec!["hook context".to_string()],
        "exactly one durable hook-context row"
    );
    assert!(
        !entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. })),
        "the turn must not fail on injected-prompt admission"
    );
    assert!(
        assistant_texts(&entries)
            .iter()
            .any(|text| text.contains("all done")),
        "the turn completes"
    );

    // Replay: a later turn on the same session must not re-deliver the
    // context row as an executable prompt. Each turn appends its own context
    // row (the hook runs per prompt), but the first row must never come back
    // as an unadmitted user message.
    let metrics_before = harnx_runtime::nats_metrics::snapshot();
    session
        .clone()
        .enqueue_text("second prompt")
        .await?
        .into_activation_result()?;
    wait_for_worker_daemon_idle(&js, session_id, metrics_before.lease_acquisitions).await?;
    let entries = log.load_events_async().await?;
    assert_eq!(
        hook_context_texts(&entries),
        vec!["hook context".to_string(), "hook context".to_string()],
        "the replayed turn appends its own context row and re-delivers nothing"
    );
    assert_eq!(
        user_texts(&entries)
            .iter()
            .filter(|text| text.as_str() == "second prompt")
            .count(),
        1,
        "the second turn runs its own prompt exactly once"
    );
    assert!(
        !entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. })),
        "the replayed turn must not fail admission checks"
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// Review N1: a UserPromptSubmit rewrite must reach the model on a worker
/// turn. The worker folds its prompt out of the admitted durable row and
/// suppresses re-appending it, so the rewrite has to land on that row —
/// admission identity intact — instead of only on `Input`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_hook_rewrite_reaches_worker_model_request() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config = daemon_config_with_hooks(
        server.url(),
        prompt_hook_config("rewrite_prompt \"rewritten worker prompt\""),
    );
    let requests: RecordedRequests = Arc::new(AsyncMutex::new(Vec::new()));
    let daemon = spawn_worker_daemon_with_call_fn(
        config,
        "worker-rewrite",
        recording_call_fn(Arc::clone(&requests), "all done", false),
    )
    .await?;

    let js = local_test_nats(server.url()).await?;
    let session_id = "event-hook-rewrite";
    let session = worker_session(server.url(), &js, session_id).await?;
    let metrics_before = harnx_runtime::nats_metrics::snapshot();
    session
        .clone()
        .enqueue_text("raw worker prompt")
        .await?
        .into_activation_result()?;
    wait_for_worker_daemon_idle(&js, session_id, metrics_before.lease_acquisitions).await?;
    wait_for_requests(&requests, 1).await?;

    let requests = requests.lock().await.clone();
    assert_eq!(
        requests.len(),
        1,
        "a single-round worker turn: {requests:?}"
    );
    let users = requests
        .last()
        .context("the worker turn must call the model")?;
    assert_eq!(
        users
            .iter()
            .filter(|text| text.as_str() == "rewritten worker prompt")
            .count(),
        1,
        "the model must see the rewritten prompt once: {users:?}"
    );
    assert!(
        !users.iter().any(|text| text.contains("raw worker prompt")),
        "the original client text must not reach the model: {users:?}"
    );

    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);
    let entries = log.load_events_async().await?;
    assert_eq!(
        user_texts(&entries)
            .iter()
            .filter(|text| text.as_str() == "raw worker prompt")
            .count(),
        1,
        "the durable log keeps the original admitted row exactly once"
    );
    assert!(
        !user_texts(&entries)
            .iter()
            .any(|text| text.contains("rewritten worker prompt")),
        "the rewrite must not append a second unanswered user row"
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}

/// Review F4 + F37: a seeded legacy approval round, denied through the
/// durable decision path, resumes on a worker. The denied call must get its
/// synthetic denial result and the model must be called — without re-firing
/// the UserPromptSubmit chain that already ran for the turn's prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_hook_hitl_denial_continuation_does_not_refire_prompt_hooks() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config = daemon_config_with_hooks(
        server.url(),
        prompt_hook_config("rewrite_prompt \"rewritten prompt\"\nadd_context \"hook context\""),
    );
    let requests: RecordedRequests = Arc::new(AsyncMutex::new(Vec::new()));
    let daemon = spawn_worker_daemon_with_call_fn(
        config,
        "worker-denied-continuation",
        recording_call_fn(Arc::clone(&requests), "all done", false),
    )
    .await?;

    let js = local_test_nats(server.url()).await?;
    let session_id = "event-hook-denied-continuation";
    let log = NatsSessionLog::new_with_replicas(js.clone(), storage_key(session_id), 1);
    seed_session_metadata(&js, session_id).await?;
    append_admitted_fixture_user(&log, "user-1", "original request").await?;
    log.append_event_async(&SessionLogEntry::ToolCalls {
        text: "assistant wants a tool".to_string(),
        thought: None,
        calls: vec![ToolCall::new(
            "echo".to_string(),
            json!({}),
            Some("call-deny".to_string()),
            None,
        )],
        timestamp: None,
        fence_token: None,
    })
    .await?;
    log.append_event_async(&SessionLogEntry::HitlApprovalRequested {
        tool_call_id: "call-deny".to_string(),
        summary: "approve echo".to_string(),
        // Seeded like a pre-T3.5 session's saved request: no live lease, so
        // no fence. The resume path must not treat it as fenced-off.
        fence_token: 0,
    })
    .await?;

    let session = worker_session(server.url(), &js, session_id).await?;
    // Snapshot before activation: wait_for_worker_daemon_idle gates on the
    // lease acquisition the activation causes, so the baseline must precede
    // the worker picking the session up.
    let metrics_before = harnx_runtime::nats_metrics::snapshot();
    activate_session(&js, session_id).await?;
    // The worker subscribes to the control subject once it holds the lease;
    // deciding before that would publish the decision into the void.
    wait_for_active_lease(&js, session_id).await?;
    assert!(
        session
            .decide_hitl_approval("call-deny", false, Some("no".into()))
            .await?
    );

    wait_for_requests(&requests, 1).await?;
    wait_for_worker_daemon_idle(&js, session_id, metrics_before.lease_acquisitions).await?;

    let requests = requests.lock().await.clone();
    assert_eq!(
        requests.len(),
        1,
        "one continuation model call: {requests:?}"
    );
    let users = requests.last().expect("continuation request");
    assert!(
        users.iter().any(|text| text.as_str() == "original request"),
        "the continuation must answer the original prompt: {users:?}"
    );
    assert!(
        !users
            .iter()
            .any(|text| text.contains("rewritten prompt") || text.contains("hook context")),
        "the prompt-hook chain must not re-fire after tools ran: {users:?}"
    );

    let entries = log.load_events_async().await?;
    assert!(
        hook_context_texts(&entries).is_empty(),
        "no hook context may be appended by the continuation (F4)"
    );
    let denied = entries.iter().any(|(_, entry)| match entry {
        SessionLogEntry::ToolResults { results, .. } => results.iter().any(|result| {
            result.id.as_deref() == Some("call-deny")
                && result.output.get("is_error") == Some(&json!(true))
        }),
        _ => false,
    });
    assert!(
        denied,
        "the denied call must persist a synthetic error result (F37): {:?}",
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::ToolResults { .. }))
            .count()
    );
    assert!(
        assistant_texts(&entries)
            .iter()
            .any(|text| text.contains("all done")),
        "the continuation completes the turn"
    );

    daemon.abort();
    let _ = daemon.await;
    Ok(())
}
