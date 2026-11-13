//! Stable placement of sub-agent rows within their originating tool blocks.

use crate::types::{
    MonitoredSessionKey, SubAgentInvocationProgress, SubAgentStatus, TranscriptItem, Tui,
};
use harnx_core::event::SubAgentProgress;

pub(super) struct RowUpdate {
    pub key: MonitoredSessionKey,
    pub status: SubAgentStatus,
    pub invocation_id: Option<String>,
    pub progress: Option<SubAgentInvocationProgress>,
}

/// What an update did to a sub-agent's transcript row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RowChange {
    /// The row already holds a terminal snapshot, which nothing reopens.
    Rejected,
    Updated,
    Created(usize),
    Relocated {
        from: usize,
        to: usize,
    },
}

pub(super) enum TranscriptEdit {
    Splice {
        replaced: std::ops::Range<usize>,
        added: usize,
    },
    Move {
        from: usize,
        to: usize,
    },
}

impl TranscriptEdit {
    fn rebase(&self, index: usize) -> usize {
        match self {
            Self::Move { from, to } if index == *from => *to,
            Self::Move { from, to } => {
                let after_removal = index - usize::from(index > *from);
                after_removal + usize::from(after_removal >= *to)
            }
            Self::Splice { replaced, added } if index >= replaced.end => {
                index - replaced.len() + added
            }
            Self::Splice { replaced, added } if index >= replaced.start => {
                replaced.start + (index - replaced.start).min(added.saturating_sub(1))
            }
            Self::Splice { .. } => index,
        }
    }
}

/// Include the call's links and result, without crossing into later activity.
fn tool_block_end(transcript: &[TranscriptItem], tool_call_id: Option<&str>) -> Option<usize> {
    let id = tool_call_id?;
    let call = transcript.iter().rposition(|item| {
        matches!(item,
            TranscriptItem::ToolCall { id: Some(current), .. } if current == id
        )
    })?;
    Some(
        call + 1
            + transcript[call + 1..]
                .iter()
                .take_while(|item| {
                    matches!(
                        item,
                        TranscriptItem::MarkdownLink { .. }
                            | TranscriptItem::ToolResultMarkdown { .. }
                            | TranscriptItem::SubAgentSession { .. }
                    )
                })
                .count(),
    )
}

pub(super) fn has_visible_origin(
    transcript: &[TranscriptItem],
    snapshot: &SubAgentProgress,
) -> bool {
    transcript.iter().any(|item| {
        matches!(item,
            TranscriptItem::SubAgentSession { invocation_id: Some(id), .. }
                if id == &snapshot.invocation_id
        )
    }) || tool_block_end(transcript, snapshot.tool_call_id.as_deref()).is_some()
}

impl Tui {
    /// Preserve selections and streaming targets when a child row/reply is
    /// inserted into an earlier tool block, in either the root or a child.
    pub(super) fn rebase_subagent_transcript_indices(
        &mut self,
        parent: Option<&MonitoredSessionKey>,
        edit: TranscriptEdit,
    ) {
        let indices = match parent {
            Some(parent) => {
                let Some(state) = self.app.monitored_sessions.get_mut(parent) else {
                    return;
                };
                vec![&mut state.transcript_focus, &mut state.streamed_text_idx]
            }
            None => vec![
                &mut self.app.transcript_focus,
                &mut self.app.transcript_selection_anchor,
                &mut self.app.main_streamed_text_idx,
                &mut self.app.streamed_text_idx,
            ],
        };
        for index in indices.into_iter().flatten() {
            *index = edit.rebase(*index);
        }
    }
}

pub(super) fn upsert_invocation(
    transcript: &mut Vec<TranscriptItem>,
    update: RowUpdate,
    invocation_id: String,
) -> RowChange {
    let existing = transcript.iter().position(|item| {
        matches!(item, TranscriptItem::SubAgentSession { invocation_id: Some(id), .. } if id == &invocation_id)
    });
    if let Some(index) = existing {
        return update_invocation(transcript, index, update);
    }
    let position = update
        .progress
        .as_ref()
        .and_then(|progress| tool_block_end(transcript, progress.snapshot.tool_call_id.as_deref()))
        .unwrap_or(transcript.len());
    transcript.insert(
        position,
        TranscriptItem::SubAgentSession {
            key: update.key,
            status: update.status,
            invocation_id: Some(invocation_id),
            progress: update.progress,
        },
    );
    RowChange::Created(position)
}

fn update_invocation(
    transcript: &mut Vec<TranscriptItem>,
    index: usize,
    update: RowUpdate,
) -> RowChange {
    let TranscriptItem::SubAgentSession {
        status, progress, ..
    } = &mut transcript[index]
    else {
        unreachable!()
    };
    if progress
        .as_ref()
        .is_some_and(SubAgentInvocationProgress::is_terminal)
    {
        return RowChange::Rejected;
    }
    let needs_origin = progress
        .as_ref()
        .is_none_or(|p| p.snapshot.tool_call_id.is_none());
    *status = update.status;
    if update.progress.is_some() {
        *progress = update.progress;
    }
    let tool_call_id = progress
        .as_ref()
        .and_then(|p| p.snapshot.tool_call_id.clone());
    if needs_origin {
        if let Some(position) = tool_block_end(transcript, tool_call_id.as_deref()) {
            // A first preview may lack correlation. Bind it once a later
            // snapshot supplies the call ID, then keep that position stable.
            let to = position - usize::from(position > index);
            if to != index {
                let row = transcript.remove(index);
                transcript.insert(to, row);
                return RowChange::Relocated { from: index, to };
            }
        }
    }
    RowChange::Updated
}

pub(super) fn upsert_legacy(transcript: &mut Vec<TranscriptItem>, update: RowUpdate) -> RowChange {
    let latest_tool = transcript
        .iter()
        .rposition(|item| matches!(item, TranscriptItem::ToolCall { .. }));
    let latest_row = transcript.iter().rposition(
        |item| matches!(item, TranscriptItem::SubAgentSession { key, .. } if key == &update.key),
    );
    let row_follows_tool = latest_row.is_some_and(|row| latest_tool.is_none_or(|tool| row > tool));
    if row_follows_tool {
        update_legacy_status(transcript, latest_row, update.status);
        return RowChange::Updated;
    }
    transcript.push(TranscriptItem::SubAgentSession {
        key: update.key,
        status: update.status,
        invocation_id: None,
        progress: None,
    });
    RowChange::Created(transcript.len() - 1)
}

fn update_legacy_status(
    transcript: &mut [TranscriptItem],
    row: Option<usize>,
    status: SubAgentStatus,
) {
    let Some(TranscriptItem::SubAgentSession {
        status: row_status, ..
    }) = row.and_then(|row| transcript.get_mut(row))
    else {
        return;
    };
    *row_status = status;
}
