use super::*;
use crate::test_support::TestConfigSandbox;
use harnx_core::session::{Session, SessionLogEntry};

fn loaded_with_projection_error(
    control: SessionLogEntry,
    worker_active: bool,
) -> crate::LoadedNatsSession {
    let mut entries = crate::transcript_snapshot::tests::fixture(3);
    // Invalid raw sequence fails D1 validation before projection, not control replay.
    entries[0].0 = 0;
    entries.push((4, control));
    plain_session(
        Session {
            tokens: 123,
            replay_warnings: vec!["existing replay warning".into()],
            ..Default::default()
        },
        entries,
        worker_active,
    )
}

fn plain_session(
    session: Session,
    entries: Vec<(u64, SessionLogEntry)>,
    worker_active: bool,
) -> crate::LoadedNatsSession {
    crate::LoadedNatsSession {
        pending_prompt_seq: harnx_core::session_reconstruct::pending_prompt_seq(&entries),
        session,
        entries,
        base_session: Session {
            agent_name: Some("plain".into()),
            ..Default::default()
        },
        worker_active,
        pending_prompt_admitted_at: None,
    }
}

fn actor(config: Config) -> SessionActor {
    make_session_actor(
        SessionKey::local("plain", "projection-failure"),
        Default::default(),
        Duration::from_secs(30),
        SessionActorConfig {
            base_config: config,
            call_fn: None,
            local_worker: Arc::new(Mutex::new(None)),
        },
    )
    .0
}

fn actor_after_load(loaded: crate::LoadedNatsSession) -> SessionActor {
    let sandbox = TestConfigSandbox::new();
    let mut actor = actor(sandbox.config());
    actor.apply_history_snapshot(LoadedHistorySnapshot::from_loaded(loaded));
    actor
}

fn assert_presentation_warning(actor: &SessionActor) {
    assert!(actor.history_snapshot.is_empty() && actor.compaction.is_none());
    assert_eq!(actor.history_warnings[0], "existing replay warning");
    assert!(actor.history_warnings[1].starts_with("Failed to build session transcript:"));
}

fn assert_preserved_fields(actor: &SessionActor, entries: &[(u64, SessionLogEntry)]) {
    assert_eq!(actor.log_entries.as_deref(), Some(entries));
    assert_eq!(actor.tokens_usage.as_ref().unwrap().context_tokens, 123);
    assert_eq!(
        actor.session_base.as_ref().unwrap().agent_name.as_deref(),
        Some("plain")
    );
}

#[tokio::test]
async fn projection_failure_preserves_pending_approval_and_worker_lease() {
    let loaded = loaded_with_projection_error(
        SessionLogEntry::HitlApprovalRequested {
            tool_call_id: "approval".into(),
            summary: "confirm".into(),
            fence_token: 1,
        },
        true,
    );
    let entries = loaded.entries.clone();
    let actor = actor_after_load(loaded);
    assert!(matches!(actor.state, SessionState::AwaitingApproval { .. }));
    assert!(actor.worker_active);
    assert_preserved_fields(&actor, &entries);
    assert_presentation_warning(&actor);
}

#[tokio::test]
async fn projection_failure_preserves_durable_cancel() {
    let loaded = loaded_with_projection_error(
        SessionLogEntry::cancel_request("stop".into(), "test".into()),
        false,
    );
    let entries = loaded.entries.clone();
    let actor = actor_after_load(loaded);
    assert_eq!(actor.state, SessionState::Interrupted { cancel_seq: 4 });
    assert!(!actor.worker_active);
    assert_preserved_fields(&actor, &entries);
    assert_presentation_warning(&actor);
}

#[tokio::test]
async fn session_get_reports_how_the_last_turn_ended() {
    // The fixture's first turn completed at seq 3; the next prompt's turn fails.
    let mut entries = crate::transcript_snapshot::tests::fixture(3);
    entries.push((
        4,
        SessionLogEntry::Message {
            id: Some("second-prompt".into()),
            role: harnx_core::message::MessageRole::User,
            content: harnx_core::message::MessageContent::Text("Try again.".into()),
            timestamp: None,
            fence_token: None,
        },
    ));
    entries.push((
        5,
        SessionLogEntry::Error {
            message: "provider returned 500".into(),
            fence_token: 1,
            timestamp: None,
        },
    ));
    let actor = actor_after_load(plain_session(Session::default(), entries, false));
    // An idle state alone can't tell this turn from one that finished.
    assert_eq!(actor.state, SessionState::Idle);
    let result = crate::ag_ui_rpc::session_get_result(&actor.session_info(), "nats");
    assert_eq!(
        result["last_turn"],
        serde_json::json!({ "seq": 5, "outcome": "error", "message": "provider returned 500" })
    );
}

/// A durable read whose prompt has been waiting well past every worker
/// deadline, with no lease to cover it.
fn stalled_session(
    entries: Vec<(u64, SessionLogEntry)>,
    admitted_at: chrono::DateTime<chrono::Utc>,
) -> crate::LoadedNatsSession {
    crate::LoadedNatsSession {
        pending_prompt_admitted_at: Some(admitted_at),
        ..plain_session(Session::default(), entries, false)
    }
}

