use super::test_config;
use crate::types::{TranscriptItem, Tui, TuiEvent};
use harnx_core::event::{AgentEvent, ModelEvent};

async fn shared_session_tui() -> Tui {
    let config = test_config();
    let mut tui = Tui::init(&config).await.unwrap();
    tui.live_events.select(Some("shared-generation".into()));
    tui.session_activity_target = Some(("session-1".to_string(), "cluster-1".to_string()));
    tui
}

fn final_event(tui: &Tui, output: &str) -> TuiEvent {
    TuiEvent::SessionAgent {
        stamp: crate::event_isolation::EventStamp::snapshot(&tui.live_events),
        historical: false,
        session_id: "session-1".to_string(),
        cluster: "cluster-1".to_string(),
        event: AgentEvent::Model(ModelEvent::Final {
            output: output.to_string(),
            usage: Default::default(),
        }),
    }
}

#[tokio::test]
async fn shared_session_agent_events_render_for_the_selected_idle_session() {
    let mut tui = shared_session_tui().await;
    tui.handle_tui_event(final_event(&tui, "answer from another frontend"))
        .await
        .unwrap();

    assert!(tui.app.transcript.iter().any(|entry| {
        matches!(entry, TranscriptItem::AssistantText { text, .. }
            if text == "answer from another frontend")
    }));
}

#[tokio::test]
async fn shared_session_agent_events_do_not_duplicate_the_locally_owned_prompt() {
    let mut tui = shared_session_tui().await;
    tui.current_prompt_abort = Some(harnx_runtime::utils::create_abort_signal());
    tui.handle_tui_event(final_event(&tui, "duplicate"))
        .await
        .unwrap();

    assert!(!tui.app.transcript.iter().any(|entry| {
        matches!(entry, TranscriptItem::AssistantText { text, .. } if text == "duplicate")
    }));
}
