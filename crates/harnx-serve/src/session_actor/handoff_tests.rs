use super::super::{load_base_config_for_tests, make_session_actor, SessionActorConfig};
use super::*;
use crate::test_support::TestConfigSandbox;
use harnx_core::{session::SessionLogEntry, tool::ToolCall};
use harnx_runtime::AgentCallFn;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn handoff_ack_wait_is_bounded() {
    let (_reply_tx, reply_rx) = oneshot::channel();
    let target = SessionKey::local("atlas", "target-session");

    let error = wait_for_handoff_ack(reply_rx, &target, Duration::ZERO)
        .await
        .expect_err("an unresponsive target must time out");

    let error = error.to_string();
    assert!(error.contains("did not acknowledge the prompt"));
    assert!(error.contains("atlas/target-session"));
}

#[test]
fn display_ref_with_default_cluster_bare_for_default_cluster_target() {
    // When the target is on the default cluster, display_ref_with_default_cluster
    // returns bare agent name.
    let target = ResolvedAgentTarget::new("atlas", "mycluster");
    let display = target.display_ref_with_default_cluster(Some("mycluster"));
    assert_eq!(display, "atlas");

    // Non-default cluster keeps suffix
    let other = ResolvedAgentTarget::new("atlas", "other-cluster");
    let display = other.display_ref_with_default_cluster(Some("mycluster"));
    assert_eq!(display, "atlas@other-cluster");

    // No default cluster falls back to display_ref behavior
    let display = target.display_ref_with_default_cluster(None);
    assert_eq!(display, "atlas@mycluster");
}

fn insert_live_owner(
    registry: &SessionMap,
    target: ResolvedAgentTarget,
    id: &str,
) -> tokio::sync::mpsc::Receiver<SessionCommand> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    registry.insert(
        SessionKey::new(target, id),
        SessionHandle {
            tx,
            actor_id: u64::MAX,
        },
    );
    rx
}

#[tokio::test]
async fn requested_handoff_session_keeps_ownership_errors_and_ids() {
    let _sandbox = TestConfigSandbox::new();
    let registry = SessionMap::default();
    let preparation = HandoffPreparation::new(load_base_config_for_tests(), registry.clone());
    let target = ResolvedAgentTarget::local("target-agent");
    let scoped = harnx_session::fork_prompt_config(&preparation.config);
    let _owner = insert_live_owner(
        &registry,
        ResolvedAgentTarget::local("other-agent"),
        "shared-id",
    );
    let error = preparation
        .resolve_session_id(&target, &scoped, Some("shared-id".into()))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "handoff failed: session 'shared-id' belongs to agent 'other-agent', not 'target-agent'"
    );

    // Other clusters don't own this target's ID. Unknown IDs and whitespace in
    // nonblank IDs retain the caller's original value.
    let _remote_owner = insert_live_owner(
        &registry,
        ResolvedAgentTarget::new("other-agent", "remote"),
        "remote-id",
    );
    let _same_owner = insert_live_owner(&registry, target.clone(), "existing-id");
    for id in ["remote-id", "existing-id", "unknown-id", " spaced-id "] {
        let resolved = preparation
            .resolve_session_id(&target, &scoped, Some(id.into()))
            .await
            .unwrap();
        assert!(matches!(resolved, HandoffSessionId::Requested(_)));
        assert_eq!(resolved.as_str(), id);
    }
}

#[tokio::test]
async fn requested_handoff_session_checks_persisted_test_owner() {
    use harnx_runtime::config::session::SessionAppendSink;

    let _sandbox = TestConfigSandbox::new();
    let preparation = HandoffPreparation::new(load_base_config_for_tests(), SessionMap::default());
    let scoped = harnx_session::fork_prompt_config(&preparation.config);
    test_log::test_session_log(&SessionKey::local("old-agent", "saved-id"))
        .append(&SessionLogEntry::Clear)
        .unwrap();
    let error = preparation
        .resolve_session_id(
            &ResolvedAgentTarget::local("target-agent"),
            &scoped,
            Some("saved-id".into()),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "handoff failed: session 'saved-id' belongs to agent 'old-agent', not 'target-agent'"
    );
}

