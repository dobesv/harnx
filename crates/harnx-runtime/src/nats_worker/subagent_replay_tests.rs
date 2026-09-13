use super::tests::{
    env_lock, seed_remote_config, spawn_metis_worker_with_call_fn, spawn_test_nats,
    subagent_test_env,
};
use crate::{
    nats_session_log::NatsSessionLog, utils::create_abort_signal, NatsSession, NatsSessionConfig,
    SessionInitializer,
};
use anyhow::Result;
use harnx_core::{
    api_types::CompletionTokenUsage, event::NullSink, message::MessageRole,
    session::SessionLogEntry, tool::ToolCall,
};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
const CI_SAFE_TIMEOUT: Duration = Duration::from_secs(30);
use anyhow::Context;
use harnx_execution_control::{ExecutionStore, OperationRef, OperationState, Owner};

const PARENT: &str = "detached-parent";
const CHILD: &str = "detached-child";
const CALL: &str = "original-delegation";

async fn session(js: &async_nats::jetstream::Context, id: &str) -> Result<NatsSession> {
    NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            initializer: SessionInitializer::named("metis", Default::default()),
            session_id: Some(id.into()),
            activation_route: crate::SessionActivationRoute::ClusterShared,
        },
        js.client().clone(),
        js.clone(),
        create_abort_signal(),
    )
    .await
}

/// The durable state left when a local frontend exits without interruption:
/// both worker futures and their leases are gone, but neither turn is cancelled.
async fn detached_delegation(
    js: &async_nats::jetstream::Context,
    dispatched: bool,
) -> Result<NatsSession> {
    let parent = session(js, PARENT).await?;
    parent.enqueue_text("delegate the work").await?;
    let store = parent.execution_store();
    let root = store.current(PARENT).await?.context("parent execution")?;
    claim_departed_owner(js, store, &root.reference).await?;
    let tool = OperationRef::new(PARENT, CALL);
    if dispatched {
        store.child(tool.clone(), root.reference).await?;
        store
            .claim(&tool, Owner::invocation("departed-subagent-server"))
            .await?;
    }
    let log = NatsSessionLog::new(js.clone(), PARENT);
    let tool_round = append_detached_tool_round(&log, dispatched).await?;
    let request = harnx_toolset::ToolRequest {
        replay: None,
        operation_id: CALL.into(),
        call_id: CALL.into(),
        tool: "session_prompt".into(),
        args: json!({"session_id": CHILD, "message": "finish child work"}),
        parent_session_id: Some(PARENT.into()),
        tool_call_id: Some(CALL.into()),
        capabilities: Default::default(),
    };
    let journal = harnx_toolset_server::invocation_journal::InvocationJournal::ensure(js).await?;
    journal
        .record(
            &request,
            ("metis_session_prompt", "departed-scope", "metis"),
            tool_round,
        )
        .await?;
    if dispatched {
        journal
            .checkpoint(PARENT, CALL, json!({"session_id": CHILD}))
            .await?;

        let child = session(js, CHILD)
            .await?
            .with_execution_parent(tool, CALL.into());
        child.enqueue_text("finish child work").await?;
        let operation = store.current(CHILD).await?.context("child execution")?;
        claim_departed_owner(js, store, &operation.reference).await?;
    }
    Ok(parent)
}

async fn append_detached_tool_round(log: &NatsSessionLog, dispatched: bool) -> Result<u64> {
    let tool_round = log
        .append_event_async(&SessionLogEntry::ToolCalls {
            text: "delegating".into(),
            thought: None,
            calls: vec![ToolCall::new(
                "metis_session_prompt".into(),
                json!({"session_id": CHILD, "message": "finish child work"}),
                Some(CALL.into()),
                None,
            )],
            timestamp: None,
            fence_token: None,
        })
        .await?;
    if dispatched {
        log.append_event_async(&SessionLogEntry::SubAgentStarted {
            agent: "metis".into(),
            session_id: CHILD.into(),
            invocation_id: Some(CALL.into()),
            tool_call_id: Some(CALL.into()),
            started_at: Some(chrono::Utc::now()),
        })
        .await?;
    }
    Ok(tool_round)
}

fn resumed_model(child_calls: Arc<AtomicUsize>) -> crate::AgentCallFn {
    Arc::new(move |_, config, _| {
        let id = config.read().session.as_ref().unwrap().id.clone();
        let child_calls = child_calls.clone();
        Box::pin(async move {
            let response = if id == CHILD {
                child_calls.fetch_add(1, Ordering::SeqCst);
                "child result"
            } else {
                "parent finished"
            };
            Ok((
                response.into(),
                None,
                vec![],
                CompletionTokenUsage::default(),
            ))
        })
    })
}

