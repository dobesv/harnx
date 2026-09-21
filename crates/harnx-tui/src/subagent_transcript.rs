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
    match event {
        AgentEvent::Turn(TurnEvent::Started) => start_child_turn(state),
        AgentEvent::Turn(TurnEvent::Ended { .. } | TurnEvent::Interrupted { .. }) => {
            freeze_unfinished_tool_timers(state);
            finish_child_turn(state)
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
        AgentEvent::Tool(ToolEvent::Started {
            id,
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
                id: Some(id),
                start_anchor: std::time::Instant::now(),
                final_elapsed_ms: None,
                rendered_cache: None,
            });
            None
        }
        AgentEvent::Tool(ToolEvent::Completed {
            id,
            output,
            markdown,
            ..
        }) => {
            // Capture elapsed for the matching running ToolCall by ID.
            // If no matching ID is found, fall back to the most recent running tool.
            let transcript = &mut state.transcript;
            let matched_idx = transcript.iter_mut().rev().position(|item| {
                matches!(
                    item,
                    TranscriptItem::ToolCall {
                        final_elapsed_ms: None,
                        id: Some(ref i),
                        ..
                    } if i == &id
                )
            });
            let fallback_idx = if matched_idx.is_none() {
                transcript.iter_mut().rev().position(|item| {
                    matches!(
                        item,
                        TranscriptItem::ToolCall {
                            final_elapsed_ms: None,
                            ..
                        }
                    )
                })
            } else {
                None
            };
            if let Some(idx) = matched_idx.or(fallback_idx) {
                let actual_idx = transcript.len().saturating_sub(1).saturating_sub(idx);
                if let TranscriptItem::ToolCall {
                    start_anchor,
                    final_elapsed_ms,
                    ..
                } = &mut transcript[actual_idx]
                {
                    *final_elapsed_ms = Some(start_anchor.elapsed().as_millis() as u64);
                }
            }
            state.transcript.extend(tool_completed_to_transcript_items(
                &output,
                markdown.as_deref(),
            ));
            None
        }
        AgentEvent::Tool(ToolEvent::Failed { id, error }) => {
            // Capture elapsed for the matching running ToolCall by ID.
            // If no matching ID is found, fall back to the most recent running tool.
            let transcript = &mut state.transcript;
            let matched_idx = transcript.iter_mut().rev().position(|item| {
                matches!(
                    item,
                    TranscriptItem::ToolCall {
                        final_elapsed_ms: None,
                        id: Some(ref i),
                        ..
                    } if i == &id
                )
            });
            let fallback_idx = if matched_idx.is_none() {
                transcript.iter_mut().rev().position(|item| {
                    matches!(
                        item,
                        TranscriptItem::ToolCall {
                            final_elapsed_ms: None,
                            ..
                        }
                    )
                })
            } else {
                None
            };
            if let Some(idx) = matched_idx.or(fallback_idx) {
                let actual_idx = transcript.len().saturating_sub(1).saturating_sub(idx);
                if let TranscriptItem::ToolCall {
                    start_anchor,
                    final_elapsed_ms,
                    ..
                } = &mut transcript[actual_idx]
                {
                    *final_elapsed_ms = Some(start_anchor.elapsed().as_millis() as u64);
                }
            }
            state.transcript.push(TranscriptItem::ErrorText(error));
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