fn user_prompt_entry(timestamp: Option<chrono::DateTime<chrono::Utc>>) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some("never-answered".into()),
        role: harnx_core::message::MessageRole::User,
        content: harnx_core::message::MessageContent::Text("wait for a worker".into()),
        timestamp,
        fence_token: None,
    }
}

fn worker_entry(timestamp: Option<chrono::DateTime<chrono::Utc>>) -> SessionLogEntry {
    SessionLogEntry::ToolCalls {
        text: "half a turn".into(),
        thought: None,
        calls: vec![],
        timestamp,
        fence_token: Some(7),
    }
}

#[tokio::test]
async fn unclaimed_prompt_past_acquisition_timeout_reports_stalled() {
    // Admitted far before the default 60s first-claim budget, never claimed.
    let admitted_at = chrono::Utc::now() - chrono::Duration::hours(2);
    let actor = actor_after_load(stalled_session(
        vec![(1, user_prompt_entry(None))],
        admitted_at,
    ));
    assert_eq!(
        actor.state,
        SessionState::Stalled {
            prompt_seq: 1,
            reason: StallReason::Unclaimed
        }
    );
    let result = crate::ag_ui_rpc::session_get_result(&actor.session_info(), "nats");
    assert_eq!(
        result["state"],
        serde_json::json!({ "status": "stalled", "reason": "unclaimed", "prompt_seq": 1 })
    );
    // Recovery stays open: the stalled turn is not a running one, but new
    // input and cancellation both remain admissible.
    assert_eq!(result["canPrompt"], true);
    assert_eq!(result["canCancel"], true);
}

#[tokio::test]
async fn lease_over_a_pending_prompt_keeps_it_running() {
    let admitted_at = chrono::Utc::now() - chrono::Duration::hours(2);
    let mut loaded = stalled_session(vec![(1, user_prompt_entry(None))], admitted_at);
    loaded.worker_active = true;
    let actor = actor_after_load(loaded);
    assert_eq!(actor.state, SessionState::Pending);
}

#[tokio::test]
async fn lost_worker_past_grace_reports_stalled() {
    let written_at = chrono::Utc::now() - chrono::Duration::hours(2);
    let actor = actor_after_load(stalled_session(
        vec![
            (1, user_prompt_entry(None)),
            (2, worker_entry(Some(written_at))),
        ],
        written_at,
    ));
    assert_eq!(
        actor.state,
        SessionState::Stalled {
            prompt_seq: 1,
            reason: StallReason::WorkerLost
        }
    );
}

#[tokio::test]
async fn recent_worker_write_keeps_the_turn_running() {
    let now = chrono::Utc::now();
    let actor = actor_after_load(stalled_session(
        vec![(1, user_prompt_entry(None)), (2, worker_entry(Some(now)))],
        now,
    ));
    assert_eq!(actor.state, SessionState::Pending);
}

#[tokio::test]
async fn approval_gate_is_not_a_stall() {
    // A turn parked at an approval gate waits on a human decision, not a
    // worker; the gate outranks any stall refinement.
    let admitted_at = chrono::Utc::now() - chrono::Duration::hours(2);
    let actor = actor_after_load(stalled_session(
        vec![
            (1, user_prompt_entry(None)),
            (
                2,
                SessionLogEntry::HitlApprovalRequested {
                    tool_call_id: "approval".into(),
                    summary: "confirm".into(),
                    fence_token: 7,
                },
            ),
        ],
        admitted_at,
    ));
    assert!(matches!(actor.state, SessionState::AwaitingApproval { .. }));
}

#[test]
fn failed_follow_keeps_derived_state_and_broadcasts_run_error() {
    // The replica whose follow failed must report the stalled turn like any
    // other replica would, instead of caching idle (issue #2625).
    let admitted_at = chrono::Utc::now() - chrono::Duration::hours(2);
    let mut actor = actor_after_load(stalled_session(
        vec![(1, user_prompt_entry(None))],
        admitted_at,
    ));
    assert_eq!(
        actor.state,
        SessionState::Stalled {
            prompt_seq: 1,
            reason: StallReason::Unclaimed
        }
    );
    let mut events = actor.broadcast_tx.subscribe();
    let config = actor.actor_config.base_config.clone();
    let done = RunFinished {
        handoff: handoff::HandoffPreparation::new(config, Default::default()),
        limits: None,
        run_id: RunId::random(),
        result: Err(anyhow::anyhow!("worker stopped without answering")),
        sink: Arc::new(BroadcastEventSender::new(
            actor.broadcast_tx.clone(),
            MessageId::random(),
            SessionEventContext::new(
                Arc::new(ConfigLock::new(actor.actor_config.base_config.clone())),
                Vec::new(),
            ),
        )),
        thread_id: derive_thread_id(&actor.key.session),
        attachment_refs: Vec::new(),
    };
    actor.fail_run(&done, &anyhow::anyhow!("worker stopped without answering"));
    assert_eq!(
        actor.state,
        SessionState::Stalled {
            prompt_seq: 1,
            reason: StallReason::Unclaimed
        }
    );
    let mut saw_run_error = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, Event::RunError(_)) {
            saw_run_error = true;
        }
    }
    assert!(saw_run_error, "failed run must still broadcast RUN_ERROR");
}
