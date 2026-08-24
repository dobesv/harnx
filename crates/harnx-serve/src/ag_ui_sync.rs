use ag_ui_core::{
    event::{BaseEvent, CustomEvent, Event},
    types::{ids::MessageId, input::RunAgentInput, message::Message as AgUiMessage},
    JsonValue,
};
use serde_json::json;
use uuid::Uuid;

pub(crate) fn wire_message_id(id: &str) -> MessageId {
    id.parse::<MessageId>().unwrap_or_else(|_| {
        MessageId::from(Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("harnx-message:{id}").as_bytes(),
        ))
    })
}

/// Return only a genuinely new user prompt.
///
/// assistant-ui includes the hydrated transcript in every `runAgent` request.
/// If another client appended a user message, that message can be the final
/// input row during a promptless refresh. Its stable ID is already present in
/// the authoritative snapshot, so appending it again would duplicate the
/// prompt and can split a tool-call/result pair in the session log.
pub(crate) fn pending_user_prompt(
    run_input: &RunAgentInput<JsonValue, JsonValue>,
    snapshot: &[AgUiMessage],
) -> Option<String> {
    match run_input.messages.last() {
        Some(AgUiMessage::User { id, content, .. })
            if !content.trim().is_empty() && !snapshot.iter().any(|message| message.id() == id) =>
        {
            Some(content.clone())
        }
        _ => None,
    }
}

pub(crate) fn history_warning_event(message: String) -> Event {
    Event::Custom(CustomEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        name: "session_history_warning".to_string(),
        value: json!({ "message": message }),
    })
}
