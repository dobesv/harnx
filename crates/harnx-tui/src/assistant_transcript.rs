//! Shared assistant row replacement and boundary link projection.

use crate::types::TranscriptItem;
use harnx_core::event::{AgentEvent, ModelEvent, NoticeEvent, SessionEvent, ToolEvent, TurnEvent};

pub(super) fn ends_assistant_round(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::Tool(ToolEvent::Started { .. } | ToolEvent::Blocked { .. })
            | AgentEvent::User(_)
            | AgentEvent::Model(ModelEvent::Error(_))
            | AgentEvent::Turn(
                TurnEvent::Started
                    | TurnEvent::Ended { .. }
                    | TurnEvent::Interrupted { .. }
                    | TurnEvent::HandoffRequested { .. }
            )
    )
}

pub(super) fn closes_assistant_stream(event: &AgentEvent) -> bool {
    ends_assistant_round(event)
        || matches!(
            event,
            AgentEvent::Tool(_)
                | AgentEvent::Model(ModelEvent::ThoughtChunk { .. } | ModelEvent::Error(_))
                | AgentEvent::Notice(
                    NoticeEvent::Info(_) | NoticeEvent::Warning(_) | NoticeEvent::Error(_)
                )
                | AgentEvent::Plan { .. }
                | AgentEvent::Session(
                    SessionEvent::Generic { .. }
                        | SessionEvent::TitleGenerationFailed(_)
                        | SessionEvent::CompactingStarted { .. }
                        | SessionEvent::CompactingCompleted { .. }
                        | SessionEvent::CompactingFailed { .. }
                )
        )
}

pub(super) fn begin_assistant_stream(transcript: &mut Vec<TranscriptItem>) -> usize {
    replace_or_append_assistant(transcript, None, String::new())
}

pub(super) fn replace_or_append_assistant(
    transcript: &mut Vec<TranscriptItem>,
    index: Option<usize>,
    output: String,
) -> usize {
    if let Some(index) = index {
        if let Some(TranscriptItem::AssistantText {
            text,
            rendered_cache,
            ..
        }) = transcript.get_mut(index)
        {
            *text = output;
            *rendered_cache = None;
            return index;
        }
    }
    let index = transcript.len();
    transcript.push(TranscriptItem::AssistantText {
        text: output,
        seq: None,
        timestamp: Some(chrono::Utc::now()),
        rendered_cache: None,
    });
    index
}

pub(super) fn refresh_assistant_links<'a>(
    transcript: &mut Vec<TranscriptItem>,
    index: usize,
    tracked: impl IntoIterator<Item = &'a mut Option<usize>>,
) {
    let Some(TranscriptItem::AssistantText { text, .. }) = transcript.get(index) else {
        return;
    };
    let mut links = Vec::new();
    crate::lifecycle::append_markdown_links(&mut links, text);
    let start = index + 1;
    let end = start
        + transcript[start..]
            .iter()
            .take_while(|item| matches!(item, TranscriptItem::MarkdownLink { .. }))
            .count();
    for tracked in tracked.into_iter().flatten() {
        *tracked = rebase_link_index(*tracked, index, &transcript[start..end], &links);
    }
    // Refresh only this assistant's contiguous links. Repeated boundaries and
    // canonical finals replace the block instead of accumulating duplicates.
    transcript.splice(start..end, links);
}

fn rebase_link_index(
    index: usize,
    reply: usize,
    old: &[TranscriptItem],
    new: &[TranscriptItem],
) -> usize {
    let start = reply + 1;
    if index < start {
        return index;
    }
    if index >= start + old.len() {
        return (index - old.len()).saturating_add(new.len());
    }
    // A canonical final may reorder or remove links. Keep a surviving URL
    // selected; if its target disappeared, fall back to its owning reply.
    new.iter().position(|item| matches!((item, &old[index - start]),
        (TranscriptItem::MarkdownLink { url: a, .. }, TranscriptItem::MarkdownLink { url: b, .. }) if a == b
    )).map(|position| start + position).unwrap_or(reply)
}
