//! Transcript reducers for independently monitored child sessions.

use crate::input::{clean_thought_chunk, concat_text_blocks};
use crate::tool_transcript::{
    blocked_tool_to_transcript_items, tool_completed_to_transcript_items,
    tool_started_to_transcript_items,
};
use crate::types::{MonitoredSessionKey, MonitoredSessionState, SubAgentStatus, TranscriptItem};
use harnx_core::event::{AgentEvent, ModelEvent, NoticeEvent, ToolEvent, TurnEvent};

pub(super) fn update_row(
    transcript: &mut [TranscriptItem],
    key: &MonitoredSessionKey,
    status: &SubAgentStatus,
) {
    if let Some(TranscriptItem::SubAgentSession {
        status: row_status,
        progress,
        ..
    }) = transcript.iter_mut().rev().find(
        |item| matches!(item, TranscriptItem::SubAgentSession { key: row_key, .. } if row_key == key),
    ) {
        // Invocation progress is authoritative for invocation rows. The
        // independently attached child-session monitor can briefly observe a
        // pending durable turn after the parent already received the terminal
        // tool result, so it must not repaint a completed invocation as running.
        if progress.is_none() {
            *row_status = status.clone();
        }
    }
}

pub(super) fn flatten_subagent_event(event: AgentEvent) -> AgentEvent {
    match event {
        AgentEvent::SubAgent { event, .. } => *event,
        event => event,
    }
}

pub(super) fn apply_child_event(
    state: &mut MonitoredSessionState,
    event: AgentEvent,
) -> Option<SubAgentStatus> {
    prepare_child_assistant_boundary(state, &event);
    match event {
        AgentEvent::Turn(TurnEvent::Started) => start_child_turn(state),
        AgentEvent::Turn(TurnEvent::Ended { .. } | TurnEvent::Interrupted { .. }) => {
            freeze_unfinished_tool_timers(state);
            finish_child_turn(state)
        }
        AgentEvent::User(harnx_core::event::UserEvent::Message { content }) => {
            append_child_prompt(state, content);
            None
        }
        AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
            append_child_message(state, concat_text_blocks(&blocks));
            None
        }
        AgentEvent::Model(ModelEvent::ThoughtChunk { blocks }) => {
            append_child_thought(state, clean_thought_chunk(&concat_text_blocks(&blocks)));
            None
        }
        AgentEvent::Model(ModelEvent::Final { output, .. }) => {
            finish_child_message(state, output);
            None
        }
        AgentEvent::Model(ModelEvent::Error(error)) => fail_child(state, error),
        AgentEvent::Tool(event) => {
            apply_child_tool_event(state, event);
            None
        }
        AgentEvent::Notice(NoticeEvent::Error(error)) => {
            state.transcript.push(TranscriptItem::ErrorText(error));
            None
        }
        AgentEvent::Notice(NoticeEvent::Warning(text)) => {
            state
                .transcript
                .push(TranscriptItem::SystemText(format!("⚠ {text}")));
            None
        }
        AgentEvent::Plan { entries } => {
            state.transcript.push(TranscriptItem::Plan(entries));
            None
        }
        _ => None,
    }
}

fn apply_child_tool_event(state: &mut MonitoredSessionState, event: ToolEvent) {
    match event {
        event @ ToolEvent::Started { .. } => {
            state.streaming_open = false;
            state
                .transcript
                .extend(tool_started_to_transcript_items(event, None));
        }
        event @ ToolEvent::Blocked { .. } => {
            state
                .transcript
                .extend(blocked_tool_to_transcript_items(event, None));
        }
        ToolEvent::Update {
            id,
            markdown,
            status,
            title,
            kind,
            locations,
            usage,
            ..
        } => {
            crate::tool_render::apply_tool_event_update(
                &mut state.transcript,
                crate::tool_render::ToolUpdatePayload {
                    id,
                    markdown,
                    status,
                    title,
                    kind,
                    locations,
                    usage,
                },
                None,
            );
        }
        ToolEvent::Completed {
            id,
            output,
            markdown,
            ..
        } => {
            crate::tool_render::complete_tool_call(&mut state.transcript, &id);
            state.transcript.extend(tool_completed_to_transcript_items(
                &output,
                markdown.as_deref(),
            ));
        }
        ToolEvent::Failed { id, error } => {
            crate::tool_render::fail_tool_call(&mut state.transcript, &id);
            state.transcript.push(TranscriptItem::ErrorText(error));
        }
        _ => {}
    }
}

fn append_child_prompt(state: &mut MonitoredSessionState, content: String) {
    // Replayed/attached user prompt for a subagent session.
    // Matches top-level AgentEvent::User handling which also appends links.
    let mut items = vec![TranscriptItem::UserText {
        text: content.clone(),
        seq: None,
        timestamp: Some(chrono::Utc::now()),
    }];
    crate::lifecycle::append_markdown_links(&mut items, &content);
    state.transcript.extend(items);
}

fn start_child_turn(state: &mut MonitoredSessionState) -> Option<SubAgentStatus> {
    state.status = SubAgentStatus::Running;
    state.streaming_open = false;
    Some(SubAgentStatus::Running)
}

fn finish_child_turn(state: &mut MonitoredSessionState) -> Option<SubAgentStatus> {
    state.streaming_open = false;
    if state.status != SubAgentStatus::Failed {
        state.status = SubAgentStatus::Completed;
    }
    Some(state.status.clone())
}