fn handoff_call_fn(requested: Option<&'static str>, source_calls: Arc<AtomicUsize>) -> AgentCallFn {
    Arc::new(move |_input, config, _abort| {
        let source = config.read().agent.as_ref().unwrap().name() == "source-agent";
        let source_calls = source_calls.clone();
        Box::pin(async move {
            let calls = if source {
                source_calls.fetch_add(1, Ordering::SeqCst);
                vec![ToolCall::new(
                    "target-agent_session_handoff".into(),
                    json!({"prompt": "delegated work", "session_id": requested}),
                    Some("collision-call".into()),
                    None,
                )]
            } else {
                vec![]
            };
            Ok(("done".into(), None, calls, Default::default()))
        })
    })
}

#[tokio::test]
async fn reserved_handoff_session_bypasses_ownership_check_at_dispatch() {
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent_with_front_matter(
        "source-agent",
        "model: openai:gpt-4o\nuse_tools: target-agent_session_handoff",
        "Source.",
    );
    sandbox.write_agent("target-agent", "Target.");
    if !crate::test_support::ensure_test_nats().await {
        return;
    }
    let registry = SessionMap::default();
    let (mut actor, _handle) = make_session_actor(
        SessionKey::local("source-agent", "source-session"),
        registry.clone(),
        Duration::from_secs(60),
        SessionActorConfig {
            base_config: load_base_config_for_tests(),
            call_fn: Some(handoff_call_fn(None, Arc::new(AtomicUsize::new(0)))),
            local_worker: Arc::new(tokio::sync::Mutex::new(None)),
        },
    );
    let mut events = actor.broadcast_tx.subscribe();
    let reap_sleep = tokio::time::sleep(Duration::from_secs(60));
    tokio::pin!(reap_sleep);
    actor
        .start_run("handoff".into(), Default::default(), &mut reap_sleep)
        .await;
    let done = tokio::time::timeout(Duration::from_secs(10), actor.run_done_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let (id, prepared) = assert_reserved_handoff(&done);

    // Simulate a cross-agent collision after reservation. The same ID fails the
    // caller branch, but the prepared Reserved identity must still dispatch.
    let _owner = insert_live_owner(&registry, ResolvedAgentTarget::local("other-agent"), &id);
    assert_requested_collision(&done.handoff, &prepared.target, &id).await;
    actor.handle_run_done(done, &mut reap_sleep).await;

    assert_reserved_handoff_committed(&actor, &registry, &mut events, &id);
}

#[tokio::test]
async fn requested_handoff_ownership_failure_is_fatal_with_original_error() {
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent_with_front_matter(
        "source-agent",
        "model: openai:gpt-4o\nuse_tools: target-agent_session_handoff",
        "Source.",
    );
    sandbox.write_agent("target-agent", "Target.");
    let registry = SessionMap::default();
    let _owner = insert_live_owner(
        &registry,
        ResolvedAgentTarget::local("other-agent"),
        "taken-id",
    );
    let source_calls = Arc::new(AtomicUsize::new(0));
    let (mut actor, _handle) = make_session_actor(
        SessionKey::local("source-agent", "source-session"),
        registry.clone(),
        Duration::from_secs(60),
        SessionActorConfig {
            base_config: load_base_config_for_tests(),
            call_fn: Some(handoff_call_fn(Some("taken-id"), source_calls.clone())),
            local_worker: Arc::new(tokio::sync::Mutex::new(None)),
        },
    );
    let mut events = actor.broadcast_tx.subscribe();
    let reap_sleep = tokio::time::sleep(Duration::from_secs(60));
    tokio::pin!(reap_sleep);
    actor
        .start_run("handoff".into(), Default::default(), &mut reap_sleep)
        .await;
    let done = tokio::time::timeout(Duration::from_secs(10), actor.run_done_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        done.result.is_err(),
        "ownership failure must terminate the tool round"
    );
    assert!(done.handoff.targets.lock().unwrap().is_empty());
    actor.handle_run_done(done, &mut reap_sleep).await;
    assert_eq!(
        source_calls.load(Ordering::SeqCst),
        1,
        "no recovery model round"
    );
    let mut error = None;
    while let Ok(event) = events.try_recv() {
        match event {
            Event::RunError(event) => error = Some(event.message),
            Event::Custom(event) => assert_ne!(event.name, "session_handoff"),
            Event::RunFinished(_) => panic!("failed handoff must not finish successfully"),
            _ => {}
        }
    }
    assert_eq!(
        error.as_deref(),
        Some(
            "handoff failed: session 'taken-id' belongs to agent 'other-agent', not 'target-agent'"
        )
    );
    assert_eq!(actor.state, SessionState::Idle);
    assert!(!registry.contains_key(&SessionKey::local("target-agent", "taken-id")));
}

#[tokio::test]
async fn handoff_preparation_retains_requested_target_by_tool_call_id() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("target-agent", "Target.");
    let registry = SessionMap::default();
    let _owner = insert_live_owner(
        &registry,
        ResolvedAgentTarget::local("target-agent"),
        "existing-id",
    );
    let preparation = HandoffPreparation::new(load_base_config_for_tests(), registry);
    let callback = preparation.callback();
    assert!(preparation.targets.lock().unwrap().is_empty());
    let result = callback(
        "target-agent".into(),
        Some("existing-id".into()),
        "explicit-call".into(),
    )
    .await
    .unwrap();
    assert_eq!(result.agent, "target-agent");
    assert_eq!(result.session_id, "existing-id");
    assert!(preparation
        .take(Some("different-call"), "missing-agent", None)
        .await
        .is_err());
    let target = preparation
        .take(Some("explicit-call"), "missing-agent", None)
        .await
        .unwrap();
    assert_eq!(target.target, ResolvedAgentTarget::local("target-agent"));
    assert!(
        matches!(target.session_id, HandoffSessionId::Requested(ref id) if id == "existing-id")
    );
    assert!(
        preparation.targets.lock().unwrap().is_empty(),
        "prepared identities are consumed once"
    );
}

