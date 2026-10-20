//! Compaction-aware transcript windows shared by frontend adapters.
//!
//! Pass a complete, ordered log from `NatsSessionLog::load_events_async`, not a
//! pre-filtered page. Mutation resolution uses the canonical session resolver.
//! Render `compaction` first, then `entries` using the existing message/tool
//! adapters. Entries retain physical sequences and control annotations; this
//! module doesn't convert them to model messages or change any UI behavior.
//!
//! Only raw `Compress` entries have browse-back markers. `Compress` rows inside
//! `EditEntries` replacements still cut the effective window, but share their
//! mutation's sequence, so no marker or ancestry link is synthesized for them.

use crate::compaction_summary::display_summary;
use crate::session::SessionLogEntry;
use crate::session_reconstruct::{active_context_window, apply_log_mutations_nats};
use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use std::ops::RangeInclusive;

mod kept_count;

/// A summary and the physical log range available through browse-back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionMarker {
    /// Pass this sequence to [`before_compaction`] to open this marker's archive.
    pub compaction_seq: u64,
    /// Display text with the exact trailing runtime recovery note removed.
    /// The unmodified text remains in the source `Compress.prompt` entry.
    pub summary: String,
    /// Inclusive archived range, excluding both this and the earlier marker.
    /// After an edited-only boundary, starts at the smallest remaining sequence.
    /// These are log bounds, not the bounds of C2's non-contiguous dropped set.
    /// `from_seq > to_seq` denotes an empty archive.
    pub from_seq: u64,
    pub to_seq: u64,
    /// Only `Some(0)` or `None` is produced today; nonzero retention is always
    /// unknown. Never use this to select rows or render a nonzero kept count.
    pub kept_count: Option<usize>,
    /// Follow this sequence with [`before_compaction`] to walk back again.
    pub earlier_compaction: Option<u64>,
}

impl CompactionMarker {
    /// Inclusive archived log bounds, or `None` when the archive is empty.
    pub fn archive_range(&self) -> Option<RangeInclusive<u64>> {
        (self.from_seq <= self.to_seq).then_some(self.from_seq..=self.to_seq)
    }
}

/// Render the optional marker followed by the effective entries in replay order.
///
/// A tool round remains separate `ToolCalls` and `ToolResults` entries. Cancel,
/// error, attachment and other control entries remain available to adapters.
/// The canonical resolver removes mutation records and non-transcript metadata
/// such as `TurnEnd`; their physical sequence numbers are not renumbered.
/// Edited rows share their mutation's sequence and can precede lower sequences
/// in replay order. Sequence-range filtering never reorders those rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptView {
    pub compaction: Option<CompactionMarker>,
    pub entries: Vec<(u64, SessionLogEntry)>,
}

/// The latest marker, re-logged kept copies, then later activity. Without a
/// compaction, returns the plain effective transcript. Edited-only compaction
/// boundaries trim entries but have no marker. No content/ID-based deduplication
/// is performed: originals belong to the preceding window.
///
/// Input sequences must be nonzero and strictly increasing (as in the NATS
/// reader). The whole log is required to resolve mutations and archive links.
pub fn default_view(entries: &[(u64, SessionLogEntry)]) -> Result<TranscriptView> {
    validate_log(entries)?;
    build_view(entries)
}

/// The view immediately before the raw `Compress` at `compaction_seq`.
///
/// Later edits/rewinds cannot change this historical view. It includes the
/// previous marker, if any, plus that window's original messages, including
/// originals that were subsequently kept. Even a since-rewound compaction can
/// be addressed explicitly while it remains in the raw log.
///
/// `seq_range` is inclusive and filters the marker by its own sequence as well
/// as the entries. `None` returns the whole window. Bounds may fall in sequence
/// gaps; a disjoint range returns an empty page. Reversed bounds and unknown or
/// non-`Compress` sequences are errors. Marker archive bounds never change when
/// paging. Pages are entry-level: join pages before assembling tool rounds if a
/// range cuts between `ToolCalls` and `ToolResults`.
pub fn before_compaction(
    entries: &[(u64, SessionLogEntry)],
    compaction_seq: u64,
    seq_range: Option<RangeInclusive<u64>>,
) -> Result<TranscriptView> {
    validate_log(entries)?;
    let index = compaction_index(entries, compaction_seq)?;
    let view = build_view(&entries[..index])?;
    page_view(view, seq_range)
}

