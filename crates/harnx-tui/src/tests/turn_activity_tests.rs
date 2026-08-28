use super::{line_to_plain, test_config};
use crate::types::{TranscriptItem, Tui, TuiEvent, SPINNER_FRAMES};
use harnx_core::event::{AgentEvent, AgentSource, ModelEvent, TurnEvent};

fn sub_agent_source() -> AgentSource {
    AgentSource {
        agent: "aristarchus".to_string(),
        session_id: Some("sub-session-1".to_string()),
        model: None,
    }
}

#[tokio::test]
async fn failed_pending_activations_for_two_targets_remain_retryable() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    let first = ("session-a".to_string(), "cluster-a".to_string());
    let second = ("session-b".to_string(), "cluster-b".to_string());

    tui.retain_pending_remote_activation(first.clone());
    tui.retain_pending_remote_activation(second.clone());

    let retryable = tui.take_pending_remote_activation_targets();
    assert_eq!(retryable.len(), 2);
    assert!(retryable.contains(&first));
    assert!(retryable.contains(&second));
}

/// A nested sub-agent error must not end the parent prompt while its
/// delegating tool call is still in flight.
#[tokio::test]
async fn sub_agent_error_does_not_clear_llm_busy() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.llm_busy = true;
    tui.queue_pending_message("follow up".to_string()).await;

    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::sub_agent(
        sub_agent_source(),
        AgentEvent::Model(ModelEvent::Error("sub-agent exploded".to_string())),
    )))
    .await
    .unwrap();

    assert!(tui.app.llm_busy);
    assert!(tui.app.pending_message.is_some());
    assert!(tui.shared_pending_message.lock().await.is_some());
    assert!(tui.app.transcript.iter().any(
        |entry| matches!(entry, TranscriptItem::ErrorText(text) if text == "sub-agent exploded")
    ));
}

#[tokio::test]
async fn sub_agent_final_does_not_clear_llm_busy() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.app.llm_busy = true;
    tui.queue_pending_message("follow up".to_string()).await;

    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::sub_agent(
        sub_agent_source(),
        AgentEvent::Model(ModelEvent::Final {
            output: "sub-agent final text".to_string(),
            usage: Default::default(),
        }),
    )))
    .await
    .unwrap();

    assert!(tui.app.llm_busy);
    assert!(tui.app.pending_message.is_some());
    assert!(tui.shared_pending_message.lock().await.is_some());
    assert!(tui.app.transcript.iter().any(
        |entry| matches!(entry, TranscriptItem::AssistantText { text, .. } if text == "sub-agent final text")
    ));
}

#[tokio::test]
async fn final_waits_for_turn_end_before_clearing_busy() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();

    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(TurnEvent::Started)))
        .await
        .unwrap();

    assert!(tui.app.llm_busy);
    assert!(SPINNER_FRAMES
        .iter()
        .any(|frame| line_to_plain(&tui.build_input_title()).starts_with(frame)));

    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::Model(ModelEvent::Final {
        output: "main final text".to_string(),
        usage: Default::default(),
    })))
    .await
    .unwrap();

    assert!(
        tui.app.llm_busy,
        "Final renders output but does not end a turn"
    );

    tui.handle_tui_event(TuiEvent::Agent(AgentEvent::Turn(TurnEvent::Ended {
        outcome: Default::default(),
    })))
    .await
    .unwrap();

    assert!(!tui.app.llm_busy);
}

#[tokio::test]
async fn shared_session_activity_starts_and_stops_spinner() {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.session_activity_target = Some(("session-1".to_string(), "cluster-1".to_string()));

    tui.handle_tui_event(TuiEvent::SessionActivity {
        session_id: "session-1".to_string(),
        cluster: "cluster-1".to_string(),
        active: true,
    })
    .await
    .unwrap();

    assert!(tui.app.llm_busy);
    assert_eq!(
        tui.active_remote_session,
        Some(("session-1".to_string(), "cluster-1".to_string()))
    );
    assert!(SPINNER_FRAMES
        .iter()
        .any(|frame| line_to_plain(&tui.build_input_title()).starts_with(frame)));

    tui.handle_tui_event(TuiEvent::SessionActivity {
        session_id: "session-1".to_string(),
        cluster: "cluster-1".to_string(),
        active: false,
    })
    .await
    .unwrap();

    assert!(!tui.app.llm_busy);
    assert_eq!(tui.active_remote_session, None);
    assert!(line_to_plain(&tui.build_input_title()).starts_with('•'));
}
