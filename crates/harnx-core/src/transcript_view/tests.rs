use super::*;
use crate::message::{MessageContent, MessageRole};

mod counts;
mod edited_compactions;
mod mutations;
mod paging;
mod summaries;
mod windows;

const FIXTURE_JSON: &str = include_str!("../../tests/fixtures/compaction-history.json");

fn fixture() -> Vec<(u64, SessionLogEntry)> {
    serde_json::from_str(FIXTURE_JSON).unwrap()
}

fn through(seq: u64) -> Vec<(u64, SessionLogEntry)> {
    fixture()
        .into_iter()
        .take_while(|(s, _)| *s <= seq)
        .collect()
}

fn message(seq: u64, text: &str) -> (u64, SessionLogEntry) {
    (
        seq,
        SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text(text.into()),
            timestamp: None,
            fence_token: None,
        },
    )
}

fn compress(seq: u64) -> (u64, SessionLogEntry) {
    (
        seq,
        SessionLogEntry::Compress {
            prompt: "summary".into(),
        },
    )
}

fn seqs(view: &TranscriptView) -> Vec<u64> {
    view.entries.iter().map(|(seq, _)| *seq).collect()
}

fn displayed_seqs(view: &TranscriptView) -> Vec<u64> {
    let messages = view.entries.iter().filter_map(|(seq, entry)| {
        matches!(
            entry,
            SessionLogEntry::Message { .. }
                | SessionLogEntry::ToolCalls { .. }
                | SessionLogEntry::ToolResults { .. }
        )
        .then_some(*seq)
    });
    view.compaction
        .iter()
        .map(|m| m.compaction_seq)
        .chain(messages)
        .collect()
}

fn marker(view: &TranscriptView) -> &CompactionMarker {
    view.compaction.as_ref().unwrap()
}
