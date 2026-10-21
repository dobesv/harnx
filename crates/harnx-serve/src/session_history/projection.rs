use super::{
    groups::{message_groups, Group},
    params::{Cursor, Selector, Window},
};
use crate::transcript_snapshot::SessionCompaction;
use ag_ui_core::types::message::Message as AgUiMessage;
use anyhow::{ensure, Result};
use harnx_core::{
    session::{Session, SessionLogEntry},
    transcript_view::{before_compaction, default_view, TranscriptView},
};
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct HistoryPage {
    pub history_snapshot: Vec<AgUiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<SessionCompaction>,
    pub next_cursor: Option<String>,
    pub prev_cursor: Option<String>,
    pub earlier_compaction: Option<u64>,
}

pub(super) struct Projection {
    groups: Vec<Group>,
    compaction: Option<SessionCompaction>,
    earlier_compaction: Option<u64>,
    bytes: usize,
}

impl Projection {
    pub fn build(
        entries: &[(u64, SessionLogEntry)],
        base: &Session,
        window: &Window,
    ) -> Result<Self> {
        let entries = &entries[..entries.partition_point(|(seq, _)| *seq <= window.as_of_seq)];
        ensure!(
            entries.last().map_or(0, |(seq, _)| *seq) == window.as_of_seq,
            "history cursor snapshot is no longer available"
        );
        let view = select_view(entries, &window.selector)?;
        let compaction = view.compaction.clone().map(SessionCompaction::from);
        let earlier_compaction = compaction.as_ref().map(|marker| marker.compaction_seq);
        let mut groups = message_groups(&view, base)?;
        if let Some(marker) = &compaction {
            groups.insert(
                0,
                Group {
                    from: marker.compaction_seq,
                    to: marker.compaction_seq,
                    messages: vec![marker.message()],
                    marker: true,
                },
            );
        }
        if let Selector::Range { from, to } = window.selector {
            groups.retain(|group| group.from <= to && group.to >= from);
        }
        Ok(Self {
            groups,
            compaction,
            earlier_compaction,
            bytes: 0,
        }
        .with_byte_size())
    }

    pub fn page(&self, cursor: &Cursor) -> Result<HistoryPage> {
        let start = cursor.offset;
        ensure!(
            start == 0 || start < self.groups.len(),
            "history cursor offset is out of range"
        );
        let end = start.saturating_add(cursor.limit).min(self.groups.len());
        let groups = &self.groups[start..end];
        Ok(HistoryPage {
            history_snapshot: groups
                .iter()
                .flat_map(|group| group.messages.clone())
                .collect(),
            compaction: groups
                .iter()
                .any(|group| group.marker)
                .then(|| self.compaction.clone())
                .flatten(),
            next_cursor: (end < self.groups.len()).then(|| at_offset(cursor, end)),
            prev_cursor: (start > 0).then(|| at_offset(cursor, start.saturating_sub(cursor.limit))),
            earlier_compaction: self.earlier_compaction,
        })
    }

    fn with_byte_size(mut self) -> Self {
        self.bytes = self
            .groups
            .iter()
            .map(|group| {
                serde_json::to_vec(&group.messages).map_or(0, |bytes| bytes.len())
                    + std::mem::size_of::<Group>()
            })
            .sum();
        self.bytes += self
            .compaction
            .as_ref()
            .map_or(0, |marker| marker.summary.len());
        self
    }

    pub fn byte_size(&self) -> usize {
        self.bytes
    }
}

fn at_offset(cursor: &Cursor, offset: usize) -> String {
    Cursor {
        offset,
        ..cursor.clone()
    }
    .encode()
}

fn select_view(entries: &[(u64, SessionLogEntry)], selector: &Selector) -> Result<TranscriptView> {
    match *selector {
        Selector::Before(seq) => before_compaction(entries, seq, None),
        Selector::Range { from, to } => range_view(entries, from, to),
    }
}

fn range_view(entries: &[(u64, SessionLogEntry)], from: u64, to: u64) -> Result<TranscriptView> {
    ensure!(
        from > 0 && from <= to,
        "sequence range must satisfy 0 < from_seq <= to_seq"
    );
    let first = entries.first().map_or(0, |(seq, _)| *seq);
    let last = entries.last().map_or(0, |(seq, _)| *seq);
    ensure!(
        from >= first && to <= last,
        "sequence range is outside session history ({first}..={last})"
    );
    // Explicit browse-back can open removed markers; ranges must follow the current branch.
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(entries)?;
    let next = effective
        .iter()
        .find(|(seq, entry)| *seq > to && matches!(entry, SessionLogEntry::Compress { .. }));
    let view = match next {
        Some((seq, _)) => before_compaction(entries, *seq, None)?,
        None => default_view(entries)?,
    };
    ensure!(
        view.compaction
            .as_ref()
            .is_none_or(|marker| from >= marker.compaction_seq),
        "sequence range crosses a compaction; use before_compaction to open each window"
    );
    Ok(view)
}