fn append_child_message(state: &mut MonitoredSessionState, text: String) {
    let open = state.streaming_open
        && matches!(
            state.transcript.last(),
            Some(TranscriptItem::AssistantText { .. })
        );
    if !open {
        close_child_assistant_stream(state);
        state.streamed_text_idx = Some(crate::assistant_transcript::begin_assistant_stream(
            &mut state.transcript,
        ));
        state.streaming_open = true;
    }
    if let Some(TranscriptItem::AssistantText {
        text: output,
        rendered_cache,
        ..
    }) = state.transcript.last_mut()
    {
        output.push_str(&text);
        *rendered_cache = None;
    }
    state.scroll.follow = true;
}

fn append_child_thought(state: &mut MonitoredSessionState, text: String) {
    if let Some(TranscriptItem::ThoughtText(output)) = state.transcript.last_mut() {
        output.push_str(&text);
    } else if !text.is_empty() {
        state.transcript.push(TranscriptItem::ThoughtText(text));
    }
}

fn finish_child_message(state: &mut MonitoredSessionState, output: String) {
    if !output.is_empty() {
        let index = crate::assistant_transcript::replace_or_append_assistant(
            &mut state.transcript,
            state.streamed_text_idx,
            output,
        );
        refresh_child_assistant_links(state, index);
    } else {
        close_child_assistant_stream(state);
    }
    state.streaming_open = false;
    state.streamed_text_idx = None;
}

fn prepare_child_assistant_boundary(state: &mut MonitoredSessionState, event: &AgentEvent) {
    if crate::assistant_transcript::closes_assistant_stream(event) {
        close_child_assistant_stream(state);
    }
    if crate::assistant_transcript::ends_assistant_round(event) {
        state.streamed_text_idx = None;
    }
}

fn close_child_assistant_stream(state: &mut MonitoredSessionState) {
    if state.streaming_open {
        if let Some(index) = state.streamed_text_idx {
            refresh_child_assistant_links(state, index);
        }
        state.streaming_open = false;
    }
}

fn refresh_child_assistant_links(state: &mut MonitoredSessionState, index: usize) {
    crate::assistant_transcript::refresh_assistant_links(
        &mut state.transcript,
        index,
        [&mut state.transcript_focus, &mut state.streamed_text_idx],
    );
}

fn fail_child(state: &mut MonitoredSessionState, error: String) -> Option<SubAgentStatus> {
    state.transcript.push(TranscriptItem::ErrorText(error));
    state.status = SubAgentStatus::Failed;
    state.streaming_open = false;
    Some(SubAgentStatus::Failed)
}

/// Freeze any running tool timers that lack a final elapsed value.
/// Called when a turn ends or is interrupted to stop timers from ticking.
pub(super) fn freeze_unfinished_tool_timers(state: &mut MonitoredSessionState) {
    for item in state.transcript.iter_mut() {
        if let TranscriptItem::ToolCall {
            start_anchor,
            ref mut final_elapsed_ms,
            ..
        } = item
        {
            if final_elapsed_ms.is_none() {
                *final_elapsed_ms = Some(start_anchor.elapsed().as_millis() as u64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_final_message_appends_markdown_links() {
        let mut state = MonitoredSessionState::new(SubAgentStatus::Running);
        apply_child_event(
            &mut state,
            AgentEvent::Model(ModelEvent::Final {
                output: "Read [Harnx](https://harnx.dev)".into(),
                usage: Default::default(),
            }),
        );
        assert!(matches!(
            state.transcript.as_slice(),
            [
                TranscriptItem::AssistantText { text, .. },
                TranscriptItem::MarkdownLink { text: link_text, url }
            ] if text.contains("Harnx") && link_text == "Harnx" && url == "https://harnx.dev"
        ));
    }

    #[test]
    fn child_user_message_appends_markdown_links() {
        let mut state = MonitoredSessionState::new(SubAgentStatus::Running);
        apply_child_event(
            &mut state,
            AgentEvent::User(harnx_core::event::UserEvent::Message {
                content: "See [Docs](https://docs.example.com)".into(),
            }),
        );
        assert!(matches!(
            state.transcript.as_slice(),
            [
                TranscriptItem::UserText { text, .. },
                TranscriptItem::MarkdownLink { text: link_text, url }
            ] if text.contains("Docs") && link_text == "Docs" && url == "https://docs.example.com"
        ));
    }

    #[test]
    fn child_tool_started_extracts_markdown_links() {
        use harnx_core::event::ToolKind;
        let mut state = MonitoredSessionState::new(SubAgentStatus::Running);
        apply_child_event(
            &mut state,
            AgentEvent::Tool(ToolEvent::Started {
                id: "child-tool-1".into(),
                name: "session_prompt".into(),
                kind: ToolKind::Other,
                markdown: Some("[Child Docs](https://child.example.com)".into()),
                input: serde_json::json!({}),
                locations: vec![],
            }),
        );
        // Should have ToolCall followed by MarkdownLink
        assert!(matches!(
            state.transcript.as_slice(),
            [
                TranscriptItem::ToolCall { .. },
                TranscriptItem::MarkdownLink { text, url }
            ] if text == "Child Docs" && url == "https://child.example.com"
        ));
    }
}
