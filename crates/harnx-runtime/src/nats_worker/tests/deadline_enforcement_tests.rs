//! Production admission and worker supervisor tests, not resolver-only fixtures.
use super::*;
use crate::nats_session::{InterruptOutcome, InterruptRequest};
use crate::SessionActivationRoute;
use std::sync::atomic::AtomicUsize;

fn blocking_model(
    entered: Arc<tokio::sync::Notify>,
    calls: Arc<AtomicUsize>,
) -> crate::agent_loop::AgentCallFn {
    Arc::new(move |input, _config, _abort| {
        let entered = entered.clone();
        let calls = calls.clone();
        let block = input.text().contains("block");
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            if block {
                entered.notify_one();
                // Deliberately ignore the abort signal, like a blocked nonstreaming
                // provider. Only worker supervision can drop this future.
                std::future::pending::<()>().await;
            }
            Ok(("reply".into(), None, Vec::new(), Default::default()))
        })
    })
}

async fn session(url: &str, id: &str) -> NatsSession {
    let client = async_nats::connect(url).await.unwrap();
    NatsSession::new(
        cluster_shared_session_config("local", id),
        client.clone(),
        async_nats::jetstream::new(client),
        harnx_core::abort::create_abort_signal(),
    )
    .await
    .unwrap()
}