async fn run_provider_handoff(requested: Option<&str>, conflicting_owner: bool) {
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("source-agent", "Source.");
    sandbox.write_agent("target-agent", "Target.");
    if !crate::test_support::ensure_test_nats().await {
        return;
    }
    let registry = SessionMap::default();
    let _owner = conflicting_owner.then(|| {
        insert_live_owner(
            &registry,
            ResolvedAgentTarget::local("other-agent"),
            requested.unwrap(),
        )
    });
    let (mut actor, _handle) = make_session_actor(
        SessionKey::local("source-agent", "provider-source"),
        registry.clone(),
        Duration::from_secs(60),
        SessionActorConfig {
            base_config: load_base_config_for_tests(),
            call_fn: Some(Arc::new(|_, _, _| {
                Box::pin(async { Ok(("done".into(), None, vec![], Default::default())) })
            })),
            local_worker: Arc::new(tokio::sync::Mutex::new(None)),
        },
    );
    let mut events = actor.broadcast_tx.subscribe();
    let reap_sleep = tokio::time::sleep(Duration::from_secs(60));
    tokio::pin!(reap_sleep);
    actor
        .start_run("handoff".into(), Default::default(), &mut reap_sleep)
        .await;
    let mut done = tokio::time::timeout(Duration::from_secs(30), actor.run_done_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        done.result,
        Ok(harnx_runtime::LoopResult::Completed)
    ));
    assert!(done.handoff.targets.lock().unwrap().is_empty());

    done.result = Ok(provider_handoff_result(requested));
    actor.handle_run_done(done, &mut reap_sleep).await;

    assert_provider_dispatch(&mut events, &registry, requested, conflicting_owner);
}

fn provider_handoff_result(requested: Option<&str>) -> harnx_runtime::LoopResult {
    // Inject the loop result of a provider response, without synthetic preparation.
    let mut output = json!({
        "action": "switch_agent",
        "agent": "target-agent",
        "prompt": "delegated work",
        "tool_call_id": "provider-call",
    });
    if let Some(id) = requested {
        output["session_id"] = json!(id);
    }
    let switch: harnx_core::tool::SwitchAgentData = serde_json::from_value(output).unwrap();
    harnx_runtime::LoopResult::HandoffRequested {
        agent: switch.agent,
        session_id: switch.session_id,
        prompt: switch.prompt,
        tool_call_id: switch.tool_call_id,
    }
}