async fn claim_departed_owner(
    js: &async_nats::jetstream::Context,
    store: &ExecutionStore,
    reference: &OperationRef,
) -> Result<()> {
    let lease =
        crate::nats_lease::NatsSessionLease::acquire(crate::nats_lease::NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id: &reference.session_id,
            worker_id: "departed-worker".into(),
            generation: 1,
            config: Default::default(),
            session_metadata: None,
        })
        .await?
        .context("old lease")?;
    store
        .claim(
            reference,
            Owner {
                instance_id: lease.worker_id().into(),
                fence: lease.fence_token(),
            },
        )
        .await?;
    lease.release().await?;
    assert!(
        !crate::nats_lease::session_has_active_lease(js, &reference.session_id).await?,
        "release must not leave an acknowledged renewal behind"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopen_and_continue_recovers_original_subagent_invocation() -> Result<()> {
    recover_delegation(true, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_recovers_subagent_without_another_user_prompt() -> Result<()> {
    recover_delegation(false, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_registers_a_request_saved_before_dispatch() -> Result<()> {
    recover_delegation(false, false).await
}

async fn recover_delegation(continue_prompt: bool, dispatched: bool) -> Result<()> {
    let _env_guard = env_lock().await;
    let (url, mut nats, _store) = spawn_test_nats().await.context("nats-server required")?;
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let js = async_nats::jetstream::new(async_nats::connect(&url).await?);
    let parent = detached_delegation(&js, dispatched).await?;
    let child_calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_metis_worker_with_call_fn(&url, resumed_model(child_calls.clone()));
    if continue_prompt {
        let result = tokio::time::timeout(
            CI_SAFE_TIMEOUT,
            parent.run_turn("continue", Arc::new(NullSink), None),
        )
        .await??;
        assert_eq!(result.response.as_deref(), Some("parent finished"));
    } else {
        assert!(
            parent.activate_pending_turn().await?.is_some(),
            "reopening must activate the unfinished prompt"
        );
        await_parent_completion(&js).await?;
    }

    verify_recovered_delegation(&js, &child_calls).await?;
    daemon.abort();
    let _ = daemon.await;
    let _ = nats.kill();
    let _ = nats.wait();
    Ok(())
}

async fn await_parent_completion(js: &async_nats::jetstream::Context) -> Result<()> {
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            let entries = NatsSessionLog::new(js.clone(), PARENT)
                .load_events_async()
                .await?;
            if entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::TurnEnd { .. }))
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("automatic recovery timed out")?
}

async fn verify_recovered_delegation(
    js: &async_nats::jetstream::Context,
    child_calls: &AtomicUsize,
) -> Result<()> {
    let entries = NatsSessionLog::new(js.clone(), PARENT)
        .load_events_async()
        .await?;
    let results: Vec<_> = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => Some(results),
            _ => None,
        })
        .flatten()
        .filter(|result| result.id.as_deref() == Some(CALL))
        .collect();
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry,
                SessionLogEntry::SubAgentStarted { invocation_id: Some(id), .. } if id == CALL
            ))
            .count(),
        1,
        "recovery must not duplicate the durable start event"
    );
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].output["response"], "child result",
        "recover the original delegation instead of synthesizing an interruption"
    );
    assert_eq!(child_calls.load(Ordering::SeqCst), 1);
    let child_entries = NatsSessionLog::new(js.clone(), CHILD)
        .load_events_async()
        .await?;
    assert_eq!(
        child_entries
            .iter()
            .filter(|(_, entry)| matches!(
                entry,
                SessionLogEntry::Message {
                    role: MessageRole::User,
                    ..
                }
            ))
            .count(),
        1,
        "recovery must not append the child prompt again"
    );
    let store = ExecutionStore::ensure(js, 1).await?;
    tokio::time::timeout(CI_SAFE_TIMEOUT, async {
        loop {
            let root = store.current(PARENT).await?.context("parent")?;
            if store.status(&root.reference).await?.state == OperationState::Completed {
                break Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    let root = store.session(PARENT, None, None).await?;
    let next = OperationRef::new(PARENT, "next-delegation");
    store.child(next.clone(), root.reference).await?;
    store
        .session(CHILD, Some(next), Some("next-delegation"))
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_allocation_survives_a_crash_before_checkpointing() -> Result<()> {
    let (url, mut nats, _store) = spawn_test_nats().await.context("nats-server required")?;
    let js = async_nats::jetstream::new(async_nats::connect(&url).await?);
    let store = crate::nats_session_metadata::SessionMetadataStore::ensure(&js, 1).await?;
    let initializer = SessionInitializer::named("metis", Default::default());
    let reserve = |id| {
        crate::utils::session_name::reserve_invocation_session_id(
            &store,
            &initializer,
            id,
            1_780_000_000_000,
        )
    };
    let first = reserve("parent/first").await?;
    // No checkpoint is written. Another allocation occupies the next candidate.
    let second = reserve("parent/second").await?;
    let (replayed, concurrent) = tokio::join!(reserve("parent/first"), reserve("parent/first"));
    assert_eq!(first.len(), 6);
    assert_ne!(first, second);
    assert_eq!(replayed?, first);
    assert_eq!(concurrent?, first);
    let _ = nats.kill();
    let _ = nats.wait();
    Ok(())
}
