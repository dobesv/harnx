//! Exercise the parent-row route against a suspended child and a real worker.
use super::*;
use crate::interaction_confirmation::live_harness::{LiveHarness, CLUSTER, WAIT};
use crate::test_utils::{TestEnvironment, ENV_LOCK};
use crate::types::{MonitoredSessionKey, TuiEvent};
use harnx_core::event::{SubAgentProgress, SubAgentProgressStatus};
use harnx_core::session::SessionLogEntry;
use std::sync::{atomic::Ordering, Arc};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_row_approves_exact_child_confirm_and_worker_executes_once() {
    answer(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_row_denies_exact_child_confirm_without_executing_tool() {
    answer(false).await;
}

async fn answer(approved: bool) {
    harnx_core::require_nextest();
    let root = tempfile::tempdir().unwrap();
    let _lock = ENV_LOCK.lock().await;
    let _env = TestEnvironment::set(root.path());
    let mut harness = LiveHarness::start(root.path()).await;
    harness.activate().await;
    let child = harness.wait_for_modal().await.pending;
    let entries = harness.log.load_events_async().await.unwrap();
    let summary = harnx_runtime::interactions::derive_pending_interaction_summary(
        &entries,
        "confirmation-agent",
        CLUSTER,
        "confirmation-session",
        "child-invocation",
    )
    .unwrap()
    .unwrap();
    let mut config = harness.tui.config.read().clone();
    config.session =
        Some(harnx_runtime::config::session::new(&config, "parent-session", None).unwrap());
    let parent_key = config.session.as_ref().unwrap().storage_key();
    let config = Arc::new(harnx_runtime::config::ConfigLock::new(config));
    harness.tui = Tui::init(&config).await.unwrap();
    harness.tui.app.modal = None;
    harness.tui.app.llm_busy = true;
    harness.tui.app.transcript = vec![row(summary)];
    harness.tui.app.transcript_focus = Some(0);
    assert!(harness.tui.answer_selected_child());
    harness.tui.submit_interaction(approved);
    tokio::time::timeout(WAIT, async {
        loop {
            let event = harness.tui.event_rx.recv().await.unwrap();
            let decided = matches!(
                &event,
                TuiEvent::Interaction(
                    crate::interaction_confirmation::InteractionEvent::ChildDecided { .. }
                )
            );
            harness.tui.handle_tui_event_inner(event).await.unwrap();
            if decided {
                break;
            }
        }
    })
    .await
    .expect("child decision reaches parent TUI");
    assert!(harness.tui.app.modal.is_none());
    assert!(
        harness.tui.app.llm_busy,
        "answering the child doesn't finish the parent"
    );
    let entries = harness.completed_entries().await;
    let responses = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::InteractionResponse {
                interaction_id,
                request_seq,
                response,
                ..
            } => Some((interaction_id, request_seq, response.action.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        responses,
        [(
            &child.key.id,
            &child.key.request_seq,
            if approved { "approve" } else { "deny" }
        )]
    );
    assert_eq!(harness.calls.load(Ordering::SeqCst), usize::from(approved));
    let snapshot = config.read().clone();
    let js = snapshot.nats_jetstream(CLUSTER).await.unwrap();
    let parent =
        harnx_runtime::nats_session_log::NatsSessionLog::new_with_replicas(js, &parent_key, 1);
    assert!(
        parent.load_events_async().await.unwrap().is_empty(),
        "no decision or prompt was written to parent"
    );
}

fn row(summary: PendingInteractionSummary) -> TranscriptItem {
    TranscriptItem::SubAgentSession {
        key: MonitoredSessionKey {
            agent: summary.agent.clone(),
            session_id: summary.session_id.clone(),
            cluster: summary.cluster.clone(),
        },
        invocation_id: Some("parent-call".into()),
        status: SubAgentStatus::Running,
        progress: Some(SubAgentInvocationProgress::new(SubAgentProgress {
            invocation_id: "parent-call".into(),
            agent: summary.agent.clone(),
            session_id: summary.session_id.clone(),
            status: SubAgentProgressStatus::Running,
            pending_interaction: Some(summary),
            elapsed_ms: 0,
            usage: Default::default(),
            tool_call_count: 0,
            title: None,
            tool_call_id: None,
        })),
    }
}
