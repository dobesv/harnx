use super::live_harness::{LiveHarness, CLUSTER, WAIT};
use super::{InteractionKey, ModalState};
use crate::test_utils::{TestEnvironment, ENV_LOCK};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::session::SessionLogEntry;
use std::sync::atomic::Ordering;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attached_tui_approves_and_worker_executes_once() {
    answer_and_check(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_prompt_tui_denies_with_note_without_executing_tool() {
    answer_and_check(false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_live_interactions_follow_manifest_one_at_a_time() {
    harnx_core::require_nextest();
    let root = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(root.path());
    let mut harness = LiveHarness::start_calls(root.path(), 2).await;
    harness.activate().await;
    let first = harness.wait_for_modal().await.pending;
    let entries = harness.log.load_events_async().await.unwrap();
    let pending = harnx_runtime::interactions::derive_pending_interactions(&entries).unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(first.key.id, pending[0].interaction_id);
    assert_eq!(first.pending_count, 1);
    harness.tui.submit_interaction(true);
    harness.wait_for_decision().await;
    let second = harness.wait_for_modal().await.pending;
    assert_eq!(second.key.id, pending[1].interaction_id);
    assert_eq!(second.key.request_seq, pending[1].request_seq);
    assert_eq!(second.pending_count, 0);
    assert_ne!(first.key, second.key);
    harness.tui.submit_interaction(false);
    harness.wait_for_decision().await;
    let entries = harness.completed_entries().await;
    let decisions = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::InteractionResponse {
                interaction_id,
                response,
                ..
            } => Some((interaction_id, response.action.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        decisions,
        [(&first.key.id, "approve"), (&second.key.id, "deny")]
    );
    assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daedalus_handoff_tui_approves_and_worker_commits_handoff() {
    daedalus_handoff_and_check(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daedalus_handoff_tui_denies_and_worker_returns_denial() {
    daedalus_handoff_and_check(false).await;
}

async fn answer_and_check(approve: bool, active_prompt: bool) {
    harnx_core::require_nextest();
    let root = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(root.path());
    let mut harness = LiveHarness::start(root.path()).await;
    if active_prompt {
        harness.tui.current_prompt_abort = Some(harnx_runtime::utils::create_abort_signal());
        harness.tui.app.llm_busy = true;
        harness.tui.sync_session_activity_monitor();
        assert!(
            harness.tui.session_activity_handle.is_none(),
            "local prompts don't use the transcript follower"
        );
    }
    harness.activate().await;
    let state = harness.wait_for_modal().await;
    assert_eq!(state.pending.message, "Run counted tool?");
    assert_eq!(harness.calls.load(Ordering::SeqCst), 0);
    let entries = harness.log.load_events_async().await.unwrap();
    assert!(entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::SuspensionReady { .. })));
    submit_modal_decision(&mut harness, approve).await;
    let entries = harness.completed_entries().await;
    assert_eq!(
        harness.calls.load(Ordering::SeqCst),
        usize::from(approve),
        "history: {entries:#?}"
    );
    assert_response(&entries, &state.pending.key, approve);
}

async fn daedalus_handoff_and_check(approve: bool) {
    harnx_core::require_nextest();
    let root = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(root.path());
    let mut harness = LiveHarness::start_daedalus(root.path()).await;
    harness.activate().await;
    let state = harness.wait_for_modal().await;
    assert_eq!(
        state.pending.message,
        "Hand off this plan to Atlas for execution?"
    );
    let entries = harness.log.load_events_async().await.unwrap();
    assert!(entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::SuspensionReady { .. })));
    assert!(!entries
        .iter()
        .any(|(_, entry)| matches!(entry, SessionLogEntry::HandoffCommitted { .. })));
    submit_modal_decision(&mut harness, approve).await;
    let entries = harness.completed_entries().await;

    assert_response(&entries, &state.pending.key, approve);

    let results = entries
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => Some(results),
            _ => None,
        })
        .expect("worker commits final result");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "atlas_session_handoff");

    let expected_agent = format!("atlas@{CLUSTER}");
    let handoffs = entries
        .iter()
        .filter(|(_, entry)| match entry {
            SessionLogEntry::HandoffCommitted { target_agent, .. } => {
                target_agent == &expected_agent
            }
            _ => false,
        })
        .count();

    if approve {
        assert_eq!(
            handoffs, 1,
            "handoff must be committed on approve: {entries:#?}"
        );
        assert_ne!(
            results[0]
                .output
                .get("is_error")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    } else {
        assert_eq!(handoffs, 0, "no handoff committed on deny");
        assert_eq!(
            results[0]
                .output
                .get("is_error")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            results[0].output["error"],
            "Atlas handoff denied by the user."
        );
    }
}

fn assert_response(entries: &[(u64, SessionLogEntry)], key: &InteractionKey, approve: bool) {
    let responses = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::InteractionResponse {
                interaction_id,
                request_seq,
                response,
                ..
            } => Some((interaction_id, request_seq, response)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 1);
    let (id, seq, response) = responses[0];
    assert_eq!(id, &key.id);
    assert_eq!(*seq, key.request_seq);
    assert_eq!(response.action, if approve { "approve" } else { "deny" });
    assert_eq!(response.note.as_deref(), Some("Operator note"));
    let user_text = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::Message { role, content, .. } if role.is_user() => {
                Some(content.to_text())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        user_text
            .iter()
            .filter(|text| text.as_str() == "Run counted tool")
            .count(),
        1
    );
    assert!(
        !user_text.iter().any(|text| text.contains("Operator note")),
        "note must not become another user message"
    );
    let results = entries
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::ToolResults { results, .. } => Some(results),
            _ => None,
        })
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0]
            .output
            .get("is_error")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        !approve
    );
}

async fn submit_modal_decision(harness: &mut LiveHarness, approve: bool) {
    if let Some(ModalState::ConfirmInteraction(state)) = harness.tui.app.modal.as_mut() {
        state.note.insert_str("Operator note");
        state.last_key_at -= WAIT;
    }
    let key = if approve {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    } else {
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
    };
    harness.tui.handle_modal_key(key).await.unwrap();
    harness.wait_for_decision().await;
}
