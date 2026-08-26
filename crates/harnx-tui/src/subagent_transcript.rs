//! Transcript reducers for independently monitored child sessions.

use crate::input::{
    clean_thought_chunk, concat_text_blocks, tool_call_body, tool_completed_to_transcript_items,
};
use crate::types::{MonitoredSessionKey, MonitoredSessionState, SubAgentStatus, TranscriptItem};
use harnx_core::event::{AgentEvent, ModelEvent, NoticeEvent, ToolEvent, TurnEvent};

pub(super) fn update_row(
    transcript: &mut [TranscriptItem],
    key: &MonitoredSessionKey,
    status: &SubAgentStatus,
) {
    if let Some(TranscriptItem::SubAgentSession {
        status: row_status, ..
    }) = transcript.iter_mut().rev().find(
        |item| matches!(item, TranscriptItem::SubAgentSession { key: row_key, .. } if row_key == key),
    ) {
        *row_status = status.clone();
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
    match event {
        AgentEvent::Turn(TurnEvent::Started) => start_child_turn(state),
        AgentEvent::Turn(TurnEvent::Ended { .. }) => finish_child_turn(state),
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
        AgentEvent::Tool(ToolEvent::Started {
            name,
            markdown,
            input,
            ..
        }) => {
            state.streaming_open = false;
            state.transcript.push(TranscriptItem::ToolCall {
                tool_name: name,
                body: tool_call_body(markdown.as_deref(), &input),
                seq: None,
                timestamp: Some(chrono::Utc::now()),
                rendered_cache: None,
            });
            None
        }
        AgentEvent::Tool(ToolEvent::Completed {
            output, markdown, ..
        }) => {
            state.transcript.extend(tool_completed_to_transcript_items(
                &output,
                markdown.as_deref(),
            ));
            None
        }
        AgentEvent::Tool(ToolEvent::Failed { error, .. })
        | AgentEvent::Notice(NoticeEvent::Error(error)) => {
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
        state.transcript.push(TranscriptItem::AssistantText {
            text: String::new(),
            seq: None,
            timestamp: Some(chrono::Utc::now()),
            rendered_cache: None,
        });
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
        let replace_streamed = state.streaming_open
            && matches!(
                state.transcript.last(),
                Some(TranscriptItem::AssistantText { .. })
            );
        if replace_streamed {
            if let Some(TranscriptItem::AssistantText {
                text,
                rendered_cache,
                ..
            }) = state.transcript.last_mut()
            {
                *text = output;
                *rendered_cache = None;
            }
        } else {
            state.transcript.push(TranscriptItem::AssistantText {
                text: output,
                seq: None,
                timestamp: Some(chrono::Utc::now()),
                rendered_cache: None,
            });
        }
    }
    state.streaming_open = false;
}

fn fail_child(state: &mut MonitoredSessionState, error: String) -> Option<SubAgentStatus> {
    state.transcript.push(TranscriptItem::ErrorText(error));
    state.status = SubAgentStatus::Failed;
    state.streaming_open = false;
    Some(SubAgentStatus::Failed)
}