fn validate_log(entries: &[(u64, SessionLogEntry)]) -> Result<()> {
    ensure!(
        !entries.first().is_some_and(|(seq, _)| *seq == 0)
            && entries.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "session log sequences must be nonzero and strictly increasing"
    );
    Ok(())
}

fn compaction_index(entries: &[(u64, SessionLogEntry)], seq: u64) -> Result<usize> {
    let Ok(index) = entries.binary_search_by_key(&seq, |(seq, _)| *seq) else {
        bail!("unknown compaction sequence {seq}");
    };
    ensure!(
        matches!(entries[index].1, SessionLogEntry::Compress { .. }),
        "sequence {seq} is not a compaction"
    );
    Ok(index)
}

fn build_view(raw: &[(u64, SessionLogEntry)]) -> Result<TranscriptView> {
    let effective = apply_log_mutations_nats(raw)?;
    let boundary = active_context_window(&effective).boundary_index();
    let compaction = boundary
        .and_then(|index| raw_compaction_seq(raw, effective[index].0))
        .map(|seq| build_marker(raw, seq))
        .transpose()?;
    let start = boundary.map_or(0, |index| index + 1);
    Ok(TranscriptView {
        compaction,
        entries: effective.into_iter().skip(start).collect(),
    })
}

fn build_marker(raw: &[(u64, SessionLogEntry)], compaction_seq: u64) -> Result<CompactionMarker> {
    let index = compaction_index(raw, compaction_seq)?;
    let SessionLogEntry::Compress { prompt } = &raw[index].1 else {
        unreachable!("compaction_index checked the entry type");
    };
    // Resolve this prefix separately: later edits must not change the marker's
    // archive bounds or its link to the view that existed before compaction.
    let archived = apply_log_mutations_nats(&raw[..index])?;
    let (from_seq, earlier_compaction) = archive_bounds(raw, &archived, compaction_seq);
    Ok(CompactionMarker {
        compaction_seq,
        summary: display_summary(prompt).to_owned(),
        from_seq,
        to_seq: compaction_seq - 1,
        kept_count: kept_count::proven_count(compaction_seq, &raw[index + 1..]),
        earlier_compaction,
    })
}

fn raw_compaction_seq(raw: &[(u64, SessionLogEntry)], seq: u64) -> Option<u64> {
    // Multiple Compress replacements can share one mutation seq. That seq can't
    // identify a unique archive, so don't expose it as a browse-back target.
    let index = raw.binary_search_by_key(&seq, |(seq, _)| *seq).ok()?;
    matches!(raw[index].1, SessionLogEntry::Compress { .. }).then_some(seq)
}

fn archive_bounds(
    raw: &[(u64, SessionLogEntry)],
    archived: &[(u64, SessionLogEntry)],
    compaction_seq: u64,
) -> (u64, Option<u64>) {
    let window = active_context_window(archived);
    let Some(index) = window.boundary_index() else {
        return (raw[0].0, None);
    };
    if let Some(seq) = raw_compaction_seq(raw, archived[index].0) {
        return (seq + 1, Some(seq));
    }
    // The edited boundary's seq can be newer than rows logically after it.
    // Keep those rows in the archive range without inventing an ancestry link.
    let from_seq = window.entries().iter().map(|(seq, _)| *seq).min();
    (from_seq.unwrap_or(compaction_seq), None)
}

fn page_view(
    mut view: TranscriptView,
    seq_range: Option<RangeInclusive<u64>>,
) -> Result<TranscriptView> {
    let Some(range) = seq_range else {
        return Ok(view);
    };
    ensure!(!range.is_empty(), "transcript sequence range is reversed");
    view.compaction = view
        .compaction
        .filter(|marker| range.contains(&marker.compaction_seq));
    view.entries.retain(|(seq, _)| range.contains(seq));
    Ok(view)
}

#[cfg(test)]
mod tests;
