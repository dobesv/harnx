use super::*;
use crate::test_utils::{TestEnvironment, ENV_LOCK};
use crate::types::{MonitoredSessionKey, SubAgentStatus, TranscriptItem};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_runtime::config::Config;
use harnx_runtime::nats_event_sink::{events_subject, AdvisoryEnvelope};
use harnx_runtime::nats_session_log::NatsSessionLog;

#[tokio::test]
async fn nats_durable_refresh_preserves_queued_handoff() {
    let _lock = ENV_LOCK.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let _environment = TestEnvironment::set(temp.path());
    let config = Config::default();
    let client = config.nats_client(LOCAL_CLUSTER_KEY).await.unwrap();
    let jetstream = async_nats::jetstream::new(client.clone());
    let target = ("source-session".to_string(), LOCAL_CLUSTER_KEY.to_string());
    let log = NatsSessionLog::new(jetstream.clone(), &target.0);
    let user_seq = log
        .append_event_async(&SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("delegate".into()),
            timestamp: None,
            fence_token: None,
        })
        .await
        .unwrap();
    let mut stream = SessionEventStream::attach(jetstream, client.clone(), &target.0)
        .await
        .unwrap();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let forwarder = SessionEventForwarder {
        event_tx: &event_tx,
        target: &target,
        attached_seq: stream.last_applied_seq(),
        attached_during_turn: true,
    };
    let handoff_seq = queue_completed_handoff(&log, &client, &target.0, user_seq).await;

    let mut active = true;
    let mut refresh = tokio::time::interval(Duration::from_millis(1));
    refresh.tick().await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(matches!(
        next_session_activity_input(stream.next(), active, &mut refresh).await,
        SessionActivityInput::RefreshDurableHistory
    ));
    assert!(matches!(
        refresh_durable_activity(&mut stream, &forwarder, &mut active).await,
        DurableRefreshOutcome::Continue
    ));
    assert!(stream.last_applied_seq() > handoff_seq);
    assert!(matches!(
        event_rx.try_recv(),
        Ok(TuiEvent::SessionActivity { active: false, .. })
    ));
    let buffered = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert!(forward_advisory(&forwarder, buffered, &mut active));
    assert!(matches!(
        event_rx.try_recv(),
        Ok(TuiEvent::SessionAgent {
            event: AgentEvent::Session(SessionEvent::HandoffCommitted { session_id, .. }),
            ..
        }) if session_id == "target-session"
    ));
    assert!(!active, "a handoff must not reactivate its source session");
}

#[tokio::test]
async fn durable_child_result_repairs_attached_row_while_parent_remains_busy() {
    assert_child_result_recovery(false).await;
}

#[tokio::test]
async fn compacted_child_result_repairs_open_view_while_parent_remains_busy() {
    assert_child_result_recovery(true).await;
}

async fn assert_child_result_recovery(compacted: bool) {
    let mut harness = crate::test_utils::TuiTestHarness::new().await;
    let tui = harness.tui();
    tui.clear_transcript();
    let target = ("parent".to_string(), tui.current_session_cluster());
    tui.session_activity_target = Some(target.clone());
    tui.app.llm_busy = true;
    let progress = completed_child_progress();
    let key = MonitoredSessionKey {
        cluster: target.1.clone(),
        agent: progress.agent.clone(),
        session_id: progress.session_id.clone(),
    };
    tui.record_subagent_started(None, key, Some(progress.invocation_id.clone()));
    tui.app.transcript_focus = Some(0);
    assert!(tui.open_focused_root_subagent());
    let mut history = child_result_history(&progress);
    if compacted {
        history.push((
            4,
            SessionLogEntry::Compress {
                prompt: "summary".into(),
            },
        ));
    }
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let forwarder = SessionEventForwarder {
        event_tx: &event_tx,
        target: &target,
        attached_seq: 1,
        attached_during_turn: true,
    };
    assert!(forwarder.recover_subagent_progress(&history, 1));
    tui.handle_tui_event(event_rx.try_recv().unwrap())
        .await
        .unwrap();
    assert!(
        event_rx.try_recv().is_err(),
        "recovery must not change root activity"
    );
    assert!(forwarder.recover_subagent_progress(&history, 3));
    assert!(
        event_rx.try_recv().is_err(),
        "applied results must not be replayed"
    );
    // Reattaching starts with a new cursor and must remain idempotent.
    assert!(forwarder.recover_subagent_progress(&history, 0));
    tui.handle_tui_event(event_rx.try_recv().unwrap())
        .await
        .unwrap();
    assert!(tui.app.llm_busy);
    assert_completed_child(tui, &progress);
}

fn assert_completed_child(tui: &Tui, progress: &harnx_core::event::SubAgentProgress) {
    let [TranscriptItem::SubAgentSession {
        status: SubAgentStatus::Completed,
        progress: Some(actual),
        ..
    }] = tui.app.transcript.as_slice()
    else {
        panic!("expected a completed child row without replayed output");
    };
    assert_eq!(&actual.snapshot, progress);
    assert_eq!(actual.elapsed_ms(), progress.elapsed_ms);
    assert_eq!(
        tui.app.subagent_view_stack[0].status,
        SubAgentStatus::Completed
    );
    assert_eq!(
        tui.app.subagent_view_stack[0]
            .progress
            .as_ref()
            .unwrap()
            .snapshot,
        *progress
    );
}

fn completed_child_progress() -> harnx_core::event::SubAgentProgress {
    harnx_core::event::SubAgentProgress {
        invocation_id: "invocation".into(),
        agent: "researcher".into(),
        session_id: "child".into(),
        status: harnx_core::event::SubAgentProgressStatus::Done,
        elapsed_ms: 375_177,
        usage: harnx_core::api_types::CompletionTokenUsage::new(Some(1200), Some(345), Some(67)),
        tool_call_count: 56,
    }
}

fn child_result_history(
    progress: &harnx_core::event::SubAgentProgress,
) -> Vec<(u64, SessionLogEntry)> {
    vec![(
        3,
        SessionLogEntry::ToolResults {
            results: vec![harnx_core::session::ToolOutput {
                id: Some("call".into()),
                name: "session_prompt".into(),
                output: serde_json::json!({"sub_agent_progress": progress}),
                markdown: None,
                content: vec![],
                switch_agent: None,
            }],
            timestamp: None,
        },
    )]
}

async fn queue_completed_handoff(
    log: &NatsSessionLog,
    client: &async_nats::Client,
    session_id: &str,
    user_seq: u64,
) -> u64 {
    let handoff_seq = log
        .append_event_async(&SessionLogEntry::HandoffCommitted {
            target_agent: "target".into(),
            target_session_id: "target-session".into(),
            handoff_tool_call_id: None,
        })
        .await
        .unwrap();
    let handoff = AdvisoryEnvelope::new(
        handoff_seq,
        AgentEvent::Session(SessionEvent::HandoffCommitted {
            agent: "target".into(),
            session_id: "target-session".into(),
            handoff_tool_call_id: None,
            after_seq: Some(handoff_seq),
        }),
    );
    client
        .publish(
            events_subject(session_id),
            handoff.to_bytes().unwrap().into(),
        )
        .await
        .unwrap();
    client.flush().await.unwrap();
    log.append_event_async(&SessionLogEntry::TurnEnd {
        through_seq: user_seq,
        fence_token: 1,
        timestamp: None,
        usage: None,
    })
    .await
    .unwrap();

    handoff_seq
}
