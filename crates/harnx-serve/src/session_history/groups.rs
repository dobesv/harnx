use crate::{ag_ui_sync::wire_message_id, history_projection::project_message};
use ag_ui_core::types::message::Message as AgUiMessage;
use anyhow::Result;
use harnx_core::{
    message::{Message, MessageContent},
    session::{Session, SessionLogEntry},
    transcript_view::TranscriptView,
};
use std::collections::{HashMap, VecDeque};

pub(super) struct Group {
    pub from: u64,
    pub to: u64,
    pub messages: Vec<AgUiMessage>,
    pub reasoning: Option<(String, Vec<String>)>,
    pub marker: bool,
}

pub(super) fn message_groups(view: &TranscriptView, base: &Session) -> Result<Vec<Group>> {
    let mut session = harnx_runtime::nats_session_log::load_session_from_entries_with_metadata(
        &view.entries,
        base.id(),
        base.clone(),
    )?;
    // Per-seq ordinals separate edit replacements without depending on window position.
    // Rebuilds after eviction must return the same IDs as cache hits.
    let mut occurrences = HashMap::<usize, usize>::new();
    for message in &mut session.messages {
        let ordinal = occurrences.entry(message.log_seq.unwrap_or(0)).or_default();
        stabilize_history_id(message, *ordinal);
        *ordinal += 1;
    }
    let mut rounds = round_bounds(&view.entries);
    Ok(session
        .messages
        .iter()
        .enumerate()
        .map(|(ordinal, message)| {
            let seq = message.log_seq.unwrap_or(0) as u64;
            let end = round_end(&mut rounds, message, seq);
            let mut projected = project_message(message, ordinal);
            stabilize_tool_ids(&mut projected.rows);
            Group {
                from: seq.min(end),
                to: seq.max(end),
                messages: projected.rows,
                reasoning: projected.reasoning,
                marker: false,
            }
        })
        .collect())
}

fn stabilize_tool_ids(rows: &mut [AgUiMessage]) {
    let Some(first) = rows.first() else { return };
    let base = first.id().to_string();
    for (index, row) in rows.iter_mut().enumerate() {
        if let AgUiMessage::Tool { id, .. } = row {
            *id = wire_message_id(&format!("history-result:{base}:{index}"));
        }
    }
}

fn round_end(bounds: &mut HashMap<u64, VecDeque<u64>>, message: &Message, seq: u64) -> u64 {
    if !matches!(message.content, MessageContent::ToolCalls(_)) {
        return seq;
    }
    bounds
        .get_mut(&seq)
        .and_then(VecDeque::pop_front)
        .unwrap_or(seq)
}

fn round_bounds(entries: &[(u64, SessionLogEntry)]) -> HashMap<u64, VecDeque<u64>> {
    let mut bounds = HashMap::<u64, VecDeque<u64>>::new();
    let mut pending = None;
    for (seq, entry) in entries {
        match entry {
            SessionLogEntry::ToolCalls { .. } => pending = Some(*seq),
            SessionLogEntry::ToolResults { .. } => {
                if let Some(call_seq) = pending.take() {
                    bounds.entry(call_seq).or_default().push_back(*seq);
                }
            }
            _ => {}
        }
    }
    bounds
}

fn stabilize_history_id(message: &mut Message, ordinal: usize) {
    if message.id.is_none() || matches!(message.content, MessageContent::ToolCalls(_)) {
        // Orphan-round repair assigns a fresh ID during replay, not in the log.
        message.id = Some(format!(
            "history:{}:{ordinal}",
            message.log_seq.unwrap_or(0)
        ));
    }
}
