//! The new-message stop: which specs opt in, and how the result reports it.
use super::*;
use harnx_toolset::{
    SUBAGENT_SESSION_LOAD_TOOL, SUBAGENT_SESSION_PROMPT_TOOL, SUBAGENT_SESSION_SEND_TOOL,
    SUBAGENT_SESSION_WAIT_TOOL,
};
use serde_json::json;

#[test]
fn only_send_and_wait_stop_on_new_message() {
    let opted_in: Vec<String> = super::super::tool_specs("helper")
        .into_iter()
        .filter(|spec| spec.stops_on_new_message())
        .map(|spec| spec.name)
        .collect();
    assert_eq!(
        opted_in,
        [SUBAGENT_SESSION_SEND_TOOL, SUBAGENT_SESSION_WAIT_TOOL]
    );
    for tool in [SUBAGENT_SESSION_PROMPT_TOOL, SUBAGENT_SESSION_LOAD_TOOL] {
        assert!(!opted_in.iter().any(|name| name == tool), "{tool}");
    }
}

#[test]
fn stopped_by_new_message_serializes_and_fits_reserved_metadata() {
    assert_eq!(
        serde_json::to_value(WaitStop::NewMessage).unwrap(),
        json!("new_message")
    );
    let stopped = SessionPromptResult {
        session_id: "child".into(),
        prompt_id: "prompt".into(),
        invocation_id: None,
        dispatch: None,
        status: SessionPromptStatus::Unknown,
        terminal: false,
        wait_expired: false,
        stopped_by: Some(WaitStop::NewMessage),
        elapsed_ms: u64::MAX,
        output: Value::Null,
    };
    let size = serde_json::to_string(&stopped).unwrap().len();
    assert!(
        result_metadata_bytes("child", "prompt") >= size,
        "the reservation must cover a stopped_by field"
    );
}

#[test]
fn widest_stop_serializes_at_least_as_long_as_every_variant() {
    let len = |stop: WaitStop| serde_json::to_string(&stop).unwrap().len();
    // Listing the variants exhaustively makes a new one fail to compile here
    // until it is checked against the reservation.
    for stop in [WaitStop::NewMessage, WaitStop::CallerDeadline] {
        match stop {
            WaitStop::NewMessage | WaitStop::CallerDeadline => {}
        }
        assert!(len(WaitStop::WIDEST) >= len(stop), "{stop:?}");
    }
    assert_eq!(
        serde_json::to_value(WaitStop::CallerDeadline).unwrap(),
        json!("caller_deadline")
    );
}