fn assert_provider_dispatch(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    registry: &SessionMap,
    requested: Option<&str>,
    conflicting_owner: bool,
) {
    let mut committed = None;
    let mut error = None;
    while let Ok(event) = events.try_recv() {
        match event {
            Event::Custom(event) if event.name == "session_handoff" => {
                committed = Some(event.value)
            }
            Event::RunError(event) => error = Some(event.message),
            _ => {}
        }
    }
    if conflicting_owner {
        assert!(committed.is_none());
        assert_eq!(
            error.unwrap(),
            format!(
                "handoff failed: session '{}' belongs to agent 'other-agent', not 'target-agent'",
                requested.unwrap(),
            )
        );
        return;
    }
    assert!(error.is_none(), "provider handoff failed: {error:?}");
    let committed = committed.expect("provider handoff must dispatch");
    let id = committed["session_id"].as_str().unwrap();
    assert!(!id.trim().is_empty());
    if let Some(requested) = requested.filter(|id| !id.trim().is_empty()) {
        assert_eq!(id, requested);
    }
    assert_eq!(committed["agent"], "target-agent");
    assert_eq!(committed["handoff_tool_call_id"], "provider-call");
    assert!(registry.contains_key(&SessionKey::local("target-agent", id)));
}

#[tokio::test]
async fn provider_handoff_without_session_id_reserves_and_dispatches() {
    run_provider_handoff(None, false).await;
}

#[tokio::test]
async fn provider_handoff_with_blank_session_id_reserves_and_dispatches() {
    run_provider_handoff(Some(" \t "), false).await;
}

#[tokio::test]
async fn provider_handoff_preserves_supplied_session_id() {
    run_provider_handoff(Some("provider-target"), false).await;
}

#[tokio::test]
async fn provider_handoff_rejects_supplied_session_owned_by_another_agent() {
    run_provider_handoff(Some("other-session"), true).await;
}

#[tokio::test]
async fn provider_handoff_fallback_preserves_requested_and_reserved_provenance() {
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("target-agent", "Target.");
    if !crate::test_support::ensure_test_nats().await {
        return;
    }
    let preparation = HandoffPreparation::new(load_base_config_for_tests(), SessionMap::default());
    let reserved = preparation.take(None, "target-agent", None).await.unwrap();
    assert!(matches!(reserved.session_id, HandoffSessionId::Reserved(_)));
    let requested = preparation
        .take(
            Some("provider-call"),
            "target-agent",
            Some("provider-target".into()),
        )
        .await
        .unwrap();
    assert!(
        matches!(requested.session_id, HandoffSessionId::Requested(ref id) if id == "provider-target")
    );
}

fn assert_reserved_handoff(done: &RunFinished) -> (String, PreparedTarget) {
    let Ok(harnx_runtime::LoopResult::HandoffRequested {
        session_id: Some(id),
        tool_call_id,
        ..
    }) = &done.result
    else {
        panic!(
            "expected prepared handoff, error: {:?}",
            done.result.as_ref().err()
        );
    };
    let id = id.clone();
    let prepared = done
        .handoff
        .targets
        .lock()
        .unwrap()
        .get(tool_call_id.as_deref().unwrap())
        .unwrap()
        .clone();
    assert!(matches!(prepared.session_id, HandoffSessionId::Reserved(_)));
    assert_eq!(prepared.target, ResolvedAgentTarget::local("target-agent"));
    (id, prepared)
}

async fn assert_requested_collision(
    handoff: &HandoffPreparation,
    target: &ResolvedAgentTarget,
    id: &str,
) {
    let (_, scoped) = crate::resolve_agent_target(&handoff.config, "target-agent")
        .await
        .unwrap();
    let error = handoff
        .resolve_session_id(target, &scoped, Some(id.to_string()))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "handoff failed: session '{id}' belongs to agent 'other-agent', not 'target-agent'"
        )
    );
}

fn assert_reserved_handoff_committed(
    actor: &SessionActor,
    registry: &SessionMap,
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    id: &str,
) {
    let mut committed = None;
    while let Ok(event) = events.try_recv() {
        match event {
            Event::RunError(error) => panic!("reserved handoff failed: {}", error.message),
            Event::Custom(event) if event.name == "session_handoff" => {
                committed = Some(event.value)
            }
            _ => {}
        }
    }
    let committed = committed.expect("reserved handoff committed despite colliding owner");
    assert_eq!(committed["session_id"], id);
    assert_eq!(committed["agent"], "target-agent");
    assert!(registry.contains_key(&SessionKey::local("target-agent", id)));
    let entries = test_log::test_session_log(&actor.key).entries();
    let output = entries
        .iter()
        .find_map(|entry| match entry {
            SessionLogEntry::ToolResults { results, .. } => results
                .iter()
                .find(|r| r.id.as_deref() == Some("collision-call")),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        output.output["handoff"]["session_id"],
        committed["session_id"]
    );
    assert_eq!(output.output["handoff"]["agent"], committed["agent"]);
}