async fn wait_for_cancel(log: &NatsSessionLog) -> Vec<(u64, SessionLogEntry)> {
    tokio::time::timeout(NATS_TEST_CONDITION_TIMEOUT, async {
        loop {
            let entries = log.load_events_async().await.unwrap();
            if entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. }))
            {
                return entries;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("worker must durably stop without a caller timer")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn omitted_default_root_freezes_24_hours_and_worker_timer_survives_caller_loss() {
    use crate::nats_session_metadata::{
        AdmissionAuthority, InvocationAdmission, RunLimitsPolicySource,
    };
    let _env_guard = env_lock().await;
    let (url, _nats, _dir) = spawn_test_nats().await.expect("nats-server required");
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let entered = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon =
        spawn_metis_worker_with_call_fn(&url, blocking_model(entered.clone(), calls.clone()));
    subagent_discovery_tests::wait_for_cluster_worker(&seeded.parent_config, "local")
        .await
        .unwrap();
    let root = session(&url, "default-24-hour-timer").await;
    // Failure injection: late pickup of an original admission, not a shortened policy.
    // Keep the full 86400-second allowance; leave 10 seconds for broker/worker setup.
    let original_time = chrono::Utc::now() - chrono::Duration::seconds(86400 - 10);
    let mut intent = InvocationAdmission::new(
        &AdmissionAuthority::External {
            admitted_at: original_time,
        },
        "default-24-hour".into(),
        None,
        None,
    );
    intent.prompt_content = Some(harnx_core::message::MessageContent::Text(
        "block default".into(),
    ));
    root.metadata_store()
        .reserve_admission(root.storage_key(), &intent, &[])
        .await
        .unwrap();
    let observer = root
        .clone()
        .with_external_admission()
        .with_admission_id("default-24-hour".into());
    let follower = tokio::spawn(async move {
        observer
            .run_turn("block default", Arc::new(NoopEventSink), None)
            .await
    });
    tokio::time::timeout(NATS_TEST_CONDITION_TIMEOUT, entered.notified())
        .await
        .unwrap();
    let frozen = root
        .metadata_store()
        .get_invocation_limits(root.storage_key(), "default-24-hour")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frozen.admitted_at, original_time);
    assert_eq!(
        frozen.deadline,
        Some(original_time + chrono::Duration::seconds(86400))
    );
    assert_eq!(frozen.policy_source, RunLimitsPolicySource::GlobalDefault);
    assert!(frozen.parent_invocation.is_none());
    assert_eq!(
        root.metadata_store()
            .get_run_limits(root.storage_key(), frozen.run_id.as_str())
            .await
            .unwrap(),
        Some(frozen.clone())
    );
    follower.abort();
    let log = NatsSessionLog::new(root.jetstream().clone(), root.storage_key());
    let entries = wait_for_cancel(&log).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let reasons: Vec<_> = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Cancel { requested_by, .. } => requested_by.as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(reasons.len(), 1);
    let timeout = crate::parse_timeout_terminal(reasons[0]).expect("worker timeout label");
    assert_eq!(timeout.scope, crate::TimeoutScope::OuterRun);
    assert_eq!(timeout.deadline, frozen.deadline.unwrap());
    assert_eq!(timeout.run_id, frozen.run_id.as_str());
    let fresh = root
        .clone()
        .with_external_admission()
        .run_turn("fresh default", Arc::new(NoopEventSink), None)
        .await
        .unwrap();
    assert_eq!(fresh.response.as_deref(), Some("reply"));
    let next_intent = root
        .metadata_store()
        .prompt_admission(root.storage_key(), &fresh.user_msg_id)
        .await
        .unwrap()
        .unwrap();
    let next = root
        .metadata_store()
        .get_invocation_limits(root.storage_key(), next_intent.invocation_id.as_str())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(next.run_id, frozen.run_id);
    assert_eq!(
        next.deadline,
        Some(next.admitted_at + chrono::Duration::seconds(86400))
    );
    let saved = root
        .metadata_store()
        .get_invocation_limits(root.storage_key(), "default-24-hour")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved, frozen);
    daemon.abort();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_deadline_stops_blocked_model_after_caller_disappears_and_new_run_survives() {
    let _env_guard = env_lock().await;
    let Some((url, _nats, _dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let entered = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon =
        spawn_metis_worker_with_call_fn(&url, blocking_model(entered.clone(), calls.clone()));
    subagent_discovery_tests::wait_for_cluster_worker(&seeded.parent_config, "local")
        .await
        .unwrap();
    let root = session(&url, "deadline-caller-loss").await;
    let log = NatsSessionLog::new(root.jetstream().clone(), root.storage_key());
    let follower_session = root
        .clone()
        .with_external_admission()
        .with_admission_timeout(Some(3));
    let follower = tokio::spawn(async move {
        follower_session
            .run_turn("block forever", Arc::new(NoopEventSink), None)
            .await
    });
    tokio::time::timeout(NATS_TEST_CONDITION_TIMEOUT, entered.notified())
        .await
        .unwrap();
    follower.abort();
    let entries = wait_for_cancel(&log).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. }))
            .count(),
        1
    );
    assert!(!entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Error { .. })));
    let first_id = entries
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::Message {
                id: Some(id), role, ..
            } if role.is_user() => Some(id.clone()),
            _ => None,
        })
        .unwrap();
    let admission = root
        .metadata_store()
        .prompt_admission(root.storage_key(), &first_id)
        .await
        .unwrap()
        .unwrap();
    let frozen = root
        .metadata_store()
        .get_invocation_limits(root.storage_key(), admission.invocation_id.as_str())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        frozen.deadline,
        Some(admission.admitted_at + chrono::Duration::seconds(3))
    );
    let marker = entries
        .iter()
        .find_map(|(_, e)| match e {
            SessionLogEntry::Cancel {
                requested_by: Some(label),
                ..
            } => crate::parse_timeout_terminal(label),
            _ => None,
        })
        .expect("worker timeout marker");
    assert_eq!(marker.scope, crate::TimeoutScope::OuterRun);
    assert_eq!(marker.deadline, frozen.deadline.unwrap());
    assert_eq!(marker.run_id, frozen.run_id.as_str());
    assert_eq!(marker.invocation_id, frozen.invocation_id.as_str());
    let reply = root
        .clone()
        .with_external_admission()
        .with_admission_timeout(Some(0))
        .run_turn("reply now", Arc::new(NoopEventSink), None)
        .await
        .unwrap();
    assert_eq!(reply.response.as_deref(), Some("reply"));
    let second = root
        .metadata_store()
        .prompt_admission(root.storage_key(), &reply.user_msg_id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(second.run_id, admission.run_id);
    assert_eq!(
        root.metadata_store()
            .get_invocation_limits(root.storage_key(), admission.invocation_id.as_str())
            .await
            .unwrap(),
        Some(frozen)
    );
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_run_timer_cannot_interrupt_a_later_blocked_independent_run() {
    let _env_guard = env_lock().await;
    let Some((url, _nats, _dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let entered = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let daemon = spawn_metis_worker_with_call_fn(&url, blocking_model(entered.clone(), calls));
    subagent_discovery_tests::wait_for_cluster_worker(&seeded.parent_config, "local")
        .await
        .unwrap();
    let root = session(&url, "deadline-fence").await;
    let first = root
        .clone()
        .with_external_admission()
        .with_admission_timeout(Some(3))
        .run_turn("reply", Arc::new(NoopEventSink), None)
        .await
        .unwrap();
    let new_session = root
        .clone()
        .with_external_admission()
        .with_admission_timeout(Some(0));
    let second = tokio::spawn(async move {
        new_session
            .run_turn("block later", Arc::new(NoopEventSink), None)
            .await
    });
    tokio::time::timeout(NATS_TEST_CONDITION_TIMEOUT, entered.notified())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(!second.is_finished(), "old timer cancelled later run");
    let log = NatsSessionLog::new(root.jetstream().clone(), root.storage_key());
    let entries = log.load_events_async().await.unwrap();
    assert!(!entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })));
    let old_stop = crate::nats_session::interrupt::interrupt_invocation(
        root.jetstream(),
        &root.jetstream().client(),
        &SessionActivationRoute::ClusterShared,
        InterruptRequest {
            session_id: root.storage_key().into(),
            cluster: "local".into(),
            replicas: 1,
            cancellation_id: "old-expiry".into(),
            requested_by: "test timer".into(),
            reason: "expired earlier invocation".into(),
        },
        first.user_msg_seq,
    )
    .await
    .unwrap();
    assert_eq!(old_stop, InterruptOutcome::Idle);
    assert!(!second.is_finished());
    root.interrupt("test cleanup").await.unwrap();
    let _ = tokio::time::timeout(NATS_TEST_CONDITION_TIMEOUT, second)
        .await
        .unwrap();
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_child_override_inherits_nonrenewing_run_and_parent_effective_deadline() {
    let _env_guard = env_lock().await;
    let Some((url, _nats, _dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let parent_calls = Arc::new(AtomicUsize::new(0));
    let child_started = Arc::new(tokio::sync::Notify::new());
    let call_fn: crate::agent_loop::AgentCallFn = {
        let parent_calls = parent_calls.clone();
        let child_started = child_started.clone();
        Arc::new(move |input, _config, _abort| {
            let is_child = input.text() == "block child";
            let parent_calls = parent_calls.clone();
            let child_started = child_started.clone();
            Box::pin(async move {
                if is_child {
                    child_started.notify_one();
                    std::future::pending::<()>().await;
                }
                parent_calls.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "delegate".into(),
                    None,
                    vec![ToolCall::new(
                        "metis_session_prompt".into(),
                        json!({"message": "block child", "timeout_secs": 0}),
                        Some("child-zero".into()),
                        None,
                    )],
                    Default::default(),
                ))
            })
        })
    };
    let daemon = spawn_metis_worker_with_call_fn(&url, call_fn);
    subagent_discovery_tests::wait_for_cluster_worker(&seeded.parent_config, "local")
        .await
        .unwrap();
    let root = session(&url, "deadline-child-zero").await;
    let parent_log = NatsSessionLog::new(root.jetstream().clone(), root.storage_key());
    let follower_session = root
        .clone()
        .with_external_admission()
        .with_admission_timeout(Some(5));
    let follower = tokio::spawn(async move {
        follower_session
            .run_turn("delegate blocked child", Arc::new(NoopEventSink), None)
            .await
    });
    tokio::time::timeout(NATS_TEST_CONDITION_TIMEOUT, child_started.notified())
        .await
        .unwrap();
    follower.abort();
    let parent_entries = wait_for_cancel(&parent_log).await;
    let round = parent_entries
        .iter()
        .find_map(|(seq, entry)| match entry {
            SessionLogEntry::ToolCalls { calls, .. }
                if calls
                    .iter()
                    .any(|call| call.id.as_deref() == Some("child-zero")) =>
            {
                Some(*seq)
            }
            _ => None,
        })
        .expect("parent durably recorded delegation");
    let journal =
        harnx_toolset_server::invocation_journal::InvocationJournal::ensure(root.jetstream(), 1)
            .await
            .unwrap();
    let recorded = journal
        .find(root.storage_key(), round, "child-zero")
        .await
        .unwrap()
        .expect("original wire request journaled");
    let child_id = recorded.checkpoint.as_ref().unwrap()["session_id"]
        .as_str()
        .expect("durable child checkpoint")
        .to_owned();
    let child_log = NatsSessionLog::for_agent(root.jetstream().clone(), "metis", &child_id);
    let child_entries = wait_for_cancel(&child_log).await;
    let parent_id = parent_entries
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::Message {
                id: Some(id), role, ..
            } if role.is_user() => Some(id),
            _ => None,
        })
        .unwrap();
    let child_prompt = child_entries
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::Message {
                id: Some(id), role, ..
            } if role.is_user() => Some(id),
            _ => None,
        })
        .unwrap();
    let store = root.metadata_store();
    let parent_admission = store
        .prompt_admission(root.storage_key(), parent_id)
        .await
        .unwrap()
        .unwrap();
    let child_admission = store
        .prompt_admission(child_log.storage_key(), child_prompt)
        .await
        .unwrap()
        .unwrap();
    let parent_limits = store
        .get_invocation_limits(root.storage_key(), parent_admission.invocation_id.as_str())
        .await
        .unwrap()
        .unwrap();
    let child_limits = store
        .get_invocation_limits(
            child_log.storage_key(),
            child_admission.invocation_id.as_str(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        child_admission.timeout_secs, None,
        "zero is normalized to inherited policy"
    );
    assert_eq!(parent_limits.run_id, child_limits.run_id);
    assert_eq!(parent_limits.deadline, child_limits.deadline);
    assert_eq!(
        child_limits
            .parent_invocation
            .as_ref()
            .unwrap()
            .invocation_id,
        parent_limits.invocation_id
    );
    assert!(child_limits.admitted_at >= parent_limits.admitted_at);
    assert_eq!(
        parent_calls.load(Ordering::SeqCst),
        1,
        "expired parent must not dispatch another model round"
    );
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_queued_admission_stops_before_model_dispatch_on_first_claim() {
    let _env_guard = env_lock().await;
    let Some((url, _nats, _dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let root = session(&url, "expired-queue").await;
    let original_time = chrono::Utc::now() - chrono::Duration::seconds(30);
    let mut intent = crate::nats_session_metadata::InvocationAdmission::new(
        &crate::nats_session_metadata::AdmissionAuthority::External {
            admitted_at: original_time,
        },
        "queued-invocation".into(),
        Some(1),
        None,
    );
    intent.prompt_content = Some(harnx_core::message::MessageContent::Text(
        "original queued work".into(),
    ));
    root.metadata_store()
        .reserve_admission(root.storage_key(), &intent, &[])
        .await
        .unwrap();
    root.clone()
        .with_external_admission()
        .with_admission_id("queued-invocation".into())
        .enqueue_text("retry must not change persisted input")
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let call_fn: crate::agent_loop::AgentCallFn = {
        let calls = calls.clone();
        Arc::new(move |_input, _config, _abort| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(("must not run".into(), None, Vec::new(), Default::default())) })
        })
    };
    let daemon = spawn_metis_worker_with_call_fn(&url, call_fn);
    let log = NatsSessionLog::new(root.jetstream().clone(), root.storage_key());
    let entries = wait_for_cancel(&log).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!entries.iter().any(|(_, entry)| matches!(
        entry,
        SessionLogEntry::Error { .. } | SessionLogEntry::TurnEnd { .. }
    )));
    let saved = root
        .metadata_store()
        .get_invocation_limits(root.storage_key(), "queued-invocation")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.admitted_at, original_time);
    assert_eq!(
        saved.deadline,
        Some(original_time + chrono::Duration::seconds(1))
    );
    assert!(entries.iter().any(|(_, entry)| matches!(entry, SessionLogEntry::Message { content, role, .. } if role.is_user() && content.to_text() == "original queued work")));
    daemon.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_worker_replay_recovers_completed_child_without_live_tool_server_or_redispatch() {
    use harnx_toolset::CheckpointStore;
    let _env_guard = env_lock().await;
    let Some((url, _nats, _dir)) = spawn_test_nats().await else {
        return;
    };
    let seeded = seed_remote_config(&url);
    let _env = subagent_test_env(&url, &seeded);
    let root = session(&url, "expired-saved-child").await;
    let original_time = chrono::Utc::now() - chrono::Duration::seconds(30);
    let mut intent = crate::nats_session_metadata::InvocationAdmission::new(
        &crate::nats_session_metadata::AdmissionAuthority::External {
            admitted_at: original_time,
        },
        "parent-replay".into(),
        Some(1),
        None,
    );
    intent.prompt_content = Some(harnx_core::message::MessageContent::Text(
        "saved delegation".into(),
    ));
    root.metadata_store()
        .reserve_admission(root.storage_key(), &intent, &[])
        .await
        .unwrap();
    root.clone()
        .with_external_admission()
        .with_admission_id("parent-replay".into())
        .enqueue_text("saved delegation")
        .await
        .unwrap();
    let parent = crate::nats_session_metadata::RunLimitsRecord::admit_root(
        intent.run_id.clone(),
        intent.invocation_id.clone(),
        original_time,
        Default::default(),
        None,
        crate::nats_session_metadata::CallTimeoutOverride::from_optional(Some(1)),
    )
    .unwrap();
    root.metadata_store()
        .put_run_limits(root.storage_key(), &parent)
        .await
        .unwrap();
    root.metadata_store()
        .put_invocation_limits(root.storage_key(), &parent)
        .await
        .unwrap();
    let child = session(&url, "completed-before-replay")
        .await
        .with_inherited_admission(
            parent.clone(),
            "original-child-wire".into(),
            original_time,
            crate::nats_session_metadata::InvocationEdgeKind::Delegation,
            None,
        );
    let child_prompt = child.enqueue_text("original child work").await.unwrap();
    let child_log = NatsSessionLog::new(child.jetstream().clone(), child.storage_key());
    child_log
        .append_event_async(&SessionLogEntry::Message {
            id: None,
            role: harnx_core::message::MessageRole::Assistant,
            content: harnx_core::message::MessageContent::Text(
                "saved successful child result".into(),
            ),
            timestamp: None,
            fence_token: Some(1),
        })
        .await
        .unwrap();
    child_log
        .append_event_async(&SessionLogEntry::TurnEnd {
            through_seq: child_prompt.user_msg_seq(),
            fence_token: 1,
            usage: None,
            timestamp: None,
        })
        .await
        .unwrap();
    let parent_log = NatsSessionLog::new(root.jetstream().clone(), root.storage_key());
    let round = parent_log
        .append_event_async(&SessionLogEntry::ToolCalls {
            text: String::new(),
            thought: None,
            calls: vec![ToolCall::new(
                "retired_session_prompt".into(),
                json!({"message": "original child work"}),
                Some("completed-call".into()),
                None,
            )],
            timestamp: None,
            fence_token: Some(1),
        })
        .await
        .unwrap();
    let request = harnx_toolset::ToolRequest {
        run_context: Some(harnx_toolset::AutonomousRunContext {
            snapshot: serde_json::to_value(&parent).unwrap(),
            started_at_ms: original_time.timestamp_millis().try_into().unwrap(),
        }),
        replay: None,
        operation_id: "original-child-wire".into(),
        call_id: "original-child-wire".into(),
        tool: "session_prompt".into(),
        args: json!({"message": "original child work"}),
        parent_session_id: Some(root.storage_key().into()),
        parent_agent: Some("metis".into()),
        parent_local_session_id: Some(root.session_id().into()),
        tool_call_id: Some("completed-call".into()),
        capabilities: Default::default(),
    };
    let journal =
        harnx_toolset_server::invocation_journal::InvocationJournal::ensure(root.jetstream(), 1)
            .await
            .unwrap();
    journal
        .record(
            &request,
            ("retired_session_prompt", "retired-scope", "retired-server"),
            round,
        )
        .await
        .unwrap();
    harnx_toolset_server::invocation_journal::JournalCheckpointStore { journal: journal.clone(), session: root.storage_key().into(), call_id: request.call_id.clone() }.checkpoint(json!({"session_id": child.session_id(), "storage_key": child.storage_key(), "cluster": "local"})).await.unwrap();
    assert!(journal.completed_reply(&request).await.unwrap().is_none());
    let calls = Arc::new(AtomicUsize::new(0));
    let call_fn: crate::agent_loop::AgentCallFn = {
        let calls = calls.clone();
        Arc::new(move |_input, _config, _abort| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok((
                    "must not dispatch".into(),
                    None,
                    Vec::new(),
                    Default::default(),
                ))
            })
        })
    };
    let daemon = spawn_metis_worker_with_call_fn(&url, call_fn);
    let entries = wait_for_cancel(&parent_log).await;
    let recovered = entries
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => results
                .iter()
                .find(|result| result.id.as_deref() == Some("completed-call")),
            _ => None,
        })
        .expect("saved child output recovered before Cancel");
    assert_eq!(
        recovered.output["response"],
        "saved successful child result"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(journal.completed_reply(&request).await.unwrap().is_some());
    assert!(!child_log
        .load_events_latest_async()
        .await
        .unwrap()
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::Cancel { .. })));
    daemon.abort();
}
