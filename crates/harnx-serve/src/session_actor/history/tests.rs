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
    crate::LoadedNatsSession {
        session: Session {
            tokens: 123,
            replay_warnings: vec!["existing replay warning".into()],
            ..Default::default()
        },
        entries,
        base_session: Session {
            agent_name: Some("plain".into()),
            ..Default::default()
        },
        worker_active,
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
    let sandbox = TestConfigSandbox::new();
    let loaded = loaded_with_projection_error(
        SessionLogEntry::HitlApprovalRequested {
            tool_call_id: "approval".into(),
            summary: "confirm".into(),
            fence_token: 1,
        },
        true,
    );
    let entries = loaded.entries.clone();
    let mut actor = actor(sandbox.config());
    actor.apply_history_snapshot(LoadedHistorySnapshot::from_loaded(loaded));
    assert!(matches!(actor.state, SessionState::AwaitingApproval { .. }));
    assert!(actor.worker_active);
    assert_preserved_fields(&actor, &entries);
    assert_presentation_warning(&actor);
}

#[tokio::test]
async fn projection_failure_preserves_durable_cancel() {
    let sandbox = TestConfigSandbox::new();
    let loaded = loaded_with_projection_error(
        SessionLogEntry::cancel_request("stop".into(), "test".into()),
        false,
    );
    let entries = loaded.entries.clone();
    let mut actor = actor(sandbox.config());
    actor.apply_history_snapshot(LoadedHistorySnapshot::from_loaded(loaded));
    assert_eq!(actor.state, SessionState::Interrupted { cancel_seq: 4 });
    assert!(!actor.worker_active);
    assert_preserved_fields(&actor, &entries);
    assert_presentation_warning(&actor);
}
