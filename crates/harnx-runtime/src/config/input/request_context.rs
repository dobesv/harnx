//! Request-only context, appended after the session or agent history.

use super::{Input, Message, MessageContent, MessageRole};

pub(super) fn append(input: &Input, messages: &mut Vec<Message>, with_session: bool) {
    append_sessionless_tool_calls(input, messages, with_session);
    append_injected_user_messages(input, messages);
    append_timing_context(input, messages);
    append_transient_note(input, messages);
}

fn append_sessionless_tool_calls(input: &Input, messages: &mut Vec<Message>, with_session: bool) {
    // Append `input.tool_calls` only when there's no session. With a session,
    // `append_session_tool_calls` + `append_session_tool_results` already pushed
    // the same `(call, result)` pair into `session.messages`.
    if !with_session {
        if let Some(tool_calls) = &input.tool_calls {
            messages.push(Message::new(
                MessageRole::Assistant,
                MessageContent::ToolCalls(tool_calls.clone()),
            ))
        }
    }
}

fn append_injected_user_messages(input: &Input, messages: &mut Vec<Message>) {
    if !input.injected_user_messages.is_empty() {
        // Mid-turn injection from durable rows: carry them to the model as
        // themselves, matching what a reload of the log would reconstruct.
        // The rows prepare_input pushed into session memory sit past the
        // `session_input_start` truncation, so this is their only appearance
        // in the request; later rounds see the memory copies instead.
        messages.extend(input.injected_user_messages.iter().cloned());
    } else if let Some(text) = &input.injected_user_text {
        messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text(text.clone()),
        ));
    }
}

fn append_timing_context(input: &Input, messages: &mut Vec<Message>) {
    if let Some(timing) = &input.timing_context {
        let already_present = messages.iter().any(|message| {
            message.role.is_user()
                && harnx_core::timing_identity::TimingReceipt::from_notice_line(
                    &message.content.to_text(),
                )
                .as_ref()
                    == Some(&timing.receipt)
        });
        if !already_present {
            messages.push(timing.message.clone());
        }
    }
}

fn append_transient_note(input: &Input, messages: &mut Vec<Message>) {
    if let Some(note) = &input.transient_note {
        messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text(note.clone()),
        ))
    }
}
